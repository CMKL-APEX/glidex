//! Usage records (spec/13-metering.md §9.1–§9.2, §10): the REST twins of
//! `glidex.usage.v1.UsageService`'s usage queries, on `service::usage`.
//! They answer the proto messages as JSON, or with `format=csv` a CSV
//! export (REST only, audited).

use super::{ApiErr, Caller, Proto};
use crate::metering::query::{self, Granularity, GroupKey};
use crate::service::usage::{self as svc, bad, granularity_pb, group_key_pb};
use axum::{
    extract::{Path, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use buffa::EnumValue;
use buffa_types::google::protobuf::Timestamp;
use glidex_api::proto::glidex::usage::v1 as pb;

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
    /// Comma-separated: project, vm, disk, nic, network, node.
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

pub(super) fn list(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// A query time (RFC 3339 or unix seconds).
pub(super) fn time(s: &Option<String>) -> Result<Option<Timestamp>, ApiErr> {
    s.as_deref()
        .map(|t| query::parse_time(t).map(|t| Timestamp::from_unix_secs(t as i64)))
        .transpose()
        .map_err(|e| bad(e).into_rest())
}

/// An enum query value: the old word (`vm`) or the proto name (`GROUP_KEY_VM`).
fn word<'a>(s: &'a str, prefix: &str) -> std::borrow::Cow<'a, str> {
    match s.strip_prefix(prefix) {
        Some(w) => w.to_ascii_lowercase().into(),
        None => s.into(),
    }
}

pub(super) fn group_by(s: &Option<String>) -> Result<Vec<EnumValue<pb::GroupKey>>, ApiErr> {
    list(s)
        .iter()
        .map(|g| {
            word(g, "GROUP_KEY_")
                .parse::<GroupKey>()
                .map(|k| group_key_pb(k).into())
        })
        .collect::<Result<_, _>>()
        .map_err(|e| bad(e).into_rest())
}

fn granularity(s: &Option<String>) -> Result<EnumValue<pb::Granularity>, ApiErr> {
    match s {
        None => Ok(pb::Granularity::GRANULARITY_UNSPECIFIED.into()),
        Some(g) => word(g, "GRANULARITY_")
            .parse::<Granularity>()
            .map(|g| granularity_pb(g).into())
            .map_err(|e| bad(e).into_rest()),
    }
}

fn usage_query(p: &UsageParams) -> Result<pb::UsageQuery, ApiErr> {
    Ok(pb::UsageQuery {
        from: time(&p.from)?.into(),
        to: time(&p.to)?.into(),
        month: p.month.clone().unwrap_or_default(),
        granularity: granularity(&p.granularity)?,
        group_by: group_by(&p.group_by)?,
        meters: list(&p.meters),
        tz: p.tz.clone().unwrap_or_default(),
        ..Default::default()
    })
}

/// `GET /usage`: host readers see every project (also deleted ones);
/// everyone else the projects they may read usage of (§10).
pub async fn usage(c: Caller, Query(p): Query<UsageParams>) -> Result<Response, ApiErr> {
    let req = pb::GetUsageRequest {
        projects: list(&p.project),
        query: usage_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::get_usage(&c, req).await?;
    Ok(respond(&c, &p, r))
}

/// `GET /projects/{id}/usage`.
pub async fn project_usage(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<UsageParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetProjectUsageRequest {
        id,
        query: usage_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::project_usage(&c, req).await?;
    Ok(respond(&c, &p, r))
}

/// `GET /vms/{id}/usage`: the VM and its NICs (and, with
/// `include=disks`, the disks attached to it now).
pub async fn vm_usage(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<UsageParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetVmUsageRequest {
        id,
        query: usage_query(&p)?.into(),
        include_disks: list(&p.include)
            .iter()
            .any(|i| i == "disks")
            .then_some(true),
        ..Default::default()
    };
    let r = svc::vm_usage(&c, req).await?;
    Ok(respond(&c, &p, r))
}

/// `GET /disks/{id}/usage`.
pub async fn disk_usage(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<UsageParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetDiskUsageRequest {
        id,
        query: usage_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::disk_usage(&c, req).await?;
    Ok(respond(&c, &p, r))
}

fn respond(c: &Caller, p: &UsageParams, r: pb::GetUsageResponse) -> Response {
    if p.format.as_deref() == Some("csv") {
        // A bulk export is audited even though it is a read (§10).
        c.detail("export", serde_json::json!("csv"));
        c.audit_always();
        return ([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], csv(&r)).into_response();
    }
    Proto(StatusCode::OK, r).into_response()
}

/// A group key's name (`project`, `vm`, …), as in CSV headers.
fn key_name(k: &EnumValue<pb::GroupKey>) -> String {
    k.as_known()
        .map(|k| {
            buffa::Enumeration::proto_name(&k)
                .trim_start_matches("GROUP_KEY_")
                .to_ascii_lowercase()
        })
        .unwrap_or_default()
}

pub(super) fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn rfc3339(t: Option<&Timestamp>) -> String {
    query::rfc3339(t.map_or(0, |t| t.seconds.max(0) as u64))
}

/// One line per row and meter (long format), raw and presented.
fn csv(r: &pb::GetUsageResponse) -> String {
    let mut out = String::from("start,end");
    for k in &r.group_by {
        let n = key_name(k);
        out.push_str(&format!(",{n}_id,{n}_name"));
    }
    out.push_str(",meter,raw,value,unit,flags\n");
    for row in &r.rows {
        let mut meters: Vec<_> = row.meters.iter().collect();
        meters.sort_by(|a, b| a.0.cmp(b.0));
        for (m, v) in meters {
            out.push_str(&format!(
                "{},{}",
                rfc3339(row.start.as_option()),
                rfc3339(row.end.as_option())
            ));
            for k in &r.group_by {
                let named = match k.as_known() {
                    Some(pb::GroupKey::GROUP_KEY_PROJECT) => row.project.as_option(),
                    Some(pb::GroupKey::GROUP_KEY_VM) => row.vm.as_option(),
                    Some(pb::GroupKey::GROUP_KEY_DISK) => row.disk.as_option(),
                    Some(pb::GroupKey::GROUP_KEY_NIC) => row.nic.as_option(),
                    Some(pb::GroupKey::GROUP_KEY_NETWORK) => row.network.as_option(),
                    Some(pb::GroupKey::GROUP_KEY_NODE) => row.node.as_option(),
                    _ => None,
                };
                match named {
                    Some(n) => {
                        out.push_str(&format!(",{},{}", csv_field(&n.id), csv_field(&n.name)))
                    }
                    None => out.push_str(",,"),
                }
            }
            out.push_str(&format!(
                ",{},{},{},{},{}\n",
                csv_field(m),
                v.raw.unwrap_or(0),
                v.value.unwrap_or(0.0),
                v.unit,
                csv_field(&row.flags.join(";"))
            ));
        }
    }
    out
}
