//! Host backend: UDP sockets owned by the host app, reached over one
//! WebSocket per socket.
//!
//! JavaScript objects live in a `thread_local!` registry keyed by socket id.
//! Rust handles only hold an `Arc<Shared>` with a mutex and wakers, which
//! makes them `Send + Sync` without unsafe code. wasm32-unknown-unknown
//! without atomics is single threaded, so the registry is always reachable.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_sys::{ArrayBuffer, Function, Promise, Reflect, Uint8Array};
use tracing::{debug, trace, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};
use webview_sockets_proto as proto;

/// Above this many queued bytes sends return `WouldBlock`.
const HIGH_WATER: usize = 256 * 1024;
/// Sends resume once the queue is below this many bytes.
const LOW_WATER: usize = 64 * 1024;
/// How often to re-check `bufferedAmount` while sends are paused.
const RECHECK_MS: i32 = 5;
/// Received bytes kept before new datagrams are dropped.
const RECV_QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// Largest single datagram payload.
const MAX_DATAGRAM: usize = u16::MAX as usize;
/// Largest DATA record header: kind, ecn, v6 addr, segment size, len.
const MAX_RECORD_HEADER: usize = 1 + 1 + 19 + 2 + 4;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = queueMicrotask)]
    fn queue_microtask(cb: &Function);

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(cb: &Function, ms: i32) -> JsValue;
}

/// Session from `webviewSockets.requestSession()`.
#[derive(Debug, Clone)]
struct Session {
    url: String,
    token: [u8; proto::TOKEN_LEN],
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static REGISTRY: RefCell<HashMap<u64, Js>> = RefCell::new(HashMap::new());
}

/// The JavaScript side of one socket.
struct Js {
    ws: WebSocket,
    _on_open: Closure<dyn FnMut(JsValue)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

fn ws_of(id: u64) -> Option<WebSocket> {
    REGISTRY
        .try_with(|r| r.borrow().get(&id).map(|js| js.ws.clone()))
        .ok()
        .flatten()
}

pub(crate) async fn init() -> io::Result<()> {
    let api = host_api().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "window.webviewSockets is not available",
        )
    })?;
    let request = Reflect::get(&api, &"requestSession".into())
        .ok()
        .and_then(|f| f.dyn_into::<Function>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "webviewSockets.requestSession is not a function",
            )
        })?;
    let promise = request
        .call0(&api)
        .map_err(js_error)?
        .dyn_into::<Promise>()
        .map_err(|_| io::Error::other("requestSession did not return a promise"))?;
    let session = JsFuture::from(promise).await.map_err(js_error)?;
    let url = get_string(&session, "url").ok_or_else(|| io::Error::other("session has no url"))?;
    let token =
        get_string(&session, "token").ok_or_else(|| io::Error::other("session has no token"))?;
    init_with_session(&url, &token)
}

pub(crate) fn init_with_session(url: &str, token: &str) -> io::Result<()> {
    let token = parse_token(token)?;
    let url = resolve_ws_url(url)?;
    debug!(%url, "udp session set");
    *SESSION.lock().expect("poisoned") = Some(Session { url, token });
    Ok(())
}

pub(crate) fn is_available() -> bool {
    SESSION.lock().expect("poisoned").is_some() || host_api().is_some()
}

/// Returns `window.webviewSockets` if the host app injected it.
fn host_api() -> Option<JsValue> {
    let api = Reflect::get(&js_sys::global(), &"webviewSockets".into()).ok()?;
    api.is_object().then_some(api)
}

fn get_string(obj: &JsValue, key: &str) -> Option<String> {
    Reflect::get(obj, &key.into()).ok()?.as_string()
}

/// Maps a rejected promise or thrown JS value to an io error.
fn js_error(err: JsValue) -> io::Error {
    let name = get_string(&err, "name").unwrap_or_default();
    let msg = get_string(&err, "message")
        .or_else(|| err.as_string())
        .unwrap_or_else(|| format!("{err:?}"));
    let kind = match name.as_str() {
        "NotAllowedError" => io::ErrorKind::PermissionDenied,
        "SyntaxError" | "TypeError" => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{name}: {msg}"))
}

fn parse_token(token: &str) -> io::Result<[u8; proto::TOKEN_LEN]> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "token must be 64 hex chars");
    let bytes = token.as_bytes();
    if bytes.len() != proto::TOKEN_LEN * 2 {
        return Err(bad());
    }
    let mut out = [0u8; proto::TOKEN_LEN];
    for (o, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let hi = (pair[0] as char).to_digit(16).ok_or_else(bad)?;
        let lo = (pair[1] as char).to_digit(16).ok_or_else(bad)?;
        *o = (hi * 16 + lo) as u8;
    }
    Ok(out)
}

/// Resolves `url` against the current location and switches http(s) to
/// ws(s).
fn resolve_ws_url(url: &str) -> io::Result<String> {
    let invalid = |e: JsValue| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad session url {url:?}: {e:?}"),
        )
    };
    let base = Reflect::get(&js_sys::global(), &"location".into())
        .ok()
        .and_then(|loc| get_string(&loc, "href"));
    let parsed = match base {
        Some(base) => web_sys::Url::new_with_base(url, &base),
        None => web_sys::Url::new(url),
    }
    .map_err(invalid)?;
    match parsed.protocol().as_str() {
        "http:" => parsed.set_protocol("ws:"),
        "https:" => parsed.set_protocol("wss:"),
        "ws:" | "wss:" => {}
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("session url has unsupported scheme {other}"),
            ));
        }
    }
    Ok(parsed.href())
}

fn proto_error(code: proto::ErrorCode, msg: &str) -> (io::ErrorKind, String) {
    let kind = match code {
        proto::ErrorCode::NotAllowed => io::ErrorKind::PermissionDenied,
        proto::ErrorCode::InvalidInput => io::ErrorKind::InvalidInput,
        proto::ErrorCode::AddrInUse => io::ErrorKind::AddrInUse,
        proto::ErrorCode::NameNotResolved => io::ErrorKind::NotFound,
        proto::ErrorCode::Limit | proto::ErrorCode::Io => io::ErrorKind::Other,
    };
    (kind, format!("host error {}: {msg}", code.name()))
}

/// Maps a WebSocket close code to the error kind reported to the caller.
///
/// The host closes with [`proto::close_code::REVOKED`] or
/// [`proto::close_code::SETTING_OFF`] when the user withdrew access; those
/// map to [`io::ErrorKind::PermissionDenied`] so callers stop retrying. Every
/// other close is [`io::ErrorKind::ConnectionAborted`].
fn close_kind(code: u16) -> io::ErrorKind {
    match code {
        proto::close_code::REVOKED | proto::close_code::SETTING_OFF => {
            io::ErrorKind::PermissionDenied
        }
        _ => io::ErrorKind::ConnectionAborted,
    }
}

fn close_reason(code: u16) -> &'static str {
    match code {
        proto::close_code::NORMAL => "closed",
        proto::close_code::REVOKED => "access revoked",
        proto::close_code::SETTING_OFF => "UDP setting switched off",
        proto::close_code::NAVIGATED => "tab navigated or closed",
        proto::close_code::LIMIT => "limit exceeded",
        proto::close_code::IO_ERROR => "host io error",
        _ => "connection lost",
    }
}

/// Handle to a socket; clones share the socket.
#[derive(Debug, Clone)]
pub(crate) struct Socket {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    id: u64,
    state: Mutex<State>,
}

#[derive(Debug)]
enum Phase {
    /// Waiting for the WebSocket to open and BOUND to arrive.
    Connecting,
    /// BOUND received.
    Bound,
    /// Bind refused or connection closed.
    Failed(io::ErrorKind, String),
}

/// A received DATA record, consumed segment by segment.
#[derive(Debug)]
struct Datagram {
    from: SocketAddr,
    data: Vec<u8>,
    /// 0 for one datagram, else the GRO segment size.
    stride: usize,
    /// Bytes of `data` already consumed.
    offset: usize,
}

impl Datagram {
    fn remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Length of the next segment.
    fn next_len(&self) -> usize {
        match self.stride {
            0 => self.remaining(),
            s => s.min(self.remaining()),
        }
    }
}

#[derive(Debug)]
struct State {
    phase: Phase,
    requested: SocketAddr,
    local: Option<SocketAddr>,
    interface_addrs: Vec<SocketAddr>,
    max_gso_segments: usize,
    gro_segments: usize,
    peer: Option<SocketAddr>,
    /// Opened in connected mode: sends wait for BOUND, which sets `peer`.
    connected_mode: bool,
    hello: Vec<u8>,
    recv: VecDeque<Datagram>,
    recv_bytes: usize,
    /// Encoded messages not yet handed to the WebSocket.
    out: Vec<Vec<u8>>,
    out_bytes: usize,
    /// Last seen `WebSocket.bufferedAmount`.
    buffered: usize,
    paused: bool,
    flush_scheduled: bool,
    recheck_scheduled: bool,
    pending_error: Option<(io::ErrorKind, String)>,
    recv_wakers: Vec<Waker>,
    send_wakers: Vec<Waker>,
    bound_wakers: Vec<Waker>,
}

impl State {
    fn failed(&self) -> Option<io::Error> {
        match &self.phase {
            Phase::Failed(kind, msg) => Some(io::Error::new(*kind, msg.clone())),
            _ => None,
        }
    }

    /// Updates the pause flag from the current backlog.
    fn update_pressure(&mut self) {
        let backlog = self.out_bytes + self.buffered;
        if self.paused {
            self.paused = backlog >= LOW_WATER;
        } else {
            self.paused = backlog > HIGH_WATER;
        }
    }

    fn enqueue(&mut self, data: proto::Data<'_>) {
        let len = data.encoded_len();
        match self.out.last_mut() {
            Some(msg) if msg.len() + len <= proto::MAX_MESSAGE_LEN => {
                proto::encode_data(&data, msg)
            }
            _ => {
                let mut msg = Vec::with_capacity(len);
                proto::encode_data(&data, &mut msg);
                self.out.push(msg);
            }
        }
        self.out_bytes += len;
    }

    /// Drops queued datagrams not from the connected peer.
    fn skip_foreign(&mut self) {
        if let Some(peer) = self.peer {
            while self.recv.front().is_some_and(|d| d.from != peer) {
                let d = self.recv.pop_front().expect("checked");
                self.recv_bytes -= d.remaining();
            }
        }
    }

    /// Consumes `len` bytes of the front datagram.
    fn consume(&mut self, len: usize) {
        let front = self.recv.front_mut().expect("caller checked");
        front.offset += len;
        self.recv_bytes -= len;
        if front.remaining() == 0 {
            self.recv.pop_front();
        }
    }

    fn fail(&mut self, kind: io::ErrorKind, msg: String) -> Vec<Waker> {
        if matches!(self.phase, Phase::Failed(..)) {
            return Vec::new();
        }
        self.phase = Phase::Failed(kind, msg);
        self.out.clear();
        self.out_bytes = 0;
        self.take_all_wakers()
    }

    fn take_all_wakers(&mut self) -> Vec<Waker> {
        let mut w = std::mem::take(&mut self.recv_wakers);
        w.append(&mut self.send_wakers);
        w.append(&mut self.bound_wakers);
        w
    }
}

fn add_waker(wakers: &mut Vec<Waker>, cx: &Context<'_>) {
    if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
        wakers.push(cx.waker().clone());
    }
}

fn wake(wakers: Vec<Waker>) {
    for w in wakers {
        w.wake();
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let js = REGISTRY
            .try_with(|r| r.borrow_mut().remove(&self.id))
            .ok()
            .flatten();
        let Some(js) = js else { return };
        js.ws.set_onopen(None);
        js.ws.set_onmessage(None);
        js.ws.set_onclose(None);
        // Like std, datagrams passed to send are not lost on drop.
        let state = self.state.get_mut().expect("poisoned");
        if matches!(state.phase, Phase::Bound) {
            for msg in state.out.drain(..) {
                js.ws.send_with_u8_array(&msg).ok();
            }
        }
        js.ws.close_with_code(proto::close_code::NORMAL).ok();
        trace!(id = self.id, "udp socket dropped");
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("poisoned")
    }

    fn on_open(&self) {
        let hello = std::mem::take(&mut self.lock().hello);
        if let Some(ws) = ws_of(self.id)
            && let Err(err) = ws.send_with_u8_array(&hello)
        {
            let err = js_error(err);
            let wakers = self.lock().fail(err.kind(), err.to_string());
            wake(wakers);
        }
    }

    fn on_message(self: &Arc<Self>, ev: MessageEvent) {
        let Ok(buf) = ev.data().dyn_into::<ArrayBuffer>() else {
            warn!(id = self.id, "ignoring non-binary message");
            return;
        };
        let msg = Uint8Array::new(&buf).to_vec();
        let mut state = self.lock();
        let wakers = match state.phase {
            Phase::Connecting => self.on_first(&mut state, &msg),
            Phase::Bound => on_data(&mut state, &msg),
            Phase::Failed(..) => Vec::new(),
        };
        let close = matches!(state.phase, Phase::Failed(..));
        drop(state);
        if close && let Some(ws) = ws_of(self.id) {
            ws.close().ok();
        }
        wake(wakers);
    }

    fn on_first(self: &Arc<Self>, state: &mut State, msg: &[u8]) -> Vec<Waker> {
        match proto::decode_first(msg) {
            Ok(proto::Frame::Bound(bound)) => {
                debug!(id = self.id, local = %bound.local, "udp socket bound");
                state.phase = Phase::Bound;
                state.local = Some(bound.local);
                if let Some(remote) = bound.remote {
                    state.peer = Some(remote);
                }
                state.interface_addrs = bound.interface_addrs;
                state.max_gso_segments = bound.max_gso_segments.max(1) as usize;
                state.gro_segments = bound.gro_segments.max(1) as usize;
                if !state.out.is_empty() {
                    self.schedule_flush(state);
                }
                state.take_all_wakers()
            }
            Ok(proto::Frame::Error(err)) => {
                let (kind, msg) = proto_error(err.code, err.message);
                debug!(id = self.id, %msg, "udp bind refused");
                state.fail(kind, msg)
            }
            Ok(proto::Frame::Hello(_)) => {
                state.fail(io::ErrorKind::InvalidData, "host sent HELLO".to_string())
            }
            Err(err) => state.fail(
                io::ErrorKind::InvalidData,
                format!("bad first frame: {err}"),
            ),
        }
    }

    fn on_close(&self, ev: CloseEvent) {
        let code = ev.code();
        let reason = ev.reason();
        debug!(id = self.id, code, %reason, "udp websocket closed");
        let mut state = self.lock();
        let when = match state.phase {
            Phase::Connecting => " before bind",
            _ => "",
        };
        let mut msg = format!(
            "connection to host closed{when} ({code} {})",
            close_reason(code)
        );
        if !reason.is_empty() {
            msg.push_str(": ");
            msg.push_str(&reason);
        }
        let wakers = state.fail(close_kind(code), msg);
        drop(state);
        wake(wakers);
    }

    /// Queues a microtask that hands queued messages to the WebSocket.
    fn schedule_flush(self: &Arc<Self>, state: &mut State) {
        if state.flush_scheduled {
            return;
        }
        state.flush_scheduled = true;
        let weak = Arc::downgrade(self);
        let cb = Closure::once_into_js(move || {
            if let Some(shared) = weak.upgrade() {
                shared.flush();
            }
        });
        queue_microtask(cb.unchecked_ref());
    }

    fn flush(self: &Arc<Self>) {
        let mut state = self.lock();
        state.flush_scheduled = false;
        if !matches!(state.phase, Phase::Bound) {
            return;
        }
        let out = std::mem::take(&mut state.out);
        state.out_bytes = 0;
        drop(state);
        let Some(ws) = ws_of(self.id) else { return };
        for msg in &out {
            if let Err(err) = ws.send_with_u8_array(msg) {
                let err = js_error(err);
                let wakers = self
                    .lock()
                    .fail(io::ErrorKind::ConnectionAborted, err.to_string());
                wake(wakers);
                return;
            }
        }
        let mut state = self.lock();
        state.buffered = ws.buffered_amount() as usize;
        state.update_pressure();
        if state.paused {
            self.schedule_recheck(&mut state);
        }
    }

    /// Re-checks `bufferedAmount` soon while sends are paused.
    fn schedule_recheck(self: &Arc<Self>, state: &mut State) {
        if state.recheck_scheduled {
            return;
        }
        state.recheck_scheduled = true;
        let weak = Arc::downgrade(self);
        let cb = Closure::once_into_js(move || {
            if let Some(shared) = weak.upgrade() {
                shared.recheck();
            }
        });
        set_timeout(cb.unchecked_ref(), RECHECK_MS);
    }

    fn recheck(self: &Arc<Self>) {
        let ws = ws_of(self.id);
        let mut state = self.lock();
        state.recheck_scheduled = false;
        if let Some(ws) = ws {
            state.buffered = ws.buffered_amount() as usize;
        }
        state.update_pressure();
        if state.paused && state.failed().is_none() {
            self.schedule_recheck(&mut state);
            return;
        }
        let wakers = std::mem::take(&mut state.send_wakers);
        drop(state);
        wake(wakers);
    }

    /// Refreshes the pressure state from the WebSocket.
    fn refresh_pressure(&self, state: &mut State) {
        if state.paused
            && let Some(ws) = ws_of(self.id)
        {
            state.buffered = ws.buffered_amount() as usize;
            state.update_pressure();
        }
    }
}

fn on_data(state: &mut State, msg: &[u8]) -> Vec<Waker> {
    if proto::message_kind(msg) == Some(proto::KIND_ERROR) {
        match proto::decode_first(msg) {
            Ok(proto::Frame::Error(err)) => {
                let (kind, msg) = proto_error(err.code, err.message);
                debug!(%msg, "udp send failed on host");
                state.pending_error = Some((kind, msg));
                return Vec::new();
            }
            Ok(_) => unreachable!("kind checked"),
            Err(err) => {
                return state.fail(
                    io::ErrorKind::InvalidData,
                    format!("bad error frame: {err}"),
                );
            }
        }
    }
    let records = match proto::decode_data(msg) {
        Ok(records) => records,
        Err(err) => return state.fail(io::ErrorKind::InvalidData, format!("bad message: {err}")),
    };
    let mut got = false;
    for record in records {
        let record = match record {
            Ok(record) => record,
            Err(err) => {
                return state.fail(io::ErrorKind::InvalidData, format!("bad record: {err}"));
            }
        };
        if state.peer.is_some_and(|p| p != record.peer) {
            continue;
        }
        if state.recv_bytes + record.payload.len() > RECV_QUEUE_BYTES {
            trace!(
                len = record.payload.len(),
                "udp receive queue full, dropping"
            );
            continue;
        }
        state.recv_bytes += record.payload.len();
        state.recv.push_back(Datagram {
            from: record.peer,
            data: record.payload.to_vec(),
            stride: record.segment_size as usize,
            offset: 0,
        });
        got = true;
    }
    if got {
        std::mem::take(&mut state.recv_wakers)
    } else {
        Vec::new()
    }
}

fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "socket is not connected")
}

fn would_block() -> io::Error {
    io::Error::from(io::ErrorKind::WouldBlock)
}

impl Socket {
    pub(crate) fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::open(addr, |token| proto::Hello::bind(token, addr))
    }

    pub(crate) fn connect_host(
        host: &str,
        port: u16,
        dns: Option<crate::DnsQueryType>,
    ) -> io::Result<Self> {
        let dns_query_type = dns.map(|d| match d {
            crate::DnsQueryType::Ipv4 => proto::DnsQueryType::Ipv4,
            crate::DnsQueryType::Ipv6 => proto::DnsQueryType::Ipv6,
        });
        let any = match dns {
            Some(crate::DnsQueryType::Ipv6) => SocketAddr::from(([0u16; 8], 0)),
            _ => SocketAddr::from(([0, 0, 0, 0], 0)),
        };
        let sock = Self::open(any, |token| proto::Hello {
            token,
            mode: proto::Mode::Connected {
                remote_host: host,
                remote_port: port,
                dns_query_type,
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        })?;
        sock.lock().connected_mode = true;
        Ok(sock)
    }

    /// Opens the WebSocket and queues the HELLO built by `hello`.
    ///
    /// `requested` is the local address reported until BOUND arrives.
    fn open<'a>(
        requested: SocketAddr,
        hello: impl FnOnce([u8; proto::TOKEN_LEN]) -> proto::Hello<'a>,
    ) -> io::Result<Self> {
        let addr = requested;
        let session = SESSION.lock().expect("poisoned").clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no UDP session, call webview_sockets::init() first",
            )
        })?;
        let ws = WebSocket::new(&session.url).map_err(js_error)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let mut hello_msg = Vec::new();
        proto::encode_hello(&hello(session.token), &mut hello_msg);
        let hello = hello_msg;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            id,
            state: Mutex::new(State {
                phase: Phase::Connecting,
                requested: addr,
                local: None,
                interface_addrs: Vec::new(),
                max_gso_segments: 1,
                gro_segments: 1,
                peer: None,
                connected_mode: false,
                hello,
                recv: VecDeque::new(),
                recv_bytes: 0,
                out: Vec::new(),
                out_bytes: 0,
                buffered: 0,
                paused: false,
                flush_scheduled: false,
                recheck_scheduled: false,
                pending_error: None,
                recv_wakers: Vec::new(),
                send_wakers: Vec::new(),
                bound_wakers: Vec::new(),
            }),
        });

        let weak = Arc::downgrade(&shared);
        let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            if let Some(s) = Weak::upgrade(&weak) {
                s.on_open();
            }
        });
        let weak = Arc::downgrade(&shared);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev| {
            if let Some(s) = Weak::upgrade(&weak) {
                s.on_message(ev);
            }
        });
        let weak = Arc::downgrade(&shared);
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |ev| {
            if let Some(s) = Weak::upgrade(&weak) {
                s.on_close(ev);
            }
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        REGISTRY.with(|r| {
            r.borrow_mut().insert(
                id,
                Js {
                    ws,
                    _on_open: on_open,
                    _on_message: on_message,
                    _on_close: on_close,
                },
            )
        });
        debug!(id, %addr, "udp socket opening");
        Ok(Self { shared })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.lock()
    }

    pub(crate) async fn bound(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| {
            let mut state = self.lock();
            match &state.phase {
                Phase::Bound => Poll::Ready(Ok(())),
                Phase::Failed(kind, msg) => Poll::Ready(Err(io::Error::new(*kind, msg.clone()))),
                Phase::Connecting => {
                    add_waker(&mut state.bound_wakers, cx);
                    Poll::Pending
                }
            }
        })
        .await
    }

    pub(crate) fn local_addr(&self) -> io::Result<SocketAddr> {
        let state = self.lock();
        Ok(state.local.unwrap_or(state.requested))
    }

    pub(crate) fn interface_addrs(&self) -> Vec<SocketAddr> {
        self.lock().interface_addrs.clone()
    }

    pub(crate) fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.lock().peer.ok_or_else(not_connected)
    }

    pub(crate) fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        self.lock().peer = Some(addr);
        Ok(())
    }

    pub(crate) fn send_to(&self, buf: &[u8], addr: SocketAddr) -> io::Result<usize> {
        self.send_to_segmented(buf, addr, 0)
    }

    pub(crate) fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut state = self.lock();
        state.skip_foreign();
        let Some(front) = state.recv.front() else {
            return Err(state.failed().unwrap_or_else(would_block));
        };
        let len = front.next_len();
        let from = front.from;
        let n = len.min(buf.len());
        buf[..n].copy_from_slice(&front.data[front.offset..front.offset + n]);
        state.consume(len);
        Ok((n, from))
    }

    pub(crate) fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let peer = {
            let state = self.lock();
            match state.peer {
                Some(peer) => peer,
                None if state.connected_mode => {
                    return Err(state.failed().unwrap_or_else(would_block));
                }
                None => return Err(not_connected()),
            }
        };
        self.send_to(buf, peer)
    }

    pub(crate) fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        {
            let state = self.lock();
            if state.peer.is_none() {
                return Err(match state.connected_mode {
                    true => state.failed().unwrap_or_else(would_block),
                    false => not_connected(),
                });
            }
        }
        self.recv_from(buf).map(|(n, _)| n)
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        if nonblocking {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sockets cannot block on wasm",
            ))
        }
    }

    pub(crate) fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        no_timeout(dur)
    }

    pub(crate) fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        no_timeout(dur)
    }

    pub(crate) fn read_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(None)
    }

    pub(crate) fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(None)
    }

    pub(crate) fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(self
            .lock()
            .pending_error
            .take()
            .map(|(kind, msg)| io::Error::new(kind, msg)))
    }

    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        Ok(self.clone())
    }

    pub(crate) fn max_gso_segments(&self) -> usize {
        self.lock().max_gso_segments
    }

    pub(crate) fn gro_segments(&self) -> usize {
        self.lock().gro_segments
    }

    pub(crate) fn send_to_segmented(
        &self,
        buf: &[u8],
        addr: SocketAddr,
        segment_size: usize,
    ) -> io::Result<usize> {
        let segment_size = match segment_size {
            0 => buf.len(),
            n => n.min(buf.len()),
        };
        if segment_size > MAX_DATAGRAM {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "datagram larger than 65535 bytes",
            ));
        }
        let mut state = self.lock();
        if let Some(err) = state.failed() {
            return Err(err);
        }
        self.shared.refresh_pressure(&mut state);
        if state.paused {
            return Err(would_block());
        }
        let max_batch = (proto::MAX_MESSAGE_LEN - MAX_RECORD_HEADER) / segment_size.max(1);
        let per_record = state.max_gso_segments.min(max_batch).max(1) * segment_size.max(1);
        if buf.is_empty() {
            state.enqueue(proto::Data {
                ecn: 0,
                peer: addr,
                segment_size: 0,
                payload: buf,
            });
        }
        for chunk in buf.chunks(per_record.max(1)) {
            let segment_size = if chunk.len() > segment_size {
                segment_size as u16
            } else {
                0
            };
            state.enqueue(proto::Data {
                ecn: 0,
                peer: addr,
                segment_size,
                payload: chunk,
            });
        }
        state.update_pressure();
        if matches!(state.phase, Phase::Bound) {
            self.shared.schedule_flush(&mut state);
        }
        Ok(buf.len())
    }

    pub(crate) fn recv_from_segmented(
        &self,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, usize)> {
        let mut state = self.lock();
        state.skip_foreign();
        let Some(front) = state.recv.front() else {
            return Err(state.failed().unwrap_or_else(would_block));
        };
        let from = front.from;
        let seg = front.next_len();
        // Whole segments that fit, at least one (truncated if needed).
        let take = if seg == 0 || seg > buf.len() {
            seg
        } else {
            (buf.len() / seg * seg).min(front.remaining())
        };
        let n = take.min(buf.len());
        buf[..n].copy_from_slice(&front.data[front.offset..front.offset + n]);
        state.consume(take);
        Ok((n, from, seg.min(n)))
    }

    pub(crate) fn poll_recv_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.lock();
        state.skip_foreign();
        if !state.recv.is_empty() {
            return Poll::Ready(Ok(()));
        }
        if let Some(err) = state.failed() {
            return Poll::Ready(Err(err));
        }
        add_waker(&mut state.recv_wakers, cx);
        Poll::Pending
    }

    pub(crate) fn poll_send_ready(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.lock();
        if let Some(err) = state.failed() {
            return Poll::Ready(Err(err));
        }
        self.shared.refresh_pressure(&mut state);
        let waiting_for_peer = state.connected_mode && matches!(state.phase, Phase::Connecting);
        if !state.paused && !waiting_for_peer {
            return Poll::Ready(Ok(()));
        }
        add_waker(&mut state.send_wakers, cx);
        if matches!(state.phase, Phase::Bound) {
            self.shared.schedule_recheck(&mut state);
        }
        Poll::Pending
    }
}

fn no_timeout(dur: Option<Duration>) -> io::Result<()> {
    match dur {
        None => Ok(()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "timeouts are not supported on wasm, use the async helpers",
        )),
    }
}
