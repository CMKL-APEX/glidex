//! IPv4 subnets and read-only queries of the host's `ip` state.

use crate::exec::{Cmd, Exec, Program};
use crate::OvsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::net::Ipv4Addr;
use std::path::Path;
use std::str::FromStr;

/// An IPv4 network in CIDR form, stored normalized (host bits cleared).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ipv4Net {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self, OvsError> {
        if prefix > 32 {
            return Err(OvsError::invalid(format!("prefix /{} out of range", prefix)));
        }
        let mask = Self::mask_bits(prefix);
        Ok(Self {
            network: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }

    fn mask_bits(prefix: u8) -> u32 {
        if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        }
    }

    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & Self::mask_bits(self.prefix) == u32::from(self.network)
    }

    pub fn overlaps(&self, other: &Ipv4Net) -> bool {
        let p = self.prefix.min(other.prefix);
        let m = Self::mask_bits(p);
        u32::from(self.network) & m == u32::from(other.network) & m
    }

    /// The `n`-th address after the network address (`host(1)` = `.1`).
    pub fn host(&self, n: u32) -> Option<Ipv4Addr> {
        let size = 1u64 << (32 - self.prefix as u32);
        if (n as u64) < size {
            Some(Ipv4Addr::from(u32::from(self.network) + n))
        } else {
            None
        }
    }

    /// Last address before the broadcast address.
    pub fn last_host(&self) -> Option<Ipv4Addr> {
        let size = 1u64 << (32 - self.prefix as u32);
        if size >= 4 {
            self.host((size - 2) as u32)
        } else {
            None
        }
    }

    /// Consecutive subnets of `prefix` inside `self`, in order.
    pub fn subnets(&self, prefix: u8) -> impl Iterator<Item = Ipv4Net> + '_ {
        let count = if prefix >= self.prefix && prefix <= 32 {
            1u64 << (prefix - self.prefix)
        } else {
            0
        };
        let step = 1u64 << (32 - prefix as u32);
        (0..count).map(move |i| Ipv4Net {
            network: Ipv4Addr::from((u32::from(self.network) as u64 + i * step) as u32),
            prefix,
        })
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = OvsError;

    fn from_str(s: &str) -> Result<Self, OvsError> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| OvsError::invalid(format!("'{}' is not CIDR notation", s)))?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|_| OvsError::invalid(format!("bad address in '{}'", s)))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| OvsError::invalid(format!("bad prefix in '{}'", s)))?;
        Ipv4Net::new(addr, prefix)
    }
}

impl Serialize for Ipv4Net {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Ipv4Net {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Destinations of all IPv4 routes in the main table, as networks
/// (`default` is `0.0.0.0/0`, host routes are `/32`).
pub fn route_destinations(exec: &dyn Exec) -> Result<Vec<Ipv4Net>, OvsError> {
    let out = exec.check(&Cmd::new(Program::Ip, ["-4", "-j", "route", "show"]))?;
    let routes: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    Ok(routes
        .iter()
        .filter_map(|r| r.get("dst").and_then(Value::as_str))
        .filter_map(|dst| match dst {
            "default" => Some(Ipv4Net::new(Ipv4Addr::UNSPECIFIED, 0).unwrap()),
            d if d.contains('/') => d.parse().ok(),
            d => d.parse::<Ipv4Addr>().ok().and_then(|a| Ipv4Net::new(a, 32).ok()),
        })
        .collect())
}

pub fn link_exists(exec: &dyn Exec, ifname: &str) -> bool {
    exec.exists(&Path::new("/sys/class/net").join(ifname))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    #[test]
    fn cidr_math() {
        let n: Ipv4Net = "10.88.0.7/24".parse().unwrap();
        assert_eq!(n.to_string(), "10.88.0.0/24");
        assert_eq!(n.host(1), Some("10.88.0.1".parse().unwrap()));
        assert_eq!(n.last_host(), Some("10.88.0.254".parse().unwrap()));
        assert!(n.contains("10.88.0.200".parse().unwrap()));
        assert!(!n.contains("10.88.1.1".parse().unwrap()));
        let sup: Ipv4Net = "10.88.0.0/16".parse().unwrap();
        assert!(sup.overlaps(&n) && n.overlaps(&sup));
        let subs: Vec<_> = sup.subnets(24).take(2).map(|s| s.to_string()).collect();
        assert_eq!(subs, ["10.88.0.0/24", "10.88.1.0/24"]);
        assert_eq!(sup.subnets(24).count(), 256);
        assert!("10.0.0.0/33".parse::<Ipv4Net>().is_err());
    }

    #[test]
    fn parses_routes() {
        let exec = RecordingExec::new();
        exec.on(
            "ip -4 -j route show",
            Output::ok(r#"[{"dst":"default","gateway":"192.168.0.189","dev":"ens3"},{"dst":"10.0.0.0/24","dev":"ens3"},{"dst":"192.168.0.189","dev":"ens3"}]"#),
        );
        let routes: Vec<String> = route_destinations(&exec)
            .unwrap()
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(routes, ["0.0.0.0/0", "10.0.0.0/24", "192.168.0.189/32"]);
    }
}
