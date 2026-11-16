# webview-sockets-proto

The wire format between a web page and the host app for native UDP
sockets. One WebSocket on `/.well-known/webview-sockets/udp` of the page's
own origin carries one UDP socket: the page sends `HELLO`, the host answers
`BOUND` or `ERROR`, then both sides exchange `DATA` messages holding one or
more datagrams. The crate is `no_std` and decodes without copying.

## Wire format v2

All frames are binary WebSocket messages, integers big endian, at most
256 KiB per message. `addr` is `u8 family (4|6), 4 or 16 address bytes,
u16 port`. `dns` is `u8`: 0 unset, 4 `"ipv4"`, 6 `"ipv6"`. `host` is
`u8 len, utf8[len]` (a name or an IP literal, at most 255 bytes).

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

The two modes and option names follow the Direct Sockets `UDPSocket`
(`localAddress`/`localPort`/`ipv6Only` versus
`remoteAddress`/`remotePort`/`dnsQueryType`, plus `sendBufferSize` and
`receiveBufferSize`, 0 meaning the host default). In connected mode the host
resolves the name, binds an ephemeral port, reports the peer in BOUND, and
only sends to and receives from that peer. `segment_size` 0 is one datagram,
else a GSO (page to host) or GRO (host to page) batch. After BOUND the host
may send ERROR for a failed send; the socket stays open. Host close codes:
1000 normal, 4001 revoked, 4002 setting off, 4003 navigated or tab closed,
4004 limit, 4005 io error.

The JavaScript codec uses the Direct Sockets names in HELLO and BOUND, e.g.
`encodeHello(token, { remoteAddress: "example.com", remotePort: 53 })`, and
encodes a DATA record whose `address` is not an IP literal as DATA_NAMED.

The same codec exists in JavaScript in [`js/codec.js`](js/codec.js), and both
implementations are checked against the shared test vectors in
[`testdata/vectors.json`](testdata/vectors.json):

```sh
cargo test -p webview-sockets-proto
node proto/js/codec.test.mjs
```

The host side is [`webview-sockets-host`](../host/README.md), the
Rust page side is [`webview-sockets`](../webview-sockets/README.md).

## License

This project is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](../LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  https://opensource.org/licenses/MIT)

at your option.
