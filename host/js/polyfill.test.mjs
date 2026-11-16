// Tests of the injected script that need no host: install rules, host
// patterns and UDPSocket option and constructor checks.
//
//   node host/js/polyfill.test.mjs
//
// Socket behaviour is tested in a browser: host/examples/test-page.
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const here = path.dirname(fileURLToPath(import.meta.url));
const script = readFileSync(path.join(here, "webview-sockets.js"), "utf8");
const codec = readFileSync(path.join(here, "../../proto/js/codec.js"), "utf8");
const codecClassic = codec.slice(0, codec.indexOf("// @@webview-sockets-codec-module-exports@@"));
const assembled = script.replace("// @@webview-sockets-codec@@", codecClassic);
const HOSTS_LINE = "const HOSTS = null; // @@webview-sockets-hosts@@";

let failures = 0;
let passed = 0;
function check(name, cond) {
  if (cond) {
    passed++;
  } else {
    failures++;
    console.log(`FAIL ${name}`);
  }
}

// Runs the script in a fresh context that looks like a page on `host`.
function page({ host = "app.localhost:8080", protocol = "http:", hosts = null, native = false, frame = false } = {}) {
  const ctx = {
    DOMException, TextEncoder, TextDecoder, Uint8Array, ArrayBuffer, DataView, console, queueMicrotask,
    location: { protocol, host },
  };
  ctx.window = ctx;
  ctx.top = frame ? {} : ctx;
  if (native) ctx.UDPSocket = function UDPSocket() {};
  vm.createContext(ctx);
  // Run in the context, but keep the host realm's error classes for instanceof.
  const src = hosts ? assembled.replace(HOSTS_LINE, `const HOSTS = ${JSON.stringify(hosts)};`) : assembled;
  vm.runInContext(`globalThis.window = this; ${src}`, ctx);
  return ctx;
}

function throwsName(f, name) {
  try {
    f();
  } catch (err) {
    return err.name === name || (name === "TypeError" && err.constructor.name === "TypeError");
  }
  return false;
}

// ---- install rules ----
{
  const w = page();
  check("installs UDPSocket", typeof w.UDPSocket === "function");
  check("installs webviewSockets", typeof w.webviewSockets === "object");
  check("webviewSockets.UDPSocket is the polyfill", w.webviewSockets.UDPSocket === w.UDPSocket);
  check("version", w.webviewSockets.version === 2);
  check("requestSession", typeof w.webviewSockets.requestSession === "function");
  check("no old udp API", w.webviewSockets.udp === undefined);
}
{
  const w = page({ native: true });
  check("keeps native UDPSocket", w.UDPSocket.toString().startsWith("function UDPSocket() {}"));
  check("polyfill still on webviewSockets", typeof w.webviewSockets.UDPSocket === "function"
    && w.webviewSockets.UDPSocket !== w.UDPSocket);
}
check("not in frames", page({ frame: true }).webviewSockets === undefined);
check("not on file:", page({ protocol: "file:" }).webviewSockets === undefined);
check("host pattern match", page({ hosts: ["*.localhost:*"] }).webviewSockets !== undefined);
check("host pattern mismatch", page({ hosts: ["*.localhost:*"], host: "example.com" }).webviewSockets === undefined);
check("host pattern one label", page({ hosts: ["*.localhost:*"], host: "a.b.localhost:1" }).webviewSockets === undefined);
check("host pattern exact", page({ hosts: ["x.test", "app.localhost:8080"] }).webviewSockets !== undefined);

// ---- constructor checks (these throw before any host contact) ----
{
  const w = page();
  const U = w.UDPSocket;
  const T = (opts, what) => check(`TypeError: ${what}`, throwsName(() => new U(opts), "TypeError"));
  const N = (opts, what) => check(`NotAllowedError: ${what}`, throwsName(() => new U(opts), "NotAllowedError"));
  T(undefined, "no options");
  T({}, "empty options");
  T(5, "non-object options");
  T({ remoteAddress: "example.com" }, "remoteAddress without remotePort");
  T({ remotePort: 53 }, "remotePort without remoteAddress");
  T({ remoteAddress: "example.com", remotePort: 70000 }, "remotePort out of range");
  T({ remoteAddress: "example.com", remotePort: NaN }, "remotePort NaN");
  T({ remoteAddress: "example.com", remotePort: 53, localAddress: "0.0.0.0" }, "remote + localAddress");
  T({ remoteAddress: "example.com", remotePort: 53, ipv6Only: true }, "ipv6Only in connected mode");
  T({ remoteAddress: "example.com", remotePort: 53, dnsQueryType: "ipv5" }, "bad dnsQueryType");
  T({ localAddress: "0.0.0.0", dnsQueryType: "ipv4" }, "dnsQueryType in bound mode");
  T({ localAddress: "not-an-ip" }, "localAddress not an IP");
  T({ localAddress: "0.0.0.0", localPort: 0 }, "localPort 0");
  T({ localPort: 5000 }, "localPort without localAddress");
  T({ localAddress: "0.0.0.0", ipv6Only: true }, "ipv6Only with IPv4 address");
  T({ localAddress: "::1", ipv6Only: false }, "ipv6Only with non-unspecified address");
  T({ localAddress: "::", sendBufferSize: 0 }, "sendBufferSize 0");
  T({ localAddress: "::", receiveBufferSize: 0 }, "receiveBufferSize 0");
  T({ localAddress: "::", receiveBufferSize: -1 }, "receiveBufferSize negative");
  N({ localAddress: "0.0.0.0", multicastLoopback: true }, "multicastLoopback");
  N({ localAddress: "0.0.0.0", multicastTimeToLive: 1 }, "multicastTimeToLive");
  N({ localAddress: "0.0.0.0", multicastAllowAddressSharing: true }, "multicastAllowAddressSharing");
  N({ remoteAddress: "224.0.0.1", remotePort: 9 }, "multicast remoteAddress");
  N({ remoteAddress: "ff02::1", remotePort: 9 }, "multicast IPv6 remoteAddress");

  // Valid options construct; without a bridge, opened rejects with NotSupportedError.
  const ok = [
    { localAddress: "::" },
    { localAddress: "::", ipv6Only: true },
    { localAddress: "127.0.0.1", localPort: 4433, sendBufferSize: 65536, receiveBufferSize: 65536 },
    { remoteAddress: "example.com", remotePort: 53, dnsQueryType: "ipv6" },
    { remoteAddress: "[::1]", remotePort: 53 },
    { remoteAddress: "10.0.0.1", remotePort: "53" },
  ];
  const sockets = [];
  for (const opts of ok) {
    try {
      sockets.push(new U(opts));
      passed++;
    } catch (err) {
      failures++;
      console.log(`FAIL constructs ${JSON.stringify(opts)}: ${err.name}: ${err.message}`);
    }
  }
  const results = await Promise.allSettled(sockets.map((s) => s.opened));
  check("opened rejects without bridge",
    results.every((r) => r.status === "rejected" && r.reason.name === "NotSupportedError"));
  const closed = await Promise.allSettled(sockets.map((s) => s.closed));
  check("closed rejects without bridge",
    closed.every((r) => r.status === "rejected" && r.reason.name === "NotSupportedError"));
  const close = await Promise.allSettled([sockets[0].close()]);
  check("close() after failed open is InvalidStateError",
    close[0].status === "rejected" && close[0].reason.name === "InvalidStateError");
  const s = new U({ localAddress: "::" });
  const early = await Promise.allSettled([s.close()]);
  check("close() before open is InvalidStateError",
    early[0].status === "rejected" && early[0].reason.name === "InvalidStateError");
  check("toStringTag", Object.prototype.toString.call(s) === "[object UDPSocket]");
  await s.opened.catch(() => {});
}

console.log(`${passed} passed, ${failures} failed`);
process.exit(failures ? 1 : 0);
