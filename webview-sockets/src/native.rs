//! Native backend: a thin wrapper around [`std::net::UdpSocket`].

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

pub(crate) async fn init() -> io::Result<()> {
    Ok(())
}

pub(crate) fn init_with_session(_url: &str, _token: &str) -> io::Result<()> {
    Ok(())
}

pub(crate) fn is_available() -> bool {
    true
}

#[derive(Debug)]
pub(crate) struct Socket(std::net::UdpSocket);

impl Socket {
    pub(crate) fn bind(addr: SocketAddr) -> io::Result<Self> {
        std::net::UdpSocket::bind(addr).map(Self)
    }

    pub(crate) fn connect_host(
        host: &str,
        port: u16,
        dns: Option<crate::DnsQueryType>,
    ) -> io::Result<Self> {
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        let remote = (host, port)
            .to_socket_addrs()?
            .find(|a| match dns {
                None => true,
                Some(crate::DnsQueryType::Ipv4) => a.is_ipv4(),
                Some(crate::DnsQueryType::Ipv6) => a.is_ipv6(),
            })
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no address for {host}"))
            })?;
        let any: SocketAddr = match remote {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let sock = std::net::UdpSocket::bind(any)?;
        sock.connect(remote)?;
        Ok(Self(sock))
    }

    pub(crate) async fn bound(&self) -> io::Result<()> {
        Ok(())
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        self.0.local_addr()
    }

    pub(crate) fn interface_addrs(&self) -> Vec<SocketAddr> {
        match self.0.local_addr() {
            Ok(addr) if !addr.ip().is_unspecified() => vec![addr],
            _ => Vec::new(),
        }
    }

    pub(crate) fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.0.peer_addr()
    }

    pub(crate) fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        self.0.connect(addr)
    }

    pub(crate) fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.0.send_to(buf, addr)
    }

    pub(crate) fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.0.recv_from(buf)
    }

    pub(crate) fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf)
    }

    pub(crate) fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buf)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.0.set_nonblocking(nonblocking)
    }

    pub(crate) fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.0.set_read_timeout(dur)
    }

    pub(crate) fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.0.set_write_timeout(dur)
    }

    pub(crate) fn read_timeout(&self) -> io::Result<Option<Duration>> {
        self.0.read_timeout()
    }

    pub(crate) fn write_timeout(&self) -> io::Result<Option<Duration>> {
        self.0.write_timeout()
    }

    pub(crate) fn take_error(&self) -> io::Result<Option<io::Error>> {
        self.0.take_error()
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    pub(crate) fn max_gso_segments(&self) -> usize {
        1
    }

    pub(crate) fn gro_segments(&self) -> usize {
        1
    }

    pub(crate) fn send_to_segmented(
        &self,
        buf: &[u8],
        addr: SocketAddr,
        segment_size: usize,
    ) -> io::Result<usize> {
        if segment_size == 0 || segment_size >= buf.len() {
            return self.0.send_to(buf, addr);
        }
        for chunk in buf.chunks(segment_size) {
            self.0.send_to(chunk, addr)?;
        }
        Ok(buf.len())
    }

    pub(crate) fn recv_from_segmented(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, usize)> {
        let (len, from) = self.0.recv_from(buf)?;
        Ok((len, from, len))
    }
}
