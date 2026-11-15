// iroh-link host API for web pages: native UDP sockets.
//
// Injected at document start into gateway pages by the Android app and the
// desktop browser. Defines window.irohHost only in the top frame of a gateway
// origin (http://<label>.(blake3|pkarr).localhost:<port>).
//
// window.irohHost.udp.requestSession() -> Promise<{ url, token }>
// window.irohHost.udp.bind({ address?, port? }) -> Promise<UdpSocketJs>
(() => {
  "use strict";
  if (window !== window.top) return;
  if (location.protocol !== "http:") return;
  if (!/^[a-z0-9]+\.(blake3|pkarr)\.localhost$/.test(location.hostname)) return;
  if (window.irohHost) return;

  // @@iroh-link-udp-codec@@

  const C = irohLinkUdpCodec;

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

  // Desktop: the host calls window.__irohHostReply(id, json) via evaluate_script.
  Object.defineProperty(window, "__irohHostReply", {
    value: (id, json) => {
      const reply = typeof json === "string" ? JSON.parse(json) : json;
      deliver(Object.assign({}, reply, { id }));
    },
  });

  // Sends {id, op} to the platform and resolves with its reply
  // {id, ok, url?, token?, error?}.
  function bridgeCall(op) {
    const id = nextId++;
    const msg = JSON.stringify({ id, op });
    const android = window.irohLinkBridge;
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
    if (typeof window.__irohHostDevSession === "function") {
      return Promise.resolve()
        .then(() => window.__irohHostDevSession())
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
        throw domError("no iroh-link host bridge on this page", "NotSupportedError");
      }
      const message = error === "Disabled" ? "UDP is turned off in the browser settings"
        : `UDP access not allowed (${error})`;
      throw domError(message, "NotAllowedError");
    }
    if (typeof reply.token !== "string" || !/^[0-9a-f]{64}$/.test(reply.token)) {
      throw domError("host returned a bad session token", "NotAllowedError");
    }
    const url = reply.url || `ws://${location.host}${C.WS_PATH}`;
    return { url, token: reply.token };
  }

  // ---- plain-JS sockets ----

  let session = null; // Promise<{url, token}> shared by bind() calls

  function getSession() {
    if (!session) {
      session = requestSession();
      session.catch(() => { session = null; });
    }
    return session;
  }

  const BIND_ERROR_NAMES = {
    NotAllowed: "NotAllowedError",
    InvalidInput: "InvalidAccessError",
    AddrInUse: "NetworkError",
    Limit: "QuotaExceededError",
    Io: "NetworkError",
  };

  function bindError(frame) {
    const err = domError(frame.message || frame.codeName, BIND_ERROR_NAMES[frame.codeName] || "NetworkError");
    Object.defineProperty(err, "udpCode", { value: frame.codeName });
    return err;
  }

  class UdpSocketJs {
    constructor(ws, bound) {
      this._ws = ws;
      this._queue = [];
      this._queued = 0;
      this._flushScheduled = false;
      this.localAddress = bound.local.address;
      this.localPort = bound.local.port;
      this.interfaceAddrs = bound.interfaceAddrs;
      this.maxGsoSegments = bound.maxGsoSegments;
      this.groSegments = bound.groSegments;
      this.onmessage = null;
      this.onerror = null;
      this.onclose = null;
      ws.onmessage = (event) => this._onMessage(event.data);
      ws.onclose = (event) => {
        if (this.onclose) this.onclose({ code: event.code, reason: event.reason });
      };
    }

    get closed() {
      return this._ws.readyState >= WebSocket.CLOSING;
    }

    // Bytes queued in the WebSocket, for backpressure.
    get bufferedAmount() {
      return this._ws.bufferedAmount + this._queued;
    }

    // data: Uint8Array | ArrayBuffer | ArrayBufferView | string (UTF-8).
    // opts.segmentSize: GSO, data holds several datagrams of this size.
    // opts.ecn: ECN bits 0-3.
    send(data, address, port, opts = {}) {
      if (this.closed) throw domError("socket is closed", "InvalidStateError");
      const record = { address, port, payload: data, ecn: opts.ecn ?? 0, segmentSize: opts.segmentSize ?? 0 };
      const bytes = C.encodeData(record); // validates
      if (this._queued + bytes.length > C.MAX_MESSAGE_LEN) this._flush();
      this._queue.push(bytes);
      this._queued += bytes.length;
      if (!this._flushScheduled) {
        this._flushScheduled = true;
        queueMicrotask(() => this._flush());
      }
    }

    close() {
      this._flush();
      this._ws.close(1000);
    }

    _flush() {
      this._flushScheduled = false;
      if (this._queue.length === 0) return;
      const parts = this._queue;
      const total = this._queued;
      this._queue = [];
      this._queued = 0;
      if (this._ws.readyState !== WebSocket.OPEN) return;
      if (parts.length === 1) {
        this._ws.send(parts[0]);
        return;
      }
      const msg = new Uint8Array(total);
      let pos = 0;
      for (const part of parts) {
        msg.set(part, pos);
        pos += part.length;
      }
      this._ws.send(msg);
    }

    _onMessage(data) {
      const bytes = new Uint8Array(data);
      if (C.messageKind(bytes) === C.KIND_ERROR) {
        const frame = C.decodeFirst(bytes);
        if (this.onerror) this.onerror({ code: frame.codeName, message: frame.message });
        else console.warn(`irohHost.udp: ${frame.codeName}: ${frame.message}`);
        return;
      }
      let records;
      try {
        records = C.decodeData(bytes);
      } catch (err) {
        console.warn("irohHost.udp: bad message from host", err);
        this._ws.close(1002);
        return;
      }
      if (!this.onmessage) return;
      for (const rec of records) {
        for (const segment of C.splitSegments(rec)) {
          this.onmessage({ data: segment, address: rec.address, port: rec.port, ecn: rec.ecn });
        }
      }
    }
  }

  async function bind(opts = {}) {
    const s = await getSession();
    const bindAddr = { address: opts.address ?? "0.0.0.0", port: opts.port ?? 0 };
    const hello = C.encodeHello(s.token, bindAddr);
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(s.url);
      ws.binaryType = "arraybuffer";
      ws.onopen = () => ws.send(hello);
      ws.onerror = () => {};
      ws.onclose = (event) => {
        reject(domError(`UDP connection to the host closed (${event.code})`, "NetworkError"));
      };
      ws.onmessage = (event) => {
        let frame;
        try {
          frame = C.decodeFirst(new Uint8Array(event.data));
        } catch (err) {
          ws.close(1002);
          reject(domError(`bad reply from host: ${err.message}`, "NetworkError"));
          return;
        }
        if (frame.kind === "bound") {
          resolve(new UdpSocketJs(ws, frame));
        } else {
          // The token may be stale (revoked, setting changed): ask again next time.
          if (frame.codeName === "NotAllowed") session = null;
          ws.onclose = null;
          ws.close();
          reject(bindError(frame));
        }
      };
    });
  }

  const udp = Object.freeze({ requestSession, bind });
  Object.defineProperty(window, "irohHost", {
    value: Object.freeze({ version: 1, udp }),
    enumerable: true,
  });
})();
