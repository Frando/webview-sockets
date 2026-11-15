//! The WebSocket route: upgrade checks, HELLO, one UDP socket per connection.

use std::{
    io::{self, IoSliceMut},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
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
use iroh_link_udp_proto::{
    Bound, Data, ErrorCode, ErrorFrame, Frame, WS_PATH, close_code, decode_data, decode_first,
    encode_bound, encode_data, encode_error,
};
use noq_udp::RecvMeta;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tracing::{Instrument, debug, info_span, trace, warn};

use crate::{SessionEntry, TabId, Token, UdpHost, policy, sock::Sock};

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
        host.inner.config.check_host,
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

/// Why a connection is refused after the upgrade.
struct Reject {
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
            ErrorCode::AddrInUse => close_code::NORMAL,
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
}

async fn setup(host: &UdpHost, origin: &str, ws: &mut WebSocket) -> Result<Session, Reject> {
    let timeout = host.inner.config.limits.hello_timeout;
    let msg = match tokio::time::timeout(timeout, ws.recv()).await {
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
    policy::check_bind(hello.bind_addr).map_err(|m| Reject::new(ErrorCode::InvalidInput, m))?;
    let sock = Sock::bind(hello.bind_addr).map_err(|err| {
        let code = match err.kind() {
            io::ErrorKind::AddrInUse => ErrorCode::AddrInUse,
            io::ErrorKind::AddrNotAvailable | io::ErrorKind::InvalidInput => {
                ErrorCode::InvalidInput
            }
            _ => ErrorCode::Io,
        };
        Reject::new(code, format!("bind {}: {err}", hello.bind_addr))
    })?;
    let local = sock
        .local_addr()
        .map_err(|err| Reject::new(ErrorCode::Io, err.to_string()))?;
    let interface_addrs = tokio::task::spawn_blocking(move || crate::sock::interface_addrs(local))
        .await
        .unwrap_or_default();
    let bound = Bound {
        local,
        max_gso_segments: sock.max_gso_segments().min(u16::MAX as usize) as u16,
        gro_segments: sock.gro_segments().min(u16::MAX as usize) as u16,
        interface_addrs,
    };
    let mut out = Vec::new();
    encode_bound(&bound, &mut out);
    if ws.send(Message::Binary(out.into())).await.is_err() {
        return Err(Reject::new(ErrorCode::Io, "websocket closed"));
    }
    debug!(%local, gso = bound.max_gso_segments, gro = bound.gro_segments, "udp socket bound");
    Ok(Session {
        guard,
        close_rx,
        sock: Arc::new(sock),
    })
}

/// Runs a bound session. Returns the close code to send, or `None` if the
/// WebSocket is already gone.
async fn serve(host: &UdpHost, session: Session, ws: &mut WebSocket) -> Option<u16> {
    let Session {
        guard,
        mut close_rx,
        sock,
    } = session;
    let (tx, mut rx) = mpsc::channel(host.inner.config.limits.recv_queue_len);
    let mut recv_task = AbortOnDrop(tokio::spawn(recv_loop(sock.clone(), tx)));
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
                    if let Err(code) = handle_data(&sock, ws, &msg).await {
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

/// Sends the DATA records of one page message. Returns a close code on a
/// protocol error or a dead WebSocket.
async fn handle_data(sock: &Sock, ws: &mut WebSocket, msg: &[u8]) -> Result<(), u16> {
    let records = decode_data(msg).map_err(|_| CLOSE_PROTOCOL)?;
    for rec in records {
        let rec = rec.map_err(|err| {
            debug!(%err, "bad data record");
            CLOSE_PROTOCOL
        })?;
        let dest = match policy::check_destination(rec.peer) {
            Ok(dest) => dest,
            Err(reason) => {
                trace!(peer = %rec.peer, reason, "forbidden destination");
                send_error(ws, ErrorCode::InvalidInput, reason).await?;
                continue;
            }
        };
        if let Err(err) = sock
            .send(dest, rec.ecn, rec.segment_size, rec.payload)
            .await
        {
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

#[cfg(unix)]
const EMSGSIZE: i32 = libc::EMSGSIZE;
#[cfg(windows)]
const EMSGSIZE: i32 = 10040; // WSAEMSGSIZE

/// Reads datagrams and queues them as WebSocket messages of up to
/// [`BATCH_LEN`] bytes. Drops messages when the queue is full.
async fn recv_loop(sock: Arc<Sock>, tx: mpsc::Sender<Vec<u8>>) -> io::Result<()> {
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
                let payload = &buf[i * slot_len..][..m.len];
                let segment_size = if m.stride < m.len {
                    u16::try_from(m.stride).unwrap_or(0)
                } else {
                    0
                };
                let rec = Data {
                    ecn: m.ecn.map(|e| e as u8).unwrap_or(0),
                    peer: canonical(m.addr),
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
        let per_token = state
            .sessions
            .values()
            .filter(|s| &s.token == token)
            .count();
        if per_token >= limits.max_sockets_per_token {
            return Err(Reject::new(
                ErrorCode::Limit,
                "too many sockets for this session",
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
                token: *token,
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
