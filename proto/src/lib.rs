//! Wire format between web pages and the host for native UDP sockets.
//!
//! One WebSocket connection on [`WS_PATH`] carries one UDP socket. All frames
//! are binary WebSocket messages, and integers are big endian.
//!
//! - The page sends [`Hello`] as the first message. It opens the socket in
//!   one of the two modes of the Direct Sockets `UDPSocket`, see [`Mode`]:
//!   bound to a local address, or connected to a remote host (a name or an
//!   IP address, resolved by the host).
//! - The host answers with [`Bound`] or [`ErrorFrame`] as the first message.
//! - After [`Bound`], both directions send messages holding one or more
//!   [`Data`] records back to back. In bound mode the page may also address
//!   a datagram to a host name with a [`NamedData`] record. The host may also
//!   send an [`ErrorFrame`] message for a failed send. The socket stays open.
//!
//! Use [`decode_first`] for messages starting with HELLO, BOUND or ERROR (see
//! [`message_kind`]), [`decode_data`] for DATA messages from the host and
//! [`decode_records`] for DATA messages from the page. Decoding borrows
//! payloads and strings from the input and does not copy them.
//!
//! The same codec exists in JavaScript, see [`JS_CODEC`] and
//! [`js_codec_classic`].
#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

/// WebSocket path on the page's own origin.
pub const WS_PATH: &str = "/.well-known/webview-sockets/udp";

/// Protocol version sent in [`Hello`].
pub const VERSION: u8 = 2;

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
/// Record kind byte of a [`NamedData`] record.
pub const KIND_DATA_NAMED: u8 = 0x11;

/// Maximum length of a host name in [`Mode::Connected`] and [`NamedData`].
pub const MAX_HOST_LEN: usize = 255;

/// Header bytes of a DATA record besides the address.
///
/// The fields are kind, ecn, segment_size and len.
const DATA_FIXED_LEN: usize = 1 + 1 + 2 + 4;

/// Header bytes of a DATA_NAMED record besides the host name.
///
/// The fields are kind, ecn, dns_query_type, host length, port,
/// segment_size and len.
const NAMED_FIXED_LEN: usize = 1 + 1 + 1 + 1 + 2 + 2 + 4;

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
pub const JS_CODEC_EXPORT_MARKER: &str = "// @@webview-sockets-codec-module-exports@@";

/// Returns the JavaScript codec without its ES module exports.
///
/// The result declares a single lexical binding `webviewSocketsCodec` holding
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
    /// The request is not allowed.
    ///
    /// The token is bad or expired, the setting is off, or the destination
    /// is not allowed.
    NotAllowed = 1,
    /// Invalid input, for example a forbidden destination address.
    InvalidInput = 2,
    /// The requested bind address is in use.
    AddrInUse = 3,
    /// A limit was exceeded.
    Limit = 4,
    /// An IO error on the host.
    Io = 5,
    /// A host name could not be resolved.
    NameNotResolved = 6,
}

impl ErrorCode {
    /// Returns the code for a wire byte, or `None` if it is unknown.
    pub fn from_u8(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::NotAllowed,
            2 => Self::InvalidInput,
            3 => Self::AddrInUse,
            4 => Self::Limit,
            5 => Self::Io,
            6 => Self::NameNotResolved,
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
            Self::NameNotResolved => "NameNotResolved",
        }
    }
}

/// The address family a host name resolves to.
///
/// It is the same as the Direct Sockets `SocketDnsQueryType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DnsQueryType {
    /// Only IPv4 addresses (A records).
    Ipv4,
    /// Only IPv6 addresses (AAAA records).
    Ipv6,
}

impl DnsQueryType {
    /// Returns the name used by Direct Sockets and the JavaScript codec.
    pub fn name(self) -> &'static str {
        match self {
            Self::Ipv4 => "ipv4",
            Self::Ipv6 => "ipv6",
        }
    }

    /// Returns whether `ip` has the family this query type asks for.
    pub fn matches(self, ip: &IpAddr) -> bool {
        match self {
            Self::Ipv4 => ip.is_ipv4(),
            Self::Ipv6 => ip.is_ipv6(),
        }
    }
}

/// How the page wants the socket opened.
///
/// These are the two modes of the Direct Sockets `UDPSocket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode<'a> {
    /// Bound mode: bind to a local address and send to any destination.
    ///
    /// Direct Sockets options `localAddress`, `localPort` and `ipv6Only`.
    Bound {
        /// Address to bind.
        ///
        /// This is `0.0.0.0:0`, `[::]:0`, or a local address and port.
        local: SocketAddr,
        /// Sets `IPV6_V6ONLY` for a `[::]` bind.
        ///
        /// `None` keeps the host default (dual stack where possible).
        ipv6_only: Option<bool>,
    },
    /// Connected mode: the host resolves `remote_host` and binds a socket.
    ///
    /// The host binds an ephemeral port and only exchanges datagrams with the
    /// resolved peer. Direct Sockets options `remoteAddress`, `remotePort` and
    /// `dnsQueryType`.
    Connected {
        /// Host name or IP address literal (IPv6 with or without brackets).
        ///
        /// At most [`MAX_HOST_LEN`] bytes are encoded.
        remote_host: &'a str,
        /// Remote port.
        remote_port: u16,
        /// Address family to resolve `remote_host` to.
        ///
        /// `None` lets the host choose.
        dns_query_type: Option<DnsQueryType>,
    },
}

/// HELLO frame, page to host, first and only record of the first message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hello<'a> {
    /// Session token from `webviewSockets.requestSession()`.
    pub token: [u8; TOKEN_LEN],
    /// Bound or connected mode.
    pub mode: Mode<'a>,
    /// Requested kernel send buffer size in bytes, 0 for the host default.
    ///
    /// Direct Sockets option `sendBufferSize`. The host may cap it.
    pub send_buffer_size: u32,
    /// Requested kernel receive buffer size in bytes, 0 for the host default.
    ///
    /// Direct Sockets option `receiveBufferSize`. The host may cap it.
    pub receive_buffer_size: u32,
}

impl<'a> Hello<'a> {
    /// Returns a bound mode HELLO with default options.
    pub fn bind(token: [u8; TOKEN_LEN], local: SocketAddr) -> Self {
        Self {
            token,
            mode: Mode::Bound {
                local,
                ipv6_only: None,
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        }
    }

    /// Returns a connected mode HELLO with default options.
    pub fn connect(token: [u8; TOKEN_LEN], remote_host: &'a str, remote_port: u16) -> Self {
        Self {
            token,
            mode: Mode::Connected {
                remote_host,
                remote_port,
                dns_query_type: None,
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        }
    }
}

/// BOUND frame, host to page, reports the opened socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// Local address of the socket.
    ///
    /// In connected mode the IP is the one the host uses to reach the peer.
    pub local: SocketAddr,
    /// The resolved peer in connected mode, `None` in bound mode.
    pub remote: Option<SocketAddr>,
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
    /// Human-readable message.
    ///
    /// At most 65535 bytes are encoded. Longer messages are cut at a char
    /// boundary.
    pub message: &'a str,
}

/// A decoded HELLO, BOUND or ERROR frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame<'a> {
    /// See [`Hello`].
    Hello(Hello<'a>),
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
            segment_size => segment_size as usize,
        };
        let empty = payload.is_empty();
        payload.chunks(size).chain(empty.then_some(payload))
    }

    /// Returns the number of bytes [`encode_data`] appends.
    pub fn encoded_len(&self) -> usize {
        DATA_FIXED_LEN + addr_len(&self.peer) + self.payload.len()
    }
}

/// DATA_NAMED record, page to host in bound mode: datagrams to a host name.
///
/// The host resolves `host` like for [`Mode::Connected`], then handles the
/// record like a [`Data`] record to the resolved address. Not allowed in
/// connected mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedData<'a> {
    /// ECN bits, 0 to 3. Encoding masks the value to 2 bits.
    pub ecn: u8,
    /// Host name or IP address literal.
    ///
    /// At most [`MAX_HOST_LEN`] bytes are encoded.
    pub host: &'a str,
    /// Destination port.
    pub port: u16,
    /// Address family to resolve `host` to, `None` lets the host choose.
    pub dns_query_type: Option<DnsQueryType>,
    /// 0 for a single datagram, else the size of each segment in `payload`.
    pub segment_size: u16,
    /// The datagram bytes.
    pub payload: &'a [u8],
}

impl NamedData<'_> {
    /// Returns the number of bytes [`encode_named_data`] appends.
    pub fn encoded_len(&self) -> usize {
        NAMED_FIXED_LEN + truncate_str(self.host, MAX_HOST_LEN).len() + self.payload.len()
    }
}

/// A record in a DATA message from the page, see [`decode_records`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    /// See [`Data`].
    Data(Data<'a>),
    /// See [`NamedData`].
    Named(NamedData<'a>),
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
    /// HELLO carries an unknown mode byte.
    UnknownMode(u8),
    /// A `dns_query_type` byte is not 0, 4 or 6.
    BadDnsQueryType(u8),
    /// A boolean or option flag byte has an invalid value.
    BadFlag(u8),
    /// ECN value larger than 3.
    BadEcn(u8),
    /// An ERROR message or a host name is not valid UTF-8.
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
            Self::UnknownMode(_) => "UnknownMode",
            Self::BadDnsQueryType(_) => "BadDnsQueryType",
            Self::BadFlag(_) => "BadFlag",
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
            Self::Oversize(len) => {
                write!(f, "message of {len} bytes exceeds {MAX_MESSAGE_LEN} bytes")
            }
            Self::UnknownKind(kind) => write!(f, "unknown kind 0x{kind:02x}"),
            Self::UnexpectedKind(kind) => write!(f, "unexpected kind 0x{kind:02x}"),
            Self::BadFamily(value) => write!(f, "bad address family {value}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported protocol version {version}")
            }
            Self::UnknownErrorCode(code) => write!(f, "unknown error code {code}"),
            Self::UnknownMode(mode) => write!(f, "unknown mode {mode}"),
            Self::BadDnsQueryType(value) => write!(f, "bad dns query type {value}"),
            Self::BadFlag(value) => write!(f, "bad flag value {value}"),
            Self::BadEcn(value) => write!(f, "bad ecn value {value}"),
            Self::InvalidUtf8 => write!(f, "string is not valid utf-8"),
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

/// Mode byte of [`Mode::Bound`].
const MODE_BOUND: u8 = 0;
/// Mode byte of [`Mode::Connected`].
const MODE_CONNECTED: u8 = 1;

/// Appends a HELLO frame to `out`.
pub fn encode_hello(hello: &Hello<'_>, out: &mut Vec<u8>) {
    out.push(KIND_HELLO);
    out.push(VERSION);
    out.extend_from_slice(&hello.token);
    out.extend_from_slice(&hello.send_buffer_size.to_be_bytes());
    out.extend_from_slice(&hello.receive_buffer_size.to_be_bytes());
    match hello.mode {
        Mode::Bound { local, ipv6_only } => {
            out.push(MODE_BOUND);
            put_addr(out, &local);
            out.push(match ipv6_only {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            });
        }
        Mode::Connected {
            remote_host,
            remote_port,
            dns_query_type,
        } => {
            out.push(MODE_CONNECTED);
            out.push(dns_byte(dns_query_type));
            put_host(out, remote_host);
            out.extend_from_slice(&remote_port.to_be_bytes());
        }
    }
}

/// Appends a BOUND frame to `out`.
pub fn encode_bound(bound: &Bound, out: &mut Vec<u8>) {
    out.push(KIND_BOUND);
    put_addr(out, &bound.local);
    match &bound.remote {
        None => out.push(0),
        Some(remote) => {
            out.push(1);
            put_addr(out, remote);
        }
    }
    out.extend_from_slice(&bound.max_gso_segments.to_be_bytes());
    out.extend_from_slice(&bound.gro_segments.to_be_bytes());
    let count = bound.interface_addrs.len().min(u8::MAX as usize);
    out.push(count as u8);
    for addr in &bound.interface_addrs[..count] {
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

/// Appends a DATA_NAMED record to `out`.
///
/// Several records may be appended to one message, mixed with DATA records.
/// The caller keeps the message within [`MAX_MESSAGE_LEN`], see
/// [`NamedData::encoded_len`].
pub fn encode_named_data(data: &NamedData<'_>, out: &mut Vec<u8>) {
    out.reserve(data.encoded_len());
    out.push(KIND_DATA_NAMED);
    out.push(data.ecn & 0b11);
    out.push(dns_byte(data.dns_query_type));
    put_host(out, data.host);
    out.extend_from_slice(&data.port.to_be_bytes());
    out.extend_from_slice(&data.segment_size.to_be_bytes());
    out.extend_from_slice(&(data.payload.len() as u32).to_be_bytes());
    out.extend_from_slice(data.payload);
}

/// Decodes a message holding a single HELLO, BOUND or ERROR frame.
pub fn decode_first(msg: &[u8]) -> Result<Frame<'_>, DecodeError> {
    check_len(msg)?;
    let mut reader = Reader(msg);
    let kind = reader.u8()?;
    let frame = match kind {
        KIND_HELLO => {
            let version = reader.u8()?;
            if version != VERSION {
                return Err(DecodeError::UnsupportedVersion(version));
            }
            let token = reader.array()?;
            let send_buffer_size = reader.u32()?;
            let receive_buffer_size = reader.u32()?;
            let mode = match reader.u8()? {
                MODE_BOUND => {
                    let local = reader.addr()?;
                    let ipv6_only = match reader.u8()? {
                        0 => None,
                        1 => Some(false),
                        2 => Some(true),
                        other => return Err(DecodeError::BadFlag(other)),
                    };
                    Mode::Bound { local, ipv6_only }
                }
                MODE_CONNECTED => {
                    let dns_query_type = reader.dns()?;
                    let remote_host = reader.host()?;
                    let remote_port = reader.u16()?;
                    Mode::Connected {
                        remote_host,
                        remote_port,
                        dns_query_type,
                    }
                }
                other => return Err(DecodeError::UnknownMode(other)),
            };
            Frame::Hello(Hello {
                token,
                mode,
                send_buffer_size,
                receive_buffer_size,
            })
        }
        KIND_BOUND => {
            let local = reader.addr()?;
            let remote = match reader.u8()? {
                0 => None,
                1 => Some(reader.addr()?),
                other => return Err(DecodeError::BadFlag(other)),
            };
            let max_gso_segments = reader.u16()?;
            let gro_segments = reader.u16()?;
            let count = reader.u8()? as usize;
            let mut interface_addrs = Vec::with_capacity(count);
            for _ in 0..count {
                interface_addrs.push(reader.addr()?);
            }
            Frame::Bound(Bound {
                local,
                remote,
                max_gso_segments,
                gro_segments,
                interface_addrs,
            })
        }
        KIND_ERROR => {
            let code = reader.u8()?;
            let code = ErrorCode::from_u8(code).ok_or(DecodeError::UnknownErrorCode(code))?;
            let len = reader.u16()? as usize;
            let message =
                core::str::from_utf8(reader.take(len)?).map_err(|_| DecodeError::InvalidUtf8)?;
            Frame::Error(ErrorFrame { code, message })
        }
        KIND_DATA | KIND_DATA_NAMED => return Err(DecodeError::UnexpectedKind(kind)),
        _ => return Err(DecodeError::UnknownKind(kind)),
    };
    if !reader.0.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(frame)
}

/// Decodes a message of DATA records, as sent by the host.
///
/// A DATA_NAMED record is an error ([`DecodeError::UnexpectedKind`]). Use
/// [`decode_records`] for messages from the page.
///
/// # Errors
///
/// Returns an error if the message is empty or too large. The iterator
/// yields errors of single records and stops after the first error.
pub fn decode_data(msg: &[u8]) -> Result<DataIter<'_>, DecodeError> {
    Ok(DataIter(decode_records(msg)?))
}

/// Decodes a message of DATA and DATA_NAMED records, as sent by the page.
///
/// # Errors
///
/// Returns an error if the message is empty or too large. The iterator
/// yields errors of single records and stops after the first error.
pub fn decode_records(msg: &[u8]) -> Result<RecordIter<'_>, DecodeError> {
    check_len(msg)?;
    Ok(RecordIter { rest: msg })
}

/// Iterator over the DATA records of a message, see [`decode_data`].
#[derive(Debug, Clone)]
pub struct DataIter<'a>(RecordIter<'a>);

impl<'a> Iterator for DataIter<'a> {
    type Item = Result<Data<'a>, DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(match self.0.next()? {
            Ok(Record::Data(data)) => Ok(data),
            Ok(Record::Named(_)) => {
                self.0.rest = &[];
                Err(DecodeError::UnexpectedKind(KIND_DATA_NAMED))
            }
            Err(err) => Err(err),
        })
    }
}

/// Iterator over the records of a message, see [`decode_records`].
#[derive(Debug, Clone)]
pub struct RecordIter<'a> {
    rest: &'a [u8],
}

impl<'a> RecordIter<'a> {
    fn next_record(&mut self) -> Result<Record<'a>, DecodeError> {
        let mut reader = Reader(self.rest);
        let kind = reader.u8()?;
        match kind {
            KIND_DATA | KIND_DATA_NAMED => {}
            KIND_HELLO | KIND_BOUND | KIND_ERROR => {
                return Err(DecodeError::UnexpectedKind(kind));
            }
            _ => return Err(DecodeError::UnknownKind(kind)),
        }
        let ecn = reader.u8()?;
        if ecn > 3 {
            return Err(DecodeError::BadEcn(ecn));
        }
        let record = if kind == KIND_DATA {
            let peer = reader.addr()?;
            let segment_size = reader.u16()?;
            let len = reader.u32()? as usize;
            Record::Data(Data {
                ecn,
                peer,
                segment_size,
                payload: reader.take(len)?,
            })
        } else {
            let dns_query_type = reader.dns()?;
            let host = reader.host()?;
            let port = reader.u16()?;
            let segment_size = reader.u16()?;
            let len = reader.u32()? as usize;
            Record::Named(NamedData {
                ecn,
                host,
                port,
                dns_query_type,
                segment_size,
                payload: reader.take(len)?,
            })
        };
        self.rest = reader.0;
        Ok(record)
    }
}

impl<'a> Iterator for RecordIter<'a> {
    type Item = Result<Record<'a>, DecodeError>;

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

fn dns_byte(dns: Option<DnsQueryType>) -> u8 {
    match dns {
        None => 0,
        Some(DnsQueryType::Ipv4) => 4,
        Some(DnsQueryType::Ipv6) => 6,
    }
}

/// Appends a host name with a u8 length prefix, cut to [`MAX_HOST_LEN`].
fn put_host(out: &mut Vec<u8>, host: &str) {
    let host = truncate_str(host, MAX_HOST_LEN);
    out.push(host.len() as u8);
    out.extend_from_slice(host.as_bytes());
}

/// Returns the longest prefix of `text` of at most `max` bytes.
fn truncate_str(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Reads big-endian fields from the front of a message.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < len {
            return Err(DecodeError::Truncated);
        }
        let (head, rest) = self.0.split_at(len);
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

    fn dns(&mut self) -> Result<Option<DnsQueryType>, DecodeError> {
        match self.u8()? {
            0 => Ok(None),
            4 => Ok(Some(DnsQueryType::Ipv4)),
            6 => Ok(Some(DnsQueryType::Ipv6)),
            other => Err(DecodeError::BadDnsQueryType(other)),
        }
    }

    fn host(&mut self) -> Result<&'a str, DecodeError> {
        let len = self.u8()? as usize;
        core::str::from_utf8(self.take(len)?).map_err(|_| DecodeError::InvalidUtf8)
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
