//! Host, origin, bind and destination checks.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// Returns whether `host` is `<label>.(blake3|pkarr).localhost:<port>`.
///
/// `label` is `[a-z0-9]+`, `port` a decimal u16.
pub(crate) fn is_gateway_host(host: &str) -> bool {
    let Some((name, port)) = host.rsplit_once(':') else {
        return false;
    };
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err()
    {
        return false;
    }
    let Some(rest) = name
        .strip_suffix(".blake3.localhost")
        .or_else(|| name.strip_suffix(".pkarr.localhost"))
    else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// Returns whether a bridge-reported origin may get a ticket.
pub(crate) fn origin_ok(origin: &str, check_host: bool) -> bool {
    match origin.strip_prefix("http://") {
        Some(host) if check_host => is_gateway_host(host),
        Some(host) => !host.is_empty(),
        None => false,
    }
}

/// Checks the WebSocket upgrade headers. Returns the origin on success.
pub(crate) fn check_upgrade<'a>(
    host: Option<&str>,
    origin: Option<&'a str>,
    check_host: bool,
) -> Result<&'a str, &'static str> {
    let host = host.ok_or("missing Host")?;
    let origin = origin.ok_or("missing Origin")?;
    if check_host && !is_gateway_host(host) {
        return Err("Host is not a gateway host");
    }
    match origin.strip_prefix("http://") {
        Some(o) if o == host => Ok(origin),
        _ => Err("Origin does not match Host"),
    }
}

/// Checks a requested bind address.
pub(crate) fn check_bind(addr: SocketAddr) -> Result<(), &'static str> {
    if addr.port() != 0 && addr.port() < 1024 {
        return Err("bind port must be 0 or >= 1024");
    }
    let ip = addr.ip().to_canonical();
    if ip.is_multicast() || is_broadcast(ip) {
        return Err("bind address must be unspecified or a local interface address");
    }
    Ok(())
}

/// Checks a destination and returns it in canonical form (IPv4-mapped IPv6
/// becomes IPv4).
pub(crate) fn check_destination(addr: SocketAddr) -> Result<SocketAddr, &'static str> {
    let ip = addr.ip().to_canonical();
    if addr.port() == 0 {
        return Err("destination port 0 is not allowed");
    }
    if ip.is_unspecified() {
        return Err("unspecified destination address is not allowed");
    }
    if ip.is_multicast() {
        return Err("multicast destinations are not allowed");
    }
    if is_broadcast(ip) {
        return Err("broadcast destinations are not allowed");
    }
    Ok(SocketAddr::new(ip, addr.port()))
}

fn is_broadcast(ip: IpAddr) -> bool {
    ip == IpAddr::V4(Ipv4Addr::BROADCAST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_host() {
        assert!(is_gateway_host("abc123.pkarr.localhost:45475"));
        assert!(is_gateway_host("x.blake3.localhost:1"));
        assert!(!is_gateway_host("abc.pkarr.localhost"));
        assert!(!is_gateway_host("abc.pkarr.localhost:"));
        assert!(!is_gateway_host("abc.pkarr.localhost:99999"));
        assert!(!is_gateway_host("abc.pkarr.localhost:+80"));
        assert!(!is_gateway_host(".pkarr.localhost:80"));
        assert!(!is_gateway_host("Abc.pkarr.localhost:80"));
        assert!(!is_gateway_host("a.b.pkarr.localhost:80"));
        assert!(!is_gateway_host("a-b.pkarr.localhost:80"));
        assert!(!is_gateway_host("a.other.localhost:80"));
        assert!(!is_gateway_host("a.pkarr.localhost.evil.com:80"));
        assert!(!is_gateway_host("127.0.0.1:80"));
    }

    #[test]
    fn upgrade() {
        let h = Some("a.pkarr.localhost:5");
        assert!(check_upgrade(h, Some("http://a.pkarr.localhost:5"), true).is_ok());
        assert!(check_upgrade(h, Some("http://b.pkarr.localhost:5"), true).is_err());
        assert!(check_upgrade(h, Some("https://a.pkarr.localhost:5"), true).is_err());
        assert!(check_upgrade(h, None, true).is_err());
        assert!(check_upgrade(None, Some("http://a.pkarr.localhost:5"), true).is_err());
        assert!(check_upgrade(Some("127.0.0.1:5"), Some("http://127.0.0.1:5"), true).is_err());
        assert!(check_upgrade(Some("127.0.0.1:5"), Some("http://127.0.0.1:5"), false).is_ok());
    }

    #[test]
    fn destinations() {
        let ok = |s: &str| check_destination(s.parse().unwrap());
        assert!(ok("127.0.0.1:9").is_ok());
        assert!(ok("[::1]:9").is_ok());
        assert_eq!(
            ok("[::ffff:10.0.0.1]:9").unwrap(),
            "10.0.0.1:9".parse().unwrap()
        );
        assert!(ok("10.0.0.1:0").is_err());
        assert!(ok("0.0.0.0:9").is_err());
        assert!(ok("[::]:9").is_err());
        assert!(ok("224.0.0.1:9").is_err());
        assert!(ok("[ff02::1]:9").is_err());
        assert!(ok("255.255.255.255:9").is_err());
        assert!(ok("[::ffff:255.255.255.255]:9").is_err());
    }

    #[test]
    fn binds() {
        let ok = |s: &str| check_bind(s.parse().unwrap());
        assert!(ok("0.0.0.0:0").is_ok());
        assert!(ok("[::]:4433").is_ok());
        assert!(ok("0.0.0.0:80").is_err());
        assert!(ok("224.0.0.1:0").is_err());
    }
}
