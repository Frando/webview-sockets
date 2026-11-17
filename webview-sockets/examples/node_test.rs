//! End-to-end checks of the host backend, run under Node against a mock host.
//!
//! Build for `wasm32-unknown-unknown` with `--cfg webview_sockets` and run
//! `tests/node/run.sh`. On other targets this example is empty.

#[cfg(all(wasm_browser, webview_sockets))]
mod imp {
    use std::io;
    use std::net::SocketAddr;

    use wasm_bindgen::prelude::*;
    use webview_sockets::UdpSocket;

    fn fail(context: &str, err: impl std::fmt::Display) -> JsValue {
        JsValue::from_str(&format!("{context}: {err}"))
    }

    fn ensure(ok: bool, what: &str) -> Result<(), JsValue> {
        if ok {
            Ok(())
        } else {
            Err(fail("check failed", what))
        }
    }

    async fn recv(sock: &UdpSocket) -> Result<(Vec<u8>, SocketAddr), JsValue> {
        let mut buf = vec![0u8; 2048];
        let (n, from) = sock
            .recv_from_async(&mut buf)
            .await
            .map_err(|err| fail("recv", err))?;
        buf.truncate(n);
        Ok((buf, from))
    }

    /// Calls `init()` and returns `"ok"` or the error kind.
    #[wasm_bindgen]
    pub async fn init_kind() -> String {
        match webview_sockets::init().await {
            Ok(()) => "ok".into(),
            Err(err) => format!("{:?}", err.kind()),
        }
    }

    /// Binds `addr` with a session and returns `"ok"` or the error kind.
    #[wasm_bindgen]
    pub async fn bind_kind(url: String, token: String, addr: String) -> String {
        let res = async {
            webview_sockets::init_with_session(&url, &token).await?;
            let sock = UdpSocket::bind(addr.as_str())?;
            sock.bound().await
        };
        match res.await {
            Ok(()) => "ok".into(),
            Err(err) => format!("{:?}", err.kind()),
        }
    }

    /// Runs the main scenario against the UDP echo server at `echo`.
    ///
    /// Expects `init()` to succeed (the caller installs `webviewSockets`).
    #[wasm_bindgen]
    pub async fn run(echo: String) -> Result<String, JsValue> {
        let echo: SocketAddr = echo.parse().map_err(|err| fail("echo addr", err))?;
        webview_sockets::init()
            .await
            .map_err(|err| fail("init", err))?;
        let mut report = Vec::new();

        // Bind is sync; sends queue until BOUND.
        let sock = UdpSocket::bind("127.0.0.1:0").map_err(|err| fail("bind", err))?;
        ensure(
            sock.local_addr().unwrap().port() == 0,
            "requested addr before bound",
        )?;
        sock.send_to(b"early", echo)
            .map_err(|err| fail("early send", err))?;
        sock.bound().await.map_err(|err| fail("bound", err))?;
        let local = sock.local_addr().unwrap();
        ensure(local.port() != 0, "port after bound")?;
        ensure(!sock.interface_addrs().is_empty(), "interface addrs")?;
        report.push(format!("bound {local}"));

        let (data, from) = recv(&sock).await?;
        ensure(data == b"early" && from == echo, "early echo")?;
        let mut buf = [0u8; 16];
        let err = sock.recv_from(&mut buf).expect_err("queue empty");
        ensure(
            err.kind() == io::ErrorKind::WouldBlock,
            "WouldBlock on empty",
        )?;

        sock.send_to_async(b"hello", echo)
            .await
            .map_err(|err| fail("send", err))?;
        let (data, _) = recv(&sock).await?;
        ensure(data == b"hello", "hello echo")?;

        // GSO send, GRO receive.
        ensure(sock.max_gso_segments() == 4, "max_gso_segments from host")?;
        ensure(sock.gro_segments() == 4, "gro_segments from host")?;
        let payload: Vec<u8> = (0..1050u32).map(|index| (index % 251) as u8).collect();
        sock.send_to_segmented(&payload, echo, 100)
            .map_err(|err| fail("gso send", err))?;
        let mut got = Vec::new();
        let mut max_stride_batch = 0;
        while got.len() < payload.len() {
            std::future::poll_fn(|cx| sock.poll_recv_ready(cx))
                .await
                .map_err(|err| fail("recv ready", err))?;
            let mut buf = vec![0u8; 4096];
            match sock.recv_from_segmented(&mut buf) {
                Ok((n, _, stride)) => {
                    ensure(stride == 100 || n == stride, "stride")?;
                    max_stride_batch = max_stride_batch.max(n.div_ceil(stride.max(1)));
                    got.extend_from_slice(&buf[..n]);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(err) => return Err(fail("gro recv", err)),
            }
        }
        ensure(got == payload, "gso/gro payload")?;
        ensure(max_stride_batch > 1, "received a GRO batch")?;
        report.push(format!("gro batch of {max_stride_batch}"));

        // A GRO batch read with recv_from yields single datagrams.
        sock.send_to_segmented(&payload[..400], echo, 100)
            .map_err(|err| fail("gso send 2", err))?;
        for i in 0..4 {
            let (data, _) = recv(&sock).await?;
            ensure(
                data == payload[i * 100..(i + 1) * 100],
                "split gro datagram",
            )?;
        }

        // Per-send host error ends up in take_error; the socket stays open.
        sock.send_to(b"x", "127.0.0.1:9")
            .map_err(|err| fail("send forbidden", err))?;
        sock.send_to(b"after", echo)
            .map_err(|err| fail("send after", err))?;
        let (data, _) = recv(&sock).await?;
        ensure(data == b"after", "socket open after send error")?;
        let err = sock.take_error().unwrap();
        ensure(
            err.as_ref().map(io::Error::kind) == Some(io::ErrorKind::InvalidInput),
            "take_error after forbidden send",
        )?;

        // Connected socket.
        let conn = UdpSocket::bind("127.0.0.1:0").map_err(|err| fail("bind 2", err))?;
        conn.connect(echo).unwrap();
        conn.bound().await.map_err(|err| fail("bound 2", err))?;
        conn.send(b"conn")
            .map_err(|err| fail("send connected", err))?;
        std::future::poll_fn(|cx| conn.poll_recv_ready(cx))
            .await
            .map_err(|err| fail("recv ready 2", err))?;
        let n = conn
            .recv(&mut buf)
            .map_err(|err| fail("recv connected", err))?;
        ensure(&buf[..n] == b"conn", "connected echo")?;

        // Backpressure: a synchronous burst must hit WouldBlock, then the
        // async send resumes once the queue drained.
        let bulk = UdpSocket::bind("127.0.0.1:0").map_err(|err| fail("bind 3", err))?;
        bulk.bound().await.map_err(|err| fail("bound 3", err))?;
        let sink: SocketAddr = "127.0.0.1:7".parse().unwrap();
        let dgram = [7u8; 1200];
        let mut sent = 0;
        loop {
            match bulk.send_to(&dgram, sink) {
                Ok(_) => sent += 1,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(fail("bulk send", err)),
            }
            ensure(sent < 100_000, "never blocked")?;
        }
        bulk.send_to_async(&dgram, sink)
            .await
            .map_err(|err| fail("send after backpressure", err))?;
        report.push(format!("WouldBlock after {sent} datagrams"));

        // Host revokes access (close code 4001): PermissionDenied.
        let err = close_with(&sock, echo, b"close-me").await;
        ensure(
            err.kind() == io::ErrorKind::PermissionDenied,
            "PermissionDenied after revoke",
        )?;
        let err = sock.send_to(b"x", echo).expect_err("closed");
        ensure(
            err.kind() == io::ErrorKind::PermissionDenied,
            "send after close",
        )?;
        report.push(format!("closed: {err}"));
        Ok(report.join("; "))
    }

    /// Sends `payload` to `echo` and returns the error once the socket fails.
    async fn close_with(sock: &UdpSocket, echo: SocketAddr, payload: &[u8]) -> io::Error {
        sock.send_to(payload, echo).unwrap();
        let mut buf = [0u8; 64];
        loop {
            if let Err(err) = sock.recv_from_async(&mut buf).await {
                return err;
            }
        }
    }

    /// Opens a socket connected to `host:port` and checks a round trip.
    ///
    /// Returns `"ok <peer>"` or the error kind of the open.
    #[wasm_bindgen]
    pub async fn connect_host(host: String, port: u16) -> Result<String, JsValue> {
        webview_sockets::init()
            .await
            .map_err(|err| fail("init", err))?;
        let sock = UdpSocket::connect_host(&host, port, Some(webview_sockets::DnsQueryType::Ipv4))
            .map_err(|err| fail("connect_host", err))?;
        // Before BOUND the peer is unknown and sends would block.
        ensure(
            sock.send(b"x").map_err(|err| err.kind()) == Err(io::ErrorKind::WouldBlock),
            "send before bound",
        )?;
        if let Err(err) = sock.bound().await {
            return Ok(format!("{:?}", err.kind()));
        }
        let peer = sock.peer_addr().map_err(|err| fail("peer_addr", err))?;
        ensure(peer.port() == port, "peer port")?;
        sock.send(b"named").map_err(|err| fail("send", err))?;
        let (data, from) = recv(&sock).await?;
        ensure(data == b"named" && from == peer, "connected echo")?;
        Ok(format!("ok {peer}"))
    }

    /// Binds a fresh socket, sends `payload` to `echo` so the mock host closes
    /// the connection, and returns the resulting error kind.
    ///
    /// Expects `init()` to succeed (the caller installs `webviewSockets`).
    #[wasm_bindgen]
    pub async fn close_kind(echo: String, payload: String) -> Result<String, JsValue> {
        let echo: SocketAddr = echo.parse().map_err(|err| fail("echo addr", err))?;
        webview_sockets::init()
            .await
            .map_err(|err| fail("init", err))?;
        let sock = UdpSocket::bind("127.0.0.1:0").map_err(|err| fail("bind", err))?;
        sock.bound().await.map_err(|err| fail("bound", err))?;
        let err = close_with(&sock, echo, payload.as_bytes()).await;
        Ok(format!("{:?}", err.kind()))
    }
}
