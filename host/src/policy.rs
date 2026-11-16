//! Host, origin, bind and destination checks.

use std::{
    collections::BTreeSet,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

/// Decides which page hosts may use sockets.
///
/// The host app passes its own rule, e.g. "only my gateway origins". The
/// checked value is the `Host` authority as sent by the browser, with the
/// port if the URL has one (`app.localhost:8080`). Independent of the policy,
/// `Origin` must equal `"http://" + Host` or `"https://" + Host`.
#[derive(Clone)]
pub enum HostPolicy {
    /// Any host. Only for dev tools.
    Any,
    /// Hosts for which the function returns true.
    Only(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl HostPolicy {
    /// Returns a policy that allows hosts for which `f` returns true.
    pub fn from_fn(f: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self::Only(Arc::new(f))
    }

    /// Returns a policy that allows exactly the given host authorities.
    pub fn allowlist(hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let hosts: BTreeSet<String> = hosts.into_iter().map(Into::into).collect();
        Self::from_fn(move |host| hosts.contains(host))
    }

    /// Returns whether `host` (an authority, `name[:port]`) is allowed.
    pub fn allows(&self, host: &str) -> bool {
        !host.is_empty()
            && match self {
                Self::Any => true,
                Self::Only(f) => f(host),
            }
    }
}

impl fmt::Debug for HostPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("HostPolicy::Any"),
            Self::Only(_) => f.write_str("HostPolicy::Only(..)"),
        }
    }
}

/// Splits an origin into its host authority, accepting `http` and `https`.
fn origin_host(origin: &str) -> Option<&str> {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
}

/// Returns whether a bridge-reported origin may get a ticket.
pub(crate) fn origin_ok(origin: &str, policy: &HostPolicy) -> bool {
    origin_host(origin).is_some_and(|host| policy.allows(host))
}

/// Checks the WebSocket upgrade headers. Returns the origin on success.
pub(crate) fn check_upgrade<'a>(
    host: Option<&str>,
    origin: Option<&'a str>,
    policy: &HostPolicy,
) -> Result<&'a str, &'static str> {
    let host = host.ok_or("missing Host")?;
    let origin = origin.ok_or("missing Origin")?;
    if !policy.allows(host) {
        return Err("Host is not allowed");
    }
    match origin_host(origin) {
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

    /// The pattern iroh-link uses: `<label>.(blake3|pkarr).localhost:<port>`.
    fn gateway() -> HostPolicy {
        HostPolicy::from_fn(|host| {
            let Some((name, port)) = host.rsplit_once(':') else {
                return false;
            };
            let label = name
                .strip_suffix(".blake3.localhost")
                .or_else(|| name.strip_suffix(".pkarr.localhost"));
            port.parse::<u16>().is_ok()
                && label.is_some_and(|l| {
                    !l.is_empty()
                        && l.bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                })
        })
    }

    #[test]
    fn policies() {
        assert!(HostPolicy::Any.allows("anything:1"));
        assert!(!HostPolicy::Any.allows(""));
        let list = HostPolicy::allowlist(["a.localhost:5", "b.localhost"]);
        assert!(list.allows("a.localhost:5"));
        assert!(list.allows("b.localhost"));
        assert!(!list.allows("a.localhost:6"));
        assert!(!list.allows("a.localhost"));
        let g = gateway();
        assert!(g.allows("abc123.pkarr.localhost:45475"));
        assert!(!g.allows("abc.pkarr.localhost"));
        assert!(!g.allows("Abc.pkarr.localhost:80"));
    }

    #[test]
    fn origins() {
        let g = gateway();
        assert!(origin_ok("http://a.pkarr.localhost:5", &g));
        assert!(origin_ok("https://a.pkarr.localhost:5", &g));
        assert!(!origin_ok("ftp://a.pkarr.localhost:5", &g));
        assert!(!origin_ok("http://example.com", &g));
        assert!(!origin_ok("null", &HostPolicy::Any));
        assert!(!origin_ok("http://", &HostPolicy::Any));
        assert!(origin_ok("http://example.com", &HostPolicy::Any));
    }

    #[test]
    fn upgrade() {
        let g = gateway();
        let h = Some("a.pkarr.localhost:5");
        assert!(check_upgrade(h, Some("http://a.pkarr.localhost:5"), &g).is_ok());
        assert!(check_upgrade(h, Some("https://a.pkarr.localhost:5"), &g).is_ok());
        assert!(check_upgrade(h, Some("http://b.pkarr.localhost:5"), &g).is_err());
        assert!(check_upgrade(h, Some("ws://a.pkarr.localhost:5"), &g).is_err());
        assert!(check_upgrade(h, None, &g).is_err());
        assert!(check_upgrade(None, Some("http://a.pkarr.localhost:5"), &g).is_err());
        let ip = Some("127.0.0.1:5");
        assert!(check_upgrade(ip, Some("http://127.0.0.1:5"), &g).is_err());
        assert!(check_upgrade(ip, Some("http://127.0.0.1:5"), &HostPolicy::Any).is_ok());
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
