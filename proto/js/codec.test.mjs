// Checks js/codec.js against testdata/vectors.json (written by the Rust test
// tests/vectors.rs). Run: node udp-proto/js/codec.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

import * as codec from "./codec.js";

const here = new URL(".", import.meta.url);
const vectors = JSON.parse(readFileSync(new URL("../testdata/vectors.json", here), "utf8"));

const fromHex = (h) => Uint8Array.from(h.match(/../g) ?? [], (b) => parseInt(b, 16));
const toHex = (b) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");

let failures = 0;
function test(name, fn) {
  try {
    fn();
    console.log(`ok   ${name}`);
  } catch (e) {
    failures++;
    console.log(`FAIL ${name}\n     ${e.stack ?? e}`);
  }
}

function checkVectors(c, label) {
  for (const v of vectors) {
    test(`${label}: ${v.type}: ${v.name}`, () => {
      const bytes = fromHex(v.hex);
      const exp = v.expected;
      switch (v.type) {
        case "first": {
          const f = c.decodeFirst(bytes);
          if (f.kind === "hello") {
            assert.deepEqual({ ...f, token: toHex(f.token) }, exp);
            assert.equal(toHex(c.encodeHello(exp.token, exp.bindAddr)), v.hex);
            assert.equal(toHex(c.encodeHello(fromHex(exp.token), exp.bindAddr)), v.hex);
          } else if (f.kind === "bound") {
            assert.deepEqual(f, exp);
            assert.equal(toHex(c.encodeBound(exp)), v.hex);
          } else {
            assert.deepEqual(f, exp);
            assert.equal(toHex(c.encodeError(exp.code, exp.message)), v.hex);
            assert.equal(toHex(c.encodeError(exp.codeName, exp.message)), v.hex);
          }
          break;
        }
        case "data": {
          const recs = c.decodeData(bytes);
          assert.deepEqual(recs.map((r) => ({ ...r, payload: toHex(r.payload) })), exp);
          // Zero copy: payloads share the input buffer.
          for (const r of recs) assert.equal(r.payload.buffer, bytes.buffer);
          const enc = c.encodeData(exp.map((r) => ({ ...r, payload: fromHex(r.payload) })));
          assert.equal(toHex(enc), v.hex);
          break;
        }
        case "invalidFirst":
          assert.throws(() => c.decodeFirst(bytes), (e) => e.name === "UdpCodecError" && e.code === exp.error);
          break;
        case "invalidData":
          assert.throws(() => c.decodeData(bytes), (e) => e.name === "UdpCodecError" && e.code === exp.error);
          break;
        default:
          throw new Error(`unknown vector type ${v.type}`);
      }
    });
  }
}

checkVectors(codec, "module");

// The classic-script part (everything above the export marker) must work
// when evaluated as a plain script, as the hosts inject it.
const src = readFileSync(new URL("codec.js", here), "utf8");
const marker = "// @@iroh-link-udp-codec-module-exports@@";
assert.ok(src.includes(marker), "export marker missing");
const classic = src.split(marker)[0];
const ctx = vm.createContext({ TextEncoder, TextDecoder, Uint8Array, DataView, ArrayBuffer });
vm.runInContext(`(function () {\n${classic}\nglobalThis.codec = irohLinkUdpCodec;\n})();`, ctx);
// Objects from another realm fail deepEqual prototype checks; compare via JSON.
const classicCodec = {
  ...ctx.codec,
  decodeFirst: (b) => JSON.parse(JSON.stringify(ctx.codec.decodeFirst(b), (k, v) =>
    k === "token" ? Array.from(v) : v), (k, v) => (k === "token" ? Uint8Array.from(v) : v)),
  decodeData: (b) => ctx.codec.decodeData(b).map((r) => ({ ...r, payload: Uint8Array.from(r.payload) })),
};
test("classic: hello roundtrip", () => {
  const token = "ab".repeat(32);
  const hello = classicCodec.decodeFirst(ctx.codec.encodeHello(token, { address: "::1", port: 5 }));
  assert.equal(toHex(hello.token), token);
  assert.deepEqual({ ...hello.bindAddr }, { address: "::1", port: 5 });
});
test("classic: data roundtrip", () => {
  const msg = ctx.codec.encodeData({ address: "10.0.0.1", port: 9, payload: "hi" });
  const [r] = classicCodec.decodeData(msg);
  assert.equal(new TextDecoder().decode(r.payload), "hi");
});

test("splitSegments", () => {
  const [r] = codec.decodeData(codec.encodeData({ address: "::1", port: 1, segmentSize: 4, payload: "0123456789" }));
  assert.deepEqual(codec.splitSegments(r).map((s) => new TextDecoder().decode(s)), ["0123", "4567", "89"]);
  const [one] = codec.decodeData(codec.encodeData({ address: "::1", port: 1, payload: "abc" }));
  assert.equal(codec.splitSegments(one).length, 1);
});

test("encode input validation", () => {
  assert.throws(() => codec.encodeHello("ab", { address: "0.0.0.0", port: 0 }), TypeError);
  assert.throws(() => codec.encodeHello("ab".repeat(32), { address: "1.2.3", port: 0 }), TypeError);
  assert.throws(() => codec.encodeHello("ab".repeat(32), { address: "1.2.3.4", port: 70000 }), TypeError);
  assert.throws(() => codec.encodeData({ address: "1:2:3", port: 1, payload: "" }), TypeError);
  assert.throws(() => codec.encodeData({ address: "::1", port: 1, payload: 5 }), TypeError);
  const big = new Uint8Array(codec.MAX_MESSAGE_LEN);
  assert.throws(() => codec.encodeData({ address: "::1", port: 1, payload: big }), (e) => e.code === "Oversize");
  assert.equal(codec.encodeData({ address: "::1", port: 1, payload: big.subarray(27) }).length, codec.MAX_MESSAGE_LEN);
});

test("ipv6 parse/format", () => {
  const cases = [
    ["::", "::"],
    ["0:0:0:0:0:0:0:1", "::1"],
    ["2001:DB8:0:0:0:0:0:1", "2001:db8::1"],
    ["::ffff:1.2.3.4", "::ffff:1.2.3.4"],
    ["[fe80::1%eth0]", "fe80::1"],
    ["1:2:3:4:5:6:7::", "1:2:3:4:5:6:7:0"],
  ];
  for (const [input, out] of cases) assert.equal(codec.formatIpv6(codec.parseIpv6(input)), out);
  for (const bad of ["1::2::3", ":::", "1:2:3:4:5:6:7:8:9", "12345::", "1:2:3:4:5:6:7:8::", "g::1"]) {
    assert.equal(codec.parseIpv6(bad), null, bad);
  }
});

test("error message truncated at char boundary", () => {
  const f = codec.decodeFirst(codec.encodeError("Io", "\u00e9".repeat(40000)));
  assert.equal(new TextEncoder().encode(f.message).length, 65534);
});

if (failures > 0) {
  console.log(`\n${failures} failed`);
  process.exit(1);
}
console.log("\nall passed");
