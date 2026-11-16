// webview-sockets page API: a Direct Sockets UDPSocket for webview pages.
//
// The host app injects this at document start into the pages it serves. In
// the top frame of http(s) pages on the hosts the app enables it defines:
//
// - window.UDPSocket: the Direct Sockets UDPSocket
//   (https://wicg.github.io/direct-sockets/), only if the browser has no
//   native one. Sockets live in the host app; each one is a WebSocket to
//   the page's own origin.
// - window.webviewSockets: { version, UDPSocket, requestSession() }. The
//   polyfill class is there even if a native UDPSocket exists.
//   requestSession() -> Promise<{ url, token }> asks the host for a session
//   (it may prompt the user); new UDPSocket() does this itself. It is for
//   code that speaks the wire protocol directly, like the Rust crate
//   webview-sockets.
//
// Non-standard extensions (absent from Direct Sockets):
// - constructor option `gro: true`: a readable chunk may hold several
//   datagrams from the same source, each `segmentSize` bytes except the last.
// - written messages may set `segmentSize` (GSO: `data` holds several
//   datagrams of that size, the last may be shorter) and `ecn` (0-3).
// - read messages carry `ecn` when it is not 0.
// - open info has `interfaceAddresses` ([{ address, port }]),
//   `maxGsoSegments` and `groSegments`.
// - `socket.onsenderror`: called with a DOMException when the host fails to
//   send a datagram after the write resolved (e.g. a host name that does not
//   resolve). The socket stays open. Without a handler it is logged.
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

  // ---- errors from the host ----

  const OPEN_ERROR_NAMES = {
    NotAllowed: "NotAllowedError",
    InvalidInput: "InvalidAccessError",
    AddrInUse: "NetworkError",
    Limit: "NetworkError",
    Io: "NetworkError",
    NameNotResolved: "NetworkError",
  };

  function frameError(frame) {
    return domError(frame.message || frame.codeName, OPEN_ERROR_NAMES[frame.codeName] || "NetworkError");
  }

  function closeError(code, reason) {
    const C2 = C.CLOSE_CODES;
    const what = {
      [C2.REVOKED]: "socket access was revoked",
      [C2.SETTING_OFF]: "sockets were turned off in the app settings",
      [C2.NAVIGATED]: "the page navigated",
      [C2.LIMIT]: "a host limit was exceeded",
      [C2.IO_ERROR]: "the host socket failed",
    }[code] || `connection to the host closed (${code}${reason ? ` ${reason}` : ""})`;
    const name = code === C2.REVOKED || code === C2.SETTING_OFF ? "NotAllowedError" : "NetworkError";
    return domError(what, name);
  }

  // ---- UDPSocket ----

  // Datagrams queued in the readable before further ones are dropped.
  const RECV_QUEUE_LEN = 4096;
  // Writes wait while more than this many bytes are unsent ...
  const SEND_HIGH = 1024 * 1024;
  // ... until the backlog is below this.
  const SEND_LOW = 256 * 1024;
  // Microtask hops without a new write before queued writes are sent as one
  // WebSocket message. WritableStream calls the sink a few microtasks apart.
  const FLUSH_IDLE_HOPS = 4;

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

  // Connection state of one socket, kept out of the public object.
  class Conn {
    constructor(socket, opts) {
      this.socket = socket;
      this.opts = opts;
      this.opened = deferred();
      this.closed = deferred();
      this.ws = null;
      this.remote = null; // { address, port } in connected mode
      // Records waiting to be sent as one WebSocket message.
      this.buf = null;
      this.view = null;
      this.queued = 0;
      this.flushing = false;
      // Last destination, so repeated sends skip parsing.
      this.lastAddress = null;
      this.lastIp = null;
      // Last source, so repeated receives skip formatting.
      this.lastFrom = new Uint8Array(16);
      this.lastFromLen = 0;
      this.lastFromText = "";
      this.writeSeq = 0;
      this.readDone = false;
      this.writeDone = false;
      this.rc = null; // ReadableStreamDefaultController
      this.wc = null; // WritableStreamDefaultController
      this.readable = null;
      this.writable = null;
      this.error = null;
      this.writeError = null;
    }

    async open() {
      for (let attempt = 0; ; attempt++) {
        let s;
        try {
          s = await getSession();
        } catch (err) {
          return this.failOpen(err);
        }
        let hello;
        try {
          hello = C.encodeHello(s.token, this.opts.connected ? {
            remoteAddress: this.opts.remoteAddress, remotePort: this.opts.remotePort,
            dnsQueryType: this.opts.dnsQueryType,
            sendBufferSize: this.opts.sendBufferSize, receiveBufferSize: this.opts.receiveBufferSize,
          } : {
            localAddress: this.opts.localAddress, localPort: this.opts.localPort ?? 0,
            ipv6Only: this.opts.ipv6Only,
            sendBufferSize: this.opts.sendBufferSize, receiveBufferSize: this.opts.receiveBufferSize,
          });
        } catch (err) {
          return this.failOpen(new TypeError(err.message));
        }
        const result = await this.connect(s.url, hello);
        if (result.ok) return this.onOpen(result.ws, result.frame);
        // A stale token (revoked, setting toggled): get a new session once.
        if (result.stale && attempt === 0) {
          session = null;
          continue;
        }
        return this.failOpen(result.error);
      }
    }

    // Opens the WebSocket and sends HELLO. Resolves with
    // { ok: true, ws, frame } or { ok: false, error, stale }.
    connect(url, hello) {
      return new Promise((resolve) => {
        const ws = new WebSocket(url);
        ws.binaryType = "arraybuffer";
        ws.onopen = () => ws.send(hello);
        ws.onerror = () => {};
        ws.onclose = (event) => {
          resolve({ ok: false, error: domError(`connection to the host closed (${event.code})`, "NetworkError") });
        };
        ws.onmessage = (event) => {
          ws.onclose = null;
          ws.onmessage = null;
          let frame;
          try {
            frame = C.decodeFirst(new Uint8Array(event.data));
          } catch (err) {
            ws.close(1002);
            resolve({ ok: false, error: domError(`bad reply from host: ${err.message}`, "NetworkError") });
            return;
          }
          if (frame.kind === "bound") {
            resolve({ ok: true, ws, frame });
          } else {
            ws.close();
            resolve({ ok: false, error: frameError(frame), stale: frame.codeName === "NotAllowed" });
          }
        };
      });
    }

    failOpen(err) {
      this.error = err;
      settle(this.opened, false, err);
      settle(this.closed, false, err);
    }

    onOpen(ws, frame) {
      this.ws = ws;
      const connected = this.opts.connected;
      if (connected) this.remote = { address: frame.remoteAddress, port: frame.remotePort };
      ws.onmessage = (event) => this.onMessage(event.data);
      ws.onclose = (event) => this.onWsClose(event);
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
      const info = {
        readable: this.readable,
        writable: this.writable,
      };
      if (connected) {
        info.remoteAddress = frame.remoteAddress;
        info.remotePort = frame.remotePort;
      }
      info.localAddress = frame.localAddress;
      info.localPort = frame.localPort;
      // Non-standard.
      info.interfaceAddresses = frame.interfaceAddrs;
      info.maxGsoSegments = frame.maxGsoSegments;
      info.groSegments = frame.groSegments;
      settle(this.opened, true, info);
    }

    onMessage(data) {
      const bytes = new Uint8Array(data);
      if (C.messageKind(bytes) === C.KIND_ERROR) {
        let frame;
        try {
          frame = C.decodeFirst(bytes);
        } catch {
          return this.protocolError();
        }
        const err = frameError(frame);
        const handler = this.socket.onsenderror;
        if (typeof handler === "function") {
          try {
            handler.call(this.socket, err);
          } catch (e) {
            console.error(e);
          }
        } else {
          console.warn(`UDPSocket: send failed: ${err.name}: ${err.message}`);
        }
        return;
      }
      if (this.readDone || !this.rc) return;
      try {
        this.receive(bytes);
      } catch {
        this.protocolError();
      }
    }

    // Enqueues the datagrams of a DATA message. Throws on a malformed one.
    receive(bytes) {
      const rc = this.rc;
      const connected = this.opts.connected;
      const gro = this.opts.gro;
      const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const end = bytes.length;
      let pos = 0;
      while (pos < end) {
        if (bytes[pos] !== C.KIND_DATA) throw new Error("unexpected record");
        const ecn = bytes[pos + 1];
        const family = bytes[pos + 2];
        const ipLen = family === 4 ? 4 : family === 6 ? 16 : 0;
        if (!ipLen || ecn > 3) throw new Error("bad record");
        const ipAt = pos + 3;
        const head = ipAt + ipLen + 8;
        if (head > end) throw new Error("truncated");
        const remotePort = view.getUint16(ipAt + ipLen);
        const segmentSize = view.getUint16(ipAt + ipLen + 2);
        const len = view.getUint32(ipAt + ipLen + 4);
        if (head + len > end) throw new Error("truncated");
        pos = head + len;
        let remoteAddress;
        if (!connected) remoteAddress = this.fromText(bytes, ipAt, ipLen);
        const step = gro || !segmentSize ? len || 1 : segmentSize;
        for (let off = 0; off < len || (off === 0 && len === 0); off += step) {
          if (rc.desiredSize <= 0) return; // reader too slow: drop, like a full socket buffer
          // Copy: chunks own their buffer, so a reader may transfer it.
          const msg = { data: bytes.slice(head + off, head + Math.min(off + step, len)) };
          if (!connected) {
            msg.remoteAddress = remoteAddress;
            msg.remotePort = remotePort;
          }
          if (ecn) msg.ecn = ecn;
          if (gro && segmentSize && len > segmentSize) msg.segmentSize = segmentSize;
          rc.enqueue(msg);
          if (len === 0) break;
        }
      }
    }

    // Formats a source address, reusing the last result.
    fromText(bytes, at, len) {
      const last = this.lastFrom;
      let same = len === this.lastFromLen;
      for (let i = 0; same && i < len; i++) same = bytes[at + i] === last[i];
      if (!same) {
        const ip = bytes.subarray(at, at + len);
        last.set(ip);
        this.lastFromLen = len;
        this.lastFromText = len === 4 ? `${ip[0]}.${ip[1]}.${ip[2]}.${ip[3]}` : C.formatIpv6(ip);
      }
      return this.lastFromText;
    }

    protocolError() {
      this.fail(domError("bad message from host", "NetworkError"), 1002);
    }

    onWsClose(event) {
      if (this.closed.settled) return;
      this.fail(closeError(event.code, event.reason));
    }

    // Errors both streams and rejects `closed`.
    fail(err, wsCode) {
      if (this.closed.settled) return;
      this.error = err;
      this.readDone = true;
      this.writeDone = true;
      try { this.rc.error(err); } catch {}
      try { this.wc.error(err); } catch {}
      this.queued = 0;
      if (wsCode && this.ws.readyState < WebSocket.CLOSING) this.ws.close(wsCode);
      settle(this.closed, false, err);
    }

    maybeClose() {
      if (!this.readDone || !this.writeDone || this.closed.settled) return;
      this.flush();
      this.ws.onclose = null;
      this.ws.onmessage = null;
      this.ws.close(1000);
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
      const record = {
        payload: data,
        ecn: chunk.ecn === undefined ? 0 : enforceRange("ecn", chunk.ecn, 3),
        segmentSize: chunk.segmentSize === undefined ? 0 : enforceRange("segmentSize", chunk.segmentSize, 0xffff),
      };
      if (this.opts.connected) {
        if (chunk.remoteAddress !== undefined || chunk.remotePort !== undefined || chunk.dnsQueryType !== undefined) {
          throw new TypeError("a connected socket cannot set remoteAddress, remotePort or dnsQueryType");
        }
        record.address = this.remote.address;
        record.port = this.remote.port;
      } else {
        if (chunk.remoteAddress === undefined || chunk.remotePort === undefined) {
          throw new TypeError("remoteAddress and remotePort are required");
        }
        record.address = String(chunk.remoteAddress);
        record.port = enforceRange("remotePort", chunk.remotePort, 0xffff);
        record.dnsQueryType = dnsQueryType(chunk.dnsQueryType);
        const ip = this.destIp(record.address);
        if (ip && isMulticast(ip)) throw domError("multicast is not allowed", "NotAllowedError");
        if (ip && (isUnspecified(ip) || isBroadcast(ip))) {
          throw domError(`destination ${record.address} is not allowed`, "NotAllowedError");
        }
        if (record.port === 0) throw domError("destination port 0 is not allowed", "NotAllowedError");
      }
      const ip = this.destIp(record.address);
      if (ip) {
        const payload = ArrayBuffer.isView(data)
          ? new Uint8Array(data.buffer, data.byteOffset, data.byteLength) : new Uint8Array(data);
        const size = 11 + ip.length + payload.length;
        const at = this.reserve(size);
        const buf = this.buf;
        const view = this.view;
        buf[at] = C.KIND_DATA;
        buf[at + 1] = record.ecn;
        buf[at + 2] = ip.length === 4 ? 4 : 6;
        buf.set(ip, at + 3);
        const p = at + 3 + ip.length;
        view.setUint16(p, record.port);
        view.setUint16(p + 2, record.segmentSize);
        view.setUint32(p + 4, payload.length);
        buf.set(payload, p + 8);
      } else {
        let bytes;
        try {
          bytes = C.encodeData(record);
        } catch (err) {
          throw new TypeError(err.message);
        }
        this.buf.set(bytes, this.reserve(bytes.length));
      }
      this.writeSeq++;
      if (!this.flushing) {
        this.flushing = true;
        this.scheduleFlush(this.writeSeq, 0);
      }
      if (this.ws.bufferedAmount + this.queued > SEND_HIGH) return this.drained();
      return undefined;
    }

    // Returns the IP bytes of a destination literal, or null for a host name.
    destIp(address) {
      if (address !== this.lastAddress) {
        this.lastIp = C.ipLiteral(address);
        this.lastAddress = address;
      }
      return this.lastIp;
    }

    // Returns the offset of `size` free bytes in the batch buffer, sending
    // the batch first if they do not fit.
    reserve(size) {
      if (size > C.MAX_MESSAGE_LEN) {
        throw new TypeError(`datagram of ${size} bytes exceeds ${C.MAX_MESSAGE_LEN} bytes`);
      }
      if (this.queued + size > C.MAX_MESSAGE_LEN) this.flush();
      if (!this.buf) {
        this.buf = new Uint8Array(C.MAX_MESSAGE_LEN);
        this.view = new DataView(this.buf.buffer);
      }
      const at = this.queued;
      this.queued += size;
      return at;
    }

    // Sends once no write arrived for FLUSH_IDLE_HOPS microtasks.
    scheduleFlush(seq, idle) {
      queueMicrotask(() => {
        if (!this.flushing) return;
        if (this.writeSeq !== seq) return this.scheduleFlush(this.writeSeq, 0);
        if (idle + 1 < FLUSH_IDLE_HOPS) return this.scheduleFlush(seq, idle + 1);
        this.flush();
      });
    }

    flush() {
      this.flushing = false;
      if (this.queued === 0) return;
      const total = this.queued;
      this.queued = 0;
      // send() copies the bytes, so the buffer is reused.
      if (this.ws.readyState === WebSocket.OPEN) this.ws.send(this.buf.subarray(0, total));
    }

    // Resolves once the WebSocket backlog drops below SEND_LOW.
    drained() {
      this.flush();
      return new Promise((resolve, reject) => {
        const check = () => {
          if (this.closed.settled) reject(this.error || domError("socket is closed", "InvalidStateError"));
          else if (this.ws.bufferedAmount < SEND_LOW) resolve();
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
      // Non-standard, see the top of this file.
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
      if (!conn.opened.settled || !conn.ws) {
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
  Object.defineProperty(window, "webviewSockets", {
    value: Object.freeze({ version: C.VERSION, UDPSocket, requestSession }),
    enumerable: true,
  });
})();
