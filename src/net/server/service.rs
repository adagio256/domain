use std::boxed::Box;
use std::time::Duration;
use std::vec::Vec;
use std::{convert::AsRef, string::String};

use futures::{Future, Stream};
use octseq::{OctetsBuilder, ShortBuf};

use crate::base::message_builder::AdditionalBuilder;
use crate::base::wire::Composer;
use crate::base::{message::ShortMessage, Message, StreamTarget};

use super::ContextAwareMessage;
use futures::prelude::stream::StreamExt;

//------------ MsgProvider ---------------------------------------------------

/// A MsgProvider can determine the number of bytes of message data to expect
/// and then turn those bytes into a concrete message type.
pub trait MsgProvider<RequestOctets: AsRef<[u8]>> {
    /// The number of bytes that need to be read before it is possible to
    /// determine how many more bytes of message should follow. Not all
    /// message types require this, e.g. UDP DNS message length is determined
    /// by the size of the UDP message received, while for TCP DNS messages
    /// the number of bytes to expect is determined by the first two bytes
    /// received.
    const MIN_HDR_BYTES: usize;

    /// The concrete type of message that we produce from given message
    /// bytes.
    type Msg;

    /// The actual number of message bytes to follow given at least
    /// MIN_HDR_BYTES of message header.
    fn determine_msg_len(hdr_buf: &mut RequestOctets) -> usize;

    /// Convert a sequence of bytes to a concrete message.
    fn from_octets(octets: RequestOctets) -> Result<Self::Msg, ShortMessage>;
}

/// An implementation of MsgProvider for DNS [`Message`]s.
impl<RequestOctets: AsRef<[u8]>> MsgProvider<RequestOctets>
    for Message<RequestOctets>
{
    /// RFC 1035 section 4.2.2 "TCP Usage" says:
    ///     "The message is prefixed with a two byte length field which gives
    ///      the message length, excluding the two byte length field.  This
    ///      length field allows the low-level processing to assemble a
    ///      complete message before beginning to parse it."
    const MIN_HDR_BYTES: usize = 2;

    type Msg = Self;

    #[must_use]
    fn determine_msg_len(hdr_buf: &mut RequestOctets) -> usize {
        u16::from_be_bytes(hdr_buf.as_ref().try_into().unwrap()) as usize
    }

    fn from_octets(octets: RequestOctets) -> Result<Self, ShortMessage> {
        Self::from_octets(octets)
    }
}

//------------ Service -------------------------------------------------------

pub type ServiceResultItem<Target, E> =
    Result<CallResult<Target>, ServiceError<E>>;
pub type ServiceResult<Target, E> =
    Result<Transaction<ServiceResultItem<Target, E>>, ServiceError<E>>;

/// A Service is responsible for generating responses to received DNS messages.
///
/// Each [`Service`] implements a single [`Self::call()`] function which takes a DNS
/// request [`Message`] and returns either a [`Transaction`] on success, or a
/// [`ServiceError`] on failure.
///
/// Responses are encapsulated inside a [`Transaction`] which is either [Single]
/// (a single response) or [Stream] (a stream of responses, e.g. for a zone
/// transfer), where each response is a [`CallResult`].
///
/// In the common case a [`CallResult`] is a DNS response message. For some
/// advanced use cases it can instead, or additionally, direct the server
/// handling the request (or a single connection it is handling) to adjust its
/// own configuration, or even to terminate the connection.
///
/// You can either implement the [`Service`] trait directly, or use the blanket
/// impl to turn any function with a compatible signature into a [`Service`]
/// implementation like so:
///
/// ```ignore
/// fn simple_service() -> impl Service<Vec<u8>, Message<Vec<u8>>> {
///     type MyServiceResult = ServiceResult<Vec<u8>, ServiceError<()>>;
///
///     fn query(msg: Message<Vec<u8>>) -> Transaction<
///         impl Future<Output = MyServiceResult>,
///         Once<Pending<MyServiceResult>>,
///     > {
///         Transaction::Single(async move {
///             let res = MessageBuilder::new_vec();
///             let mut answer = res.start_answer(&msg, Rcode::NoError).unwrap();
///             answer
///                 .push((
///                     Dname::root_ref(),
///                     Class::In,
///                     86400,
///                     A::from_octets(192, 0, 2, 1),
///                 ))
///                 .unwrap();
///
///             let mut target = StreamTarget::new_vec();
///             target
///                 .append_slice(&answer.into_message().into_octets())
///                 .map_err(|err| ServiceError::Other(err.to_string()))?;
///             Ok(CallResult::new(target))
///         })
///     }
///
///     |msg| Ok(query(msg))
/// }
///
/// let service: Service = simple_service().into();
/// ```
pub trait Service<RequestOctets: AsRef<[u8]> = Vec<u8>> {
    type Error: Send + Sync + 'static;
    type Target: Composer + Default + Send + Sync + 'static;

    #[allow(clippy::type_complexity)]
    fn call(
        &self,
        message: &ContextAwareMessage<Message<RequestOctets>>,
    ) -> ServiceResult<Self::Target, Self::Error>;
}

impl<F, SrvErr, ReqOct, Tgt> Service<ReqOct> for F
where
    F: Fn(
        &ContextAwareMessage<Message<ReqOct>>,
    ) -> ServiceResult<Tgt, SrvErr>,
    ReqOct: AsRef<[u8]>,
    Tgt: Composer + Default + Send + Sync + 'static,
    SrvErr: Send + Sync + 'static,
{
    type Error = SrvErr;
    type Target = Tgt;

    fn call(
        &self,
        message: &ContextAwareMessage<Message<ReqOct>>,
    ) -> Result<
        Transaction<Result<CallResult<Tgt>, ServiceError<SrvErr>>>,
        ServiceError<Self::Error>,
    > {
        (*self)(message)
    }
}

//------------ ServiceError --------------------------------------------------

#[derive(Debug)]
pub enum ServiceError<T> {
    ServiceSpecificError(T),
    ShuttingDown,
    Other(String),
}

impl<T> std::fmt::Display for ServiceError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ServiceError::ServiceSpecificError(_err) => {
                write!(f, "ServiceSpecificError")
            }
            ServiceError::ShuttingDown => {
                write!(f, "ShuttingDown")
            }
            ServiceError::Other(err) => {
                write!(f, "Other({})", err)
            }
        }
    }
}

//------------ ServiceCommand ------------------------------------------------

#[derive(Copy, Clone, Debug)]
pub enum ServiceCommand {
    CloseConnection,
    Init,
    Reconfigure { idle_timeout: Duration },
    Shutdown,
}

//------------ CallResult ----------------------------------------------------

pub struct CallResult<Target> {
    pub response: AdditionalBuilder<StreamTarget<Target>>,
    pub command: Option<ServiceCommand>,
}

/// Directions to a server on how to respond to a request.
///
/// [`CallResult`] supports the following ways to handle a client request:
///
///   - Respond to the client. This is the default case.
///
///   - Respond to the client and adjust the servers handling of requests.
///     This could be required for example to honour a client request EDNS(0)
///     OPT RR that requests that the timeout from server to client be altered.
///
///   - Ignore the client request, e.g. due to policy.
///
///   - Terminate the connection with the client, e.g. due to policy or
///     or because the service is shutting down.
///
/// For reasons of policy it may be necessary to ignore certain client
/// requests without sending a response
impl<Target> CallResult<Target>
where
    Target: OctetsBuilder + AsRef<[u8]> + AsMut<[u8]>,
    Target::AppendError: Into<ShortBuf>,
{
    #[must_use]
    pub fn new(response: AdditionalBuilder<StreamTarget<Target>>) -> Self {
        Self {
            response,
            command: None,
        }
    }

    #[must_use]
    pub fn with_command(self, command: ServiceCommand) -> Self {
        Self {
            response: self.response,
            command: Some(command),
        }
    }
}

//------------ Transaction ---------------------------------------------------

/// A server transaction generating the responses for a request.
pub enum Transaction<Item> {
    /// The transaction will be concluded with a single response.
    Single(Option<Box<dyn Future<Output = Item> + Send + Unpin>>),

    /// The transaction will results in stream of multiple responses.
    Stream(Box<dyn Stream<Item = Item> + Send + Unpin>),
}

impl<Item> Transaction<Item> {
    pub fn single(
        fut: Box<dyn Future<Output = Item> + Send + Unpin>,
    ) -> Self {
        Self::Single(Some(fut))
    }

    pub fn stream(
        stream: Box<dyn Stream<Item = Item> + Send + Unpin>,
    ) -> Self {
        Self::Stream(stream)
    }

    pub async fn next(&mut self) -> Option<Item> {
        match self {
            Transaction::Single(opt_fut) => match opt_fut.take() {
                Some(fut) => Some(fut.await),
                None => None,
            },

            Transaction::Stream(stream) => stream.next().await,
        }
    }
}
