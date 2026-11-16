//! Cross-language test vectors shared with `js/codec.test.mjs`.
//!
//! Checks that `testdata/vectors.json` matches what this crate encodes. Run
//! with `UPDATE_VECTORS=1` to rewrite the file (it is also written if
//! missing).

use std::{fmt::Write as _, net::SocketAddr, path::PathBuf};

use webview_sockets_proto::*;

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

fn json_dns(d: Option<DnsQueryType>) -> String {
    d.map_or("null".to_string(), |d| format!("\"{}\"", d.name()))
}

fn json_data(r: &Data<'_>) -> String {
    format!(
        "{{\"ecn\": {}, \"address\": {}, \"port\": {}, \"segmentSize\": {}, \"payload\": \"{}\"}}",
        r.ecn,
        json_str(&r.peer.ip().to_string()),
        r.peer.port(),
        r.segment_size,
        hex(r.payload)
    )
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

    fn hello(&mut self, name: &str, h: Hello<'_>) {
        let mut buf = Vec::new();
        encode_hello(&h, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Hello(h));
        let mode = match h.mode {
            Mode::Bound { local, ipv6_only } => format!(
                "\"mode\": \"bound\", \"localAddress\": {}, \"localPort\": {}, \"ipv6Only\": {}",
                json_str(&local.ip().to_string()),
                local.port(),
                ipv6_only.map_or("null".to_string(), |b| b.to_string())
            ),
            Mode::Connected {
                remote_host,
                remote_port,
                dns_query_type,
            } => format!(
                "\"mode\": \"connected\", \"remoteAddress\": {}, \"remotePort\": {remote_port}, \"dnsQueryType\": {}",
                json_str(remote_host),
                json_dns(dns_query_type)
            ),
        };
        let exp = format!(
            "{{\"kind\": \"hello\", \"version\": {VERSION}, \"token\": \"{}\", \"sendBufferSize\": {}, \"receiveBufferSize\": {}, {mode}}}",
            hex(&h.token),
            h.send_buffer_size,
            h.receive_buffer_size,
        );
        self.push(name, "first", &buf, exp);
    }

    fn bound(&mut self, name: &str, b: Bound) {
        let mut buf = Vec::new();
        encode_bound(&b, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(b.clone()));
        let ifaces: Vec<_> = b.interface_addrs.iter().map(json_addr).collect();
        let (remote_address, remote_port) = match b.remote {
            Some(r) => (json_str(&r.ip().to_string()), r.port().to_string()),
            None => ("null".to_string(), "null".to_string()),
        };
        let exp = format!(
            "{{\"kind\": \"bound\", \"localAddress\": {}, \"localPort\": {}, \"remoteAddress\": {remote_address}, \"remotePort\": {remote_port}, \"maxGsoSegments\": {}, \"groSegments\": {}, \"interfaceAddrs\": [{}]}}",
            json_str(&b.local.ip().to_string()),
            b.local.port(),
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
        let items: Vec<_> = recs.iter().map(json_data).collect();
        self.push(name, "data", &buf, format!("[{}]", items.join(", ")));
    }

    /// Page-to-host messages that may hold DATA_NAMED records.
    fn records(&mut self, name: &str, recs: &[Record<'_>]) {
        let mut buf = Vec::new();
        for r in recs {
            match r {
                Record::Data(d) => encode_data(d, &mut buf),
                Record::Named(n) => encode_named_data(n, &mut buf),
            }
        }
        let decoded: Vec<_> = decode_records(&buf)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded, recs);
        let items: Vec<_> = recs
            .iter()
            .map(|r| match r {
                Record::Data(d) => json_data(d),
                Record::Named(n) => format!(
                    "{{\"ecn\": {}, \"address\": {}, \"port\": {}, \"dnsQueryType\": {}, \"segmentSize\": {}, \"payload\": \"{}\"}}",
                    n.ecn,
                    json_str(n.host),
                    n.port,
                    json_dns(n.dns_query_type),
                    n.segment_size,
                    hex(n.payload)
                ),
            })
            .collect();
        self.push(name, "records", &buf, format!("[{}]", items.join(", ")));
    }

    fn invalid(&mut self, name: &str, ty: &str, bytes: &[u8]) {
        let err = match ty {
            "invalidFirst" => decode_first(bytes).unwrap_err(),
            "invalidData" => match decode_records(bytes) {
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
    v.hello("hello v4 any", Hello::bind(token, addr("0.0.0.0:0")));
    v.hello(
        "hello v6 any v6only",
        Hello {
            token: [0xff; 32],
            mode: Mode::Bound {
                local: addr("[::]:0"),
                ipv6_only: Some(true),
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        },
    );
    v.hello(
        "hello v6 any dual stack",
        Hello {
            token,
            mode: Mode::Bound {
                local: addr("[::]:0"),
                ipv6_only: Some(false),
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        },
    );
    v.hello(
        "hello v4 specific buffers",
        Hello {
            token,
            mode: Mode::Bound {
                local: addr("192.168.1.20:4433"),
                ipv6_only: None,
            },
            send_buffer_size: 1 << 20,
            receive_buffer_size: 4 << 20,
        },
    );
    v.hello(
        "hello connected name",
        Hello::connect(token, "example.com", 443),
    );
    v.hello(
        "hello connected name ipv6",
        Hello {
            token,
            mode: Mode::Connected {
                remote_host: "dns.example",
                remote_port: 53,
                dns_query_type: Some(DnsQueryType::Ipv6),
            },
            send_buffer_size: 65536,
            receive_buffer_size: 0,
        },
    );
    v.hello(
        "hello connected ip literal ipv4",
        Hello {
            token,
            mode: Mode::Connected {
                remote_host: "10.0.0.1",
                remote_port: 9,
                dns_query_type: Some(DnsQueryType::Ipv4),
            },
            send_buffer_size: 0,
            receive_buffer_size: 0,
        },
    );
    v.hello(
        "hello connected ipv6 literal",
        Hello::connect(token, "2001:db8::1", 4433),
    );
    v.bound(
        "bound v4 no ifaces",
        Bound {
            local: addr("0.0.0.0:51820"),
            remote: None,
            max_gso_segments: 1,
            gro_segments: 1,
            interface_addrs: vec![],
        },
    );
    v.bound(
        "bound v6 dual stack",
        Bound {
            local: addr("[::]:40000"),
            remote: None,
            max_gso_segments: 64,
            gro_segments: 64,
            interface_addrs: vec![
                addr("10.0.2.15:40000"),
                addr("[2001:db8::15]:40000"),
                addr("[fe80::a00:27ff:fe4e:66a1]:40000"),
            ],
        },
    );
    v.bound(
        "bound connected",
        Bound {
            local: addr("192.168.1.20:40001"),
            remote: Some(addr("[2001:db8::1]:443")),
            max_gso_segments: 1,
            gro_segments: 1,
            interface_addrs: vec![addr("192.168.1.20:40001")],
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
    v.error(
        "error name not resolved",
        ErrorFrame {
            code: ErrorCode::NameNotResolved,
            message: "no such host: nx.example",
        },
    );
    v.records(
        "records named",
        &[Record::Named(NamedData {
            ecn: 0,
            host: "example.com",
            port: 53,
            dns_query_type: None,
            segment_size: 0,
            payload: b"q",
        })],
    );
    v.records(
        "records named gso and data",
        &[
            Record::Named(NamedData {
                ecn: 1,
                host: "relay.example",
                port: 7842,
                dns_query_type: Some(DnsQueryType::Ipv4),
                segment_size: 3,
                payload: b"abcdefg",
            }),
            Record::Data(Data {
                ecn: 0,
                peer: addr("1.2.3.4:5"),
                segment_size: 0,
                payload: b"x",
            }),
            Record::Named(NamedData {
                ecn: 3,
                host: "v6.example",
                port: 1,
                dns_query_type: Some(DnsQueryType::Ipv6),
                segment_size: 0,
                payload: b"",
            }),
        ],
    );

    v.invalid("first empty", "invalidFirst", &[]);
    v.invalid("first unknown kind", "invalidFirst", &[0x42, 0]);
    v.invalid("first data kind", "invalidFirst", &[KIND_DATA]);
    v.invalid(
        "first truncated hello",
        "invalidFirst",
        &[KIND_HELLO, VERSION, 0, 0],
    );
    let hello_head = |version: u8| {
        let mut out = vec![KIND_HELLO, version];
        out.extend_from_slice(&[0; 32 + 8]);
        out
    };
    let mut bad_version = hello_head(1);
    bad_version.extend_from_slice(&[0, 4, 0, 0, 0, 0, 0, 0, 0]);
    v.invalid("first bad version", "invalidFirst", &bad_version);
    let mut bad_family = hello_head(VERSION);
    bad_family.extend_from_slice(&[0, 5, 0, 0, 0, 0, 0, 0, 0]);
    v.invalid("first bad family", "invalidFirst", &bad_family);
    let mut bad_mode = hello_head(VERSION);
    bad_mode.extend_from_slice(&[2, 4, 0, 0, 0, 0, 0, 0, 0]);
    v.invalid("first unknown mode", "invalidFirst", &bad_mode);
    let mut bad_v6only = hello_head(VERSION);
    bad_v6only.extend_from_slice(&[0, 4, 0, 0, 0, 0, 0, 0, 3]);
    v.invalid("first bad ipv6only flag", "invalidFirst", &bad_v6only);
    let mut bad_dns = hello_head(VERSION);
    bad_dns.extend_from_slice(&[1, 5, 1, b'a', 0, 1]);
    v.invalid("first bad dns query type", "invalidFirst", &bad_dns);
    let mut bad_host = hello_head(VERSION);
    bad_host.extend_from_slice(&[1, 0, 1, 0xff, 0, 1]);
    v.invalid("first host not utf8", "invalidFirst", &bad_host);
    let mut truncated_host = hello_head(VERSION);
    truncated_host.extend_from_slice(&[1, 0, 5, b'a']);
    v.invalid("first truncated host", "invalidFirst", &truncated_host);
    v.invalid(
        "first bad bound remote flag",
        "invalidFirst",
        &[KIND_BOUND, 4, 0, 0, 0, 0, 0, 1, 2, 0, 1, 0, 1, 0],
    );
    v.invalid(
        "first trailing",
        "invalidFirst",
        &[KIND_ERROR, 1, 0, 1, b'x', 0],
    );
    v.invalid(
        "first unknown error code",
        "invalidFirst",
        &[KIND_ERROR, 7, 0, 0],
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
        "named bad dns query type",
        "invalidData",
        &[KIND_DATA_NAMED, 0, 9, 1, b'a', 0, 1, 0, 0, 0, 0, 0, 0],
    );
    v.invalid(
        "named truncated",
        "invalidData",
        &[KIND_DATA_NAMED, 0, 0, 3, b'a'],
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
