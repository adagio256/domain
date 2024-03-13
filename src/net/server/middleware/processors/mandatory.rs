//! Core DNS RFC standards based message processing for MUST requirements.
use octseq::Octets;
use tracing::{debug, trace};

use crate::{
    base::{
        iana::Rcode, message_builder::AdditionalBuilder, wire::Composer, Message, StreamTarget
    },
    net::server::{
        message::{
            ContextAwareMessage, TransportSpecificContext,
            UdpSpecificTransportContext,
        },
        middleware::processor::MiddlewareProcessor,
        prelude::mk_builder_for_target,
    },
};
use core::ops::ControlFlow;

/// A [`MiddlewareProcessor`] for enforcing core RFC MUST requirements on
/// processed messages.
///
/// Standards covered by ths implementation:
///
/// | RFC    | Status  |
/// |--------|---------|
/// | [1035] | TBD     |
/// | [2181] | TBD     |
/// | [6891] | TBD     |
///
/// [`MiddlewareProcessor`]:
///     crate::net::server::middleware::processor::MiddlewareProcessor
/// [1035]: https://datatracker.ietf.org/doc/html/rfc1035
/// [2181]: https://datatracker.ietf.org/doc/html/rfc2181
/// [6891]: https://datatracker.ietf.org/doc/html/rfc6891
#[derive(Debug, Default)]
pub struct MandatoryMiddlewareProcessor;

impl MandatoryMiddlewareProcessor {
    /// Constructs an instance of this processor.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl MandatoryMiddlewareProcessor {
    fn truncate<RequestOctets, Target>(
        request: &ContextAwareMessage<Message<RequestOctets>>,
        response: &mut AdditionalBuilder<StreamTarget<Target>>,
    ) where
        RequestOctets: Octets,
        Target: Composer + Default,
    {
        if let TransportSpecificContext::Udp(UdpSpecificTransportContext {
            max_response_size_hint: Some(max_response_size_hint),
        }) = request.transport()
        {
            let max_response_size_hint = *max_response_size_hint as usize;
            let response_len = response.as_slice().len();

            if response_len > max_response_size_hint {
                // Truncate per RFC 1035 section 6.2 and RFC 2181 sections 5.1
                // and 9:
                //
                // https://datatracker.ietf.org/doc/html/rfc1035#section-6.2
                //   "When a response is so long that truncation is required,
                //    the truncation should start at the end of the response
                //    and work forward in the datagram.  Thus if there is any
                //    data for the authority section, the answer section is
                //    guaranteed to be unique."
                //
                // https://datatracker.ietf.org/doc/html/rfc2181#section-5.1
                //   "A query for a specific (or non-specific) label, class,
                //    and type, will always return all records in the
                //    associated RRSet - whether that be one or more RRs.  The
                //    response must be marked as "truncated" if the entire
                //    RRSet will not fit in the response."
                //
                // https://datatracker.ietf.org/doc/html/rfc2181#section-9
                //   "Where TC is set, the partial RRSet that would not
                //    completely fit may be left in the response.  When a DNS
                //    client receives a reply with TC set, it should ignore
                //    that response, and query again, using a mechanism, such
                //    as a TCP connection, that will permit larger replies."
                //
                // https://datatracker.ietf.org/doc/html/rfc6891#section-7
                //   "The minimal response MUST be the DNS header, question
                //     section, and an OPT record.  This MUST also occur when
                //     a truncated response (using the DNS header's TC bit) is
                //     returned."

                // Tell the client that we are truncating the response.
                response.header_mut().set_tc(true);

                // Remember the original length.
                let old_len = response.as_slice().len();

                // Copy the header, question and opt record from the
                // additional section, but leave the answer and authority
                // sections empty.
                let source = response.as_message();
                let mut target = mk_builder_for_target();

                *target.header_mut() = source.header();

                let mut target = target.question();
                for rr in source.question() {
                    target.push(rr.unwrap()).unwrap(); // TODO: SAFETY
                }

                let mut target = target.additional();
                if let Some(opt) = source.opt() {
                    target.push(opt.as_record()).unwrap(); // TODO: SAFETY
                }

                let new_len = target.as_slice().len();
                trace!("Truncating response from {old_len} bytes to {new_len} bytes");

                *response = target;
            }
        }
    }
}

//--- MiddlewareProcessor

// TODO: If we extend this later to do a lot more than setting a couple of
// header flags, and if we think that there may be a need for alternate
// truncation strategies, then it might make sense to factor out truncation to
// make it "pluggable" by the user.
impl<RequestOctets, Target> MiddlewareProcessor<RequestOctets, Target>
    for MandatoryMiddlewareProcessor
where
    RequestOctets: AsRef<[u8]> + Octets,
    Target: Composer + Default,
{
    fn preprocess(
        &self,
        request: &mut ContextAwareMessage<Message<RequestOctets>>,
    ) -> ControlFlow<AdditionalBuilder<StreamTarget<Target>>> {
        // https://www.rfc-editor.org/rfc/rfc6891.html#section-6.1.1
        // 6.1.1: Basic Elements
        // ...
        // "If a query message with more than one OPT RR is received, a
        //  FORMERR (RCODE=1) MUST be returned"
        let mut new_max_response_size_hint = None;
        if let Ok(additional) = request.message().additional() {
            let mut iter = additional.limit_to::<Opt<_>>();
            if let Some(opt) = iter.next() {
                if iter.next().is_some() {
                    // More than one OPT RR received.
                    debug!("RFC 6891 6.1.1 violation: request contains more than one OPT RR.");
                    let mut builder = mk_builder_for_target();
                    builder.header_mut().set_rcode(Rcode::FormErr);
                    return ControlFlow::Break(builder.additional());
                } else if request.transport().is_udp() {
                    if let Ok(opt) = opt {
                        // https://datatracker.ietf.org/doc/html/rfc6891#section-6.2.3
                        // 6.2.3. Requestor's Payload Size
                        //
                        //   "The requestor's UDP payload size (encoded in the RR CLASS field) is
                        //    the number of octets of the largest UDP payload that can be
                        //    reassembled and delivered in the requestor's network stack.  Note
                        //    that path MTU, with or without fragmentation, could be smaller than
                        //    this.
                        //
                        //    Values lower than 512 MUST be treated as equal to 512."
                        let requestors_udp_payload_size: u16 =
                            opt.class().into();
                        if requestors_udp_payload_size < 512 {
                            debug!("RFC 6891 6.2.3 violation: OPT RR class (requestor's UDP payload size) < 512");
                        } else if request.max_response_size_hint().is_none() {
                            trace!("Setting max response size hint from EDNS(0) requestor's UDP payload size ({})", requestors_udp_payload_size);
                            new_max_response_size_hint =
                                Some(requestors_udp_payload_size);
                        }
                    }
                }
            }

            if let Some(max_response_size_hint) = new_max_response_size_hint {
                request.set_max_response_size_hint(max_response_size_hint);
            }
        }

        ControlFlow::Continue(())
    }

    fn postprocess(
        &self,
        request: &ContextAwareMessage<Message<RequestOctets>>,
        response: &mut AdditionalBuilder<StreamTarget<Target>>,
    ) {
        Self::truncate(request, response);

        // https://datatracker.ietf.org/doc/html/rfc1035#section-4.1.1
        // 4.1.1: Header section format
        //
        // ID      A 16 bit identifier assigned by the program that
        //         generates any kind of query.  This identifier is copied
        //         the corresponding reply and can be used by the requester
        //         to match up replies to outstanding queries.
        response
            .header_mut()
            .set_id(request.message().header().id());

        // QR      A one bit field that specifies whether this message is a
        //         query (0), or a response (1).
        response.header_mut().set_qr(true);

        // RD      Recursion Desired - this bit may be set in a query and
        //         is copied into the response.  If RD is set, it directs
        //         the name server to pursue the query recursively.
        //         Recursive query support is optional.
        response
            .header_mut()
            .set_rd(request.message().header().rd());

        // https://www.rfc-editor.org/rfc/rfc6891.html#section-6.1.1
        // 6.1.1: Basic Elements
        // ...
        // "If an OPT record is present in a received request, compliant
        //  responders MUST include an OPT record in their respective
        //  responses."
        //
        // TODO: What if anything should we do if we detect a request with an
        // OPT record but a response that lacks an OPT record?

        // https://www.rfc-editor.org/rfc/rfc6891.html#section-7
        // 7: Transport considerations
        // ...
        // "Lack of presence of an OPT record in a request MUST be taken as an
        //  indication that the requestor does not implement any part of this
        //  specification and that the responder MUST NOT include an OPT
        //  record in its response."
        //
        // So strip off any OPT record present if the query lacked an OPT
        // record.

        // TODO: How can we strip off the OPT record in the response if no OPT
        // record is present in the request?
        //
        // if request.opt().is_none() && response.opt().is_some() {
        // }

        // TODO: For non-error responses is it mandatory that the question
        // from the request be copied to the response? Unbound and domain
        // think so. If this has not been done, how should we react here?
    }
}
