//! The WebSocket route: upgrade checks, HELLO, one UDP socket per connection.

use std::{
    collections::HashMap,
    io::{self, IoSliceMut},
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use noq_udp::RecvMeta;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{Instrument, debug, info_span, trace, warn};
use webview_sockets_proto::{
    Bound, Data, DnsQueryType, ErrorCode, ErrorFrame, Frame, Mode, Record, WS_PATH, close_code,
    decode_first, decode_records, encode_bound, encode_data, encode_error,
};

use crate::{
    Limits, SessionEntry, TabId, Token, UdpHost, policy,
    sock::{Sock, SockOptions},
};

/// Received datagrams are batched into WebSocket messages of about this size.
const BATCH_LEN: usize = 64 * 1024;

/// Receive buffers per recv call.
const RECV_SLOTS: usize = 4;

/// WebSocket close code for protocol errors.
const CLOSE_PROTOCOL: u16 = 1002;
/// WebSocket close code for unsupported (text) messages.
const CLOSE_UNSUPPORTED: u16 = 1003;
/// WebSocket close code for policy violations (bad token).
const CLOSE_POLICY: u16 = 1008;

/// How long a resolved host name of a DATA_NAMED record is reused.
const NAME_CACHE_TTL: Duration = Duration::from_secs(30);
/// Maximum number of cached host names per socket.
const NAME_CACHE_LEN: usize = 64;

pub(crate) fn router(host: UdpHost) -> Router {
    Router::new().route(WS_PATH, get(upgrade)).with_state(host)
}

async fn upgrade(
    State(host): State<UdpHost>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !host.enabled() {
        return (StatusCode::FORBIDDEN, "UDP is turned off").into_response();
    }
    let get = |name| headers.get(name).and_then(|v| v.to_str().ok());
    let origin = match policy::check_upgrade(
        get(header::HOST),
        get(header::ORIGIN),
        &host.inner.config.host_policy,
    ) {
        Ok(origin) => origin.to_string(),
        Err(reason) => {
            debug!(reason, "rejected udp websocket upgrade");
            return (StatusCode::FORBIDDEN, reason).into_response();
        }
    };
    let max = host.inner.config.limits.max_message_len;
    ws.max_message_size(max)
        .max_frame_size(max)
        .on_upgrade(move |ws| {
            let span = info_span!("udp", %origin, tab = tracing::field::Empty);
            run(host, origin, ws).instrument(span)
        })
}

/// Why a connection or a send is refused.
pub(crate) struct Reject {
    code: ErrorCode,
    message: String,
    close: u16,
}

impl Reject {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        let close = match code {
            ErrorCode::Limit => close_code::LIMIT,
            ErrorCode::Io => close_code::IO_ERROR,
            ErrorCode::NotAllowed | ErrorCode::InvalidInput => CLOSE_POLICY,
            ErrorCode::AddrInUse | ErrorCode::NameNotResolved => close_code::NORMAL,
        };
        Self {
            code,
            message: message.into(),
            close,
        }
    }
}

async fn run(host: UdpHost, origin: String, mut ws: WebSocket) {
    let session = match setup(&host, &origin, &mut ws).await {
        Ok(session) => session,
        Err(reject) => {
            debug!(code = reject.code.name(), message = %reject.message, "udp session rejected");
            send_error(&mut ws, reject.code, &reject.message).await.ok();
            close(ws, reject.close).await;
            return;
        }
    };
    let code = serve(&host, session, &mut ws).await;
    if let Some(code) = code {
        close(ws, code).await;
    }
}

struct Session {
    guard: SessionGuard,
    close_rx: oneshot::Receiver<u16>,
    sock: Arc<Sock>,
    /// The peer in connected mode.
    remote: Option<SocketAddr>,
}

async fn setup(host: &UdpHost, origin: &str, ws: &mut WebSocket) -> Result<Session, Reject> {
    let limits = &host.inner.config.limits;
    let msg = match tokio::time::timeout(limits.hello_timeout, ws.recv()).await {
        Ok(Some(Ok(Message::Binary(msg)))) => msg,
        Ok(_) => return Err(Reject::new(ErrorCode::InvalidInput, "expected HELLO")),
        Err(_) => return Err(Reject::new(ErrorCode::InvalidInput, "no HELLO in time")),
    };
    let hello = match decode_first(&msg) {
        Ok(Frame::Hello(hello)) => hello,
        Ok(_) => return Err(Reject::new(ErrorCode::InvalidInput, "expected HELLO")),
        Err(err) => return Err(Reject::new(ErrorCode::InvalidInput, err.to_string())),
    };
    let (guard, close_rx) = host.register(&hello.token, origin)?;
    let span = tracing::Span::current();
    span.record("tab", guard.tab.0);
    let mut opts = SockOptions {
        ipv6_only: None,
        send_buffer: buffer_size(hello.send_buffer_size, limits),
        recv_buffer: buffer_size(hello.receive_buffer_size, limits),
    };
    let (bind_addr, remote) = match hello.mode {
        Mode::Bound { local, ipv6_only } => {
            policy::check_bind(local).map_err(|m| Reject::new(ErrorCode::InvalidInput, m))?;
            if ipv6_only.is_some() && local.ip() != IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED) {
                return Err(Reject::new(
                    ErrorCode::InvalidInput,
                    "ipv6Only needs the local address ::",
                ));
            }
            opts.ipv6_only = ipv6_only;
            (local, None)
        }
        Mode::Connected {
            remote_host,
            remote_port,
            dns_query_type,
        } => {
            let remote = resolve(remote_host, remote_port, dns_query_type, limits).await?;
            let remote = policy::check_destination(remote)
                .map_err(|m| Reject::new(ErrorCode::InvalidInput, m))?;
            let any = match remote {
                SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
                SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
            };
            (any, Some(remote))
        }
    };
    let sock = Sock::bind(bind_addr, &opts).map_err(|err| {
        let code = match err.kind() {
            io::ErrorKind::AddrInUse => ErrorCode::AddrInUse,
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::InvalidInput => {
                ErrorCode::InvalidInput
            }
            _ => ErrorCode::Io,
        };
        Reject::new(code, format!("bind {bind_addr}: {err}"))
    })?;
    let mut local = sock
        .local_addr()
        .map_err(|err| Reject::new(ErrorCode::Io, err.to_string()))?;
    if let Some(remote) = remote {
        // Report the address the peer sees, like a connected socket would.
        match crate::sock::route_local_ip(remote) {
            Ok(ip) => local.set_ip(ip),
            Err(err) => {
                return Err(Reject::new(
                    ErrorCode::Io,
                    format!("no route to {remote}: {err}"),
                ));
            }
        }
    }
    let interface_addrs = tokio::task::spawn_blocking(move || crate::sock::interface_addrs(local))
        .await
        .unwrap_or_default();
    let bound = Bound {
        local,
        remote,
        max_gso_segments: sock.max_gso_segments().min(u16::MAX as usize) as u16,
        gro_segments: sock.gro_segments().min(u16::MAX as usize) as u16,
        interface_addrs,
    };
    let mut out = Vec::new();
    encode_bound(&bound, &mut out);
    if ws.send(Message::Binary(out.into())).await.is_err() {
        return Err(Reject::new(ErrorCode::Io, "websocket closed"));
    }
    debug!(%local, ?remote, gso = bound.max_gso_segments, gro = bound.gro_segments, "udp socket bound");
    Ok(Session {
        guard,
        close_rx,
        sock: Arc::new(sock),
        remote,
    })
}

/// Returns the kernel buffer size for a requested size, 0 meaning default.
fn buffer_size(requested: u32, limits: &Limits) -> usize {
    match requested {
        0 => limits.max_socket_buffer,
        n => (n as usize).min(limits.max_socket_buffer),
    }
}

/// Resolves a host name or IP literal to one socket address.
///
/// IP literals (IPv6 with or without brackets) are used as is. Names are
/// looked up with the OS resolver and filtered by `dns`; the first match
/// wins.
pub(crate) async fn resolve(
    host: &str,
    port: u16,
    dns: Option<DnsQueryType>,
    limits: &Limits,
) -> Result<SocketAddr, Reject> {
    let literal = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    if !is_host_name(host) {
        return Err(Reject::new(
            ErrorCode::InvalidInput,
            format!("invalid host name {host:?}"),
        ));
    }
    let lookup = tokio::net::lookup_host((host, port));
    let addrs = match tokio::time::timeout(limits.resolve_timeout, lookup).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(err)) => {
            return Err(Reject::new(
                ErrorCode::NameNotResolved,
                format!("resolving {host}: {err}"),
            ));
        }
        Err(_) => {
            return Err(Reject::new(
                ErrorCode::NameNotResolved,
                format!("resolving {host}: timed out"),
            ));
        }
    };
    let found = addrs
        .map(|a| SocketAddr::new(a.ip().to_canonical(), a.port()))
        .find(|a| dns.is_none_or(|d| d.matches(&a.ip())));
    trace!(%host, ?dns, ?found, "resolved");
    found.ok_or_else(|| {
        let family = dns.map_or("", |d| d.name());
        Reject::new(
            ErrorCode::NameNotResolved,
            format!("no {family} address for {host}"),
        )
    })
}

/// Returns whether `host` looks like a DNS name: labels of letters, digits,
/// `-` and `_`, at most 253 bytes.
fn is_host_name(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Runs a bound session. Returns the close code to send, or `None` if the
/// WebSocket is already gone.
async fn serve(host: &UdpHost, session: Session, ws: &mut WebSocket) -> Option<u16> {
    let Session {
        guard,
        mut close_rx,
        sock,
        remote,
    } = session;
    let limits = &host.inner.config.limits;
    let (tx, mut rx) = mpsc::channel(limits.recv_queue_len);
    let mut recv_task = AbortOnDrop(tokio::spawn(recv_loop(sock.clone(), remote, tx)));
    // Names resolve to the socket's family unless the page asks otherwise.
    let default_dns = match sock.local_addr() {
        Ok(SocketAddr::V4(_)) => Some(DnsQueryType::Ipv4),
        _ => None,
    };
    let mut sender = Sender {
        sock: &sock,
        remote,
        default_dns,
        limits,
        names: HashMap::new(),
    };
    let code = loop {
        tokio::select! {
            code = &mut close_rx => break code.unwrap_or(close_code::NORMAL),
            frame = rx.recv() => match frame {
                Some(frame) => {
                    if ws.send(Message::Binary(frame.into())).await.is_err() {
                        return None;
                    }
                }
                None => {
                    let res = (&mut recv_task.0).await;
                    warn!(?res, "udp receive failed");
                    break close_code::IO_ERROR;
                }
            },
            msg = ws.recv() => match msg {
                Some(Ok(Message::Binary(msg))) => {
                    if let Err(code) = sender.handle(ws, &msg).await {
                        break code;
                    }
                }
                Some(Ok(Message::Text(_))) => break CLOSE_UNSUPPORTED,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) => {
                    // Reading on flushes the queued close reply.
                    drop(guard);
                    let drain = async { while let Some(Ok(_)) = ws.recv().await {} };
                    tokio::time::timeout(Duration::from_secs(1), drain).await.ok();
                    return None;
                }
                Some(Err(_)) | None => return None,
            },
        }
    };
    debug!(code, "closing udp session");
    drop(guard);
    Some(code)
}

/// Sends the datagrams of page messages.
struct Sender<'a> {
    sock: &'a Sock,
    /// The peer in connected mode; other destinations are refused.
    remote: Option<SocketAddr>,
    /// Address family for names without a dns query type.
    default_dns: Option<DnsQueryType>,
    limits: &'a Limits,
    /// Resolved DATA_NAMED host names with the time of the lookup.
    names: HashMap<(String, Option<DnsQueryType>), (IpAddr, Instant)>,
}

impl Sender<'_> {
    /// Sends the records of one page message. Returns a close code on a
    /// protocol error or a dead WebSocket.
    ///
    /// Name lookups for DATA_NAMED records run inline, so a slow resolver
    /// delays the following datagrams of this socket.
    async fn handle(&mut self, ws: &mut WebSocket, msg: &[u8]) -> Result<(), u16> {
        let records = decode_records(msg).map_err(|_| CLOSE_PROTOCOL)?;
        for rec in records {
            let rec = rec.map_err(|err| {
                debug!(%err, "bad data record");
                CLOSE_PROTOCOL
            })?;
            let (dest, ecn, segment_size, payload) = match rec {
                Record::Data(d) => (d.peer, d.ecn, d.segment_size, d.payload),
                Record::Named(n) => {
                    if self.remote.is_some() {
                        send_error(
                            ws,
                            ErrorCode::InvalidInput,
                            "a connected socket cannot send to a host name",
                        )
                        .await?;
                        continue;
                    }
                    match self.lookup(n.host, n.port, n.dns_query_type).await {
                        Ok(dest) => (dest, n.ecn, n.segment_size, n.payload),
                        Err(reject) => {
                            send_error(ws, reject.code, &reject.message).await?;
                            continue;
                        }
                    }
                }
            };
            let dest = match policy::check_destination(dest) {
                Ok(dest) => dest,
                Err(reason) => {
                    trace!(peer = %dest, reason, "forbidden destination");
                    send_error(ws, ErrorCode::InvalidInput, reason).await?;
                    continue;
                }
            };
            if let Some(remote) = self.remote
                && dest != remote
            {
                trace!(peer = %dest, %remote, "send to other than the connected peer");
                send_error(
                    ws,
                    ErrorCode::InvalidInput,
                    "a connected socket only sends to its remote address",
                )
                .await?;
                continue;
            }
            if let Err(err) = self.sock.send(dest, ecn, segment_size, payload).await {
                let code = match err.kind() {
                    io::ErrorKind::InvalidInput => ErrorCode::InvalidInput,
                    _ if err.raw_os_error() == Some(EMSGSIZE) => ErrorCode::InvalidInput,
                    _ => ErrorCode::Io,
                };
                send_error(ws, code, &format!("send to {dest}: {err}")).await?;
            }
        }
        Ok(())
    }

    /// Resolves a DATA_NAMED host, reusing recent results.
    async fn lookup(
        &mut self,
        host: &str,
        port: u16,
        dns: Option<DnsQueryType>,
    ) -> Result<SocketAddr, Reject> {
        let dns = dns.or(self.default_dns);
        let key = (host.to_string(), dns);
        if let Some((ip, at)) = self.names.get(&key)
            && at.elapsed() < NAME_CACHE_TTL
        {
            return Ok(SocketAddr::new(*ip, port));
        }
        let addr = resolve(host, port, dns, self.limits).await?;
        if self.names.len() >= NAME_CACHE_LEN {
            self.names
                .retain(|_, (_, at)| at.elapsed() < NAME_CACHE_TTL);
            if self.names.len() >= NAME_CACHE_LEN {
                self.names.clear();
            }
        }
        self.names.insert(key, (addr.ip(), Instant::now()));
        Ok(addr)
    }
}

#[cfg(unix)]
const EMSGSIZE: i32 = libc::EMSGSIZE;
#[cfg(windows)]
const EMSGSIZE: i32 = 10040; // WSAEMSGSIZE

/// Reads datagrams and queues them as WebSocket messages of up to
/// [`BATCH_LEN`] bytes. Drops messages when the queue is full, and in
/// connected mode datagrams from other peers than `remote`.
async fn recv_loop(
    sock: Arc<Sock>,
    remote: Option<SocketAddr>,
    tx: mpsc::Sender<Vec<u8>>,
) -> io::Result<()> {
    let slot_len = (sock.gro_segments() * 1500).max(u16::MAX as usize);
    let mut buf = vec![0u8; slot_len * RECV_SLOTS];
    let mut meta = [RecvMeta::default(); RECV_SLOTS];
    let mut frame = Vec::new();
    let mut dropped = 0u64;
    let mut push = |frame: &mut Vec<u8>| -> bool {
        match tx.try_send(std::mem::take(frame)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped += 1;
                if dropped.is_power_of_two() {
                    debug!(dropped, "udp receive queue full, dropping");
                }
                true
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    };
    loop {
        sock.readable().await?;
        loop {
            let res = {
                let mut slices: Vec<IoSliceMut<'_>> =
                    buf.chunks_mut(slot_len).map(IoSliceMut::new).collect();
                sock.try_recv(&mut slices, &mut meta)
            };
            let n = match res {
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if is_transient(&err) => continue,
                Err(err) => return Err(err),
            };
            for (i, m) in meta.iter().take(n).enumerate() {
                let peer = canonical(m.addr);
                if remote.is_some_and(|r| r != peer) {
                    trace!(%peer, "dropping datagram from other than the connected peer");
                    continue;
                }
                let payload = &buf[i * slot_len..][..m.len];
                let segment_size = if m.stride < m.len {
                    u16::try_from(m.stride).unwrap_or(0)
                } else {
                    0
                };
                let rec = Data {
                    ecn: m.ecn.map(|e| e as u8).unwrap_or(0),
                    peer,
                    segment_size,
                    payload,
                };
                if !frame.is_empty()
                    && frame.len() + rec.encoded_len() > BATCH_LEN
                    && !push(&mut frame)
                {
                    return Ok(());
                }
                encode_data(&rec, &mut frame);
            }
        }
        if !frame.is_empty() && !push(&mut frame) {
            return Ok(());
        }
    }
}

/// Errors on an unconnected UDP socket that do not end the session (ICMP
/// feedback on some platforms).
fn is_transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
    )
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

async fn send_error(ws: &mut WebSocket, code: ErrorCode, message: &str) -> Result<(), u16> {
    let mut out = Vec::new();
    encode_error(&ErrorFrame { code, message }, &mut out);
    ws.send(Message::Binary(out.into()))
        .await
        .map_err(|_| close_code::NORMAL)
}

async fn close(mut ws: WebSocket, code: u16) {
    let reason = match code {
        close_code::REVOKED => "revoked",
        close_code::SETTING_OFF => "setting off",
        close_code::NAVIGATED => "navigated",
        close_code::LIMIT => "limit",
        close_code::IO_ERROR => "io error",
        CLOSE_POLICY => "not allowed",
        CLOSE_PROTOCOL => "protocol error",
        _ => "",
    };
    ws.send(Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    })))
    .await
    .ok();
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Removes the session from the host when dropped.
#[derive(Debug)]
pub(crate) struct SessionGuard {
    host: UdpHost,
    id: u64,
    tab: TabId,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let mut state = self.host.inner.lock();
        state.sessions.remove(&self.id);
        self.host.inner.update_active(&state);
    }
}

impl UdpHost {
    /// Validates a HELLO token for `origin` and registers a session.
    fn register(
        &self,
        token: &Token,
        origin: &str,
    ) -> Result<(SessionGuard, oneshot::Receiver<u16>), Reject> {
        let limits = &self.inner.config.limits;
        let mut state = self.inner.lock();
        if !state.stored.enabled {
            return Err(Reject::new(ErrorCode::NotAllowed, "UDP is turned off"));
        }
        let Some(ticket) = state.tickets.get(token) else {
            return Err(Reject::new(
                ErrorCode::NotAllowed,
                "invalid or expired token",
            ));
        };
        if ticket.origin != origin {
            return Err(Reject::new(
                ErrorCode::NotAllowed,
                "token was issued for another origin",
            ));
        }
        let tab = ticket.tab;
        let per_tab = state.sessions.values().filter(|s| s.tab == tab).count();
        if per_tab >= limits.max_sockets_per_tab {
            return Err(Reject::new(
                ErrorCode::Limit,
                "too many sockets for this tab",
            ));
        }
        let per_origin = state
            .sessions
            .values()
            .filter(|s| s.origin == origin)
            .count();
        if per_origin >= limits.max_sockets_per_origin {
            return Err(Reject::new(
                ErrorCode::Limit,
                "too many sockets for this origin",
            ));
        }
        let id = state.next_session;
        state.next_session += 1;
        let (close_tx, close_rx) = oneshot::channel();
        state.sessions.insert(
            id,
            SessionEntry {
                origin: origin.to_string(),
                tab,
                close: Some(close_tx),
            },
        );
        self.inner.update_active(&state);
        Ok((
            SessionGuard {
                host: self.clone(),
                id,
                tab,
            },
            close_rx,
        ))
    }
}
