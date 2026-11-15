use alloc::{string::ToString, vec, vec::Vec};

use super::*;

fn v4(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn hello(bind_addr: SocketAddr) -> Hello {
    let mut token = [0u8; TOKEN_LEN];
    for (i, b) in token.iter_mut().enumerate() {
        *b = i as u8;
    }
    Hello { token, bind_addr }
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
    let h = hello(v4("1.2.3.4:258"));
    let buf = encoded(|o| encode_hello(&h, o));
    assert_eq!(buf.len(), 2 + 32 + 7);
    assert_eq!(&buf[34..], &[4, 1, 2, 3, 4, 1, 2]);
}

#[test]
fn bound_roundtrip() {
    let b = Bound {
        local: v4("[::]:5000"),
        max_gso_segments: 64,
        gro_segments: 1,
        interface_addrs: vec![v4("10.0.0.2:5000"), v4("[2001:db8::2]:5000")],
    };
    let buf = encoded(|o| encode_bound(&b, o));
    assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(b));

    let empty = Bound {
        local: v4("127.0.0.1:1"),
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
    for code in 1..=5u8 {
        let code = ErrorCode::from_u8(code).unwrap();
        let e = ErrorFrame {
            code,
            message: "forbidden destination \u{e9}",
        };
        let buf = encoded(|o| encode_error(&e, o));
        assert_eq!(decode_first(&buf).unwrap(), Frame::Error(e));
    }
    assert_eq!(ErrorCode::from_u8(0), None);
    assert_eq!(ErrorCode::from_u8(6), None);
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
    bad_version[1] = 2;
    assert_eq!(
        decode_first(&bad_version),
        Err(DecodeError::UnsupportedVersion(2))
    );
    let mut bad_family = h.clone();
    bad_family[34] = 5;
    assert_eq!(decode_first(&bad_family), Err(DecodeError::BadFamily(5)));

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
    assert!(classic.contains("irohLinkUdpCodec"));
}
