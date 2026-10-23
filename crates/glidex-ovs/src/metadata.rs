//! The metadata endpoint's datapath on node NAT networks
//! (spec/workload-identity.md §7.1, W11).
//!
//! A guest asks `169.254.169.254:80`. Its VM port's OpenFlow rule marks
//! the packet with that port's index ([`mark`]); netd's `inet glidex`
//! table, for marked packets only, DNATs it to `glidex-metad`
//! ([`LISTEN_PORT`]) and SNATs it to the port's own address
//! ([`peer_address`]). metad, unprivileged and in the host's network
//! namespace, reads which VM port asked from the peer address `accept`
//! gives it. The guest's own source address is never trusted: a guest can
//! set any, but only netd's rule on its port sets the mark.

use crate::OvsError;
use crate::exec::{Cmd, Exec, Program};
use std::net::Ipv4Addr;

/// What guests ask.
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(169, 254, 169, 254);
pub const GUEST_PORT: u16 = 80;
/// Where `glidex-metad` listens, on [`ADDRESS`] (an unprivileged port).
pub const LISTEN_PORT: u16 = 8775;
/// A port's mark is `MARK_BASE | index`; nothing else in glidex marks
/// packets, and the bits Tailscale (0xff0000) and Kubernetes (0xc000) use
/// stay clear for indexes below 0x4000.
pub const MARK_BASE: u32 = 0x6d00_0000;
pub const MARK_MASK: u32 = 0xffff_c000;
/// Indexes `1..=MAX_INDEX`: one per VM port on a NAT network of this node.
pub const MAX_INDEX: u16 = 0x3fff;
/// Peer addresses: `169.254.192.0/18`, clear of Tailscale's 100.64.0.0/10.
const PEER_BASE: u32 = u32::from_be_bytes([169, 254, 192, 0]);

pub fn mark(index: u16) -> u32 {
    MARK_BASE | u32::from(index)
}

/// The address metad sees a port's requests come from.
pub fn peer_address(index: u16) -> Ipv4Addr {
    Ipv4Addr::from(PEER_BASE + u32::from(index))
}

/// The port index behind a peer address, if it is one.
pub fn index_of(addr: Ipv4Addr) -> Option<u16> {
    let a = u32::from(addr);
    let i = a.checked_sub(PEER_BASE)?;
    (1..=u32::from(MAX_INDEX)).contains(&i).then_some(i as u16)
}

/// The lowest index not in `used`.
pub fn allocate(used: &[u16]) -> Option<u16> {
    (1..=MAX_INDEX).find(|i| !used.contains(i))
}

/// The OpenFlow rule for port `ofport` with `index`: mark its metadata
/// requests, then switch them as usual. The cookie is the mark, so the
/// rule is removed by it.
pub fn flow(ofport: u32, index: u16) -> String {
    format!(
        "cookie={mark:#x},priority=200,in_port={ofport},tcp,nw_dst={ADDRESS},tp_dst={GUEST_PORT},actions=set_field:{mark:#x}->pkt_mark,NORMAL",
        mark = mark(index)
    )
}

/// Put `port`'s rule on `bridge` (again, after a restart of OVS: rules
/// aren't kept).
pub fn add_flow(exec: &dyn Exec, bridge: &str, port: &str, index: u16) -> Result<(), OvsError> {
    let out = exec.check(&Cmd::new(
        Program::OvsVsctl,
        ["get", "Interface", port, "ofport"],
    ))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let ofport: u32 = text.trim().parse().map_err(|_| {
        OvsError::conflict(format!(
            "{port} has no OpenFlow port number yet: {}",
            text.trim()
        ))
    })?;
    exec.check(&Cmd::new(
        Program::OvsOfctl,
        [
            "add-flow".to_string(),
            bridge.to_string(),
            flow(ofport, index),
        ],
    ))?;
    Ok(())
}

/// Remove index `index`'s rule from `bridge` (none: fine).
pub fn del_flow(exec: &dyn Exec, bridge: &str, index: u16) -> Result<(), OvsError> {
    exec.check(&Cmd::new(
        Program::OvsOfctl,
        [
            "del-flows".to_string(),
            bridge.to_string(),
            format!("cookie={:#x}/-1", mark(index)),
        ],
    ))?;
    Ok(())
}

/// [`ADDRESS`] on the loopback, where metad listens and the DNAT points.
pub fn ensure_address(exec: &dyn Exec) -> Result<(), OvsError> {
    exec.check(&Cmd::new(
        Program::Ip,
        ["addr", "replace", &format!("{ADDRESS}/32"), "dev", "lo"],
    ))?;
    Ok(())
}

/// The `inet glidex` rules for the endpoint (`nat_bridges`: the NAT
/// bridges' names; `ports`: the indexes in use). Chains of their own, so
/// they sit beside netd's NAT and fence rules; the guard and the accept go
/// into the `input` chain ([`input_rules`]).
pub fn nft_chains(nat_bridges: &[&str], ports: &[u16]) -> String {
    if nat_bridges.is_empty() {
        return String::new();
    }
    let ifs: Vec<String> = nat_bridges.iter().map(|b| format!("\"{b}\"")).collect();
    let mut s = format!(
        "  chain metadata_pre {{\n    type nat hook prerouting priority dstnat; policy accept;\n    iifname {{ {ifs} }} ip daddr {ADDRESS} tcp dport {GUEST_PORT} meta mark & {MARK_MASK:#x} == {MARK_BASE:#x} dnat ip to {ADDRESS}:{LISTEN_PORT}\n  }}\n",
        ifs = ifs.join(", ")
    );
    if !ports.is_empty() {
        let elements: Vec<String> = ports
            .iter()
            .map(|i| format!("{:#x} : {}", mark(*i), peer_address(*i)))
            .collect();
        s.push_str(&format!(
            "  map metadata_peer {{\n    type mark : ipv4_addr\n    elements = {{ {} }}\n  }}\n  chain metadata_in {{\n    type nat hook input priority srcnat; policy accept;\n    ip daddr {ADDRESS} tcp dport {LISTEN_PORT} snat ip to meta mark map @metadata_peer\n  }}\n",
            elements.join(", ")
        ));
    }
    s
}

/// The `input` chain's rules for the endpoint, first in it: only a marked
/// connection from a NAT bridge reaches metad; anything else for its port
/// (a host process, another interface) is dropped.
pub fn input_rules(nat_bridges: &[&str]) -> String {
    if nat_bridges.is_empty() {
        return String::new();
    }
    let ifs: Vec<String> = nat_bridges.iter().map(|b| format!("\"{b}\"")).collect();
    format!(
        "    iifname {{ {ifs} }} ip daddr {ADDRESS} tcp dport {LISTEN_PORT} meta mark & {MARK_MASK:#x} == {MARK_BASE:#x} accept\n    ip daddr {ADDRESS} tcp dport {LISTEN_PORT} drop\n",
        ifs = ifs.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_marks_and_addresses() {
        assert_eq!(mark(1), 0x6d00_0001);
        assert_eq!(mark(MAX_INDEX) & MARK_MASK, MARK_BASE);
        assert_eq!(peer_address(1), Ipv4Addr::new(169, 254, 192, 1));
        assert_eq!(peer_address(MAX_INDEX), Ipv4Addr::new(169, 254, 255, 255));
        assert_eq!(index_of(Ipv4Addr::new(169, 254, 192, 7)), Some(7));
        assert_eq!(index_of(Ipv4Addr::new(169, 254, 192, 0)), None);
        assert_eq!(index_of(Ipv4Addr::new(169, 254, 169, 254)), None);
        assert_eq!(index_of(Ipv4Addr::new(10, 88, 0, 7)), None);
        // Clear of Tailscale's and Kubernetes' bits.
        assert_eq!(mark(MAX_INDEX) & 0x00ff_c000, 0);
        assert_eq!(allocate(&[]), Some(1));
        assert_eq!(allocate(&[1, 2, 4]), Some(3));
    }

    #[test]
    fn the_flow_marks_only_metadata_requests_from_its_port() {
        assert_eq!(
            flow(5, 3),
            "cookie=0x6d000003,priority=200,in_port=5,tcp,nw_dst=169.254.169.254,tp_dst=80,actions=set_field:0x6d000003->pkt_mark,NORMAL"
        );
    }

    #[test]
    fn nft_rules() {
        assert_eq!(nft_chains(&[], &[1]), "");
        assert_eq!(input_rules(&[]), "");
        let c = nft_chains(&["gxbr-nat"], &[1, 2]);
        assert!(c.contains("iifname { \"gxbr-nat\" } ip daddr 169.254.169.254 tcp dport 80 meta mark & 0xffffc000 == 0x6d000000 dnat ip to 169.254.169.254:8775"), "{c}");
        assert!(
            c.contains("0x6d000001 : 169.254.192.1, 0x6d000002 : 169.254.192.2"),
            "{c}"
        );
        assert!(c.contains("snat ip to meta mark map @metadata_peer"), "{c}");
        // No ports yet: no map (nft refuses an empty one in a map lookup).
        assert!(!nft_chains(&["gxbr-nat"], &[]).contains("metadata_peer"));
        let r = input_rules(&["gxbr-nat"]);
        assert!(
            r.ends_with("ip daddr 169.254.169.254 tcp dport 8775 drop\n"),
            "{r}"
        );
    }
}
