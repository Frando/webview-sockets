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
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  let sock = null;
  // Called for each received datagram before the default handler; return
  // true to consume it (used by automation and bench).
  let tap = null;

  // ---- UI helpers ----

  function log(line) {
    const element = $("log");
    const lines = element.textContent.split("\n");
    if (lines.length > MAX_LOG_LINES) element.textContent = lines.slice(-MAX_LOG_LINES).join("\n");
    element.textContent += `${new Date().toLocaleTimeString()} ${line}\n`;
    element.scrollTop = element.scrollHeight;
  }

  function setStatus(text, kind) {
    const element = $("status");
    element.textContent = text;
    element.className = `status ${kind}`;
  }

  function result(ok, detail) {
    const line = `UDP-DEMO ${ok ? "PASS" : "FAIL"} ${detail}`;
    document.title = line;
    console.log(line);
    log(line);
  }

  function formatAddr(address, port) {
    return address.includes(":") ? `[${address}]:${port}` : `${address}:${port}`;
  }

  const useUdpSocket = params.get("api") === "udpsocket";

  // "name:5", "1.2.3.4:5" or "[::1]:5" -> { address, port }.
  function parseHostPort(text) {
    const match = /^\s*(?:\[([0-9a-fA-F:.]+)\]|([^\s:\[\]]+)):(\d{1,5})\s*$/.exec(text);
    if (!match) throw new Error(`expected host:port, got "${text}"`);
    const port = Number(match[3]);
    if (port < 1 || port > 65535) throw new Error(`bad port ${port}`);
    return { address: match[1] || match[2], port };
  }

  // "1.2.3.4:5" or "[::1]:5" -> { address, port }.
  function parsePeer(text) {
    const match = /^\s*(?:\[([0-9a-fA-F:.]+)\]|([0-9.]+)):(\d{1,5})\s*$/.exec(text);
    if (!match) throw new Error(`peer must be ip:port or [ipv6]:port, got "${text}"`);
    const port = Number(match[3]);
    if (port < 1 || port > 65535) throw new Error(`bad port ${port}`);
    return { address: match[1] || match[2], port };
  }

  function printable(data) {
    const text = decoder.decode(data);
    // eslint-disable-next-line no-control-regex
    if (!text.includes("\uFFFD") && !/[\x00-\x08\x0e-\x1f]/.test(text)) {
      return JSON.stringify(text.length > 200 ? `${text.slice(0, 200)}...` : text);
    }
    const hex = Array.from(data.subarray(0, 32), (byte) => byte.toString(16).padStart(2, "0")).join("");
    return `hex ${hex}${data.length > 32 ? "..." : ""}`;
  }

  function addrRow(label, value) {
    const item = document.createElement("li");
    const labelElement = document.createElement("span");
    labelElement.className = "label";
    labelElement.textContent = label;
    const valueElement = document.createElement("code");
    valueElement.textContent = value;
    const copy = document.createElement("button");
    copy.className = "small";
    copy.textContent = "copy";
    copy.onclick = () => navigator.clipboard?.writeText(value).catch(() => {});
    item.append(labelElement, valueElement, copy);
    $("addrs").append(item);
  }

  // ---- setup ----

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const script = document.createElement("script");
      script.src = src;
      script.onload = resolve;
      script.onerror = () => reject(new Error(`could not load ${src}`));
      document.head.append(script);
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
    const wrapped = {
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
        const message = { data: typeof data === "string" ? encoder.encode(data) : data };
        if (address !== undefined) Object.assign(message, { remoteAddress: address, remotePort: port });
        writer.write(message).catch((err) => wrapped.onerror?.({ name: err.name, code: err.name, message: err.message }));
      },
      close() {
        writer.releaseLock();
        reader.releaseLock();
        socket.close().catch(() => {});
      },
    };
    socket.onsenderror = (err) => wrapped.onerror?.({ name: err.name, code: err.name, message: err.message });
    socket.closed.then(() => ({ code: 1000, reason: "" }), (err) => ({ code: 0, reason: `${err.name}: ${err.message}` }))
      .then((event) => {
        wrapped.closed = true;
        wrapped.onclose?.(event);
      });
    (async () => {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        wrapped.onmessage?.({ data: value.data, address: value.remoteAddress, port: value.remotePort, ecn: value.ecn ?? 0 });
      }
    })().catch(() => {});
    return wrapped;
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

  function onDatagram(message) {
    if (tap && tap(message)) return;
    const from = formatAddr(message.address, message.port);
    log(`<- ${from} ${message.data.length} bytes: ${printable(message.data)}`);
    if ($("echo").checked) {
      sock.send(message.data, message.address, message.port);
      log(`-> ${from} echo ${message.data.length} bytes`);
    }
  }

  function sendText(peerText, text) {
    const peer = parsePeer(peerText);
    sock.send(encoder.encode(text), peer.address, peer.port);
    log(`-> ${formatAddr(peer.address, peer.port)} ${JSON.stringify(text)}`);
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
    const want = formatAddr(peer.address, peer.port);
    const reply = new Promise((resolve) => {
      tap = (message) => {
        if (message.port !== peer.port) return false;
        resolve(message);
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
    const message = await Promise.race([reply, timeout]);
    clearTimeout(timer);
    tap = null;
    if (!message) {
      result(false, `no reply from ${want} within 6 s`);
      return;
    }
    const got = decoder.decode(message.data);
    const ms = (performance.now() - start).toFixed(2);
    if (expect !== null && got !== expect) {
      result(false, `expected ${JSON.stringify(expect)} from ${want}, got ${JSON.stringify(got)}`);
    } else {
      result(true, `reply ${JSON.stringify(got)} from ${formatAddr(message.address, message.port)} in ${ms} ms`);
    }
  }

  // ---- connected socket: ?connect=host:port&send=&expect= ----

  // Opens a socket connected to host:port (the app resolves a name), sends
  // `text` until a reply arrives and reports it.
  async function runConnected(host, target, text, expect) {
    let connected;
    try {
      const peer = parseHostPort(target);
      connected = await connect(host, peer.address, peer.port, params.get("dnsQueryType"));
      log(`connected ${formatAddr(connected.localAddress, connected.localPort)} -> ${formatAddr(connected.remoteAddress, connected.remotePort)} (${peer.address})`);
      let failed;
      const error = new Promise((resolve) => { failed = resolve; });
      connected.onerror = ({ code, message }) => failed(`${code}: ${message}`);
      const reply = new Promise((resolve) => {
        connected.onmessage = (message) => {
          log(`<- (connected) ${message.data.length} bytes: ${printable(message.data)}`);
          resolve(decoder.decode(message.data));
        };
      });
      const start = performance.now();
      let timer;
      const resend = (left) => {
        if (connected.closed) return;
        connected.send(encoder.encode(text));
        log(`-> (connected) ${JSON.stringify(text)}`);
        if (left > 1) timer = setTimeout(() => resend(left - 1), 1000);
      };
      resend(5);
      const timeout = new Promise((resolve) => setTimeout(() => resolve(null), 6000));
      const got = await Promise.race([reply, error.then((err) => ({ error: err })), timeout]);
      clearTimeout(timer);
      const where = `${formatAddr(connected.remoteAddress, connected.remotePort)} (connected to ${peer.address})`;
      if (got === null) result(false, `no reply from ${where} within 6 s`);
      else if (got.error) result(false, `send error: ${got.error}`);
      else if (expect !== null && got !== expect) result(false, `expected ${JSON.stringify(expect)} from ${where}, got ${JSON.stringify(got)}`);
      else result(true, `reply ${JSON.stringify(got)} from ${where} in ${(performance.now() - start).toFixed(2)} ms`);
    } catch (err) {
      result(false, `connected socket to ${target}: ${err.name}: ${err.message}`);
    } finally {
      connected?.close();
    }
  }

  // ---- bench: ?bench=ip:port ----

  function percentile(sorted, p) {
    if (sorted.length === 0) return NaN;
    return sorted[Math.min(sorted.length - 1, Math.floor((sorted.length * p) / 100))];
  }

  // Sequential 1200-byte ping-pong; returns RTTs in ms.
  async function benchRtt(peer, rounds, warmup) {
    const payload = new Uint8Array(BENCH_SIZE);
    const view = new DataView(payload.buffer);
    const rtts = [];
    let lost = 0;
    let waiting = null; // { seq, resolve }
    tap = (message) => {
      if (message.data.length >= 4 && waiting) {
        const seq = new DataView(message.data.buffer, message.data.byteOffset).getUint32(0);
        if (seq === waiting.seq) waiting.resolve(true);
      }
      return true;
    };
    for (let i = 0; i < warmup + rounds; i++) {
      view.setUint32(0, i);
      const got = new Promise((resolve) => {
        const timer = setTimeout(() => resolve(false), 1000);
        waiting = { seq: i, resolve: (value) => { clearTimeout(timer); resolve(value); } };
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

  // Keeps `windowSize` datagrams in flight until `count` replies arrived or were
  // declared lost (no reply for 300 ms refills the window).
  async function benchThroughput(peer, count, windowSize) {
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
      while (sent < count && sent - received - lost < windowSize) sendOne();
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
    const rounds = Number(params.get("n") || 2000);
    const count = Number(params.get("count") || 20000);
    const windowSize = Number(params.get("window") || 64);
    $("send").disabled = true;
    log(`bench: ${rounds} x ${BENCH_SIZE} B ping-pong to ${formatAddr(peer.address, peer.port)}`);
    const { rtts, lost } = await benchRtt(peer, rounds, 50);
    const sorted = rtts.slice().sort((left, right) => left - right);
    const mean = rtts.reduce((sum, rtt) => sum + rtt, 0) / (rtts.length || 1);
    const rttLine = `rtt p50=${percentile(sorted, 50).toFixed(3)}ms p99=${percentile(sorted, 99).toFixed(3)}ms `
      + `mean=${mean.toFixed(3)}ms min=${(sorted[0] ?? NaN).toFixed(3)}ms lost=${lost}/${rounds}`;
    log(`bench: ${rttLine}`);

    log(`bench: throughput ${count} x ${BENCH_SIZE} B, window ${windowSize}`);
    const throughput = await benchThroughput(peer, count, windowSize);
    const pps = throughput.received / throughput.secs;
    const mbit = (pps * BENCH_SIZE * 8) / 1e6;
    const tputLine = `tput ${Math.round(pps)} pkt/s each way ${mbit.toFixed(1)} Mbit/s each way `
      + `lost=${throughput.lost}/${count} time=${throughput.secs.toFixed(2)}s window=${windowSize}`;
    log(`bench: ${tputLine}`);
    $("send").disabled = false;
    const ok = rtts.length > 0 && throughput.received > 0;
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
    addrRow("bound", formatAddr(sock.localAddress, sock.localPort));
    for (const a of sock.interfaceAddrs) addrRow("interface", formatAddr(a.address, a.port));
    log(`bound ${formatAddr(sock.localAddress, sock.localPort)}, gso=${sock.maxGsoSegments} gro=${sock.groSegments}`);

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
