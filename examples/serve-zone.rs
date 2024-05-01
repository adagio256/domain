//! Loads a zone file and serves it over localhost UDP and TCP.
//!
//! Try queries such as:
//!
//!   dig @127.0.0.1 -p 8053 NS example.com dig @127.0.0.1 -p 8053 A
//!   example.com dig @127.0.0.1 -p 8053 AAAA example.com dig @127.0.0.1 -p
//!   8053 CNAME example.com
//!
//! Also try with TCP, e.g.:
//!
//!   dig @127.0.0.1 -p 8053 +tcp A example.com
//!
//! Also try AXFR, e.g.:
//!
//!   dig @127.0.0.1 -p 8053 AXFR example.com
//!
//! With a large zone and the following dig options and an XFR thread pool of
//! size 16 a peak rate of 110MB/s (localhost only) was recorded:
//!
//!   $ dig -4 @127.0.0.1 -p 8053 +noanswer +tries=1 +noidnout AXFR de.
//!   ; <<>> DiG 9.18.24 <<>> +noanswer -4 @127.0.0.1 -p 8053 +tries +noidnout AXFR de.
//!   ; (1 server found)
//!   ;; global options: +cmd
//!   ;; Query time: 47669 msec
//!   ;; SERVER: 127.0.0.1#8053(127.0.0.1) (TCP)
//!   ;; WHEN: Thu May 02 00:14:04 CEST 2024
//!   ;; XFR size: 43347447 records (messages 16393621, bytes 2557835040)

use core::str::FromStr;

use std::future::pending;
use std::io::BufReader;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, UdpSocket};
use tracing_subscriber::EnvFilter;

use domain::base::iana::Rcode;
use domain::base::{Name, Rtype, ToName, Ttl};
use domain::net::server::buf::VecBufSource;
use domain::net::server::dgram::DgramServer;
use domain::net::server::message::Request;
#[cfg(feature = "siphasher")]
use domain::net::server::middleware::cookies::CookiesMiddlewareSvc;
use domain::net::server::middleware::edns::EdnsMiddlewareSvc;
use domain::net::server::middleware::mandatory::MandatoryMiddlewareSvc;
use domain::net::server::middleware::xfr::XfrMiddlewareSvc;
use domain::net::server::service::{CallResult, ServiceResult};
use domain::net::server::stream::{self, StreamServer};
use domain::net::server::util::{mk_builder_for_target, service_fn};
use domain::net::server::ConnectionConfig;
use domain::zonefile::inplace;
use domain::zonetree::{Answer, Rrset, SharedRrset};
use domain::zonetree::{Zone, ZoneTree};

#[tokio::main()]
async fn main() {
    // Initialize tracing based logging. Override with env var RUST_LOG, e.g.
    // RUST_LOG=trace.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_thread_ids(true)
        .without_time()
        .try_init()
        .ok();

    // Populate a zone tree with test data
    let zone_bytes = include_bytes!("../test-data/zonefiles/nsd-example.txt");
    let mut zone_bytes = BufReader::new(&zone_bytes[..]);
    // let zone_bytes = std::fs::File::open("/etc/nsd/zones/de-zone").unwrap();
    // let mut zone_bytes = BufReader::new(zone_bytes);

    // We're reading from static data so this cannot fail due to I/O error.
    // Don't handle errors that shouldn't happen, keep the example focused
    // on what we want to demonstrate.
    let reader = inplace::Zonefile::load(&mut zone_bytes).unwrap();
    let zone = Zone::try_from(reader).unwrap();

    // Make changes to a zone to create a diff for IXFR use.
    let mut writer = zone.write().await;
    {
        let node = writer.open(true).await.unwrap();
        let mut new_ns = Rrset::new(Rtype::NS, Ttl::from_secs(60));
        let ns_rec = domain::rdata::Ns::new(
            Name::from_str("write-test.example.com").unwrap(),
        );
        new_ns.push_data(ns_rec.into());
        node.update_rrset(SharedRrset::new(new_ns)).await.unwrap();
    }
    let diff = writer.commit().await.unwrap();

    let mut zones = ZoneTree::new();
    zones.insert_zone(zone.clone()).unwrap();
    let zones = Arc::new(zones);

    let addr = "127.0.0.1:8053";
    let svc = service_fn(my_service, zones);

    // Insert XFR middleware to automagically handle AXFR and IXFR requests.
    let num_xfr_threads =
        std::thread::available_parallelism().unwrap().get() / 2;
    println!("Using {num_xfr_threads} threads for XFR");
    let mut svc = XfrMiddlewareSvc::<Vec<u8>, _>::new(svc, num_xfr_threads);
    svc.add_zone(zone.clone());
    if let Some(diff) = diff {
        svc.add_diff(&zone, diff);
    }

    #[cfg(feature = "siphasher")]
    let svc = CookiesMiddlewareSvc::<Vec<u8>, _>::with_random_secret(svc);
    let svc = EdnsMiddlewareSvc::<Vec<u8>, _>::new(svc);
    let svc = MandatoryMiddlewareSvc::<Vec<u8>, _>::new(svc);
    let svc = Arc::new(svc);

    let sock = UdpSocket::bind(addr).await.unwrap();
    let sock = Arc::new(sock);
    let mut udp_metrics = vec![];
    let num_cores = std::thread::available_parallelism().unwrap().get();
    for _i in 0..num_cores {
        let udp_srv =
            DgramServer::new(sock.clone(), VecBufSource, svc.clone());
        let metrics = udp_srv.metrics();
        udp_metrics.push(metrics);
        tokio::spawn(async move { udp_srv.run().await });
    }

    let sock = TcpListener::bind(addr).await.unwrap();
    let mut conn_config = ConnectionConfig::new();
    conn_config.set_max_queued_responses(1024);
    let mut config = stream::Config::new();
    config.set_connection_config(conn_config);
    let tcp_srv = StreamServer::with_config(sock, VecBufSource, svc, config);
    let tcp_metrics = tcp_srv.metrics();

    tokio::spawn(async move { tcp_srv.run().await });

    eprintln!("Ready");

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(5000)).await;

            let mut udp_num_connections = 0;
            let mut udp_num_inflight_requests = 0;
            let mut udp_num_pending_writes = 0;
            let mut udp_num_received_requests = 0;
            let mut udp_num_sent_responses = 0;

            for metrics in udp_metrics.iter() {
                udp_num_connections += metrics.num_connections();
                udp_num_inflight_requests += metrics.num_inflight_requests();
                udp_num_pending_writes += metrics.num_pending_writes();
                udp_num_received_requests += metrics.num_received_requests();
                udp_num_sent_responses += metrics.num_sent_responses();
            }
            eprintln!(
                "Server status: #conn/#in-flight/#pending-writes/#msgs-recvd/#msgs-sent: UDP={}/{}/{}/{}/{} TCP={}/{}/{}/{}/{}",
                udp_num_connections,
                udp_num_inflight_requests,
                udp_num_pending_writes,
                udp_num_received_requests,
                udp_num_sent_responses,
                tcp_metrics.num_connections(),
                tcp_metrics.num_inflight_requests(),
                tcp_metrics.num_pending_writes(),
                tcp_metrics.num_received_requests(),
                tcp_metrics.num_sent_responses(),
            );
        }
    });

    pending::<()>().await;
}

#[allow(clippy::type_complexity)]
fn my_service(
    request: Request<Vec<u8>>,
    zones: Arc<ZoneTree>,
) -> ServiceResult<Vec<u8>> {
    let question = request.message().sole_question().unwrap();
    let zone = zones
        .find_zone(question.qname(), question.qclass())
        .map(|zone| zone.read());
    let answer = match zone {
        Some(zone) => {
            let qname = question.qname().to_bytes();
            let qtype = question.qtype();
            zone.query(qname, qtype).unwrap()
        }
        None => Answer::new(Rcode::NXDOMAIN),
    };

    let builder = mk_builder_for_target();
    let additional = answer.to_message(request.message(), builder);
    Ok(CallResult::new(additional))
}
