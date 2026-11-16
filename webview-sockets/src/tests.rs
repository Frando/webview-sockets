//! Native tests: two sockets on loopback talking to each other.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use super::UdpSocket;

fn pair() -> (UdpSocket, UdpSocket) {
    let a = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a");
    let b = UdpSocket::bind("127.0.0.1:0").expect("bind b");
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
    }
    (a, b)
}

#[test]
fn echo_send_to_recv_from() {
    let (a, b) = pair();
    let b_addr = b.local_addr().expect("addr");
    a.send_to(b"hello", b_addr).expect("send");
    let mut buf = [0u8; 64];
    let (n, from) = b.recv_from(&mut buf).expect("recv");
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(from, a.local_addr().expect("addr"));
    b.send_to(&buf[..n], from).expect("echo");
    let (n, from) = a.recv_from(&mut buf).expect("recv echo");
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(from, b_addr);
}

#[test]
fn connected_send_recv() {
    let (a, b) = pair();
    a.connect(b.local_addr().expect("addr")).expect("connect");
    b.connect(a.local_addr().expect("addr")).expect("connect");
    assert_eq!(a.peer_addr().expect("peer"), b.local_addr().expect("addr"));
    a.send(b"ping").expect("send");
    let mut buf = [0u8; 16];
    let n = b.recv(&mut buf).expect("recv");
    assert_eq!(&buf[..n], b"ping");
}

#[test]
fn segmented_fallback() {
    let (a, b) = pair();
    assert_eq!(a.max_gso_segments(), 1);
    assert_eq!(b.gro_segments(), 1);
    let data: Vec<u8> = (0..25u8).collect();
    let n = a
        .send_to_segmented(&data, b.local_addr().expect("addr"), 10)
        .expect("send");
    assert_eq!(n, 25);
    let mut buf = [0u8; 64];
    for expected in data.chunks(10) {
        let (n, _from, stride) = b.recv_from_segmented(&mut buf).expect("recv");
        assert_eq!(&buf[..n], expected);
        assert_eq!(stride, n);
    }
}

#[test]
fn nonblocking_would_block() {
    let (a, _b) = pair();
    a.set_nonblocking(true).expect("nonblocking");
    let mut buf = [0u8; 16];
    let err = a.recv_from(&mut buf).expect_err("nothing to receive");
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn interface_addrs_and_init() {
    block_on_ready(super::init()).expect("init is a no-op");
    assert!(super::is_available());
    let s = UdpSocket::bind("0.0.0.0:0").expect("bind");
    assert!(s.interface_addrs().is_empty());
    let s = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let addr: SocketAddr = s.local_addr().expect("addr");
    assert_eq!(s.interface_addrs(), vec![addr]);
    block_on_ready(s.bound()).expect("bound");
}

#[test]
fn bad_segment_size() {
    let (a, b) = pair();
    let err = a
        .send_to_segmented(&[0; 10], b.local_addr().expect("addr"), 70_000)
        .expect_err("too large");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

/// Polls a future that is ready immediately.
fn block_on_ready<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(out) => out,
        std::task::Poll::Pending => panic!("future not ready"),
    }
}

#[test]
fn connect_host_by_name() {
    let b = UdpSocket::bind("127.0.0.1:0").expect("bind");
    b.set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let port = b.local_addr().expect("addr").port();
    let a = UdpSocket::connect_host("localhost", port, Some(super::DnsQueryType::Ipv4))
        .expect("connect");
    assert_eq!(
        a.peer_addr().expect("peer"),
        SocketAddr::from((Ipv4Addr::LOCALHOST, port))
    );
    a.send(b"named").expect("send");
    let mut buf = [0u8; 16];
    let (n, from) = b.recv_from(&mut buf).expect("recv");
    assert_eq!(&buf[..n], b"named");
    assert_eq!(from, a.local_addr().expect("addr"));
    let literal = UdpSocket::connect_host("[::1]", port, None);
    if let Ok(s) = literal {
        assert!(s.peer_addr().expect("peer").is_ipv6());
    }
    let err = UdpSocket::connect_host("localhost", port, Some(super::DnsQueryType::Ipv6))
        .and_then(|s| s.peer_addr());
    // localhost may or may not resolve to ::1.
    if let Ok(peer) = err {
        assert!(peer.is_ipv6());
    }
}
