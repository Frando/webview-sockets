//! Cross-language test vectors shared with `js/codec.test.mjs`.
//!
//! Checks that `testdata/vectors.json` matches what this crate encodes. Run
//! with `UPDATE_VECTORS=1` to rewrite the file (it is also written if
//! missing).

use std::{fmt::Write as _, net::SocketAddr, path::PathBuf};

use iroh_link_udp_proto::*;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    write!(out, "\\u{u:04x}").unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_addr(a: &SocketAddr) -> String {
    format!(
        "{{\"address\": {}, \"port\": {}}}",
        json_str(&a.ip().to_string()),
        a.port()
    )
}

struct Vectors(Vec<String>);

impl Vectors {
    fn push(&mut self, name: &str, ty: &str, bytes: &[u8], expected: String) {
        self.0.push(format!(
            "  {{\"name\": {}, \"type\": \"{ty}\", \"hex\": \"{}\", \"expected\": {expected}}}",
            json_str(name),
            hex(bytes)
        ));
    }

    fn hello(&mut self, name: &str, h: Hello) {
        let mut buf = Vec::new();
        encode_hello(&h, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Hello(h));
        let exp = format!(
            "{{\"kind\": \"hello\", \"version\": {VERSION}, \"token\": \"{}\", \"bindAddr\": {}}}",
            hex(&h.token),
            json_addr(&h.bind_addr)
        );
        self.push(name, "first", &buf, exp);
    }

    fn bound(&mut self, name: &str, b: Bound) {
        let mut buf = Vec::new();
        encode_bound(&b, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(b.clone()));
        let ifaces: Vec<_> = b.interface_addrs.iter().map(json_addr).collect();
        let exp = format!(
            "{{\"kind\": \"bound\", \"local\": {}, \"maxGsoSegments\": {}, \"groSegments\": {}, \"interfaceAddrs\": [{}]}}",
            json_addr(&b.local),
            b.max_gso_segments,
            b.gro_segments,
            ifaces.join(", ")
        );
        self.push(name, "first", &buf, exp);
    }

    fn error(&mut self, name: &str, e: ErrorFrame<'_>) {
        let mut buf = Vec::new();
        encode_error(&e, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Error(e));
        let exp = format!(
            "{{\"kind\": \"error\", \"code\": {}, \"codeName\": \"{}\", \"message\": {}}}",
            e.code as u8,
            e.code.name(),
            json_str(e.message)
        );
        self.push(name, "first", &buf, exp);
    }

    fn data(&mut self, name: &str, recs: &[Data<'_>]) {
        let mut buf = Vec::new();
        for r in recs {
            encode_data(r, &mut buf);
        }
        let decoded: Vec<_> = decode_data(&buf)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded, recs);
        let items: Vec<_> = recs
            .iter()
            .map(|r| {
                format!(
                    "{{\"ecn\": {}, \"address\": {}, \"port\": {}, \"segmentSize\": {}, \"payload\": \"{}\"}}",
                    r.ecn,
                    json_str(&r.peer.ip().to_string()),
                    r.peer.port(),
                    r.segment_size,
                    hex(r.payload)
                )
            })
            .collect();
        self.push(name, "data", &buf, format!("[{}]", items.join(", ")));
    }

    fn invalid(&mut self, name: &str, ty: &str, bytes: &[u8]) {
        let err = match ty {
            "invalidFirst" => decode_first(bytes).unwrap_err(),
            "invalidData" => match decode_data(bytes) {
                Err(e) => e,
                Ok(iter) => iter
                    .filter_map(Result::err)
                    .next()
                    .expect("vector must be invalid"),
            },
            _ => unreachable!(),
        };
        self.push(
            name,
            ty,
            bytes,
            format!("{{\"error\": \"{}\"}}", err.name()),
        );
    }
}

fn build() -> String {
    let mut v = Vectors(Vec::new());
    let mut token = [0u8; TOKEN_LEN];
    for (i, b) in token.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(7).wrapping_add(0xa0);
    }
    v.hello(
        "hello v4 any",
        Hello {
            token,
            bind_addr: addr("0.0.0.0:0"),
        },
    );
    v.hello(
        "hello v6 any",
        Hello {
            token: [0xff; 32],
            bind_addr: addr("[::]:0"),
        },
    );
    v.hello(
        "hello v4 specific",
        Hello {
            token,
            bind_addr: addr("192.168.1.20:4433"),
        },
    );
    v.bound(
        "bound v4 no ifaces",
        Bound {
            local: addr("0.0.0.0:51820"),
            max_gso_segments: 1,
            gro_segments: 1,
            interface_addrs: vec![],
        },
    );
    v.bound(
        "bound v6 dual stack",
        Bound {
            local: addr("[::]:40000"),
            max_gso_segments: 64,
            gro_segments: 64,
            interface_addrs: vec![
                addr("10.0.2.15:40000"),
                addr("[2001:db8::15]:40000"),
                addr("[fe80::a00:27ff:fe4e:66a1]:40000"),
            ],
        },
    );
    v.error(
        "error not allowed",
        ErrorFrame {
            code: ErrorCode::NotAllowed,
            message: "token expired",
        },
    );
    v.error(
        "error invalid input utf8",
        ErrorFrame {
            code: ErrorCode::InvalidInput,
            message: "forbidden destination \u{e9}\u{1f600}",
        },
    );
    v.error(
        "error io empty",
        ErrorFrame {
            code: ErrorCode::Io,
            message: "",
        },
    );
    v.data(
        "data single v4",
        &[Data {
            ecn: 0,
            peer: addr("1.2.3.4:443"),
            segment_size: 0,
            payload: b"hello",
        }],
    );
    v.data(
        "data single v6 ecn",
        &[Data {
            ecn: 2,
            peer: addr("[2001:db8::1]:4433"),
            segment_size: 0,
            payload: &[0, 1, 2, 0xff],
        }],
    );
    v.data(
        "data empty payload",
        &[Data {
            ecn: 0,
            peer: addr("127.0.0.1:9"),
            segment_size: 0,
            payload: b"",
        }],
    );
    v.data(
        "data gso",
        &[Data {
            ecn: 1,
            peer: addr("[::1]:7"),
            segment_size: 4,
            payload: b"0123456789",
        }],
    );
    let v6_forms = [
        "[::]:1",
        "[::1]:2",
        "[::ffff:1.2.3.4]:3",
        "[fe80::1:0:0:2]:4",
        "[1:0:0:2::3]:5",
        "[1::2:0:0:3:4]:6",
        "[1:2:3:4:5:6:7:0]:7",
        "[1:0:2:3:4:5:6:7]:8",
        "[abcd:ef01:2345:6789:abcd:ef01:2345:6789]:65535",
        "[::1.2.3.4]:9",
    ];
    let recs: Vec<_> = v6_forms
        .iter()
        .enumerate()
        .map(|(i, a)| Data {
            ecn: (i % 4) as u8,
            peer: addr(a),
            segment_size: 0,
            payload: &b"xyz"[..i % 4],
        })
        .collect();
    v.data("data multi ipv6 text forms", &recs);

    v.invalid("first empty", "invalidFirst", &[]);
    v.invalid("first unknown kind", "invalidFirst", &[0x42, 0]);
    v.invalid("first data kind", "invalidFirst", &[KIND_DATA]);
    v.invalid(
        "first truncated hello",
        "invalidFirst",
        &[KIND_HELLO, 1, 0, 0],
    );
    let mut bad_version = vec![KIND_HELLO, 2];
    bad_version.extend_from_slice(&[0; 32]);
    bad_version.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0]);
    v.invalid("first bad version", "invalidFirst", &bad_version);
    let mut bad_family = vec![KIND_HELLO, 1];
    bad_family.extend_from_slice(&[0; 32]);
    bad_family.extend_from_slice(&[5, 0, 0, 0, 0, 0, 0]);
    v.invalid("first bad family", "invalidFirst", &bad_family);
    v.invalid(
        "first trailing",
        "invalidFirst",
        &[KIND_ERROR, 1, 0, 1, b'x', 0],
    );
    v.invalid(
        "first unknown error code",
        "invalidFirst",
        &[KIND_ERROR, 6, 0, 0],
    );
    v.invalid(
        "first bad utf8",
        "invalidFirst",
        &[KIND_ERROR, 1, 0, 1, 0xff],
    );
    v.invalid("data empty", "invalidData", &[]);
    v.invalid(
        "data truncated payload",
        "invalidData",
        &[KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 5, b'a'],
    );
    v.invalid(
        "data bad ecn",
        "invalidData",
        &[KIND_DATA, 4, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0],
    );
    v.invalid(
        "data bad family",
        "invalidData",
        &[KIND_DATA, 0, 0, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0],
    );
    v.invalid(
        "data then unknown kind",
        "invalidData",
        &[KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0x77],
    );
    v.invalid(
        "data then hello",
        "invalidData",
        &[
            KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0, KIND_HELLO,
        ],
    );

    format!("[\n{}\n]\n", v.0.join(",\n"))
}

#[test]
fn vectors_json_up_to_date() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/vectors.json");
    let json = build();
    let update = std::env::var_os("UPDATE_VECTORS").is_some();
    match std::fs::read_to_string(&path) {
        Ok(existing) if !update => assert_eq!(
            existing, json,
            "testdata/vectors.json is stale, rerun with UPDATE_VECTORS=1"
        ),
        _ => {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, json).unwrap();
        }
    }
}
