//! A UDP socket with GSO sends and GRO receives (noq-udp over tokio).

use std::{
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

use noq_udp::{EcnCodepoint, RecvMeta, Transmit, UdpSocketState};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::Interest;

/// Upper bound for the bytes of one GSO send.
///
/// This is the limit of the UDP length field minus headers, rounded down.
const MAX_GSO_BYTES: usize = 65000;

/// Options for [`Sock::bind`].
#[derive(Debug, Clone)]
pub(crate) struct SockOptions {
    /// Sets `IPV6_V6ONLY` for a `[::]` bind.
    ///
    /// `None` means dual stack where the OS allows it.
    pub(crate) ipv6_only: Option<bool>,
    /// Requested kernel send buffer size, best effort.
    pub(crate) send_buffer: usize,
    /// Requested kernel receive buffer size, best effort.
    pub(crate) recv_buffer: usize,
}

/// A bound UDP socket.
#[derive(Debug)]
pub(crate) struct Sock {
    io: tokio::net::UdpSocket,
    state: UdpSocketState,
    family: Family,
}

/// The address family of a bound socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    V4,
    V6,
}

impl Sock {
    /// Binds a socket.
    ///
    /// `[::]` is bound dual-stack where the OS allows it.
    pub(crate) fn bind(addr: SocketAddr, options: &SockOptions) -> io::Result<Self> {
        let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        if addr.is_ipv6() && addr.ip().is_unspecified() {
            match options.ipv6_only {
                Some(only) => socket.set_only_v6(only)?,
                // Best effort: some systems do not allow dual-stack sockets.
                None => socket.set_only_v6(false).unwrap_or_default(),
            }
        }
        // Best effort, capped by the OS (rmem_max/wmem_max on Linux).
        socket.set_recv_buffer_size(options.recv_buffer).ok();
        socket.set_send_buffer_size(options.send_buffer).ok();
        socket.bind(&addr.into())?;
        socket.set_nonblocking(true)?;
        let std_socket: std::net::UdpSocket = socket.into();
        let state = UdpSocketState::new((&std_socket).into())?;
        let io = tokio::net::UdpSocket::from_std(std_socket)?;
        let family = match addr {
            SocketAddr::V4(_) => Family::V4,
            SocketAddr::V6(_) => Family::V6,
        };
        Ok(Self { io, state, family })
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

    /// Sends `payload` to `dest` as one or more datagrams.
    ///
    /// A nonzero `segment_size` splits the payload into datagrams of that
    /// size. Uses GSO where available and falls back to single sends.
    pub(crate) async fn send(
        &self,
        dest: SocketAddr,
        ecn: u8,
        segment_size: u16,
        payload: &[u8],
    ) -> io::Result<()> {
        let dest = self.map_dest(dest)?;
        let ecn = EcnCodepoint::from_bits(ecn);
        let segment_size = segment_size as usize;
        if segment_size == 0 || segment_size >= payload.len() {
            return self.send_one(dest, ecn, payload, None).await;
        }
        let per_send = self
            .max_gso_segments()
            .min(MAX_GSO_BYTES / segment_size)
            .max(1);
        for chunk in payload.chunks(segment_size * per_send) {
            if per_send == 1 || chunk.len() <= segment_size {
                self.send_one(dest, ecn, chunk, None).await?;
                continue;
            }
            if let Err(err) = self.send_one(dest, ecn, chunk, Some(segment_size)).await {
                // noq-udp turns GSO off after EIO or EINVAL. Resend one by one.
                if self.max_gso_segments() > 1 {
                    return Err(err);
                }
                for datagram in chunk.chunks(segment_size) {
                    self.send_one(dest, ecn, datagram, None).await?;
                }
            }
        }
        Ok(())
    }

    /// Sends one datagram, or one GSO batch if `segment_size` is set.
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
        match (self.family, dest.ip()) {
            (Family::V6, IpAddr::V4(ip)) => {
                Ok(SocketAddr::new(ip.to_ipv6_mapped().into(), dest.port()))
            }
            (Family::V4, IpAddr::V6(_)) => Err(io::Error::new(
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

    /// Receives a batch without waiting.
    ///
    /// Returns `WouldBlock` if nothing is queued.
    pub(crate) fn try_recv(
        &self,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> io::Result<usize> {
        self.io.try_io(Interest::READABLE, || {
            self.state.recv((&self.io).into(), buffers, meta)
        })
    }
}

/// Returns the local IP the OS would use to reach `remote`.
///
/// Connects a short-lived UDP socket, which sends nothing.
pub(crate) fn route_local_ip(remote: SocketAddr) -> io::Result<IpAddr> {
    let any: SocketAddr = match remote {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let probe = std::net::UdpSocket::bind(any)?;
    probe.connect(remote)?;
    Ok(probe.local_addr()?.ip())
}

/// Returns the non-loopback interface addresses with the port of `local`.
///
/// For an unspecified IPv4 bind these are all IPv4 interface addresses. For
/// `[::]` these are the IPv6 addresses without link-local ones, and the IPv4
/// addresses. A specific bind address is returned as is.
pub(crate) fn interface_addrs(local: SocketAddr) -> Vec<SocketAddr> {
    let port = local.port();
    if !local.ip().is_unspecified() {
        return vec![local];
    }
    let mut out = Vec::new();
    for interface in netdev::get_interfaces() {
        if !interface.is_up() || interface.is_loopback() {
            continue;
        }
        for network in &interface.ipv4 {
            out.push(SocketAddr::new(network.addr().into(), port));
        }
        if local.is_ipv6() {
            for network in &interface.ipv6 {
                let ip = network.addr();
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
