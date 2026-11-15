//! A UDP socket with GSO sends and GRO receives (noq-udp over tokio).

use std::{
    io::{self, IoSliceMut},
    net::{IpAddr, SocketAddr},
};

use noq_udp::{EcnCodepoint, RecvMeta, Transmit, UdpSocketState};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::Interest;

/// Upper bound for the bytes of one GSO send (the UDP length field limit
/// minus headers, rounded down).
const MAX_GSO_BYTES: usize = 65000;

/// Requested kernel socket buffer size.
const SOCKET_BUFFER: usize = 2 * 1024 * 1024;

/// A bound UDP socket.
#[derive(Debug)]
pub(crate) struct Sock {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    v6: bool,
}

impl Sock {
    /// Binds a socket. `[::]` is bound dual-stack where the OS allows it.
    pub(crate) fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        if addr.is_ipv6() && addr.ip().is_unspecified() {
            // Best effort: some systems do not allow dual-stack sockets.
            socket.set_only_v6(false).ok();
        }
        // Best effort, capped by the OS (rmem_max/wmem_max on Linux).
        socket.set_recv_buffer_size(SOCKET_BUFFER).ok();
        socket.set_send_buffer_size(SOCKET_BUFFER).ok();
        socket.bind(&addr.into())?;
        socket.set_nonblocking(true)?;
        let std: std::net::UdpSocket = socket.into();
        let state = UdpSocketState::new((&std).into())?;
        let io = tokio::net::UdpSocket::from_std(std)?;
        Ok(Self {
            io,
            state,
            v6: addr.is_ipv6(),
        })
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.io.local_addr()
    }

    pub(crate) fn max_gso_segments(&self) -> usize {
        self.state.max_gso_segments().get()
    }

    pub(crate) fn gro_segments(&self) -> usize {
        self.state.gro_segments().get()
    }

    /// Sends `payload` to `dest`, split into `segment_size` datagrams if
    /// nonzero. Uses GSO where available and falls back to single sends.
    pub(crate) async fn send(
        &self,
        dest: SocketAddr,
        ecn: u8,
        segment_size: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        let dest = self.map_dest(dest)?;
        let ecn = EcnCodepoint::from_bits(ecn);
        let seg = segment_size as usize;
        if seg == 0 || seg >= payload.len() {
            return self.send_one(dest, ecn, payload, None).await;
        }
        let per_send = self.max_gso_segments().min(MAX_GSO_BYTES / seg).max(1);
        for chunk in payload.chunks(seg * per_send) {
            if per_send == 1 || chunk.len() <= seg {
                self.send_one(dest, ecn, chunk, None).await?;
                continue;
            }
            if let Err(err) = self.send_one(dest, ecn, chunk, Some(seg)).await {
                // noq-udp turns GSO off after EIO/EINVAL; resend one by one.
                if self.max_gso_segments() > 1 {
                    return Err(err);
                }
                for dgram in chunk.chunks(seg) {
                    self.send_one(dest, ecn, dgram, None).await?;
                }
            }
        }
        Ok(())
    }

    async fn send_one(
        &self,
        destination: SocketAddr,
        ecn: Option<EcnCodepoint>,
        contents: &[u8],
        segment_size: Option<usize>,
    ) -> io::Result<()> {
        let transmit = Transmit {
            destination,
            ecn,
            contents,
            segment_size,
            src_ip: None,
        };
        self.io
            .async_io(Interest::WRITABLE, || {
                self.state.try_send((&self.io).into(), &transmit)
            })
            .await
    }

    /// Maps the destination to the socket's address family.
    fn map_dest(&self, dest: SocketAddr) -> io::Result<SocketAddr> {
        match (self.v6, dest.ip()) {
            (true, IpAddr::V4(ip)) => Ok(SocketAddr::new(ip.to_ipv6_mapped().into(), dest.port())),
            (false, IpAddr::V6(_)) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv6 destination on an IPv4 socket",
            )),
            _ => Ok(dest),
        }
    }

    /// Waits until the socket may be readable.
    pub(crate) async fn readable(&self) -> io::Result<()> {
        self.io.readable().await
    }

    /// Receives a batch without waiting; `WouldBlock` if nothing is queued.
    pub(crate) fn try_recv(
        &self,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> io::Result<usize> {
        self.io.try_io(Interest::READABLE, || {
            self.state.recv((&self.io).into(), bufs, meta)
        })
    }
}

/// Returns the non-loopback interface addresses with the port of `local`.
///
/// For an unspecified IPv4 bind these are all IPv4 interface addresses, for
/// `[::]` IPv6 (without link-local) and IPv4 addresses. A specific bind
/// address is returned as is.
pub(crate) fn interface_addrs(local: SocketAddr) -> Vec<SocketAddr> {
    let port = local.port();
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let mut out = Vec::new();
    for iface in netdev::get_interfaces() {
        if !iface.is_up() || iface.is_loopback() {
            continue;
        }
        for net in &iface.ipv4 {
            out.push(SocketAddr::new(net.addr().into(), port));
        }
        if local.is_ipv6() {
            for net in &iface.ipv6 {
                let ip = net.addr();
                let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
                if !link_local && !ip.is_loopback() && !ip.is_unspecified() {
                    out.push(SocketAddr::new(ip.into(), port));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}
