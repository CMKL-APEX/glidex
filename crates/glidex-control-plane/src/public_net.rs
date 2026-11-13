//! Public internet addresses, for fetches a caller names (federation
//! issuers, image downloads): which addresses count as public, and a
//! resolver that hands the HTTP client only those, at connect time.

use std::net::{IpAddr, Ipv4Addr};

/// Whether `ip` is on the public internet. Not: loopback, private,
/// link-local, unique-local, shared (CGNAT), unspecified, "this network",
/// broadcast, multicast, reserved (240/4), benchmarking (198.18/15), the
/// IETF and documentation ranges; and an IPv4 address carried inside IPv6
/// (mapped, compatible, NAT64 or 6to4) is judged as that IPv4 address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            let embedded = |hi: u16, lo: u16| {
                Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8)
            };
            // NAT64 (64:ff9b::/96, 64:ff9b:1::/48): the IPv4 address it reaches.
            if (s[0], s[1]) == (0x64, 0xff9b) && (s[2..6] == [0; 4] || s[2] == 1) {
                return is_public_v4(embedded(s[6], s[7]));
            }
            // 6to4 (2002::/16): the IPv4 address it tunnels to.
            if s[0] == 0x2002 {
                return is_public_v4(embedded(s[1], s[2]));
            }
            // IPv4-compatible (::a.b.c.d, deprecated).
            if s[..6] == [0; 6] && !v6.is_loopback() && !v6.is_unspecified() {
                return is_public_v4(embedded(s[6], s[7]));
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                // Site-local (fec0::/10, deprecated) and documentation.
                || (s[0] & 0xffc0) == 0xfec0
                || (s[0], s[1]) == (0x2001, 0x0db8))
        }
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || v4.is_documentation()
        || a == 0
        || a >= 240
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19))
        || (a, b, c) == (192, 0, 0))
}

/// Resolves host names to their public addresses only, so a name can't
/// lead the control plane to its own host or network, whenever it is
/// looked up (a name that answers differently a moment later included).
pub struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|a| is_public(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_public_addresses_are_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "198.18.0.1",
            "198.19.255.1",
            "192.0.0.8",
            "192.0.2.1",
            "203.0.113.5",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::a00:1",
            "2002:7f00:1::",
            "2002:a00:1::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "140.82.112.3",
            "8.8.8.8",
            "100.128.0.1",
            "198.20.0.1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }
}
