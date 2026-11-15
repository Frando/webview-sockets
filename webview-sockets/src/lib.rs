//! A UDP socket shaped like [`std::net::UdpSocket`] that also works in web
//! pages opened in iroh-link.
//!
//! There are three backends, chosen at compile time:
//!
//! - Native (everything except `wasm32-unknown-unknown`): a thin wrapper
//!   around [`std::net::UdpSocket`].
//! - `wasm32-unknown-unknown` with `--cfg wasm_native_udp`: the socket lives
//!   in the iroh-link app. The page asks `window.irohHost.udp.requestSession()`
//!   for a session (see [`init`]) and every socket opens one WebSocket to the
//!   host, speaking the `iroh-link-udp-proto` wire format.
//! - `wasm32-unknown-unknown` without the cfg: every call fails with
//!   [`io::ErrorKind::Unsupported`].
//!
//! # Behaviour on wasm
//!
//! JavaScript cannot block, so a socket behaves like a std socket after
//! `set_nonblocking(true)`: [`UdpSocket::send_to`] and
//! [`UdpSocket::recv_from`] return [`io::ErrorKind::WouldBlock`] instead of
//! blocking. Use `UdpSocket::poll_recv_ready` and
//! `UdpSocket::poll_send_ready` (or the `*_async` helpers) to wait.
//!
//! [`UdpSocket::bind`] is synchronous. It opens the WebSocket and returns at
//! once; until the host confirms the bind, [`UdpSocket::local_addr`] reports
//! the requested address and sends are queued. [`UdpSocket::bound`] waits for
//! the confirmation.
//!
//! Errors map as follows: the user or host denied access, or the host closed
//! the WebSocket because access was revoked (close code 4001) or the UDP
//! setting was switched off (4002) -> [`io::ErrorKind::PermissionDenied`];
//! any other WebSocket close -> [`io::ErrorKind::ConnectionAborted`]; a bad
//! address ->
//! [`io::ErrorKind::InvalidInput`], no `window.irohHost` ->
//! [`io::ErrorKind::Unsupported`].
//!
//! # GSO and GRO
//!
//! [`UdpSocket::send_to_segmented`] sends several datagrams of equal size in
//! one call and [`UdpSocket::recv_from_segmented`] may return several
//! datagrams received from the same peer at once. On native both fall back to
//! one datagram per syscall.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
#[cfg(target_family = "wasm")]
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(not(wasm_browser))]
#[path = "native.rs"]
mod imp;

#[cfg(all(wasm_browser, wasm_native_udp))]
#[path = "host.rs"]
mod imp;

#[cfg(all(wasm_browser, not(wasm_native_udp)))]
#[path = "unsupported.rs"]
mod imp;

/// Asks the iroh-link app for a UDP session.
///
/// On wasm with `wasm_native_udp` this calls
/// `window.irohHost.udp.requestSession()`, which may show a permission prompt.
/// It must succeed before [`UdpSocket::bind`]. Calling it again requests a new
/// session. In a Worker, use [`init_with_session`] instead.
///
/// On native this does nothing.
pub async fn init() -> io::Result<()> {
    imp::init().await
}

/// Uses a session obtained elsewhere, e.g. by the page for a Worker.
///
/// `url` is the WebSocket URL from `requestSession()` (absolute, or relative
/// to the current location), `token` the 64 hex char token.
///
/// On native this does nothing.
pub async fn init_with_session(url: &str, token: &str) -> io::Result<()> {
    imp::init_with_session(url, token)
}

/// Returns whether UDP sockets can work in this environment.
///
/// True on native. On wasm with `wasm_native_udp`, true if
/// `window.irohHost.udp` exists or a session was set. False otherwise.
pub fn is_available() -> bool {
    imp::is_available()
}

/// A UDP socket.
///
/// See the [crate docs](crate) for how it behaves on each target.
#[derive(Debug)]
pub struct UdpSocket {
    inner: imp::Socket,
}

impl UdpSocket {
    /// Creates a UDP socket bound to `addr`.
    ///
    /// Like [`std::net::UdpSocket::bind`], every address is tried in turn.
    /// On wasm only addresses that need no DNS lookup work, and the call
    /// returns before the host confirms the bind, see [`UdpSocket::bound`].
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let mut last = None;
        for addr in addr.to_socket_addrs()? {
            match imp::Socket::bind(addr) {
                Ok(inner) => return Ok(Self { inner }),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(no_addr))
    }

    /// Waits until the host confirmed the bind.
    ///
    /// Returns at once on native. On wasm it fails if the host refused the
    /// bind or the connection closed.
    pub async fn bound(&self) -> io::Result<()> {
        self.inner.bound().await
    }

    /// Returns the local address of the socket.
    ///
    /// On wasm this is the requested address until the host confirmed the
    /// bind.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Returns the local interface addresses with the bound port.
    ///
    /// On wasm the host reports them. On native this is just the local
    /// address if it is not unspecified.
    pub fn interface_addrs(&self) -> Vec<SocketAddr> {
        self.inner.interface_addrs()
    }

    /// Returns the address of the connected peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Connects the socket to a remote address.
    ///
    /// Afterwards [`UdpSocket::send`] goes to that address and only datagrams
    /// from it are received.
    pub fn connect(&self, addr: impl ToSocketAddrs) -> io::Result<()> {
        let addr = first_addr(addr)?;
        self.inner.connect(addr)
    }

    /// Sends a datagram to `addr`, returning the number of bytes sent.
    pub fn send_to(&self, buf: &[u8], addr: impl ToSocketAddrs) -> io::Result<usize> {
        let addr = first_addr(addr)?;
        self.inner.send_to(buf, addr)
    }

    /// Receives one datagram, returning its length and source.
    ///
    /// Like std, a datagram larger than `buf` is truncated.
    pub fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf)
    }

    /// Sends a datagram to the connected peer.
    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf)
    }

    /// Receives one datagram from the connected peer.
    pub fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf)
    }

    /// Moves the socket into or out of nonblocking mode.
    ///
    /// On wasm sockets are always nonblocking: `true` is accepted and `false`
    /// fails with [`io::ErrorKind::Unsupported`].
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }

    /// Sets the read timeout, `None` blocks forever.
    ///
    /// On wasm only `None` is accepted.
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(dur)
    }

    /// Sets the write timeout, `None` blocks forever.
    ///
    /// On wasm only `None` is accepted.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.inner.set_write_timeout(dur)
    }

    /// Returns the read timeout.
    pub fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.read_timeout()
    }

    /// Returns the write timeout.
    pub fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.inner.write_timeout()
    }

    /// Returns and clears a pending error.
    ///
    /// On wasm this is the last per-send error the host reported, e.g. a
    /// forbidden destination.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.inner.take_error()
    }

    /// Creates a new handle to the same socket.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
        })
    }

    /// Returns the maximum number of segments [`UdpSocket::send_to_segmented`]
    /// passes to the network in one go.
    ///
    /// 1 means no GSO. Native returns 1. On wasm the host reports it; it is 1
    /// until the bind is confirmed.
    pub fn max_gso_segments(&self) -> usize {
        self.inner.max_gso_segments()
    }

    /// Returns the maximum number of datagrams [`UdpSocket::recv_from_segmented`]
    /// may return at once.
    ///
    /// 1 means no GRO. Native returns 1.
    pub fn gro_segments(&self) -> usize {
        self.inner.gro_segments()
    }

    /// Sends `buf` as datagrams of `segment_size` bytes each to `addr`.
    ///
    /// The last datagram may be shorter. `segment_size` 0 sends one datagram.
    /// Batches larger than [`UdpSocket::max_gso_segments`] are split, so this
    /// works whatever the GSO support. Returns `buf.len()` on success.
    pub fn send_to_segmented(
        &self,
        buf: &[u8],
        addr: SocketAddr,
        segment_size: usize,
    ) -> io::Result<usize> {
        if segment_size > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "segment size larger than 65535",
            ));
        }
        self.inner.send_to_segmented(buf, addr, segment_size)
    }

    /// Receives one or more datagrams from the same source.
    ///
    /// Returns the total length, the source and the stride: every datagram
    /// in `buf[..len]` is `stride` bytes long, except the last which may be
    /// shorter. A single datagram has `stride == len`. Only whole datagrams
    /// that fit into `buf` are returned; a single datagram larger than `buf`
    /// is truncated.
    pub fn recv_from_segmented(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr, usize)> {
        self.inner.recv_from_segmented(buf)
    }
}

/// Readiness and async helpers, only on wasm targets.
///
/// On native, use a reactor such as tokio instead.
#[cfg(target_family = "wasm")]
impl UdpSocket {
    /// Polls until a datagram can be received or the socket failed.
    ///
    /// `Ready(Ok(()))` means [`UdpSocket::recv_from`] will not return
    /// [`io::ErrorKind::WouldBlock`] (unless another handle took the
    /// datagram).
    pub fn poll_recv_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_recv_ready(cx)
    }

    /// Polls until a datagram can be sent or the socket failed.
    ///
    /// Pending while more than 256 KiB wait to be written, until the backlog
    /// drops below 64 KiB.
    pub fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.poll_send_ready(cx)
    }

    /// Receives one datagram, waiting until one arrives.
    pub async fn recv_from_async(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            std::future::poll_fn(|cx| self.poll_recv_ready(cx)).await?;
            match self.recv_from(buf) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                res => return res,
            }
        }
    }

    /// Sends one datagram, waiting while the send queue is full.
    pub async fn send_to_async(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        loop {
            std::future::poll_fn(|cx| self.poll_send_ready(cx)).await?;
            match self.send_to(buf, addr) {
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                res => return res,
            }
        }
    }
}

fn first_addr(addr: impl ToSocketAddrs) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?.next().ok_or_else(no_addr)
}

fn no_addr() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "no socket address given")
}

#[cfg(test)]
mod tests;
