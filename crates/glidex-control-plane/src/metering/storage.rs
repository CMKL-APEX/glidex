//! Stored size (spec/metering.md §5.3): what a disk's file takes on the
//! host (`qemu-img info` `actual-size`), and each image's file, which is
//! shared by every project and metered at host level.

use super::ledger::{MeteringError, Round, Subject, SubjectKind};
use crate::images::{Disk, DiskPhase, Image, ImageStatus};

const MIB: u64 = 1 << 20;

/// Disks worth measuring: their file exists and isn't being rewritten.
pub fn measurable(d: &Disk) -> bool {
    matches!(d.phase, DiskPhase::Ready) && d.size_bytes > 0
}

/// Record measured disk sizes (`disk.stored`) and ready images
/// (`image.stored`) as gauges held until the next pass.
pub fn record(round: &mut Round, disks: &[(Disk, u64)], images: &[Image], now: u64) -> Result<(), MeteringError> {
    for (d, actual) in disks {
        let mut s = Subject::new(SubjectKind::Disk, d.id.clone(), d.name.clone(), (!d.project.is_empty()).then(|| d.project.clone()));
        s.vm_id = d.attached_to.clone();
        round.gauge_run(&s, "disk.stored", "disk", d.created_at * 1000, actual.div_ceil(MIB), 0, now, None)?;
    }
    for i in images {
        let s = Subject::new(SubjectKind::Image, i.id.clone(), i.name.clone(), None);
        if matches!(i.status, ImageStatus::Ready) && i.file_size_bytes > 0 {
            round.gauge_run(&s, "image.stored", "image", i.created_at * 1000, i.file_size_bytes.div_ceil(MIB), 0, now, None)?;
        } else {
            round.gauge_end_at(&s, "image.stored", now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{combine, Ledger, LedgerSettings, HOUR_MS};
    use super::*;
    use redb::Database;
    use std::sync::Arc;

    #[test]
    fn stored_sizes_are_held_between_passes() {
        let dir = tempfile::TempDir::new().unwrap();
        let l = Ledger::new(Arc::new(Database::create(dir.path().join("t.db")).unwrap()), LedgerSettings::from_secs(30, 120)).unwrap();
        l.set_started_at_for_test(0);
        let t0 = 1_791_000_000 / 3600 * HOUR_MS;
        let d: Disk = serde_json::from_value(serde_json::json!({
            "id": "d1", "name": "root", "project": "p1", "format": "qcow2", "size_bytes": 10u64 << 30,
            "origin": { "kind": "blank" }, "created_at": t0 / 1000, "phase": "ready", "attached_to": "vm-1",
        }))
        .unwrap();
        let img: Image = serde_json::from_value(serde_json::json!({
            "id": "i1", "name": "ubuntu", "kind": "disk", "source": { "kind": "catalog", "key": "ubuntu", "url": "https://x", "version": "1" },
            "status": { "state": "ready" }, "format": "qcow2", "virtual_size_bytes": 1u64 << 30,
            "file_size_bytes": 600u64 << 20, "sha256": "x", "arch": "x86_64", "created_at": t0 / 1000,
        }))
        .unwrap_or_else(|e| panic!("image fixture: {e}"));
        let mut r = l.begin_round(t0 + 900_000).unwrap();
        // 200 MiB written at first, 1 GiB fifteen minutes later.
        record(&mut r, &[(d.clone(), 200 << 20)], std::slice::from_ref(&img), t0).unwrap();
        record(&mut r, &[(d, 1 << 30)], &[img], t0 + 900_000).unwrap();
        l.commit(r).unwrap();
        let rows = l.scan(0, u64::MAX / 10, None).unwrap();
        let t = combine(&rows);
        assert_eq!(t["disk.stored"], 200 * 900, "the first level held for 15 minutes");
        assert_eq!(t["image.stored"], 600 * 900);
        let img_row = rows.iter().find(|r| r.subject.kind == SubjectKind::Image).unwrap();
        assert_eq!(img_row.subject.project, None, "images are host-level");
    }
}
