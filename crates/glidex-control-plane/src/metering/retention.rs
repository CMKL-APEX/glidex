//! Daily upkeep (spec/metering.md §7.3, §8.5.1): final monthly
//! percentiles, hourly → daily roll-up, and expiry.

use super::ledger::{Ledger, MeteringError, SlotRow, SubjectKind};
use super::query::{bucket, Granularity, GroupKey, Tz};
use super::rates::{bandwidth_p95, compute_p95, disk_io_p95, group_slots};
use crate::config::MeteringConfig;

const DAY: u64 = 86400;

/// The groupings whose final figures are kept per billing month.
const GROUPINGS: [(&str, SubjectKind, &[GroupKey]); 9] = [
    ("bw/nic", SubjectKind::Nic, &[GroupKey::Project, GroupKey::Nic]),
    ("bw/vm", SubjectKind::Nic, &[GroupKey::Project, GroupKey::Vm]),
    ("bw/project", SubjectKind::Nic, &[GroupKey::Project]),
    ("bw/network", SubjectKind::Network, &[GroupKey::Project, GroupKey::Network]),
    ("io/disk", SubjectKind::Disk, &[GroupKey::Project, GroupKey::Disk]),
    ("io/vm", SubjectKind::Disk, &[GroupKey::Project, GroupKey::Vm]),
    ("io/project", SubjectKind::Disk, &[GroupKey::Project]),
    ("cm/vm", SubjectKind::Vm, &[GroupKey::Project, GroupKey::Vm]),
    ("cm/project", SubjectKind::Vm, &[GroupKey::Project]),
];

/// Final 95th percentiles of one billing month `[start, end)`, one
/// value per group, under `<grouping>/<ids>`.
pub fn month_figures(slots: &[(u64, SlotRow)], start: u64, end: u64) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    for (name, kind, group_by) in GROUPINGS {
        let rows: Vec<(u64, SlotRow)> =
            slots.iter().filter(|(_, r)| r.subject.as_ref().is_some_and(|s| s.kind == kind)).cloned().collect();
        for (ids, g) in group_slots(&rows, start, end, group_by) {
            let ids: Vec<String> = ids.into_iter().map(|i| i.unwrap_or_else(|| "-".into())).collect();
            let p95 = match kind {
                SubjectKind::Disk => serde_json::to_value(disk_io_p95(&g)),
                SubjectKind::Vm => serde_json::to_value(compute_p95(&g)),
                _ => serde_json::to_value(bandwidth_p95(&g, kind)),
            }
            .unwrap_or_default();
            out.push((format!("{name}/{}", ids.join("/")), serde_json::json!({ "keys": g.keys, "p95": p95 })));
        }
    }
    out
}

/// Once a day: finalize complete months, roll up and expire.
pub fn run(ledger: &Ledger, cfg: &MeteringConfig, tz: &Tz, now: u64) -> Result<(), MeteringError> {
    let today = (now / DAY).to_string();
    if ledger.meta("retention_day")?.as_deref() == Some(today.as_str()) {
        return Ok(());
    }
    // 1. Final figures for every complete billing month since metering began.
    let complete = ledger.complete_through()?;
    let mut start = bucket(ledger.started_at()? / 1000, Granularity::Month, tz).0;
    loop {
        let end = bucket(start, Granularity::Month, tz).1;
        if end > complete {
            break;
        }
        let flag = format!("finalized/{start}");
        if ledger.meta(&flag)?.is_none() {
            let slots = ledger.scan_slots(start, end, None, |_| true)?;
            for (key, value) in month_figures(&slots, start, end) {
                ledger.put_month_rates(start, &key, &value)?;
            }
            ledger.set_meta(&flag, &now.to_string())?;
        }
        start = end;
    }
    // 2-4. Roll up and expire.
    ledger.roll_up_hours_before(now.saturating_sub(cfg.retention_days * DAY), tz.offset_secs)?;
    ledger.prune_slots_before(now.saturating_sub(cfg.retention_rate_days * DAY))?;
    ledger.prune_daily_before(now.saturating_sub(cfg.retention_daily_days * DAY))?;
    ledger.prune_month_rates_before(now.saturating_sub(cfg.retention_daily_days * DAY))?;
    ledger.set_meta("retention_day", &today)
}

#[cfg(test)]
mod tests {
    use super::super::ledger::{LedgerSettings, Origin, Subject, HOUR_MS};
    use super::super::query::Granularity;
    use super::*;
    use redb::Database;
    use std::sync::Arc;

    /// Days roll up at the billing zone's midnight, so an old billing
    /// month (here Bangkok, +07:00) still sums exactly.
    #[test]
    fn roll_up_keeps_billing_months_exact() {
        use super::super::query::{aggregate, month_bounds, parse_tz, Query};
        let dir = tempfile::TempDir::new().unwrap();
        let l = Ledger::new(Arc::new(Database::create(dir.path().join("t.db")).unwrap()), LedgerSettings::from_secs(30, 120)).unwrap();
        let bkk = parse_tz("+07:00").unwrap();
        let (oct, nov) = month_bounds("2026-10", &bkk).unwrap();
        l.set_started_at_for_test((oct - 2 * DAY) * 1000);
        let s = Subject::new(SubjectKind::Vm, "vm", "vm", Some("p".into()));
        // 1 unit per hour from two days before October to two days after.
        let mut r = l.begin_round((nov + 3 * DAY) * 1000).unwrap();
        let mut t = oct - 2 * DAY;
        r.counter(&s, "cpu.used", "i", 0, t * 1000, Origin::ZeroAt(t * 1000)).unwrap();
        let mut v = 0;
        while t < nov + 2 * DAY {
            t += 3600;
            v += 1000;
            r.counter(&s, "cpu.used", "i", v, t * 1000, Origin::Unknown).unwrap();
        }
        l.commit(r).unwrap();
        let month = |l: &Ledger| {
            let rows = l.scan(oct, nov, None).unwrap();
            let q = Query { from: oct, to: nov, granularity: Granularity::Month, tz: bkk.clone(), group_by: vec![], meters: None, now: nov };
            aggregate(&rows, &q).iter().map(|r| r.meters["cpu.used"]).sum::<u64>()
        };
        let before = month(&l);
        assert_eq!(before, 744 * 1000, "31 days of hours");
        l.roll_up_hours_before(nov + 2 * DAY, bkk.offset_secs).unwrap();
        assert_eq!(month(&l), before, "exact after the roll-up");
    }

    #[test]
    fn months_are_finalized_once_and_slots_expire() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Arc::new(Database::create(dir.path().join("t.db")).unwrap());
        let l = Ledger::new(db, LedgerSettings::from_secs(30, 120)).unwrap();
        // 2026-09-30T23:00Z: the last hour of September.
        let t0 = 1_790_809_200_000u64;
        l.set_started_at_for_test(t0);
        let mut nic = Subject::new(SubjectKind::Nic, "vm.0", "web/nic0", Some("p1".into()));
        nic.vm_id = Some("vm".into());
        let mut r = l.begin_round(t0).unwrap();
        r.counter(&nic, "net.rx_bytes", "port", 0, t0, Origin::ZeroAt(t0)).unwrap();
        let mut v = 0;
        let mut t = t0;
        while t < t0 + 2 * HOUR_MS {
            t += 30_000;
            v += 37_500_000; // 10 Mbps
            r.counter(&nic, "net.rx_bytes", "port", v, t, Origin::Unknown).unwrap();
        }
        l.commit(r).unwrap();
        let cfg = MeteringConfig::default();
        let now = (t0 + 3 * HOUR_MS) / 1000;
        // Close the hours.
        l.commit(l.begin_round(now * 1000).unwrap()).unwrap();
        run(&l, &cfg, &Tz::utc(), now).unwrap();
        let sept = 1_788_220_800; // 2026-09-01T00:00Z
        let figures = l.month_rates(sept, "bw/").unwrap();
        let vm = figures.iter().find(|(k, _)| k == "bw/vm/p1/vm").expect("VM figure");
        assert_eq!(vm.1["p95"]["rx_mbps"], 10.0);
        assert_eq!(vm.1["p95"]["slots"]["counted"], 12, "September's last hour only");
        assert!(l.meta(&format!("finalized/{sept}")).unwrap().is_some());
        // October isn't complete: not finalized. A second run the same day is a no-op.
        assert!(l.month_rates(1_790_812_800, "bw/").unwrap().is_empty());
        run(&l, &cfg, &Tz::utc(), now).unwrap();
        // Long after: slots expire, the month's figures stay.
        run(&l, &cfg, &Tz::utc(), now + 200 * DAY).unwrap();
        assert!(l.scan_slots(0, u64::MAX / 10, None, |_| true).unwrap().is_empty());
        assert_eq!(l.month_rates(sept, "bw/vm/").unwrap().len(), 1);
    }
}
