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
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

/// The top 16 bits of every port-security cookie ("ps").
pub const COOKIE_PREFIX: u64 = 0x7073 << 48;
/// Set on the cookie of a port's final drop.
pub const BASE: u64 = 1;
/// Matches both of a port's cookies.
pub const PORT_MASK: u64 = !0xff;

/// Priorities, highest first. The metadata rule is at 200.
const PRIO_DENY: u16 = 320;
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
    if let Mode::Nat { address } = checks.mode {
        v.push(rule(
            PRIO_ALLOW_ADDR,
            &format!("ip,dl_src={mac},nw_src={address},"),
            "NORMAL",
        ));
        // DHCP before the lease.
        v.push(rule(
            PRIO_ALLOW_ADDR,
            &format!("udp,dl_src={mac},nw_src=0.0.0.0,tp_dst=67,"),
            "NORMAL",
        ));
        v.push(rule(
            PRIO_ALLOW_ADDR,
            &format!("arp,dl_src={mac},arp_spa={address},arp_sha={mac},"),
            "NORMAL",
        ));
        v.push(rule(PRIO_DENY_ADDR, "ip,", "drop"));
        v.push(rule(PRIO_DENY_ADDR, "arp,", "drop"));
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

    #[test]
    fn upper_case_macs_are_matched_lower_case() {
        let f = flows(
            7,
            C,
            &Checks {
                mode: Mode::Bridged,
                mac: MAC.to_ascii_uppercase(),
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
