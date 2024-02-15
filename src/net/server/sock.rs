//! Network socket abstractions.
//!
//! TODO: When Rust gains more support for async fns in traits consider if it
//! is possible then to modify functions that return [`Poll`] to be async fns
//! instead.

use futures::Future;
use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

//------------ AsyncDgramSock ------------------------------------------------

/// Asynchronous sending of datagrams.
pub trait AsyncDgramSock {
    fn poll_send_to(
        &self,
        cx: &mut Context,
        data: &[u8],
        dest: &SocketAddr,
    ) -> Poll<io::Result<usize>>;

    fn poll_recv_from(
        &self,
        cx: &mut Context,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>>;

    fn poll_peek_from(
        &self,
        cx: &mut Context,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>>;
}

impl AsyncDgramSock for UdpSocket {
    fn poll_send_to(
        &self,
        cx: &mut Context,
        data: &[u8],
        dest: &SocketAddr,
    ) -> Poll<io::Result<usize>> {
        UdpSocket::poll_send_to(self, cx, data, *dest)
    }

    fn poll_recv_from(
        &self,
        cx: &mut Context,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        UdpSocket::poll_recv_from(self, cx, buf)
    }

    fn poll_peek_from(
        &self,
        cx: &mut Context,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<SocketAddr>> {
        UdpSocket::poll_peek_from(self, cx, buf)
    }
}

//------------ AsyncAccept ---------------------------------------------------

pub trait AsyncAccept {
    type Error: Send;
    type StreamType: AsyncRead + AsyncWrite + Send + Sync + 'static;
    type Stream: Future<Output = Result<Self::StreamType, Self::Error>> + Send;

    #[allow(clippy::type_complexity)]
    fn poll_accept(
        &self,
        cx: &mut Context,
    ) -> Poll<io::Result<(Self::Stream, SocketAddr)>>;
}

impl AsyncAccept for TcpListener {
    type Error = io::Error;
    type StreamType = TcpStream;
    type Stream = futures::future::Ready<Result<Self::StreamType, io::Error>>;

    #[allow(clippy::type_complexity)]
    fn poll_accept(
        &self,
        cx: &mut Context,
    ) -> Poll<io::Result<(Self::Stream, SocketAddr)>> {
        TcpListener::poll_accept(self, cx).map(|res| {
            // TODO: Should we support some sort of callback here to set
            // arbitrary socket options? E.g. TCP keep alive ala
            // https://stackoverflow.com/a/75697898 ? Or is it okay that this
            // is the plain implementation and users who want to set things
            // like TCP keep alive would need to provide their own impl? (just
            // as the serve example currently does).
            res.map(|(stream, addr)| {
                (futures::future::ready(Ok(stream)), addr)
            })
        })
    }
}