// Plain-JS native UDP demo for webview pages.
//
// Uses window.webviewSockets.udp (bind, connect). With ?api=udpsocket it
// runs on the Direct Sockets UDPSocket instead (the compatibility layer, or
// the browser's own). Outside a webview-sockets host app, it loads the
// test-host dev script from /.well-known/webview-sockets/polyfill.js.
//
// Query params:
//   ?peer=ip:port&send=text&expect=text  send, wait for the reply, report
//   ?connect=host:port&send=text&expect=text[&dnsQueryType=ipv4]
//                                        same with a connected socket; host may be a name
//   ?api=udpsocket                       use new UDPSocket(...) for everything
//   ?bench=ip:port[&n=2000&count=20000&window=64]  RTT + throughput vs an echo server
//   ?echo=1  start in echo mode
// Results: console line and document.title "UDP-DEMO PASS|FAIL <detail>".
"use strict";

(() => {
  const HOST_JS = "/.well-known/webview-sockets/polyfill.js";
  const BENCH_SIZE = 1200;
  const MAX_LOG_LINES = 500;

  const $ = (id) => document.getElementById(id);
  const params = new URLSearchParams(location.search);
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  let sock = null;
  // Called for each received datagram before the default handler; return
  // true to consume it (used by automation and bench).
  let tap = null;

  // ---- UI helpers ----

  function log(line) {
    const el = $("log");
    const lines = el.textContent.split("\n");
    if (lines.length > MAX_LOG_LINES) el.textContent = lines.slice(-MAX_LOG_LINES).join("\n");
    el.textContent += `${new Date().toLocaleTimeString()} ${line}\n`;
    el.scrollTop = el.scrollHeight;
  }

  function setStatus(text, kind) {
    const el = $("status");
    el.textContent = text;
    el.className = `status ${kind}`;
  }

  function result(ok, detail) {
    const line = `UDP-DEMO ${ok ? "PASS" : "FAIL"} ${detail}`;
    document.title = line;
    console.log(line);
    log(line);
  }

  function fmtAddr(address, port) {
    return address.includes(":") ? `[${address}]:${port}` : `${address}:${port}`;
  }

  const useUdpSocket = params.get("api") === "udpsocket";

  // "name:5", "1.2.3.4:5" or "[::1]:5" -> { address, port }.
  function parseHostPort(text) {
    const m = /^\s*(?:\[([0-9a-fA-F:.]+)\]|([^\s:\[\]]+)):(\d{1,5})\s*$/.exec(text);
    if (!m) throw new Error(`expected host:port, got "${text}"`);
    const port = Number(m[3]);
    if (port < 1 || port > 65535) throw new Error(`bad port ${port}`);
    return { address: m[1] || m[2], port };
  }

  // "1.2.3.4:5" or "[::1]:5" -> { address, port }.
  function parsePeer(text) {
    const m = /^\s*(?:\[([0-9a-fA-F:.]+)\]|([0-9.]+)):(\d{1,5})\s*$/.exec(text);
    if (!m) throw new Error(`peer must be ip:port or [ipv6]:port, got "${text}"`);
    const port = Number(m[3]);
    if (port < 1 || port > 65535) throw new Error(`bad port ${port}`);
    return { address: m[1] || m[2], port };
  }

  function printable(data) {
    const text = dec.decode(data);
    // eslint-disable-next-line no-control-regex
    if (!text.includes("\uFFFD") && !/[\x00-\x08\x0e-\x1f]/.test(text)) {
      return JSON.stringify(text.length > 200 ? `${text.slice(0, 200)}...` : text);
    }
    const hex = Array.from(data.subarray(0, 32), (b) => b.toString(16).padStart(2, "0")).join("");
    return `hex ${hex}${data.length > 32 ? "..." : ""}`;
  }

  function addrRow(label, value) {
    const li = document.createElement("li");
    const l = document.createElement("span");
    l.className = "label";
    l.textContent = label;
    const v = document.createElement("code");
    v.textContent = value;
    const copy = document.createElement("button");
    copy.className = "small";
    copy.textContent = "copy";
    copy.onclick = () => navigator.clipboard?.writeText(value).catch(() => {});
    li.append(l, v, copy);
    $("addrs").append(li);
  }

  // ---- setup ----

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src;
      s.onload = resolve;
      s.onerror = () => reject(new Error(`could not load ${src}`));
      document.head.append(s);
    });
  }

  // Returns window.webviewSockets or throws with the reason it is unavailable.
  async function getHost() {
    if (!window.webviewSockets) {
      try {
        await loadScript(HOST_JS);
      } catch {
        // Not served by the test host either; reported below.
      }
    }
    if (!window.webviewSockets) {
      throw new Error("not running in a webview-sockets host app (window.webviewSockets is missing)");
    }
    if (!window.webviewSockets.udp || typeof window.webviewSockets.udp.connect !== "function") {
      throw new Error(`host API version ${window.webviewSockets.version} has no udp.connect()`);
    }
    return window.webviewSockets;
  }

  // Wraps a Direct Sockets UDPSocket in the shape of a webviewSockets.udp
  // socket, so the rest of the demo runs on either API.
  async function fromUdpSocket(options) {
    const socket = new UDPSocket(options);
    const info = await socket.opened;
    const writer = info.writable.getWriter();
    const reader = info.readable.getReader();
    const s = {
      localAddress: info.localAddress,
      localPort: info.localPort,
      remoteAddress: info.remoteAddress ?? null,
      remotePort: info.remotePort ?? null,
      interfaceAddrs: info.interfaceAddresses ?? [],
      maxGsoSegments: info.maxGsoSegments,
      groSegments: info.groSegments,
      closed: false,
      onmessage: null,
      onerror: null,
      onclose: null,
      send(data, address, port) {
        const msg = { data: typeof data === "string" ? enc.encode(data) : data };
        if (address !== undefined) Object.assign(msg, { remoteAddress: address, remotePort: port });
        writer.write(msg).catch((err) => s.onerror?.({ name: err.name, code: err.name, message: err.message }));
      },
      close() {
        writer.releaseLock();
        reader.releaseLock();
        socket.close().catch(() => {});
      },
    };
    socket.onsenderror = (err) => s.onerror?.({ name: err.name, code: err.name, message: err.message });
    socket.closed.then(() => ({ code: 1000, reason: "" }), (err) => ({ code: 0, reason: `${err.name}: ${err.message}` }))
      .then((ev) => {
        s.closed = true;
        s.onclose?.(ev);
      });
    (async () => {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        s.onmessage?.({ data: value.data, address: value.remoteAddress, port: value.remotePort, ecn: value.ecn ?? 0 });
      }
    })().catch(() => {});
    return s;
  }

  function bind(host, localAddress) {
    return useUdpSocket ? fromUdpSocket({ localAddress }) : host.udp.bind({ localAddress });
  }

  function connect(host, address, port, dnsQueryType) {
    if (useUdpSocket) {
      return fromUdpSocket({ remoteAddress: address, remotePort: port, ...(dnsQueryType ? { dnsQueryType } : {}) });
    }
    return host.udp.connect(address, port, { dnsQueryType: dnsQueryType || undefined });
  }

  // Binds dual-stack [::], falling back to 0.0.0.0.
  async function bindSocket(host) {
    try {
      return await bind(host, "::");
    } catch (err) {
      if (err.name === "NotAllowedError" || err.name === "NotSupportedError") throw err;
      log(`bind [::] failed (${err.name}: ${err.message}), trying 0.0.0.0`);
      return await bind(host, "0.0.0.0");
    }
  }

  function onDatagram(msg) {
    if (tap && tap(msg)) return;
    const from = fmtAddr(msg.address, msg.port);
    log(`<- ${from} ${msg.data.length} bytes: ${printable(msg.data)}`);
    if ($("echo").checked) {
      sock.send(msg.data, msg.address, msg.port);
      log(`-> ${from} echo ${msg.data.length} bytes`);
    }
  }

  function sendText(peerText, text) {
    const peer = parsePeer(peerText);
    sock.send(enc.encode(text), peer.address, peer.port);
    log(`-> ${fmtAddr(peer.address, peer.port)} ${JSON.stringify(text)}`);
    return peer;
  }

  // ---- automation: ?peer=&send=&expect= ----

  async function runSendExpect(peerText, text, expect) {
    let peer;
    try {
      peer = parsePeer(peerText);
    } catch (err) {
      result(false, err.message);
      return;
    }
    const want = fmtAddr(peer.address, peer.port);
    const reply = new Promise((resolve) => {
      tap = (msg) => {
        if (msg.port !== peer.port) return false;
        resolve(msg);
        return false; // still log it
      };
    });
    const start = performance.now();
    // UDP may drop: resend once per second, up to 5 times.
    let timer;
    const resend = (left) => {
      if (sock.closed) return;
      sendText(peerText, text);
      if (left > 1) timer = setTimeout(() => resend(left - 1), 1000);
    };
    resend(5);
    const timeout = new Promise((resolve) => setTimeout(() => resolve(null), 6000));
    const msg = await Promise.race([reply, timeout]);
    clearTimeout(timer);
    tap = null;
    if (!msg) {
      result(false, `no reply from ${want} within 6 s`);
      return;
    }
    const got = dec.decode(msg.data);
    const ms = (performance.now() - start).toFixed(2);
    if (expect !== null && got !== expect) {
      result(false, `expected ${JSON.stringify(expect)} from ${want}, got ${JSON.stringify(got)}`);
    } else {
      result(true, `reply ${JSON.stringify(got)} from ${fmtAddr(msg.address, msg.port)} in ${ms} ms`);
    }
  }

  // ---- connected socket: ?connect=host:port&send=&expect= ----

  // Opens a socket connected to host:port (the app resolves a name), sends
  // `text` until a reply arrives and reports it.
  async function runConnected(host, target, text, expect) {
    let c;
    try {
      const peer = parseHostPort(target);
      c = await connect(host, peer.address, peer.port, params.get("dnsQueryType"));
      log(`connected ${fmtAddr(c.localAddress, c.localPort)} -> ${fmtAddr(c.remoteAddress, c.remotePort)} (${peer.address})`);
      let failed;
      const error = new Promise((resolve) => { failed = resolve; });
      c.onerror = ({ code, message }) => failed(`${code}: ${message}`);
      const reply = new Promise((resolve) => {
        c.onmessage = (msg) => {
          log(`<- (connected) ${msg.data.length} bytes: ${printable(msg.data)}`);
          resolve(dec.decode(msg.data));
        };
      });
      const start = performance.now();
      let timer;
      const resend = (left) => {
        if (c.closed) return;
        c.send(enc.encode(text));
        log(`-> (connected) ${JSON.stringify(text)}`);
        if (left > 1) timer = setTimeout(() => resend(left - 1), 1000);
      };
      resend(5);
      const timeout = new Promise((resolve) => setTimeout(() => resolve(null), 6000));
      const got = await Promise.race([reply, error.then((e) => ({ error: e })), timeout]);
      clearTimeout(timer);
      const where = `${fmtAddr(c.remoteAddress, c.remotePort)} (connected to ${peer.address})`;
      if (got === null) result(false, `no reply from ${where} within 6 s`);
      else if (got.error) result(false, `send error: ${got.error}`);
      else if (expect !== null && got !== expect) result(false, `expected ${JSON.stringify(expect)} from ${where}, got ${JSON.stringify(got)}`);
      else result(true, `reply ${JSON.stringify(got)} from ${where} in ${(performance.now() - start).toFixed(2)} ms`);
    } catch (err) {
      result(false, `connected socket to ${target}: ${err.name}: ${err.message}`);
    } finally {
      c?.close();
    }
  }

  // ---- bench: ?bench=ip:port ----

  function percentile(sorted, p) {
    if (sorted.length === 0) return NaN;
    return sorted[Math.min(sorted.length - 1, Math.floor((sorted.length * p) / 100))];
  }

  // Sequential 1200-byte ping-pong; returns RTTs in ms.
  async function benchRtt(peer, n, warmup) {
    const payload = new Uint8Array(BENCH_SIZE);
    const view = new DataView(payload.buffer);
    const rtts = [];
    let lost = 0;
    let waiting = null; // { seq, resolve }
    tap = (msg) => {
      if (msg.data.length >= 4 && waiting) {
        const seq = new DataView(msg.data.buffer, msg.data.byteOffset).getUint32(0);
        if (seq === waiting.seq) waiting.resolve(true);
      }
      return true;
    };
    for (let i = 0; i < warmup + n; i++) {
      view.setUint32(0, i);
      const got = new Promise((resolve) => {
        const t = setTimeout(() => resolve(false), 1000);
        waiting = { seq: i, resolve: (v) => { clearTimeout(t); resolve(v); } };
      });
      const start = performance.now();
      sock.send(payload, peer.address, peer.port);
      const ok = await got;
      const rtt = performance.now() - start;
      if (i < warmup) continue;
      if (ok) rtts.push(rtt);
      else lost++;
    }
    tap = null;
    return { rtts, lost };
  }

  // Keeps `win` datagrams in flight until `count` replies arrived or were
  // declared lost (no reply for 300 ms refills the window).
  async function benchThroughput(peer, count, win) {
    const payload = new Uint8Array(BENCH_SIZE);
    const view = new DataView(payload.buffer);
    let sent = 0;
    let received = 0;
    let lost = 0;
    let lastProgress = performance.now();
    const sendOne = () => {
      view.setUint32(0, sent++);
      sock.send(payload, peer.address, peer.port);
    };
    const fill = () => {
      while (sent < count && sent - received - lost < win) sendOne();
    };
    return new Promise((resolve) => {
      const start = performance.now();
      const finish = () => {
        clearInterval(stall);
        tap = null;
        resolve({ received, lost, secs: (performance.now() - start) / 1000 });
      };
      tap = () => {
        received++;
        lastProgress = performance.now();
        if (received + lost >= count) finish();
        else fill();
        return true;
      };
      const stall = setInterval(() => {
        if (sock.closed) {
          lost = count - received;
          finish();
          return;
        }
        if (performance.now() - lastProgress < 300) return;
        lost += sent - received - lost;
        lastProgress = performance.now();
        if (received + lost >= count) finish();
        else fill();
      }, 100);
      fill();
    });
  }

  async function runBench(peerText) {
    let peer;
    try {
      peer = parsePeer(peerText);
    } catch (err) {
      result(false, `bench: ${err.message}`);
      return;
    }
    const n = Number(params.get("n") || 2000);
    const count = Number(params.get("count") || 20000);
    const win = Number(params.get("window") || 64);
    $("send").disabled = true;
    log(`bench: ${n} x ${BENCH_SIZE} B ping-pong to ${fmtAddr(peer.address, peer.port)}`);
    const { rtts, lost } = await benchRtt(peer, n, 50);
    const sorted = rtts.slice().sort((a, b) => a - b);
    const mean = rtts.reduce((a, b) => a + b, 0) / (rtts.length || 1);
    const rttLine = `rtt p50=${percentile(sorted, 50).toFixed(3)}ms p99=${percentile(sorted, 99).toFixed(3)}ms `
      + `mean=${mean.toFixed(3)}ms min=${(sorted[0] ?? NaN).toFixed(3)}ms lost=${lost}/${n}`;
    log(`bench: ${rttLine}`);

    log(`bench: throughput ${count} x ${BENCH_SIZE} B, window ${win}`);
    const t = await benchThroughput(peer, count, win);
    const pps = t.received / t.secs;
    const mbit = (pps * BENCH_SIZE * 8) / 1e6;
    const tputLine = `tput ${Math.round(pps)} pkt/s each way ${mbit.toFixed(1)} Mbit/s each way `
      + `lost=${t.lost}/${count} time=${t.secs.toFixed(2)}s window=${win}`;
    log(`bench: ${tputLine}`);
    $("send").disabled = false;
    const ok = rtts.length > 0 && t.received > 0;
    result(ok, `bench ${rttLine} ${tputLine}`);
  }

  // ---- main ----

  async function main() {
    $("echo").checked = params.get("echo") === "1";
    if (params.get("peer")) $("peer").value = params.get("peer");
    if (params.get("send")) $("text").value = params.get("send");
    if (params.get("connect")) $("target").value = params.get("connect");

    let host;
    try {
      host = await getHost();
      sock = await bindSocket(host);
    } catch (err) {
      const reason = err.name && err.name !== "Error" ? `${err.name}: ${err.message}` : err.message;
      setStatus(`Native UDP: not available (${reason})`, "bad");
      log(`not available: ${reason}`);
      if (params.has("peer") || params.has("bench") || params.has("connect")) result(false, `not available: ${reason}`);
      return;
    }

    setStatus(`Native UDP: available (${useUdpSocket ? "UDPSocket" : "webviewSockets.udp"})`, "ok");
    addrRow("bound", fmtAddr(sock.localAddress, sock.localPort));
    for (const a of sock.interfaceAddrs) addrRow("interface", fmtAddr(a.address, a.port));
    log(`bound ${fmtAddr(sock.localAddress, sock.localPort)}, gso=${sock.maxGsoSegments} gro=${sock.groSegments}`);

    sock.onmessage = onDatagram;
    sock.onerror = ({ code, message }) => log(`error ${code}: ${message}`);
    sock.onclose = ({ code, reason }) => {
      setStatus(`Native UDP: socket closed (${code}${reason ? ` ${reason}` : ""})`, "bad");
      log(`socket closed: ${code} ${reason}`);
      $("send").disabled = true;
    };

    $("send").disabled = false;
    $("send-form").onsubmit = (event) => {
      event.preventDefault();
      try {
        sendText($("peer").value, $("text").value);
      } catch (err) {
        log(`send failed: ${err.message}`);
      }
    };

    $("connect-form").onsubmit = (event) => {
      event.preventDefault();
      runConnected(host, $("target").value, $("ctext").value, null);
    };
    $("connect").disabled = false;

    if (params.get("bench")) {
      await runBench(params.get("bench"));
    } else if (params.get("connect")) {
      await runConnected(host, params.get("connect"), params.get("send") ?? "hello", params.get("expect"));
    } else if (params.get("peer") && params.has("send")) {
      await runSendExpect(params.get("peer"), params.get("send"), params.get("expect"));
    }
  }

  main().catch((err) => {
    setStatus(`error: ${err.message}`, "bad");
    result(false, `error: ${err.message}`);
  });
})();
