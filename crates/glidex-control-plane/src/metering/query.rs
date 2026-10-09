//! Reading usage back (spec/metering.md §8): buckets in a fixed-offset
//! time zone, grouping, derived meters and presentation units.

use super::ledger::{is_max_meter, Flag, SubjectKind, UsageRecord};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// A fixed UTC offset, whole hours (§8.4): hourly rows then fall wholly
/// inside one day and one month.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tz {
    pub name: String,
    pub offset_secs: i64,
}

impl Tz {
    pub fn utc() -> Self {
        Self { name: "UTC".into(), offset_secs: 0 }
    }
}

const ZONEINFO: &str = "/usr/share/zoneinfo";

/// `UTC`, `±HH:00`, or an IANA name whose rules are a single fixed,
/// whole-hour offset (no daylight saving time), e.g. `Asia/Bangkok`.
pub fn parse_tz(s: &str) -> Result<Tz, String> {
    parse_tz_in(s, Path::new(ZONEINFO))
}

fn parse_tz_in(s: &str, zoneinfo: &Path) -> Result<Tz, String> {
    let named = |offset_secs| Ok(Tz { name: s.to_string(), offset_secs });
    if matches!(s, "UTC" | "Etc/UTC" | "Z" | "GMT" | "Etc/GMT") {
        return named(0);
    }
    if let Some(rest) = s.strip_prefix('+').or_else(|| s.strip_prefix('-')) {
        let (h, m) = rest.split_once(':').unwrap_or((rest, "00"));
        let (h, m): (i64, i64) = (h.parse().map_err(|_| format!("bad offset {s}"))?, m.parse().map_err(|_| format!("bad offset {s}"))?);
        if m != 0 || h > 14 {
            return Err(format!("{s}: the billing time zone must be a whole number of hours from UTC"));
        }
        return named(if s.starts_with('-') { -h * 3600 } else { h * 3600 });
    }
    let valid = !s.is_empty() && !s.contains("..") && s.chars().all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c));
    if !valid {
        return Err(format!("{s}: not a time zone name"));
    }
    let data = std::fs::read(zoneinfo.join(s)).map_err(|_| format!("{s}: unknown time zone"))?;
    let offset = tzif_fixed_offset(&data).map_err(|e| format!("{s}: {e}"))?;
    named(offset)
}

/// The offset of a TZif (v2+) file whose footer POSIX rule has no DST.
fn tzif_fixed_offset(data: &[u8]) -> Result<i64, String> {
    if !data.starts_with(b"TZif") {
        return Err("not a TZif file".into());
    }
    // The footer is the last line: "\n<POSIX TZ>\n".
    let text = data.strip_suffix(b"\n").ok_or("no POSIX rule (TZif v1)")?;
    let start = text.iter().rposition(|&b| b == b'\n').ok_or("no POSIX rule")? + 1;
    let rule = std::str::from_utf8(&text[start..]).map_err(|_| "bad POSIX rule")?;
    // std name: <...> or letters.
    let rest = if let Some(r) = rule.strip_prefix('<') {
        &r[r.find('>').ok_or("bad POSIX rule")? + 1..]
    } else {
        rule.trim_start_matches(|c: char| c.is_ascii_alphabetic())
    };
    let end = rest.find(|c: char| !(c.is_ascii_digit() || "+-:".contains(c))).unwrap_or(rest.len());
    let (off, dst) = rest.split_at(end);
    if !dst.is_empty() {
        return Err("has daylight saving time; use a zone with a fixed offset".into());
    }
    let (sign, off) = match off.strip_prefix('-') {
        Some(o) => (1, o), // POSIX offsets are west-positive.
        None => (-1, off.trim_start_matches('+')),
    };
    let mut parts = off.split(':');
    let h: i64 = parts.next().and_then(|p| p.parse().ok()).ok_or("bad POSIX offset")?;
    if parts.any(|p| p.parse::<i64>().ok() != Some(0)) {
        return Err("is not a whole number of hours from UTC".into());
    }
    Ok(sign * h * 3600)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Hour,
    Day,
    Month,
}

impl std::str::FromStr for Granularity {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "month" => Ok(Self::Month),
            _ => Err(format!("granularity must be hour, day or month, not {s}")),
        }
    }
}

// Civil dates (proleptic Gregorian), H. Hinnant's algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// `[start, end)` (unix s) of the bucket containing `t`.
pub fn bucket(t: u64, g: Granularity, tz: &Tz) -> (u64, u64) {
    let local = t as i64 + tz.offset_secs;
    let (start, end) = match g {
        Granularity::Hour => {
            let s = local.div_euclid(3600) * 3600;
            (s, s + 3600)
        }
        Granularity::Day => {
            let s = local.div_euclid(86400) * 86400;
            (s, s + 86400)
        }
        Granularity::Month => {
            let (y, m, _) = civil_from_days(local.div_euclid(86400));
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            (days_from_civil(y, m, 1) * 86400, days_from_civil(ny, nm, 1) * 86400)
        }
    };
    ((start - tz.offset_secs).max(0) as u64, (end - tz.offset_secs).max(0) as u64)
}

/// `[start, end)` (unix s) of the calendar month `YYYY-MM` in `tz`.
pub fn month_bounds(month: &str, tz: &Tz) -> Result<(u64, u64), String> {
    let bad = || format!("month must be YYYY-MM, not {month}");
    let (y, m) = month.split_once('-').ok_or_else(bad)?;
    let (y, m): (i64, u32) = (y.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if !(1..=12).contains(&m) || y < 1970 {
        return Err(bad());
    }
    let start = days_from_civil(y, m, 1) * 86400 - tz.offset_secs;
    Ok(bucket(start.max(0) as u64, Granularity::Month, tz))
}

/// `YYYY-MM` of the month containing `t` in `tz`.
pub fn month_label(t: u64, tz: &Tz) -> String {
    let (y, m, _) = civil_from_days((t as i64 + tz.offset_secs).div_euclid(86400));
    format!("{y:04}-{m:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
pub fn rfc3339(t: u64) -> String {
    let (y, m, d) = civil_from_days((t / 86400) as i64);
    let s = t % 86400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", s / 3600, s / 60 % 60, s % 60)
}

/// Unix seconds, or RFC 3339 (`YYYY-MM-DDTHH:MM[:SS](Z|±HH:MM)`).
pub fn parse_time(s: &str) -> Result<u64, String> {
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let bad = || format!("{s}: expected unix seconds or RFC 3339");
    let (date, time) = s.split_once('T').ok_or_else(bad)?;
    let mut d = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, day) = match (d.next(), d.next(), d.next()) {
        (Some(Ok(y)), Some(Ok(m)), Some(Ok(day))) if (1..=12).contains(&m) && (1..=31).contains(&day) => (y, m as u32, day as u32),
        _ => return Err(bad()),
    };
    let (clock, offset) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0)
    } else {
        let i = time.rfind(['+', '-']).ok_or_else(bad)?;
        let (c, o) = time.split_at(i);
        let tz = parse_tz(o).map_err(|_| bad())?;
        (c, tz.offset_secs)
    };
    let mut c = clock.split(':').map(|p| p.split('.').next().unwrap_or(p).parse::<i64>());
    let (h, mi, sec) = match (c.next(), c.next(), c.next()) {
        (Some(Ok(h)), Some(Ok(mi)), sec) => (h, mi, sec.unwrap_or(Ok(0)).map_err(|_| bad())?),
        _ => return Err(bad()),
    };
    let t = days_from_civil(y, m, day) * 86400 + h * 3600 + mi * 60 + sec - offset;
    u64::try_from(t).map_err(|_| bad())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupKey {
    Project,
    Vm,
    Disk,
    Nic,
    Network,
}

impl std::str::FromStr for GroupKey {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "project" => Ok(Self::Project),
            "vm" => Ok(Self::Vm),
            "disk" => Ok(Self::Disk),
            "nic" => Ok(Self::Nic),
            "network" => Ok(Self::Network),
            _ => Err(format!("group_by: unknown key {s} (project, vm, disk, nic, network)")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Named {
    pub id: String,
    pub name: String,
}

/// The value of one group key for a subject, if it has one.
pub fn key_of(s: &super::ledger::Subject, k: GroupKey) -> Option<Named> {
    let named = |id: &str, name: &str| Some(Named { id: id.to_string(), name: name.to_string() });
    match (k, s.kind) {
        (GroupKey::Project, _) => s.project.as_ref().map(|p| Named { id: p.clone(), name: p.clone() }),
        (GroupKey::Vm, SubjectKind::Vm) => named(&s.id, &s.name),
        // A NIC's name is `<vm name>/nic<i>`.
        (GroupKey::Vm, SubjectKind::Nic) => s.vm_id.as_deref().and_then(|v| named(v, s.name.rsplit_once('/').map_or(&s.name, |x| x.0))),
        // A disk counts towards the VM it is attached to (§8.6).
        (GroupKey::Vm, SubjectKind::Disk) => s.vm_id.as_deref().and_then(|v| named(v, v)),
        (GroupKey::Disk, SubjectKind::Disk) | (GroupKey::Nic, SubjectKind::Nic) | (GroupKey::Network, SubjectKind::Network) => named(&s.id, &s.name),
        (GroupKey::Network, SubjectKind::Nic) => s.network.as_deref().and_then(|n| named(n, n)),
        _ => None,
    }
}

/// A subject names itself; others (a disk's VM) only fill a gap.
pub fn names_itself(k: GroupKey, kind: SubjectKind) -> bool {
    matches!(
        (k, kind),
        (GroupKey::Project, _)
            | (GroupKey::Vm, SubjectKind::Vm)
            | (GroupKey::Disk, SubjectKind::Disk)
            | (GroupKey::Nic, SubjectKind::Nic)
            | (GroupKey::Network, SubjectKind::Network)
    )
}

pub struct Query {
    /// Hours `[from, to)`, unix s.
    pub from: u64,
    pub to: u64,
    pub granularity: Granularity,
    pub tz: Tz,
    pub group_by: Vec<GroupKey>,
    /// Only these meters (derived ones included); `None`: all.
    pub meters: Option<BTreeSet<String>>,
    /// Unix s; buckets are clipped to it for averages.
    pub now: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub start: u64,
    pub end: u64,
    pub keys: BTreeMap<GroupKey, Named>,
    pub meters: BTreeMap<String, u64>,
    /// `interpolated`, `reset`, `incomplete`, `source_proc`, `revised`
    /// (an adjustment row was added after the hour closed), `provisional`
    /// (includes an hour that is still open).
    pub flags: BTreeSet<String>,
}

fn flag_name(f: &Flag) -> &'static str {
    match f {
        Flag::Interpolated => "interpolated",
        Flag::Reset => "reset",
        Flag::Incomplete { .. } => "incomplete",
        Flag::SourceProc => "source_proc",
        Flag::ExtGap => "ext_gap",
        Flag::NetworkTotalUnavailable => "network_total_unavailable",
    }
}

pub fn aggregate(records: &[UsageRecord], q: &Query) -> Vec<Row> {
    // Grouped by ids; names are display only (the latest seen wins).
    let mut groups: BTreeMap<(u64, Vec<Option<String>>), Row> = BTreeMap::new();
    for r in records.iter().filter(|r| r.hour >= q.from && r.hour < q.to) {
        let (start, end) = bucket(r.hour, q.granularity, &q.tz);
        let keys: Vec<Option<Named>> = q.group_by.iter().map(|k| key_of(&r.subject, *k)).collect();
        let ids = keys.iter().map(|k| k.as_ref().map(|n| n.id.clone())).collect();
        let row = groups.entry((start, ids)).or_insert_with(|| Row {
            start,
            end,
            keys: BTreeMap::new(),
            meters: BTreeMap::new(),
            flags: BTreeSet::new(),
        });
        for (k, v) in q.group_by.iter().zip(keys) {
            if let Some(v) = v {
                if names_itself(*k, r.subject.kind) || !row.keys.contains_key(k) {
                    row.keys.insert(*k, v);
                }
            }
        }
        for (m, v) in &r.meters {
            let e = row.meters.entry(m.clone()).or_insert(0);
            *e = if is_max_meter(m) { (*e).max(*v) } else { e.saturating_add(*v) };
        }
        row.flags.extend(r.flags.iter().map(|f| flag_name(f).to_string()));
        if r.seq > 0 {
            row.flags.insert("revised".into());
        }
        if r.written_at == 0 {
            row.flags.insert("provisional".into());
        }
    }
    let mut rows: Vec<Row> = groups.into_values().collect();
    for row in &mut rows {
        let secs = row.end.min(q.to).min(q.now).saturating_sub(row.start.max(q.from));
        derive(&mut row.meters, secs);
        if let Some(only) = &q.meters {
            row.meters.retain(|m, _| only.contains(m));
        }
    }
    rows
}

/// Derived meters (§2, §8.5): totals, internal traffic, averages.
pub fn derive(m: &mut BTreeMap<String, u64>, secs: u64) {
    let get = |m: &BTreeMap<String, u64>, k: &str| m.get(k).copied();
    let sum2 = |m: &BTreeMap<String, u64>, a: &str, b: &str| match (get(m, a), get(m, b)) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0) + y.unwrap_or(0)),
    };
    let mut add = Vec::new();
    if let Some(total) = sum2(m, "net.rx_bytes", "net.tx_bytes") {
        add.push(("net.bytes", total));
        if let Some(ext) = sum2(m, "net.ext_rx_bytes", "net.ext_tx_bytes") {
            add.push(("net.ext_bytes", ext));
            add.push(("net.internal_bytes", total.saturating_sub(ext)));
        }
    }
    if let Some(total) = get(m, "bridge.bytes") {
        if let Some(ext) = sum2(m, "bridge.ext_rx_bytes", "bridge.ext_tx_bytes") {
            add.push(("bridge.ext_bytes", ext));
            add.push(("bridge.internal_bytes", total.saturating_sub(ext)));
        }
    }
    // kbit/s averages over the bucket; presented in Mbps.
    for (bytes, avg) in [
        ("net.rx_bytes", "net.rx_kbps_avg"),
        ("net.tx_bytes", "net.tx_kbps_avg"),
        ("net.ext_rx_bytes", "net.ext_rx_kbps_avg"),
        ("net.ext_tx_bytes", "net.ext_tx_kbps_avg"),
        ("bridge.bytes", "bridge.kbps_avg"),
    ] {
        if let Some(kbps) = get(m, bytes).and_then(|b| (b * 8 / 1000).checked_div(secs)) {
            add.push((avg, kbps));
        }
    }
    // Disk I/O (§8.6): averages over the bucket, and op-weighted
    // latency in µs (Σtime / Σops; none without ops or a time counter).
    for (ops, avg) in [("disk.read_ops", "disk.read_iops_avg"), ("disk.write_ops", "disk.write_iops_avg")] {
        if let Some(o) = get(m, ops) {
            if let Some(v) = (o * 1000).checked_div(secs) {
                add.push((avg, v));
            }
        }
    }
    if let Some(total) = sum2(m, "disk.read_ops", "disk.write_ops") {
        if let Some(v) = (total * 1000).checked_div(secs) {
            add.push(("disk.iops_avg", v));
        }
    }
    for (by, avg) in [("disk.read_bytes", "disk.read_kBps_avg"), ("disk.write_bytes", "disk.write_kBps_avg")] {
        if let Some(v) = get(m, by).and_then(|b| (b / 1000).checked_div(secs)) {
            add.push((avg, v));
        }
    }
    for (t, ops, lat) in [("disk.read_time_ns", "disk.read_ops", "disk.read_latency_us"), ("disk.write_time_ns", "disk.write_ops", "disk.write_latency_us")] {
        if let (Some(t), Some(o)) = (get(m, t), get(m, ops)) {
            if let Some(us) = (t / 1000).checked_div(o) {
                add.push((lat, us));
            }
        }
    }
    for (k, v) in add {
        m.insert(k.to_string(), v);
    }
}

/// The presented value and unit of a raw meter value (§8.3).
pub fn present(meter: &str, raw: u64) -> (f64, &'static str) {
    let r = raw as f64;
    let (v, unit) = match meter {
        "cpu.used" => (r / 3.6e9, "core-hours"),
        "cpu.alloc" => (r / 3600.0, "vCPU-hours"),
        "mem.used" | "mem.alloc" => (r / 3600.0, "MiB-hours"),
        "mem.peak" => (r, "MiB"),
        "disk.alloc" | "disk.stored" | "image.stored" => (r / 3600.0 / 1024.0, "GiB-hours"),
        "vm.running" | "vm.paused" => (r / 3600.0, "hours"),
        m if m.ends_with("bytes") => (r / (1u64 << 30) as f64, "GiB"),
        m if m.ends_with("iops_peak") || m.ends_with("iops_avg") => (r / 1000.0, "IOPS"),
        m if m.ends_with("kBps_peak") || m.ends_with("kBps_avg") => (r / 1000.0, "MB/s"),
        m if m.ends_with("latency_us") => (r / 1000.0, "ms"),
        m if m.ends_with("_ops") => (r, "ops"),
        m if m.ends_with("_time_ns") => (r / 1e9, "s"),
        m if m.ends_with("_kbps_peak") || m.ends_with("_kbps_avg") => (r / 1000.0, "Mbps"),
        m if m.ends_with("pps_peak") => (r / 1000.0, "pps"),
        m if m.ends_with("packets") => (r, "packets"),
        _ => (r, ""),
    };
    ((v * 1e6).round() / 1e6, unit)
}

#[cfg(test)]
mod tests {
    use super::super::ledger::Subject;
    use super::*;

    fn rec(hour: u64, kind: SubjectKind, id: &str, project: &str, meters: &[(&str, u64)]) -> UsageRecord {
        UsageRecord {
            hour,
            subject: Subject::new(kind, id, id, Some(project.into())),
            seq: 0,
            meters: meters.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            flags: BTreeSet::new(),
            written_at: 1,
        }
    }

    #[test]
    fn civil_round_trip_and_rfc3339() {
        for days in [-1000, 0, 11016, 20727, 20728, 60000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(rfc3339(1_791_158_400), "2026-10-05T00:00:00Z");
        assert_eq!(parse_time("2026-10-05T00:00:00Z").unwrap(), 1_791_158_400);
        assert_eq!(parse_time("2026-10-05T07:00+07:00").unwrap(), 1_791_158_400);
        assert_eq!(parse_time("1791158400").unwrap(), 1_791_158_400);
        assert!(parse_time("yesterday").is_err());
    }

    #[test]
    fn month_buckets_in_a_whole_hour_zone() {
        let bkk = parse_tz("+07:00").unwrap();
        // 2026-10-31T17:00Z is 2026-11-01T00:00 in Bangkok.
        let t = parse_time("2026-10-31T17:00:00Z").unwrap();
        let (s, e) = bucket(t, Granularity::Month, &bkk);
        assert_eq!((rfc3339(s), rfc3339(e)), ("2026-10-31T17:00:00Z".into(), "2026-11-30T17:00:00Z".into()));
        let (s, _) = bucket(t - 3600, Granularity::Month, &bkk);
        assert_eq!(rfc3339(s), "2026-09-30T17:00:00Z");
        // October in Bangkok has exactly 31 × 24 hours.
        let (s, e) = bucket(t - 3600, Granularity::Month, &bkk);
        assert_eq!((e - s) / 3600, 744);
        assert_eq!(bucket(t, Granularity::Day, &Tz::utc()), (t - 17 * 3600, t + 7 * 3600));
    }

    #[test]
    fn month_bounds_and_labels() {
        let bkk = parse_tz("+07:00").unwrap();
        let (s, e) = month_bounds("2026-10", &bkk).unwrap();
        assert_eq!((rfc3339(s), rfc3339(e)), ("2026-09-30T17:00:00Z".into(), "2026-10-31T17:00:00Z".into()));
        assert_eq!(month_label(s, &bkk), "2026-10");
        assert_eq!(month_label(s, &Tz::utc()), "2026-09");
        assert!(month_bounds("2026-13", &bkk).is_err() && month_bounds("Oct", &bkk).is_err());
    }

    #[test]
    fn time_zones_must_be_fixed_whole_hours() {
        assert_eq!(parse_tz("UTC").unwrap().offset_secs, 0);
        assert_eq!(parse_tz("-05:00").unwrap().offset_secs, -5 * 3600);
        assert!(parse_tz("+05:30").is_err());
        assert!(parse_tz("../etc/passwd").is_err());
        assert_eq!(tzif_fixed_offset(b"TZif2....\n<+07>-7\n").unwrap(), 7 * 3600);
        assert_eq!(tzif_fixed_offset(b"TZif2....\nEST5\n").unwrap(), -5 * 3600);
        assert!(tzif_fixed_offset(b"TZif2....\nIST-5:30\n").unwrap_err().contains("whole number"));
        assert!(tzif_fixed_offset(b"TZif2....\nCET-1CEST,M3.5.0,M10.5.0/3\n").unwrap_err().contains("daylight"));
        // The host's zoneinfo, when present.
        if Path::new(ZONEINFO).join("Asia/Bangkok").exists() {
            assert_eq!(parse_tz("Asia/Bangkok").unwrap().offset_secs, 7 * 3600);
            assert!(parse_tz("Europe/Berlin").is_err());
            assert!(parse_tz("Asia/Kolkata").is_err());
        }
    }

    #[test]
    fn group_derive_and_present() {
        let h0 = 1_791_158_400; // 2026-10-05T00:00Z
        let mut nic = rec(h0, SubjectKind::Nic, "vm1.0", "p1", &[("net.rx_bytes", 3 << 30), ("net.tx_bytes", 1 << 30), ("net.ext_rx_bytes", 2 << 30)]);
        nic.subject.vm_id = Some("vm1".into());
        nic.subject.name = "web-1/nic0".into();
        let mut vm = rec(h0 + 3600, SubjectKind::Vm, "vm1", "p1", &[("cpu.used", 7_200_000_000), ("mem.peak", 512)]);
        vm.subject.name = "web-1".into();
        let mut vm2 = rec(h0 + 7200, SubjectKind::Vm, "vm1", "p1", &[("cpu.used", 3_600_000_000), ("mem.peak", 768)]);
        vm2.seq = 1;
        vm2.subject.name = "web-1".into();
        let other = rec(h0, SubjectKind::Vm, "vm2", "p2", &[("cpu.used", 1)]);
        let q = Query {
            from: h0,
            to: h0 + 86400,
            granularity: Granularity::Day,
            tz: Tz::utc(),
            group_by: vec![GroupKey::Project, GroupKey::Vm],
            meters: None,
            now: h0 + 86400,
        };
        let rows = aggregate(&[nic, vm, vm2, other], &q);
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert_eq!(r.keys[&GroupKey::Vm], Named { id: "vm1".into(), name: "web-1".into() });
        assert_eq!(r.meters["cpu.used"], 10_800_000_000);
        assert_eq!(r.meters["mem.peak"], 768, "peaks take the max");
        assert_eq!(r.meters["net.bytes"], 4 << 30);
        assert_eq!(r.meters["net.internal_bytes"], 2 << 30);
        assert_eq!(r.meters["net.rx_kbps_avg"], (3u64 << 30) * 8 / 1000 / 86400);
        assert!(r.flags.contains("revised"));
        assert_eq!(present("cpu.used", r.meters["cpu.used"]), (3.0, "core-hours"));
        assert_eq!(present("net.bytes", 4 << 30), (4.0, "GiB"));
        assert_eq!(present("net.rx_kbps_peak", 1500), (1.5, "Mbps"));
        assert_eq!(present("disk.iops_peak", 500_000), (500.0, "IOPS"));
        assert_eq!(present("disk.read_latency_us", 1_250), (1.25, "ms"));
        let mut d: BTreeMap<String, u64> =
            [("disk.read_ops", 1000), ("disk.write_ops", 500), ("disk.read_time_ns", 3_000_000_000), ("disk.read_bytes", 4_096_000)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect();
        derive(&mut d, 100);
        assert_eq!((d["disk.iops_avg"], d["disk.read_iops_avg"]), (15_000, 10_000), "15 and 10 IOPS");
        assert_eq!(d["disk.read_latency_us"], 3_000, "3 s / 1000 ops = 3 ms");
        assert!(!d.contains_key("disk.write_latency_us"), "no time counter, no latency");
        // Only some meters.
        let only = Query { meters: Some(["net.bytes".to_string()].into()), ..q };
        let rows = aggregate(&[rec(h0, SubjectKind::Nic, "x.0", "p1", &[("net.rx_bytes", 5)])], &only);
        assert_eq!(rows[0].meters.keys().collect::<Vec<_>>(), vec!["net.bytes"]);
    }
}
