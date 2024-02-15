//! Servers for communicating with DNS clients.

use std::net::UdpSocket;

use tokio::net::TcpListener;

use self::{dgram::server::DgramServer, stream::server::StreamServer};

use super::buf::VecBufSource;

pub mod dgram;
pub mod stream;

/// A UDP transport connection-less based DNS server.
pub type UdpServer<Svc> = DgramServer<UdpSocket, VecBufSource, Svc>;

/// A TCP connection-oriented transport based DNS server.
pub type TcpServer<Svc> = StreamServer<TcpListener, VecBufSource, Svc>;