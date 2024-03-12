// DNSSEC validator transport

use bytes::Bytes;
use crate::base::Message;
use crate::base::MessageBuilder;
use crate::base::ParsedDname;
use crate::base::StaticCompressor;
use crate::validator::context::ValidationContext;
use crate::validator::types::ValidationState;
use crate::net::client::request::ComposeRequest;
use crate::net::client::request::Error;
use crate::net::client::request::GetResponse;
use crate::net::client::request::SendRequest;
use crate::rdata::AllRecordData;
use crate::validator;
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
pub struct Config {
}

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
        Self {
        }
    }
}

//------------ Connection -----------------------------------------------------

#[derive(Clone)]
/// A connection that caches responses from an upstream connection.
pub struct Connection<Upstream, > {
    /// Upstream transport to use for requests.
    upstream: Upstream,

    vc: Arc<ValidationContext>,

    /// The configuration of this connection.
    config: Config,
}

impl<Upstream> Connection<Upstream> {
    /// Create a new connection with default configuration parameters.
    ///
    /// Note that Upstream needs to implement [SendRequest]
    /// (and Clone/Send/Sync) to be useful.
    pub fn new(upstream: Upstream, vc: Arc<ValidationContext>) -> Self {
        Self::with_config(upstream, vc, Default::default())
    }

    /// Create a new connection with specified configuration parameters.
    ///
    /// Note that Upstream needs to implement [SendRequest]
    /// (and Clone/Send/Sync) to be useful.
    pub fn with_config(upstream: Upstream, vc: Arc<ValidationContext>, 
	config: Config) -> Self {
        Self {
            upstream,
	    vc,
            config,
        }
    }
}

//------------ SendRequest ----------------------------------------------------

impl<CR, Upstream, > SendRequest<CR> for Connection<Upstream,>
where
    CR: Clone + ComposeRequest + 'static,
    Upstream: Clone + SendRequest<CR> + Send + Sync + 'static,
{
    fn send_request(
        &self,
        request_msg: CR,
    ) -> Box<dyn GetResponse + Send + > {
        Box::new(Request::<CR, Upstream, >::new(
            request_msg,
            self.upstream.clone(),
	    self.vc.clone(),
            self.config.clone(),
        ))
    }
}

//------------ Request --------------------------------------------------------

/// The state of a request that is executed.
pub struct Request<CR, Upstream, >
where
    CR: Send + Sync,
    Upstream: Send + Sync,
{
    /// State of the request.
    //state: RequestState,

    /// The request message.
    request_msg: CR,

    /// The upstream transport of the connection.
    upstream: Upstream,

    /// The validation context.
    vc: Arc<ValidationContext>,

    /// The configuration of the connection.
    config: Config,
}

impl<CR, Upstream, > Request<CR, Upstream, >
where
    CR: Clone + ComposeRequest + Send + Sync,
    Upstream: SendRequest<CR> + Send + Sync,
{
    /// Create a new Request object.
    fn new(
        request_msg: CR,
        upstream: Upstream,
	vc: Arc<ValidationContext>,
        config: Config,
    ) -> Request<CR, Upstream, > {
        Self {
            request_msg,
            upstream,
	    vc,
            config,
        }
    }

    /// This is the implementation of the get_response method.
    ///
    /// This function is not cancel safe.
    async fn get_response_impl(&mut self) -> Result<Message<Bytes>, Error> {

	// We should check for the CD flag. If set then just perform the
	// request without validating.

	// We should make sure the DO is set, otherwise we can't validate.

	let mut request =
		self.upstream.send_request(self.request_msg.clone());

	let response_msg = request.get_response().await?;

	// We should validate.
	let res = validator::validate_msg(&response_msg, &self.vc);
	println!("get_response_impl: {res:?}");
	match res {
	    Err(err) => {
		todo!();
	    }
	    Ok(state) => {
		match state {
		    ValidationState::Secure => todo!(),
		    ValidationState::Insecure => todo!(),
		    ValidationState::Bogus => todo!(),
		    ValidationState::Indeterminate => {
			// Check the state of the DO flag to see if we have to
			// strip DNSSEC records. Clear the AD flag if it is
			// set.
			let dnssec_ok = self.request_msg.dnssec_ok();
			if dnssec_ok {
			    todo!();
			}
			else
			{
			    let msg = remove_dnssec(&response_msg, false);
			    todo!();
			}
		    }
		}
	    }
	}

	todo!();

	Ok(response_msg)
    }

}

impl<CR, Upstream, > Debug for Request<CR, Upstream, >
where
    CR: Send + Sync,
    Upstream: Send + Sync,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        f.debug_struct("Request")
            .field("fut", &format_args!("_"))
            .finish()
    }
}

impl<CR, Upstream, > GetResponse for Request<CR, Upstream, >
where
    CR: Clone + ComposeRequest + Debug + Sync,
    Upstream: SendRequest<CR> + Send + Sync + 'static,
{
    fn get_response(
        &mut self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Message<Bytes>, Error>>
                + Send
                + '_,
        >,
    > {
        Box::pin(self.get_response_impl())
    }
}

/// Return a new message without the DNSSEC type RRSIG, NSEC, and NSEC3.
fn remove_dnssec(
    msg: &Message<Bytes>,
    ad: bool,
) -> Result<Message<Bytes>, Error> {
    let mut target =
        MessageBuilder::from_target(StaticCompressor::new(Vec::new()))
            .expect("Vec is expected to have enough space");

    let source = msg;

    *target.header_mut() = source.header();

    if !ad {
        // Clear ad
        target.header_mut().set_ad(false);
    }

    let source = source.question();
    let mut target = target.question();
    for rr in source {
        target.push(rr?).expect("push failed");
    }
    let mut source = source.answer()?;
    let mut target = target.answer();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedDname<_>>>()?
            .expect("record expected");
        if is_dnssec(rr.rtype()) {
            continue;
        }
        target.push(rr).expect("push error");
    }

    let mut source =
        source.next_section()?.expect("section should be present");
    let mut target = target.authority();
    for rr in &mut source {
        let rr = rr?
            .into_record::<AllRecordData<_, ParsedDname<_>>>()?
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
            .into_record::<AllRecordData<_, ParsedDname<_>>>()?
            .expect("record expected");
        if is_dnssec(rr.rtype()) {
            continue;
        }
        target.push(rr).expect("push error");
    }

    let result = target.as_builder().clone();
    Ok(
        Message::<Bytes>::from_octets(result.finish().into_target().into())
            .expect(
                "Message should be able to parse output from MessageBuilder",
            ),
    )
}

