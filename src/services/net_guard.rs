//! SSRF guard for worker-controlled egress (`fetch()` and WebSocket).
//!
//! A literal IP in the URL bypasses the DNS resolver, so two layers are needed:
//! `guard_url_host` rejects literal non-public IPs, and `FilteringResolver`
//! rejects hostnames that resolve to one (judged at connection time, which also
//! defeats DNS rebinding). Both use `ip_is_public`.
//!
//! `FETCH_ALLOW_PRIVATE_NETWORK=1` disables the guard for local development.

use once_cell::sync::Lazy;
use reqwest::dns::Addrs;
use reqwest::dns::Name;
use reqwest::dns::Resolve;
use reqwest::dns::Resolving;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;

/// Whether worker code may reach non-public IP ranges.
///
/// Off by default. Set `FETCH_ALLOW_PRIVATE_NETWORK=1` for local development,
/// where a worker legitimately fetches `http://localhost:3000`.
pub static ALLOW_PRIVATE_NETWORK: Lazy<bool> = Lazy::new(|| {
    std::env::var("FETCH_ALLOW_PRIVATE_NETWORK")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
});

/// Egress policy applied to a single outbound request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EgressPolicy {
    /// Runner-constructed or operator-configured target: internal worker
    /// routing, storage bindings, worker-to-worker. Not filtered.
    Unrestricted,

    /// Worker-controlled URL. Non-public IPs are rejected.
    PublicOnly,
}

/// Return true when `ip` is a globally routable address.
///
/// `IpAddr::is_global` is still unstable, so the predicate is spelled out.
/// IPv4-mapped IPv6 addresses are unwrapped and judged as IPv4, otherwise
/// `http://[::ffff:127.0.0.1]/` would slip past the IPv6 checks.
pub fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => ipv4_is_public(v4),
            None => ipv6_is_public(v6),
        },
    }
}

fn ipv4_is_public(ip: Ipv4Addr) -> bool {
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
    {
        return false;
    }

    let [a, b, _, _] = ip.octets();

    // Shared address space / CGNAT: 100.64.0.0/10 (also Alibaba metadata).
    if a == 100 && (64..=127).contains(&b) {
        return false;
    }

    // IETF protocol assignments: 192.0.0.0/24.
    if a == 192 && b == 0 && ip.octets()[2] == 0 {
        return false;
    }

    // Benchmarking: 198.18.0.0/15.
    if a == 198 && (b == 18 || b == 19) {
        return false;
    }

    // Reserved: 240.0.0.0/4 (broadcast already rejected above).
    if a >= 240 {
        return false;
    }

    true
}

fn ipv6_is_public(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }

    let first = ip.segments()[0];

    // Unique local addresses: fc00::/7.
    if (first & 0xfe00) == 0xfc00 {
        return false;
    }

    // Link-local unicast: fe80::/10.
    if (first & 0xffc0) == 0xfe80 {
        return false;
    }

    // Documentation: 2001:db8::/32.
    if first == 0x2001 && ip.segments()[1] == 0x0db8 {
        return false;
    }

    true
}

fn blocked_msg(host: &str) -> String {
    format!("fetch to non-public address is not allowed: {}", host)
}

/// Reject a URL whose host is a literal non-public IP.
///
/// A literal IP bypasses the DNS resolver, so it is checked here. Hostnames
/// pass through and are judged by `FilteringResolver` at resolution time.
pub fn guard_url_host(url: &str) -> Result<(), String> {
    if *ALLOW_PRIVATE_NETWORK {
        return Ok(());
    }

    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {}", e))?;

    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if !ip_is_public(IpAddr::V4(ip)) {
                return Err(blocked_msg(&ip.to_string()));
            }
        }

        Some(url::Host::Ipv6(ip)) => {
            if !ip_is_public(IpAddr::V6(ip)) {
                return Err(blocked_msg(&ip.to_string()));
            }
        }

        Some(url::Host::Domain(_)) => {}

        None => return Err("URL has no host".to_string()),
    }

    Ok(())
}

/// DNS resolver that rejects any hostname resolving to a non-public address.
///
/// The whole resolution is rejected if any returned address is non-public: a
/// name that mixes public and private answers is either a misconfiguration or
/// a rebinding attack, and there is no safe subset to keep.
pub struct FilteringResolver;

impl Resolve for FilteringResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();

        Box::pin(async move {
            let lookup = format!("{}:0", host);

            let addrs: Vec<SocketAddr> = tokio::net::lookup_host(lookup).await?.collect();

            for addr in &addrs {
                if !ip_is_public(addr.ip()) {
                    let msg = blocked_msg(&addr.ip().to_string());

                    return Err(Box::<dyn std::error::Error + Send + Sync>::from(msg));
                }
            }

            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(s: &str) -> bool {
        ip_is_public(s.parse().unwrap())
    }

    #[test]
    fn rejects_ipv4_non_public_ranges() {
        assert!(!public("0.0.0.0"));
        assert!(!public("127.0.0.1"));
        assert!(!public("10.0.0.5"));
        assert!(!public("172.16.0.1"));
        assert!(!public("172.31.255.255"));
        assert!(!public("192.168.1.1"));
        assert!(!public("169.254.169.254")); // cloud metadata
        assert!(!public("100.64.0.1")); // CGNAT
        assert!(!public("100.100.100.200")); // Alibaba metadata
        assert!(!public("192.0.0.1")); // IETF protocol assignments
        assert!(!public("198.18.0.1")); // benchmarking
        assert!(!public("240.0.0.1")); // reserved
        assert!(!public("255.255.255.255")); // broadcast
    }

    #[test]
    fn accepts_ipv4_public() {
        assert!(public("1.1.1.1"));
        assert!(public("8.8.8.8"));
        assert!(public("140.82.121.4")); // github
        assert!(public("172.15.255.255")); // just below 172.16/12
        assert!(public("172.32.0.1")); // just above 172.16/12
    }

    #[test]
    fn rejects_ipv6_non_public_ranges() {
        assert!(!public("::"));
        assert!(!public("::1"));
        assert!(!public("fc00::1")); // ULA
        assert!(!public("fd12:3456::1")); // ULA
        assert!(!public("fe80::1")); // link-local
        assert!(!public("ff02::1")); // multicast
        assert!(!public("2001:db8::1")); // documentation
    }

    #[test]
    fn accepts_ipv6_public() {
        assert!(public("2606:4700:4700::1111")); // cloudflare
        assert!(public("2001:4860:4860::8888")); // google
    }

    #[test]
    fn unwraps_ipv4_mapped_ipv6() {
        assert!(!public("::ffff:127.0.0.1"));
        assert!(!public("::ffff:10.0.0.1"));
        assert!(!public("::ffff:169.254.169.254"));
        assert!(public("::ffff:8.8.8.8"));
    }

    #[test]
    fn guard_rejects_literal_private_urls() {
        assert!(guard_url_host("http://127.0.0.1/").is_err());
        assert!(guard_url_host("http://127.0.0.1:8080/x?y=1").is_err());
        assert!(guard_url_host("http://10.0.0.5/").is_err());
        assert!(guard_url_host("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(guard_url_host("http://[::1]/").is_err());
        assert!(guard_url_host("http://[::ffff:127.0.0.1]/").is_err());
    }

    #[test]
    fn guard_rejects_encoded_loopback() {
        // The WHATWG parser normalises these to 127.0.0.1 before we see them.
        assert!(guard_url_host("http://2130706433/").is_err()); // decimal
        assert!(guard_url_host("http://0x7f.0.0.1/").is_err()); // hex octet
        assert!(guard_url_host("http://127.1/").is_err()); // short form
    }

    #[test]
    fn guard_accepts_public_hosts() {
        assert!(guard_url_host("https://example.com/").is_ok());
        assert!(guard_url_host("https://1.1.1.1/").is_ok());
        assert!(guard_url_host("https://[2606:4700:4700::1111]/").is_ok());
    }
}
