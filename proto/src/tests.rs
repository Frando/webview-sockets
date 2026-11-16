use alloc::{string::ToString, vec, vec::Vec};

use super::*;

fn v4(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn token() -> [u8; TOKEN_LEN] {
    let mut token = [0u8; TOKEN_LEN];
    for (i, b) in token.iter_mut().enumerate() {
        *b = i as u8;
    }
    token
}

fn hello(bind_addr: SocketAddr) -> Hello<'static> {
    Hello::bind(token(), bind_addr)
}

fn encoded<F: FnOnce(&mut Vec<u8>)>(f: F) -> Vec<u8> {
    let mut out = Vec::new();
    f(&mut out);
    out
}

#[test]
fn hello_roundtrip_v4_v6() {
    for addr in ["0.0.0.0:0", "[::]:0", "192.168.1.2:4433", "[fe80::1]:9"] {
        let h = hello(v4(addr));
        let buf = encoded(|o| encode_hello(&h, o));
        assert_eq!(buf[0], KIND_HELLO);
        assert_eq!(buf[1], VERSION);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Hello(h));
    }
}

#[test]
fn hello_exact_bytes() {
    let mut h = hello(v4("1.2.3.4:258"));
    h.send_buffer_size = 0x0102_0304;
    h.mode = Mode::Bound {
        local: v4("1.2.3.4:258"),
        ipv6_only: Some(true),
    };
    let buf = encoded(|o| encode_hello(&h, o));
    assert_eq!(buf.len(), 2 + 32 + 8 + 1 + 7 + 1);
    assert_eq!(&buf[34..42], &[1, 2, 3, 4, 0, 0, 0, 0]);
    assert_eq!(&buf[42..], &[0, 4, 1, 2, 3, 4, 1, 2, 2]);
}

#[test]
fn hello_connected_roundtrip() {
    for (host, dns) in [
        ("example.com", None),
        ("127.0.0.1", Some(DnsQueryType::Ipv4)),
        ("[::1]", Some(DnsQueryType::Ipv6)),
        ("", None),
    ] {
        let h = Hello {
            token: token(),
            mode: Mode::Connected {
                remote_host: host,
                remote_port: 443,
                dns_query_type: dns,
            },
            send_buffer_size: 1,
            receive_buffer_size: 65536,
        };
        let buf = encoded(|o| encode_hello(&h, o));
        assert_eq!(decode_first(&buf).unwrap(), Frame::Hello(h));
    }
    // Long names are cut to MAX_HOST_LEN at a char boundary.
    let long = "\u{e9}".repeat(200);
    let h = Hello::connect(token(), &long, 1);
    let buf = encoded(|o| encode_hello(&h, o));
    let Frame::Hello(d) = decode_first(&buf).unwrap() else {
        panic!("not hello");
    };
    let Mode::Connected { remote_host, .. } = d.mode else {
        panic!("not connected");
    };
    assert_eq!(remote_host.len(), 254);
}

#[test]
fn named_data_roundtrip() {
    let named = NamedData {
        ecn: 2,
        host: "example.com",
        port: 53,
        dns_query_type: Some(DnsQueryType::Ipv6),
        segment_size: 0,
        payload: b"query",
    };
    let data = Data {
        ecn: 0,
        peer: v4("1.2.3.4:5"),
        segment_size: 0,
        payload: b"x",
    };
    let mut buf = Vec::new();
    encode_named_data(&named, &mut buf);
    encode_data(&data, &mut buf);
    assert_eq!(buf.len(), named.encoded_len() + data.encoded_len());
    let recs: Vec<_> = decode_records(&buf)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(recs, [Record::Named(named), Record::Data(data)]);
    // The host-to-page decoder refuses named records.
    let mut it = decode_data(&buf).unwrap();
    assert_eq!(
        it.next(),
        Some(Err(DecodeError::UnexpectedKind(KIND_DATA_NAMED)))
    );
    assert_eq!(it.next(), None);
    // Truncated prefixes and bad dns bytes.
    let one = encoded(|o| encode_named_data(&named, o));
    for n in 1..one.len() {
        let mut it = decode_records(&one[..n]).unwrap();
        assert_eq!(it.next(), Some(Err(DecodeError::Truncated)), "n={n}");
    }
    let mut bad_dns = one.clone();
    bad_dns[2] = 5;
    assert_eq!(
        decode_records(&bad_dns).unwrap().next(),
        Some(Err(DecodeError::BadDnsQueryType(5)))
    );
    let mut bad_utf8 = one.clone();
    bad_utf8[4] = 0xff;
    assert_eq!(
        decode_records(&bad_utf8).unwrap().next(),
        Some(Err(DecodeError::InvalidUtf8))
    );
}

#[test]
fn bound_roundtrip() {
    let b = Bound {
        local: v4("[::]:5000"),
        remote: None,
        max_gso_segments: 64,
        gro_segments: 1,
        interface_addrs: vec![v4("10.0.0.2:5000"), v4("[2001:db8::2]:5000")],
    };
    let buf = encoded(|o| encode_bound(&b, o));
    assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(b));

    let connected = Bound {
        local: v4("192.168.1.2:5000"),
        remote: Some(v4("[2001:db8::1]:443")),
        max_gso_segments: 1,
        gro_segments: 1,
        interface_addrs: vec![],
    };
    let buf = encoded(|o| encode_bound(&connected, o));
    assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(connected));

    let empty = Bound {
        local: v4("127.0.0.1:1"),
        remote: None,
        max_gso_segments: 1,
        gro_segments: 1,
        interface_addrs: vec![],
    };
    let buf = encoded(|o| encode_bound(&empty, o));
    assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(empty));
}

#[test]
fn bound_caps_interface_addrs() {
    let b = Bound {
        local: v4("127.0.0.1:1"),
        remote: None,
        max_gso_segments: 1,
        gro_segments: 1,
        interface_addrs: vec![v4("127.0.0.1:1"); 300],
    };
    let buf = encoded(|o| encode_bound(&b, o));
    let Frame::Bound(d) = decode_first(&buf).unwrap() else {
        panic!("not bound");
    };
    assert_eq!(d.interface_addrs.len(), 255);
}

#[test]
fn error_roundtrip_all_codes() {
    for code in 1..=6u8 {
        let code = ErrorCode::from_u8(code).unwrap();
        let e = ErrorFrame {
            code,
            message: "forbidden destination \u{e9}",
        };
        let buf = encoded(|o| encode_error(&e, o));
        assert_eq!(decode_first(&buf).unwrap(), Frame::Error(e));
    }
    assert_eq!(ErrorCode::from_u8(0), None);
    assert_eq!(ErrorCode::from_u8(7), None);
}

#[test]
fn error_message_truncated_at_char_boundary() {
    let long = "\u{e9}".repeat(40_000); // 80000 bytes
    let e = ErrorFrame {
        code: ErrorCode::Io,
        message: &long,
    };
    let buf = encoded(|o| encode_error(&e, o));
    let Frame::Error(d) = decode_first(&buf).unwrap() else {
        panic!("not error");
    };
    assert_eq!(d.message.len(), 65534);
}

#[test]
fn data_multi_record() {
    let recs = [
        Data {
            ecn: 0,
            peer: v4("1.2.3.4:5"),
            segment_size: 0,
            payload: b"hello",
        },
        Data {
            ecn: 3,
            peer: v4("[2001:db8::1]:443"),
            segment_size: 4,
            payload: b"0123456789",
        },
        Data {
            ecn: 2,
            peer: v4("127.0.0.1:9"),
            segment_size: 0,
            payload: b"",
        },
    ];
    let mut buf = Vec::new();
    for r in &recs {
        encode_data(r, &mut buf);
    }
    assert_eq!(buf.len(), recs.iter().map(|r| r.encoded_len()).sum());
    let decoded: Vec<_> = decode_data(&buf)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(decoded, recs);
    // Zero copy: payload points into the message.
    let p = decoded[0].payload.as_ptr() as usize;
    let range = buf.as_ptr() as usize..buf.as_ptr() as usize + buf.len();
    assert!(range.contains(&p));
}

#[test]
fn data_header_sizes() {
    let d4 = Data {
        ecn: 0,
        peer: v4("1.2.3.4:5"),
        segment_size: 0,
        payload: b"",
    };
    let d6 = Data {
        peer: v4("[::1]:5"),
        ..d4
    };
    assert_eq!(d4.encoded_len(), 15);
    assert_eq!(d6.encoded_len(), 27);
}

#[test]
fn data_masks_ecn() {
    let d = Data {
        ecn: 0xff,
        peer: v4("1.2.3.4:5"),
        segment_size: 0,
        payload: b"x",
    };
    let buf = encoded(|o| encode_data(&d, o));
    assert_eq!(buf[1], 3);
}

#[test]
fn segments() {
    let d = Data {
        ecn: 0,
        peer: v4("1.2.3.4:5"),
        segment_size: 4,
        payload: b"0123456789",
    };
    let s: Vec<&[u8]> = d.segments().collect();
    assert_eq!(s, [&b"0123"[..], b"4567", b"89"]);
    let single = Data {
        segment_size: 0,
        ..d
    };
    assert_eq!(single.segments().count(), 1);
    let empty = Data { payload: b"", ..d };
    assert_eq!(empty.segments().collect::<Vec<_>>(), [&b""[..]]);
}

#[test]
fn malformed_first() {
    let h = encoded(|o| encode_hello(&hello(v4("[::]:0")), o));
    // Every strict prefix is truncated.
    for n in 1..h.len() {
        assert_eq!(decode_first(&h[..n]), Err(DecodeError::Truncated), "n={n}");
    }
    let mut trailing = h.clone();
    trailing.push(0);
    assert_eq!(decode_first(&trailing), Err(DecodeError::TrailingBytes));
    let mut bad_version = h.clone();
    bad_version[1] = 1;
    assert_eq!(
        decode_first(&bad_version),
        Err(DecodeError::UnsupportedVersion(1))
    );
    let mut bad_mode = h.clone();
    bad_mode[42] = 2;
    assert_eq!(decode_first(&bad_mode), Err(DecodeError::UnknownMode(2)));
    let mut bad_family = h.clone();
    bad_family[43] = 5;
    assert_eq!(decode_first(&bad_family), Err(DecodeError::BadFamily(5)));
    let mut bad_v6only = h.clone();
    *bad_v6only.last_mut().unwrap() = 3;
    assert_eq!(decode_first(&bad_v6only), Err(DecodeError::BadFlag(3)));
    let c = encoded(|o| encode_hello(&Hello::connect(token(), "a", 1), o));
    let mut bad_dns = c.clone();
    bad_dns[43] = 1;
    assert_eq!(decode_first(&bad_dns), Err(DecodeError::BadDnsQueryType(1)));
    let mut bad_host = c.clone();
    bad_host[45] = 0xff;
    assert_eq!(decode_first(&bad_host), Err(DecodeError::InvalidUtf8));

    assert_eq!(decode_first(&[]), Err(DecodeError::Empty));
    assert_eq!(decode_first(&[0x7f]), Err(DecodeError::UnknownKind(0x7f)));
    assert_eq!(
        decode_first(&[KIND_DATA]),
        Err(DecodeError::UnexpectedKind(KIND_DATA))
    );
    assert_eq!(
        decode_first(&[KIND_ERROR, 9, 0, 0]),
        Err(DecodeError::UnknownErrorCode(9))
    );
    assert_eq!(
        decode_first(&[KIND_ERROR, 1, 0, 2, 0xff, 0xfe]),
        Err(DecodeError::InvalidUtf8)
    );
    let big = vec![KIND_HELLO; MAX_MESSAGE_LEN + 1];
    assert_eq!(
        decode_first(&big),
        Err(DecodeError::Oversize(MAX_MESSAGE_LEN + 1))
    );
}

#[test]
fn malformed_data() {
    let d = Data {
        ecn: 1,
        peer: v4("[::1]:5"),
        segment_size: 0,
        payload: b"abc",
    };
    let one = encoded(|o| encode_data(&d, o));
    for n in 1..one.len() {
        let mut it = decode_data(&one[..n]).unwrap();
        assert_eq!(it.next(), Some(Err(DecodeError::Truncated)), "n={n}");
        assert_eq!(it.next(), None);
    }
    // Good record followed by garbage: first ok, then error, then end.
    let mut two = one.clone();
    two.push(0x55);
    let mut it = decode_data(&two).unwrap();
    assert_eq!(it.next(), Some(Ok(d)));
    assert_eq!(it.next(), Some(Err(DecodeError::UnknownKind(0x55))));
    assert_eq!(it.next(), None);

    let mut bad_ecn = one.clone();
    bad_ecn[1] = 4;
    assert_eq!(
        decode_data(&bad_ecn).unwrap().next(),
        Some(Err(DecodeError::BadEcn(4)))
    );
    let mut bad_family = one.clone();
    bad_family[2] = 0;
    assert_eq!(
        decode_data(&bad_family).unwrap().next(),
        Some(Err(DecodeError::BadFamily(0)))
    );
    let mut hello_in_data = one.clone();
    hello_in_data[0] = KIND_HELLO;
    assert_eq!(
        decode_data(&hello_in_data).unwrap().next(),
        Some(Err(DecodeError::UnexpectedKind(KIND_HELLO)))
    );
    // Length field larger than the rest.
    let mut long_len = one.clone();
    let len_at = one.len() - 3 - 4;
    long_len[len_at..len_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        decode_data(&long_len).unwrap().next(),
        Some(Err(DecodeError::Truncated))
    );
    assert_eq!(decode_data(&[]).unwrap_err(), DecodeError::Empty);
    let big = vec![0u8; MAX_MESSAGE_LEN + 1];
    assert!(matches!(decode_data(&big), Err(DecodeError::Oversize(_))));
}

#[test]
fn max_message_fits() {
    let payload = vec![7u8; MAX_MESSAGE_LEN - 27];
    let d = Data {
        ecn: 0,
        peer: v4("[::1]:5"),
        segment_size: 0,
        payload: &payload,
    };
    let buf = encoded(|o| encode_data(&d, o));
    assert_eq!(buf.len(), MAX_MESSAGE_LEN);
    assert_eq!(decode_data(&buf).unwrap().next(), Some(Ok(d)));
}

#[test]
fn message_kind_and_names() {
    assert_eq!(message_kind(&[]), None);
    assert_eq!(message_kind(&[KIND_DATA, 1]), Some(KIND_DATA));
    assert_eq!(DecodeError::Truncated.to_string(), "message truncated");
    assert_eq!(ErrorCode::AddrInUse.name(), "AddrInUse");
}

#[test]
fn js_codec_classic_strips_exports() {
    let classic = js_codec_classic();
    assert!(classic.len() < JS_CODEC.len());
    assert!(!classic.contains("export const"));
    assert!(!classic.contains(JS_CODEC_EXPORT_MARKER));
    assert!(classic.contains("webviewSocketsCodec"));
}
