//! Backend for wasm without `--cfg webview_sockets`: nothing works.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use std::time::Duration;

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "UDP needs webview-sockets built with --cfg webview_sockets",
    )
}

pub(crate) async fn init() -> io::Result<()> {
    Err(unsupported())
}

pub(crate) fn init_with_session(_url: &str, _token: &str) -> io::Result<()> {
    Err(unsupported())
}

pub(crate) fn is_available() -> bool {
    false
}

/// A socket that cannot exist: [`Socket::bind`] always fails.
#[derive(Debug)]
pub(crate) struct Socket(Infallible);

impl Socket {
    pub(crate) fn bind(_addr: SocketAddr) -> io::Result<Self> {
        Err(unsupported())
    }

    pub(crate) fn connect_host(
        _host: &str,
        _port: u16,
        _dns: Option<crate::DnsQueryType>,
    ) -> io::Result<Self> {
        Err(unsupported())
    }

    pub(crate) async fn bound(&self) -> io::Result<()> {
        match self.0 {}
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        match self.0 {}
    }

    pub(crate) fn interface_addrs(&self) -> Vec<SocketAddr> {
        match self.0 {}
    }

    pub(crate) fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self.0 {}
    }

    pub(crate) fn connect(&self, _addr: SocketAddr) -> io::Result<()> {
        match self.0 {}
    }

    pub(crate) fn send_to(&self, _buf: &[u8], _addr: SocketAddr) -> io::Result<usize> {
        match self.0 {}
    }

    pub(crate) fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self.0 {}
    }

    pub(crate) fn send(&self, _buf: &[u8]) -> io::Result<usize> {
        match self.0 {}
    }

    pub(crate) fn recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
        match self.0 {}
    }

    pub(crate) fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        match self.0 {}
    }

    pub(crate) fn set_read_timeout(&self, _dur: Option<Duration>) -> io::Result<()> {
        match self.0 {}
    }

    pub(crate) fn set_write_timeout(&self, _dur: Option<Duration>) -> io::Result<()> {
        match self.0 {}
    }

    pub(crate) fn read_timeout(&self) -> io::Result<Option<Duration>> {
        match self.0 {}
    }

    pub(crate) fn write_timeout(&self) -> io::Result<Option<Duration>> {
        match self.0 {}
    }

    pub(crate) fn take_error(&self) -> io::Result<Option<io::Error>> {
        match self.0 {}
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        match self.0 {}
    }

    pub(crate) fn max_gso_segments(&self) -> usize {
        match self.0 {}
    }

    pub(crate) fn gro_segments(&self) -> usize {
        match self.0 {}
    }

    pub(crate) fn send_to_segmented(
        &self,
        _buf: &[u8],
        _addr: SocketAddr,
        _segment_size: usize,
    ) -> io::Result<usize> {
        match self.0 {}
    }

    pub(crate) fn recv_from_segmented(
        &self,
        _buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, usize)> {
        match self.0 {}
    }

    pub(crate) fn poll_recv_ready(&self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.0 {}
    }

    pub(crate) fn poll_send_ready(&self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.0 {}
    }
}
