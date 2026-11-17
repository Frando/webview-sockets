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
/// port if the URL has one (`app.localhost:8080`). Whatever the policy,
/// `Origin` must be `http://` or `https://` followed by the `Host` value.
#[derive(Clone)]
pub enum HostPolicy {
    /// Any host. Only for dev tools.
    Any,
    /// Hosts for which the function returns true.
    Only(Arc<dyn Fn(&str) -> bool + Send + Sync>),
    /// Hosts matching one of the patterns, see [`HostPolicy::patterns`].
    Patterns(Arc<[String]>),
}

impl HostPolicy {
    /// Returns a policy that allows hosts for which `allows` returns true.
    pub fn from_fn(allows: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        Self::Only(Arc::new(allows))
    }

    /// Returns a policy that allows exactly the given host authorities.
    pub fn allowlist(hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let hosts: BTreeSet<String> = hosts.into_iter().map(Into::into).collect();
        Self::from_fn(move |host| hosts.contains(host))
    }

    /// Returns a policy that allows hosts matching one of `patterns`.
    ///
    /// A pattern is a host authority in which `*` matches one or more
    /// characters other than `.` and `:`, e.g. `*.localhost:*` or
    /// `app.localhost:8080`. Matching is case sensitive, and browsers send
    /// lowercase hosts. Unlike the other policies, the patterns also gate the
    /// injected script, see [`crate::UdpHost::injected_js`].
    pub fn patterns(patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Patterns(patterns.into_iter().map(Into::into).collect())
    }

    /// Returns whether `host` (an authority, `name[:port]`) is allowed.
    pub fn allows(&self, host: &str) -> bool {
        !host.is_empty()
            && match self {
                Self::Any => true,
                Self::Only(allows) => allows(host),
                Self::Patterns(patterns) => {
                    patterns.iter().any(|pattern| host_matches(pattern, host))
                }
            }
    }
}

/// Returns whether `host` matches `pattern`.
///
/// A `*` in the pattern stands for one or more characters other than `.`
/// and `:`. Mirrors `hostMatches` in the injected script.
pub(crate) fn host_matches(pattern: &str, host: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == host,
        Some((head, tail)) => {
            let Some(rest) = host.strip_prefix(head) else {
                return false;
            };
            let label_len = rest.find(['.', ':']).unwrap_or(rest.len());
            (1..=label_len).any(|len| host_matches(tail, &rest[len..]))
        }
    }
}

impl fmt::Debug for HostPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => f.write_str("HostPolicy::Any"),
            Self::Only(_) => f.write_str("HostPolicy::Only(..)"),
            Self::Patterns(patterns) => f
                .debug_tuple("HostPolicy::Patterns")
                .field(patterns)
                .finish(),
        }
    }
}

/// Returns the host authority of an `http` or `https` origin.
fn origin_host(origin: &str) -> Option<&str> {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
}

/// Returns whether a bridge-reported origin may get a ticket.
pub(crate) fn origin_allowed(origin: &str, policy: &HostPolicy) -> bool {
    origin_host(origin).is_some_and(|host| policy.allows(host))
}

/// Checks the WebSocket upgrade headers and returns the origin.
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
        Some(origin_host) if origin_host == host => Ok(origin),
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

/// Checks a destination and returns it in canonical form.
///
/// An IPv4-mapped IPv6 address becomes an IPv4 address.
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
                && label.is_some_and(|label| {
                    !label.is_empty()
                        && label
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
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
        let gateway_policy = gateway();
        assert!(gateway_policy.allows("abc123.pkarr.localhost:45475"));
        assert!(!gateway_policy.allows("abc.pkarr.localhost"));
        assert!(!gateway_policy.allows("Abc.pkarr.localhost:80"));
        let patterns = HostPolicy::patterns(["*.localhost:*", "app.test", "a*b.x"]);
        assert!(patterns.allows("test.localhost:8080"));
        assert!(!patterns.allows("a.b.localhost:8080"));
        assert!(!patterns.allows(".localhost:8080"));
        assert!(!patterns.allows("test.localhost"));
        assert!(!patterns.allows("test.localhost:"));
        assert!(patterns.allows("app.test"));
        assert!(!patterns.allows("app.test:1"));
        assert!(patterns.allows("axyb.x"));
        assert!(!patterns.allows("ab.x"));
        assert!(!patterns.allows("a.b.x"));
    }

    #[test]
    fn origins() {
        let gateway_policy = gateway();
        assert!(origin_allowed(
            "http://a.pkarr.localhost:5",
            &gateway_policy
        ));
        assert!(origin_allowed(
            "https://a.pkarr.localhost:5",
            &gateway_policy
        ));
        assert!(!origin_allowed(
            "ftp://a.pkarr.localhost:5",
            &gateway_policy
        ));
        assert!(!origin_allowed("http://example.com", &gateway_policy));
        assert!(!origin_allowed("null", &HostPolicy::Any));
        assert!(!origin_allowed("http://", &HostPolicy::Any));
        assert!(origin_allowed("http://example.com", &HostPolicy::Any));
    }

    #[test]
    fn upgrade() {
        let gateway_policy = gateway();
        let host = Some("a.pkarr.localhost:5");
        assert!(check_upgrade(host, Some("http://a.pkarr.localhost:5"), &gateway_policy).is_ok());
        assert!(check_upgrade(host, Some("https://a.pkarr.localhost:5"), &gateway_policy).is_ok());
        assert!(check_upgrade(host, Some("http://b.pkarr.localhost:5"), &gateway_policy).is_err());
        assert!(check_upgrade(host, Some("ws://a.pkarr.localhost:5"), &gateway_policy).is_err());
        assert!(check_upgrade(host, None, &gateway_policy).is_err());
        assert!(check_upgrade(None, Some("http://a.pkarr.localhost:5"), &gateway_policy).is_err());
        let ip = Some("127.0.0.1:5");
        assert!(check_upgrade(ip, Some("http://127.0.0.1:5"), &gateway_policy).is_err());
        assert!(check_upgrade(ip, Some("http://127.0.0.1:5"), &HostPolicy::Any).is_ok());
    }

    #[test]
    fn destinations() {
        let check = |text: &str| check_destination(text.parse().unwrap());
        assert!(check("127.0.0.1:9").is_ok());
        assert!(check("[::1]:9").is_ok());
        assert_eq!(
            check("[::ffff:10.0.0.1]:9").unwrap(),
            "10.0.0.1:9".parse().unwrap()
        );
        assert!(check("10.0.0.1:0").is_err());
        assert!(check("0.0.0.0:9").is_err());
        assert!(check("[::]:9").is_err());
        assert!(check("224.0.0.1:9").is_err());
        assert!(check("[ff02::1]:9").is_err());
        assert!(check("255.255.255.255:9").is_err());
        assert!(check("[::ffff:255.255.255.255]:9").is_err());
    }

    #[test]
    fn binds() {
        let check = |text: &str| check_bind(text.parse().unwrap());
        assert!(check("0.0.0.0:0").is_ok());
        assert!(check("[::]:4433").is_ok());
        assert!(check("0.0.0.0:80").is_err());
        assert!(check("224.0.0.1:0").is_err());
    }
}
