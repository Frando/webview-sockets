//! Wire format between web pages and the host for native UDP sockets.
//!
//! One WebSocket connection on [`WS_PATH`] carries one UDP socket. All frames
//! are binary WebSocket messages, integers are big endian.
//!
//! - The page sends [`Hello`] as the first message.
//! - The host answers with [`Bound`] or [`ErrorFrame`] as the first message.
//! - After [`Bound`], both directions send messages holding one or more
//!   [`Data`] records back to back. The host may also send an [`ErrorFrame`]
//!   message for a failed send; the socket stays open.
//!
//! Use [`decode_first`] for messages starting with HELLO, BOUND or ERROR (see
//! [`message_kind`]) and [`decode_data`] for DATA messages. Decoding borrows
//! payloads and strings from the input, it does not copy.
//!
//! The same codec exists in JavaScript, see [`JS_CODEC`] and
//! [`js_codec_classic`].
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// WebSocket path on the page's own origin.
pub const WS_PATH: &str = "/.well-known/iroh-link/udp";

/// Protocol version sent in [`Hello`].
pub const VERSION: u8 = 1;

/// Maximum size of one WebSocket message in bytes (256 KiB).
pub const MAX_MESSAGE_LEN: usize = 256 * 1024;

/// Length of the session token in bytes.
pub const TOKEN_LEN: usize = 32;

/// Message kind byte of a [`Hello`] frame.
pub const KIND_HELLO: u8 = 0x01;
/// Message kind byte of a [`Bound`] frame.
pub const KIND_BOUND: u8 = 0x02;
/// Message kind byte of an [`ErrorFrame`].
pub const KIND_ERROR: u8 = 0x03;
/// Record kind byte of a [`Data`] record.
pub const KIND_DATA: u8 = 0x10;

/// Header bytes of a DATA record besides the address: kind, ecn,
/// segment_size, len.
const DATA_FIXED_LEN: usize = 1 + 1 + 2 + 4;

/// WebSocket close codes the host uses.
pub mod close_code {
    /// Normal close.
    pub const NORMAL: u16 = 1000;
    /// The user revoked UDP access for the origin.
    pub const REVOKED: u16 = 4001;
    /// The UDP setting was switched off.
    pub const SETTING_OFF: u16 = 4002;
    /// The tab navigated away or was closed.
    pub const NAVIGATED: u16 = 4003;
    /// A limit was exceeded.
    pub const LIMIT: u16 = 4004;
    /// The socket failed with an IO error.
    pub const IO_ERROR: u16 = 4005;
}

/// The JavaScript codec as an ES module source.
///
/// See [`js_codec_classic`] for a variant usable in a classic script.
pub const JS_CODEC: &str = include_str!("../js/codec.js");

/// Marker line in [`JS_CODEC`] that starts the ES module export block.
pub const JS_CODEC_EXPORT_MARKER: &str = "// @@iroh-link-udp-codec-module-exports@@";

/// Returns the JavaScript codec without its ES module exports.
///
/// The result declares a single lexical binding `irohLinkUdpCodec` holding
/// `{ encodeHello, decodeFirst, encodeData, decodeData, ... }`. Hosts that
/// inject a classic script should paste it inside their own IIFE so that no
/// name leaks into the page.
pub fn js_codec_classic() -> &'static str {
    match JS_CODEC.split_once(JS_CODEC_EXPORT_MARKER) {
        Some((classic, _)) => classic,
        None => JS_CODEC,
    }
}

/// Error code carried in an [`ErrorFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ErrorCode {
    /// Bad or expired token, or the UDP setting is off.
    NotAllowed = 1,
    /// Invalid input, for example a forbidden destination address.
    InvalidInput = 2,
    /// The requested bind address is in use.
    AddrInUse = 3,
    /// A limit was exceeded.
    Limit = 4,
    /// An IO error on the host.
    Io = 5,
}

impl ErrorCode {
    /// Returns the code for a wire byte, if known.
    pub fn from_u8(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::NotAllowed,
            2 => Self::InvalidInput,
            3 => Self::AddrInUse,
            4 => Self::Limit,
            5 => Self::Io,
            _ => return None,
        })
    }

    /// Returns the name used by the JavaScript codec, e.g. `"NotAllowed"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotAllowed => "NotAllowed",
            Self::InvalidInput => "InvalidInput",
            Self::AddrInUse => "AddrInUse",
            Self::Limit => "Limit",
            Self::Io => "Io",
        }
    }
}

/// HELLO frame, page to host, first and only record of the first message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello {
    /// Session token from `irohHost.udp.requestSession()`.
    pub token: [u8; TOKEN_LEN],
    /// Address to bind: `0.0.0.0:0`, `[::]:0` or a local address and port.
    pub bind_addr: SocketAddr,
}

/// BOUND frame, host to page, reports the bound socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// Local address of the bound socket.
    pub local: SocketAddr,
    /// Maximum number of GSO segments per send, 1 if GSO is not supported.
    pub max_gso_segments: u16,
    /// Maximum number of GRO segments per receive, 1 if GRO is not supported.
    pub gro_segments: u16,
    /// Local interface IPs with the bound port.
    ///
    /// At most 255 entries are encoded, the rest are dropped.
    pub interface_addrs: Vec<SocketAddr>,
}

/// ERROR frame, host to page.
///
/// Sent as the first message instead of [`Bound`], or after [`Bound`] for a
/// failed send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorFrame<'a> {
    /// What went wrong.
    pub code: ErrorCode,
    /// Human readable message.
    ///
    /// At most 65535 bytes are encoded; longer messages are cut at a char
    /// boundary.
    pub message: &'a str,
}

/// A decoded HELLO, BOUND or ERROR frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame<'a> {
    /// See [`Hello`].
    Hello(Hello),
    /// See [`Bound`].
    Bound(Bound),
    /// See [`ErrorFrame`].
    Error(ErrorFrame<'a>),
}

/// DATA record: one datagram or one GSO/GRO batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Data<'a> {
    /// ECN bits, 0 to 3. Encoding masks the value to 2 bits.
    pub ecn: u8,
    /// Destination when sent by the page, source when sent by the host.
    pub peer: SocketAddr,
    /// 0 for a single datagram, else the size of each segment in `payload`.
    ///
    /// The last segment may be shorter.
    pub segment_size: u16,
    /// The datagram bytes.
    pub payload: &'a [u8],
}

impl<'a> Data<'a> {
    /// Returns the datagrams in this record, split by `segment_size`.
    ///
    /// An empty payload yields one empty datagram.
    pub fn segments(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        let payload = self.payload;
        let size = match self.segment_size {
            0 => payload.len().max(1),
            n => n as usize,
        };
        let empty = payload.is_empty();
        payload.chunks(size).chain(empty.then_some(payload))
    }

    /// Returns the number of bytes [`encode_data`] appends for this record.
    pub fn encoded_len(&self) -> usize {
        DATA_FIXED_LEN + addr_len(&self.peer) + self.payload.len()
    }
}

/// Error when decoding a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The message ended in the middle of a frame or record.
    Truncated,
    /// The message is empty.
    Empty,
    /// The message is larger than [`MAX_MESSAGE_LEN`].
    Oversize(usize),
    /// Unknown frame or record kind.
    UnknownKind(u8),
    /// The kind is valid but not allowed here, e.g. DATA in [`decode_first`].
    UnexpectedKind(u8),
    /// Address family byte is neither 4 nor 6.
    BadFamily(u8),
    /// HELLO carries an unsupported protocol version.
    UnsupportedVersion(u8),
    /// ERROR carries an unknown error code.
    UnknownErrorCode(u8),
    /// ECN value larger than 3.
    BadEcn(u8),
    /// ERROR message is not valid UTF-8.
    InvalidUtf8,
    /// Bytes left after a HELLO, BOUND or ERROR frame.
    TrailingBytes,
}

impl DecodeError {
    /// Returns a stable name, shared with the JavaScript codec's `err.code`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Truncated => "Truncated",
            Self::Empty => "Empty",
            Self::Oversize(_) => "Oversize",
            Self::UnknownKind(_) => "UnknownKind",
            Self::UnexpectedKind(_) => "UnexpectedKind",
            Self::BadFamily(_) => "BadFamily",
            Self::UnsupportedVersion(_) => "UnsupportedVersion",
            Self::UnknownErrorCode(_) => "UnknownErrorCode",
            Self::BadEcn(_) => "BadEcn",
            Self::InvalidUtf8 => "InvalidUtf8",
            Self::TrailingBytes => "TrailingBytes",
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "message truncated"),
            Self::Empty => write!(f, "empty message"),
            Self::Oversize(n) => {
                write!(f, "message of {n} bytes exceeds {MAX_MESSAGE_LEN} bytes")
            }
            Self::UnknownKind(k) => write!(f, "unknown kind 0x{k:02x}"),
            Self::UnexpectedKind(k) => write!(f, "unexpected kind 0x{k:02x}"),
            Self::BadFamily(v) => write!(f, "bad address family {v}"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Self::UnknownErrorCode(c) => write!(f, "unknown error code {c}"),
            Self::BadEcn(v) => write!(f, "bad ecn value {v}"),
            Self::InvalidUtf8 => write!(f, "error message is not valid utf-8"),
            Self::TrailingBytes => write!(f, "trailing bytes after frame"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Returns the kind byte of a message, or `None` if it is empty.
///
/// Use it to choose between [`decode_first`] and [`decode_data`] after
/// BOUND, when ERROR and DATA messages can both arrive.
pub fn message_kind(msg: &[u8]) -> Option<u8> {
    msg.first().copied()
}

/// Appends a HELLO frame to `out`.
pub fn encode_hello(hello: &Hello, out: &mut Vec<u8>) {
    out.push(KIND_HELLO);
    out.push(VERSION);
    out.extend_from_slice(&hello.token);
    put_addr(out, &hello.bind_addr);
}

/// Appends a BOUND frame to `out`.
pub fn encode_bound(bound: &Bound, out: &mut Vec<u8>) {
    out.push(KIND_BOUND);
    put_addr(out, &bound.local);
    out.extend_from_slice(&bound.max_gso_segments.to_be_bytes());
    out.extend_from_slice(&bound.gro_segments.to_be_bytes());
    let n = bound.interface_addrs.len().min(u8::MAX as usize);
    out.push(n as u8);
    for addr in &bound.interface_addrs[..n] {
        put_addr(out, addr);
    }
}

/// Appends an ERROR frame to `out`.
pub fn encode_error(err: &ErrorFrame<'_>, out: &mut Vec<u8>) {
    let msg = truncate_str(err.message, u16::MAX as usize);
    out.push(KIND_ERROR);
    out.push(err.code as u8);
    out.extend_from_slice(&(msg.len() as u16).to_be_bytes());
    out.extend_from_slice(msg.as_bytes());
}

/// Appends a DATA record to `out`.
///
/// Several records may be appended to one message. The caller keeps the
/// message within [`MAX_MESSAGE_LEN`], see [`Data::encoded_len`].
pub fn encode_data(data: &Data<'_>, out: &mut Vec<u8>) {
    out.reserve(data.encoded_len());
    out.push(KIND_DATA);
    out.push(data.ecn & 0b11);
    put_addr(out, &data.peer);
    out.extend_from_slice(&data.segment_size.to_be_bytes());
    out.extend_from_slice(&(data.payload.len() as u32).to_be_bytes());
    out.extend_from_slice(data.payload);
}

/// Decodes a message holding a single HELLO, BOUND or ERROR frame.
pub fn decode_first(msg: &[u8]) -> Result<Frame<'_>, DecodeError> {
    check_len(msg)?;
    let mut r = Reader(msg);
    let kind = r.u8()?;
    let frame = match kind {
        KIND_HELLO => {
            let version = r.u8()?;
            if version != VERSION {
                return Err(DecodeError::UnsupportedVersion(version));
            }
            let token = r.take(TOKEN_LEN)?.try_into().expect("length checked");
            let bind_addr = r.addr()?;
            Frame::Hello(Hello { token, bind_addr })
        }
        KIND_BOUND => {
            let local = r.addr()?;
            let max_gso_segments = r.u16()?;
            let gro_segments = r.u16()?;
            let n = r.u8()? as usize;
            let mut interface_addrs = Vec::with_capacity(n);
            for _ in 0..n {
                interface_addrs.push(r.addr()?);
            }
            Frame::Bound(Bound {
                local,
                max_gso_segments,
                gro_segments,
                interface_addrs,
            })
        }
        KIND_ERROR => {
            let code = r.u8()?;
            let code = ErrorCode::from_u8(code).ok_or(DecodeError::UnknownErrorCode(code))?;
            let len = r.u16()? as usize;
            let message =
                core::str::from_utf8(r.take(len)?).map_err(|_| DecodeError::InvalidUtf8)?;
            Frame::Error(ErrorFrame { code, message })
        }
        KIND_DATA => return Err(DecodeError::UnexpectedKind(kind)),
        _ => return Err(DecodeError::UnknownKind(kind)),
    };
    if !r.0.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(frame)
}

/// Decodes a message of DATA records.
///
/// Fails early if the message is empty or too large; per-record errors are
/// yielded by the iterator, which stops after the first error.
pub fn decode_data(msg: &[u8]) -> Result<DataIter<'_>, DecodeError> {
    check_len(msg)?;
    Ok(DataIter { rest: msg })
}

/// Iterator over the DATA records of a message, see [`decode_data`].
#[derive(Debug, Clone)]
pub struct DataIter<'a> {
    rest: &'a [u8],
}

impl<'a> DataIter<'a> {
    fn next_record(&mut self) -> Result<Data<'a>, DecodeError> {
        let mut r = Reader(self.rest);
        let kind = r.u8()?;
        match kind {
            KIND_DATA => {}
            KIND_HELLO | KIND_BOUND | KIND_ERROR => {
                return Err(DecodeError::UnexpectedKind(kind));
            }
            _ => return Err(DecodeError::UnknownKind(kind)),
        }
        let ecn = r.u8()?;
        if ecn > 3 {
            return Err(DecodeError::BadEcn(ecn));
        }
        let peer = r.addr()?;
        let segment_size = r.u16()?;
        let len = r.u32()? as usize;
        let payload = r.take(len)?;
        self.rest = r.0;
        Ok(Data {
            ecn,
            peer,
            segment_size,
            payload,
        })
    }
}

impl<'a> Iterator for DataIter<'a> {
    type Item = Result<Data<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let res = self.next_record();
        if res.is_err() {
            self.rest = &[];
        }
        Some(res)
    }
}

fn check_len(msg: &[u8]) -> Result<(), DecodeError> {
    if msg.is_empty() {
        Err(DecodeError::Empty)
    } else if msg.len() > MAX_MESSAGE_LEN {
        Err(DecodeError::Oversize(msg.len()))
    } else {
        Ok(())
    }
}

fn addr_len(addr: &SocketAddr) -> usize {
    match addr {
        SocketAddr::V4(_) => 1 + 4 + 2,
        SocketAddr::V6(_) => 1 + 16 + 2,
    }
}

fn put_addr(out: &mut Vec<u8>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("length checked"))
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn addr(&mut self) -> Result<SocketAddr, DecodeError> {
        match self.u8()? {
            4 => {
                let ip = Ipv4Addr::from(self.array::<4>()?);
                let port = self.u16()?;
                Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
            }
            6 => {
                let ip = Ipv6Addr::from(self.array::<16>()?);
                let port = self.u16()?;
                Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
            }
            other => Err(DecodeError::BadFamily(other)),
        }
    }
}

#[cfg(test)]
mod tests;
