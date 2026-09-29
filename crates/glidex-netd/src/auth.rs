//! Peer authorization for the full socket (spec §7.2).

use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
use nix::unistd::{getgrouplist, Gid, Group, Uid, User};
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

/// Root, or a member (primary or supplementary) of `group_gid`.
pub fn authorized(peer: &Peer, group_gid: Option<u32>) -> bool {
    if peer.uid == 0 {
        return true;
    }
    let Some(gid) = group_gid else {
        return false;
    };
    if peer.gid == gid {
        return true;
    }
    let Ok(Some(user)) = User::from_uid(Uid::from_raw(peer.uid)) else {
        return false;
    };
    let Ok(name) = CString::new(user.name) else {
        return false;
    };
    getgrouplist(&name, Gid::from_raw(peer.gid))
        .map(|groups| groups.iter().any(|g| g.as_raw() == gid))
        .unwrap_or(false)
}
