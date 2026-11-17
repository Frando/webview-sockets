# webview-sockets-proto

The wire format between a web page and the app hosting it. One WebSocket
on `/.well-known/webview-sockets/udp` of the page's own origin carries one
UDP socket: the page sends `HELLO`, the host answers `BOUND` or `ERROR`,
then both sides exchange `DATA` messages with one or more datagrams. The
Rust crate is `no_std` and zero-copy. [`js/codec.js`](js/codec.js) is the
same codec in JavaScript.

## Wire format v2

Binary messages, big endian, at most 256 KiB. `addr` is `u8 family (4|6),
4 or 16 address bytes, u16 port`. `dns` is `u8`: 0 unset, 4 ipv4, 6 ipv6.
`host` is `u8 len, utf8[len]` (a name or an IP literal).

```text
HELLO  0x01  u8 version=2  u8[32] token  u32 send_buffer_size
             u32 receive_buffer_size  u8 mode
       mode 0 (bound):     addr local  u8 ipv6_only (0 unset, 1 false, 2 true)
       mode 1 (connected): dns  host remote  u16 remote_port
BOUND  0x02  addr local  u8 has_remote  [addr remote]  u16 max_gso_segments
             u16 gro_segments  u8 n  addr[n] interface_addrs
ERROR  0x03  u8 code  u16 len  utf8 message
       1 NotAllowed  2 InvalidInput  3 AddrInUse  4 Limit  5 Io
       6 NameNotResolved
DATA   0x10  u8 ecn  addr peer  u16 segment_size  u32 len  payload
DATA_NAMED 0x11  u8 ecn  dns  host  u16 port  u16 segment_size  u32 len
                 payload   (page to host, bound mode only)
```

- Buffer sizes of 0 mean the host default.
- In connected mode the host resolves the name, binds an ephemeral port,
  reports the peer in BOUND, and only talks to that peer.
- `segment_size` 0 is one datagram, otherwise a GSO (page to host) or GRO
  (host to page) batch.
- After BOUND, ERROR reports a failed send. The socket stays open.
- Close codes: 1000 normal, 4001 revoked, 4002 switched off, 4003 tab
  navigated or closed, 4004 limit, 4005 I/O error.

The JS codec uses the Direct Sockets option names
(`encodeHello(token, { remoteAddress: "example.com", remotePort: 53 })`)
and encodes DATA with a non-IP `address` as DATA_NAMED.

## Session tokens

The token comes from the platform bridge, not this protocol. The page sends
`{"ns": "webview-sockets", "id": 1, "op": "requestSession"}`. The app
answers `{"id": 1, "ok": true, "url": "ws://.../.well-known/webview-sockets/udp",
"token": "<64 hex chars>"}` or `{"id": 1, "ok": false, "error": "NotAllowed"}`.

## Tests

Both codecs are checked against [`testdata/vectors.json`](testdata/vectors.json):

```sh
cargo test -p webview-sockets-proto
node proto/js/codec.test.mjs
```

See also [`webview-sockets-host`](../host/README.md) and
[`webview-sockets`](../webview-sockets/README.md).

## License

Licensed under either of [Apache License, Version 2.0](../LICENSE-APACHE)
or [MIT license](../LICENSE-MIT), at your option.
