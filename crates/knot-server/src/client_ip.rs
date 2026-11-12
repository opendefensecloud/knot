//! The client's IP address, through operator-trusted reverse proxies.
//!
//! With `trusted_proxies` empty (the default) the client is the TCP peer,
//! as before. When the peer is a trusted proxy, `X-Forwarded-For` is read
//! from the right, skipping trusted hops; the first untrusted address is
//! the client. A header from an untrusted peer is never believed.
//!
//! [`ClientIp::resolved`] is false when an untrusted peer sent the header:
//! the address is then the peer's, and the header was not believed. With
//! no proxy trusted at all, knot is likely behind a proxy it wasn't told
//! about, where every client looks alike, so the per-IP buckets skip such a
//! request rather than lump all clients together. With proxies trusted, the
//! untrusted peer itself is the client and is limited by its own address.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request};
use ipnet::IpNet;
use knot_config::Config;

/// The proxies whose `X-Forwarded-For` knot believes.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    pub fn new(networks: Vec<IpNet>) -> Self {
        Self(networks)
    }

    /// From `trusted_proxies`. A list that doesn't parse (config validation
    /// refuses it at startup, so only hand-built configs get here) is logged
    /// and trusts nobody.
    pub fn from_config(config: &Config) -> Self {
        match config.trusted_proxies_list() {
            Ok(networks) => Self(networks),
            Err(error) => {
                tracing::error!(%error, "trusted_proxies does not parse; trusting no proxy");
                Self::default()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `address` lies in a trusted network (IPv4-mapped IPv6 is
    /// compared as IPv4).
    pub fn contains(&self, address: IpAddr) -> bool {
        let canonical_address = address.to_canonical();
        self.0
            .iter()
            .any(|network| network.contains(&canonical_address) || network.contains(&address))
    }
}

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp {
    pub ip: IpAddr,
    /// False exactly when the peer is not trusted and the request carries
    /// `X-Forwarded-For`: an untrusted peer sent the header, and `ip` is the
    /// peer. The per-IP buckets skip such a request only when no proxy is
    /// trusted (`trusted_proxies` empty); otherwise they limit the peer.
    pub resolved: bool,
    /// A malformed `X-Forwarded-For` entry stopped the walk (for a metric).
    pub malformed_forwarded_entry: bool,
}

/// Resolve the client of a request whose TCP peer is `peer` (`None` when
/// there is no `ConnectInfo`, e.g. `oneshot` tests: then `0.0.0.0`).
pub fn resolve(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &TrustedProxies) -> ClientIp {
    let peer_ip = peer.map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |peer_ip| {
        peer_ip.to_canonical()
    });
    let forwarded_lines = headers.get_all(X_FORWARDED_FOR);
    let carries_forwarded_header = forwarded_lines.iter().next().is_some();
    if !trusted.contains(peer_ip) {
        return ClientIp {
            ip: peer_ip,
            resolved: !carries_forwarded_header,
            malformed_forwarded_entry: false,
        };
    }
    // Every entry of every line, in order; `None` marks a malformed entry
    // (a line that isn't text counts as one malformed entry).
    let forwarded_entries: Vec<Option<IpAddr>> = forwarded_lines
        .iter()
        .flat_map(|line| match line.to_str() {
            Ok(text) => text.split(',').map(parse_forwarded_entry).collect(),
            Err(_) => vec![None],
        })
        .collect();
    let mut last_valid_address = None;
    for entry in forwarded_entries.iter().rev() {
        let Some(address) = *entry else {
            return ClientIp {
                ip: last_valid_address.unwrap_or(peer_ip),
                resolved: true,
                malformed_forwarded_entry: true,
            };
        };
        if !trusted.contains(address) {
            return ClientIp {
                ip: address,
                resolved: true,
                malformed_forwarded_entry: false,
            };
        }
        last_valid_address = Some(address);
    }
    // Every hop is trusted: the right-most entry is what the nearest proxy
    // saw. Anything further left may come from a client inside the trusted
    // range and can't be believed.
    ClientIp {
        ip: forwarded_entries
            .last()
            .copied()
            .flatten()
            .unwrap_or(peer_ip),
        resolved: true,
        malformed_forwarded_entry: false,
    }
}

const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// One `X-Forwarded-For` entry: `ip`, `ip:port` (IPv4), `[v6]` or
/// `[v6]:port`, canonicalised. `None` when malformed.
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    let address = if let Some(bracketed) = entry.strip_prefix('[') {
        let (inside_brackets, after_brackets) = bracketed.split_once(']')?;
        if !after_brackets.is_empty() {
            after_brackets.strip_prefix(':')?.parse::<u16>().ok()?;
        }
        IpAddr::V6(inside_brackets.parse::<Ipv6Addr>().ok()?)
    } else if let Ok(address) = entry.parse::<IpAddr>() {
        address
    } else {
        let (host, port) = entry.split_once(':')?;
        port.parse::<u16>().ok()?;
        IpAddr::V4(host.parse::<Ipv4Addr>().ok()?)
    };
    Some(address.to_canonical())
}

/// [`resolve`] for a request, reading the peer from `ConnectInfo`.
pub fn from_request<Body>(request: &Request<Body>, trusted: &TrustedProxies) -> ClientIp {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|connect_info| connect_info.0.ip());
    resolve(peer, request.headers(), trusted)
}

/// A rate-limit key for a client address: IPv4 per address, IPv6 per `/64`
/// (one host usually holds a whole `/64`).
#[derive(Hash, Eq, PartialEq, Clone, Copy, Debug)]
pub struct IpKey(IpAddr);

impl IpKey {
    pub fn of(ip: IpAddr) -> Self {
        match ip.to_canonical() {
            IpAddr::V4(address) => Self(IpAddr::V4(address)),
            IpAddr::V6(address) => {
                let network_bits = u128::from(address) & !(u128::from(u64::MAX));
                Self(IpAddr::V6(Ipv6Addr::from(network_bits)))
            }
        }
    }

    pub fn ip(&self) -> IpAddr {
        self.0
    }
}

impl fmt::Display for IpKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            IpAddr::V4(address) => write!(formatter, "{address}"),
            IpAddr::V6(network) => write!(formatter, "{network}/64"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const PEER: &str = "10.0.0.2";

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address")
    }

    fn trusting(networks: &[&str]) -> TrustedProxies {
        TrustedProxies::new(
            networks
                .iter()
                .map(|network| network.parse().expect("test network"))
                .collect(),
        )
    }

    fn private_proxies() -> TrustedProxies {
        trusting(&["10.0.0.0/8"])
    }

    fn forwarded(lines: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(
                "x-forwarded-for",
                HeaderValue::from_str(line).expect("test header"),
            );
        }
        headers
    }

    fn client_via_private_proxy(lines: &[&str]) -> ClientIp {
        resolve(Some(ip(PEER)), &forwarded(lines), &private_proxies())
    }

    #[test]
    fn no_trusted_proxies_and_no_header_is_the_peer() {
        let client = resolve(
            Some(ip("203.0.113.9")),
            &HeaderMap::new(),
            &TrustedProxies::default(),
        );
        assert_eq!(client.ip, ip("203.0.113.9"));
        assert!(client.resolved);
    }

    #[test]
    fn header_is_ignored_without_trusted_proxies_and_marks_unresolved() {
        let client = resolve(
            Some(ip("203.0.113.9")),
            &forwarded(&["198.51.100.7"]),
            &TrustedProxies::default(),
        );
        assert_eq!(client.ip, ip("203.0.113.9"));
        assert!(!client.resolved, "an unknown proxy is in front");
    }

    #[test]
    fn header_from_an_untrusted_peer_is_ignored() {
        let client = resolve(
            Some(ip("203.0.113.9")),
            &forwarded(&["198.51.100.7"]),
            &private_proxies(),
        );
        assert_eq!(client.ip, ip("203.0.113.9"));
        assert!(!client.resolved);
    }

    #[test]
    fn a_trusted_peer_without_a_header_is_the_client() {
        let client = resolve(Some(ip(PEER)), &HeaderMap::new(), &private_proxies());
        assert_eq!(client.ip, ip(PEER));
        assert!(client.resolved);
    }

    #[test]
    fn the_right_most_untrusted_hop_is_the_client() {
        let client = client_via_private_proxy(&["198.51.100.7, 10.0.0.5"]);
        assert_eq!(client.ip, ip("198.51.100.7"));
        assert!(client.resolved);
        assert!(!client.malformed_forwarded_entry);
    }

    #[test]
    fn a_spoofed_left_most_entry_is_not_believed() {
        let client = client_via_private_proxy(&["6.6.6.6, 198.51.100.7, 10.0.0.5"]);
        assert_eq!(client.ip, ip("198.51.100.7"));
    }

    #[test]
    fn several_header_lines_are_joined_in_order() {
        let client = client_via_private_proxy(&["6.6.6.6", "198.51.100.7, 10.0.0.5"]);
        assert_eq!(client.ip, ip("198.51.100.7"));
        let client = client_via_private_proxy(&["6.6.6.6, 198.51.100.7", "10.0.0.5"]);
        assert_eq!(
            client.ip,
            ip("198.51.100.7"),
            "the trusted hop on line two is skipped"
        );
    }

    #[test]
    fn ports_and_brackets_are_accepted() {
        for (entry, expected) in [
            ("198.51.100.7:4711", "198.51.100.7"),
            ("[2001:db8::1]:443", "2001:db8::1"),
            ("[2001:db8::1]", "2001:db8::1"),
            ("2001:db8::1", "2001:db8::1"),
            (" 198.51.100.7 ", "198.51.100.7"),
        ] {
            let client = client_via_private_proxy(&[entry]);
            assert_eq!(client.ip, ip(expected), "entry {entry:?}");
            assert!(!client.malformed_forwarded_entry, "entry {entry:?}");
        }
    }

    #[test]
    fn mapped_ipv6_is_canonicalised() {
        let client = client_via_private_proxy(&["::ffff:198.51.100.7"]);
        assert_eq!(client.ip, ip("198.51.100.7"));
        let via_mapped_peer = resolve(
            Some(ip("::ffff:10.0.0.2")),
            &forwarded(&["198.51.100.7"]),
            &private_proxies(),
        );
        assert_eq!(
            via_mapped_peer.ip,
            ip("198.51.100.7"),
            "a mapped peer is trusted as IPv4"
        );
        let mapped_trusted_hop = client_via_private_proxy(&["198.51.100.7, ::ffff:10.0.0.5"]);
        assert_eq!(mapped_trusted_hop.ip, ip("198.51.100.7"));
    }

    #[test]
    fn an_all_trusted_chain_gives_the_right_most_entry() {
        let client = client_via_private_proxy(&["10.0.0.9, 10.0.0.5"]);
        assert_eq!(client.ip, ip("10.0.0.5"), "what the nearest proxy saw");
        assert!(client.resolved);
    }

    #[test]
    fn a_malformed_entry_stops_the_walk_at_the_last_valid_address() {
        let client = client_via_private_proxy(&["198.51.100.7, garbage, 10.0.0.5"]);
        assert_eq!(client.ip, ip("10.0.0.5"));
        assert!(client.malformed_forwarded_entry);
        assert!(client.resolved);
    }

    #[test]
    fn a_malformed_right_most_entry_falls_back_to_the_peer() {
        for lines in [
            &["198.51.100.7, garbage"][..],
            &["unknown"],
            &["198.51.100.7,"],
            &[""],
            &["198.51.100.7:99999"],
            &["[2001:db8::1"],
            &["[2001:db8::1]x"],
            &["fe80::1%eth0"],
        ] {
            let client = client_via_private_proxy(lines);
            assert_eq!(client.ip, ip(PEER), "lines {lines:?}");
            assert!(client.malformed_forwarded_entry, "lines {lines:?}");
        }
    }

    #[test]
    fn a_non_utf8_header_line_is_malformed() {
        let mut headers = HeaderMap::new();
        headers.append(
            "x-forwarded-for",
            HeaderValue::from_bytes(b"198.51.100.7\xff").expect("opaque header"),
        );
        let client = resolve(Some(ip(PEER)), &headers, &private_proxies());
        assert_eq!(client.ip, ip(PEER));
        assert!(client.malformed_forwarded_entry);
    }

    #[test]
    fn a_missing_peer_is_unspecified() {
        let client = resolve(None, &HeaderMap::new(), &private_proxies());
        assert_eq!(client.ip, ip("0.0.0.0"));
        assert!(client.resolved);
    }

    #[test]
    fn from_request_reads_connect_info() {
        let mut request = Request::builder()
            .header("x-forwarded-for", "198.51.100.7")
            .body(())
            .expect("request");
        request
            .extensions_mut()
            .insert(ConnectInfo::<SocketAddr>("10.0.0.2:5555".parse().unwrap()));
        let client = from_request(&request, &private_proxies());
        assert_eq!(client.ip, ip("198.51.100.7"));
    }

    #[test]
    fn trust_checks_use_the_full_address() {
        let trusted = trusting(&["2001:db8:1:2::1/128"]);
        assert!(trusted.contains(ip("2001:db8:1:2::1")));
        assert!(
            !trusted.contains(ip("2001:db8:1:2::2")),
            "same /64, different proxy"
        );
        assert!(private_proxies().contains(ip("::ffff:10.1.1.1")));
        assert!(!private_proxies().contains(ip("11.0.0.1")));
    }

    #[test]
    fn from_config_parses_and_falls_back_to_trusting_nobody() {
        let config = Config {
            trusted_proxies: "10.0.0.0/8, 192.168.1.5".into(),
            ..Default::default()
        };
        let trusted = TrustedProxies::from_config(&config);
        assert!(trusted.contains(ip("10.9.9.9")));
        assert!(trusted.contains(ip("192.168.1.5")));
        let broken = Config {
            trusted_proxies: "0.0.0.0/0".into(),
            ..Default::default()
        };
        assert!(TrustedProxies::from_config(&broken).is_empty());
    }

    #[test]
    fn ipv6_keys_group_by_slash_64() {
        assert_eq!(
            IpKey::of(ip("2001:db8:1:2:aaaa::1")),
            IpKey::of(ip("2001:db8:1:2:bbbb::2"))
        );
        assert_ne!(
            IpKey::of(ip("2001:db8:1:2::1")),
            IpKey::of(ip("2001:db8:1:3::1"))
        );
        assert_eq!(
            IpKey::of(ip("2001:db8:1:2:aaaa::1")).ip(),
            ip("2001:db8:1:2::")
        );
    }

    #[test]
    fn ipv4_keys_are_per_address() {
        assert_ne!(IpKey::of(ip("198.51.100.7")), IpKey::of(ip("198.51.100.8")));
        assert_eq!(
            IpKey::of(ip("::ffff:198.51.100.7")),
            IpKey::of(ip("198.51.100.7"))
        );
    }

    #[test]
    fn ip_keys_display_as_address_or_network() {
        assert_eq!(IpKey::of(ip("198.51.100.7")).to_string(), "198.51.100.7");
        assert_eq!(
            IpKey::of(ip("2001:db8:1:2:aaaa::1")).to_string(),
            "2001:db8:1:2::/64"
        );
    }
}
