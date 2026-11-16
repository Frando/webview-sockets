// webview-sockets page API: native UDP sockets for webview pages.
//
// The host app injects this at document start into the pages it serves. In
// the top frame of http(s) pages on the hosts the app enables it defines
// window.webviewSockets:
//
//   webviewSockets.udp.bind({ localAddress = "0.0.0.0", localPort = 0,
//       ipv6Only?, sendBufferSize?, receiveBufferSize?, gro? })
//     -> Promise<Socket>
//   webviewSockets.udp.connect(host, port, { dnsQueryType?, sendBufferSize?,
//       receiveBufferSize?, gro? })
//     -> Promise<Socket>   (host may be a name; the app resolves it)
//   webviewSockets.requestSession() -> Promise<{ url, token }>
//   webviewSockets.UDPSocket          Direct Sockets compatibility class
//
// Socket: localAddress, localPort, remoteAddress, remotePort (connected),
// interfaceAddrs, maxGsoSegments, groSegments, closed, bufferedAmount,
// send(data, address?, port?, { segmentSize?, ecn?, dnsQueryType? }), close(),
// onmessage({ data, address, port, ecn, segmentSize? }),
// onerror({ name, code, message }) for failed sends (socket stays open),
// onclose({ code, reason, error? }).
//
// It also installs window.UDPSocket, the Direct Sockets API
// (https://wicg.github.io/direct-sockets/), if the browser has no native
// one. It is built on the socket above with Streams and has about half its
// throughput.
(() => {
  "use strict";
  // Host patterns from UdpHost::injected_js(), null for any host.
  const HOSTS = null; // @@webview-sockets-hosts@@

  if (window !== window.top) return;
  if (location.protocol !== "http:" && location.protocol !== "https:") return;
  if (HOSTS && !HOSTS.some((pattern) => hostMatches(pattern, location.host))) return;
  if (window.webviewSockets) return;

  // `*` in a pattern matches one or more characters other than "." and ":".
  function hostMatches(pattern, host) {
    const re = pattern.split("*").map((s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("[^.:]+");
    return new RegExp(`^${re}$`).test(host);
  }

  // @@webview-sockets-codec@@

  const C = webviewSocketsCodec;

  // ---- platform bridge ----

  let nextId = 1;
  const pending = new Map();
  let androidHooked = false;

  function deliver(reply) {
    if (typeof reply === "string") {
      try {
        reply = JSON.parse(reply);
      } catch {
        return;
      }
    }
    if (!reply || typeof reply !== "object") return;
    const p = pending.get(reply.id);
    if (!p) return;
    pending.delete(reply.id);
    p(reply);
  }

  // Desktop: the host calls window.__webviewSocketsReply(id, json) via evaluate_script.
  Object.defineProperty(window, "__webviewSocketsReply", {
    value: (id, json) => {
      const reply = typeof json === "string" ? JSON.parse(json) : json;
      deliver(Object.assign({}, reply, { id }));
    },
  });

  // Sends {ns: "webview-sockets", id, op} to the platform and resolves with
  // its reply {id, ok, url?, token?, error?}.
  function bridgeCall(op) {
    const id = nextId++;
    const msg = JSON.stringify({ ns: "webview-sockets", id, op });
    const android = window.webviewSocketsBridge;
    if (android && typeof android.postMessage === "function") {
      if (!androidHooked) {
        androidHooked = true;
        const onReply = (event) => deliver(event.data);
        if (typeof android.addEventListener === "function") {
          android.addEventListener("message", onReply);
        } else {
          android.onmessage = onReply;
        }
      }
      return new Promise((resolve) => {
        pending.set(id, resolve);
        android.postMessage(msg);
      });
    }
    if (window.ipc && typeof window.ipc.postMessage === "function") {
      return new Promise((resolve) => {
        pending.set(id, resolve);
        window.ipc.postMessage(msg);
      });
    }
    if (typeof window.__webviewSocketsDevSession === "function") {
      return Promise.resolve()
        .then(() => window.__webviewSocketsDevSession())
        .then((r) => Object.assign({}, r, { id }), (e) => ({ id, ok: false, error: String(e) }));
    }
    return Promise.resolve({ id, ok: false, error: "NotSupported" });
  }

  function domError(message, name) {
    return new DOMException(message, name);
  }

  async function requestSession() {
    const reply = await bridgeCall("requestSession");
    if (!reply.ok) {
      const error = reply.error || "NotAllowed";
      if (error === "NotSupported") {
        throw domError("no webview-sockets bridge on this page", "NotSupportedError");
      }
      const message = error === "Disabled" ? "sockets are turned off in the app settings"
        : `socket access not allowed (${error})`;
      throw domError(message, "NotAllowedError");
    }
    if (typeof reply.token !== "string" || !/^[0-9a-f]{64}$/.test(reply.token)) {
      throw domError("host returned a bad session token", "NotAllowedError");
    }
    const url = reply.url || `ws://${location.host}${C.WS_PATH}`;
    return { url, token: reply.token };
  }

  // One session per page, shared by all sockets.
  let session = null;

  function getSession() {
    if (!session) {
      session = requestSession();
      session.catch(() => { session = null; });
    }
    return session;
  }

  // ---- WebIDL-style option conversion ----

  function enforceRange(name, value, max) {
    const n = Number(value);
    if (!Number.isFinite(n)) throw new TypeError(`${name} must be a finite number`);
    const t = Math.trunc(n);
    if (t < 0 || t > max) throw new TypeError(`${name} is out of range: ${value}`);
    return t;
  }

  const port = (name, v) => (v === undefined ? undefined : enforceRange(name, v, 0xffff));
  const u32 = (name, v) => (v === undefined ? undefined : enforceRange(name, v, 0xffffffff));

  function dnsQueryType(v) {
    if (v === undefined) return undefined;
    const s = String(v);
    if (s !== "ipv4" && s !== "ipv6") throw new TypeError(`invalid dnsQueryType: ${s}`);
    return s;
  }

  function isMulticast(ip) {
    return ip.length === 4 ? (ip[0] & 0xf0) === 0xe0 : ip[0] === 0xff
      || (isV4Mapped(ip) && (ip[12] & 0xf0) === 0xe0);
  }

  function isV4Mapped(ip) {
    return ip.length === 16 && ip.subarray(0, 10).every((b) => b === 0) && ip[10] === 0xff && ip[11] === 0xff;
  }

  function isUnspecified(ip) {
    return ip.every((b) => b === 0);
  }

  function isBroadcast(ip) {
    const v4 = isV4Mapped(ip) ? ip.subarray(12) : ip;
    return v4.length === 4 && v4.every((b) => b === 0xff);
  }

  // Converts and checks UDPSocketOptions like the Direct Sockets constructor.
  function parseOptions(options) {
    if (options === undefined || options === null) options = {};
    if (typeof options !== "object" && typeof options !== "function") {
      throw new TypeError("options must be an object");
    }
    const o = {
      remoteAddress: options.remoteAddress === undefined ? undefined : String(options.remoteAddress),
      remotePort: port("remotePort", options.remotePort),
      localAddress: options.localAddress === undefined ? undefined : String(options.localAddress),
      localPort: port("localPort", options.localPort),
      sendBufferSize: u32("sendBufferSize", options.sendBufferSize),
      receiveBufferSize: u32("receiveBufferSize", options.receiveBufferSize),
      dnsQueryType: dnsQueryType(options.dnsQueryType),
      ipv6Only: options.ipv6Only === undefined ? undefined : Boolean(options.ipv6Only),
      gro: Boolean(options.gro),
    };
    if (options.multicastAllowAddressSharing !== undefined || options.multicastTimeToLive !== undefined
      || options.multicastLoopback !== undefined) {
      throw domError("multicast is not allowed", "NotAllowedError");
    }
    if (o.remoteAddress !== undefined) {
      const ip = C.ipLiteral(o.remoteAddress);
      if (ip && isMulticast(ip)) throw domError("multicast is not allowed", "NotAllowedError");
    }
    if ((o.remoteAddress === undefined) !== (o.remotePort === undefined)) {
      throw new TypeError("remoteAddress and remotePort must be given together");
    }
    const connected = o.remoteAddress !== undefined;
    if (o.localPort !== undefined) {
      if (o.localPort === 0) throw new TypeError("localPort must not be 0");
      if (o.localAddress === undefined) throw new TypeError("localPort needs localAddress");
    }
    if (o.localAddress !== undefined) {
      if (connected) throw new TypeError("localAddress cannot be combined with remoteAddress");
      if (!C.ipLiteral(o.localAddress)) throw new TypeError(`invalid localAddress: ${o.localAddress}`);
    }
    if (!connected && o.localAddress === undefined) {
      throw new TypeError("either remoteAddress and remotePort or localAddress is required");
    }
    if (!connected && o.dnsQueryType !== undefined) {
      throw new TypeError("dnsQueryType is only allowed with remoteAddress");
    }
    if (o.ipv6Only !== undefined) {
      if (connected) throw new TypeError("ipv6Only is only allowed with localAddress");
      const ip = C.ipLiteral(o.localAddress);
      if (ip.length !== 16 || !isUnspecified(ip)) {
        throw new TypeError("ipv6Only needs localAddress \"::\"");
      }
    }
    if (o.sendBufferSize === 0) throw new TypeError("sendBufferSize must not be 0");
    if (o.receiveBufferSize === 0) throw new TypeError("receiveBufferSize must not be 0");
    o.connected = connected;
    return o;
  }

  // ---- errors ----

  const ERROR_NAMES = {
    NotAllowed: "NotAllowedError",
    InvalidInput: "InvalidAccessError",
    AddrInUse: "NetworkError",
    Limit: "NetworkError",
    Io: "NetworkError",
    NameNotResolved: "NetworkError",
  };

  function frameError(frame) {
    const err = domError(frame.message || frame.codeName, ERROR_NAMES[frame.codeName] || "NetworkError");
    Object.defineProperty(err, "udpCode", { value: frame.codeName });
    return err;
  }

  function closeError(code, reason) {
    const K = C.CLOSE_CODES;
    const what = {
      [K.REVOKED]: "socket access was revoked",
      [K.SETTING_OFF]: "sockets were turned off in the app settings",
      [K.NAVIGATED]: "the page navigated",
      [K.LIMIT]: "a host limit was exceeded",
      [K.IO_ERROR]: "the host socket failed",
    }[code] || `connection to the host closed (${code}${reason ? ` ${reason}` : ""})`;
    const name = code === K.REVOKED || code === K.SETTING_OFF ? "NotAllowedError" : "NetworkError";
    return domError(what, name);
  }

  // ---- fast socket ----

  const EMPTY = new Uint8Array(0);

  class Socket {
    constructor(ws, frame, gro) {
      this._ws = ws;
      this._gro = gro;
      // Records waiting to be sent as one WebSocket message.
      this._buf = null;
      this._view = null;
      this._queued = 0;
      this._flushScheduled = false;
      // Last destination, so repeated sends skip parsing.
      this._lastAddress = null;
      this._lastIp = null;
      // Last source, so repeated receives skip formatting.
      this._lastFrom = new Uint8Array(16);
      this._lastFromLen = 0;
      this._lastFromText = "";
      this._closedByPage = false;
      this.localAddress = frame.localAddress;
      this.localPort = frame.localPort;
      this.remoteAddress = frame.remoteAddress;
      this.remotePort = frame.remotePort;
      this.interfaceAddrs = frame.interfaceAddrs;
      this.maxGsoSegments = frame.maxGsoSegments;
      this.groSegments = frame.groSegments;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
      ws.onmessage = (event) => this._onMessage(event.data);
      ws.onclose = (event) => this._onClose(event.code, event.reason);
    }

    get closed() {
      return this._ws.readyState >= WebSocket.CLOSING;
    }

    // Bytes not yet handed to the network, for backpressure.
    get bufferedAmount() {
      return this._ws.bufferedAmount + this._queued;
    }

    // data: Uint8Array | ArrayBuffer | ArrayBufferView | string (UTF-8).
    // address may be a host name (bound sockets). On a connected socket
    // address and port may be left out. opts.segmentSize: GSO, data holds
    // several datagrams of this size. opts.ecn: ECN bits 0-3.
    // opts.dnsQueryType: "ipv4" | "ipv6" for a host name address.
    send(data, address, port, opts) {
      if (this.closed) throw domError("socket is closed", "InvalidStateError");
      if (address === undefined || address === null) {
        if (this.remoteAddress === null) throw new TypeError("address and port are required");
        address = this.remoteAddress;
        port = this.remotePort;
      }
      const ecn = opts && opts.ecn ? opts.ecn & 3 : 0;
      const segmentSize = opts && opts.segmentSize ? opts.segmentSize : 0;
      const ip = this._destIp(address);
      if (ip && typeof data !== "string"
        && Number.isInteger(port) && port >= 0 && port <= 0xffff
        && Number.isInteger(segmentSize) && segmentSize <= 0xffff) {
        const payload = data instanceof Uint8Array ? data
          : ArrayBuffer.isView(data) ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
            : data instanceof ArrayBuffer ? new Uint8Array(data) : null;
        if (payload) {
          const at = this._reserve(11 + ip.length + payload.length);
          const buf = this._buf;
          const view = this._view;
          buf[at] = C.KIND_DATA;
          buf[at + 1] = ecn;
          buf[at + 2] = ip.length === 4 ? 4 : 6;
          buf.set(ip, at + 3);
          const p = at + 3 + ip.length;
          view.setUint16(p, port);
          view.setUint16(p + 2, segmentSize);
          view.setUint32(p + 4, payload.length);
          buf.set(payload, p + 8);
          this._scheduleFlush();
          return;
        }
      }
      // Host names, strings and anything to validate: the generic encoder.
      const bytes = C.encodeData({
        address, port, payload: data, ecn, segmentSize, dnsQueryType: opts ? opts.dnsQueryType : undefined,
      });
      const at = this._reserve(bytes.length);
      this._buf.set(bytes, at);
      this._scheduleFlush();
    }

    close() {
      this._flush();
      this._closedByPage = true;
      this._ws.close(1000);
    }

    _destIp(address) {
      if (address !== this._lastAddress) {
        this._lastIp = C.ipLiteral(address);
        this._lastAddress = address;
      }
      return this._lastIp;
    }

    // Returns the offset of `size` free bytes in the batch buffer, sending
    // the batch first if they do not fit.
    _reserve(size) {
      if (size > C.MAX_MESSAGE_LEN) {
        throw new TypeError(`datagram of ${size} bytes exceeds ${C.MAX_MESSAGE_LEN} bytes`);
      }
      if (this._queued + size > C.MAX_MESSAGE_LEN) this._flush();
      if (!this._buf) {
        this._buf = new Uint8Array(C.MAX_MESSAGE_LEN);
        this._view = new DataView(this._buf.buffer);
      }
      const at = this._queued;
      this._queued += size;
      return at;
    }

    // Sends everything queued in this task as one message, in a microtask.
    _scheduleFlush() {
      if (this._flushScheduled) return;
      this._flushScheduled = true;
      queueMicrotask(() => this._flush());
    }

    _flush() {
      this._flushScheduled = false;
      if (this._queued === 0) return;
      const total = this._queued;
      this._queued = 0;
      // send() copies the bytes, so the buffer is reused.
      if (this._ws.readyState === WebSocket.OPEN) this._ws.send(this._buf.subarray(0, total));
    }

    _onMessage(data) {
      const bytes = new Uint8Array(data);
      if (bytes[0] === C.KIND_ERROR) {
        let err;
        try {
          err = frameError(C.decodeFirst(bytes));
        } catch {
          return this._protocolError();
        }
        if (this.onerror) this.onerror({ name: err.name, code: err.udpCode, message: err.message, error: err });
        else console.warn(`webviewSockets.udp: send failed: ${err.udpCode}: ${err.message}`);
        return;
      }
      try {
        this._receive(bytes);
      } catch (err) {
        if (err instanceof ProtocolError) this._protocolError();
        else throw err;
      }
    }

    // Delivers the datagrams of a DATA message. Payloads are zero-copy views.
    _receive(bytes) {
      const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const end = bytes.length;
      const gro = this._gro;
      let pos = 0;
      // Check the whole message before delivering any of it.
      while (pos < end) {
        const family = bytes[pos + 2];
        const ipLen = family === 4 ? 4 : family === 6 ? 16 : 0;
        if (bytes[pos] !== C.KIND_DATA || !ipLen || bytes[pos + 1] > 3) throw new ProtocolError();
        const head = pos + 3 + ipLen + 8;
        if (head > end) throw new ProtocolError();
        pos = head + view.getUint32(head - 4);
        if (pos > end) throw new ProtocolError();
      }
      pos = 0;
      while (pos < end) {
        const ecn = bytes[pos + 1];
        const ipAt = pos + 3;
        const ipLen = bytes[pos + 2] === 4 ? 4 : 16;
        const p = ipAt + ipLen;
        const port = view.getUint16(p);
        const segmentSize = view.getUint16(p + 2);
        const len = view.getUint32(p + 4);
        const start = p + 8;
        pos = start + len;
        const onmessage = this.onmessage;
        if (!onmessage) continue;
        const address = this._fromText(bytes, ipAt, ipLen);
        if (gro || !segmentSize || len <= segmentSize) {
          const msg = { data: len ? bytes.subarray(start, start + len) : EMPTY, address, port, ecn };
          if (gro && segmentSize && len > segmentSize) msg.segmentSize = segmentSize;
          onmessage(msg);
        } else {
          for (let off = 0; off < len; off += segmentSize) {
            onmessage({ data: bytes.subarray(start + off, start + Math.min(off + segmentSize, len)), address, port, ecn });
          }
        }
      }
    }

    _fromText(bytes, at, len) {
      const last = this._lastFrom;
      let same = len === this._lastFromLen;
      for (let i = 0; same && i < len; i++) same = bytes[at + i] === last[i];
      if (!same) {
        const ip = bytes.subarray(at, at + len);
        last.set(ip);
        this._lastFromLen = len;
        this._lastFromText = len === 4 ? `${ip[0]}.${ip[1]}.${ip[2]}.${ip[3]}` : C.formatIpv6(ip);
      }
      return this._lastFromText;
    }

    _protocolError() {
      console.warn("webviewSockets.udp: bad message from host");
      this._ws.close(1002);
    }

    _onClose(code, reason) {
      const handler = this.onclose;
      if (!handler) return;
      const event = { code, reason };
      if (!this._closedByPage) event.error = code === 1002 ? domError("bad message from host", "NetworkError") : closeError(code, reason);
      handler(event);
    }
  }

  class ProtocolError extends Error {}

  // Opens a socket from HELLO options (codec names). Retries once with a
  // fresh session if the token went stale.
  async function open(helloOptions, gro) {
    for (let attempt = 0; ; attempt++) {
      const s = await getSession();
      const hello = C.encodeHello(s.token, helloOptions);
      const result = await handshake(s.url, hello);
      if (result.ws) return new Socket(result.ws, result.frame, gro);
      if (result.stale && attempt === 0) {
        session = null;
        continue;
      }
      throw result.error;
    }
  }

  // Opens the WebSocket, sends HELLO and waits for BOUND or ERROR.
  function handshake(url, hello) {
    return new Promise((resolve) => {
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      ws.onopen = () => ws.send(hello);
      ws.onerror = () => {};
      ws.onclose = (event) => {
        resolve({ error: domError(`connection to the host closed (${event.code})`, "NetworkError") });
      };
      ws.onmessage = (event) => {
        ws.onclose = null;
        ws.onmessage = null;
        let frame;
        try {
          frame = C.decodeFirst(new Uint8Array(event.data));
        } catch (err) {
          ws.close(1002);
          resolve({ error: domError(`bad reply from host: ${err.message}`, "NetworkError") });
          return;
        }
        if (frame.kind === "bound") {
          resolve({ ws, frame });
        } else {
          ws.close();
          resolve({ error: frameError(frame), stale: frame.codeName === "NotAllowed" });
        }
      };
    });
  }

  function bind(options = {}) {
    return open({
      localAddress: options.localAddress ?? options.address ?? "0.0.0.0",
      localPort: options.localPort ?? options.port ?? 0,
      ipv6Only: options.ipv6Only,
      sendBufferSize: options.sendBufferSize,
      receiveBufferSize: options.receiveBufferSize,
    }, Boolean(options.gro));
  }

  function connect(host, port, options = {}) {
    return open({
      remoteAddress: host,
      remotePort: port,
      dnsQueryType: options.dnsQueryType,
      sendBufferSize: options.sendBufferSize,
      receiveBufferSize: options.receiveBufferSize,
    }, Boolean(options.gro));
  }

  // ---- Direct Sockets UDPSocket, on top of the fast socket ----

  // Datagrams queued in the readable before further ones are dropped.
  const RECV_QUEUE_LEN = 4096;
  // Writes wait while more than this many bytes are unsent ...
  const SEND_HIGH = 1024 * 1024;
  // ... until the backlog is below this.
  const SEND_LOW = 256 * 1024;

  function deferred() {
    let resolve, reject;
    const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
    promise.catch(() => {});
    return { promise, resolve, reject, settled: false };
  }

  function settle(d, ok, value) {
    if (d.settled) return;
    d.settled = true;
    if (ok) d.resolve(value);
    else d.reject(value);
  }

  // State of one UDPSocket, kept out of the public object.
  class Conn {
    constructor(socket, opts) {
      this.socket = socket;
      this.opts = opts;
      this.opened = deferred();
      this.closed = deferred();
      this.sock = null;
      this.readable = null;
      this.writable = null;
      this.rc = null;
      this.wc = null;
      this.readDone = false;
      this.writeDone = false;
      this.writeError = null;
      this.error = null;
    }

    async open() {
      const o = this.opts;
      let sock;
      try {
        sock = o.connected
          ? await connect(o.remoteAddress, o.remotePort, o)
          : await bind({ ...o, localPort: o.localPort ?? 0 });
      } catch (err) {
        this.error = err.name === "UdpCodecError" ? new TypeError(err.message) : err;
        settle(this.opened, false, this.error);
        settle(this.closed, false, this.error);
        return;
      }
      this.sock = sock;
      const connected = o.connected;
      sock.onmessage = (m) => {
        const rc = this.rc;
        if (this.readDone || rc.desiredSize <= 0) return; // reader too slow: drop
        // Copy: chunks own their buffer, so a reader may transfer it.
        const msg = { data: m.data.slice() };
        if (!connected) {
          msg.remoteAddress = m.address;
          msg.remotePort = m.port;
        }
        if (m.ecn) msg.ecn = m.ecn;
        if (m.segmentSize) msg.segmentSize = m.segmentSize;
        rc.enqueue(msg);
      };
      sock.onerror = (e) => {
        const handler = this.socket.onsenderror;
        if (typeof handler === "function") handler.call(this.socket, e.error);
        else console.warn(`UDPSocket: send failed: ${e.name}: ${e.message}`);
      };
      sock.onclose = (e) => this.fail(e.error || domError("socket closed", "NetworkError"));
      this.readable = new ReadableStream({
        start: (c) => { this.rc = c; },
        cancel: () => {
          this.readDone = true;
          this.maybeClose();
        },
      }, { highWaterMark: RECV_QUEUE_LEN });
      this.writable = new WritableStream({
        start: (c) => { this.wc = c; },
        write: (chunk) => {
          try {
            return this.write(chunk);
          } catch (err) {
            // A rejected write errors the writable, like a native UDPSocket.
            this.writeError = err;
            this.writeDone = true;
            queueMicrotask(() => this.maybeClose());
            throw err;
          }
        },
        close: () => {
          this.writeDone = true;
          this.maybeClose();
        },
        abort: () => {
          this.writeDone = true;
          this.maybeClose();
        },
      });
      const info = { readable: this.readable, writable: this.writable };
      if (connected) {
        info.remoteAddress = sock.remoteAddress;
        info.remotePort = sock.remotePort;
      }
      info.localAddress = sock.localAddress;
      info.localPort = sock.localPort;
      // Non-standard.
      info.interfaceAddresses = sock.interfaceAddrs;
      info.maxGsoSegments = sock.maxGsoSegments;
      info.groSegments = sock.groSegments;
      settle(this.opened, true, info);
    }

    // Errors both streams and rejects `closed`.
    fail(err) {
      if (this.closed.settled) return;
      this.error = err;
      this.readDone = true;
      this.writeDone = true;
      try { this.rc.error(err); } catch {}
      try { this.wc.error(err); } catch {}
      if (!this.sock.closed) this.sock.close();
      settle(this.closed, false, err);
    }

    maybeClose() {
      if (!this.readDone || !this.writeDone || this.closed.settled) return;
      this.sock.onclose = null;
      this.sock.close();
      if (this.writeError) {
        this.error = this.writeError;
        settle(this.closed, false, this.writeError);
      } else {
        settle(this.closed, true, undefined);
      }
    }

    write(chunk) {
      if (this.closed.settled) return Promise.reject(this.error || domError("socket is closed", "InvalidStateError"));
      if (chunk === null || typeof chunk !== "object") throw new TypeError("chunk must be a UDPMessage");
      const data = chunk.data;
      if (!(data instanceof ArrayBuffer || ArrayBuffer.isView(data))) {
        throw new TypeError("data must be an ArrayBuffer or ArrayBufferView");
      }
      const extra = {
        ecn: chunk.ecn === undefined ? 0 : enforceRange("ecn", chunk.ecn, 3),
        segmentSize: chunk.segmentSize === undefined ? 0 : enforceRange("segmentSize", chunk.segmentSize, 0xffff),
      };
      let address;
      let port;
      if (this.opts.connected) {
        if (chunk.remoteAddress !== undefined || chunk.remotePort !== undefined || chunk.dnsQueryType !== undefined) {
          throw new TypeError("a connected socket cannot set remoteAddress, remotePort or dnsQueryType");
        }
      } else {
        if (chunk.remoteAddress === undefined || chunk.remotePort === undefined) {
          throw new TypeError("remoteAddress and remotePort are required");
        }
        address = String(chunk.remoteAddress);
        port = enforceRange("remotePort", chunk.remotePort, 0xffff);
        const ip = this.sock._destIp(address);
        if (ip && isMulticast(ip)) throw domError("multicast is not allowed", "NotAllowedError");
        if (ip && (isUnspecified(ip) || isBroadcast(ip))) {
          throw domError(`destination ${address} is not allowed`, "NotAllowedError");
        }
        if (port === 0) throw domError("destination port 0 is not allowed", "NotAllowedError");
        if (chunk.dnsQueryType !== undefined) extra.dnsQueryType = dnsQueryType(chunk.dnsQueryType);
      }
      try {
        this.sock.send(data, address, port, extra);
      } catch (err) {
        throw err.name === "InvalidStateError" ? err : new TypeError(err.message);
      }
      return this.backpressure();
    }

    backpressure() {
      if (this.sock.bufferedAmount <= SEND_HIGH) return undefined;
      return new Promise((resolve, reject) => {
        const check = () => {
          if (this.closed.settled) reject(this.error || domError("socket is closed", "InvalidStateError"));
          else if (this.sock.bufferedAmount < SEND_LOW) resolve();
          else setTimeout(check, 1);
        };
        check();
      });
    }
  }

  const conns = new WeakMap();

  function connOf(socket) {
    const conn = conns.get(socket);
    if (!conn) throw new TypeError("illegal invocation");
    return conn;
  }

  class UDPSocket {
    constructor(options = {}) {
      const opts = parseOptions(options);
      const conn = new Conn(this, opts);
      conns.set(this, conn);
      // Non-standard: failed sends after the write resolved.
      this.onsenderror = null;
      conn.open();
    }

    get opened() {
      return connOf(this).opened.promise;
    }

    get closed() {
      return connOf(this).closed.promise;
    }

    close() {
      const conn = connOf(this);
      if (!conn.opened.settled || !conn.sock) {
        return Promise.reject(domError("socket is not open", "InvalidStateError"));
      }
      if (conn.closed.settled) return conn.closed.promise;
      if (conn.readable.locked || conn.writable.locked) {
        return Promise.reject(domError("readable or writable is locked", "InvalidStateError"));
      }
      conn.readable.cancel().catch(() => {});
      conn.writable.abort().catch(() => {});
      return conn.closed.promise;
    }
  }

  Object.defineProperty(UDPSocket.prototype, Symbol.toStringTag, { value: "UDPSocket" });

  if (typeof window.UDPSocket !== "function") {
    Object.defineProperty(window, "UDPSocket", {
      value: UDPSocket, writable: true, configurable: true, enumerable: false,
    });
  }
  const udp = Object.freeze({ bind, connect });
  Object.defineProperty(window, "webviewSockets", {
    value: Object.freeze({ version: C.VERSION, udp, UDPSocket, requestSession }),
    enumerable: true,
  });
})();
