// Mock iroh-link UDP host plus a UDP echo server, used to run the wasm shim
// under Node (Node >= 22 has a global WebSocket client).
//
// Usage: node mock-host.mjs <path to wasm-bindgen --target nodejs output .js>
//
// Behaviour of the mock host:
// - token must be TOKEN, else ERROR NotAllowed.
// - bind port 1 -> ERROR AddrInUse.
// - BOUND reports maxGsoSegments = groSegments = 4.
// - sends to port 9 -> ERROR InvalidInput (socket stays open); to port 7 are
//   dropped (sink).
// - received datagrams are coalesced into GRO batches per tick.
// - the payload "close-me" makes the host close with code 4001 (revoked),
//   "close-off" with 4002 (setting off), "close-nav" with 4003 (navigated)
//   and "close-normal" with 1000.

import { createHash } from "node:crypto";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import dgram from "node:dgram";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const codec = (await import(path.join(here, "../../../udp-proto/js/codec.js"))).default;

const TOKEN = "ab".repeat(32);
const GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// ---- minimal WebSocket server framing ----

function wsAccept(req, socket) {
  const key = req.headers["sec-websocket-key"];
  const accept = createHash("sha1").update(key + GUID).digest("base64");
  socket.write(
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
      `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  const conn = { socket, onmessage: () => {}, onclose: () => {}, closed: false };
  let buf = Buffer.alloc(0);
  let parts = [];
  socket.on("data", (chunk) => {
    buf = Buffer.concat([buf, chunk]);
    for (;;) {
      if (buf.length < 2) return;
      const fin = buf[0] & 0x80;
      const op = buf[0] & 0x0f;
      let len = buf[1] & 0x7f;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        off = 10;
      }
      const masked = buf[1] & 0x80;
      const maskOff = off;
      if (masked) off += 4;
      if (buf.length < off + len) return;
      const payload = Buffer.from(buf.subarray(off, off + len));
      if (masked) for (let i = 0; i < len; i++) payload[i] ^= buf[maskOff + (i % 4)];
      buf = buf.subarray(off + len);
      if (op === 8) {
        wsClose(conn, 1000);
        return;
      }
      if (op === 9) {
        sendFrame(socket, 10, payload);
        continue;
      }
      if (op === 2 || op === 0) {
        parts.push(payload);
        if (fin) {
          const msg = Buffer.concat(parts);
          parts = [];
          conn.onmessage(new Uint8Array(msg.buffer, msg.byteOffset, msg.length));
        }
      }
    }
  });
  socket.on("close", () => {
    if (!conn.closed) {
      conn.closed = true;
      conn.onclose();
    }
  });
  socket.on("error", () => {});
  return conn;
}

function sendFrame(socket, op, payload) {
  const len = payload.length;
  let head;
  if (len < 126) {
    head = Buffer.from([0x80 | op, len]);
  } else if (len < 65536) {
    head = Buffer.alloc(4);
    head[0] = 0x80 | op;
    head[1] = 126;
    head.writeUInt16BE(len, 2);
  } else {
    head = Buffer.alloc(10);
    head[0] = 0x80 | op;
    head[1] = 127;
    head.writeBigUInt64BE(BigInt(len), 2);
  }
  socket.write(Buffer.concat([head, Buffer.from(payload)]));
}

function wsSend(conn, bytes) {
  if (!conn.closed) sendFrame(conn.socket, 2, bytes);
}

function wsClose(conn, code) {
  if (conn.closed) return;
  const p = Buffer.alloc(2);
  p.writeUInt16BE(code, 0);
  sendFrame(conn.socket, 8, p);
  conn.socket.end();
  conn.closed = true;
  conn.onclose();
}

// ---- mock host session: one UDP socket per WebSocket ----

function session(conn) {
  let udp = null;
  let pending = [];
  conn.onclose = () => udp?.close();
  conn.onmessage = (msg) => {
    if (!udp) {
      const hello = codec.decodeFirst(msg);
      const token = Buffer.from(hello.token).toString("hex");
      if (token !== TOKEN) {
        wsSend(conn, codec.encodeError("NotAllowed", "bad token"));
        wsClose(conn, 1000);
        return;
      }
      if (hello.bindAddr.port === 1) {
        wsSend(conn, codec.encodeError("AddrInUse", "port 1 in use"));
        wsClose(conn, 1000);
        return;
      }
      udp = dgram.createSocket("udp4");
      udp.on("message", (data, rinfo) => {
        pending.push({ address: rinfo.address, port: rinfo.port, data });
        if (pending.length === 1) setImmediate(flushRecv);
      });
      udp.bind(hello.bindAddr.port, hello.bindAddr.address, () => {
        const local = udp.address();
        wsSend(
          conn,
          codec.encodeBound({
            local: { address: local.address, port: local.port },
            maxGsoSegments: 4,
            groSegments: 4,
            interfaceAddrs: [{ address: "127.0.0.1", port: local.port }],
          }),
        );
      });
      return;
    }
    for (const rec of codec.decodeData(msg)) {
      if (rec.port === 9) {
        wsSend(conn, codec.encodeError("InvalidInput", "port 9 forbidden"));
        continue;
      }
      if (rec.port === 7) continue;
      for (const seg of codec.splitSegments(rec)) {
        udp.send(seg, rec.port, rec.address);
      }
    }
  };

  // Coalesces consecutive same-peer, same-size datagrams (up to 4) into one
  // GRO record; the last of a batch may be shorter.
  function flushRecv() {
    const records = [];
    let cur = null;
    for (const d of pending) {
      const closeCode = CLOSE_PAYLOADS[new TextDecoder().decode(d.data)];
      if (closeCode !== undefined) {
        if (records.length) wsSend(conn, codec.encodeData(records));
        wsClose(conn, closeCode);
        pending = [];
        return;
      }
      const fits =
        cur &&
        cur.address === d.address &&
        cur.port === d.port &&
        cur.parts.length < 4 &&
        cur.lastLen === cur.size &&
        d.data.length <= cur.size;
      if (fits) {
        cur.parts.push(d.data);
        cur.lastLen = d.data.length;
      } else {
        cur = { address: d.address, port: d.port, size: d.data.length, lastLen: d.data.length, parts: [d.data] };
        records.push(cur);
      }
    }
    pending = [];
    wsSend(
      conn,
      codec.encodeData(
        records.map((r) => ({
          address: r.address,
          port: r.port,
          segmentSize: r.parts.length > 1 ? r.size : 0,
          payload: Buffer.concat(r.parts),
        })),
      ),
    );
  }
}

// ---- main ----

const echo = dgram.createSocket("udp4");
echo.on("message", (data, rinfo) => echo.send(data, rinfo.port, rinfo.address));
await new Promise((r) => echo.bind(0, "127.0.0.1", r));
const echoAddr = `127.0.0.1:${echo.address().port}`;

const server = createServer((_req, res) => res.writeHead(404).end());
server.on("upgrade", (req, socket) => {
  if (req.url !== codec.WS_PATH) {
    socket.destroy();
    return;
  }
  session(wsAccept(req, socket));
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const url = `http://127.0.0.1:${server.address().port}${codec.WS_PATH}`;

// Payloads that make the mock host close the WebSocket with a given code.
const CLOSE_PAYLOADS = {
  "close-me": codec.CLOSE_CODES.REVOKED,
  "close-off": codec.CLOSE_CODES.SETTING_OFF,
  "close-nav": codec.CLOSE_CODES.NAVIGATED,
  "close-normal": codec.CLOSE_CODES.NORMAL,
};

const require = createRequire(import.meta.url);
const shim = require(path.resolve(process.argv[2]));

let failed = false;
function check(name, got, want) {
  const ok = got === want;
  if (!ok) failed = true;
  console.log(`${ok ? "ok  " : "FAIL"} ${name}: ${got}${ok ? "" : ` (want ${want})`}`);
}

check("init without irohHost", await shim.init_kind(), "Unsupported");
globalThis.irohHost = {
  udp: {
    requestSession: async () => {
      throw new DOMException("blocked", "NotAllowedError");
    },
  },
};
check("init denied", await shim.init_kind(), "PermissionDenied");
check("bad token", await shim.bind_kind(url, "00".repeat(32), "0.0.0.0:0"), "PermissionDenied");
check("token not hex", await shim.bind_kind(url, "zz", "0.0.0.0:0"), "InvalidInput");
check("addr in use", await shim.bind_kind(url, TOKEN, "0.0.0.0:1"), "AddrInUse");
const deadUrl = `ws://127.0.0.1:1${codec.WS_PATH}`;
check("host unreachable", await shim.bind_kind(deadUrl, TOKEN, "0.0.0.0:0"), "ConnectionAborted");

globalThis.irohHost = { udp: { requestSession: async () => ({ url, token: TOKEN }) } };
try {
  console.log(`ok   run: ${await shim.run(echoAddr)}`);
} catch (e) {
  failed = true;
  console.log(`FAIL run: ${e}`);
}
for (const [payload, want] of [
  ["close-me", "PermissionDenied"],
  ["close-off", "PermissionDenied"],
  ["close-nav", "ConnectionAborted"],
  ["close-normal", "ConnectionAborted"],
]) {
  try {
    check(`host close ${payload}`, await shim.close_kind(echoAddr, payload), want);
  } catch (e) {
    failed = true;
    console.log(`FAIL host close ${payload}: ${e}`);
  }
}

echo.close();
server.close();
server.closeAllConnections?.();
console.log(failed ? "NODE-TEST FAIL" : "NODE-TEST PASS");
process.exit(failed ? 1 : 0);
