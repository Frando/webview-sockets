//! Cross-language test vectors shared with `js/codec.test.mjs`.
//!
//! Checks that `testdata/vectors.json` matches what this crate encodes. Run
//! with `UPDATE_VECTORS=1` to rewrite the file (it is also written if
//! missing).

use std::{fmt::Write as _, net::SocketAddr, path::PathBuf};

use webview_sockets_proto::*;

fn addr(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn json_str(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ch if (ch as u32) < 0x20 || (ch as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    write!(out, "\\u{unit:04x}").unwrap();
                }
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn json_dns(dns: Option<DnsQueryType>) -> String {
    dns.map_or("null".to_string(), |dns| format!("\"{}\"", dns.name()))
}

fn json_data(data: &Data<'_>) -> String {
    format!(
        "{{\"ecn\": {}, \"address\": {}, \"port\": {}, \"segmentSize\": {}, \"payload\": \"{}\"}}",
        data.ecn,
        json_str(&data.peer.ip().to_string()),
        data.peer.port(),
        data.segment_size,
        hex(data.payload)
    )
}

fn json_addr(addr: &SocketAddr) -> String {
    format!(
        "{{\"address\": {}, \"port\": {}}}",
        json_str(&addr.ip().to_string()),
        addr.port()
    )
}

struct Vectors(Vec<String>);

impl Vectors {
    fn push(&mut self, name: &str, kind: &str, bytes: &[u8], expected: String) {
        self.0.push(format!(
            "  {{\"name\": {}, \"type\": \"{kind}\", \"hex\": \"{}\", \"expected\": {expected}}}",
            json_str(name),
            hex(bytes)
        ));
    }

    fn hello(&mut self, name: &str, hello: Hello<'_>) {
        let mut buf = Vec::new();
        encode_hello(&hello, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Hello(hello));
        let mode = match hello.mode {
            Mode::Bound { local, ipv6_only } => format!(
                "\"mode\": \"bound\", \"localAddress\": {}, \"localPort\": {}, \"ipv6Only\": {}",
                json_str(&local.ip().to_string()),
                local.port(),
                ipv6_only.map_or("null".to_string(), |only| only.to_string())
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
        let expected = format!(
            "{{\"kind\": \"hello\", \"version\": {VERSION}, \"token\": \"{}\", \"sendBufferSize\": {}, \"receiveBufferSize\": {}, {mode}}}",
            hex(&hello.token),
            hello.send_buffer_size,
            hello.receive_buffer_size,
        );
        self.push(name, "first", &buf, expected);
    }

    fn bound(&mut self, name: &str, bound: Bound) {
        let mut buf = Vec::new();
        encode_bound(&bound, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Bound(bound.clone()));
        let ifaces: Vec<_> = bound.interface_addrs.iter().map(json_addr).collect();
        let (remote_address, remote_port) = match bound.remote {
            Some(remote) => (
                json_str(&remote.ip().to_string()),
                remote.port().to_string(),
            ),
            None => ("null".to_string(), "null".to_string()),
        };
        let expected = format!(
            "{{\"kind\": \"bound\", \"localAddress\": {}, \"localPort\": {}, \"remoteAddress\": {remote_address}, \"remotePort\": {remote_port}, \"maxGsoSegments\": {}, \"groSegments\": {}, \"interfaceAddrs\": [{}]}}",
            json_str(&bound.local.ip().to_string()),
            bound.local.port(),
            bound.max_gso_segments,
            bound.gro_segments,
            ifaces.join(", ")
        );
        self.push(name, "first", &buf, expected);
    }

    fn error(&mut self, name: &str, error: ErrorFrame<'_>) {
        let mut buf = Vec::new();
        encode_error(&error, &mut buf);
        assert_eq!(decode_first(&buf).unwrap(), Frame::Error(error));
        let expected = format!(
            "{{\"kind\": \"error\", \"code\": {}, \"codeName\": \"{}\", \"message\": {}}}",
            error.code as u8,
            error.code.name(),
            json_str(error.message)
        );
        self.push(name, "first", &buf, expected);
    }

    fn data(&mut self, name: &str, records: &[Data<'_>]) {
        let mut buf = Vec::new();
        for record in records {
            encode_data(record, &mut buf);
        }
        let decoded: Vec<_> = decode_data(&buf)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded, records);
        let items: Vec<_> = records.iter().map(json_data).collect();
        self.push(name, "data", &buf, format!("[{}]", items.join(", ")));
    }

    /// Page-to-host messages that may hold DATA_NAMED records.
    fn records(&mut self, name: &str, records: &[Record<'_>]) {
        let mut buf = Vec::new();
        for record in records {
            match record {
                Record::Data(data) => encode_data(data, &mut buf),
                Record::Named(named) => encode_named_data(named, &mut buf),
            }
        }
        let decoded: Vec<_> = decode_records(&buf)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded, records);
        let items: Vec<_> = records
            .iter()
            .map(|record| match record {
                Record::Data(data) => json_data(data),
                Record::Named(named) => format!(
                    "{{\"ecn\": {}, \"address\": {}, \"port\": {}, \"dnsQueryType\": {}, \"segmentSize\": {}, \"payload\": \"{}\"}}",
                    named.ecn,
                    json_str(named.host),
                    named.port,
                    json_dns(named.dns_query_type),
                    named.segment_size,
                    hex(named.payload)
                ),
            })
            .collect();
        self.push(name, "records", &buf, format!("[{}]", items.join(", ")));
    }

    fn invalid(&mut self, name: &str, kind: &str, bytes: &[u8]) {
        let err = match kind {
            "invalidFirst" => decode_first(bytes).unwrap_err(),
            "invalidData" => match decode_records(bytes) {
                Err(err) => err,
                Ok(iter) => iter
                    .filter_map(Result::err)
                    .next()
                    .expect("vector must be invalid"),
            },
            _ => unreachable!(),
        };
        self.push(
            name,
            kind,
            bytes,
            format!("{{\"error\": \"{}\"}}", err.name()),
        );
    }
}

fn build() -> String {
    let mut vectors = Vectors(Vec::new());
    let mut token = [0u8; TOKEN_LEN];
    for (index, byte) in token.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(0xa0);
    }
    vectors.hello("hello v4 any", Hello::bind(token, addr("0.0.0.0:0")));
    vectors.hello(
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
    vectors.hello(
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
    vectors.hello(
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
    vectors.hello(
        "hello connected name",
        Hello::connect(token, "example.com", 443),
    );
    vectors.hello(
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
    vectors.hello(
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
    vectors.hello(
        "hello connected ipv6 literal",
        Hello::connect(token, "2001:db8::1", 4433),
    );
    vectors.bound(
        "bound v4 no ifaces",
        Bound {
            local: addr("0.0.0.0:51820"),
            remote: None,
            max_gso_segments: 1,
            gro_segments: 1,
            interface_addrs: vec![],
        },
    );
    vectors.bound(
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
    vectors.bound(
        "bound connected",
        Bound {
            local: addr("192.168.1.20:40001"),
            remote: Some(addr("[2001:db8::1]:443")),
            max_gso_segments: 1,
            gro_segments: 1,
            interface_addrs: vec![addr("192.168.1.20:40001")],
        },
    );
    vectors.error(
        "error not allowed",
        ErrorFrame {
            code: ErrorCode::NotAllowed,
            message: "token expired",
        },
    );
    vectors.error(
        "error invalid input utf8",
        ErrorFrame {
            code: ErrorCode::InvalidInput,
            message: "forbidden destination \u{e9}\u{1f600}",
        },
    );
    vectors.error(
        "error io empty",
        ErrorFrame {
            code: ErrorCode::Io,
            message: "",
        },
    );
    vectors.data(
        "data single v4",
        &[Data {
            ecn: 0,
            peer: addr("1.2.3.4:443"),
            segment_size: 0,
            payload: b"hello",
        }],
    );
    vectors.data(
        "data single v6 ecn",
        &[Data {
            ecn: 2,
            peer: addr("[2001:db8::1]:4433"),
            segment_size: 0,
            payload: &[0, 1, 2, 0xff],
        }],
    );
    vectors.data(
        "data empty payload",
        &[Data {
            ecn: 0,
            peer: addr("127.0.0.1:9"),
            segment_size: 0,
            payload: b"",
        }],
    );
    vectors.data(
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
    let records: Vec<_> = v6_forms
        .iter()
        .enumerate()
        .map(|(index, text)| Data {
            ecn: (index % 4) as u8,
            peer: addr(text),
            segment_size: 0,
            payload: &b"xyz"[..index % 4],
        })
        .collect();
    vectors.data("data multi ipv6 text forms", &records);
    vectors.error(
        "error name not resolved",
        ErrorFrame {
            code: ErrorCode::NameNotResolved,
            message: "no such host: nx.example",
        },
    );
    vectors.records(
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
    vectors.records(
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

    vectors.invalid("first empty", "invalidFirst", &[]);
    vectors.invalid("first unknown kind", "invalidFirst", &[0x42, 0]);
    vectors.invalid("first data kind", "invalidFirst", &[KIND_DATA]);
    vectors.invalid(
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
    vectors.invalid("first bad version", "invalidFirst", &bad_version);
    let mut bad_family = hello_head(VERSION);
    bad_family.extend_from_slice(&[0, 5, 0, 0, 0, 0, 0, 0, 0]);
    vectors.invalid("first bad family", "invalidFirst", &bad_family);
    let mut bad_mode = hello_head(VERSION);
    bad_mode.extend_from_slice(&[2, 4, 0, 0, 0, 0, 0, 0, 0]);
    vectors.invalid("first unknown mode", "invalidFirst", &bad_mode);
    let mut bad_v6only = hello_head(VERSION);
    bad_v6only.extend_from_slice(&[0, 4, 0, 0, 0, 0, 0, 0, 3]);
    vectors.invalid("first bad ipv6only flag", "invalidFirst", &bad_v6only);
    let mut bad_dns = hello_head(VERSION);
    bad_dns.extend_from_slice(&[1, 5, 1, b'a', 0, 1]);
    vectors.invalid("first bad dns query type", "invalidFirst", &bad_dns);
    let mut bad_host = hello_head(VERSION);
    bad_host.extend_from_slice(&[1, 0, 1, 0xff, 0, 1]);
    vectors.invalid("first host not utf8", "invalidFirst", &bad_host);
    let mut truncated_host = hello_head(VERSION);
    truncated_host.extend_from_slice(&[1, 0, 5, b'a']);
    vectors.invalid("first truncated host", "invalidFirst", &truncated_host);
    vectors.invalid(
        "first bad bound remote flag",
        "invalidFirst",
        &[KIND_BOUND, 4, 0, 0, 0, 0, 0, 1, 2, 0, 1, 0, 1, 0],
    );
    vectors.invalid(
        "first trailing",
        "invalidFirst",
        &[KIND_ERROR, 1, 0, 1, b'x', 0],
    );
    vectors.invalid(
        "first unknown error code",
        "invalidFirst",
        &[KIND_ERROR, 7, 0, 0],
    );
    vectors.invalid(
        "first bad utf8",
        "invalidFirst",
        &[KIND_ERROR, 1, 0, 1, 0xff],
    );
    vectors.invalid("data empty", "invalidData", &[]);
    vectors.invalid(
        "data truncated payload",
        "invalidData",
        &[KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 5, b'a'],
    );
    vectors.invalid(
        "data bad ecn",
        "invalidData",
        &[KIND_DATA, 4, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0],
    );
    vectors.invalid(
        "data bad family",
        "invalidData",
        &[KIND_DATA, 0, 0, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0],
    );
    vectors.invalid(
        "data then unknown kind",
        "invalidData",
        &[KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0, 0x77],
    );
    vectors.invalid(
        "named bad dns query type",
        "invalidData",
        &[KIND_DATA_NAMED, 0, 9, 1, b'a', 0, 1, 0, 0, 0, 0, 0, 0],
    );
    vectors.invalid(
        "named truncated",
        "invalidData",
        &[KIND_DATA_NAMED, 0, 0, 3, b'a'],
    );
    vectors.invalid(
        "data then hello",
        "invalidData",
        &[
            KIND_DATA, 0, 4, 1, 2, 3, 4, 0, 1, 0, 0, 0, 0, 0, 0, KIND_HELLO,
        ],
    );

    format!("[\n{}\n]\n", vectors.0.join(",\n"))
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
