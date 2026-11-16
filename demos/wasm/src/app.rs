//! The wasm exports of the demo.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::future::poll_fn;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use js_sys::{Function, Promise};
use n0_future::time::{Duration, Instant, sleep, timeout};
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, spawn_local};
use webview_sockets::UdpSocket;

/// Payload size of the bench datagrams.
const BENCH_SIZE: usize = 1200;
/// Receive buffer; GRO records larger than this are returned in parts.
const RECV_BUF: usize = 64 * 1024;
/// Queue between the receive loop and a running automation.
const TAP_CAPACITY: usize = 4096;
/// How often automation runs check for per-send errors.
const ERROR_POLL: Duration = Duration::from_millis(100);

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);
    #[wasm_bindgen(js_namespace = console, js_name = log)]
    fn console_log(msg: &str);
}

/// A received datagram handed to an automation run.
struct Packet {
    data: Vec<u8>,
    from: SocketAddr,
}

/// Diverts received datagrams to an automation run.
struct Tap {
    tx: mpsc::Sender<Packet>,
    /// If false, the datagram is also logged (and echoed).
    consume: bool,
}

struct Inner {
    sock: UdpSocket,
    echo: Cell<bool>,
    on_log: RefCell<Option<Function>>,
    on_close: RefCell<Option<Function>>,
    tap: RefCell<Option<Tap>>,
}

/// A bound UDP socket plus the demo state.
#[wasm_bindgen]
pub struct Demo {
    inner: Rc<Inner>,
}

/// Asks the host for a UDP session, binds a socket and starts receiving.
///
/// Rejects with the reason if native UDP is not available.
#[wasm_bindgen]
pub async fn start() -> Result<Demo, JsError> {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
    webview_sockets::init().await.map_err(|e| match e.kind() {
        io::ErrorKind::Unsupported => {
            js_err(format!("not running in a webview-sockets host app ({e})"))
        }
        _ => js_err(e),
    })?;
    let mut notes = Vec::new();
    let sock = match bind("[::]:0").await {
        Ok(sock) => sock,
        Err(e) if is_fatal(&e) => return Err(js_err(e)),
        Err(e) => {
            notes.push(format!("bind [::]:0 failed ({e}), trying 0.0.0.0:0"));
            bind("0.0.0.0:0").await.map_err(js_err)?
        }
    };
    let inner = Rc::new(Inner {
        sock,
        echo: Cell::new(false),
        on_log: RefCell::new(None),
        on_close: RefCell::new(None),
        tap: RefCell::new(None),
    });
    for note in notes {
        console_log(&note);
    }
    spawn_local(recv_loop(inner.clone()));
    Ok(Demo { inner })
}

async fn bind(addr: &str) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind(addr)?;
    sock.bound().await?;
    Ok(sock)
}

/// Errors after which another bind address will not help.
fn is_fatal(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
    )
}

fn js_err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

#[wasm_bindgen]
impl Demo {
    /// The bound address, e.g. `[::]:50123`.
    #[wasm_bindgen(getter)]
    pub fn local_addr(&self) -> String {
        self.inner
            .sock
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }

    /// The host's interface addresses with the bound port.
    #[wasm_bindgen(getter)]
    pub fn interface_addrs(&self) -> Vec<String> {
        self.inner
            .sock
            .interface_addrs()
            .iter()
            .map(|a| a.to_string())
            .collect()
    }

    /// Segments the host sends in one GSO call (1: no GSO).
    #[wasm_bindgen(getter)]
    pub fn max_gso_segments(&self) -> usize {
        self.inner.sock.max_gso_segments()
    }

    /// Datagrams the host may receive in one GRO call (1: no GRO).
    #[wasm_bindgen(getter)]
    pub fn gro_segments(&self) -> usize {
        self.inner.sock.gro_segments()
    }

    /// Sets the callback for log lines, called with one string.
    pub fn set_on_log(&self, cb: Option<Function>) {
        *self.inner.on_log.borrow_mut() = cb;
    }

    /// Sets the callback for a closed socket, called with the reason.
    pub fn set_on_close(&self, cb: Option<Function>) {
        *self.inner.on_close.borrow_mut() = cb;
    }

    /// Turns echo mode on or off: reply to every datagram with its bytes.
    pub fn set_echo(&self, on: bool) {
        self.inner.echo.set(on);
    }

    /// Sends `text` as one datagram to `peer` (`ip:port` or `[v6]:port`).
    pub fn send_text(&self, peer: &str, text: &str) -> Result<(), JsError> {
        let peer = parse_peer(peer).map_err(js_err)?;
        self.inner.send_text(peer, text).map_err(js_err)?;
        self.inner.check_error_soon();
        Ok(())
    }

    /// Sends `segments` datagrams to `peer` with one segmented (GSO) send.
    ///
    /// Each datagram is `"<text> <i>/<n>"`, padded with spaces to equal
    /// length. Returns the segment size.
    pub fn send_gso(&self, peer: &str, text: &str, segments: usize) -> Result<usize, JsError> {
        let peer = parse_peer(peer).map_err(js_err)?;
        let seg = self.inner.send_gso(peer, text, segments).map_err(js_err)?;
        self.inner.check_error_soon();
        Ok(seg)
    }

    /// Sends `text` to `peer` until a reply arrives (5 tries, 6 s).
    ///
    /// Resolves to `"PASS ..."` or `"FAIL ..."`. Without `expect`, any reply
    /// from the peer's port passes.
    pub fn send_expect(&self, peer: String, text: String, expect: Option<String>) -> Promise {
        let inner = self.inner.clone();
        report(async move { inner.send_expect(&peer, &text, expect.as_deref()).await })
    }

    /// Sends `segments` datagrams with one GSO send and waits for all echoes.
    ///
    /// Resolves to `"PASS ..."` or `"FAIL ..."`.
    pub fn gso_expect(&self, peer: String, text: String, segments: usize) -> Promise {
        let inner = self.inner.clone();
        report(async move { inner.gso_expect(&peer, &text, segments).await })
    }

    /// Runs a 1200-byte ping-pong and a windowed throughput test against an
    /// echo server.
    ///
    /// With `gso`, each window refill is one segmented send. Resolves to
    /// `"PASS ..."` or `"FAIL ..."`.
    pub fn bench(&self, peer: String, n: usize, count: usize, window: usize, gso: bool) -> Promise {
        let inner = self.inner.clone();
        report(async move { inner.bench(&peer, n, count, window, gso).await })
    }
}

/// Turns an automation result into a promise of `"PASS ..."`/`"FAIL ..."`.
fn report(fut: impl Future<Output = Result<String, String>> + 'static) -> Promise {
    future_to_promise(async move {
        let line = match fut.await {
            Ok(detail) => format!("PASS {detail}"),
            Err(detail) => format!("FAIL {detail}"),
        };
        Ok(JsValue::from_str(&line))
    })
}

fn parse_peer(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text
        .trim()
        .parse()
        .map_err(|_| format!("peer must be ip:port or [ipv6]:port, got {text:?}"))?;
    if addr.port() == 0 {
        return Err(format!("bad port 0 in {text:?}"));
    }
    Ok(addr)
}

/// Quoted text if `data` is printable UTF-8, else a hex prefix.
fn printable(data: &[u8]) -> String {
    match std::str::from_utf8(data) {
        Ok(text)
            if !text
                .chars()
                .any(|c| matches!(c, '\0'..='\x08' | '\x0e'..='\x1f')) =>
        {
            let short: String = text.chars().take(200).collect();
            let more = if short.len() < text.len() { "..." } else { "" };
            format!("{short:?}{more}")
        }
        _ => {
            let mut out = String::from("hex ");
            for b in data.iter().take(32) {
                write!(out, "{b:02x}").expect("string write");
            }
            if data.len() > 32 {
                out.push_str("...");
            }
            out
        }
    }
}

async fn recv_loop(inner: Rc<Inner>) {
    let mut buf = vec![0u8; RECV_BUF];
    let err = loop {
        if let Err(e) = poll_fn(|cx| inner.sock.poll_recv_ready(cx)).await {
            break e;
        }
        // Drain everything that is queued; each call may return a GRO batch.
        let err = loop {
            match inner.sock.recv_from_segmented(&mut buf) {
                Ok((0, from, _)) => inner.dispatch(&[], from),
                Ok((len, from, stride)) => {
                    for seg in buf[..len].chunks(stride.max(1)) {
                        inner.dispatch(seg, from);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break None,
                Err(e) => break Some(e),
            }
        };
        if let Some(e) = err {
            break e;
        }
        inner.check_error();
    };
    inner.log(format!("socket closed: {err}"));
    // Wake a waiting automation run.
    inner.tap.borrow_mut().take();
    let cb = inner.on_close.borrow().clone();
    if let Some(cb) = cb {
        cb.call1(&JsValue::NULL, &JsValue::from_str(&err.to_string()))
            .ok();
    }
}

/// Waits until the socket accepts more data to send.
async fn send_ready(sock: &UdpSocket) -> io::Result<()> {
    poll_fn(|cx| sock.poll_send_ready(cx)).await
}

/// Sends a segmented batch, waiting while the send queue is full.
async fn send_segmented(
    sock: &UdpSocket,
    buf: &[u8],
    to: SocketAddr,
    seg: usize,
) -> io::Result<()> {
    loop {
        send_ready(sock).await?;
        match sock.send_to_segmented(buf, to, seg) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            res => return res.map(|_| ()),
        }
    }
}

/// Builds `"<text> <i>/<n>"` segments padded to equal length.
fn gso_batch(text: &str, segments: usize) -> (Vec<u8>, usize) {
    let parts: Vec<String> = (1..=segments)
        .map(|i| format!("{text} {i}/{segments}"))
        .collect();
    let seg = parts.iter().map(|p| p.len()).max().unwrap_or(0).max(1);
    let mut buf = Vec::with_capacity(seg * segments);
    for p in &parts {
        buf.extend_from_slice(p.as_bytes());
        buf.resize(buf.len() + seg - p.len(), b' ');
    }
    (buf, seg)
}

fn percentile(sorted: &[f64], p: usize) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[(sorted.len() * p / 100).min(sorted.len() - 1)]
}

fn seq_of(data: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(..4)?.try_into().ok()?))
}

impl Inner {
    fn log(&self, line: String) {
        let cb = self.on_log.borrow().clone();
        match cb {
            Some(cb) => {
                cb.call1(&JsValue::NULL, &JsValue::from_str(&line)).ok();
            }
            None => console_log(&line),
        }
    }

    /// Logs a per-send error the host reported, if any.
    fn check_error(&self) {
        if let Ok(Some(e)) = self.sock.take_error() {
            self.log(format!("send error: {e}"));
        }
    }

    /// Reports per-send errors that arrive shortly after a manual send.
    fn check_error_soon(self: &Rc<Self>) {
        let this = self.clone();
        spawn_local(async move {
            sleep(Duration::from_millis(300)).await;
            this.check_error();
        });
    }

    fn dispatch(&self, data: &[u8], from: SocketAddr) {
        if let Some(tap) = self.tap.borrow().as_ref() {
            // Drop on overflow, like UDP.
            tap.tx
                .try_send(Packet {
                    data: data.to_vec(),
                    from,
                })
                .ok();
            if tap.consume {
                return;
            }
        }
        self.log(format!(
            "<- {from} {} bytes: {}",
            data.len(),
            printable(data)
        ));
        if self.echo.get() {
            match self.sock.send_to(data, from) {
                Ok(_) => self.log(format!("-> {from} echo {} bytes", data.len())),
                Err(e) => self.log(format!("echo to {from} failed: {e}")),
            }
        }
    }

    /// Installs a tap; dropping the receiver does not remove it.
    fn tap(&self, consume: bool) -> mpsc::Receiver<Packet> {
        let (tx, rx) = mpsc::channel(TAP_CAPACITY);
        *self.tap.borrow_mut() = Some(Tap { tx, consume });
        rx
    }

    fn untap(&self) {
        self.tap.borrow_mut().take();
    }

    fn send_text(self: &Rc<Self>, peer: SocketAddr, text: &str) -> io::Result<()> {
        self.sock.send_to(text.as_bytes(), peer)?;
        self.log(format!("-> {peer} {text:?}"));
        Ok(())
    }

    fn send_gso(
        self: &Rc<Self>,
        peer: SocketAddr,
        text: &str,
        segments: usize,
    ) -> io::Result<usize> {
        let segments = segments.clamp(1, 64);
        let (buf, seg) = gso_batch(text, segments);
        self.sock.send_to_segmented(&buf, peer, seg)?;
        self.log(format!(
            "-> {peer} GSO batch of {segments} x {seg} bytes (host max_gso_segments={})",
            self.sock.max_gso_segments()
        ));
        Ok(seg)
    }

    async fn send_expect(
        self: &Rc<Self>,
        peer: &str,
        text: &str,
        expect: Option<&str>,
    ) -> Result<String, String> {
        let peer = parse_peer(peer)?;
        let mut rx = self.tap(false);
        let res = self.send_expect_inner(&mut rx, peer, text, expect).await;
        self.untap();
        res
    }

    async fn send_expect_inner(
        self: &Rc<Self>,
        rx: &mut mpsc::Receiver<Packet>,
        peer: SocketAddr,
        text: &str,
        expect: Option<&str>,
    ) -> Result<String, String> {
        let start = Instant::now();
        let deadline = start + Duration::from_secs(6);
        let mut next_send = start;
        let mut tries = 0;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("no reply from {peer} within 6 s"));
            }
            // UDP may drop: resend once per second, up to 5 times.
            if now >= next_send && tries < 5 {
                self.send_text(peer, text)
                    .map_err(|e| format!("send: {e}"))?;
                tries += 1;
                next_send = now + Duration::from_secs(1);
            }
            if let Ok(Some(e)) = self.sock.take_error() {
                return Err(format!("send error: {e}"));
            }
            let wake = if tries < 5 {
                next_send.min(deadline)
            } else {
                deadline
            };
            // Wake regularly to notice per-send errors from the host.
            let wake = wake.min(now + ERROR_POLL);
            let pkt = match timeout(wake.saturating_duration_since(now), rx.recv()).await {
                Ok(Some(pkt)) => pkt,
                Ok(None) => return Err("socket closed".into()),
                Err(_) => continue,
            };
            if pkt.from.port() != peer.port() {
                continue;
            }
            let got = String::from_utf8_lossy(&pkt.data);
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            return match expect {
                Some(want) if got != want => {
                    Err(format!("expected {want:?} from {peer}, got {got:?}"))
                }
                _ => Ok(format!("reply {got:?} from {} in {ms:.2} ms", pkt.from)),
            };
        }
    }

    async fn gso_expect(
        self: &Rc<Self>,
        peer: &str,
        text: &str,
        segments: usize,
    ) -> Result<String, String> {
        let peer = parse_peer(peer)?;
        let segments = segments.clamp(1, 64);
        let mut rx = self.tap(false);
        let res = self.gso_expect_inner(&mut rx, peer, text, segments).await;
        self.untap();
        res
    }

    async fn gso_expect_inner(
        self: &Rc<Self>,
        rx: &mut mpsc::Receiver<Packet>,
        peer: SocketAddr,
        text: &str,
        segments: usize,
    ) -> Result<String, String> {
        let (buf, seg) = gso_batch(text, segments);
        let want: BTreeSet<Vec<u8>> = buf.chunks(seg).map(|c| c.to_vec()).collect();
        let mut got = BTreeSet::new();
        let start = Instant::now();
        for _ in 0..5 {
            self.send_gso(peer, text, segments)
                .map_err(|e| format!("send: {e}"))?;
            let round = Instant::now() + Duration::from_secs(1);
            loop {
                if let Ok(Some(e)) = self.sock.take_error() {
                    return Err(format!("send error: {e}"));
                }
                let now = Instant::now();
                let left = round.saturating_duration_since(now).min(ERROR_POLL);
                match timeout(left, rx.recv()).await {
                    Ok(Some(pkt)) if pkt.from.port() == peer.port() => {
                        if want.contains(&pkt.data) {
                            got.insert(pkt.data);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => return Err("socket closed".into()),
                    Err(_) if Instant::now() >= round => break,
                    Err(_) => {}
                }
                if got.len() == want.len() {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    return Ok(format!(
                        "gso {segments} x {seg} B echoed by {peer} in {ms:.2} ms \
                         (max_gso_segments={} gro_segments={})",
                        self.sock.max_gso_segments(),
                        self.sock.gro_segments()
                    ));
                }
            }
        }
        Err(format!(
            "only {}/{segments} GSO segments echoed by {peer}",
            got.len()
        ))
    }

    async fn bench(
        self: &Rc<Self>,
        peer: &str,
        n: usize,
        count: usize,
        window: usize,
        gso: bool,
    ) -> Result<String, String> {
        let peer = parse_peer(peer).map_err(|e| format!("bench: {e}"))?;
        let mut rx = self.tap(true);
        let res = self
            .bench_inner(&mut rx, peer, n, count, window.max(1), gso)
            .await;
        self.untap();
        res.map_err(|e| format!("bench: {e}"))
    }

    async fn bench_inner(
        &self,
        rx: &mut mpsc::Receiver<Packet>,
        peer: SocketAddr,
        n: usize,
        count: usize,
        window: usize,
        gso: bool,
    ) -> Result<String, String> {
        self.log(format!("bench: {n} x {BENCH_SIZE} B ping-pong to {peer}"));
        let (rtts, lost) = self.bench_rtt(rx, peer, n, 50).await?;
        let mut sorted = rtts.clone();
        sorted.sort_by(f64::total_cmp);
        let mean = rtts.iter().sum::<f64>() / rtts.len().max(1) as f64;
        let rtt_line = format!(
            "rtt p50={:.3}ms p99={:.3}ms mean={mean:.3}ms min={:.3}ms lost={lost}/{n}",
            percentile(&sorted, 50),
            percentile(&sorted, 99),
            sorted.first().copied().unwrap_or(f64::NAN),
        );
        self.log(format!("bench: {rtt_line}"));

        self.log(format!(
            "bench: throughput {count} x {BENCH_SIZE} B, window {window}, gso {gso} (host max {})",
            self.sock.max_gso_segments()
        ));
        let (received, lost, secs) = self.bench_throughput(rx, peer, count, window, gso).await?;
        let pps = received as f64 / secs;
        let mbit = pps * (BENCH_SIZE * 8) as f64 / 1e6;
        let tput_line = format!(
            "tput {pps:.0} pkt/s each way {mbit:.1} Mbit/s each way lost={lost}/{count} \
             time={secs:.2}s window={window} gso={gso}"
        );
        self.log(format!("bench: {tput_line}"));
        if rtts.is_empty() || received == 0 {
            return Err(format!("no replies: {rtt_line} {tput_line}"));
        }
        Ok(format!("bench {rtt_line} {tput_line}"))
    }

    /// Sequential ping-pong; returns the RTTs in ms and the lost count.
    async fn bench_rtt(
        &self,
        rx: &mut mpsc::Receiver<Packet>,
        peer: SocketAddr,
        n: usize,
        warmup: usize,
    ) -> Result<(Vec<f64>, usize), String> {
        let mut payload = vec![0u8; BENCH_SIZE];
        let mut rtts = Vec::with_capacity(n);
        let mut lost = 0;
        for i in 0..(warmup + n) as u32 {
            payload[..4].copy_from_slice(&i.to_be_bytes());
            let start = Instant::now();
            send_segmented(&self.sock, &payload, peer, 0)
                .await
                .map_err(|e| format!("send: {e}"))?;
            let deadline = start + Duration::from_secs(1);
            let ok = loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match timeout(left, rx.recv()).await {
                    Ok(Some(pkt)) if seq_of(&pkt.data) == Some(i) => break true,
                    Ok(Some(_)) => continue,
                    Ok(None) => return Err("socket closed".into()),
                    Err(_) => break false,
                }
            };
            let rtt = start.elapsed().as_secs_f64() * 1000.0;
            if (i as usize) < warmup {
                continue;
            }
            if ok {
                rtts.push(rtt);
            } else {
                lost += 1;
            }
        }
        Ok((rtts, lost))
    }

    /// Keeps `window` datagrams in flight until `count` replies arrived or
    /// were declared lost (no reply for 300 ms).
    ///
    /// Returns (received, lost, seconds).
    async fn bench_throughput(
        &self,
        rx: &mut mpsc::Receiver<Packet>,
        peer: SocketAddr,
        count: usize,
        window: usize,
        gso: bool,
    ) -> Result<(usize, usize, f64), String> {
        let mut sent = 0usize;
        let mut received = 0usize;
        let mut lost = 0usize;
        // Replies with a lower seq were already counted as lost.
        let mut floor = 0usize;
        let mut batch = Vec::with_capacity(window * BENCH_SIZE);
        let start = Instant::now();
        while received + lost < count {
            // Refill the window, as one segmented send if gso.
            batch.clear();
            while sent < count && sent.saturating_sub(received + lost) < window {
                let mut payload = [0u8; BENCH_SIZE];
                payload[..4].copy_from_slice(&(sent as u32).to_be_bytes());
                if gso {
                    batch.extend_from_slice(&payload);
                } else {
                    send_segmented(&self.sock, &payload, peer, 0)
                        .await
                        .map_err(|e| format!("send: {e}"))?;
                }
                sent += 1;
            }
            if !batch.is_empty() {
                send_segmented(&self.sock, &batch, peer, BENCH_SIZE)
                    .await
                    .map_err(|e| format!("send: {e}"))?;
            }
            match timeout(Duration::from_millis(300), rx.recv()).await {
                Ok(Some(pkt)) => {
                    let mut pkt = Some(pkt);
                    // Count everything already queued before refilling.
                    while let Some(p) = pkt {
                        if seq_of(&p.data).is_some_and(|s| s as usize >= floor) {
                            received += 1;
                        }
                        pkt = rx.try_recv().ok();
                    }
                }
                Ok(None) => return Err("socket closed".into()),
                Err(_) => {
                    lost += sent.saturating_sub(received + lost);
                    floor = sent;
                }
            }
        }
        Ok((received, lost, start.elapsed().as_secs_f64()))
    }
}
