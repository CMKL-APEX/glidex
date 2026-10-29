//! Bandwidth, disk I/O, CPU and memory, and live rates (spec/13-metering.md
//! §9.3–§9.4): the REST twins of `glidex.usage.v1`'s reports
//! (`UsageService`) and of `MetricsService`, on `service::usage`. The
//! reports also answer CSV (`format=csv`, REST only, audited).

use super::usage::{csv_field, group_by, list, time};
use super::{ApiErr, Caller, Proto};
use crate::service::usage as svc;
use axum::{
    extract::{Path, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use glidex_api::proto::glidex::usage::v1 as pb;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

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

fn report_query(p: &RateParams) -> Result<pb::ReportQuery, ApiErr> {
    Ok(pb::ReportQuery {
        month: p.month.clone().unwrap_or_default(),
        from: time(&p.from)?.into(),
        to: time(&p.to)?.into(),
        group_by: group_by(&p.group_by)?,
        tz: p.tz.clone().unwrap_or_default(),
        ..Default::default()
    })
}

fn series_query(p: &RateParams) -> Result<pb::SeriesQuery, ApiErr> {
    Ok(pb::SeriesQuery {
        month: p.month.clone().unwrap_or_default(),
        from: time(&p.from)?.into(),
        to: time(&p.to)?.into(),
        tz: p.tz.clone().unwrap_or_default(),
        p95: p.p95,
        ..Default::default()
    })
}

/// A report as JSON, or its rows as CSV with `format=csv`.
fn report<T: serde::Serialize>(c: &Caller, p: &RateParams, r: T) -> Response {
    if p.format.as_deref() == Some("csv") {
        c.detail("export", serde_json::json!("csv"));
        c.audit_always();
        let v = serde_json::to_value(&r).unwrap_or_default();
        return (
            [(header::CONTENT_TYPE, "text/csv; charset=utf-8")],
            flat_csv(&v["rows"]),
        )
            .into_response();
    }
    Proto(StatusCode::OK, r).into_response()
}

/// Rows as CSV: one column per leaf (`p95.rx_mbps`, `vm.name`, …).
/// `latency_source` keeps its old values (`counter`, `none`).
fn flat_csv(rows: &Value) -> String {
    fn flatten(prefix: &str, v: &Value, out: &mut BTreeMap<String, String>) {
        match v {
            Value::Object(m) => {
                for (k, x) in m {
                    flatten(
                        &if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        },
                        x,
                        out,
                    );
                }
            }
            Value::Null => {}
            Value::String(s) if prefix == "latency_source" => {
                out.insert(
                    prefix.to_string(),
                    s.trim_start_matches("LATENCY_SOURCE_").to_ascii_lowercase(),
                );
            }
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
    let mut out = cols
        .iter()
        .map(|c| csv_field(c))
        .collect::<Vec<_>>()
        .join(",");
    out.push('\n');
    for m in &flat {
        out.push_str(
            &cols
                .iter()
                .map(|c| m.get(*c).map(|v| csv_field(v)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    out
}

/// `GET /usage/bandwidth` (§9.3).
pub async fn bandwidth(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let req = pb::GetBandwidthReportRequest {
        projects: list(&p.project),
        query: report_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::bandwidth_report(&c, req).await?;
    Ok(report(&c, &p, r))
}

/// `GET /usage/disk-io`.
pub async fn disk_io(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let req = pb::GetDiskIoReportRequest {
        projects: list(&p.project),
        query: report_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::disk_io_report(&c, req).await?;
    Ok(report(&c, &p, r))
}

/// `GET /usage/compute`.
pub async fn compute(c: Caller, Query(p): Query<RateParams>) -> Result<Response, ApiErr> {
    let req = pb::GetComputeReportRequest {
        projects: list(&p.project),
        query: report_query(&p)?.into(),
        ..Default::default()
    };
    let r = svc::compute_report(&c, req).await?;
    Ok(report(&c, &p, r))
}

/// `GET /vms/{id}/bandwidth`: the 5-minute series, and `p95` with `?p95=true`.
pub async fn vm_bandwidth(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<RateParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetVmBandwidthRequest {
        id,
        query: series_query(&p)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::vm_bandwidth(&c, req).await?).into_response())
}

/// `GET /networks/{id}/bandwidth`.
pub async fn network_bandwidth(
    c: Caller,
    Path(name): Path<String>,
    Query(p): Query<RateParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetNetworkBandwidthRequest {
        name,
        query: series_query(&p)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::network_bandwidth(&c, req).await?).into_response())
}

/// `GET /vms/{id}/io`: the VM's disks summed, and each separately.
pub async fn vm_io(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<RateParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetVmIoRequest {
        id,
        query: series_query(&p)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::vm_io(&c, req).await?).into_response())
}

/// `GET /vms/{id}/compute`.
pub async fn vm_compute(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<RateParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetVmComputeRequest {
        id,
        query: series_query(&p)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::vm_compute(&c, req).await?).into_response())
}

/// `GET /disks/{id}/io`.
pub async fn disk_io_series(
    c: Caller,
    Path(id): Path<String>,
    Query(p): Query<RateParams>,
) -> Result<Response, ApiErr> {
    let req = pb::GetDiskIoRequest {
        id,
        query: series_query(&p)?.into(),
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::disk_io(&c, req).await?).into_response())
}

/// `GET /vms/{id}/stats` (§9.4).
pub async fn vm_stats(c: Caller, Path(id): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::GetVmStatsRequest {
        id,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::vm_stats(&c, req).await?).into_response())
}

/// What a node answers the server relaying `/vms/{id}/stats` (§8.4): the
/// `GetVmStatsResponse` as JSON.
pub(crate) fn live_vm_stats(meter: &crate::metering::Meter, vm_id: &str) -> Value {
    serde_json::to_value(svc::live_vm_stats(meter, vm_id)).unwrap_or_default()
}

/// `GET /networks/{id}/stats` (§9.4).
pub async fn network_stats(c: Caller, Path(name): Path<String>) -> Result<Response, ApiErr> {
    let req = pb::GetNetworkStatsRequest {
        name,
        ..Default::default()
    };
    Ok(Proto(StatusCode::OK, svc::network_stats(&c, req).await?).into_response())
}
