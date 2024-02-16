//! Support for datagram based server transports.
//!
//! Wikipedia defines [Datagram] as:
//!
//! > _A datagram is a basic transfer unit associated with a packet-switched
//! > network. Datagrams are typically structured in header and payload
//! > sections. Datagrams provide a connectionless communication service
//! > across a packet-switched network. The delivery, arrival time, and order
//! > of arrival of datagrams need not be guaranteed by the network._
//!
//! [Datagram]: https://en.wikipedia.org/wiki/Datagram
use std::fmt::Debug;
use std::net::SocketAddr;
use std::net::UdpSocket;
use std::{future::poll_fn, string::String};

use std::{
    io,
    sync::{Arc, Mutex},
};

use tokio::task::JoinHandle;
use tokio::{io::ReadBuf, sync::watch};
use tracing::{enabled, error, trace, Level};

use crate::base::Message;
use crate::net::server::buf::BufSource;
use crate::net::server::error::Error;
use crate::net::server::message::ContextAwareMessage;
use crate::net::server::message::MessageProcessor;
use crate::net::server::metrics::ServerMetrics;
use crate::net::server::middleware::chain::MiddlewareChain;
use crate::net::server::service::{CallResult, Service, ServiceCommand};
use crate::net::server::sock::AsyncDgramSock;
use crate::net::server::util::to_pcap_text;

use super::buf::VecBufSource;

/// A UDP transport based DNS server transport.
///
/// UDP aka User Datagram Protocol, as implied by the name, is a datagram
/// based protocol. This type defines a type of [`DgramServer`] that expects
/// connections to be received via [`UdpSocket`] and can thus be used to
/// implement a UDP based DNS server.
pub type UdpServer<Svc> = DgramServer<UdpSocket, VecBufSource, Svc>;

//------------ DgramServer ---------------------------------------------------

/// A server for connecting clients via a datagram based network transport to
/// a [`Service`].
///
/// [`DgramServer`] doesn't itself define how messages should be received,
/// message buffers should be allocated, message lengths should be determined
/// or how request messages should be received and responses sent. Instead it
/// is generic over types that provide these abilities.
///
/// By using different implementations of these traits, or even your own
/// implementations, the behaviour of [`DgramServer`] can be tuned as needed.
///
/// The [`DgramServer`] needs a socket to receive incoming messages, a
/// [`BufSource`] to create message buffers on demand, and a [`Service`] to
/// handle received request messages and generate corresponding response
/// messages for [`DgramServer`] to deliver to the client.
///
/// A socket is anything that implements the [`AsyncDgramSock`] trait. This
/// crate provides an implementation for [`UdpSocket`].

pub struct DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    command_rx: watch::Receiver<ServiceCommand>,
    command_tx: Arc<Mutex<watch::Sender<ServiceCommand>>>,
    sock: Arc<Sock>,
    buf: Arc<Buf>,
    service: Arc<Svc>,
    middleware_chain: Option<MiddlewareChain<Buf::Output, Svc::Target>>,
    metrics: Arc<ServerMetrics>,
}

/// Creation
///
impl<Sock, Buf, Svc> DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    /// Constructs a new [`DgramServer`] instance.
    ///
    /// Takes:
    /// - A socket which must implement [`AsyncDgramSock`] and is responsible
    /// receiving new messages and send responses back to the client.
    /// - A [`BufSource`] for creating buffers on demand.
    /// - A [`Service`] for handling received requests and generating responses.
    ///
    /// Invoke [`run()`] to receive and process incoming messages.
    ///
    /// [`run()`]: Self::run()
    #[must_use]
    pub fn new(sock: Sock, buf: Arc<Buf>, service: Arc<Svc>) -> Self {
        let (command_tx, command_rx) = watch::channel(ServiceCommand::Init);
        let command_tx = Arc::new(Mutex::new(command_tx));
        let metrics = Arc::new(ServerMetrics::connection_less());

        DgramServer {
            command_tx,
            command_rx,
            sock: sock.into(),
            buf,
            service,
            metrics,
            middleware_chain: None,
        }
    }

    /// Configure the [`DgramServer`] to process messages via a [`MiddlewareChain`].
    #[must_use]
    pub fn with_middleware(
        mut self,
        middleware_chain: MiddlewareChain<Buf::Output, Svc::Target>,
    ) -> Self {
        self.middleware_chain = Some(middleware_chain);
        self
    }
}

/// Access
///
impl<Sock, Buf, Svc> DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    /// Get a reference to the network source being used to receive messages.
    #[must_use]
    pub fn source(&self) -> Arc<Sock> {
        self.sock.clone()
    }

    /// Get a reference to the metrics for this server.
    #[must_use]
    pub fn metrics(&self) -> Arc<ServerMetrics> {
        self.metrics.clone()
    }
}

/// Control
///
impl<Sock, Buf, Svc> DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    /// Start the server.
    ///
    /// # Drop behaviour
    ///
    /// When dropped [`shutdown()`] will be invoked.
    ///
    /// [`shutdown()`]: Self::shutdown
    pub async fn run(&self)
    where
        Svc::Single: Send,
    {
        if let Err(err) = self.run_until_error().await {
            error!("DgramServer: {err}");
        }
    }

    /// Stop the server.
    ///
    /// No new messages will be received.
    ///
    /// Tip: Await the [`tokio::task::JoinHandle`] that you received when
    /// spawning a task to run the server to know when shutdown is complete.
    ///
    ///
    /// [`tokio::task::JoinHandle`]:
    ///     https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html
    // TODO: Do we also need a non-graceful terminate immediately function?
    pub fn shutdown(&self) -> Result<(), Error> {
        self.command_tx
            .lock()
            .unwrap()
            .send(ServiceCommand::Shutdown)
            .map_err(|_| Error::CommandCouldNotBeSent)
    }
}

//--- Internal details

impl<Sock, Buf, Svc> DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    /// Receive incoming messages until shutdown or fatal error.
    ///
    // TODO: Use a strongly typed error, not String.
    async fn run_until_error(&self) -> Result<(), String>
    where
        Svc::Single: Send,
    {
        let mut command_rx = self.command_rx.clone();

        loop {
            tokio::select! {
                biased;

                command_res = command_rx.changed() => {
                    command_res.map_err(|err| format!("Error while receiving command: {err}"))?;

                    let cmd = *command_rx.borrow_and_update();

                    match cmd {
                        ServiceCommand::Reconfigure { .. } => {
                            /* TODO */

                            // TODO: Support dynamic replacement of the
                            // middleware chain? E.g. via
                            // ArcSwapOption<MiddlewareChain> instead of
                            // Option?
                        }

                        ServiceCommand::Shutdown => break,

                        ServiceCommand::Init => {
                            // The initial "Init" value in the watch channel is never
                            // actually seen because the select Into impl only calls
                            // watch::Receiver::borrow_and_update() AFTER changed()
                            // signals that a new value has been placed in the watch
                            // channel. So the only way to end up here would be if
                            // we somehow wrongly placed another ServiceCommand::Init
                            // value into the watch channel after the initial one.
                            unreachable!()
                        }

                        ServiceCommand::CloseConnection => {
                            // A datagram server does not have connections so handling
                            // the close of a connection which can never happen has no
                            // meaning as it cannot occur.
                            unreachable!()
                        }
                    }
                }

                recv_res = self.recv_from() => {
                    let (msg, addr, bytes_read) = recv_res
                        .map_err(|err|
                            format!("Error while receiving message: {err}")
                        )?;

                    if enabled!(Level::TRACE) {
                        let pcap_text = to_pcap_text(&msg, bytes_read);
                        trace!(%addr, pcap_text, "Received message");
                    }

                    self.process_request(
                        msg, addr, self.sock.clone(),
                        self.middleware_chain.clone(),
                        &self.service,
                        self.metrics.clone()
                    )
                        .map_err(|err|
                            format!("Error while processing message: {err}")
                        )?;
                }
            }
        }

        Ok(())
    }

    async fn recv_from(
        &self,
    ) -> Result<(Buf::Output, SocketAddr, usize), io::Error> {
        let mut res = self.buf.create_buf();
        let (addr, bytes_read) = {
            let mut buf = ReadBuf::new(res.as_mut());
            let addr = poll_fn(|ctx| self.sock.poll_recv_from(ctx, &mut buf))
                .await?;
            (addr, buf.filled().len())
        };
        Ok((res, addr, bytes_read))
    }

    async fn send_to(
        sock: &Sock,
        data: &[u8],
        dest: &SocketAddr,
    ) -> Result<(), io::Error> {
        let sent = poll_fn(|ctx| sock.poll_send_to(ctx, data, dest)).await?;
        if sent != data.len() {
            Err(io::Error::new(io::ErrorKind::Other, "short send"))
        } else {
            Ok(())
        }
    }
}

//--- MessageProcessor

impl<Sock, Buf, Svc> MessageProcessor<Buf, Svc>
    for DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    type State = Arc<Sock>;

    fn add_context_to_request(
        &self,
        request: Message<Buf::Output>,
        addr: SocketAddr,
    ) -> ContextAwareMessage<Message<Buf::Output>> {
        ContextAwareMessage::new(request, false, addr)
    }

    fn handle_final_call_result(
        CallResult { response, .. }: CallResult<Svc::Target>,
        addr: SocketAddr,
        sock: Self::State,
        _metrics: Arc<ServerMetrics>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            if let Some(response) = response {
                let target = response.finish();
                let bytes = target.as_dgram_slice();

                if enabled!(Level::TRACE) {
                    let pcap_text = to_pcap_text(bytes, bytes.len());
                    trace!(%addr, pcap_text, "Sent response");
                }

                let _ = Self::send_to(&sock, bytes, &addr).await;
            }

            // TODO:
            // metrics.num_pending_writes.store(???, Ordering::Relaxed);
        })
    }
}

//--- Drop

impl<Sock, Buf, Svc> Drop for DgramServer<Sock, Buf, Svc>
where
    Sock: AsyncDgramSock + Send + Sync + 'static,
    Buf: BufSource + Send + Sync + 'static,
    Buf::Output: Send + Sync + 'static + Debug,
    Svc: Service<Buf::Output> + Send + Sync + 'static,
{
    fn drop(&mut self) {
        // Shutdown the DgramServer. Don't handle the failure case here as
        // I'm not sure if it's safe to log or write to stderr from a Drop
        // impl.
        let _ = self.shutdown();
    }
}