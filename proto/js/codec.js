// Codec for the webview-sockets wire format v2.
//
// Mirrors the Rust crate webview-sockets-proto. HELLO and BOUND use the
// option and open-info names of the Direct Sockets UDPSocket (localAddress,
// remotePort, dnsQueryType, ...). DATA records and interface addresses are
// { address: string, port: number }; IPv6 addresses use the same text form as
// Rust's Ipv6Addr Display (RFC 5952, "::ffff:a.b.c.d" for IPv4-mapped).
//
// Use as an ES module:
//   import { encodeHello, decodeFirst, encodeData, decodeData } from "./codec.js";
//
// Use in a classic (injected) script: take everything above the export marker
// line near the end of this file (Rust: webview_sockets_proto::js_codec_classic())
// and paste it inside your own IIFE. It declares one binding,
// `webviewSocketsCodec`, holding all functions and constants.
//
// Errors are thrown as Error objects with name "UdpCodecError" and a `code`
// matching the Rust DecodeError::name(), e.g. "Truncated", "UnknownKind".

const webviewSocketsCodec = (() => {
  "use strict";

  const VERSION = 2;
  const MAX_MESSAGE_LEN = 256 * 1024;
  const TOKEN_LEN = 32;
  const WS_PATH = "/.well-known/webview-sockets/udp";
  const KIND_HELLO = 0x01;
  const KIND_BOUND = 0x02;
  const KIND_ERROR = 0x03;
  const KIND_DATA = 0x10;
  const KIND_DATA_NAMED = 0x11;
  const MAX_HOST_LEN = 255;
  const MODE_BOUND = 0;
  const MODE_CONNECTED = 1;
  const ERROR_CODES = {
    1: "NotAllowed", 2: "InvalidInput", 3: "AddrInUse", 4: "Limit", 5: "Io", 6: "NameNotResolved",
  };
  const DNS_TO_BYTE = { ipv4: 4, ipv6: 6 };
  const BYTE_TO_DNS = { 0: null, 4: "ipv4", 6: "ipv6" };
  const CLOSE_CODES = {
    NORMAL: 1000, REVOKED: 4001, SETTING_OFF: 4002, NAVIGATED: 4003, LIMIT: 4004, IO_ERROR: 4005,
  };

  const utf8Enc = new TextEncoder();
  const utf8Dec = new TextDecoder("utf-8", { fatal: true });

  function codecError(code, message) {
    const err = new Error(message);
    err.name = "UdpCodecError";
    err.code = code;
    return err;
  }

  function invalid(message) {
    return new TypeError(message);
  }

  // ---- addresses ----

  function parseIpv4(s) {
    const parts = s.split(".");
    if (parts.length !== 4) return null;
    const out = new Uint8Array(4);
    for (let i = 0; i < 4; i++) {
      if (!/^(0|[1-9][0-9]{0,2})$/.test(parts[i])) return null;
      const n = Number(parts[i]);
      if (n > 255) return null;
      out[i] = n;
    }
    return out;
  }

  function parseIpv6(s) {
    if (s.startsWith("[") && s.endsWith("]")) s = s.slice(1, -1);
    const pct = s.indexOf("%");
    if (pct >= 0) s = s.slice(0, pct);
    const halves = s.split("::");
    if (halves.length > 2) return null;
    const groups = (str) => {
      if (str === "") return [];
      const words = [];
      const parts = str.split(":");
      for (let i = 0; i < parts.length; i++) {
        const p = parts[i];
        if (i === parts.length - 1 && p.includes(".")) {
          const v4 = parseIpv4(p);
          if (!v4) return null;
          words.push((v4[0] << 8) | v4[1], (v4[2] << 8) | v4[3]);
        } else {
          if (!/^[0-9a-fA-F]{1,4}$/.test(p)) return null;
          words.push(parseInt(p, 16));
        }
      }
      return words;
    };
    const head = groups(halves[0]);
    const tail = halves.length === 2 ? groups(halves[1]) : [];
    if (!head || !tail) return null;
    let words;
    if (halves.length === 2) {
      const fill = 8 - head.length - tail.length;
      if (fill < 1) return null;
      words = head.concat(new Array(fill).fill(0), tail);
    } else {
      words = head;
    }
    if (words.length !== 8) return null;
    const out = new Uint8Array(16);
    words.forEach((w, i) => {
      out[2 * i] = w >> 8;
      out[2 * i + 1] = w & 0xff;
    });
    return out;
  }

  function formatIpv4(b) {
    return `${b[0]}.${b[1]}.${b[2]}.${b[3]}`;
  }

  function formatIpv6(b) {
    const words = [];
    for (let i = 0; i < 8; i++) words.push((b[2 * i] << 8) | b[2 * i + 1]);
    // IPv4-mapped, like Rust's Display.
    if (words.slice(0, 5).every((w) => w === 0) && words[5] === 0xffff) {
      return "::ffff:" + formatIpv4(b.subarray(12, 16));
    }
    // Longest run of zero words (length >= 2, first wins).
    let bestStart = -1, bestLen = 0;
    for (let i = 0; i < 8; ) {
      if (words[i] !== 0) { i++; continue; }
      let j = i;
      while (j < 8 && words[j] === 0) j++;
      if (j - i > bestLen) { bestStart = i; bestLen = j - i; }
      i = j;
    }
    const hex = (ws) => ws.map((w) => w.toString(16)).join(":");
    if (bestLen < 2) return hex(words);
    return hex(words.slice(0, bestStart)) + "::" + hex(words.slice(bestStart + bestLen));
  }

  function addrLen(addr) {
    return (addrBytes(addr).length === 4 ? 4 : 16) + 3;
  }

  function addrBytes(addr) {
    if (!addr || typeof addr.address !== "string") throw invalid("address must be a string");
    const v4 = parseIpv4(addr.address);
    if (v4) return v4;
    const v6 = parseIpv6(addr.address);
    if (v6) return v6;
    throw invalid(`invalid IP address: ${addr.address}`);
  }

  // Returns the IP bytes of a literal (IPv6 with or without brackets), or null.
  function ipLiteral(address) {
    if (typeof address !== "string") return null;
    return parseIpv4(address) ?? (address.includes(":") ? parseIpv6(address) : null);
  }

  // Host name as UTF-8, cut to MAX_HOST_LEN at a char boundary like Rust.
  function hostBytes(host) {
    if (typeof host !== "string") throw invalid("host must be a string");
    return cutUtf8(utf8Enc.encode(host), MAX_HOST_LEN);
  }

  function cutUtf8(bytes, max) {
    if (bytes.length <= max) return bytes;
    let end = max;
    while ((bytes[end] & 0xc0) === 0x80) end--;
    return bytes.subarray(0, end);
  }

  function dnsByte(dnsQueryType) {
    if (dnsQueryType === undefined || dnsQueryType === null) return 0;
    const b = DNS_TO_BYTE[dnsQueryType];
    if (!b) throw invalid(`invalid dnsQueryType: ${dnsQueryType}`);
    return b;
  }

  function checkU32(name, v) {
    if (v === undefined || v === null) return 0;
    if (!Number.isInteger(v) || v < 0 || v > 0xffffffff) throw invalid(`invalid ${name}: ${v}`);
    return v;
  }

  function checkPort(port) {
    if (!Number.isInteger(port) || port < 0 || port > 0xffff) throw invalid(`invalid port: ${port}`);
    return port;
  }

  // ---- writer / reader ----

  class Writer {
    constructor(size) {
      this.buf = new Uint8Array(size);
      this.view = new DataView(this.buf.buffer);
      this.pos = 0;
    }
    u8(v) { this.buf[this.pos++] = v; }
    u16(v) { this.view.setUint16(this.pos, v); this.pos += 2; }
    u32(v) { this.view.setUint32(this.pos, v); this.pos += 4; }
    bytes(b) { this.buf.set(b, this.pos); this.pos += b.length; }
    addr(a) {
      const ip = addrBytes(a);
      this.u8(ip.length === 4 ? 4 : 6);
      this.bytes(ip);
      this.u16(checkPort(a.port));
    }
  }

  class Reader {
    constructor(bytes) {
      this.buf = bytes;
      this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      this.pos = 0;
    }
    get remaining() { return this.buf.length - this.pos; }
    need(n) { if (this.remaining < n) throw codecError("Truncated", "message truncated"); }
    u8() { this.need(1); return this.buf[this.pos++]; }
    u16() { this.need(2); const v = this.view.getUint16(this.pos); this.pos += 2; return v; }
    u32() { this.need(4); const v = this.view.getUint32(this.pos); this.pos += 4; return v; }
    take(n) { this.need(n); const b = this.buf.subarray(this.pos, this.pos + n); this.pos += n; return b; }
    dns() {
      const b = this.u8();
      if (!(b in BYTE_TO_DNS)) throw codecError("BadDnsQueryType", `bad dns query type ${b}`);
      return BYTE_TO_DNS[b];
    }
    host() {
      const raw = this.take(this.u8());
      try {
        return utf8Dec.decode(raw);
      } catch {
        throw codecError("InvalidUtf8", "string is not valid utf-8");
      }
    }
    addr() {
      const family = this.u8();
      let address;
      if (family === 4) address = formatIpv4(this.take(4));
      else if (family === 6) address = formatIpv6(this.take(16));
      else throw codecError("BadFamily", `bad address family ${family}`);
      return { address, port: this.u16() };
    }
  }

  function toBytes(data) {
    if (data instanceof Uint8Array) return data;
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (typeof data === "string") return utf8Enc.encode(data);
    throw invalid("data must be a Uint8Array, ArrayBuffer, ArrayBufferView or string");
  }

  function checkLen(bytes) {
    if (bytes.length === 0) throw codecError("Empty", "empty message");
    if (bytes.length > MAX_MESSAGE_LEN) {
      throw codecError("Oversize", `message of ${bytes.length} bytes exceeds ${MAX_MESSAGE_LEN} bytes`);
    }
  }

  function hexToBytes(hex) {
    if (typeof hex !== "string" || !/^([0-9a-fA-F]{2})*$/.test(hex)) throw invalid("invalid hex string");
    const out = new Uint8Array(hex.length / 2);
    for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.substr(2 * i, 2), 16);
    return out;
  }

  // ---- encoders ----

  // token: Uint8Array(32) or a 64-char hex string as returned by requestSession().
  // options: Direct Sockets UDPSocketOptions. With remoteAddress the socket is
  // connected ({ remoteAddress, remotePort, dnsQueryType? }, remoteAddress may
  // be a host name), else bound ({ localAddress = "0.0.0.0", localPort = 0,
  // ipv6Only? }). sendBufferSize and receiveBufferSize work in both modes.
  // Checks only what the wire needs; the Direct Sockets option rules are up to
  // the caller.
  function encodeHello(token, options = {}) {
    const t = typeof token === "string" ? hexToBytes(token) : toBytes(token);
    if (t.length !== TOKEN_LEN) throw invalid(`token must be ${TOKEN_LEN} bytes`);
    const sendBufferSize = checkU32("sendBufferSize", options.sendBufferSize);
    const receiveBufferSize = checkU32("receiveBufferSize", options.receiveBufferSize);
    const connected = options.remoteAddress !== undefined && options.remoteAddress !== null;
    let w;
    if (connected) {
      const host = hostBytes(options.remoteAddress);
      const dns = dnsByte(options.dnsQueryType);
      w = new Writer(2 + TOKEN_LEN + 8 + 3 + host.length + 2);
      head(w);
      w.u8(MODE_CONNECTED);
      w.u8(dns);
      w.u8(host.length);
      w.bytes(host);
      w.u16(checkPort(options.remotePort));
    } else {
      const local = { address: options.localAddress ?? "0.0.0.0", port: options.localPort ?? 0 };
      const v6only = options.ipv6Only;
      if (v6only !== undefined && v6only !== null && typeof v6only !== "boolean") {
        throw invalid(`invalid ipv6Only: ${v6only}`);
      }
      w = new Writer(2 + TOKEN_LEN + 8 + 1 + addrLen(local) + 1);
      head(w);
      w.u8(MODE_BOUND);
      w.addr(local);
      w.u8(v6only === true ? 2 : v6only === false ? 1 : 0);
    }
    return w.buf;

    function head(w) {
      w.u8(KIND_HELLO);
      w.u8(VERSION);
      w.bytes(t);
      w.u32(sendBufferSize);
      w.u32(receiveBufferSize);
    }
  }

  // For tests and mock hosts. Takes the shape decodeFirst returns:
  // { localAddress, localPort, remoteAddress?, remotePort?, maxGsoSegments?,
  //   groSegments?, interfaceAddrs? }.
  function encodeBound({
    localAddress, localPort, remoteAddress = null, remotePort = null,
    maxGsoSegments = 1, groSegments = 1, interfaceAddrs = [],
  }) {
    const local = { address: localAddress, port: localPort };
    const remote = remoteAddress === null ? null : { address: remoteAddress, port: remotePort };
    const addrs = interfaceAddrs.slice(0, 255);
    const size = 1 + addrLen(local) + 1 + (remote ? addrLen(remote) : 0) + 5
      + addrs.reduce((n, a) => n + addrLen(a), 0);
    const w = new Writer(size);
    w.u8(KIND_BOUND);
    w.addr(local);
    w.u8(remote ? 1 : 0);
    if (remote) w.addr(remote);
    w.u16(maxGsoSegments);
    w.u16(groSegments);
    w.u8(addrs.length);
    addrs.forEach((a) => w.addr(a));
    return w.buf;
  }

  // For tests and mock hosts. code: number or name, e.g. "InvalidInput".
  function encodeError(code, message = "") {
    const num = typeof code === "number" ? code
      : Number(Object.keys(ERROR_CODES).find((k) => ERROR_CODES[k] === code));
    if (!ERROR_CODES[num]) throw invalid(`unknown error code ${code}`);
    // Cut at a char boundary, like the Rust encoder.
    const m = cutUtf8(utf8Enc.encode(message), 0xffff);
    const w = new Writer(4 + m.length);
    w.u8(KIND_ERROR);
    w.u8(num);
    w.u16(m.length);
    w.bytes(m);
    return w.buf;
  }

  // records: one record or an array of
  //   { address, port, payload, ecn?, segmentSize?, dnsQueryType? }.
  // payload: Uint8Array | ArrayBuffer | ArrayBufferView | string (UTF-8).
  // An address that is not an IP literal is a host name: the record becomes a
  // DATA_NAMED record (page to host, bound mode only) and dnsQueryType picks
  // the address family. Returns one message; throws if it would exceed
  // MAX_MESSAGE_LEN.
  function encodeData(records) {
    const list = Array.isArray(records) ? records : [records];
    const prepared = list.map((r) => {
      const payload = toBytes(r.payload);
      const ip = ipLiteral(r.address);
      if (!ip && typeof r.address === "string" && r.address.includes(":")) {
        throw invalid(`invalid IP address: ${r.address}`);
      }
      if (ip) return { r, payload, ip, size: 8 + ip.length + 3 + payload.length };
      const host = hostBytes(r.address);
      if (host.length === 0) throw invalid("address must not be empty");
      return { r, payload, host, size: 12 + host.length + payload.length };
    });
    const size = prepared.reduce((n, p) => n + p.size, 0);
    if (size > MAX_MESSAGE_LEN) {
      throw codecError("Oversize", `message of ${size} bytes exceeds ${MAX_MESSAGE_LEN} bytes`);
    }
    const w = new Writer(size);
    for (const { r, payload, ip, host } of prepared) {
      const seg = r.segmentSize ?? 0;
      if (!Number.isInteger(seg) || seg < 0 || seg > 0xffff) throw invalid(`invalid segmentSize: ${seg}`);
      const port = checkPort(r.port);
      if (ip) {
        w.u8(KIND_DATA);
        w.u8((r.ecn ?? 0) & 3);
        w.u8(ip.length === 4 ? 4 : 6);
        w.bytes(ip);
        w.u16(port);
      } else {
        w.u8(KIND_DATA_NAMED);
        w.u8((r.ecn ?? 0) & 3);
        w.u8(dnsByte(r.dnsQueryType));
        w.u8(host.length);
        w.bytes(host);
        w.u16(port);
      }
      w.u16(seg);
      w.u32(payload.length);
      w.bytes(payload);
    }
    return w.buf;
  }

  // ---- decoders ----

  // Returns the kind byte of a message, or undefined if empty.
  function messageKind(bytes) {
    return toBytes(bytes)[0];
  }

  // Decodes a message holding a single HELLO, BOUND or ERROR frame. Returns
  //   { kind: "hello", version, token: Uint8Array, sendBufferSize,
  //     receiveBufferSize, mode: "bound", localAddress, localPort, ipv6Only }
  //   { kind: "hello", ..., mode: "connected", remoteAddress, remotePort,
  //     dnsQueryType }
  //   { kind: "bound", localAddress, localPort, remoteAddress, remotePort,
  //     maxGsoSegments, groSegments, interfaceAddrs: [{ address, port }] }
  //   { kind: "error", code: number, codeName: string, message }
  // Absent optional values (ipv6Only, dnsQueryType, remote*) are null.
  function decodeFirst(bytes) {
    bytes = toBytes(bytes);
    checkLen(bytes);
    const r = new Reader(bytes);
    const kind = r.u8();
    let frame;
    if (kind === KIND_HELLO) {
      const version = r.u8();
      if (version !== VERSION) throw codecError("UnsupportedVersion", `unsupported protocol version ${version}`);
      const token = r.take(TOKEN_LEN);
      const sendBufferSize = r.u32();
      const receiveBufferSize = r.u32();
      frame = { kind: "hello", version, token, sendBufferSize, receiveBufferSize };
      const mode = r.u8();
      if (mode === MODE_BOUND) {
        const local = r.addr();
        const flag = r.u8();
        if (flag > 2) throw codecError("BadFlag", `bad flag value ${flag}`);
        Object.assign(frame, {
          mode: "bound", localAddress: local.address, localPort: local.port,
          ipv6Only: flag === 0 ? null : flag === 2,
        });
      } else if (mode === MODE_CONNECTED) {
        const dnsQueryType = r.dns();
        const remoteAddress = r.host();
        Object.assign(frame, { mode: "connected", remoteAddress, remotePort: r.u16(), dnsQueryType });
      } else {
        throw codecError("UnknownMode", `unknown mode ${mode}`);
      }
    } else if (kind === KIND_BOUND) {
      const local = r.addr();
      const flag = r.u8();
      if (flag > 1) throw codecError("BadFlag", `bad flag value ${flag}`);
      const remote = flag === 1 ? r.addr() : null;
      const maxGsoSegments = r.u16();
      const groSegments = r.u16();
      const n = r.u8();
      const interfaceAddrs = [];
      for (let i = 0; i < n; i++) interfaceAddrs.push(r.addr());
      frame = {
        kind: "bound", localAddress: local.address, localPort: local.port,
        remoteAddress: remote ? remote.address : null, remotePort: remote ? remote.port : null,
        maxGsoSegments, groSegments, interfaceAddrs,
      };
    } else if (kind === KIND_ERROR) {
      const code = r.u8();
      if (!ERROR_CODES[code]) throw codecError("UnknownErrorCode", `unknown error code ${code}`);
      const raw = r.take(r.u16());
      let message;
      try {
        message = utf8Dec.decode(raw);
      } catch {
        throw codecError("InvalidUtf8", "string is not valid utf-8");
      }
      frame = { kind: "error", code, codeName: ERROR_CODES[code], message };
    } else if (kind === KIND_DATA || kind === KIND_DATA_NAMED) {
      throw codecError("UnexpectedKind", `unexpected kind 0x${kind.toString(16)}`);
    } else {
      throw codecError("UnknownKind", `unknown kind 0x${kind.toString(16)}`);
    }
    if (r.remaining !== 0) throw codecError("TrailingBytes", "trailing bytes after frame");
    return frame;
  }

  // Decodes a message of DATA and DATA_NAMED records. Returns an array of
  //   { ecn, address, port, segmentSize, payload }
  // where payload is a zero-copy subarray of the input. DATA_NAMED records
  // (only sent by pages) carry a host name in `address` and an extra
  // `dnsQueryType` (null, "ipv4" or "ipv6"). Throws on any malformed record.
  function decodeData(bytes) {
    bytes = toBytes(bytes);
    checkLen(bytes);
    const r = new Reader(bytes);
    const out = [];
    while (r.remaining > 0) {
      const kind = r.u8();
      if (kind !== KIND_DATA && kind !== KIND_DATA_NAMED) {
        if (kind === KIND_HELLO || kind === KIND_BOUND || kind === KIND_ERROR) {
          throw codecError("UnexpectedKind", `unexpected kind 0x${kind.toString(16)}`);
        }
        throw codecError("UnknownKind", `unknown kind 0x${kind.toString(16)}`);
      }
      const ecn = r.u8();
      if (ecn > 3) throw codecError("BadEcn", `bad ecn value ${ecn}`);
      if (kind === KIND_DATA) {
        const { address, port } = r.addr();
        const segmentSize = r.u16();
        const payload = r.take(r.u32());
        out.push({ ecn, address, port, segmentSize, payload });
      } else {
        const dnsQueryType = r.dns();
        const address = r.host();
        const port = r.u16();
        const segmentSize = r.u16();
        const payload = r.take(r.u32());
        out.push({ ecn, address, port, dnsQueryType, segmentSize, payload });
      }
    }
    return out;
  }

  // Splits a decoded DATA record into datagrams (GRO). Zero copy.
  function splitSegments(record) {
    const { payload, segmentSize } = record;
    if (!segmentSize || payload.length === 0) return [payload];
    const out = [];
    for (let i = 0; i < payload.length; i += segmentSize) {
      out.push(payload.subarray(i, Math.min(i + segmentSize, payload.length)));
    }
    return out;
  }

  return {
    VERSION, MAX_MESSAGE_LEN, TOKEN_LEN, WS_PATH,
    KIND_HELLO, KIND_BOUND, KIND_ERROR, KIND_DATA, KIND_DATA_NAMED, MAX_HOST_LEN,
    ERROR_CODES, CLOSE_CODES,
    encodeHello, encodeBound, encodeError, encodeData,
    messageKind, decodeFirst, decodeData, splitSegments,
    formatIpv6, parseIpv6, ipLiteral,
  };
})();
// @@webview-sockets-codec-module-exports@@
export const {
  VERSION, MAX_MESSAGE_LEN, TOKEN_LEN, WS_PATH,
  KIND_HELLO, KIND_BOUND, KIND_ERROR, KIND_DATA, KIND_DATA_NAMED, MAX_HOST_LEN,
  ERROR_CODES, CLOSE_CODES,
  encodeHello, encodeBound, encodeError, encodeData,
  messageKind, decodeFirst, decodeData, splitSegments,
  formatIpv6, parseIpv6, ipLiteral,
} = webviewSocketsCodec;
export default webviewSocketsCodec;
