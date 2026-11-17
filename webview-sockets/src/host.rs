//! Host backend: UDP sockets owned by the host app.
//!
//! Each socket is reached over its own WebSocket to the host app.
//! JavaScript objects live in a `thread_local!` registry keyed by socket id.
//! Rust handles only hold an `Arc<Shared>` with a mutex and wakers, which
//! makes them `Send + Sync` without unsafe code. wasm32-unknown-unknown
//! without atomics is single threaded, so the registry is always reachable.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use js_sys::{ArrayBuffer, Function, Promise, Reflect, Uint8Array};
use tracing::{debug, trace, warn};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};
use webview_sockets_proto as proto;

use crate::DnsQueryType;

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
    fn queue_microtask(callback: &Function);

    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(callback: &Function, ms: i32) -> JsValue;
}

/// Session from `webviewSockets.requestSession()`.
#[derive(Debug, Clone)]
struct Session {
    url: String,
    token: [u8; proto::TOKEN_LEN],
}

/// The session set by [`init`] or [`init_with_session`].
static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// The id of the next socket, the key in [`REGISTRY`].
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The JavaScript side of each open socket, by socket id.
    static REGISTRY: RefCell<HashMap<u64, JsSocket>> = RefCell::new(HashMap::new());
}

/// The JavaScript side of one socket.
struct JsSocket {
    ws: WebSocket,
    _on_open: Closure<dyn FnMut(JsValue)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

/// Returns the WebSocket of socket `id`, or `None` if it was dropped.
fn websocket(id: u64) -> Option<WebSocket> {
    REGISTRY
        .try_with(|registry| registry.borrow().get(&id).map(|js| js.ws.clone()))
        .ok()
        .flatten()
}

/// Requests a session from `window.webviewSockets` and stores it.
pub(crate) async fn init() -> io::Result<()> {
    let api = host_api().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "window.webviewSockets is not available",
        )
    })?;
    let request = Reflect::get(&api, &"requestSession".into())
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
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

/// Stores a session for the sockets opened from now on.
pub(crate) fn init_with_session(url: &str, token: &str) -> io::Result<()> {
    let token = parse_token(token)?;
    let url = resolve_ws_url(url)?;
    debug!(%url, "udp session set");
    *SESSION.lock().expect("poisoned") = Some(Session { url, token });
    Ok(())
}

/// Returns whether a session is set or the host API is present.
pub(crate) fn is_available() -> bool {
    SESSION.lock().expect("poisoned").is_some() || host_api().is_some()
}

/// Returns `window.webviewSockets` if the host app injected it.
fn host_api() -> Option<JsValue> {
    let api = Reflect::get(&js_sys::global(), &"webviewSockets".into()).ok()?;
    api.is_object().then_some(api)
}

/// Returns the string property `key` of `object`.
fn get_string(object: &JsValue, key: &str) -> Option<String> {
    Reflect::get(object, &key.into()).ok()?.as_string()
}

/// Maps a rejected promise or thrown JS value to an io error.
fn js_error(err: JsValue) -> io::Error {
    let name = get_string(&err, "name").unwrap_or_default();
    let message = get_string(&err, "message")
        .or_else(|| err.as_string())
        .unwrap_or_else(|| format!("{err:?}"));
    let kind = match name.as_str() {
        "NotAllowedError" => io::ErrorKind::PermissionDenied,
        "SyntaxError" | "TypeError" => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("{name}: {message}"))
}

/// Parses a session token from 64 hex chars.
fn parse_token(token: &str) -> io::Result<[u8; proto::TOKEN_LEN]> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "token must be 64 hex chars");
    let bytes = token.as_bytes();
    if bytes.len() != proto::TOKEN_LEN * 2 {
        return Err(invalid());
    }
    let mut out = [0u8; proto::TOKEN_LEN];
    for (byte, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = (pair[0] as char).to_digit(16).ok_or_else(invalid)?;
        let low = (pair[1] as char).to_digit(16).ok_or_else(invalid)?;
        *byte = (high * 16 + low) as u8;
    }
    Ok(out)
}

/// Resolves `url` against the current location.
///
/// An `http` or `https` URL becomes a `ws` or `wss` URL.
fn resolve_ws_url(url: &str) -> io::Result<String> {
    let invalid = |err: JsValue| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad session url {url:?}: {err:?}"),
        )
    };
    let base = Reflect::get(&js_sys::global(), &"location".into())
        .ok()
        .and_then(|location| get_string(&location, "href"));
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

/// Maps an ERROR frame to an error kind and message.
fn proto_error(code: proto::ErrorCode, message: &str) -> (io::ErrorKind, String) {
    let kind = match code {
        proto::ErrorCode::NotAllowed => io::ErrorKind::PermissionDenied,
        proto::ErrorCode::InvalidInput => io::ErrorKind::InvalidInput,
        proto::ErrorCode::AddrInUse => io::ErrorKind::AddrInUse,
        proto::ErrorCode::NameNotResolved => io::ErrorKind::NotFound,
        proto::ErrorCode::Limit | proto::ErrorCode::Io => io::ErrorKind::Other,
    };
    (kind, format!("host error {}: {message}", code.name()))
}

/// Maps a WebSocket close code to the error kind reported to the caller.
///
/// The host closes with [`proto::close_code::REVOKED`] or
/// [`proto::close_code::SETTING_OFF`] when the user withdrew access. Those
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

/// Returns a short description of a WebSocket close code.
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

/// Handle to a socket. Clones share the socket.
#[derive(Debug, Clone)]
pub(crate) struct Socket {
    shared: Arc<Shared>,
}

/// The part of a socket that all handles share.
#[derive(Debug)]
struct Shared {
    id: u64,
    state: Mutex<State>,
}

/// How far the socket got in its life.
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
    /// Returns the number of bytes not yet consumed.
    fn remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Length of the next segment.
    fn next_len(&self) -> usize {
        match self.stride {
            0 => self.remaining(),
            stride => stride.min(self.remaining()),
        }
    }
}

/// How the socket was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    /// Bound to a local address, see [`Socket::bind`].
    Bound,
    /// Connected to a host name, see [`Socket::connect_host`].
    ///
    /// Sends wait for BOUND, which sets the peer.
    Connected,
}

/// The mutable state of a socket.
#[derive(Debug)]
struct State {
    phase: Phase,
    mode: OpenMode,
    /// The address passed to bind, reported until BOUND arrives.
    requested: SocketAddr,
    /// The local address reported in BOUND.
    local: Option<SocketAddr>,
    interface_addrs: Vec<SocketAddr>,
    max_gso_segments: usize,
    gro_segments: usize,
    /// The connected peer, set by `connect` or by BOUND in connected mode.
    peer: Option<SocketAddr>,
    /// The encoded HELLO, sent when the WebSocket opens.
    hello: Vec<u8>,
    /// Received datagrams not yet read.
    recv: VecDeque<Datagram>,
    /// Bytes not yet read in `recv`.
    recv_bytes: usize,
    /// Encoded messages not yet handed to the WebSocket.
    out: Vec<Vec<u8>>,
    /// Bytes in `out`.
    out_bytes: usize,
    /// Last seen `WebSocket.bufferedAmount`.
    buffered: usize,
    /// Sends return `WouldBlock` while the backlog is too large.
    paused: bool,
    flush_scheduled: bool,
    recheck_scheduled: bool,
    /// The last error the host reported for a send, for `take_error`.
    pending_error: Option<(io::ErrorKind, String)>,
    recv_wakers: Vec<Waker>,
    send_wakers: Vec<Waker>,
    bound_wakers: Vec<Waker>,
}

impl State {
    /// Returns the error of a failed socket, or `None` if it did not fail.
    fn failed(&self) -> Option<io::Error> {
        match &self.phase {
            Phase::Failed(kind, message) => Some(io::Error::new(*kind, message.clone())),
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

    /// Appends a DATA record to the last outgoing message, or to a new one.
    fn enqueue(&mut self, data: proto::Data<'_>) {
        let len = data.encoded_len();
        match self.out.last_mut() {
            Some(message) if message.len() + len <= proto::MAX_MESSAGE_LEN => {
                proto::encode_data(&data, message)
            }
            _ => {
                let mut message = Vec::with_capacity(len);
                proto::encode_data(&data, &mut message);
                self.out.push(message);
            }
        }
        self.out_bytes += len;
    }

    /// Drops queued datagrams not from the connected peer.
    fn skip_foreign(&mut self) {
        if let Some(peer) = self.peer {
            while self
                .recv
                .front()
                .is_some_and(|datagram| datagram.from != peer)
            {
                let datagram = self.recv.pop_front().expect("checked");
                self.recv_bytes -= datagram.remaining();
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

    /// Marks the socket as failed and returns the wakers to wake.
    fn fail(&mut self, kind: io::ErrorKind, message: String) -> Vec<Waker> {
        if matches!(self.phase, Phase::Failed(..)) {
            return Vec::new();
        }
        self.phase = Phase::Failed(kind, message);
        self.out.clear();
        self.out_bytes = 0;
        self.take_all_wakers()
    }

    /// Takes the wakers of all waiting tasks.
    fn take_all_wakers(&mut self) -> Vec<Waker> {
        let mut wakers = std::mem::take(&mut self.recv_wakers);
        wakers.append(&mut self.send_wakers);
        wakers.append(&mut self.bound_wakers);
        wakers
    }
}

/// Adds the waker of `cx` to `wakers` unless an equivalent one is there.
fn add_waker(wakers: &mut Vec<Waker>, cx: &Context<'_>) {
    if !wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
        wakers.push(cx.waker().clone());
    }
}

/// Wakes all `wakers`.
fn wake(wakers: Vec<Waker>) {
    for waker in wakers {
        waker.wake();
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        let js = REGISTRY
            .try_with(|registry| registry.borrow_mut().remove(&self.id))
            .ok()
            .flatten();
        let Some(js) = js else { return };
        js.ws.set_onopen(None);
        js.ws.set_onmessage(None);
        js.ws.set_onclose(None);
        // Like std, datagrams passed to send are not lost on drop.
        let state = self.state.get_mut().expect("poisoned");
        if matches!(state.phase, Phase::Bound) {
            for message in state.out.drain(..) {
                js.ws.send_with_u8_array(&message).ok();
            }
        }
        js.ws.close_with_code(proto::close_code::NORMAL).ok();
        trace!(id = self.id, "udp socket dropped");
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("poisoned")
    }

    /// Sends HELLO when the WebSocket opens.
    fn on_open(&self) {
        let hello = std::mem::take(&mut self.lock().hello);
        if let Some(ws) = websocket(self.id)
            && let Err(err) = ws.send_with_u8_array(&hello)
        {
            let err = js_error(err);
            let wakers = self.lock().fail(err.kind(), err.to_string());
            wake(wakers);
        }
    }

    /// Handles a message from the host.
    fn on_message(self: &Arc<Self>, event: MessageEvent) {
        let Ok(buf) = event.data().dyn_into::<ArrayBuffer>() else {
            warn!(id = self.id, "ignoring non-binary message");
            return;
        };
        let message = Uint8Array::new(&buf).to_vec();
        let mut state = self.lock();
        let wakers = match state.phase {
            Phase::Connecting => self.on_first(&mut state, &message),
            Phase::Bound => on_data(&mut state, &message),
            Phase::Failed(..) => Vec::new(),
        };
        let close = matches!(state.phase, Phase::Failed(..));
        drop(state);
        if close && let Some(ws) = websocket(self.id) {
            ws.close().ok();
        }
        wake(wakers);
    }

    /// Handles the first message, BOUND or ERROR.
    fn on_first(self: &Arc<Self>, state: &mut State, message: &[u8]) -> Vec<Waker> {
        match proto::decode_first(message) {
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
                let (kind, message) = proto_error(err.code, err.message);
                debug!(id = self.id, %message, "udp bind refused");
                state.fail(kind, message)
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

    /// Fails the socket when the WebSocket closes.
    fn on_close(&self, event: CloseEvent) {
        let code = event.code();
        let reason = event.reason();
        debug!(id = self.id, code, %reason, "udp websocket closed");
        let mut state = self.lock();
        let when = match state.phase {
            Phase::Connecting => " before bind",
            _ => "",
        };
        let mut message = format!(
            "connection to host closed{when} ({code} {})",
            close_reason(code)
        );
        if !reason.is_empty() {
            message.push_str(": ");
            message.push_str(&reason);
        }
        let wakers = state.fail(close_kind(code), message);
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
        let callback = Closure::once_into_js(move || {
            if let Some(shared) = weak.upgrade() {
                shared.flush();
            }
        });
        queue_microtask(callback.unchecked_ref());
    }

    /// Hands queued messages to the WebSocket.
    fn flush(self: &Arc<Self>) {
        let mut state = self.lock();
        state.flush_scheduled = false;
        if !matches!(state.phase, Phase::Bound) {
            return;
        }
        let out = std::mem::take(&mut state.out);
        state.out_bytes = 0;
        drop(state);
        let Some(ws) = websocket(self.id) else {
            return;
        };
        for message in &out {
            if let Err(err) = ws.send_with_u8_array(message) {
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
        let callback = Closure::once_into_js(move || {
            if let Some(shared) = weak.upgrade() {
                shared.recheck();
            }
        });
        set_timeout(callback.unchecked_ref(), RECHECK_MS);
    }

    /// Updates the backlog and wakes waiting senders once it is small enough.
    fn recheck(self: &Arc<Self>) {
        let ws = websocket(self.id);
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
            && let Some(ws) = websocket(self.id)
        {
            state.buffered = ws.buffered_amount() as usize;
            state.update_pressure();
        }
    }
}

/// Handles a message after BOUND: received datagrams or a send error.
fn on_data(state: &mut State, message: &[u8]) -> Vec<Waker> {
    if proto::message_kind(message) == Some(proto::KIND_ERROR) {
        match proto::decode_first(message) {
            Ok(proto::Frame::Error(err)) => {
                let (kind, message) = proto_error(err.code, err.message);
                debug!(%message, "udp send failed on host");
                state.pending_error = Some((kind, message));
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
    let records = match proto::decode_data(message) {
        Ok(records) => records,
        Err(err) => return state.fail(io::ErrorKind::InvalidData, format!("bad message: {err}")),
    };
    let mut received = false;
    for record in records {
        let record = match record {
            Ok(record) => record,
            Err(err) => {
                return state.fail(io::ErrorKind::InvalidData, format!("bad record: {err}"));
            }
        };
        if state.peer.is_some_and(|peer| peer != record.peer) {
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
        received = true;
    }
    if received {
        std::mem::take(&mut state.recv_wakers)
    } else {
        Vec::new()
    }
}

/// Returns the error for a send or receive without a peer.
fn not_connected() -> io::Error {
    io::Error::new(io::ErrorKind::NotConnected, "socket is not connected")
}

/// Returns a `WouldBlock` error.
fn would_block() -> io::Error {
    io::Error::from(io::ErrorKind::WouldBlock)
}

impl Socket {
    /// Opens a socket bound to `addr` on the host.
    pub(crate) fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::open(addr, OpenMode::Bound, |token| {
            proto::Hello::bind(token, addr)
        })
    }

    /// Opens a socket on the host connected to `host:port`.
    pub(crate) fn connect_host(
        host: &str,
        port: u16,
        dns: Option<DnsQueryType>,
    ) -> io::Result<Self> {
        let dns_query_type = dns.map(|dns| match dns {
            DnsQueryType::Ipv4 => proto::DnsQueryType::Ipv4,
            DnsQueryType::Ipv6 => proto::DnsQueryType::Ipv6,
        });
        let any = match dns {
            Some(DnsQueryType::Ipv6) => SocketAddr::from(([0u16; 8], 0)),
            _ => SocketAddr::from(([0, 0, 0, 0], 0)),
        };
        Self::open(any, OpenMode::Connected, |token| proto::Hello {
            token,
            mode: proto::Mode::Connected {
                remote_host: host,
                remote_port: port,
                dns_query_type,
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        })
    }

    /// Opens the WebSocket and queues the HELLO built by `hello`.
    ///
    /// `requested` is the local address reported until BOUND arrives.
    fn open<'a>(
        requested: SocketAddr,
        mode: OpenMode,
        hello: impl FnOnce([u8; proto::TOKEN_LEN]) -> proto::Hello<'a>,
    ) -> io::Result<Self> {
        let session = SESSION.lock().expect("poisoned").clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no UDP session, call webview_sockets::init() first",
            )
        })?;
        let ws = WebSocket::new(&session.url).map_err(js_error)?;
        ws.set_binary_type(BinaryType::Arraybuffer);

        let mut hello_message = Vec::new();
        proto::encode_hello(&hello(session.token), &mut hello_message);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared {
            id,
            state: Mutex::new(State {
                phase: Phase::Connecting,
                mode,
                requested,
                local: None,
                interface_addrs: Vec::new(),
                max_gso_segments: 1,
                gro_segments: 1,
                peer: None,
                hello: hello_message,
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
            if let Some(shared) = Weak::upgrade(&weak) {
                shared.on_open();
            }
        });
        let weak = Arc::downgrade(&shared);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event| {
            if let Some(shared) = Weak::upgrade(&weak) {
                shared.on_message(event);
            }
        });
        let weak = Arc::downgrade(&shared);
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event| {
            if let Some(shared) = Weak::upgrade(&weak) {
                shared.on_close(event);
            }
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        REGISTRY.with(|registry| {
            registry.borrow_mut().insert(
                id,
                JsSocket {
                    ws,
                    _on_open: on_open,
                    _on_message: on_message,
                    _on_close: on_close,
                },
            )
        });
        debug!(id, %requested, ?mode, "udp socket opening");
        Ok(Self { shared })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared.lock()
    }

    pub(crate) async fn bound(&self) -> io::Result<()> {
        std::future::poll_fn(|cx| {
            let mut state = self.lock();
            match &state.phase {
                Phase::Bound => Poll::Ready(Ok(())),
                Phase::Failed(kind, message) => {
                    Poll::Ready(Err(io::Error::new(*kind, message.clone())))
                }
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
        let copied = len.min(buf.len());
        buf[..copied].copy_from_slice(&front.data[front.offset..front.offset + copied]);
        state.consume(len);
        Ok((copied, from))
    }

    pub(crate) fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let peer = {
            let state = self.lock();
            match (state.peer, state.mode) {
                (Some(peer), _) => peer,
                (None, OpenMode::Connected) => {
                    return Err(state.failed().unwrap_or_else(would_block));
                }
                (None, OpenMode::Bound) => return Err(not_connected()),
            }
        };
        self.send_to(buf, peer)
    }

    pub(crate) fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        {
            let state = self.lock();
            if state.peer.is_none() {
                return Err(match state.mode {
                    OpenMode::Connected => state.failed().unwrap_or_else(would_block),
                    OpenMode::Bound => not_connected(),
                });
            }
        }
        self.recv_from(buf).map(|(len, _)| len)
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
            .map(|(kind, message)| io::Error::new(kind, message)))
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
            size => size.min(buf.len()),
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
        let stride = front.next_len();
        // Whole segments that fit, at least one (truncated if needed).
        let take = if stride == 0 || stride > buf.len() {
            stride
        } else {
            (buf.len() / stride * stride).min(front.remaining())
        };
        let copied = take.min(buf.len());
        buf[..copied].copy_from_slice(&front.data[front.offset..front.offset + copied]);
        state.consume(take);
        Ok((copied, from, stride.min(copied)))
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
        let waiting_for_peer =
            state.mode == OpenMode::Connected && matches!(state.phase, Phase::Connecting);
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

/// Accepts only `None`, because sockets on wasm cannot block.
fn no_timeout(dur: Option<Duration>) -> io::Result<()> {
    match dur {
        None => Ok(()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "timeouts are not supported on wasm, use the async helpers",
        )),
    }
}
