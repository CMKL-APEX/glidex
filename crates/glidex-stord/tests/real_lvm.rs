//! Against a running glidex-stord and a real volume group (spec/24-storage.md
//! §19, H4–H5). Ignored by default; run with
//!
//! ```text
//! GLIDEX_STORD_SOCKET=/run/glidex-stord/stord.sock GLIDEX_STORD_VG=gx-test \
//!   GLIDEX_STORD_POOL=pool cargo test -p glidex-stord --test real_lvm -- --ignored
//! ```
//!
//! The VG must be in that stord's `allowed_vgs`, and must be a throwaway.

use glidex_stord::client::{Client, ClientError};
use glidex_stord::proto::{Activated, Op, PoolStatus, Volume};
use serde_json::Value;
use std::os::unix::fs::MetadataExt;

const M: u64 = 1 << 20;

fn env(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| panic!("set {k}"))
}

#[test]
#[ignore]
fn a_real_pool_takes_images_snapshots_and_gives_them_back() {
    let sock = env("GLIDEX_STORD_SOCKET");
    let vg = env("GLIDEX_STORD_VG");
    let pool = env("GLIDEX_STORD_POOL");
    let mut c = Client::connect(std::path::Path::new(&sock)).unwrap();
    let st: PoolStatus = c
        .call(Op::PoolStatus {
            vg: vg.clone(),
            pool: pool.clone(),
        })
        .unwrap();
    assert!(st.usable, "{st:?}");

    let tag = std::process::id();
    let img = format!("gx-i-test-{tag}");
    let disk = format!("gx-d-test-{tag}");
    let a: Activated = c
        .call(Op::CreateVolume {
            vg: vg.clone(),
            pool: pool.clone(),
            name: img.clone(),
            size_bytes: 64 * M,
        })
        .unwrap();
    let meta = std::fs::metadata(&a.device).unwrap();
    assert_eq!(meta.mode() & 0o777, 0o600, "{a:?}");
    let _: Value = c
        .call(Op::SealVolume {
            vg: vg.clone(),
            name: img.clone(),
        })
        .unwrap();
    let d: Activated = c
        .call(Op::SnapshotVolume {
            vg: vg.clone(),
            origin: img.clone(),
            name: disk.clone(),
            size_bytes: 128 * M,
        })
        .unwrap();
    assert_eq!(d.size_bytes, 128 * M, "{d:?}");
    // Writable by the device's new owner (this test runs as that user).
    std::fs::OpenOptions::new()
        .write(true)
        .open(&d.path)
        .unwrap();
    let vols: Vec<Volume> = c.call(Op::ListVolumes { vg: vg.clone() }).unwrap();
    let v = vols.iter().find(|v| v.name == disk).unwrap();
    assert_eq!(v.origin.as_deref(), Some(img.as_str()));
    assert!(v.active && !v.read_only, "{v:?}");

    // The image stays while the disk uses it.
    match c.call_value(Op::DeleteVolume {
        vg: vg.clone(),
        name: img.clone(),
    }) {
        Err(ClientError::Remote(e)) => assert_eq!(e.code, "busy"),
        other => panic!("{other:?}"),
    }
    let e: Activated = c
        .call(Op::ExtendVolume {
            vg: vg.clone(),
            name: disk.clone(),
            size_bytes: 256 * M,
        })
        .unwrap();
    assert_eq!(e.size_bytes, 256 * M);
    let _: Value = c
        .call(Op::Deactivate {
            vg: vg.clone(),
            name: disk.clone(),
        })
        .unwrap();
    let again: Activated = c
        .call(Op::Activate {
            vg: vg.clone(),
            name: disk.clone(),
        })
        .unwrap();
    // A fresh activation is handed over again (H5).
    assert_eq!(
        std::fs::metadata(&again.device).unwrap().uid(),
        nix::unistd::getuid().as_raw()
    );
    for name in [&disk, &img] {
        let _: Value = c
            .call(Op::DeleteVolume {
                vg: vg.clone(),
                name: name.clone(),
            })
            .unwrap();
    }
    let vols: Vec<Volume> = c.call(Op::ListVolumes { vg }).unwrap();
    assert!(!vols.iter().any(|v| v.name == disk || v.name == img));
}
