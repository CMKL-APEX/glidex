//! `glidex.usage.v1` (spec/13-metering.md §9, §10): usage records, billing-month
//! rate reports (bandwidth, disk I/O, CPU and memory), the 5-minute series
//! for graphs and live rates. The REST twins in `api/usage.rs` and
//! `api/rates.rs` (which also write the CSV exports) and the RPC adapter in
//! `api/rpc/usage.rs` call these.

use super::common::ts;
use super::{Call, ServiceError, ServiceResult};
use crate::api::{add_network, disk_entities, project_entities, vm_entities};
use crate::authz::{Ent, EntitySet};
use crate::metering::ledger::{SlotRow, Subject};
use crate::metering::query::{self, Granularity, GroupKey, Named, Row};
use crate::metering::rates::{
    GroupSlots, SeriesKind, bandwidth_p95, compute_p95, disk_io_p95, group_slots, series_points,
};
use crate::metering::{Meter, MeteringError, SubjectKind, UsageRecord};
use axum::http::StatusCode;
use buffa::EnumValue;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::usage::v1 as pb;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Longest range one usage request may cover (§9.1).
const MAX_SPAN_SECS: u64 = 400 * 86400;
const DAY: u64 = 86400;

pub(crate) fn bad(msg: impl Into<String>) -> ServiceError {
    ServiceError::new(StatusCode::BAD_REQUEST, "invalid", msg)
}

fn storage(e: MeteringError) -> ServiceError {
    ServiceError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "persistence_error",
        e.to_string(),
    )
}

fn meter(c: &Call) -> ServiceResult<Arc<Meter>> {
    c.manager().meter().ok_or_else(|| {
        ServiceError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "metering_unavailable",
            "metering is not running",
        )
    })
}

// ---- conversions ------------------------------------------------------------

pub fn group_key_pb(k: GroupKey) -> pb::GroupKey {
    match k {
        GroupKey::Project => pb::GroupKey::GROUP_KEY_PROJECT,
        GroupKey::Vm => pb::GroupKey::GROUP_KEY_VM,
        GroupKey::Disk => pb::GroupKey::GROUP_KEY_DISK,
        GroupKey::Nic => pb::GroupKey::GROUP_KEY_NIC,
        GroupKey::Network => pb::GroupKey::GROUP_KEY_NETWORK,
        GroupKey::Node => pb::GroupKey::GROUP_KEY_NODE,
    }
}

fn group_key_of(k: &EnumValue<pb::GroupKey>) -> ServiceResult<GroupKey> {
    Ok(match k.as_known() {
        Some(pb::GroupKey::GROUP_KEY_PROJECT) => GroupKey::Project,
        Some(pb::GroupKey::GROUP_KEY_VM) => GroupKey::Vm,
        Some(pb::GroupKey::GROUP_KEY_DISK) => GroupKey::Disk,
        Some(pb::GroupKey::GROUP_KEY_NIC) => GroupKey::Nic,
        Some(pb::GroupKey::GROUP_KEY_NETWORK) => GroupKey::Network,
        Some(pb::GroupKey::GROUP_KEY_NODE) => GroupKey::Node,
        _ => {
            return Err(bad(
                "group_by: unknown key (project, vm, disk, nic, network, node)",
            ));
        }
    })
}

fn group_by_of(v: &[EnumValue<pb::GroupKey>]) -> ServiceResult<Vec<GroupKey>> {
    v.iter().map(group_key_of).collect()
}

fn group_by_pb(v: &[GroupKey]) -> Vec<EnumValue<pb::GroupKey>> {
    v.iter().map(|k| group_key_pb(*k).into()).collect()
}

pub fn granularity_pb(g: Granularity) -> pb::Granularity {
    match g {
        Granularity::Hour => pb::Granularity::GRANULARITY_HOUR,
        Granularity::Day => pb::Granularity::GRANULARITY_DAY,
        Granularity::Month => pb::Granularity::GRANULARITY_MONTH,
    }
}

fn granularity_of(g: &EnumValue<pb::Granularity>) -> ServiceResult<Granularity> {
    match g.as_known() {
        Some(pb::Granularity::GRANULARITY_HOUR) => Ok(Granularity::Hour),
        Some(pb::Granularity::GRANULARITY_DAY | pb::Granularity::GRANULARITY_UNSPECIFIED) => {
            Ok(Granularity::Day)
        }
        Some(pb::Granularity::GRANULARITY_MONTH) => Ok(Granularity::Month),
        None => Err(bad("granularity: unknown value (hour, day, month)")),
    }
}

/// A request time as unix seconds.
fn secs_of(t: Option<&Timestamp>) -> ServiceResult<Option<u64>> {
    t.map(|t| u64::try_from(t.seconds).map_err(|_| bad("times before 1970 have no usage")))
        .transpose()
}

fn none_if_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

/// A group key's value; projects and nodes get their current display
/// names from `names` when given.
fn named_pb(k: GroupKey, n: &Named, names: Option<&BTreeMap<String, String>>) -> pb::Named {
    let name = match names {
        Some(names) if matches!(k, GroupKey::Project | GroupKey::Node | GroupKey::Network) => {
            names.get(&n.id).unwrap_or(&n.name).clone()
        }
        _ => n.name.clone(),
    };
    pb::Named {
        id: n.id.clone(),
        name,
        ..Default::default()
    }
}

/// Set a row's key fields: `{ Project => project, … }` for the keys the
/// row has.
macro_rules! set_keys {
    ($row:expr, $keys:expr, $names:expr, { $($k:ident => $f:ident),* $(,)? }) => {
        for (k, v) in $keys {
            match k {
                $(GroupKey::$k => $row.$f = named_pb(*k, v, $names).into(),)*
                #[allow(unreachable_patterns)]
                _ => {}
            }
        }
    };
}

/// An internal figure (`rates::BandwidthP95`, …, or the stored final
/// figures) as its proto twin, which has the same field names.
fn via_json<T: serde::de::DeserializeOwned + Default>(v: Value) -> T {
    serde_json::from_value(v).unwrap_or_else(|e| {
        tracing::warn!("metering figures don't decode: {e}");
        T::default()
    })
}

fn slots_pb(counted: usize, interpolated: usize) -> pb::SlotCounts {
    pb::SlotCounts {
        counted: Some(counted as u32),
        interpolated: Some(interpolated as u32),
        ..Default::default()
    }
}

/// Re-type a message as another with the same fields (the per-operation
/// responses of one shape, as buf's unique-response rule asks).
pub fn recast<A: buffa::Message, B: buffa::Message + Default>(a: &A) -> B {
    B::decode_from_slice(&a.encode_to_vec()).unwrap_or_default()
}

/// Display names of projects, nodes and networks, by id (the ids don't
/// collide).
fn names(c: &Call) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = c
        .manager()
        .projects()
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    m.extend(
        c.manager()
            .nodes()
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.meta.id, n.spec.name)),
    );
    m.extend(
        c.manager()
            .list_networks()
            .unwrap_or_default()
            .into_iter()
            .map(|n| (n.id, n.name)),
    );
    m
}

// ---- scope and subjects ---------------------------------------------------------

/// The projects a usage request covers (§10): `None` = all (host
/// readers only). Requested projects the caller can't read are `404`.
fn scope(c: &Call, requested: &[String]) -> ServiceResult<Option<BTreeSet<String>>> {
    let wanted: Vec<&str> = requested
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    let all = c.allowed("readUsage", Ent::Cluster, EntitySet::new());
    if !wanted.is_empty() {
        let mut set = BTreeSet::new();
        for w in wanted {
            let id = c.target_project(Some(w))?;
            if !all {
                // Not readable: not found, never silently left out.
                c.require_readable(
                    "readProjectUsage",
                    Ent::Project(id.clone()),
                    project_entities(&id),
                    "project",
                )?;
            }
            set.insert(id);
        }
        if let [only] = Vec::from_iter(&set)[..] {
            c.set_project(only);
        }
        return Ok(Some(set));
    }
    if all {
        return Ok(None);
    }
    let mine: BTreeSet<String> = match c.visible_projects()? {
        crate::auth::LinkedProjects::All => c
            .manager()
            .projects()
            .list()
            .map(|v| v.into_iter().map(|p| p.id).collect())
            .unwrap_or_default(),
        crate::auth::LinkedProjects::Some(v) => v.into_iter().collect(),
    };
    let readable: BTreeSet<String> = mine
        .into_iter()
        .filter(|id| {
            c.allowed(
                "readProjectUsage",
                Ent::Project(id.clone()),
                project_entities(id),
            )
        })
        .collect();
    if readable.is_empty() {
        c.require_action("readUsage", Ent::Cluster, EntitySet::new(), &[])?;
    }
    Ok(Some(readable))
}

async fn visible_vm(c: &Call, id: &str) -> ServiceResult<crate::models::Vm> {
    let vm = c
        .manager()
        .get_vm(id)
        .await
        .map_err(|_| ServiceError::new(StatusCode::NOT_FOUND, "not_found", "VM not found"))?;
    c.set_project(&vm.project);
    c.set_target(format!("vm:{}", vm.id));
    let (e, es) = vm_entities(&vm);
    c.require_visible("readVm", e, es, "VM")?;
    Ok(vm)
}

async fn visible_disk(c: &Call, id: &str) -> ServiceResult<crate::images::DiskResponse> {
    let d = c
        .manager()
        .get_disk(id)
        .await
        .map_err(|_| ServiceError::new(StatusCode::NOT_FOUND, "not_found", "disk not found"))?;
    c.set_project(&d.project);
    c.set_target(format!("disk:{}", d.id));
    let (e, es) = disk_entities(&d.id, &d.project);
    c.require_visible("readDisk", e, es, "disk")?;
    Ok(d)
}

fn visible_network(c: &Call, key: &str) -> ServiceResult<crate::network::Network> {
    let n = crate::service::network::visible_network(c, key)?;
    let mut es = EntitySet::new();
    let e = add_network(&mut es, &n);
    c.require(e, es)?;
    Ok(n)
}

// ---- usage records (§9.1, §9.2) ---------------------------------------------------

/// `GET /usage`.
pub async fn get_usage(c: &Call, req: pb::GetUsageRequest) -> ServiceResult<pb::GetUsageResponse> {
    let projects = scope(c, &req.projects)?;
    usage(
        c,
        req.query.as_option(),
        projects,
        &[GroupKey::Project],
        |_| true,
    )
}

/// `GET /projects/{id}/usage`.
pub async fn project_usage(
    c: &Call,
    req: pb::GetProjectUsageRequest,
) -> ServiceResult<pb::GetUsageResponse> {
    let pid = c.target_project(Some(&req.id))?;
    c.set_project(&pid);
    c.require_readable(
        "readProject",
        Ent::Project(pid.clone()),
        project_entities(&pid),
        "project",
    )?;
    c.require(Ent::Project(pid.clone()), project_entities(&pid))?;
    usage(
        c,
        req.query.as_option(),
        Some([pid].into()),
        &[GroupKey::Project],
        |_| true,
    )
}

/// `GET /vms/{id}/usage`: the VM and its NICs (and, with `include_disks`,
/// the disks attached to it now).
pub async fn vm_usage(c: &Call, req: pb::GetVmUsageRequest) -> ServiceResult<pb::GetUsageResponse> {
    let vm = visible_vm(c, &req.id).await?;
    let disks: BTreeSet<String> = if req.include_disks == Some(true) {
        c.manager().attached_disk_ids(&vm.id)
    } else {
        BTreeSet::new()
    };
    let vm_id = vm.id.clone();
    usage(
        c,
        req.query.as_option(),
        Some([vm.project.clone()].into()),
        &[GroupKey::Vm],
        move |r| match r.subject.kind {
            SubjectKind::Vm => r.subject.id == vm_id,
            SubjectKind::Nic => r.subject.vm_id.as_deref() == Some(vm_id.as_str()),
            SubjectKind::Disk => disks.contains(&r.subject.id),
            _ => false,
        },
    )
}

/// `GET /disks/{id}/usage`.
pub async fn disk_usage(
    c: &Call,
    req: pb::GetDiskUsageRequest,
) -> ServiceResult<pb::GetUsageResponse> {
    let d = visible_disk(c, &req.id).await?;
    let disk_id = d.id.clone();
    usage(
        c,
        req.query.as_option(),
        Some([d.project.clone()].into()),
        &[GroupKey::Disk],
        move |r| r.subject.kind == SubjectKind::Disk && r.subject.id == disk_id,
    )
}

fn usage(
    c: &Call,
    q: Option<&pb::UsageQuery>,
    projects: Option<BTreeSet<String>>,
    default_group: &[GroupKey],
    keep: impl Fn(&UsageRecord) -> bool,
) -> ServiceResult<pb::GetUsageResponse> {
    let default_q = pb::UsageQuery::default();
    let q = q.unwrap_or(&default_q);
    let meter = meter(c)?;
    let tz = query::parse_tz(none_if_empty(&q.tz).unwrap_or(&meter.config().billing_timezone))
        .map_err(bad)?;
    let now = crate::metering::now_ms() / 1000;
    let month = none_if_empty(&q.month)
        .map(|m| query::month_bounds(m, &tz))
        .transpose()
        .map_err(bad)?;
    let to = secs_of(q.to.as_option())?.unwrap_or(now).div_ceil(3600) * 3600;
    let from = match secs_of(q.from.as_option())? {
        // Default: the start of the current billing month.
        None => query::bucket(now, Granularity::Month, &tz).0,
        Some(f) => f / 3600 * 3600,
    };
    let (from, to) = month.unwrap_or((from, to));
    if to <= from {
        return Err(bad("`to` must be after `from`"));
    }
    if to - from > MAX_SPAN_SECS {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "range_too_large",
            "at most 400 days per request",
        ));
    }
    let granularity = granularity_of(&q.granularity)?;
    let mut group_by = group_by_of(&q.group_by)?;
    if group_by.is_empty() {
        group_by = default_group.to_vec();
    }
    let meters: Option<BTreeSet<String>> = (!q.meters.is_empty()).then(|| {
        q.meters
            .iter()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
            .collect()
    });
    let ledger = meter.ledger();
    let records: Vec<UsageRecord> = ledger
        .scan(from, to, projects.as_ref())
        .map_err(storage)?
        .into_iter()
        .filter(|r| keep(r))
        .collect();
    let aq = query::Query {
        from,
        to,
        granularity,
        tz: tz.clone(),
        group_by: group_by.clone(),
        meters,
        now,
    };
    let mut rows = query::aggregate(&records, &aq);
    let started_at = ledger.started_at().map_err(storage)? / 1000;
    for row in &mut rows {
        if row.start < started_at {
            row.flags.insert("partial".into());
        }
    }
    let names = names(c);
    let complete_through = ledger.complete_through().map_err(storage)?;
    Ok(pb::GetUsageResponse {
        from: ts(from).into(),
        to: ts(to).into(),
        granularity: granularity_pb(granularity).into(),
        timezone: tz.name.clone(),
        group_by: group_by_pb(&group_by),
        rows: rows.iter().map(|r| usage_row_pb(r, &names)).collect(),
        complete_through: ts(complete_through).into(),
        metering_started_at: ts(started_at).into(),
        ..Default::default()
    })
}

fn usage_row_pb(r: &Row, names: &BTreeMap<String, String>) -> pb::UsageRow {
    let mut row = pb::UsageRow {
        start: ts(r.start).into(),
        end: ts(r.end).into(),
        meters: r
            .meters
            .iter()
            .map(|(m, raw)| {
                let (value, unit) = query::present(m, *raw);
                (
                    m.clone(),
                    pb::MeterValue {
                        raw: Some(*raw),
                        value: Some(value),
                        unit: unit.into(),
                        ..Default::default()
                    },
                )
            })
            .collect(),
        flags: r.flags.iter().cloned().collect(),
        ..Default::default()
    };
    set_keys!(row, &r.keys, Some(names), {
        Project => project, Vm => vm, Disk => disk, Nic => nic, Network => network, Node => node,
    });
    row
}

// ---- billing-month reports (§9.3) --------------------------------------------------

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

struct RangeQuery<'a> {
    month: &'a str,
    from: Option<&'a Timestamp>,
    to: Option<&'a Timestamp>,
    tz: &'a str,
}

fn range(
    q: &RangeQuery,
    billing_tz: &str,
    max_days: u64,
    default_days: Option<u64>,
) -> ServiceResult<Range> {
    let tz = query::parse_tz(none_if_empty(q.tz).unwrap_or(billing_tz)).map_err(bad)?;
    let now = crate::metering::now_ms() / 1000;
    let (from, to, month) = match (none_if_empty(q.month), secs_of(q.from)?, default_days) {
        (Some(m), _, _) => {
            let (s, e) = query::month_bounds(m, &tz).map_err(bad)?;
            (s, e, Some(m.to_string()))
        }
        (None, Some(f), _) => {
            let to = secs_of(q.to)?.unwrap_or(now);
            (f / 3600 * 3600, to.div_ceil(3600) * 3600, None)
        }
        (None, None, Some(days)) => (
            now.saturating_sub(days * DAY) / 300 * 300,
            now.div_ceil(300) * 300,
            None,
        ),
        (None, None, None) => {
            let (s, e) = query::bucket(now, Granularity::Month, &tz);
            (s, e, Some(query::month_label(now, &tz)))
        }
    };
    if to <= from {
        return Err(bad("`to` must be after `from`"));
    }
    if to - from > max_days * DAY {
        return Err(ServiceError::new(
            StatusCode::BAD_REQUEST,
            "range_too_large",
            format!("at most {max_days} days per request"),
        ));
    }
    Ok(Range {
        from,
        to,
        month,
        tz,
        now,
    })
}

/// A report before it is encoded per family.
struct Report {
    range: Range,
    group_by: Vec<GroupKey>,
    kind: SubjectKind,
    groups: BTreeMap<Vec<Option<String>>, GroupSlots>,
    /// Hourly totals by the same groups, for averages and peaks.
    totals: BTreeMap<Vec<Option<String>>, BTreeMap<String, u64>>,
    /// The month's stored final figures (`{keys, p95}`) when its slots are gone.
    finals: Vec<Value>,
    names: BTreeMap<String, String>,
    sample_secs: u64,
    complete: u64,
}

fn report(
    c: &Call,
    projects: &[String],
    q: Option<&pb::ReportQuery>,
    family: Family,
) -> ServiceResult<Report> {
    let projects = scope(c, projects)?;
    let meter = meter(c)?;
    let cfg = meter.config().clone();
    let default_q = pb::ReportQuery::default();
    let q = q.unwrap_or(&default_q);
    let r = range(
        &RangeQuery {
            month: &q.month,
            from: q.from.as_option(),
            to: q.to.as_option(),
            tz: &q.tz,
        },
        &cfg.billing_timezone,
        100,
        None,
    )?;
    let allowed: &[GroupKey] = match family {
        Family::Bandwidth => &[
            GroupKey::Project,
            GroupKey::Vm,
            GroupKey::Nic,
            GroupKey::Network,
            GroupKey::Node,
        ],
        Family::DiskIo => &[
            GroupKey::Project,
            GroupKey::Vm,
            GroupKey::Disk,
            GroupKey::Node,
        ],
        Family::Compute => &[GroupKey::Project, GroupKey::Vm, GroupKey::Node],
    };
    let mut group_by = group_by_of(&q.group_by)?;
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
    let in_scope = |s: &Subject| {
        s.kind == kind
            && projects
                .as_ref()
                .is_none_or(|ps| s.project.as_ref().is_some_and(|x| ps.contains(x)))
    };
    let slots: Vec<(u64, SlotRow)> = meter
        .ledger()
        .scan_slots(r.from, r.to, projects.as_ref(), in_scope)
        .map_err(storage)?;
    let groups = group_slots(&slots, r.from, r.to, &group_by);
    // Averages and peaks from the hourly rows, by the same groups.
    let records: Vec<_> = meter
        .ledger()
        .scan(r.from, r.to, projects.as_ref())
        .map_err(storage)?
        .into_iter()
        .filter(|x| in_scope(&x.subject))
        .collect();
    let aq = query::Query {
        from: r.from,
        to: r.to,
        granularity: Granularity::Hour,
        tz: query::Tz::utc(),
        group_by: group_by.clone(),
        meters: None,
        now: r.now,
    };
    let mut totals: BTreeMap<Vec<Option<String>>, BTreeMap<String, u64>> = BTreeMap::new();
    for row in query::aggregate(&records, &aq) {
        let ids = group_by
            .iter()
            .map(|k| row.keys.get(k).map(|n| n.id.clone()))
            .collect();
        let t = totals.entry(ids).or_default();
        for (m, v) in row.meters {
            let e = t.entry(m.clone()).or_insert(0);
            *e = if crate::metering::ledger::is_max_meter(&m) {
                (*e).max(v)
            } else {
                e.saturating_add(v)
            };
        }
    }
    // Slots gone (older than retention_rate_days): the month's final figures.
    let mut finals = Vec::new();
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
            for (_, v) in meter
                .ledger()
                .month_rates(r.from, &format!("{gname}/"))
                .map_err(storage)?
            {
                let project = v["keys"]["project"]["id"].as_str().map(String::from);
                if projects
                    .as_ref()
                    .is_none_or(|ps| project.as_ref().is_some_and(|x| ps.contains(x)))
                {
                    finals.push(v);
                }
            }
        }
    }
    let complete = meter.ledger().complete_through().map_err(storage)?;
    Ok(Report {
        range: r,
        group_by,
        kind,
        groups,
        totals,
        finals,
        names: names(c),
        sample_secs: cfg.sample_secs,
        complete,
    })
}

/// The group keys of stored final figures (`{"project": {id, name}, …}`).
fn final_keys(v: &Value) -> BTreeMap<GroupKey, Named> {
    v["keys"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, n)| {
            Some((
                k.parse().ok()?,
                Named {
                    id: n["id"].as_str()?.to_string(),
                    name: n["name"].as_str().unwrap_or_default().to_string(),
                },
            ))
        })
        .collect()
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Averages over the seconds a group was present, and 30-second peaks,
/// from its hourly totals (§8.5, §8.6).
struct Totals<'a> {
    meters: &'a BTreeMap<String, u64>,
    secs: u64,
}

impl Totals<'_> {
    fn g(&self, m: &str) -> Option<u64> {
        self.meters.get(m).copied()
    }
    fn per_s(&self, v: Option<u64>, div: f64) -> Option<f64> {
        v.and_then(|v| (self.secs > 0).then(|| r3(v as f64 / self.secs as f64 / div)))
    }
    fn bits_per_s(&self, m: &str) -> Option<f64> {
        self.per_s(self.g(m).map(|b| b * 8), 1e6)
    }
    fn peak(&self, m: &str, div: f64) -> Option<f64> {
        self.g(m).map(|v| r3(v as f64 / div))
    }
}

/// The row's own totals, its seconds present and its keys.
fn group_totals<'a>(
    r: &'a Report,
    ids: &Vec<Option<String>>,
    g: &GroupSlots,
    empty: &'a BTreeMap<String, u64>,
) -> Totals<'a> {
    Totals {
        meters: r.totals.get(ids).unwrap_or(empty),
        secs: g.slots.len() as u64 * 300,
    }
}

fn report_head(r: &Report) -> (String, Timestamp, Timestamp, String, bool, bool) {
    (
        r.range.month.clone().unwrap_or_default(),
        ts(r.range.from),
        ts(r.range.to),
        r.range.tz.name.clone(),
        r.complete >= r.range.to,
        !r.finals.is_empty(),
    )
}

/// `GET /usage/bandwidth`.
pub async fn bandwidth_report(
    c: &Call,
    req: pb::GetBandwidthReportRequest,
) -> ServiceResult<pb::GetBandwidthReportResponse> {
    let r = report(c, &req.projects, req.query.as_option(), Family::Bandwidth)?;
    let empty = BTreeMap::new();
    let res = Some(r.sample_secs as u32);
    let mut rows = Vec::new();
    for (ids, g) in &r.groups {
        let t = group_totals(&r, ids, g, &empty);
        let (avg, peak) = if r.kind == SubjectKind::Network {
            (
                pb::BandwidthRates {
                    mbps: t.bits_per_s("bridge.bytes"),
                    ext_rx_mbps: t.bits_per_s("bridge.ext_rx_bytes"),
                    ext_tx_mbps: t.bits_per_s("bridge.ext_tx_bytes"),
                    ..Default::default()
                },
                pb::BandwidthRates {
                    mbps: t.peak("bridge.kbps_peak", 1000.0),
                    ext_rx_mbps: t.peak("bridge.ext_rx_kbps_peak", 1000.0),
                    ext_tx_mbps: t.peak("bridge.ext_tx_kbps_peak", 1000.0),
                    resolution_secs: res,
                    ..Default::default()
                },
            )
        } else {
            (
                pb::BandwidthRates {
                    rx_mbps: t.bits_per_s("net.rx_bytes"),
                    tx_mbps: t.bits_per_s("net.tx_bytes"),
                    ext_rx_mbps: t.bits_per_s("net.ext_rx_bytes"),
                    ext_tx_mbps: t.bits_per_s("net.ext_tx_bytes"),
                    ..Default::default()
                },
                pb::BandwidthRates {
                    rx_mbps: t.peak("net.rx_kbps_peak", 1000.0),
                    tx_mbps: t.peak("net.tx_kbps_peak", 1000.0),
                    ext_rx_mbps: t.peak("net.ext_rx_kbps_peak", 1000.0),
                    ext_tx_mbps: t.peak("net.ext_tx_kbps_peak", 1000.0),
                    resolution_secs: res,
                    ..Default::default()
                },
            )
        };
        let p = bandwidth_p95(g, r.kind);
        let slots = slots_pb(p.slots.counted, p.slots.interpolated);
        let mut p95: pb::BandwidthP95 = via_json(serde_json::to_value(&p).unwrap_or_default());
        p95.slots = None.into();
        let mut row = pb::BandwidthRow {
            avg: avg.into(),
            peak: peak.into(),
            p95: p95.into(),
            slots: slots.into(),
            ..Default::default()
        };
        set_keys!(row, &g.keys, Some(&r.names), {
            Project => project, Vm => vm, Nic => nic, Network => network, Node => node,
        });
        rows.push(row);
    }
    for v in &r.finals {
        let mut row = pb::BandwidthRow {
            p95: via_json::<pb::BandwidthP95>(v["p95"].clone()).into(),
            ..Default::default()
        };
        set_keys!(row, &final_keys(v), None, {
            Project => project, Vm => vm, Nic => nic, Network => network, Node => node,
        });
        rows.push(row);
    }
    let (month, from, to, timezone, fin, from_final) = report_head(&r);
    Ok(pb::GetBandwidthReportResponse {
        month,
        from: from.into(),
        to: to.into(),
        timezone,
        r#final: Some(fin),
        from_final_figures: Some(from_final),
        group_by: group_by_pb(&r.group_by),
        rows,
        ..Default::default()
    })
}

/// `GET /usage/disk-io`.
pub async fn disk_io_report(
    c: &Call,
    req: pb::GetDiskIoReportRequest,
) -> ServiceResult<pb::GetDiskIoReportResponse> {
    let r = report(c, &req.projects, req.query.as_option(), Family::DiskIo)?;
    let empty = BTreeMap::new();
    let mut rows = Vec::new();
    for (ids, g) in &r.groups {
        let t = group_totals(&r, ids, g, &empty);
        let lat = |tm: &str, o: &str| match (t.g(tm), t.g(o)) {
            (Some(tm), Some(o)) if o > 0 => Some(r3(tm as f64 / o as f64 / 1e6)),
            _ => None,
        };
        let avg = pb::DiskIoRates {
            read_iops: t.per_s(t.g("disk.read_ops"), 1.0),
            write_iops: t.per_s(t.g("disk.write_ops"), 1.0),
            read_mbps: t.per_s(t.g("disk.read_bytes"), 1e6),
            write_mbps: t.per_s(t.g("disk.write_bytes"), 1e6),
            read_latency_ms: lat("disk.read_time_ns", "disk.read_ops"),
            write_latency_ms: lat("disk.write_time_ns", "disk.write_ops"),
            ..Default::default()
        };
        let peak = pb::DiskIoRates {
            read_iops: t.peak("disk.read_iops_peak", 1000.0),
            write_iops: t.peak("disk.write_iops_peak", 1000.0),
            iops: t.peak("disk.iops_peak", 1000.0),
            read_mbps: t.peak("disk.read_kBps_peak", 1000.0),
            write_mbps: t.peak("disk.write_kBps_peak", 1000.0),
            mbps: t.peak("disk.kBps_peak", 1000.0),
            resolution_secs: Some(r.sample_secs as u32),
            ..Default::default()
        };
        let p = disk_io_p95(g);
        let slots = slots_pb(p.slots.counted, p.slots.interpolated);
        let mut p95: pb::DiskIoP95 = via_json(serde_json::to_value(&p).unwrap_or_default());
        p95.slots = None.into();
        let source = if t.g("disk.read_time_ns").is_some() {
            pb::LatencySource::LATENCY_SOURCE_COUNTER
        } else {
            pb::LatencySource::LATENCY_SOURCE_NONE
        };
        let mut row = pb::DiskIoRow {
            avg: avg.into(),
            peak: peak.into(),
            p95: p95.into(),
            slots: slots.into(),
            latency_source: source.into(),
            ..Default::default()
        };
        set_keys!(row, &g.keys, Some(&r.names), {
            Project => project, Vm => vm, Disk => disk, Node => node,
        });
        rows.push(row);
    }
    for v in &r.finals {
        let mut row = pb::DiskIoRow {
            p95: via_json::<pb::DiskIoP95>(v["p95"].clone()).into(),
            ..Default::default()
        };
        set_keys!(row, &final_keys(v), None, {
            Project => project, Vm => vm, Disk => disk, Node => node,
        });
        rows.push(row);
    }
    let (month, from, to, timezone, fin, from_final) = report_head(&r);
    Ok(pb::GetDiskIoReportResponse {
        month,
        from: from.into(),
        to: to.into(),
        timezone,
        r#final: Some(fin),
        from_final_figures: Some(from_final),
        group_by: group_by_pb(&r.group_by),
        rows,
        ..Default::default()
    })
}

/// `GET /usage/compute`.
pub async fn compute_report(
    c: &Call,
    req: pb::GetComputeReportRequest,
) -> ServiceResult<pb::GetComputeReportResponse> {
    let r = report(c, &req.projects, req.query.as_option(), Family::Compute)?;
    let empty = BTreeMap::new();
    let mut rows = Vec::new();
    for (ids, g) in &r.groups {
        let t = group_totals(&r, ids, g, &empty);
        let ratio = |u: Option<u64>, a: Option<u64>, scale: f64| match (u, a) {
            (Some(u), Some(a)) if a > 0 => Some(r3(u as f64 * 100.0 / (a as f64 * scale))),
            _ => None,
        };
        let avg = pb::ComputeRates {
            cpu_cores: t.per_s(t.g("cpu.used"), 1e6),
            vcpus: t.per_s(t.g("cpu.alloc"), 1.0),
            cpu_percent: ratio(t.g("cpu.used"), t.g("cpu.alloc"), 1e6),
            mem_mib: t.per_s(t.g("mem.used"), 1.0),
            mem_alloc_mib: t.per_s(t.g("mem.alloc"), 1.0),
            mem_percent: ratio(t.g("mem.used"), t.g("mem.alloc"), 1.0),
            ..Default::default()
        };
        let peak = pb::ComputeRates {
            cpu_cores: t.peak("cpu.cores_peak", 1000.0),
            mem_mib: t.peak("mem.peak", 1.0),
            resolution_secs: Some(r.sample_secs as u32),
            ..Default::default()
        };
        let p = compute_p95(g);
        let slots = slots_pb(p.slots.counted, p.slots.interpolated);
        let mut p95: pb::ComputeP95 = via_json(serde_json::to_value(&p).unwrap_or_default());
        p95.slots = None.into();
        let mut row = pb::ComputeRow {
            avg: avg.into(),
            peak: peak.into(),
            p95: p95.into(),
            slots: slots.into(),
            ..Default::default()
        };
        set_keys!(row, &g.keys, Some(&r.names), {
            Project => project, Vm => vm, Node => node,
        });
        rows.push(row);
    }
    for v in &r.finals {
        let mut row = pb::ComputeRow {
            p95: via_json::<pb::ComputeP95>(v["p95"].clone()).into(),
            ..Default::default()
        };
        set_keys!(row, &final_keys(v), None, {
            Project => project, Vm => vm, Node => node,
        });
        rows.push(row);
    }
    let (month, from, to, timezone, fin, from_final) = report_head(&r);
    Ok(pb::GetComputeReportResponse {
        month,
        from: from.into(),
        to: to.into(),
        timezone,
        r#final: Some(fin),
        from_final_figures: Some(from_final),
        group_by: group_by_pb(&r.group_by),
        rows,
        ..Default::default()
    })
}

// ---- series (§9.3) ------------------------------------------------------------------

/// One series: its range, its points and, when asked for, its p95.
struct Series<P> {
    from: Timestamp,
    to: Timestamp,
    points: Vec<pb::RatePoint>,
    p95: Option<P>,
}

/// A 5-minute series for graphs (§9.3).
fn series<P: serde::de::DeserializeOwned + Default>(
    c: &Call,
    q: Option<&pb::SeriesQuery>,
    kind: SubjectKind,
    project: Option<&str>,
    group_by: &[GroupKey],
    keep: impl Fn(&Subject) -> bool,
) -> ServiceResult<Series<P>> {
    let meter = meter(c)?;
    let default_q = pb::SeriesQuery::default();
    let q = q.unwrap_or(&default_q);
    let r = range(
        &RangeQuery {
            month: &q.month,
            from: q.from.as_option(),
            to: q.to.as_option(),
            tz: &q.tz,
        },
        &meter.config().billing_timezone,
        31,
        Some(1),
    )?;
    let rows = meter
        .ledger()
        .scan_slots(
            r.from,
            r.to,
            project.map(|p| BTreeSet::from([p.to_string()])).as_ref(),
            |s| s.kind == kind && keep(s),
        )
        .map_err(storage)?;
    let groups = group_slots(&rows, r.from, r.to, group_by);
    let skind = match kind {
        SubjectKind::Disk => SeriesKind::Disk,
        SubjectKind::Vm => SeriesKind::Compute,
        _ => SeriesKind::Network,
    };
    let (points, p95) = match groups.values().next() {
        Some(g) => {
            let points = series_points(g, skind).into_iter().map(via_json).collect();
            let p95 = (q.p95 == Some(true)).then(|| {
                via_json(
                    match skind {
                        SeriesKind::Disk => serde_json::to_value(disk_io_p95(g)),
                        SeriesKind::Compute => serde_json::to_value(compute_p95(g)),
                        SeriesKind::Network => serde_json::to_value(bandwidth_p95(g, kind)),
                    }
                    .unwrap_or_default(),
                )
            });
            (points, p95)
        }
        None => (Vec::new(), None),
    };
    Ok(Series {
        from: ts(r.from),
        to: ts(r.to),
        points,
        p95,
    })
}

/// `GET /vms/{id}/bandwidth`.
pub async fn vm_bandwidth(
    c: &Call,
    req: pb::GetVmBandwidthRequest,
) -> ServiceResult<pb::GetVmBandwidthResponse> {
    let vm = visible_vm(c, &req.id).await?;
    let s = series::<pb::BandwidthP95>(
        c,
        req.query.as_option(),
        SubjectKind::Nic,
        Some(&vm.project),
        &[GroupKey::Vm],
        |s| s.vm_id.as_deref() == Some(vm.id.as_str()),
    )?;
    Ok(pb::GetVmBandwidthResponse {
        from: s.from.into(),
        to: s.to.into(),
        points: s.points,
        p95: s.p95.into(),
        ..Default::default()
    })
}

/// `GET /networks/{id}/bandwidth`.
pub async fn network_bandwidth(
    c: &Call,
    req: pb::GetNetworkBandwidthRequest,
) -> ServiceResult<pb::GetNetworkBandwidthResponse> {
    let n = visible_network(c, &req.name)?;
    let s = series::<pb::BandwidthP95>(
        c,
        req.query.as_option(),
        SubjectKind::Network,
        n.project.as_deref(),
        &[GroupKey::Network],
        |s| s.id == n.id,
    )?;
    Ok(pb::GetNetworkBandwidthResponse {
        from: s.from.into(),
        to: s.to.into(),
        points: s.points,
        p95: s.p95.into(),
        ..Default::default()
    })
}

/// `GET /vms/{id}/io`: the VM's disks summed, and each separately.
pub async fn vm_io(c: &Call, req: pb::GetVmIoRequest) -> ServiceResult<pb::GetVmIoResponse> {
    let vm = visible_vm(c, &req.id).await?;
    let q = req.query.as_option();
    let s = series::<pb::DiskIoP95>(
        c,
        q,
        SubjectKind::Disk,
        Some(&vm.project),
        &[GroupKey::Vm],
        |s| s.vm_id.as_deref() == Some(vm.id.as_str()),
    )?;
    let mut disks = std::collections::HashMap::new();
    for d in c.manager().attached_disk_ids(&vm.id) {
        let one = series::<pb::DiskIoP95>(
            c,
            q,
            SubjectKind::Disk,
            Some(&vm.project),
            &[GroupKey::Disk],
            |s| s.id == d,
        )?;
        disks.insert(
            d.clone(),
            pb::DiskIoSeries {
                from: one.from.into(),
                to: one.to.into(),
                points: one.points,
                p95: one.p95.into(),
                ..Default::default()
            },
        );
    }
    Ok(pb::GetVmIoResponse {
        from: s.from.into(),
        to: s.to.into(),
        points: s.points,
        p95: s.p95.into(),
        disks: disks.into_iter().collect(),
        ..Default::default()
    })
}

/// `GET /disks/{id}/io`.
pub async fn disk_io(c: &Call, req: pb::GetDiskIoRequest) -> ServiceResult<pb::GetDiskIoResponse> {
    let d = visible_disk(c, &req.id).await?;
    let s = series::<pb::DiskIoP95>(
        c,
        req.query.as_option(),
        SubjectKind::Disk,
        Some(&d.project),
        &[GroupKey::Disk],
        |s| s.id == d.id,
    )?;
    Ok(pb::GetDiskIoResponse {
        from: s.from.into(),
        to: s.to.into(),
        points: s.points,
        p95: s.p95.into(),
        ..Default::default()
    })
}

/// `GET /vms/{id}/compute`.
pub async fn vm_compute(
    c: &Call,
    req: pb::GetVmComputeRequest,
) -> ServiceResult<pb::GetVmComputeResponse> {
    let vm = visible_vm(c, &req.id).await?;
    let s = series::<pb::ComputeP95>(
        c,
        req.query.as_option(),
        SubjectKind::Vm,
        Some(&vm.project),
        &[GroupKey::Vm],
        |s| s.id == vm.id,
    )?;
    Ok(pb::GetVmComputeResponse {
        from: s.from.into(),
        to: s.to.into(),
        points: s.points,
        p95: s.p95.into(),
        ..Default::default()
    })
}

// ---- live rates (§9.4) ------------------------------------------------------------------

/// `GET /vms/{id}/stats`: the latest rates of the VM, its NICs and its
/// disks, from the last two samples (not stored).
pub async fn vm_stats(
    c: &Call,
    req: pb::GetVmStatsRequest,
) -> ServiceResult<pb::GetVmStatsResponse> {
    let vm = visible_vm(c, &req.id).await?;
    // A VM on another node: its live rates are that node's (§8.4).
    if let Some(addr) = c.manager().remote_node_addr(&vm) {
        let Some(cluster) = c.manager().cluster() else {
            return Err(ServiceError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "no cluster",
            ));
        };
        let r = cluster
            .client
            .request(
                &addr,
                axum::http::Method::GET,
                &format!("/cluster/v1/vms/{}/stats", vm.id),
                &[],
                bytes::Bytes::new(),
            )
            .await
            .map_err(|e| {
                ServiceError::new(StatusCode::BAD_GATEWAY, "node_unreachable", e.to_string())
            })?;
        if !r.status.is_success() {
            return Err(ServiceError::new(
                r.status,
                "unavailable",
                "the node has no stats for it",
            ));
        }
        // The node answers this message as JSON; a node from before the
        // port sends the same field names (with nulls for unset ones).
        return serde_json::from_slice(&r.body)
            .map_err(|e| ServiceError::new(StatusCode::BAD_GATEWAY, "bad_gateway", e.to_string()));
    }
    let meter = meter(c)?;
    Ok(live_vm_stats(&meter, &vm.id))
}

/// The latest rates of a VM, its NICs and its disks (also what a node tells
/// the server that relays them).
pub(crate) fn live_vm_stats(meter: &Meter, vm_id: &str) -> pb::GetVmStatsResponse {
    let live = meter.live_stats(|s| {
        (s.kind == SubjectKind::Vm && s.id == vm_id) || s.vm_id.as_deref() == Some(vm_id)
    });
    let pick = |kind: SubjectKind| -> Vec<pb::LiveRates> {
        live.iter()
            .filter(|l| l.subject.kind == kind)
            .map(|l| pb::LiveRates {
                id: l.subject.id.clone(),
                name: l.subject.name.clone(),
                network: l.subject.network.clone().unwrap_or_default(),
                values: l.values.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                ..Default::default()
            })
            .collect()
    };
    let vm_values = live
        .iter()
        .find(|l| l.subject.kind == SubjectKind::Vm)
        .map(|l| l.values.iter().map(|(k, v)| (k.clone(), *v)).collect())
        .unwrap_or_default();
    let at = live.iter().map(|l| l.sampled_at).max();
    pb::GetVmStatsResponse {
        sampled_at: at.map(|t| ts(t / 1000)).into(),
        resolution_secs: Some(meter.config().sample_secs as u32),
        vm: vm_values,
        nics: pick(SubjectKind::Nic),
        disks: pick(SubjectKind::Disk),
        ..Default::default()
    }
}

/// `GET /networks/{id}/stats` (§9.4).
pub async fn network_stats(
    c: &Call,
    req: pb::GetNetworkStatsRequest,
) -> ServiceResult<pb::GetNetworkStatsResponse> {
    let n = visible_network(c, &req.name)?;
    let meter = meter(c)?;
    let live = meter.live_stats(|s| s.kind == SubjectKind::Network && s.id == n.id);
    Ok(pb::GetNetworkStatsResponse {
        sampled_at: live.first().map(|l| ts(l.sampled_at / 1000)).into(),
        resolution_secs: Some(meter.config().sample_secs as u32),
        values: live
            .first()
            .map(|l| l.values.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default(),
        ..Default::default()
    })
}
