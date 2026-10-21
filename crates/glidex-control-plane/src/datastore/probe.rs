//! What a node finds at its path to a datastore (spec/storage.md §4.1).

use super::record::{Datastore, NodeRow};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

/// The file that says a shared directory is a datastore's (S5).
pub const MARKER: &str = ".glidex-datastore";

/// What the marker holds: whose the directory is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Marker {
    pub datastore: String,
    /// Empty on a standalone host.
    pub cluster: String,
    pub created_at: u64,
}

/// Look at `ds` from `node`: its directory exists, is under the root (S11),
/// is this datastore's when shared (its marker; with `may_claim`, written
/// here into an empty directory), and takes a `disks/` directory this node
/// can write. Blocks.
pub fn probe(
    ds: &Datastore,
    node: &str,
    cluster: &str,
    may_claim: bool,
    root: &Path,
    now: u64,
) -> NodeRow {
    let path = ds.path_for(node).to_string();
    let mut row = NodeRow {
        path: path.clone(),
        checked_at: now,
        ..Default::default()
    };
    let fail = |mut row: NodeRow, reason: &str, message: String| {
        row.reason = reason.into();
        row.message = message;
        row
    };
    let p = Path::new(&path);
    if p == root || !p.starts_with(root) {
        let msg = format!("{path} is not under {}", root.display());
        return fail(row, "OutsideRoot", msg);
    }
    match std::fs::metadata(p) {
        Err(e) => return fail(row, "Missing", format!("{path}: {e}")),
        Ok(m) if !m.is_dir() => return fail(row, "NotADirectory", path),
        Ok(_) => {}
    }
    if ds.is_shared()
        && let Err((reason, message)) = claim(p, &ds.id, cluster, may_claim, now)
    {
        return fail(row, reason, message);
    }
    let disks = p.join("disks");
    let dirs = if ds.is_shared() {
        vec![disks.clone(), p.join(".locks")]
    } else {
        vec![disks.clone()]
    };
    for d in dirs {
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&d)
        {
            return fail(row, "ReadOnly", format!("{}: {e}", d.display()));
        }
    }
    // Writable for real: a read-only mount or a sandbox shows up only here.
    let test = disks.join(".gx-probe");
    if let Err(e) = std::fs::write(&test, b"") {
        return fail(row, "ReadOnly", format!("{}: {e}", disks.display()));
    }
    let _ = std::fs::remove_file(&test);
    (row.total_bytes, row.free_bytes) = space(p);
    row.reachable = true;
    if ds.is_shared()
        && let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo")
        && let Some((fstype, options)) = mount_of(p, &info)
    {
        row.warnings = mount_warnings(&fstype, &options);
    }
    row
}

/// The filesystem type and its options (mount and superblock options
/// together) of the mount holding `p`, from `/proc/self/mountinfo`.
fn mount_of(p: &Path, mountinfo: &str) -> Option<(String, Vec<String>)> {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let mut best: Option<(usize, String, Vec<String>)> = None;
    for line in mountinfo.lines() {
        // id parent dev root mountpoint options [optional…] - fstype source super
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let pre: Vec<&str> = pre.split(' ').collect();
        let post: Vec<&str> = post.split(' ').collect();
        let (Some(point), Some(opts), Some(fstype)) = (pre.get(4), pre.get(5), post.first()) else {
            continue;
        };
        let point = unescape(point);
        if !p.starts_with(&point) || best.as_ref().is_some_and(|b| b.0 > point.len()) {
            continue;
        }
        let mut options: Vec<String> = opts.split(',').map(str::to_string).collect();
        if let Some(sup) = post.get(2) {
            options.extend(sup.split(',').map(str::to_string));
        }
        best = Some((point.len(), fstype.to_string(), options));
    }
    best.map(|(_, t, o)| (t, o))
}

/// A mount point as mountinfo writes it: space, tab, newline and
/// backslash as octal escapes.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(c) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(c);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What is risky for disks about a shared mount (spec/storage.md §17):
/// on NFS, `soft` (a server outage fails guests' writes, which can leave
/// a qcow2 corrupt) and locks kept on this node only (`nolock`,
/// `local_lock=`), with which the volume locks (S16) no longer keep two
/// nodes' hypervisors off one disk.
fn mount_warnings(fstype: &str, options: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if !fstype.starts_with("nfs") {
        return out;
    }
    let has = |o: &str| options.iter().any(|x| x == o);
    if has("soft") || has("softerr") {
        out.push(
            "mounted soft: an NFS server outage fails the guests' disk writes; mount it hard"
                .to_string(),
        );
    }
    let local = options
        .iter()
        .find_map(|o| o.strip_prefix("local_lock="))
        .filter(|v| *v != "none");
    if has("nolock") || local.is_some() {
        out.push(format!(
            "NFS locks are local to this node ({}): disk locks don't keep other nodes off a disk; mount without nolock or local_lock",
            local.map(|v| format!("local_lock={v}")).unwrap_or_else(|| "nolock".into())
        ));
    }
    out
}

/// Make sure the shared directory `p` is datastore `id`'s of `cluster`: its
/// marker says so, or, with `may_claim`, it is empty and this node writes
/// the marker (S5).
///
/// Only one node may claim, and only before any node reaches the datastore
/// (`VmManager::probe_datastores`): an unmounted mount point is an empty
/// directory too, on the local disk, and a node that claimed its own would
/// put disks where no other node sees them. Every other node must find the
/// marker. The marker is created with `O_EXCL`, so of two clusters claiming
/// one export at once only one wins (checked across NFSv4 clients,
/// spec/storage.md §19 H3).
fn claim(
    p: &Path,
    id: &str,
    cluster: &str,
    may_claim: bool,
    now: u64,
) -> Result<(), (&'static str, String)> {
    let path = p.join(MARKER);
    for _ in 0..2 {
        match std::fs::read(&path) {
            Ok(bytes) => {
                return match serde_json::from_slice::<Marker>(&bytes) {
                    Ok(m) if m.datastore == id && m.cluster == cluster => Ok(()),
                    Ok(m) => Err((
                        "ForeignMarker",
                        format!(
                            "{} is datastore {} of cluster {:?}",
                            p.display(),
                            m.datastore,
                            m.cluster
                        ),
                    )),
                    Err(e) => Err(("ForeignMarker", format!("{}: {e}", path.display()))),
                };
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(("ReadOnly", format!("{}: {e}", path.display()))),
        }
        if !may_claim {
            return Err((
                "NoMarker",
                format!(
                    "{} has no {MARKER}: is the datastore mounted on this node?",
                    p.display()
                ),
            ));
        }
        // Only an empty directory is claimed: files without a marker are
        // someone else's (or a datastore whose marker was lost).
        let others = std::fs::read_dir(p)
            .map_err(|e| ("ReadOnly", format!("{}: {e}", p.display())))?
            .flatten()
            .filter(|e| e.file_name() != "lost+found")
            .count();
        if others > 0 {
            return Err((
                "NotEmpty",
                format!("{} has files but no {MARKER}", p.display()),
            ));
        }
        let marker = Marker {
            datastore: id.into(),
            cluster: cluster.into(),
            created_at: now,
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&path)
        {
            Ok(mut f) => {
                let body = serde_json::to_vec(&marker).unwrap_or_default();
                return f
                    .write_all(&body)
                    .and_then(|_| f.sync_all())
                    .map_err(|e| ("ReadOnly", format!("{}: {e}", path.display())));
            }
            // Another node claimed it first: read what it wrote.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(("ReadOnly", format!("{}: {e}", path.display()))),
        }
    }
    Err(("ForeignMarker", format!("{}: unreadable", path.display())))
}

/// Total and free bytes of the filesystem holding `p` (0, 0 if unknown).
fn space(p: &Path) -> (u64, u64) {
    match nix::sys::statvfs::statvfs(p) {
        Ok(s) => {
            let frag = s.fragment_size();
            (s.blocks() * frag, s.blocks_available() * frag)
        }
        Err(_) => (0, 0),
    }
}

/// The built-in `local` datastore: the node's own disk directory, which
/// the control plane made at start; only its size is news.
pub fn probe_dir(dir: &Path, now: u64) -> NodeRow {
    let mut row = NodeRow {
        path: dir.to_string_lossy().into_owned(),
        checked_at: now,
        reachable: true,
        ..Default::default()
    };
    (row.total_bytes, row.free_bytes) = space(dir);
    row
}

/// Whether `new` says something `old` doesn't: worth a write. Free space
/// alone counts only when it moved by more than 1 % of the total, so a
/// busy datastore doesn't write every minute.
pub fn worth_writing(old: Option<&NodeRow>, new: &NodeRow) -> bool {
    let Some(old) = old else { return true };
    let moved = old.free_bytes.abs_diff(new.free_bytes);
    old.reachable != new.reachable
        || old.reason != new.reason
        || old.warnings != new.warnings
        || old.image_volumes != new.image_volumes
        || old.message != new.message
        || old.path != new.path
        || old.total_bytes != new.total_bytes
        || moved > new.total_bytes / 100
}

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::record::DriverSpec;
    use std::collections::BTreeMap;

    fn ds(path: &str) -> Datastore {
        Datastore {
            id: "d".into(),
            name: "d".into(),
            driver: DriverSpec::Dir {
                path: path.into(),
                node_paths: BTreeMap::new(),
            },
            nodes: Vec::new(),
            default_for_disks: false,
            overcommit: true,
            created_at: 0,
        }
    }

    #[test]
    fn probes_say_why() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let here = tmp.path().join("fast");
        std::fs::create_dir(&here).unwrap();
        let ok = probe(&ds(here.to_str().unwrap()), "n1", "", false, root, 7);
        assert!(ok.reachable, "{ok:?}");
        assert!(ok.total_bytes > 0 && ok.free_bytes <= ok.total_bytes);
        assert!(here.join("disks").is_dir());
        assert!(!here.join("disks/.gx-probe").exists());

        let gone = probe(
            &ds(tmp.path().join("gone").to_str().unwrap()),
            "n1",
            "",
            false,
            root,
            7,
        );
        assert_eq!((gone.reachable, gone.reason.as_str()), (false, "Missing"));
        assert_eq!(
            probe(&ds("/elsewhere"), "n1", "", false, root, 7).reason,
            "OutsideRoot"
        );
        std::fs::write(tmp.path().join("file"), b"").unwrap();
        let file = probe(
            &ds(tmp.path().join("file").to_str().unwrap()),
            "n1",
            "",
            false,
            root,
            7,
        );
        assert_eq!(file.reason, "NotADirectory");

        // A node's own path wins.
        let mut d = ds("/elsewhere");
        d.driver
            .node_paths_mut()
            .insert("n2".into(), here.to_str().unwrap().into());
        assert!(probe(&d, "n2", "", false, root, 7).reachable);
        assert!(!probe(&d, "n1", "", false, root, 7).reachable);
    }

    /// A shared directory is claimed once, by the node allowed to, and
    /// found by the rest through its marker (S5).
    #[test]
    fn a_shared_directory_is_claimed_once_and_found_by_its_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let dir = root.join("nfs");
        std::fs::create_dir(&dir).unwrap();
        let mut d = ds(dir.to_str().unwrap());
        d.driver = DriverSpec::SharedDir {
            path: dir.to_str().unwrap().into(),
            node_paths: BTreeMap::new(),
        };
        // Not the claimer: an empty directory may be a missing mount.
        let r = probe(&d, "n2", "c1", false, root, 7);
        assert_eq!((r.reachable, r.reason.as_str()), (false, "NoMarker"));
        assert!(!dir.join(MARKER).exists());
        // The claimer writes the marker; then everyone finds it.
        assert!(probe(&d, "n1", "c1", true, root, 7).reachable);
        let m: Marker = serde_json::from_slice(&std::fs::read(dir.join(MARKER)).unwrap()).unwrap();
        assert_eq!((m.datastore.as_str(), m.cluster.as_str()), ("d", "c1"));
        assert!(probe(&d, "n2", "c1", false, root, 7).reachable);
        assert!(dir.join("disks").is_dir() && dir.join(".locks").is_dir());
        // Another cluster's, or another datastore's: not this one.
        assert_eq!(probe(&d, "n2", "c2", true, root, 7).reason, "ForeignMarker");
        d.id = "other".into();
        assert_eq!(probe(&d, "n2", "c1", true, root, 7).reason, "ForeignMarker");

        // Files but no marker: someone else's, never claimed.
        let busy = root.join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("data"), b"x").unwrap();
        let mut b = ds(busy.to_str().unwrap());
        b.driver = DriverSpec::SharedDir {
            path: busy.to_str().unwrap().into(),
            node_paths: BTreeMap::new(),
        };
        assert_eq!(probe(&b, "n1", "c1", true, root, 7).reason, "NotEmpty");
        assert!(!busy.join(MARKER).exists());
    }

    #[test]
    fn risky_nfs_mounts_are_warned_about() {
        let info = "\
22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 22 0:40 / /srv/glidex/nfs\\040a rw,relatime shared:9 - nfs4 fs:/e rw,vers=4.2,soft,local_lock=none,addr=10.0.0.1
41 22 0:41 / /srv/glidex/v3 rw,relatime - nfs fs:/f rw,vers=3,hard,nolock,local_lock=all
42 22 0:42 / /srv/glidex/ok rw,relatime - nfs4 fs:/g rw,vers=4.2,hard,local_lock=none
";
        let at = |p: &str| {
            let (t, o) = mount_of(Path::new(p), info).unwrap();
            mount_warnings(&t, &o)
        };
        let soft = at("/srv/glidex/nfs a/disks");
        assert_eq!(soft.len(), 1, "{soft:?}");
        assert!(soft[0].contains("soft"));
        let v3 = at("/srv/glidex/v3");
        assert_eq!(v3.len(), 1, "{v3:?}");
        assert!(v3[0].contains("local_lock=all"), "{v3:?}");
        assert!(at("/srv/glidex/ok").is_empty());
        // Not NFS: the root filesystem.
        assert!(at("/srv/glidex/other").is_empty());
        let mut a = NodeRow::default();
        let b = NodeRow {
            warnings: vec!["x".into()],
            ..Default::default()
        };
        assert!(worth_writing(Some(&a), &b));
        a.warnings = b.warnings.clone();
        assert!(!worth_writing(Some(&a), &b));
    }

    #[test]
    fn small_free_space_changes_are_not_written() {
        let a = NodeRow {
            reachable: true,
            total_bytes: 1000,
            free_bytes: 500,
            ..Default::default()
        };
        let mut b = a.clone();
        b.free_bytes = 495;
        assert!(!worth_writing(Some(&a), &b));
        b.free_bytes = 480;
        assert!(worth_writing(Some(&a), &b));
        assert!(worth_writing(None, &b));
    }
}
