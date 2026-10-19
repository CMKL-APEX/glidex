//! What a node finds at its path to a datastore (spec/storage.md §4.1).

use super::record::{Datastore, NodeRow};
use std::path::Path;

/// Look at `ds` from `node`: its directory exists, is under the root (S11),
/// and takes a `disks/` directory this node can write. Blocks.
pub fn probe(ds: &Datastore, node: &str, root: &Path, now: u64) -> NodeRow {
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
    let disks = p.join("disks");
    if let Err(e) = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&disks)
    {
        return fail(row, "ReadOnly", format!("{}: {e}", disks.display()));
    }
    // Writable for real: a read-only mount or a sandbox shows up only here.
    let test = disks.join(".gx-probe");
    if let Err(e) = std::fs::write(&test, b"") {
        return fail(row, "ReadOnly", format!("{}: {e}", disks.display()));
    }
    let _ = std::fs::remove_file(&test);
    (row.total_bytes, row.free_bytes) = space(p);
    row.reachable = true;
    row
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
        || old.message != new.message
        || old.path != new.path
        || old.total_bytes != new.total_bytes
        || moved > new.total_bytes / 100
}

use std::os::unix::fs::DirBuilderExt;

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
        let ok = probe(&ds(here.to_str().unwrap()), "n1", root, 7);
        assert!(ok.reachable, "{ok:?}");
        assert!(ok.total_bytes > 0 && ok.free_bytes <= ok.total_bytes);
        assert!(here.join("disks").is_dir());
        assert!(!here.join("disks/.gx-probe").exists());

        let gone = probe(
            &ds(tmp.path().join("gone").to_str().unwrap()),
            "n1",
            root,
            7,
        );
        assert_eq!((gone.reachable, gone.reason.as_str()), (false, "Missing"));
        assert_eq!(
            probe(&ds("/elsewhere"), "n1", root, 7).reason,
            "OutsideRoot"
        );
        std::fs::write(tmp.path().join("file"), b"").unwrap();
        let file = probe(
            &ds(tmp.path().join("file").to_str().unwrap()),
            "n1",
            root,
            7,
        );
        assert_eq!(file.reason, "NotADirectory");

        // A node's own path wins.
        let mut d = ds("/elsewhere");
        let DriverSpec::Dir { node_paths, .. } = &mut d.driver;
        node_paths.insert("n2".into(), here.to_str().unwrap().into());
        assert!(probe(&d, "n2", root, 7).reachable);
        assert!(!probe(&d, "n1", root, 7).reachable);
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
