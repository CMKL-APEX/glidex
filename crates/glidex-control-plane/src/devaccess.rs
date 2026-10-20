//! Whether this host's VMs may open a device (`/dev/kvm`, `/dev/vfio/<n>`,
//! `/dev/vhost-net`). The VM units run as the control plane's user with its
//! groups but allow devices the control plane's own unit closes
//! (`DevicePolicy=closed`). The kernel's device policy answers `access(2)`
//! too, so from here an `access` check says no for every device; the
//! device file's owner, group and mode say what a VM unit would get.

use std::path::Path;

/// Whether a VM of this host could open device `path` read-write.
pub fn vm_can_open(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(m) = std::fs::metadata(path) else {
        return false;
    };
    let mut gids: Vec<u32> = nix::unistd::getgroups()
        .unwrap_or_default()
        .into_iter()
        .map(|g| g.as_raw())
        .collect();
    gids.push(nix::unistd::getegid().as_raw());
    mode_allows_rw(
        m.mode(),
        m.uid(),
        m.gid(),
        nix::unistd::geteuid().as_raw(),
        &gids,
    )
}

/// Unix permission bits for read and write: root always; else the owner's,
/// the group's or the others' bits, the first class that applies.
fn mode_allows_rw(mode: u32, file_uid: u32, file_gid: u32, uid: u32, gids: &[u32]) -> bool {
    let rw = if uid == 0 {
        return true;
    } else if uid == file_uid {
        (mode >> 6) & 0o6
    } else if gids.contains(&file_gid) {
        (mode >> 3) & 0o6
    } else {
        mode & 0o6
    };
    rw == 0o6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfio_access_follows_owner_group_and_mode() {
        const KVM: u32 = 108;
        // prism1's 99-vfio.rules: root:kvm 0666, open to everyone.
        assert!(mode_allows_rw(0o20666, 0, KVM, 994, &[994]));
        // glidex's own rule: root:glidex 0660, for members only.
        assert!(mode_allows_rw(0o20660, 0, 994, 994, &[994, KVM]));
        assert!(!mode_allows_rw(0o20660, 0, 994, 1000, &[1000]));
        // root:root 0600: only root.
        assert!(!mode_allows_rw(0o20600, 0, 0, 994, &[994, KVM]));
        assert!(mode_allows_rw(0o20600, 0, 0, 0, &[0]));
        // The class that applies decides, even when another would allow.
        assert!(!mode_allows_rw(0o20066, 994, KVM, 994, &[KVM]));
        // Read-only isn't enough.
        assert!(!mode_allows_rw(0o20644, 0, 0, 994, &[994]));
        assert!(!vm_can_open(Path::new("/dev/vfio/no-such-group")));
    }
}
