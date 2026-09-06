//! Reverse-proxy awareness: trusted proxies and client IP resolution.
//!
//! The rate limiter and the audit fields of refresh tokens key on the
//! *client* IP. When the server sits behind a reverse proxy, every TCP
//! peer is the proxy itself, so the client address must be recovered
//! from `X-Forwarded-For` — **but only when the peer is a trusted
//! proxy**, otherwise any client could spoof its bucket identity by
//! sending a fake header.
//!
//! The algorithm (see [`resolve_client_ip`]) follows the de-facto
//! standard: walk the `X-Forwarded-For` list from **right to left**,
//! skipping trusted proxies, and stop at the first address that is not
//! a trusted proxy — that is the client as observed by the outermost
//! untrusted hop. With no trusted proxies configured (the default) the
//! header is ignored entirely and the TCP peer address is used.

use std::net::IpAddr;

/// An IP network: a base address plus a prefix length.
///
/// IPv4 prefixes range from 0 to 32, IPv6 prefixes from 0 to 128. A
/// single address parsed without a mask becomes a `/32` (or `/128`)
/// network. Mixed-family checks never match: an IPv4 `contains` query
/// against an IPv6 network (and vice versa) is always `false`, without
/// IPv4-mapped special casing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

/// Parses an exact IP or CIDR block: `"10.0.0.4"`, `"10.0.0.0/8"`,
/// `"::1"`, `"fd00::/8"`.
///
/// Returns `None` for malformed input, out-of-range prefixes or
/// host bits set inside the network part (`"10.0.0.1/8"` — the
/// `10.x` network must be written `10.0.0.0/8`). Host-bit checks keep
/// configuration mistakes loud instead of silently matching less than
/// intended.
pub fn parse_cidr(value: &str) -> Option<Cidr> {
    let (address, prefix) = match value.split_once('/') {
        Some((address, prefix)) => {
            let prefix: u8 = prefix.parse().ok()?;
            let address: IpAddr = address.parse().ok()?;
            let max_prefix = match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            if prefix > max_prefix {
                return None;
            }
            (address, prefix)
        }
        None => {
            let address: IpAddr = value.parse().ok()?;
            let prefix = match address {
                IpAddr::V4(_) => 32,
                IpAddr::V6(_) => 128,
            };
            (address, prefix)
        }
    };

    let cidr = Cidr {
        network: address,
        prefix,
    };
    if !cidr.network_is_normalized() {
        return None;
    }
    Some(cidr)
}

impl Cidr {
    /// Returns `true` when `network` has no host bits set.
    fn network_is_normalized(&self) -> bool {
        self.masked(self.network) == self.network
    }

    /// Clears the host bits of `address` according to this network's
    /// prefix.
    fn masked(&self, address: IpAddr) -> IpAddr {
        match (self.network, address) {
            (IpAddr::V4(_), IpAddr::V4(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                IpAddr::V4((u32::from(address) & mask).into())
            }
            (IpAddr::V6(_), IpAddr::V6(address)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                IpAddr::V6((u128::from(address) & mask).into())
            }
            // Mixed families never match.
            _ => address,
        }
    }

    /// Returns `true` when `address` falls inside this network (same IP
    /// family only).
    pub fn contains(&self, address: IpAddr) -> bool {
        match (self.network, address) {
            (IpAddr::V4(_), IpAddr::V6(_)) | (IpAddr::V6(_), IpAddr::V4(_)) => false,
            _ => self.masked(address) == self.network,
        }
    }

    /// Parses every entry, skipping invalid ones (already rejected at
    /// configuration validation; this only runs on pre-validated input).
    pub fn parse_all(values: &[String]) -> Vec<Cidr> {
        values.iter().filter_map(|v| parse_cidr(v)).collect()
    }
}

/// Extracts the client IP for a request whose TCP peer is `peer` and
/// whose (possibly multi-valued, comma-separated) `X-Forwarded-For`
/// header values are `xff_values`.
///
/// - Peer not trusted → the peer address itself (the header is ignored,
///   so it cannot be spoofed by direct clients).
/// - Peer trusted → the right-most **untrusted** entry of the header;
///   if every entry is a trusted proxy the left-most entry is used (it
///   is the origin of a chain made only of proxies). Unparsable entries
///   are skipped. No header → the peer address.
pub fn resolve_client_ip(peer: IpAddr, xff_values: &[&str], trusted: &[Cidr]) -> IpAddr {
    let is_trusted = |ip: IpAddr| trusted.iter().any(|cidr| cidr.contains(ip));

    if !is_trusted(peer) {
        return peer;
    }

    let mut candidates: Vec<IpAddr> = Vec::new();
    for value in xff_values {
        for part in value.split(',') {
            if let Ok(ip) = part.trim().parse::<IpAddr>() {
                candidates.push(ip);
            }
        }
    }
    if candidates.is_empty() {
        return peer;
    }

    for ip in candidates.iter().rev() {
        if !is_trusted(*ip) {
            return *ip;
        }
    }
    // Only trusted hops in the chain: the left-most entry is the origin.
    candidates[0]
}

/// Collects the (possibly multi-valued) `X-Forwarded-For` header values
/// of a request as trimmed strings.
pub fn forwarded_for_values(headers: &axum::http::HeaderMap) -> Vec<&str> {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().expect("valid test address")
    }

    #[test]
    fn parses_exact_addresses_as_full_networks() {
        let cidr = parse_cidr("10.0.0.4").expect("parses");
        assert!(cidr.contains(ip("10.0.0.4")));
        assert!(!cidr.contains(ip("10.0.0.5")));

        let cidr = parse_cidr("::1").expect("parses");
        assert!(cidr.contains(ip("::1")));
        assert!(!cidr.contains(ip("::2")));
    }

    #[test]
    fn parses_cidr_blocks() {
        let cidr = parse_cidr("10.0.0.0/8").expect("parses");
        assert!(cidr.contains(ip("10.255.0.1")));
        assert!(!cidr.contains(ip("11.0.0.1")));

        let cidr = parse_cidr("192.168.1.0/24").expect("parses");
        assert!(cidr.contains(ip("192.168.1.0")));
        assert!(cidr.contains(ip("192.168.1.255")));
        assert!(!cidr.contains(ip("192.168.2.1")));

        let cidr = parse_cidr("fd00::/8").expect("parses");
        assert!(cidr.contains(ip("fd12::abcd")));
        assert!(!cidr.contains(ip("fe80::1")));
    }

    #[test]
    fn zero_prefix_matches_everything_of_the_family() {
        let cidr = parse_cidr("0.0.0.0/0").expect("parses");
        assert!(cidr.contains(ip("203.0.113.9")));
        assert!(!cidr.contains(ip("fd00::1")));

        let cidr = parse_cidr("::/0").expect("parses");
        assert!(cidr.contains(ip("fd00::1")));
        assert!(!cidr.contains(ip("203.0.113.9")));
    }

    #[test]
    fn mixed_families_never_match() {
        let cidr = parse_cidr("127.0.0.1").expect("parses");
        assert!(!cidr.contains(ip("::ffff:127.0.0.1")));
    }

    #[test]
    fn rejects_malformed_input() {
        for value in [
            "not-an-ip",
            "10.0.0.0/33",
            "10.0.0.0/-1",
            "10.0.0.0/",
            "10.0.0.0/8/9",
            "fd00::/129",
            "",
        ] {
            assert!(parse_cidr(value).is_none(), "value: {value:?}");
        }
    }

    #[test]
    fn rejects_host_bits_in_network_part() {
        // 10.0.0.1/8 keeps a host bit; the canonical form is 10.0.0.0/8.
        assert!(parse_cidr("10.0.0.1/8").is_none());
        assert!(parse_cidr("fd00:1::/16").is_none());
        // ...but exact addresses and canonical networks pass.
        assert!(parse_cidr("10.0.0.1").is_some());
        assert!(parse_cidr("10.0.0.0/8").is_some());
        assert!(parse_cidr("fd00::/16").is_some());
    }

    #[test]
    fn parse_all_skips_invalid_entries() {
        let values: Vec<String> = ["10.0.0.0/8", "oops", "127.0.0.1"]
            .iter()
            .map(|value| value.to_string())
            .collect();
        let parsed = Cidr::parse_all(&values);
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn untrusted_peers_ignore_the_header() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned()]);
        let resolved = resolve_client_ip(ip("203.0.113.9"), &["1.2.3.4, 5.6.7.8"], &trusted);

        assert_eq!(resolved, ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peers_use_rightmost_untrusted_entry() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned()]);
        // client, proxy2, proxy1 — proxy hops are trusted, the client
        // is the right-most untrusted entry.
        let resolved = resolve_client_ip(
            ip("127.0.0.1"),
            &["198.51.100.7, 127.0.0.1, 127.0.0.1"],
            &trusted,
        );

        assert_eq!(resolved, ip("198.51.100.7"));
    }

    #[test]
    fn trusted_proxy_chains_skip_trusted_hops() {
        let trusted = Cidr::parse_all(&["10.0.0.0/8".to_owned()]);
        let resolved = resolve_client_ip(
            ip("10.1.2.3"),
            &["198.51.100.7, 10.0.0.9, 10.0.0.10"],
            &trusted,
        );

        assert_eq!(resolved, ip("198.51.100.7"));
    }

    #[test]
    fn all_trusted_chains_fall_back_to_the_leftmost_entry() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned(), "10.0.0.0/8".to_owned()]);
        let resolved = resolve_client_ip(ip("127.0.0.1"), &["10.2.3.4, 10.5.6.7"], &trusted);

        assert_eq!(resolved, ip("10.2.3.4"));
    }

    #[test]
    fn garbage_entries_are_skipped() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned()]);
        let resolved = resolve_client_ip(ip("127.0.0.1"), &["not-an-ip, , 198.51.100.7"], &trusted);

        assert_eq!(resolved, ip("198.51.100.7"));
    }

    #[test]
    fn missing_header_falls_back_to_the_peer() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned()]);
        let resolved = resolve_client_ip(ip("127.0.0.1"), &[], &trusted);

        assert_eq!(resolved, ip("127.0.0.1"));
    }

    #[test]
    fn multiple_header_values_are_joined() {
        let trusted = Cidr::parse_all(&["127.0.0.1".to_owned()]);
        let resolved = resolve_client_ip(
            ip("127.0.0.1"),
            &["10.0.0.1", "198.51.100.7, 127.0.0.1"],
            &trusted,
        );

        assert_eq!(resolved, ip("198.51.100.7"));
    }
}
