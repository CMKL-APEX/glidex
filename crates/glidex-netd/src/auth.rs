//! Peer authorization for the full sockets (spec §7.2): who may connect,
//! and which ops each group may send (security spec §8.1).

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::{getgrouplist, Gid, Group, Uid, User};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::net::UnixStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub gid: u32,
    pub pid: i32,
}

pub fn peer(stream: &UnixStream) -> std::io::Result<Peer> {
    let cred = getsockopt(stream, PeerCredentials).map_err(std::io::Error::from)?;
    Ok(Peer {
        uid: cred.uid(),
        gid: cred.gid(),
        pid: cred.pid(),
    })
}

pub fn group_gid(name: &str) -> Option<u32> {
    Group::from_name(name).ok().flatten().map(|g| g.gid.as_raw())
}

/// The peer's groups: primary, plus supplementary from `getgrouplist`
/// (just the primary if the uid has no passwd entry).
pub fn peer_gids(peer: &Peer) -> Vec<u32> {
    let mut gids = vec![peer.gid];
    let Ok(Some(user)) = User::from_uid(Uid::from_raw(peer.uid)) else {
        return gids;
    };
    let Ok(name) = CString::new(user.name) else {
        return gids;
    };
    if let Ok(groups) = getgrouplist(&name, Gid::from_raw(peer.gid)) {
        gids.extend(groups.iter().map(|g| g.as_raw()).filter(|g| *g != peer.gid));
    }
    gids
}

/// Root, or a member (primary or supplementary) of `group_gid`.
pub fn authorized(peer: &Peer, group_gid: Option<u32>) -> bool {
    if peer.uid == 0 {
        return true;
    }
    let Some(gid) = group_gid else {
        return false;
    };
    peer.gid == gid || peer_gids(peer).contains(&gid)
}

// ---- per-op policy (security spec §8.1) -----------------------------------

/// Group name → op patterns it may send on a full socket. A pattern is an
/// op's wire name (`Op::name()`), `*`, or a prefix ending in `*`
/// (`list_*`).
pub type Policy = BTreeMap<String, Vec<String>>;

/// Without a `policy` in `netd.json`: the full socket's group and the
/// admin group (`glidex`, `glidex-admin` unless renamed) may send every op
/// (security decision 2).
pub fn default_policy(group: &str, admin_group: &str) -> Policy {
    [group, admin_group]
        .into_iter()
        .map(|g| (g.to_string(), vec!["*".to_string()]))
        .collect()
}

/// Ops every full-socket peer may send whatever the policy says: the
/// handshake, and `probe`, which the world-accessible status socket
/// serves anyway.
pub const ALWAYS_ALLOWED: &[&str] = &["hello", "probe"];

/// `op` matches `pattern`: equal, or `pattern` is `<prefix>*` and `op`
/// starts with `<prefix>`. A `*` anywhere else is literal, so it never
/// matches a wire name.
pub fn op_matches(pattern: &str, op: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => op.starts_with(prefix),
        None => pattern == op,
    }
}

/// Whether a peer may send `op`: root always; otherwise some group in
/// `policy` that the peer belongs to (`member_of`) grants it.
pub fn allows(policy: &Policy, uid: u32, op: &str, member_of: impl Fn(&str) -> bool) -> bool {
    uid == 0
        || ALWAYS_ALLOWED.contains(&op)
        || policy
            .iter()
            .any(|(group, patterns)| patterns.iter().any(|p| op_matches(p, op)) && member_of(group))
}

/// The policy's groups `peer` belongs to, resolved once per connection.
/// Groups that don't exist on the host grant nothing.
pub fn policy_groups(policy: &Policy, peer: &Peer) -> Vec<String> {
    let gids = peer_gids(peer);
    policy
        .keys()
        .filter(|name| group_gid(name).is_some_and(|gid| gids.contains(&gid)))
        .cloned()
        .collect()
}

/// Patterns that can never match an op (a `*` before the end), for a
/// startup warning.
pub fn suspicious_patterns(policy: &Policy) -> Vec<String> {
    policy
        .values()
        .flatten()
        .filter(|p| p.strip_suffix('*').unwrap_or(p).contains('*'))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher() {
        assert!(op_matches("*", "install_ovs"));
        assert!(op_matches("list_*", "list_nat"));
        assert!(op_matches("list_*", "list_vm_ports"));
        assert!(!op_matches("list_*", "release_vm"));
        assert!(op_matches("release_vm", "release_vm"));
        assert!(!op_matches("release_vm", "release_vm_x"));
        assert!(!op_matches("release", "release_vm"), "no implicit prefix match");
        assert!(!op_matches("*_vm", "release_vm"), "only a trailing * is a glob");
        assert!(!op_matches("", "hello"));
    }

    #[test]
    fn decision() {
        let policy: Policy = serde_json::from_str(
            r#"{"glidex": ["list_*", "attach_vm_port", "detach_vm_port"], "netops": ["ensure_*"], "glidex-admin": ["*"]}"#,
        )
        .unwrap();
        let only = |groups: &'static [&'static str]| move |g: &str| groups.contains(&g);

        assert!(allows(&policy, 1000, "list_bridges", only(&["glidex"])));
        assert!(allows(&policy, 1000, "attach_vm_port", only(&["glidex"])));
        assert!(!allows(&policy, 1000, "install_ovs", only(&["glidex"])));
        assert!(!allows(&policy, 1000, "ensure_nat", only(&["glidex"])));
        // Grants from several groups add up.
        assert!(allows(&policy, 1000, "ensure_nat", only(&["glidex", "netops"])));
        assert!(allows(&policy, 1000, "install_ovs", only(&["glidex-admin"])));
        // No matching group: denied, except the handshake and probe.
        assert!(!allows(&policy, 1000, "list_nat", only(&[])));
        assert!(allows(&policy, 1000, "hello", only(&[])));
        assert!(allows(&policy, 1000, "probe", only(&[])));
        // Root is exempt, even from an empty policy.
        assert!(allows(&Policy::new(), 0, "install_ovs", only(&[])));
        assert!(!allows(&Policy::new(), 1000, "list_nat", only(&["glidex"])));

        let default = default_policy("glidex", "glidex-admin");
        assert!(allows(&default, 1000, "install_ovs", only(&["glidex"])));
        assert!(allows(&default, 1000, "delete_uplink", only(&["glidex-admin"])));
        assert!(!allows(&default, 1000, "delete_uplink", only(&["users"])));
    }

    #[test]
    fn warns_about_inner_globs() {
        let policy: Policy = serde_json::from_str(r#"{"g": ["list_*", "*", "*_vm", "a**"]}"#).unwrap();
        assert_eq!(suspicious_patterns(&policy), vec!["*_vm", "a**"]);
    }
}
