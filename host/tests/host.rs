//! End-to-end tests of the UDP host over a real WebSocket.

use std::{net::SocketAddr, time::Duration};

use futures_util::{SinkExt, StreamExt};
use iroh_link_udp_host::{Choice, Decision, Grant, Limits, TabId, UdpHost, UdpHostConfig, WS_PATH};
use iroh_link_udp_proto::{
    Bound, Data, ErrorCode, Frame, Hello, close_code, decode_data, decode_first, encode_data,
    encode_hello,
};
use tokio::net::{TcpStream, UdpSocket};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const TIMEOUT: Duration = Duration::from_secs(5);

struct Env {
    host: UdpHost,
    addr: SocketAddr,
    dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
        Self::with_limits(Limits::default()).await
    }

    async fn with_limits(limits: Limits) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = UdpHostConfig::new(dir.path().join("udp-grants.json"));
        config.limits = limits;
        let host = UdpHost::new(config).unwrap();
        host.set_enabled(true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = host.router();
        tokio::spawn(async move { axum::serve(listener, router).await });
        Self { host, addr, dir }
    }

    fn authority(&self, label: &str) -> String {
        format!("{label}.pkarr.localhost:{}", self.addr.port())
    }

    fn origin(&self, label: &str) -> String {
        format!("http://{}", self.authority(label))
    }

    /// Grants `label` always and returns a token for `tab`.
    fn token(&self, label: &str, tab: u64) -> [u8; 32] {
        let ticket = self
            .host
            .answer(&self.origin(label), TabId(tab), Choice::AllowAlways)
            .unwrap();
        assert_eq!(
            ticket.url,
            format!("ws://{}{WS_PATH}", self.authority(label))
        );
        decode_token(&ticket.token)
    }

    async fn connect_raw(
        &self,
        host: Option<&str>,
        origin: Option<&str>,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let mut req = format!("ws://{}{WS_PATH}", self.addr)
            .into_client_request()
            .unwrap();
        let headers = req.headers_mut();
        match host {
            Some(h) => headers.insert("host", HeaderValue::from_str(h).unwrap()),
            None => headers.remove("host"),
        };
        if let Some(o) = origin {
            headers.insert("origin", HeaderValue::from_str(o).unwrap());
        }
        tokio_tungstenite::connect_async(req)
            .await
            .map(|(ws, _)| ws)
    }

    async fn connect(&self, label: &str) -> Ws {
        let authority = self.authority(label);
        let origin = self.origin(label);
        self.connect_raw(Some(&authority), Some(&origin))
            .await
            .unwrap()
    }

    /// Connects, sends HELLO and returns the first frame.
    async fn open(&self, label: &str, token: [u8; 32], bind: &str) -> (Ws, Reply) {
        let mut ws = self.connect(label).await;
        let mut hello = Vec::new();
        encode_hello(
            &Hello {
                token,
                bind_addr: bind.parse().unwrap(),
            },
            &mut hello,
        );
        ws.send(Message::Binary(hello.into())).await.unwrap();
        let reply = first_reply(&mut ws).await;
        (ws, reply)
    }

    async fn bound(&self, label: &str, token: [u8; 32]) -> (Ws, Bound) {
        match self.open(label, token, "0.0.0.0:0").await {
            (ws, Reply::Bound(b)) => (ws, b),
            (_, other) => panic!("expected BOUND, got {other:?}"),
        }
    }
}

#[derive(Debug)]
enum Reply {
    Bound(Bound),
    Error(ErrorCode, #[allow(dead_code)] String),
}

fn decode_token(hex: &str) -> [u8; 32] {
    assert_eq!(hex.len(), 64);
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

async fn next_msg(ws: &mut Ws) -> Option<Message> {
    loop {
        match tokio::time::timeout(TIMEOUT, ws.next())
            .await
            .expect("timeout")
        {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            Some(Ok(msg)) => return Some(msg),
            Some(Err(_)) | None => return None,
        }
    }
}

async fn first_reply(ws: &mut Ws) -> Reply {
    let Some(Message::Binary(msg)) = next_msg(ws).await else {
        panic!("expected a binary message");
    };
    match decode_first(&msg).unwrap() {
        Frame::Bound(b) => Reply::Bound(b),
        Frame::Error(e) => Reply::Error(e.code, e.message.to_string()),
        Frame::Hello(_) => panic!("unexpected HELLO"),
    }
}

/// Reads until the close frame and returns its code.
async fn close_code_of(ws: &mut Ws) -> u16 {
    loop {
        match next_msg(ws).await {
            Some(Message::Close(Some(frame))) => return frame.code.into(),
            Some(Message::Close(None)) | None => panic!("closed without a code"),
            Some(_) => continue,
        }
    }
}

async fn expect_rejected(ws: &mut Ws, code: ErrorCode, close: u16) {
    match first_reply(ws).await {
        Reply::Error(c, _) => assert_eq!(c, code),
        other => panic!("expected ERROR {code:?}, got {other:?}"),
    }
    assert_eq!(close_code_of(ws).await, close);
}

async fn send_data(ws: &mut Ws, to: SocketAddr, segment_size: u16, payload: &[u8]) {
    let mut msg = Vec::new();
    encode_data(
        &Data {
            ecn: 0,
            peer: to,
            segment_size,
            payload,
        },
        &mut msg,
    );
    ws.send(Message::Binary(msg.into())).await.unwrap();
}

/// Reads the next DATA message and returns (peer, datagrams).
async fn recv_data(ws: &mut Ws) -> Vec<(SocketAddr, Vec<u8>)> {
    let Some(Message::Binary(msg)) = next_msg(ws).await else {
        panic!("expected a binary message");
    };
    let mut out = Vec::new();
    for rec in decode_data(&msg).unwrap() {
        let rec = rec.unwrap();
        for seg in rec.segments() {
            out.push((rec.peer, seg.to_vec()));
        }
    }
    out
}

fn spawn_echo() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let sock = UdpSocket::from_std(sock).unwrap();
    let addr = sock.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            let (n, from) = sock.recv_from(&mut buf).await.unwrap();
            sock.send_to(&buf[..n], from).await.unwrap();
        }
    });
    (addr, task)
}

#[tokio::test]
async fn upgrade_checks() {
    let env = Env::new().await;
    let good_host = env.authority("app");
    let good_origin = env.origin("app");
    assert!(
        env.connect_raw(Some(&good_host), Some(&good_origin))
            .await
            .is_ok()
    );
    // Bad Host.
    let ip_host = env.addr.to_string();
    let ip_origin = format!("http://{ip_host}");
    assert!(
        env.connect_raw(Some(&ip_host), Some(&ip_origin))
            .await
            .is_err()
    );
    let bad = format!("Ab.pkarr.localhost:{}", env.addr.port());
    assert!(
        env.connect_raw(Some(&bad), Some(&format!("http://{bad}")))
            .await
            .is_err()
    );
    // Bad or missing Origin.
    assert!(
        env.connect_raw(Some(&good_host), Some(&env.origin("other")))
            .await
            .is_err()
    );
    assert!(
        env.connect_raw(Some(&good_host), Some("null"))
            .await
            .is_err()
    );
    assert!(env.connect_raw(Some(&good_host), None).await.is_err());
    // Setting off.
    env.host.set_enabled(false);
    assert!(
        env.connect_raw(Some(&good_host), Some(&good_origin))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn token_checks() {
    let env = Env::with_limits(Limits {
        hello_timeout: Duration::from_millis(200),
        ..Default::default()
    })
    .await;
    // Unknown token.
    let (mut ws, reply) = env.open("app", [7; 32], "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::NotAllowed, _)));
    assert_eq!(close_code_of(&mut ws).await, 1008);

    // Token for another origin.
    let token = env.token("app", 1);
    let (mut ws, reply) = env.open("other", token, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::NotAllowed, _)));
    assert_eq!(close_code_of(&mut ws).await, 1008);

    // Token expired by navigation.
    env.host.tab_navigated(TabId(1));
    let (mut ws, reply) = env.open("app", token, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::NotAllowed, _)));
    assert_eq!(close_code_of(&mut ws).await, 1008);

    // No HELLO in time.
    let mut ws = env.connect("app").await;
    expect_rejected(&mut ws, ErrorCode::InvalidInput, 1008).await;

    // Bad bind address.
    let token = env.token("app", 1);
    let (mut ws, reply) = env.open("app", token, "0.0.0.0:80").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::InvalidInput, _)));
    assert_eq!(close_code_of(&mut ws).await, 1008);

    // Setting off invalidates tokens.
    env.host.set_enabled(false);
    env.host.set_enabled(true);
    let (_, reply) = env.open("app", token, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::NotAllowed, _)));
}

#[tokio::test]
async fn happy_path_echo() {
    let env = Env::new().await;
    let (echo, _task) = spawn_echo();
    let token = env.token("app", 1);
    let (mut ws, bound) = env.bound("app", token).await;
    assert_ne!(bound.local.port(), 0);
    assert!(bound.max_gso_segments >= 1);
    assert!(bound.gro_segments >= 1);
    assert!(
        bound
            .interface_addrs
            .iter()
            .all(|a| a.port() == bound.local.port())
    );
    assert_eq!(*env.host.active_tabs().borrow(), [TabId(1)].into());

    send_data(&mut ws, echo, 0, b"hello").await;
    let got = recv_data(&mut ws).await;
    assert_eq!(got, vec![(echo, b"hello".to_vec())]);

    // An empty datagram round trips too.
    send_data(&mut ws, echo, 0, b"").await;
    let got = recv_data(&mut ws).await;
    assert_eq!(got, vec![(echo, Vec::new())]);

    ws.close(None).await.unwrap();
    tokio::time::timeout(TIMEOUT, async {
        let mut rx = env.host.active_tabs();
        rx.wait_for(|t| t.is_empty()).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn ipv6_dual_stack_bind() {
    let env = Env::new().await;
    let (echo, _task) = spawn_echo();
    let token = env.token("app", 1);
    let (mut ws, reply) = env.open("app", token, "[::]:0").await;
    let Reply::Bound(bound) = reply else {
        // No IPv6 on this machine.
        return;
    };
    assert!(bound.local.is_ipv6());
    send_data(&mut ws, echo, 0, b"v4 via v6").await;
    let got = recv_data(&mut ws).await;
    // The source is reported as plain IPv4.
    assert_eq!(got, vec![(echo, b"v4 via v6".to_vec())]);
}

#[tokio::test]
async fn forbidden_destinations() {
    let env = Env::new().await;
    let (echo, _task) = spawn_echo();
    let token = env.token("app", 1);
    let (mut ws, _) = env.bound("app", token).await;
    for dest in [
        "224.0.0.1:9",
        "0.0.0.0:9",
        "255.255.255.255:9",
        "127.0.0.1:0",
        "[ff02::1]:9",
    ] {
        send_data(&mut ws, dest.parse().unwrap(), 0, b"x").await;
        match first_reply(&mut ws).await {
            Reply::Error(ErrorCode::InvalidInput, _) => {}
            other => panic!("{dest}: expected InvalidInput, got {other:?}"),
        }
    }
    // The socket is still usable; loopback is allowed.
    send_data(&mut ws, echo, 0, b"still open").await;
    assert_eq!(
        recv_data(&mut ws).await,
        vec![(echo, b"still open".to_vec())]
    );
}

#[tokio::test]
async fn gso_send_splits_segments() {
    let env = Env::new().await;
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let token = env.token("app", 1);
    let (mut ws, bound) = env.bound("app", token).await;

    let payload: Vec<u8> = (0..350u32).map(|i| i as u8).collect();
    send_data(&mut ws, peer_addr, 100, &payload).await;
    let mut buf = [0u8; 2048];
    let mut got = Vec::new();
    for _ in 0..4 {
        let (n, from) = tokio::time::timeout(TIMEOUT, peer.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from.port(), bound.local.port());
        got.push(buf[..n].to_vec());
    }
    let expected: Vec<Vec<u8>> = payload.chunks(100).map(|c| c.to_vec()).collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn receive_many_datagrams() {
    let env = Env::new().await;
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    let token = env.token("app", 1);
    let (mut ws, bound) = env.bound("app", token).await;
    let to: SocketAddr = format!("127.0.0.1:{}", bound.local.port()).parse().unwrap();
    // Stay below the default socket receive buffer (about 90 datagrams of
    // this size on Linux), the sender does not pace.
    let n = 50;
    for i in 0..n {
        peer.send_to(&[i as u8; 1200], to).await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < n {
        got.extend(recv_data(&mut ws).await);
    }
    assert_eq!(got.len(), n);
    for (i, (from, data)) in got.iter().enumerate() {
        assert_eq!(*from, peer_addr);
        assert_eq!(data, &vec![i as u8; 1200]);
    }
}

#[tokio::test]
async fn revoke_and_tab_lifecycle_close_codes() {
    let env = Env::new().await;
    let t1 = env.token("app", 1);
    let t2 = env.token("other", 2);
    let (mut ws1, _) = env.bound("app", t1).await;
    let (mut ws2, _) = env.bound("other", t2).await;
    assert_eq!(
        *env.host.active_tabs().borrow(),
        [TabId(1), TabId(2)].into()
    );

    env.host.revoke(&env.origin("app"));
    assert_eq!(close_code_of(&mut ws1).await, close_code::REVOKED);
    assert_eq!(env.host.grants(), vec![(env.origin("other"), Grant::Allow)]);
    assert_eq!(
        env.host.request(&env.origin("app"), TabId(1)),
        Decision::NeedPrompt
    );

    env.host.tab_closed(TabId(2));
    assert_eq!(close_code_of(&mut ws2).await, close_code::NAVIGATED);

    let t2 = env.token("other", 3);
    let (mut ws3, _) = env.bound("other", t2).await;
    env.host.tab_navigated(TabId(3));
    assert_eq!(close_code_of(&mut ws3).await, close_code::NAVIGATED);

    let t4 = env.token("other", 4);
    let (mut ws4, _) = env.bound("other", t4).await;
    env.host.set_enabled(false);
    assert_eq!(close_code_of(&mut ws4).await, close_code::SETTING_OFF);

    tokio::time::timeout(TIMEOUT, async {
        let mut rx = env.host.active_tabs();
        rx.wait_for(|t| t.is_empty()).await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn block_closes_and_denies() {
    let env = Env::new().await;
    let origin = env.origin("app");
    assert!(
        env.host
            .answer(&origin, TabId(1), Choice::AllowOnce)
            .is_some()
    );
    let ticket = env.host.request(&origin, TabId(1));
    let Decision::Granted(ticket) = ticket else {
        panic!("expected granted");
    };
    let (mut ws, _) = env.bound("app", decode_token(&ticket.token)).await;
    assert!(env.host.answer(&origin, TabId(1), Choice::Block).is_none());
    assert_eq!(close_code_of(&mut ws).await, close_code::REVOKED);
    assert_eq!(env.host.request(&origin, TabId(1)), Decision::Denied);
}

#[tokio::test]
async fn decisions_and_persistence() {
    let env = Env::new().await;
    let path = env.dir.path().join("udp-grants.json");
    let a = env.origin("a");
    let b = env.origin("b");
    let c = env.origin("c");
    assert_eq!(env.host.request(&a, TabId(1)), Decision::NeedPrompt);
    assert!(env.host.answer(&a, TabId(1), Choice::AllowAlways).is_some());
    assert!(env.host.answer(&b, TabId(1), Choice::Block).is_none());
    assert!(env.host.answer(&c, TabId(1), Choice::AllowOnce).is_some());
    // Non-gateway origins never get a ticket.
    assert_eq!(
        env.host.request("http://example.com", TabId(1)),
        Decision::Denied
    );
    assert!(
        env.host
            .answer("https://example.com", TabId(1), Choice::AllowAlways)
            .is_none()
    );

    // Same token for the same (origin, tab).
    let Decision::Granted(t1) = env.host.request(&a, TabId(1)) else {
        panic!()
    };
    let Decision::Granted(t2) = env.host.request(&a, TabId(1)) else {
        panic!()
    };
    assert_eq!(t1, t2);
    let Decision::Granted(t3) = env.host.request(&a, TabId(2)) else {
        panic!()
    };
    assert_ne!(t1.token, t3.token);

    let host2 = UdpHost::new(UdpHostConfig::new(&path)).unwrap();
    assert!(host2.enabled());
    assert!(matches!(host2.request(&a, TabId(9)), Decision::Granted(_)));
    assert_eq!(host2.request(&b, TabId(9)), Decision::Denied);
    // Allow once is not persisted.
    assert_eq!(host2.request(&c, TabId(1)), Decision::NeedPrompt);
    assert_eq!(
        host2.grants(),
        vec![(a.clone(), Grant::Allow), (b.clone(), Grant::Block)]
    );
    // The running host still accepts its token.
    let (_, reply) = env.open("a", decode_token(&t1.token), "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Bound(_)));

    host2.set_enabled(false);
    assert_eq!(host2.request(&a, TabId(1)), Decision::Disabled);
    let host3 = UdpHost::new(UdpHostConfig::new(&path)).unwrap();
    assert!(!host3.enabled());

    // Off by default.
    let fresh = UdpHost::new(UdpHostConfig::new(env.dir.path().join("none.json"))).unwrap();
    assert!(!fresh.enabled());
    assert_eq!(fresh.request(&a, TabId(1)), Decision::Disabled);
    assert!(fresh.answer(&a, TabId(1), Choice::AllowAlways).is_none());
}

#[tokio::test]
async fn allow_once_scope() {
    let env = Env::new().await;
    let origin = env.origin("app");
    let ticket = env
        .host
        .answer(&origin, TabId(1), Choice::AllowOnce)
        .unwrap();
    assert_eq!(
        env.host.request(&origin, TabId(1)),
        Decision::Granted(ticket.clone())
    );
    assert_eq!(env.host.request(&origin, TabId(2)), Decision::NeedPrompt);
    assert!(env.host.grants().is_empty());
    env.host.tab_navigated(TabId(1));
    assert_eq!(env.host.request(&origin, TabId(1)), Decision::NeedPrompt);
    let (_, reply) = env
        .open("app", decode_token(&ticket.token), "0.0.0.0:0")
        .await;
    assert!(matches!(reply, Reply::Error(ErrorCode::NotAllowed, _)));
}

#[tokio::test]
async fn ninth_socket_rejected() {
    let env = Env::new().await;
    let token = env.token("app", 1);
    let mut open = Vec::new();
    for _ in 0..8 {
        open.push(env.bound("app", token).await.0);
    }
    let (mut ws, reply) = env.open("app", token, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::Limit, _)));
    assert_eq!(close_code_of(&mut ws).await, close_code::LIMIT);

    // The per-origin limit also applies across tabs.
    let other_tab = env.token("app", 2);
    let (_, reply) = env.open("app", other_tab, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Error(ErrorCode::Limit, _)));

    // Closing one frees a slot.
    let mut first = open.remove(0);
    first.close(None).await.unwrap();
    while next_msg(&mut first).await.is_some() {}
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, reply) = env.open("app", token, "0.0.0.0:0").await;
    assert!(matches!(reply, Reply::Bound(_)));
}
