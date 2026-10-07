//! Bandwidth, disk I/O, CPU and memory, and live rates (spec/metering.md
//! §9.3–§9.4).

use super::usage::{bad, list, meter, scope};
use super::{err, vm_entities, ApiErr, Caller};
use crate::metering::ledger::{SlotRow, Subject, SubjectKind};
use crate::metering::query::{self, GroupKey, Granularity, Named};
use crate::metering::rates::{bandwidth_p95, compute_p95, disk_io_p95, group_slots, series_points, GroupSlots, SeriesKind};
use axum::{
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

const DAY: u64 = 86400;

#[derive(Debug, Default, serde::Deserialize)]
pub struct RateParams {
    #[serde(default)]
    project: Option<String>,
    /// `YYYY-MM` in the billing time zone; default: the current month.
    #[serde(default)]
    month: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    group_by: Option<String>,
    #[serde(default)]
    format: Option<String>,
    /// Series endpoints: add the p95 over the range.
    #[serde(default)]
    p95: Option<bool>,
    /// Months in this zone instead of `metering.billing_timezone`
    /// (fixed whole-hour offsets only, §8.1).
    #[serde(default)]
    tz: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Bandwidth,
    DiskIo,
    Compute,
}

struct Range {
    from: u64,
    to: u64,
    month: Option<String>,
    tz: query::Tz,
    now: u64,
}

fn range(p: &RateParams, billing_tz: &str, max_days: u64, default_days: Option<u64>) -> Result<Range, ApiErr> {
    let tz = query::parse_tz(p.tz.as_deref().unwrap_or(billing_tz)).map_err(bad)?;
    let now = crate::metering::now_ms() / 1000;
    let (from, to, month) = match (&p.month, &p.from, default_days) {
        (Some(m), _, _) => {
            let (s, e) = query::month_bounds(m, &tz).map_err(bad)?;
            (s, e, Some(m.clone()))
        }
        (None, Some(f), _) => {
            let to = p.to.as_deref().map(query::parse_time).transpose().map_err(bad)?.unwrap_or(now);
            (query::parse_time(f).map_err(bad)? / 3600 * 3600, to.div_ceil(3600) * 3600, None)
        }
        (None, None, Some(days)) => (now.saturating_sub(days * DAY) / 300 * 300, now.div_ceil(300) * 300, None),
        (None, None, None) => {
            let (s, e) = query::bucket(now, Granularity::Month, &tz);
            (s, e, Some(query::month_label(now, &tz)))
        }
    };
    if to <= from {
        return Err(bad("`to` must be after `from`"));
    }
    if to - from > max_days * DAY {
        return Err(err(StatusCode::BAD_REQUEST, "range_too_large", format!("at most {max_days} days per request")));
    }
    Ok(Range { from, to, month, tz, now })
}

fn names(c: &Caller) -> BTreeMap<String, String> {
    c.manager().projects().list().unwrap_or_default().into_iter().map(|p| (p.id, p.name)).collect()
}

fn keys_json(keys: &BTreeMap<GroupKey, Named>, names: &BTreeMap<String, String>, out: &mut serde_json::Map<String, Value>) {
    for (k, v) in keys {
        let mut n = v.clone();
        if *k == GroupKey::Project {
            if let Some(name) = names.get(&n.id) {
                n.name = name.clone();
            }
        }
        out.insert(serde_json::to_value(k).ok().and_then(|x| x.as_str().map(String::from)).unwrap_or_default(), json!(n));
    }
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Averages over the seconds a group was present, and 30-second peaks,
/// from the hourly rows (§8.5, §8.6).
fn avg_and_peak(family: Family, kind: SubjectKind, meters: &BTreeMap<String, u64>, secs: u64, sample_secs: u64) -> (Value, Value) {
    let g = |m: &str| meters.get(m).copied();
    let per_s = |v: Option<u64>, div: f64| v.and_then(|v| (secs > 0).then(|| r3(v as f64 / secs as f64 / div)));
    let peak = |m: &str, div: f64| g(m).map(|v| r3(v as f64 / div));
    match (family, kind) {
        (Family::Bandwidth, SubjectKind::Network) => (
            json!({ "mbps": per_s(g("bridge.bytes").map(|b| b * 8), 1e6),
                    "ext_rx_mbps": per_s(g("bridge.ext_rx_bytes").map(|b| b * 8), 1e6),
                    "ext_tx_mbps": per_s(g("bridge.ext_tx_bytes").map(|b| b * 8), 1e6) }),
            json!({ "mbps": peak("bridge.kbps_peak", 1000.0),
                    "ext_rx_mbps": peak("bridge.ext_rx_kbps_peak", 1000.0),
                    "ext_tx_mbps": peak("bridge.ext_tx_kbps_peak", 1000.0),
                    "resolution_secs": sample_secs }),
        ),
        (Family::Bandwidth, _) => (
            json!({ "rx_mbps": per_s(g("net.rx_bytes").map(|b| b * 8), 1e6),
                    "tx_mbps": per_s(g("net.tx_bytes").map(|b| b * 8), 1e6),
                    "ext_rx_mbps": per_s(g("net.ext_rx_bytes").map(|b| b * 8), 1e6),
                    "ext_tx_mbps": per_s(g("net.ext_tx_bytes").map(|b| b * 8), 1e6) }),
            json!({ "rx_mbps": peak("net.rx_kbps_peak", 1000.0),
                    "tx_mbps": peak("net.tx_kbps_peak", 1000.0),
                    "ext_rx_mbps": peak("net.ext_rx_kbps_peak", 1000.0),
                    "ext_tx_mbps": peak("net.ext_tx_kbps_peak", 1000.0),
                    "resolution_secs": sample_secs }),
        ),
        (Family::Compute, _) => {
            let ratio = |u: Option<u64>, a: Option<u64>, scale: f64| match (u, a) {
                (Some(u), Some(a)) if a > 0 => Some(r3(u as f64 * 100.0 / (a as f64 * scale))),
                _ => None,
            };
            (
                json!({ "cpu_cores": per_s(g("cpu.used"), 1e6), "vcpus": per_s(g("cpu.alloc"), 1.0),
                        "cpu_percent": ratio(g("cpu.used"), g("cpu.alloc"), 1e6),
                        "mem_mib": per_s(g("mem.used"), 1.0), "mem_alloc_mib": per_s(g("mem.alloc"), 1.0),
                        "mem_percent": ratio(g("mem.used"), g("mem.alloc"), 1.0) }),
                json!({ "cpu_cores": peak("cpu.cores_peak", 1000.0), "mem_mib": peak("mem.peak", 1.0),
                        "resolution_secs": sample_secs }),
            )
        }
        (Family::DiskIo, _) => {
            let lat = |t: &str, o: &str| match (g(t), g(o)) {
                (Some(t), Some(o)) if o > 0 => Some(r3(t as f64 / o as f64 / 1e6)),
                _ => None,
            };
            (
                json!({ "read_iops": per_s(g("disk.read_ops"), 1.0), "write_iops": per_s(g("disk.write_ops"), 1.0),
                        "read_mbps": per_s(g("disk.read_bytes"), 1e6), "write_mbps": per_s(g("disk.write_bytes"), 1e6),
                        "read_latency_ms": lat("disk.read_time_ns", "disk.read_ops"),
                        "write_latency_ms": lat("disk.write_time_ns", "disk.write_ops") }),
                json!({ "read_iops": peak("disk.read_iops_peak", 1000.0), "write_iops": peak("disk.write_iops_peak", 1000.0),
                        "iops": peak("disk.iops_peak", 1000.0),
                        "read_mbps": peak("disk.read_kBps_peak", 1000.0), "write_mbps": peak("disk.write_kBps_peak", 1000.0),
                        "mbps": peak("disk.kBps_peak", 1000.0), "resolution_secs": sample_secs }),
            )
        }
    }
}

/// `GET /usage/bandwidth`, `/usage/disk-io` and `/usage/compute` (§9.3).
async fn report(c: Caller, p: RateParams, family: Family) -> Result<Response, ApiErr> {
    let projects = scope(&c, &p.project)?;
    let meter = meter(&c)?;
    let cfg = meter.config().clone();
    let r = range(&p, &cfg.billing_timezone, 100, None)?;
    let allowed: &[GroupKey] = match family {
        Family::Bandwidth => &[GroupKey::Project, GroupKey::Vm, GroupKey::Nic, GroupKey::Network],
        Family::DiskIo => &[GroupKey::Project, GroupKey::Vm, GroupKey::Disk],
        Family::Compute => &[GroupKey::Project, GroupKey::Vm],
    };
    let mut group_by: Vec<GroupKey> = list(&p.group_by).iter().map(|g| g.parse()).collect::<Result<_, _>>().map_err(bad)?;
    if group_by.is_empty() {
        group_by = vec![GroupKey::Project];
    }
    if let Some(k) = group_by.iter().find(|k| !allowed.contains(k)) {
        return Err(bad(format!("group_by {k:?} is not available here")));
    }
    let kind = match family {
        Family::DiskIo => SubjectKind::Disk,
        Family::Compute => SubjectKind::Vm,
        Family::Bandwidth if group_by.contains(&GroupKey::Network) => SubjectKind::Network,
        Family::Bandwidth => SubjectKind::Nic,
    };
    let in_scope = |s: &Subject| s.kind == kind && projects.as_ref().is_none_or(|ps| s.project.as_ref().is_some_and(|x| ps.contains(x)));
    let storage = |e: crate::metering::MeteringError| err(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", e.to_string());
    let slots: Vec<(u64, SlotRow)> = meter.ledger().scan_slots(r.from, r.to, projects.as_ref(), in_scope).map_err(storage)?;
    let groups = group_slots(&slots, r.from, r.to, &group_by);
    // Averages and peaks from the hourly rows, by the same groups.
    let records: Vec<_> = meter.ledger().scan(r.from, r.to, projects.as_ref()).map_err(storage)?.into_iter().filter(|x| in_scope(&x.subject)).collect();
    let q = query::Query { from: r.from, to: r.to, granularity: Granularity::Hour, tz: query::Tz::utc(), group_by: group_by.clone(), meters: None, now: r.now };
    let mut totals: BTreeMap<Vec<Option<String>>, BTreeMap<String, u64>> = BTreeMap::new();
    for row in query::aggregate(&records, &q) {
        let ids = group_by.iter().map(|k| row.keys.get(k).map(|n| n.id.clone())).collect();
        let t = totals.entry(ids).or_default();
        for (m, v) in row.meters {
            let e = t.entry(m.clone()).or_insert(0);
            *e = if crate::metering::ledger::is_max_meter(&m) { (*e).max(v) } else { e.saturating_add(v) };
        }
    }
    let names = names(&c);
    let mut rows: Vec<Value> = Vec::new();
    for (ids, g) in &groups {
        let mut o = serde_json::Map::new();
        keys_json(&g.keys, &names, &mut o);
        let secs = g.slots.len() as u64 * 300;
        let (avg, peak) = avg_and_peak(family, kind, totals.get(ids).unwrap_or(&BTreeMap::new()), secs, cfg.sample_secs);
        o.insert("avg".into(), avg);
        o.insert("peak".into(), peak);
        match family {
            Family::Bandwidth => {
                let mut b = serde_json::to_value(bandwidth_p95(g, kind)).unwrap_or_default();
                let slots = b.as_object_mut().and_then(|m| m.remove("slots")).unwrap_or_default();
                o.insert("p95".into(), b);
                o.insert("slots".into(), slots);
            }
            Family::DiskIo => {
                let mut d = serde_json::to_value(disk_io_p95(g)).unwrap_or_default();
                let slots = d.as_object_mut().and_then(|m| m.remove("slots")).unwrap_or_default();
                let source = if totals.get(ids).is_some_and(|t| t.contains_key("disk.read_time_ns")) { "counter" } else { "none" };
                o.insert("p95".into(), d);
                o.insert("slots".into(), slots);
                o.insert("latency_source".into(), source.into());
            }
            Family::Compute => {
                let mut c = serde_json::to_value(compute_p95(g)).unwrap_or_default();
                let slots = c.as_object_mut().and_then(|m| m.remove("slots")).unwrap_or_default();
                o.insert("p95".into(), c);
                o.insert("slots".into(), slots);
            }
        }
        rows.push(o.into());
    }
    // Slots gone (older than retention_rate_days): the month's final figures.
    let mut from_final = false;
    if groups.is_empty() && r.month.is_some() {
        let grouping = match (family, group_by.last()) {
            (Family::Bandwidth, Some(GroupKey::Nic)) => Some("bw/nic"),
            (Family::Bandwidth, Some(GroupKey::Vm)) => Some("bw/vm"),
            (Family::Bandwidth, Some(GroupKey::Network)) => Some("bw/network"),
            (Family::Bandwidth, Some(GroupKey::Project)) => Some("bw/project"),
            (Family::DiskIo, Some(GroupKey::Disk)) => Some("io/disk"),
            (Family::DiskIo, Some(GroupKey::Vm)) => Some("io/vm"),
            (Family::DiskIo, Some(GroupKey::Project)) => Some("io/project"),
            (Family::Compute, Some(GroupKey::Vm)) => Some("cm/vm"),
            (Family::Compute, Some(GroupKey::Project)) => Some("cm/project"),
            _ => None,
        };
        if let Some(gname) = grouping {
            for (_, v) in meter.ledger().month_rates(r.from, &format!("{gname}/")).map_err(storage)? {
                let project = v["keys"]["project"]["id"].as_str().map(String::from);
                if projects.as_ref().is_none_or(|ps| project.as_ref().is_some_and(|x| ps.contains(x))) {
                    let mut o = serde_json::Map::new();
                    if let Some(keys) = v["keys"].as_object() {
                        o.extend(keys.clone());
                    }
                    o.insert("p95".into(), v["p95"].clone());
                    rows.push(o.into());
                    from_final = true;
                }
            }
        }
    }
    let complete = meter.ledger().complete_through().map_err(storage)?;
    let body = json!({
        "month": r.month,
        "from": query::rfc3339(r.from),
        "to": query::rfc3339(r.to),
        "timezone": r.tz.name,
        "final": complete >= r.to,
        "from_final_figures": from_final,
        "group_by": group_by,
        "rows": rows,
    });
    if p.format.as_deref() == Some("csv") {
        c.detail("export", json!("csv"));
        c.audit_always();
        return Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], flat_csv(&body["rows"])).into_response());
    }
    Ok(Json(body).into_response())
}

/// Rows as CSV: one column per leaf (`p95.rx_mbps`, `vm.name`, …).
fn flat_csv(rows: &Value) -> String {
    fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    flatten(&if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") }, x, out);
                }
            }
            Value::Null => {}
            Value::String(s) => {
                out.insert(prefix.to_string(), s.clone());
            }
            other => {
                out.insert(prefix.to_string(), other.to_string());
            }
        }
    }
    let flat: Vec<BTreeMap<String, String>> = rows
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let mut m = BTreeMap::new();
            flatten("", r, &mut m);
            m
        })
        .collect();
    let cols: BTreeSet<&String> = flat.iter().flat_map(|m| m.keys()).collect();
    let field = |s: &str| if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let mut out = cols.iter().map(|c| field(c)).collect::<Vec<_>>().join(",");
    out.push('\n');
    for m in &flat {
        out.push_str(&cols.iter().map(|c| m.get(*c).map(|v| field(v)).unwrap_or_default()).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

pub async fn bandwidth(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    report(c, p, Family::Bandwidth).await
}

pub async fn disk_io(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    report(c, p, Family::DiskIo).await
}

pub async fn compute(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    report(c, p, Family::Compute).await
}

/// A 5-minute series for graphs (§9.3): `points`, and `p95` with `?p95=true`.
fn series(
    c: &Caller,
    p: &RateParams,
    kind: SubjectKind,
    project: Option<&str>,
    group_by: &[GroupKey],
    keep: impl Fn(&Subject) -> bool,
) -> Result<Value, ApiErr> {
    let meter = meter(c)?;
    let r = range(p, &meter.config().billing_timezone, 31, Some(1))?;
    let rows = meter
        .ledger()
        .scan_slots(r.from, r.to, project.map(|p| BTreeSet::from([p.to_string()])).as_ref(), |s| s.kind == kind && keep(s))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", e.to_string()))?;
    let groups = group_slots(&rows, r.from, r.to, group_by);
    let one = |g: &GroupSlots| {
        let skind = match kind {
            SubjectKind::Disk => SeriesKind::Disk,
            SubjectKind::Vm => SeriesKind::Compute,
            _ => SeriesKind::Network,
        };
        let mut o = json!({ "points": series_points(g, skind) });
        if p.p95 == Some(true) {
            o["p95"] = match skind {
                SeriesKind::Disk => serde_json::to_value(disk_io_p95(g)),
                SeriesKind::Compute => serde_json::to_value(compute_p95(g)),
                SeriesKind::Network => serde_json::to_value(bandwidth_p95(g, kind)),
            }
            .unwrap_or_default();
        }
        o
    };
    let mut out = match groups.values().next() {
        Some(g) => one(g),
        None => json!({ "points": [] }),
    };
    out["from"] = query::rfc3339(r.from).into();
    out["to"] = query::rfc3339(r.to).into();
    Ok(out)
}

async fn visible_vm(c: &Caller, id: &str) -> Result<crate::models::Vm, ApiErr> {
    let vm = c.manager().get_vm(id).await.map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "VM not found"))?;
    c.set_project(&vm.project);
    c.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    c.require_visible("readVm", e, es, "VM")?;
    Ok(vm)
}

async fn visible_disk(c: &Caller, id: &str) -> Result<crate::images::DiskResponse, ApiErr> {
    let d = c.manager().get_disk(id).await.map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "disk not found"))?;
    c.set_project(&d.project);
    c.set_target(format!("disk:{}", d.id));
    let (e, es) = super::disk_entities(&d.id, &d.project);
    c.require_visible("readDisk", e, es, "disk")?;
    Ok(d)
}

fn visible_network(c: &Caller, name: &str) -> Result<crate::network::Network, ApiErr> {
    let n = c.manager().get_network(name).map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "network not found"))?;
    let (e, es) = super::net::network_entities(&n);
    if !c.allowed("readNetwork", e.clone(), es.clone()) {
        return Err(err(StatusCode::NOT_FOUND, "not_found", "network not found"));
    }
    c.set_target(format!("network:{}", n.name));
    c.require(e, es)?;
    Ok(n)
}

pub async fn vm_bandwidth(c: Caller, Path(id): Path<String>, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let vm = visible_vm(&c, &id).await?;
    let v = series(&c, &p, SubjectKind::Nic, Some(&vm.project), &[GroupKey::Vm], |s| s.vm_id.as_deref() == Some(vm.id.as_str()))?;
    Ok(Json(v).into_response())
}

pub async fn network_bandwidth(c: Caller, Path(name): Path<String>, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let n = visible_network(&c, &name)?;
    let v = series(&c, &p, SubjectKind::Network, n.project.as_deref(), &[GroupKey::Network], |s| s.id == n.name)?;
    Ok(Json(v).into_response())
}

pub async fn vm_io(c: Caller, Path(id): Path<String>, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let vm = visible_vm(&c, &id).await?;
    let on_vm = |s: &Subject| s.vm_id.as_deref() == Some(vm.id.as_str());
    let mut v = series(&c, &p, SubjectKind::Disk, Some(&vm.project), &[GroupKey::Vm], on_vm)?;
    let mut disks = serde_json::Map::new();
    let ids: BTreeSet<String> = c.manager().list_disks().into_iter().filter(|d| d.attached_to.as_deref() == Some(vm.id.as_str())).map(|d| d.id).collect();
    for d in ids {
        disks.insert(d.clone(), series(&c, &p, SubjectKind::Disk, Some(&vm.project), &[GroupKey::Disk], |s| s.id == d)?);
    }
    v["disks"] = disks.into();
    Ok(Json(v).into_response())
}

pub async fn vm_compute(c: Caller, Path(id): Path<String>, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let vm = visible_vm(&c, &id).await?;
    let v = series(&c, &p, SubjectKind::Vm, Some(&vm.project), &[GroupKey::Vm], |s| s.id == vm.id)?;
    Ok(Json(v).into_response())
}

pub async fn disk_io_series(c: Caller, Path(id): Path<String>, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let d = visible_disk(&c, &id).await?;
    let v = series(&c, &p, SubjectKind::Disk, Some(&d.project), &[GroupKey::Disk], |s| s.id == d.id)?;
    Ok(Json(v).into_response())
}

/// `GET /vms/{id}/stats` (§9.4): the latest rates of the VM, its NICs
/// and its disks, from the last two samples (not stored).
pub async fn vm_stats(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let vm = visible_vm(&c, &id).await?;
    let meter = meter(&c)?;
    let live = meter.live_stats(|s| (s.kind == SubjectKind::Vm && s.id == vm.id) || s.vm_id.as_deref() == Some(vm.id.as_str()));
    let pick = |kind: SubjectKind| -> Vec<Value> {
        live.iter()
            .filter(|l| l.subject.kind == kind)
            .map(|l| json!({ "id": l.subject.id, "name": l.subject.name, "network": l.subject.network, "values": l.values }))
            .collect()
    };
    let vm_values = live.iter().find(|l| l.subject.kind == SubjectKind::Vm).map(|l| json!(l.values));
    let at = live.iter().map(|l| l.sampled_at).max();
    Ok(Json(json!({
        "sampled_at": at.map(|t| query::rfc3339(t / 1000)),
        "resolution_secs": meter.config().sample_secs,
        "vm": vm_values,
        "nics": pick(SubjectKind::Nic),
        "disks": pick(SubjectKind::Disk),
    }))
    .into_response())
}

/// `GET /networks/{name}/stats` (§9.4).
pub async fn network_stats(c: Caller, Path(name): Path<String>) -> Result<Response, ApiErr> {
    let n = visible_network(&c, &name)?;
    let meter = meter(&c)?;
    let live = meter.live_stats(|s| s.kind == SubjectKind::Network && s.id == n.name);
    Ok(Json(json!({
        "sampled_at": live.first().map(|l| query::rfc3339(l.sampled_at / 1000)),
        "resolution_secs": meter.config().sample_secs,
        "values": live.first().map(|l| json!(l.values)),
    }))
    .into_response())
}
