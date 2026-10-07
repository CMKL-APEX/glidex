//! Usage records (spec/metering.md §9.1–§9.2, §10).

use super::{err, project_entities, vm_entities, ApiErr, Caller};
use crate::authz::{Ent, EntitySet};
use crate::metering::query::{self, GroupKey, Granularity, Query as UsageQuery, Row};
use crate::metering::{SubjectKind, UsageRecord};
use axum::{
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use std::collections::{BTreeMap, BTreeSet};

/// Longest range one request may cover (§9.1).
const MAX_SPAN_SECS: u64 = 400 * 86400;

#[derive(Debug, Default, serde::Deserialize)]
pub struct UsageParams {
    /// Project ids or names, comma-separated.
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    /// `YYYY-MM` in the billing time zone (instead of `from`/`to`).
    #[serde(default)]
    month: Option<String>,
    #[serde(default)]
    granularity: Option<String>,
    /// Comma-separated: project, vm, disk, nic, network.
    #[serde(default)]
    group_by: Option<String>,
    /// Comma-separated meter names (derived ones included).
    #[serde(default)]
    meters: Option<String>,
    /// `json` (default) or `csv`.
    #[serde(default)]
    format: Option<String>,
    /// Time zone for day and month buckets; default `metering.billing_timezone`.
    #[serde(default)]
    tz: Option<String>,
    /// `disks`: a VM's usage includes its attached disks (§9.2).
    #[serde(default)]
    include: Option<String>,
}

pub(super) fn bad(msg: impl Into<String>) -> ApiErr {
    err(StatusCode::BAD_REQUEST, "invalid", msg)
}

pub(super) fn list(s: &Option<String>) -> Vec<String> {
    s.as_deref().unwrap_or("").split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

pub(super) fn meter(c: &Caller) -> Result<std::sync::Arc<crate::metering::Meter>, ApiErr> {
    c.manager()
        .meter()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "metering_unavailable", "metering is not running"))
}

/// `GET /usage`: host readers see every project (also deleted ones);
/// everyone else the projects they may read usage of (§10).
pub async fn usage(c: Caller, Query(p): Query<UsageParams>) -> Result<Response, ApiErr> {
    let projects = scope(&c, &p.project)?;
    respond(&c, &p, projects, &[GroupKey::Project], |_| true)
}

/// The projects a usage request covers (§10): `None` = all (host
/// readers only). Requested projects the caller can't read are `404`.
pub(super) fn scope(c: &Caller, requested: &Option<String>) -> Result<Option<BTreeSet<String>>, ApiErr> {
    let wanted = list(requested);
    let all = c.allowed("readUsage", Ent::Host, EntitySet::new());
    let mut projects: Option<BTreeSet<String>> = None;
    if !wanted.is_empty() {
        let mut set = BTreeSet::new();
        for w in &wanted {
            let id = c.target_project(Some(w))?;
            if !all {
                // Not readable: not found, never silently left out.
                c.require_readable("readProjectUsage", Ent::Project(id.clone()), project_entities(&id), "project")?;
            }
            set.insert(id);
        }
        if let [only] = Vec::from_iter(&set)[..] {
            c.set_project(only);
        }
        projects = Some(set);
    } else if !all {
        let mine: BTreeSet<String> = match c.visible_projects()? {
            crate::auth::LinkedProjects::All => c.manager().projects().list().map(|v| v.into_iter().map(|p| p.id).collect()).unwrap_or_default(),
            crate::auth::LinkedProjects::Some(v) => v.into_iter().collect(),
        };
        let readable: BTreeSet<String> = mine
            .into_iter()
            .filter(|id| c.allowed("readProjectUsage", Ent::Project(id.clone()), project_entities(id)))
            .collect();
        if readable.is_empty() {
            c.require_action("readUsage", Ent::Host, EntitySet::new(), &[])?;
        }
        projects = Some(readable);
    }
    Ok(projects)
}

/// `GET /projects/{id}/usage`.
pub async fn project_usage(c: Caller, Path(id): Path<String>, Query(p): Query<UsageParams>) -> Result<Response, ApiErr> {
    let pid = c.target_project(Some(&id))?;
    c.set_project(&pid);
    c.require_readable("readProject", Ent::Project(pid.clone()), project_entities(&pid), "project")?;
    c.require(Ent::Project(pid.clone()), project_entities(&pid))?;
    respond(&c, &p, Some([pid].into()), &[GroupKey::Project], |_| true)
}

/// `GET /vms/{id}/usage`: the VM and its NICs (and, with
/// `include=disks`, the disks attached to it now).
pub async fn vm_usage(c: Caller, Path(id): Path<String>, Query(p): Query<UsageParams>) -> Result<Response, ApiErr> {
    let vm = c.manager().get_vm(&id).await.map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "VM not found"))?;
    c.set_project(&vm.project);
    c.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    c.require_visible("readVm", e, es, "VM")?;
    let disks: BTreeSet<String> = if list(&p.include).iter().any(|i| i == "disks") {
        c.manager().attached_disk_ids(&vm.id)
    } else {
        BTreeSet::new()
    };
    let vm_id = vm.id.clone();
    respond(&c, &p, Some([vm.project.clone()].into()), &[GroupKey::Vm], move |r| match r.subject.kind {
        SubjectKind::Vm => r.subject.id == vm_id,
        SubjectKind::Nic => r.subject.vm_id.as_deref() == Some(vm_id.as_str()),
        SubjectKind::Disk => disks.contains(&r.subject.id),
        _ => false,
    })
}

/// `GET /disks/{id}/usage`.
pub async fn disk_usage(c: Caller, Path(id): Path<String>, Query(p): Query<UsageParams>) -> Result<Response, ApiErr> {
    let d = c.manager().get_disk(&id).await.map_err(|_| err(StatusCode::NOT_FOUND, "not_found", "disk not found"))?;
    c.set_project(&d.project);
    c.set_target(format!("disk:{}", d.id));
    let (e, es) = super::disk_entities(&d.id, &d.project);
    c.require_visible("readDisk", e, es, "disk")?;
    let disk_id = d.id.clone();
    respond(&c, &p, Some([d.project.clone()].into()), &[GroupKey::Disk], move |r| {
        r.subject.kind == SubjectKind::Disk && r.subject.id == disk_id
    })
}

fn respond(
    c: &Caller,
    p: &UsageParams,
    projects: Option<BTreeSet<String>>,
    default_group: &[GroupKey],
    keep: impl Fn(&UsageRecord) -> bool,
) -> Result<Response, ApiErr> {
    let meter = meter(c)?;
    let tz = query::parse_tz(p.tz.as_deref().unwrap_or(&meter.config().billing_timezone)).map_err(bad)?;
    let now = crate::metering::now_ms() / 1000;
    let month = p.month.as_deref().map(|m| query::month_bounds(m, &tz)).transpose().map_err(bad)?;
    let to = match &p.to {
        Some(t) => query::parse_time(t).map_err(bad)?,
        None => now,
    }
    .div_ceil(3600)
        * 3600;
    let from = match &p.from {
        // Default: the start of the current billing month.
        None => query::bucket(now, Granularity::Month, &tz).0,
        Some(f) => query::parse_time(f).map_err(bad)? / 3600 * 3600,
    };
    let (from, to) = month.unwrap_or((from, to));
    if to <= from {
        return Err(bad("`to` must be after `from`"));
    }
    if to - from > MAX_SPAN_SECS {
        return Err(err(StatusCode::BAD_REQUEST, "range_too_large", "at most 400 days per request"));
    }
    let granularity: Granularity = p.granularity.as_deref().unwrap_or("day").parse().map_err(bad)?;
    let mut group_by: Vec<GroupKey> = list(&p.group_by).iter().map(|g| g.parse()).collect::<Result<_, _>>().map_err(bad)?;
    if group_by.is_empty() {
        group_by = default_group.to_vec();
    }
    let meters = p.meters.as_ref().map(|_| list(&p.meters).into_iter().collect());
    let ledger = meter.ledger();
    let storage = |e: crate::metering::MeteringError| err(StatusCode::INTERNAL_SERVER_ERROR, "persistence_error", e.to_string());
    let records: Vec<UsageRecord> = ledger.scan(from, to, projects.as_ref()).map_err(storage)?.into_iter().filter(|r| keep(r)).collect();
    let q = UsageQuery { from, to, granularity, tz: tz.clone(), group_by: group_by.clone(), meters, now };
    let mut rows = query::aggregate(&records, &q);
    let started_at = ledger.started_at().map_err(storage)? / 1000;
    let names: BTreeMap<String, String> =
        c.manager().projects().list().unwrap_or_default().into_iter().map(|p| (p.id, p.name)).collect();
    for row in &mut rows {
        if row.start < started_at {
            row.flags.insert("partial".into());
        }
        if let Some(pr) = row.keys.get_mut(&GroupKey::Project) {
            if let Some(n) = names.get(&pr.id) {
                pr.name = n.clone();
            }
        }
    }
    let complete_through = ledger.complete_through().map_err(storage)?;
    if p.format.as_deref() == Some("csv") {
        // A bulk export is audited even though it is a read (§10).
        c.detail("export", serde_json::json!("csv"));
        c.audit_always();
        return Ok(([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], csv(&rows, &group_by)).into_response());
    }
    let rows: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
    Ok(Json(serde_json::json!({
        "from": query::rfc3339(from),
        "to": query::rfc3339(to),
        "granularity": p.granularity.as_deref().unwrap_or("day"),
        "timezone": tz.name,
        "group_by": group_by,
        "rows": rows,
        "complete_through": query::rfc3339(complete_through),
        "metering_started_at": query::rfc3339(started_at),
    }))
    .into_response())
}

fn row_json(r: &Row) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("start".into(), query::rfc3339(r.start).into());
    o.insert("end".into(), query::rfc3339(r.end).into());
    for (k, v) in &r.keys {
        o.insert(serde_json::to_value(k).ok().and_then(|k| k.as_str().map(String::from)).unwrap_or_default(), serde_json::json!(v));
    }
    let meters: serde_json::Map<String, serde_json::Value> = r
        .meters
        .iter()
        .map(|(m, raw)| {
            let (value, unit) = query::present(m, *raw);
            (m.clone(), serde_json::json!({ "raw": raw, "value": value, "unit": unit }))
        })
        .collect();
    o.insert("meters".into(), meters.into());
    if !r.flags.is_empty() {
        o.insert("flags".into(), serde_json::json!(r.flags));
    }
    o.into()
}

/// One line per row and meter (long format), raw and presented.
fn csv(rows: &[Row], group_by: &[GroupKey]) -> String {
    fn field(s: &str) -> String {
        if s.contains([',', '"', '\n']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }
    let key_name = |k: &GroupKey| serde_json::to_value(k).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let mut out = String::from("start,end");
    for k in group_by {
        let n = key_name(k);
        out.push_str(&format!(",{n}_id,{n}_name"));
    }
    out.push_str(",meter,raw,value,unit,flags\n");
    for r in rows {
        for (m, raw) in &r.meters {
            let (value, unit) = query::present(m, *raw);
            out.push_str(&format!("{},{}", query::rfc3339(r.start), query::rfc3339(r.end)));
            for k in group_by {
                match r.keys.get(k) {
                    Some(n) => out.push_str(&format!(",{},{}", field(&n.id), field(&n.name))),
                    None => out.push_str(",,"),
                }
            }
            let flags: Vec<&str> = r.flags.iter().map(String::as_str).collect();
            out.push_str(&format!(",{},{},{},{},{}\n", field(m), raw, value, unit, field(&flags.join(";"))));
        }
    }
    out
}
