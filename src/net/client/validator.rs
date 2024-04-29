// DNSSEC validator transport

use crate::base::opt::AllOptData;
use crate::base::opt::ExtendedError;
use crate::base::Message;
use crate::base::MessageBuilder;
use crate::base::ParsedName;
use crate::base::Rtype;
use crate::base::StaticCompressor;
use crate::dep::octseq::OctetsInto;
use crate::net::client::request::ComposeRequest;
use crate::net::client::request::Error;
use crate::net::client::request::GetResponse;
use crate::net::client::request::RequestMessage;
use crate::net::client::request::SendRequest;
use crate::rdata::AllRecordData;
use crate::validator;
use crate::validator::context::ValidationContext;
use crate::validator::types::ValidationState;
use bytes::Bytes;
use std::boxed::Box;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::vec::Vec;

//------------ Config ---------------------------------------------------------

/// Configuration of a cache.
#[derive(Clone, Debug)]
pub struct Config {}

impl Config {
    /// Creates a new config with default values.
    ///
    /// The default values are documented at the relevant set_* methods.
    pub fn new() -> Self {
        Default::default()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {}
    }
}

//------------ Connection -----------------------------------------------------

#[derive(Clone)]
/// A connection that caches responses from an upstream connection.
pub struct Connection<Upstream, VCUpstream> {
    /// Upstream transport to use for requests.
    upstream: Upstream,

    vc: Arc<ValidationContext<VCUpstream>>,

    /// The configuration of this connection.
    config: Config,
}

impl<Upstream, VCUpstream> Connection<Upstream, VCUpstream> {
    /// Create a new connection with default configuration parameters.
    ///
    /// Note that Upstream needs to implement [SendRequest]
    /// (and Clone/Send/Sync) to be useful.
    pub fn new(
        upstream: Upstream,
        vc: Arc<ValidationContext<VCUpstream>>,
    ) -> Self {
        Self::with_config(upstream, vc, Default::default())
    }

    /// Create a new connection with specified configuration parameters.
    ///
    /// Note that Upstream needs to implement [SendRequest]
    /// (and Clone/Send/Sync) to be useful.
    pub fn with_config(
        upstream: Upstream,
        vc: Arc<ValidationContext<VCUpstream>>,
        config: Config,
    ) -> Self {
        Self {
            upstream,
            vc,
            config,
        }
    }
}

//------------ SendRequest ----------------------------------------------------

impl<CR, Upstream, VCUpstream> SendRequest<CR>
    for Connection<Upstream, VCUpstream>
where
    CR: ComposeRequest + Clone + Send + Sync + 'static,
    Upstream: Clone + SendRequest<CR> + Send + Sync + 'static,
    VCUpstream:
        Clone + SendRequest<RequestMessage<Bytes>> + Send + Sync + 'static,
{
    fn send_request(
        &self,
        request_msg: CR,
    ) -> Box<dyn GetResponse + Send + Sync> {
        Box::new(Request::<CR, Upstream, VCUpstream>::new(
            request_msg,
            self.upstream.clone(),
            self.vc.clone(),
            self.config.clone(),
        ))
    }
}

//------------ Request --------------------------------------------------------

/// The state of a request that is executed.
pub struct Request<CR, Upstream, VCUpstream>
where
    Upstream: Send + Sync,
{
    /// State of the request.
    //state: RequestState,

    /// The request message.
    request_msg: CR,

    /// The upstream transport of the connection.
    upstream: Upstream,

    /// The validation context.
    vc: Arc<ValidationContext<VCUpstream>>,

    /// The configuration of the connection.
    _config: Config,
}

impl<CR, Upstream, VCUpstream> Request<CR, Upstream, VCUpstream>
where
    Upstream: SendRequest<CR> + Send + Sync,
{
    /// Create a new Request object.
    fn new(
        request_msg: CR,
        upstream: Upstream,
        vc: Arc<ValidationContext<VCUpstream>>,
        config: Config,
    ) -> Request<CR, Upstream, VCUpstream> {
        Self {
            request_msg,
            upstream,
            vc,
            _config: config,
        }
    }

    /// This is the implementation of the get_response method.
    ///
    /// This function is not cancel safe.
    async fn get_response_impl(&mut self) -> Result<Message<Bytes>, Error>
    where
        CR: Clone + ComposeRequest,
        Upstream: Clone + SendRequest<CR>,
        VCUpstream: Clone + SendRequest<RequestMessage<Bytes>>,
    {
        // Store the DO flag of the request.
        let dnssec_ok = self.request_msg.dnssec_ok();
        if !dnssec_ok {
            // Set the DO flag, otherwise we can't validate.
            self.request_msg.set_dnssec_ok(true);
        }

        // Store the CD flag of the request.
        let cd = self.request_msg.header().cd();
        if !cd {
            // Set the CD flag to get all results even if they fail to validate
            // upstream.
            self.request_msg.header_mut().set_cd(true);
        }

        let mut request =
            self.upstream.send_request(self.request_msg.clone());

        let response_msg = request.get_response().await?;
        println!("get_response_impl: response {response_msg:?}");

        if cd {
            // Clear the AD flag if it is set. Return the response without
            // checking.
            if response_msg.header().ad() {
                let mut response_msg =
                    Message::from_octets(response_msg.as_slice().to_vec())
                        .unwrap();
                response_msg.header_mut().set_ad(false);
                let response_msg = Message::<Bytes>::from_octets(
                    response_msg.into_octets().octets_into(),
                )
                .unwrap();
                return Ok(response_msg);
            }
            return Ok(response_msg);
        }

        let res = validator::validate_msg(&response_msg, &self.vc).await;
        println!("get_response_impl: {res:?}");
        match res {
            Err(_err) => {
                todo!();
            }
            Ok((state, opt_ede)) => {
                match state {
                    ValidationState::Secure => {
                        // Check the state of the DO flag to see if we have to
                        // strip DNSSEC records. Set the AD flag if it is
                        // not set and either AD or DO is set in the request.
                        // We always have to clear CD.
                        if dnssec_ok {
                            // Set AD and clear CD.
                            let mut response_msg = Message::from_octets(
                                response_msg.as_slice().to_vec(),
                            )
                            .unwrap();
                            response_msg.header_mut().set_ad(true);
                            response_msg.header_mut().set_cd(false);
                            let response_msg = Message::<Bytes>::from_octets(
                                response_msg.into_octets().octets_into(),
                            )
                            .unwrap();
                            return Ok(response_msg);
                        } else {
                            // Set AD if it was set in the request.
                            let msg = remove_dnssec(
                                &response_msg,
                                self.request_msg.header().ad(),
                            );
                            return msg;
                        }
                    }
                    ValidationState::Bogus => todo!(),
                    ValidationState::Insecure
                    | ValidationState::Indeterminate => {
                        let response_msg = match opt_ede {
                            Some(ede) => add_opt(&response_msg, ede).unwrap(),
                            None => response_msg,
                        };
                        // Check the state of the DO flag to see if we have to
                        // strip DNSSEC records. Clear the AD flag if it is
                        // set. Always clear CD.
                        if dnssec_ok {
                            // Clear AD if it is set. Clear CD.
                            let mut response_msg = Message::from_octets(
                                response_msg.as_slice().to_vec(),
                            )
                            .unwrap();
                            response_msg.header_mut().set_ad(false);
                            response_msg.header_mut().set_cd(false);
                            let response_msg = Message::<Bytes>::from_octets(
                                response_msg.into_octets().octets_into(),
                            )
                            .unwrap();
                            return Ok(response_msg);
                        } else {
                            let msg = remove_dnssec(&response_msg, false);
                            return msg;
                        }
                    }
                }
            }
        }
    }
}

impl<CR, Upstream, VCUpstream> Debug for Request<CR, Upstream, VCUpstream>
where
    Upstream: Send + Sync,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        f.debug_struct("Request")
            .field("fut", &format_args!("_"))
            .finish()
    }
}

impl<CR, Upstream, VCUpstream> GetResponse
    for Request<CR, Upstream, VCUpstream>
where
    CR: Clone + ComposeRequest,
    Upstream: Clone + SendRequest<CR> + Send + Sync + 'static,
    VCUpstream:
        Clone + SendRequest<RequestMessage<Bytes>> + Send + Sync + 'static,
{
    fn get_response(
        &mut self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Message<Bytes>, Error>>
                + Send
                + Sync
                + '_,
        >,
    > {
        Box::pin(self.get_response_impl())
    }
}

/// Return a new message without the DNSSEC type DNSKEY, RRSIG, NSEC, and NSEC3.
/// Assume that it is safe to clear CD. Only RRSIG needs to be removed
/// from the answer section unless the qtype is RRSIG. Remove all
/// DNSSEC records from the authority and additional sections.
fn remove_dnssec(
    msg: &Message<Bytes>,
    ad: bool,
) -> Result<Message<Bytes>, Error> {
    println!("remove_dnssec: ad {ad:?}");
    let mut target =
        MessageBuilder::from_target(StaticCompressor::new(Vec::new()))
            .expect("Vec is expected to have enough space");

    let source = msg;
    let opt = source.opt();

    *target.header_mut() = source.header();

    if ad != source.header().ad() {
        // Change AD.
        target.header_mut().set_ad(ad);
    }
    target.header_mut().set_cd(false);

    let source = source.question();
    let mut target = target.question();
    let mut qtype = Rtype::ANY;
    for rr in source {
        qtype = rr.clone()?.qtype();
        target.push(rr?).expect("push failed");
    }
    let mut source = source.answer()?;
    let mut target = target.answer();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedName<_>>>()?
            .expect("record expected");
        if is_dnssec(rr.rtype()) && rr.rtype() != qtype {
            println!("remove_dnssec: skipping {rr:?}");
            println!("rtype: {:?}, qtype: {qtype:?}", rr.rtype());
            continue;
        }
        target.push(rr).expect("push error");
    }

    let mut source =
        source.next_section()?.expect("section should be present");
    let mut target = target.authority();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedName<_>>>()?
            .expect("record expected");
        if is_dnssec(rr.rtype()) {
            continue;
        }
        target.push(rr).expect("push error");
    }

    let source = source.next_section()?.expect("section should be present");
    let mut target = target.additional();
    for rr in source {
        let rr = rr?;
        let rr = rr
            .into_record::<AllRecordData<_, ParsedName<_>>>()?
            .expect("record expected");
        if rr.rtype() == Rtype::OPT {
            continue;
        }
        if is_dnssec(rr.rtype()) {
            continue;
        }
        target.push(rr).expect("push error");
    }
    if let Some(opt) = opt {
        target
            .opt(|ob| {
                ob.set_dnssec_ok(false);
                // XXX something is missing ob.set_rcode(opt.rcode());
                ob.set_udp_payload_size(opt.udp_payload_size());
                ob.set_version(opt.version());
                for o in opt.opt().iter() {
                    let x: AllOptData<_, _> = o.unwrap();
                    ob.push(&x).unwrap();
                }
                Ok(())
            })
            .unwrap();
    }

    let result = target.as_builder().clone();
    Ok(
        Message::<Bytes>::from_octets(result.finish().into_target().into())
            .expect(
                "Message should be able to parse output from MessageBuilder",
            ),
    )
}

/// Check if a type is a DNSSEC type that needs to be removed.
fn is_dnssec(rtype: Rtype) -> bool {
    rtype == Rtype::DNSKEY
        || rtype == Rtype::RRSIG
        || rtype == Rtype::NSEC
        || rtype == Rtype::NSEC3
}

// Add an option
fn add_opt(
    msg: &Message<Bytes>,
    ede: ExtendedError<Bytes>,
) -> Result<Message<Bytes>, Error> {
    let mut target =
        MessageBuilder::from_target(StaticCompressor::new(Vec::new()))
            .expect("Vec is expected to have enough space");

    let source = msg;

    *target.header_mut() = msg.header();

    let source = source.question();
    let mut target = target.question();
    for rr in source {
        target.push(rr?).unwrap();
    }
    let mut source = source.answer()?;
    let mut target = target.answer();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedName<_>>>()?
            .expect("record expected");
        target.push(rr).unwrap();
    }

    let mut source =
        source.next_section()?.expect("section should be present");
    let mut target = target.authority();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedName<_>>>()?
            .expect("record expected");
        target.push(rr).unwrap();
    }

    let source = source.next_section()?.expect("section should be present");
    let mut target = target.additional();
    for rr in source {
        let rr = rr?;
        if rr.rtype() != Rtype::OPT {
            let rr = rr
                .into_record::<AllRecordData<_, ParsedName<_>>>()?
                .expect("record expected");
            target.push(rr).unwrap();
        }
    }

    if let Some(opt) = msg.opt() {
        target
            .opt(|ob| {
                ob.set_dnssec_ok(opt.dnssec_ok());
                // XXX something is missing ob.set_rcode(opt.rcode());
                ob.set_udp_payload_size(opt.udp_payload_size());
                ob.set_version(opt.version());
                for o in opt.opt().iter() {
                    let x: AllOptData<_, _> = o.unwrap();
                    ob.push(&x).unwrap();
                }
                ob.push(&ede).unwrap();
                Ok(())
            })
            .unwrap();
    }

    let result = target.as_builder().clone();
    let msg = Message::<Bytes>::from_octets(
        result.finish().into_target().octets_into(),
    )
    .expect("Message should be able to parse output from MessageBuilder");
    Ok(msg)
}
