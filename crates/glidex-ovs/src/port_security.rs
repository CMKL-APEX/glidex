//! What a VM port on a glidex host bridge may send
//! (spec/09-networking.md §9.1, decision 18).
//!
//! Each port gets OpenFlow rules on its `in_port`: drops for what no port
//! sends (a DHCP server's replies, IPv6 router advertisements), allows for
//! what passes the port's checks, and a drop for the rest. They sit above
//! the bridge's own `NORMAL` rule, and the metadata rule
//! ([`crate::metadata::flow`]) sits between the drops and the allows.
//!
//! Every rule of a port carries one of two cookies built from the port's
//! name ([`cookie`]): the final drop has [`BASE`] set, the checks don't.
//! Replacing the checks keeps the final drop in place, so while they change
//! the port's traffic is dropped, not switched unchecked; removing the port
//! removes both ([`remove`]).

use crate::OvsError;
use crate::exec::{Cmd, Exec, Program};
use crate::names;
use crate::net::Ipv4Net;
use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The top 16 bits of every port-security cookie ("ps").
pub const COOKIE_PREFIX: u64 = 0x7073 << 48;
/// Set on the cookie of a port's final drop.
pub const BASE: u64 = 1;
/// Matches both of a port's cookies.
pub const PORT_MASK: u64 = !0xff;

/// Priorities, highest first. The metadata rule is at 200.
const PRIO_DENY: u16 = 320;
const PRIO_ALLOW_ND: u16 = 160;
const PRIO_DENY_ND: u16 = 155;
const PRIO_ALLOW_ADDR: u16 = 150;
const PRIO_DENY_ADDR: u16 = 140;
const PRIO_ALLOW_MAC: u16 = 130;
const PRIO_DROP: u16 = 100;

/// The network's mode, as far as the checks go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A NAT network: the port's DHCP reservation is its address.
    Nat {
        address: Ipv4Addr,
    },
    Isolated,
    Bridged,
}

/// What one port is checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    pub mode: Mode,
    /// The NIC's MAC, lower case.
    pub mac: String,
    /// The NIC's `allowed_addresses` (decision 20); empty: none given.
    pub allowed: Vec<AllowedAddress>,
}

/// At most this many `allowed_addresses` per NIC.
pub const MAX_ALLOWED: usize = 16;

/// One entry of a NIC's `allowed_addresses`: an IPv4 or IPv6 address, or
/// a prefix (`10.0.0.0/29`), with no host bits set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AllowedAddress {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl AllowedAddress {
    pub fn parse(s: &str) -> Result<Self, OvsError> {
        let bad = |why: &str| OvsError::invalid(format!("allowed address '{s}': {why}"));
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = a.parse().map_err(|_| bad("not an IP address"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| bad("bad prefix length"))?,
            None => max,
        };
        let me = Self { addr, prefix };
        if me.network() != addr {
            return Err(bad("host bits are set"));
        }
        if addr.is_unspecified() || addr.is_multicast() || addr.is_loopback() {
            return Err(bad("not a unicast address"));
        }
        Ok(me)
    }

    fn bits(&self) -> u128 {
        match self.addr {
            IpAddr::V4(a) => u128::from(u32::from(a)),
            IpAddr::V6(a) => u128::from(a),
        }
    }

    fn width(&self) -> u8 {
        if self.addr.is_ipv4() { 32 } else { 128 }
    }

    fn mask(&self) -> u128 {
        let w = u32::from(self.width());
        let host = w - u32::from(self.prefix);
        let all = if w == 128 {
            u128::MAX
        } else {
            (1u128 << w) - 1
        };
        all & !(if host == 128 {
            u128::MAX
        } else {
            (1u128 << host) - 1
        })
    }

    fn network(&self) -> IpAddr {
        let n = self.bits() & self.mask();
        match self.addr {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::from(n as u32)),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::from(n)),
        }
    }

    /// Whether the two share an address.
    pub fn overlaps(&self, other: &AllowedAddress) -> bool {
        if self.addr.is_ipv4() != other.addr.is_ipv4() {
            return false;
        }
        let m = self.mask() & other.mask();
        self.bits() & m == other.bits() & m
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.overlaps(&AllowedAddress {
            addr: ip,
            prefix: if ip.is_ipv4() { 32 } else { 128 },
        })
    }

    /// Whether it lies inside `net` (an IPv6 entry never does).
    pub fn within(&self, net: &Ipv4Net) -> bool {
        match self.addr {
            IpAddr::V4(a) => self.prefix >= net.prefix() && net.contains(a),
            IpAddr::V6(_) => false,
        }
    }
}

impl fmt::Display for AllowedAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.prefix == self.width() {
            write!(f, "{}", self.addr)
        } else {
            write!(f, "{}/{}", self.addr, self.prefix)
        }
    }
}

/// A NIC's `allowed_addresses`, parsed: at most [`MAX_ALLOWED`], no two
/// overlapping.
pub fn parse_allowed(list: &[String]) -> Result<Vec<AllowedAddress>, OvsError> {
    if list.len() > MAX_ALLOWED {
        return Err(OvsError::invalid(format!(
            "at most {MAX_ALLOWED} allowed addresses per NIC"
        )));
    }
    let v: Vec<AllowedAddress> = list
        .iter()
        .map(|s| AllowedAddress::parse(s.trim()))
        .collect::<Result<_, _>>()?;
    for (i, a) in v.iter().enumerate() {
        if let Some(b) = v[i + 1..].iter().find(|b| a.overlaps(b)) {
            return Err(OvsError::invalid(format!(
                "allowed addresses {a} and {b} overlap"
            )));
        }
    }
    Ok(v)
}

/// The checks cookie of `vm_id`'s NIC `nic`: the prefix, the 32 bits of
/// the VM id that name its port (`gx<8 hex>-<nic>`), and the NIC index.
/// Port names are unique on a host, so cookies are too.
pub fn cookie(vm_id: &str, nic: u8) -> Result<u64, OvsError> {
    let port = names::port_name(vm_id, nic)?;
    let hex = &port[2..10];
    let id = u32::from_str_radix(hex, 16)
        .map_err(|_| OvsError::invalid(format!("vm_id '{vm_id}' does not start with hex")))?;
    Ok(COOKIE_PREFIX | (u64::from(id) << 16) | (u64::from(nic) << 8))
}

/// The port's final drop, under `cookie | BASE`.
pub fn base_flow(ofport: u32, cookie: u64) -> String {
    format!(
        "cookie={:#x},priority={PRIO_DROP},in_port={ofport},actions=drop",
        cookie | BASE
    )
}

/// The port's checks, under `cookie`, in priority order.
pub fn flows(ofport: u32, cookie: u64, checks: &Checks) -> Vec<String> {
    let mac = checks.mac.to_ascii_lowercase();
    let rule = |prio: u16, m: &str, act: &str| {
        format!("cookie={cookie:#x},priority={prio},in_port={ofport},{m}actions={act}")
    };
    let mut v = Vec::new();
    if matches!(checks.mode, Mode::Nat { .. } | Mode::Isolated) {
        // Not a DHCP server, not an IPv6 router.
        v.push(rule(PRIO_DENY, "udp,tp_src=67,", "drop"));
        v.push(rule(PRIO_DENY, "icmp6,icmp_type=134,", "drop"));
    }
    // The IPv4 sources: on NAT the reservation and the list; elsewhere
    // the list, when there is one (decision 20).
    let listed = !checks.allowed.is_empty();
    let mut v4: Vec<String> = Vec::new();
    if let Mode::Nat { address } = checks.mode {
        v4.push(address.to_string());
    }
    let v6: Vec<String> = checks
        .allowed
        .iter()
        .filter(|a| a.addr.is_ipv6())
        .map(|a| a.to_string())
        .collect();
    v4.extend(
        checks
            .allowed
            .iter()
            .filter(|a| a.addr.is_ipv4())
            .map(|a| a.to_string()),
    );
    let ipv6_checked = listed && !matches!(checks.mode, Mode::Nat { .. });
    if ipv6_checked {
        // A neighbour advertisement claims only its own addresses (and
        // link-local ones, which ND needs).
        for t in v6.iter().map(String::as_str).chain(["fe80::/10"]) {
            v.push(rule(
                PRIO_ALLOW_ND,
                &format!("icmp6,icmp_type=136,icmp_code=0,dl_src={mac},nd_target={t},"),
                "NORMAL",
            ));
        }
        v.push(rule(PRIO_DENY_ND, "icmp6,icmp_type=136,", "drop"));
    }
    if !v4.is_empty() || listed {
        for a in &v4 {
            v.push(rule(
                PRIO_ALLOW_ADDR,
                &format!("ip,dl_src={mac},nw_src={a},"),
                "NORMAL",
            ));
        }
        // DHCP before the lease.
        v.push(rule(
            PRIO_ALLOW_ADDR,
            &format!("udp,dl_src={mac},nw_src=0.0.0.0,tp_dst=67,"),
            "NORMAL",
        ));
        for a in &v4 {
            v.push(rule(
                PRIO_ALLOW_ADDR,
                &format!("arp,dl_src={mac},arp_spa={a},arp_sha={mac},"),
                "NORMAL",
            ));
        }
        if ipv6_checked {
            // Link-local sources, and `::` for duplicate address detection.
            for a in v6.iter().map(String::as_str).chain(["fe80::/10", "::"]) {
                v.push(rule(
                    PRIO_ALLOW_ADDR,
                    &format!("ipv6,dl_src={mac},ipv6_src={a},"),
                    "NORMAL",
                ));
            }
        }
        v.push(rule(PRIO_DENY_ADDR, "ip,", "drop"));
        v.push(rule(PRIO_DENY_ADDR, "arp,", "drop"));
        if ipv6_checked {
            v.push(rule(PRIO_DENY_ADDR, "ipv6,", "drop"));
        }
    }
    v.push(rule(PRIO_ALLOW_MAC, &format!("dl_src={mac},"), "NORMAL"));
    v
}

fn ofctl(exec: &dyn Exec, args: &[&str]) -> Result<(), OvsError> {
    exec.check(&Cmd::new(Program::OvsOfctl, args.iter().copied()))
        .map(|_| ())
}

/// Put `checks` on `port` of `bridge` (again: after a restart of OVS, or
/// with new checks). Idempotent. The final drop goes first and stays, so
/// the port is never open while its checks are replaced.
pub fn apply(
    exec: &dyn Exec,
    bridge: &str,
    port: &str,
    cookie: u64,
    checks: &Checks,
) -> Result<(), OvsError> {
    let ofport = crate::vsctl::ofport(exec, port)?;
    ofctl(exec, &["add-flow", bridge, &base_flow(ofport, cookie)])?;
    ofctl(
        exec,
        &["del-flows", bridge, &format!("cookie={cookie:#x}/-1")],
    )?;
    let mut text = flows(ofport, cookie, checks).join("\n");
    text.push('\n');
    exec.check(&Cmd::new(Program::OvsOfctl, ["add-flows", bridge, "-"]).stdin(text))?;
    Ok(())
}

/// Remove every rule of the port with `cookie` from `bridge` (none: fine).
pub fn remove(exec: &dyn Exec, bridge: &str, cookie: u64) -> Result<(), OvsError> {
    ofctl(
        exec,
        &[
            "del-flows",
            bridge,
            &format!("cookie={cookie:#x}/{PORT_MASK:#x}"),
        ],
    )
}

/// The cookies of the rules on `bridge` now (to find ports whose rules
/// went missing, as after a restart of `ovs-vswitchd`).
pub fn installed_cookies(exec: &dyn Exec, bridge: &str) -> Result<BTreeSet<u64>, OvsError> {
    let out = exec.check(&Cmd::new(Program::OvsOfctl, ["dump-flows", bridge]))?;
    Ok(parse_cookies(&out.stdout_str()))
}

fn parse_cookies(dump: &str) -> BTreeSet<u64> {
    dump.lines()
        .filter_map(|l| {
            let rest = &l[l.find("cookie=0x")? + "cookie=0x".len()..];
            let end = rest
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(rest.len());
            u64::from_str_radix(&rest[..end], 16).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const VM: &str = "1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab";
    const MAC: &str = "02:aa:bb:cc:dd:ee";
    const C: u64 = 0x7073_1a2b_3c4d_0100;

    fn checks(mode: Mode) -> Checks {
        Checks {
            mode,
            mac: MAC.into(),
            allowed: vec![],
        }
    }

    #[test]
    fn cookies_are_per_port() {
        assert_eq!(cookie(VM, 1).unwrap(), C);
        assert_eq!(cookie(VM, 0).unwrap(), 0x7073_1a2b_3c4d_0000);
        assert_eq!(cookie(VM, 1).unwrap() & PORT_MASK, C);
        assert_eq!((cookie(VM, 1).unwrap() | BASE) & PORT_MASK, C);
        assert_ne!(
            cookie(VM, 0).unwrap() & PORT_MASK,
            cookie(VM, 1).unwrap() & PORT_MASK
        );
        // Clear of the metadata rules' cookies (0x6d00xxxx).
        assert_ne!(C >> 48, 0);
        assert!(cookie("not-a-uuid", 0).is_err());
    }

    #[test]
    fn nat_checks_mac_address_dhcp_and_router_messages() {
        let f = flows(
            7,
            C,
            &checks(Mode::Nat {
                address: Ipv4Addr::new(10, 88, 0, 2),
            }),
        );
        assert_eq!(
            f,
            [
                "cookie=0x70731a2b3c4d0100,priority=320,in_port=7,udp,tp_src=67,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=320,in_port=7,icmp6,icmp_type=134,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=150,in_port=7,ip,dl_src=02:aa:bb:cc:dd:ee,nw_src=10.88.0.2,actions=NORMAL",
                "cookie=0x70731a2b3c4d0100,priority=150,in_port=7,udp,dl_src=02:aa:bb:cc:dd:ee,nw_src=0.0.0.0,tp_dst=67,actions=NORMAL",
                "cookie=0x70731a2b3c4d0100,priority=150,in_port=7,arp,dl_src=02:aa:bb:cc:dd:ee,arp_spa=10.88.0.2,arp_sha=02:aa:bb:cc:dd:ee,actions=NORMAL",
                "cookie=0x70731a2b3c4d0100,priority=140,in_port=7,ip,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=140,in_port=7,arp,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=130,in_port=7,dl_src=02:aa:bb:cc:dd:ee,actions=NORMAL",
            ]
        );
        assert_eq!(
            base_flow(7, C),
            "cookie=0x70731a2b3c4d0101,priority=100,in_port=7,actions=drop"
        );
    }

    #[test]
    fn isolated_checks_mac_dhcp_server_and_router_messages() {
        assert_eq!(
            flows(7, C, &checks(Mode::Isolated)),
            [
                "cookie=0x70731a2b3c4d0100,priority=320,in_port=7,udp,tp_src=67,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=320,in_port=7,icmp6,icmp_type=134,actions=drop",
                "cookie=0x70731a2b3c4d0100,priority=130,in_port=7,dl_src=02:aa:bb:cc:dd:ee,actions=NORMAL",
            ]
        );
    }

    #[test]
    fn bridged_checks_the_mac_only() {
        assert_eq!(
            flows(7, C, &checks(Mode::Bridged)),
            [
                "cookie=0x70731a2b3c4d0100,priority=130,in_port=7,dl_src=02:aa:bb:cc:dd:ee,actions=NORMAL"
            ]
        );
    }

    fn allowed(list: &[&str]) -> Vec<AllowedAddress> {
        parse_allowed(&list.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn allowed_addresses_parse_and_overlap() {
        let a = |s: &str| AllowedAddress::parse(s);
        assert_eq!(a("10.0.0.5").unwrap().to_string(), "10.0.0.5");
        assert_eq!(a("10.0.0.8/29").unwrap().to_string(), "10.0.0.8/29");
        assert_eq!(a("10.0.0.5/32").unwrap().to_string(), "10.0.0.5");
        assert_eq!(a("fd00::5").unwrap().to_string(), "fd00::5");
        assert_eq!(a("fd00::/64").unwrap().to_string(), "fd00::/64");
        for bad in [
            "10.0.0.9/29",
            "10.0.0.0/33",
            "nope",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fd00::1/64",
            "",
        ] {
            assert!(a(bad).is_err(), "{bad}");
        }
        assert!(a("10.0.0.8/29").unwrap().overlaps(&a("10.0.0.12").unwrap()));
        assert!(!a("10.0.0.8/29").unwrap().overlaps(&a("10.0.0.16").unwrap()));
        assert!(a("fd00::/64").unwrap().overlaps(&a("fd00::9").unwrap()));
        assert!(!a("fd00::/64").unwrap().overlaps(&a("10.0.0.1").unwrap()));
        let net: Ipv4Net = "10.88.0.0/24".parse().unwrap();
        assert!(a("10.88.0.64/26").unwrap().within(&net));
        assert!(!a("10.88.0.0/23").unwrap().within(&net));
        assert!(!a("10.89.0.1").unwrap().within(&net));
        assert!(!a("fd00::1").unwrap().within(&net));
        assert!(
            a("10.0.0.8/29")
                .unwrap()
                .contains("10.0.0.9".parse().unwrap())
        );
        // A list: at most 16, none overlapping.
        let many: Vec<String> = (1..=17).map(|i| format!("10.0.1.{i}")).collect();
        assert!(parse_allowed(&many).is_err());
        assert!(parse_allowed(&many[..16]).is_ok());
        assert!(parse_allowed(&["10.0.0.8/29".into(), "10.0.0.9".into()]).is_err());
    }

    #[test]
    fn nat_with_a_list_adds_its_addresses_to_the_reservation() {
        let f = flows(
            7,
            C,
            &Checks {
                allowed: allowed(&["10.88.0.200", "10.88.0.208/29"]),
                ..checks(Mode::Nat {
                    address: Ipv4Addr::new(10, 88, 0, 2),
                })
            },
        );
        let has = |m: &str| f.iter().any(|x| x.contains(m));
        for a in ["10.88.0.2", "10.88.0.200", "10.88.0.208/29"] {
            assert!(
                has(&format!("ip,dl_src={MAC},nw_src={a},actions=NORMAL")),
                "{f:#?}"
            );
            assert!(
                has(&format!("arp,dl_src={MAC},arp_spa={a},arp_sha={MAC},")),
                "{f:#?}"
            );
        }
        // IPv6 stays checked by MAC only on a NAT network.
        assert!(!has("ipv6") && !has("nd_target"), "{f:#?}");
    }

    #[test]
    fn isolated_and_bridged_with_a_list_check_ip_arp_and_nd() {
        for mode in [Mode::Isolated, Mode::Bridged] {
            let f = flows(
                7,
                C,
                &Checks {
                    allowed: allowed(&["192.168.1.10", "fd00::10"]),
                    ..checks(mode)
                },
            );
            let rules: Vec<&str> = f
                .iter()
                .map(|x| x.strip_prefix("cookie=0x70731a2b3c4d0100,").unwrap())
                .collect();
            let want = [
                "priority=160,in_port=7,icmp6,icmp_type=136,icmp_code=0,dl_src=02:aa:bb:cc:dd:ee,nd_target=fd00::10,actions=NORMAL",
                "priority=160,in_port=7,icmp6,icmp_type=136,icmp_code=0,dl_src=02:aa:bb:cc:dd:ee,nd_target=fe80::/10,actions=NORMAL",
                "priority=155,in_port=7,icmp6,icmp_type=136,actions=drop",
                "priority=150,in_port=7,ip,dl_src=02:aa:bb:cc:dd:ee,nw_src=192.168.1.10,actions=NORMAL",
                "priority=150,in_port=7,udp,dl_src=02:aa:bb:cc:dd:ee,nw_src=0.0.0.0,tp_dst=67,actions=NORMAL",
                "priority=150,in_port=7,arp,dl_src=02:aa:bb:cc:dd:ee,arp_spa=192.168.1.10,arp_sha=02:aa:bb:cc:dd:ee,actions=NORMAL",
                "priority=150,in_port=7,ipv6,dl_src=02:aa:bb:cc:dd:ee,ipv6_src=fd00::10,actions=NORMAL",
                "priority=150,in_port=7,ipv6,dl_src=02:aa:bb:cc:dd:ee,ipv6_src=fe80::/10,actions=NORMAL",
                "priority=150,in_port=7,ipv6,dl_src=02:aa:bb:cc:dd:ee,ipv6_src=::,actions=NORMAL",
                "priority=140,in_port=7,ip,actions=drop",
                "priority=140,in_port=7,arp,actions=drop",
                "priority=140,in_port=7,ipv6,actions=drop",
                "priority=130,in_port=7,dl_src=02:aa:bb:cc:dd:ee,actions=NORMAL",
            ];
            let skip = if mode == Mode::Isolated { 2 } else { 0 };
            assert_eq!(rules[skip..], want, "{mode:?}");
        }
    }

    #[test]
    fn upper_case_macs_are_matched_lower_case() {
        let f = flows(
            7,
            C,
            &Checks {
                mode: Mode::Bridged,
                mac: MAC.to_ascii_uppercase(),
                allowed: vec![],
            },
        );
        assert!(f[0].contains("dl_src=02:aa:bb:cc:dd:ee"), "{f:?}");
    }

    #[test]
    fn apply_keeps_the_final_drop_while_replacing_the_checks() {
        let exec = RecordingExec::new();
        exec.on(
            "ovs-vsctl get Interface gx1a2b3c4d-1 ofport",
            Output::ok("7\n"),
        );
        apply(
            &exec,
            "gxbr-iso",
            "gx1a2b3c4d-1",
            C,
            &checks(Mode::Isolated),
        )
        .unwrap();
        let calls = exec.calls();
        assert_eq!(
            calls[1..],
            [
                "ovs-ofctl add-flow gxbr-iso cookie=0x70731a2b3c4d0101,priority=100,in_port=7,actions=drop".to_string(),
                "ovs-ofctl del-flows gxbr-iso cookie=0x70731a2b3c4d0100/-1".to_string(),
                format!(
                    "ovs-ofctl add-flows gxbr-iso - <<< {}\n",
                    flows(7, C, &checks(Mode::Isolated)).join("\n")
                ),
            ],
            "{calls:#?}"
        );
    }

    #[test]
    fn apply_needs_an_ofport() {
        let exec = RecordingExec::new();
        exec.on("ovs-vsctl get Interface", Output::ok("[]\n"));
        assert!(matches!(
            apply(&exec, "gxbr-iso", "gx1a2b3c4d-1", C, &checks(Mode::Bridged)),
            Err(OvsError::Conflict { .. })
        ));
        assert!(!exec.calls().iter().any(|c| c.starts_with("ovs-ofctl")));
    }

    #[test]
    fn remove_takes_both_cookies() {
        let exec = RecordingExec::new();
        remove(&exec, "gxbr-iso", C).unwrap();
        assert_eq!(
            exec.calls(),
            ["ovs-ofctl del-flows gxbr-iso cookie=0x70731a2b3c4d0100/0xffffffffffffff00"]
        );
    }

    #[test]
    fn installed_cookies_from_a_dump() {
        let dump = "NXST_FLOW reply (xid=0x4):\n cookie=0x70731a2b3c4d0101, duration=3.1s, table=0, n_packets=0, n_bytes=0, priority=100,in_port=7 actions=drop\n cookie=0x6d000001, duration=3.1s, table=0, n_packets=0, n_bytes=0, priority=200,tcp,in_port=7 actions=NORMAL\n duration=9.0s, table=0, n_packets=4, n_bytes=300, priority=0 actions=NORMAL\n";
        let exec = RecordingExec::new();
        exec.on("ovs-ofctl dump-flows gxbr-nat", Output::ok(dump));
        assert_eq!(
            installed_cookies(&exec, "gxbr-nat").unwrap(),
            [0x6d000001, 0x7073_1a2b_3c4d_0101].into_iter().collect()
        );
    }

    /// Real OVS: a throwaway bridge with two veth ports, their far ends in
    /// two network namespaces, as root on a host running Open vSwitch:
    ///
    /// ```text
    /// <test binary> --ignored with_open_vswitch
    /// ```
    ///
    /// With the NAT checks, port A reaches B from its own MAC and address
    /// and not from another of either; a DHCP server reply and a router
    /// advertisement from A hit their drop rules; with the bridged checks
    /// another address passes; without rules everything passes. Not run yet.
    #[test]
    #[ignore = "needs root and a running Open vSwitch; adds a bridge for the test"]
    fn with_open_vswitch() {
        use std::process::Command;
        let exec = crate::SystemExec::default();
        let pid = std::process::id() % 10000;
        let br = format!("gxpst{pid}");
        let (na, nb) = (format!("gxpsa{pid}"), format!("gxpsb{pid}"));
        let (pa, pb) = (format!("gxpa{pid}"), format!("gxpb{pid}"));
        struct Cleanup(String, String, String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("ovs-vsctl")
                    .args(["--if-exists", "del-br", &self.0])
                    .status();
                for ns in [&self.1, &self.2] {
                    let _ = Command::new("ip").args(["netns", "del", ns]).status();
                }
            }
        }
        let _cleanup = Cleanup(br.clone(), na.clone(), nb.clone());
        let sh = |prog: &str, args: &[&str]| {
            let st = Command::new(prog).args(args).status().unwrap();
            assert!(st.success(), "{prog} {args:?}");
        };
        let in_ns = |ns: &str, args: &[&str]| {
            let mut a = vec!["netns", "exec", ns];
            a.extend_from_slice(args);
            Command::new("ip").args(&a).status().unwrap().success()
        };
        sh("ovs-vsctl", &["add-br", &br]);
        sh("ip", &["link", "set", &br, "up"]);
        let mac_a = "02:00:00:00:77:02";
        for (ns, port, mac, addr) in [
            (&na, &pa, mac_a, "10.77.0.2/24"),
            (&nb, &pb, "02:00:00:00:77:03", "10.77.0.3/24"),
        ] {
            sh("ip", &["netns", "add", ns]);
            sh(
                "ip",
                &[
                    "link", "add", port, "type", "veth", "peer", "name", "eth0", "netns", ns,
                ],
            );
            sh("ip", &["link", "set", port, "up"]);
            sh("ovs-vsctl", &["add-port", &br, port]);
            for args in [
                &["link", "set", "lo", "up"][..],
                &["link", "set", "eth0", "address", mac],
                &["addr", "add", addr, "dev", "eth0"],
                &["link", "set", "eth0", "up"],
            ] {
                let mut a = vec!["-n", ns.as_str()];
                a.extend_from_slice(args);
                sh("ip", &a);
            }
        }
        let ping = || {
            let _ = in_ns(&na, &["ip", "neigh", "flush", "dev", "eth0"]);
            in_ns(&na, &["ping", "-c", "2", "-W", "1", "10.77.0.3"])
        };
        let readdress = |addr: &str| {
            let _ = in_ns(
                &na,
                &["ip", "addr", "flush", "dev", "eth0", "scope", "global"],
            );
            assert!(in_ns(&na, &["ip", "addr", "add", addr, "dev", "eth0"]));
        };
        let remac = |mac: &str| assert!(in_ns(&na, &["ip", "link", "set", "eth0", "address", mac]));
        let c = 0x7073_0000_0000_0100;
        let nat = Checks {
            mode: Mode::Nat {
                address: Ipv4Addr::new(10, 77, 0, 2),
            },
            mac: mac_a.into(),
            allowed: vec![],
        };

        assert!(ping(), "no rules yet");
        apply(&exec, &br, &pa, c, &nat).unwrap();
        // Twice: idempotent.
        apply(&exec, &br, &pa, c, &nat).unwrap();
        assert!(ping(), "own MAC and address");
        readdress("10.77.0.9/24");
        assert!(!ping(), "another address");
        readdress("10.77.0.2/24");
        remac("02:00:00:00:77:09");
        assert!(!ping(), "another MAC");
        remac(mac_a);
        assert!(ping());

        // A DHCP server's reply and a router advertisement: dropped.
        assert!(in_ns(
            &na,
            &[
                "python3",
                "-c",
                "import socket\ns=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\ns.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\ns.bind(('10.77.0.2',67))\ns.sendto(b'x',('10.77.0.3',68))"
            ]
        ));
        assert!(in_ns(
            &na,
            &[
                "python3",
                "-c",
                "import socket,time\ntime.sleep(2)\ns=socket.socket(socket.AF_INET6,socket.SOCK_RAW,socket.IPPROTO_ICMPV6)\ns.sendto(bytes([134,0,0,0,64,0,7,8])+bytes(8),('ff02::1',0,0,socket.if_nametoindex('eth0')))"
            ]
        ));
        let dump = Command::new("ovs-ofctl")
            .args(["dump-flows", &br])
            .output()
            .unwrap();
        let dump = String::from_utf8_lossy(&dump.stdout);
        for m in ["udp,in_port", "icmp6,in_port"] {
            let line = dump
                .lines()
                .find(|l| l.contains("priority=320") && l.contains(m))
                .unwrap_or_else(|| panic!("no {m} drop: {dump}"));
            assert!(!line.contains("n_packets=0,"), "{line}");
        }

        apply(
            &exec,
            &br,
            &pa,
            c,
            &Checks {
                mode: Mode::Bridged,
                mac: mac_a.into(),
                allowed: vec![],
            },
        )
        .unwrap();
        readdress("10.77.0.9/24");
        assert!(ping(), "bridged: any address from its own MAC");
        remac("02:00:00:00:77:09");
        assert!(!ping(), "bridged: another MAC");

        remove(&exec, &br, c).unwrap();
        assert!(ping(), "no rules: anything");
    }
}
