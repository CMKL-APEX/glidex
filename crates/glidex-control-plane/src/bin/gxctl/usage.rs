//! `usage`, `stats`, `bandwidth`, `io` and `compute` (spec/metering.md
//! §9, §12) on the generated `glidex.usage.v1` clients, over the Connect
//! protocol (spec/grpc.md §9.1). `--csv` exports stay REST: the CSV is
//! the REST route's own answer (and its audited export).

use crate::client::{self, ApiClient, RpcTransport};
use crate::flag_value;
use crate::tenancy::{err, rpc_err};
use buffa::Enumeration;
use buffa_types::google::protobuf::Timestamp;
use colored::Colorize;
use glidex_api::proto::glidex::usage::v1::*;
use hyper::Method;
use serde_json::Value;

async fn usage_rpc(client: &ApiClient) -> Result<UsageServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(UsageServiceClient::new(t.clone(), t.config()))
}

async fn metrics_rpc(client: &ApiClient) -> Result<MetricsServiceClient<RpcTransport>, String> {
    let t = client.rpc_transport().await.map_err(|e| e.message)?;
    Ok(MetricsServiceClient::new(t.clone(), t.config()))
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// `--from`/`--to`: unix seconds or RFC 3339 (`…T09:00Z`, `…T09:00:00+07:00`).
fn time(args: &[&str], flag: &str) -> Result<Option<Timestamp>, String> {
    let Some(v) = flag_value(args, flag) else {
        return Ok(None);
    };
    if let Ok(n) = v.parse::<i64>() {
        return Ok(Some(Timestamp::from_unix_secs(n)));
    }
    let parse = |t: &str| serde_json::from_value::<Timestamp>(Value::String(t.into())).ok();
    // RFC 3339 needs seconds; add them to `HH:MM`.
    let with_secs = || {
        let (date, rest) = v.split_once('T')?;
        let i = rest.find(['Z', '+', '-'])?;
        let (clock, zone) = rest.split_at(i);
        (clock.len() == 5).then(|| format!("{date}T{clock}:00{zone}"))
    };
    parse(v)
        .or_else(|| with_secs().and_then(|t| parse(&t)))
        .map(Some)
        .ok_or_else(|| format!("{v}: expected unix seconds or RFC 3339"))
}

fn words(args: &[&str], flag: &str) -> Vec<String> {
    flag_value(args, flag)
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

/// `--by project,vm,…`
fn group_by(args: &[&str]) -> Result<Vec<buffa::EnumValue<GroupKey>>, String> {
    words(args, "--by")
        .iter()
        .map(|w| {
            GroupKey::from_proto_name(&format!("GROUP_KEY_{}", w.to_ascii_uppercase()))
                .map(Into::into)
                .ok_or_else(|| {
                    format!("group_by: unknown key {w} (project, vm, disk, nic, network, node)")
                })
        })
        .collect()
}

/// A group key's field name in a row (`GROUP_KEY_VM` → `vm`).
fn key_field(k: &Value) -> Option<String> {
    Some(k.as_str()?.strip_prefix("GROUP_KEY_")?.to_ascii_lowercase())
}

fn projects(client: &ApiClient, args: &[&str]) -> Vec<String> {
    let p = words(args, "--project");
    if p.is_empty() {
        client.project().into_iter().collect()
    } else {
        p
    }
}

/// A `--csv` export, from the REST route.
async fn csv(client: &ApiClient, path: &str) {
    match client.request_bytes(Method::GET, path, None).await {
        Ok(r) => print!("{}", String::from_utf8_lossy(&r.body)),
        Err(e) => err(e.message),
    }
}

/// `GET /usage` path from `usage` arguments (the `--csv` export).
fn usage_path(args: &[&str], project: Option<String>) -> String {
    let mut path = "/usage".to_string();
    if let Some(p) = flag_value(args, "--project")
        .map(str::to_string)
        .or(project)
    {
        path = client::add_query(&path, "project", &p);
    }
    let granularity = flag_value(args, "--granularity").unwrap_or("month");
    path = client::add_query(&path, "granularity", granularity);
    for (flag, key) in [
        ("--from", "from"),
        ("--to", "to"),
        ("--by", "group_by"),
        ("--meters", "meters"),
        ("--tz", "tz"),
    ] {
        if let Some(v) = flag_value(args, flag) {
            path = client::add_query(&path, key, v);
        }
    }
    if args.contains(&"--csv") {
        path = client::add_query(&path, "format", "csv");
    }
    path
}

fn usage_query(args: &[&str]) -> Result<UsageQuery, String> {
    let g = flag_value(args, "--granularity").unwrap_or("month");
    let granularity =
        Granularity::from_proto_name(&format!("GRANULARITY_{}", g.to_ascii_uppercase()))
            .ok_or_else(|| format!("granularity: unknown value {g} (hour, day, month)"))?;
    Ok(UsageQuery {
        from: time(args, "--from")?.into(),
        to: time(args, "--to")?.into(),
        granularity: granularity.into(),
        group_by: group_by(args)?,
        meters: words(args, "--meters"),
        tz: flag_value(args, "--tz").unwrap_or_default().to_string(),
        ..Default::default()
    })
}

/// `usage [--from D] [--to D] [--by project|vm|disk|nic|network] [--granularity hour|day|month] [--meters m,…] [--tz Z] [--csv]`
pub async fn resource_usage(client: &ApiClient, args: &[&str]) {
    match args.first() {
        Some(&"bandwidth") => return rate_report(client, "bandwidth", &args[1..]).await,
        Some(&"disk-io") => return rate_report(client, "disk-io", &args[1..]).await,
        Some(&"compute") => return rate_report(client, "compute", &args[1..]).await,
        _ => {}
    }
    if args.contains(&"--csv") {
        return csv(client, &usage_path(args, client.project())).await;
    }
    let query = match usage_query(args) {
        Ok(q) => q,
        Err(e) => return err(e),
    };
    let svc = match usage_rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let r = match svc
        .get_usage(GetUsageRequest {
            projects: projects(client, args),
            query: query.into(),
            ..Default::default()
        })
        .await
    {
        Ok(r) => r.into_owned(),
        Err(e) => return err(rpc_err(e)),
    };
    let v = serde_json::to_value(&r).unwrap_or_default();
    println!(
        "{} {} → {} ({}), complete through {}",
        "Usage".bold(),
        s(&v["from"]),
        s(&v["to"]),
        s(&v["timezone"]),
        s(&v["complete_through"])
    );
    let rows = v["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  No usage recorded.");
        return;
    }
    let keys: Vec<String> = v["group_by"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(key_field)
        .collect();
    for r in rows {
        let who: Vec<String> = keys
            .iter()
            .filter_map(|k| {
                let n = &r[k];
                n.is_object()
                    .then(|| format!("{}={}", k, n["name"].as_str().unwrap_or("?")))
            })
            .collect();
        let flags: Vec<&str> = r["flags"]
            .as_array()
            .map(|f| f.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        println!(
            "\n  {} {} {}",
            s(&r["start"]).dimmed(),
            who.join(" ").cyan(),
            if flags.is_empty() {
                String::new()
            } else {
                format!("[{}]", flags.join(", ")).yellow().to_string()
            }
        );
        if let Some(meters) = r["meters"].as_object() {
            let mut meters: Vec<_> = meters.iter().collect();
            meters.sort_by(|a, b| a.0.cmp(b.0));
            for (m, x) in meters {
                println!(
                    "    {:<26} {:>16} {}",
                    m,
                    format!("{}", x["value"]),
                    s(&x["unit"])
                );
            }
        }
    }
}

/// `usage bandwidth|disk-io|compute [--month YYYY-MM] [--by k,…] [--project P] [--csv]` (§9.3).
async fn rate_report(client: &ApiClient, what: &str, args: &[&str]) {
    if args.contains(&"--csv") {
        let mut path = format!("/usage/{what}");
        if let Some(p) = flag_value(args, "--project")
            .map(str::to_string)
            .or_else(|| client.project())
        {
            path = client::add_query(&path, "project", &p);
        }
        for (flag, key) in [
            ("--month", "month"),
            ("--by", "group_by"),
            ("--from", "from"),
            ("--to", "to"),
        ] {
            if let Some(v) = flag_value(args, flag) {
                path = client::add_query(&path, key, v);
            }
        }
        return csv(client, &client::add_query(&path, "format", "csv")).await;
    }
    let query = match (|| -> Result<ReportQuery, String> {
        Ok(ReportQuery {
            month: flag_value(args, "--month").unwrap_or_default().to_string(),
            from: time(args, "--from")?.into(),
            to: time(args, "--to")?.into(),
            group_by: group_by(args)?,
            ..Default::default()
        })
    })() {
        Ok(q) => q.into(),
        Err(e) => return err(e),
    };
    let svc = match usage_rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let projects = projects(client, args);
    let v = match what {
        "disk-io" => svc
            .get_disk_io_report(GetDiskIoReportRequest {
                projects,
                query,
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned())),
        "compute" => svc
            .get_compute_report(GetComputeReportRequest {
                projects,
                query,
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned())),
        _ => svc
            .get_bandwidth_report(GetBandwidthReportRequest {
                projects,
                query,
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned())),
    };
    let v = match v {
        Ok(v) => v.unwrap_or_default(),
        Err(e) => return err(rpc_err(e)),
    };
    let period = v["month"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} → {}", s(&v["from"]), s(&v["to"])));
    println!(
        "{} {} ({}){}",
        match what {
            "disk-io" => "Disk I/O",
            "compute" => "CPU and memory",
            _ => "Bandwidth",
        }
        .bold(),
        period,
        s(&v["timezone"]),
        if v["final"].as_bool() == Some(true) {
            ""
        } else {
            ", month to date"
        }
    );
    let rows = v["rows"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("  Nothing recorded.");
        return;
    }
    let keys: Vec<String> = v["group_by"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(key_field)
        .collect();
    for r in rows {
        let who: Vec<String> = keys
            .iter()
            .filter(|k| r[k.as_str()].is_object())
            .map(|k| format!("{}={}", k, r[k.as_str()]["name"].as_str().unwrap_or("?")))
            .collect();
        let counted = r["slots"]["counted"]
            .as_u64()
            .or_else(|| r["p95"]["slots"]["counted"].as_u64())
            .unwrap_or(0);
        println!(
            "\n  {}  {}",
            who.join(" ").cyan(),
            format!("({counted} slots of 5 min)").dimmed()
        );
        for part in ["avg", "peak", "p95"] {
            if let Some(o) = r[part].as_object() {
                let vals: Vec<String> = o
                    .iter()
                    .filter(|(k, v)| !v.is_null() && *k != "resolution_secs" && *k != "slots")
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                let label = match part {
                    "peak" => "30-second peak",
                    "p95" => "95th percentile",
                    _ => "average",
                };
                println!("    {:<16} {}", label, vals.join("  "));
            }
        }
        if r["latency_source"] == "LATENCY_SOURCE_NONE" {
            println!(
                "    {}",
                "latency not available (Cloud Hypervisor)".dimmed()
            );
        }
    }
}

fn values_line<'a>(label: &str, values: impl IntoIterator<Item = (&'a String, &'a f64)>) {
    let mut vals: Vec<_> = values.into_iter().collect();
    vals.sort_by(|a, b| a.0.cmp(b.0));
    let vals: Vec<String> = vals
        .iter()
        .map(|(k, x)| format!("{k}={}", Value::from(**x)))
        .collect();
    println!("  {:<24} {}", label, vals.join("  "));
}

/// `stats <vm>`: the latest rates (§9.4).
pub async fn stats(client: &ApiClient, args: &[&str]) {
    let Some(vm) = args.first() else {
        return err("Usage: stats <vm>");
    };
    let id = match client.resolve_vm(vm).await {
        Ok(id) => id,
        Err(e) => return err(e),
    };
    let svc = match metrics_rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let r = match svc
        .get_vm_stats(GetVmStatsRequest {
            id,
            ..Default::default()
        })
        .await
    {
        Ok(r) => r.into_owned(),
        Err(e) => return err(rpc_err(e)),
    };
    let Some(at) = r.sampled_at.as_option() else {
        println!("No recent sample (is the VM running?).");
        return;
    };
    let at = serde_json::to_value(at).unwrap_or_default();
    println!(
        "{} sampled at {} (over {} s)",
        vm.bold(),
        s(&at),
        r.resolution_secs.unwrap_or(0)
    );
    if !r.vm.is_empty() {
        values_line("vm", &r.vm);
    }
    for x in r.nics.iter().chain(&r.disks) {
        values_line(if x.name.is_empty() { "?" } else { &x.name }, &x.values);
    }
}

/// `bandwidth <vm> | --network <net>`, `io <vm> | --disk <disk>` and
/// `compute <vm>`: the 5-minute series with its p95 (§9.3).
pub async fn series(client: &ApiClient, what: &str, args: &[&str]) {
    enum Of {
        Vm(String),
        Network(String),
        Disk(String),
    }
    let of = match (
        what,
        flag_value(args, "--network"),
        flag_value(args, "--disk"),
        args.first(),
    ) {
        ("bandwidth", Some(n), _, _) => Of::Network(n.to_string()),
        ("io", _, Some(d), _) => Of::Disk(d.to_string()),
        (_, None, None, Some(vm)) if !vm.starts_with("--") => match client.resolve_vm(vm).await {
            Ok(id) => Of::Vm(id),
            Err(e) => return err(e),
        },
        _ => {
            let alt = match what {
                "io" => " | --disk <disk>",
                "bandwidth" => " | --network <net>",
                _ => "",
            };
            return err(format!("Usage: {what} <vm>{alt} [--from D] [--to D]"));
        }
    };
    let query = match (|| -> Result<SeriesQuery, String> {
        Ok(SeriesQuery {
            from: time(args, "--from")?.into(),
            to: time(args, "--to")?.into(),
            p95: Some(true),
            ..Default::default()
        })
    })() {
        Ok(q) => q,
        Err(e) => return err(e),
    };
    let svc = match metrics_rpc(client).await {
        Ok(s) => s,
        Err(e) => return err(e),
    };
    let json = |r: Result<Value, connectrpc::ConnectError>| r.map_err(rpc_err);
    let v = match (what, of) {
        ("bandwidth", Of::Network(name)) => json(
            svc.get_network_bandwidth(GetNetworkBandwidthRequest {
                name,
                query: query.into(),
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned()).unwrap_or_default()),
        ),
        ("bandwidth", Of::Vm(id)) => json(
            svc.get_vm_bandwidth(GetVmBandwidthRequest {
                id,
                query: query.into(),
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned()).unwrap_or_default()),
        ),
        ("io", Of::Disk(id)) => json(
            svc.get_disk_io(GetDiskIoRequest {
                id,
                query: query.into(),
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned()).unwrap_or_default()),
        ),
        ("io", Of::Vm(id)) => json(
            svc.get_vm_io(GetVmIoRequest {
                id,
                query: query.into(),
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned()).unwrap_or_default()),
        ),
        (_, Of::Vm(id)) => json(
            svc.get_vm_compute(GetVmComputeRequest {
                id,
                query: query.into(),
                ..Default::default()
            })
            .await
            .map(|r| serde_json::to_value(r.into_owned()).unwrap_or_default()),
        ),
        _ => return err(format!("Usage: {what} <vm>")),
    };
    let v = match v {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let points = v["points"].as_array().cloned().unwrap_or_default();
    println!(
        "{} → {}  ({} points of 5 min)",
        s(&v["from"]),
        s(&v["to"]),
        points.len()
    );
    for p in &points {
        let vals: Vec<String> = p
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, _)| *k != "slot")
            .map(|(k, x)| format!("{k}={x}"))
            .collect();
        println!("  {}  {}", s(&p["slot"]).dimmed(), vals.join("  "));
    }
    if let Some(o) = v["p95"].as_object() {
        let vals: Vec<String> = o
            .iter()
            .filter(|(k, x)| !x.is_null() && *k != "slots")
            .map(|(k, x)| format!("{k}={x}"))
            .collect();
        println!("  {} {}", "95th percentile:".bold(), vals.join("  "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_keys() {
        assert_eq!(
            time(&["--from", "1791158400"], "--from").unwrap(),
            Some(Timestamp::from_unix_secs(1_791_158_400))
        );
        assert_eq!(
            time(&["--from", "2026-10-05T07:00+07:00"], "--from").unwrap(),
            Some(Timestamp::from_unix_secs(1_791_158_400))
        );
        assert_eq!(
            time(&["--to", "2026-10-05T00:00:00Z"], "--to").unwrap(),
            Some(Timestamp::from_unix_secs(1_791_158_400))
        );
        assert!(time(&["--to", "yesterday"], "--to").is_err());
        assert_eq!(time(&[], "--to").unwrap(), None);
        assert_eq!(
            group_by(&["--by", "project,vm"]).unwrap(),
            vec![
                buffa::EnumValue::from(GroupKey::GROUP_KEY_PROJECT),
                buffa::EnumValue::from(GroupKey::GROUP_KEY_VM)
            ]
        );
        assert!(group_by(&["--by", "vms"]).is_err());
        assert_eq!(
            key_field(&Value::from("GROUP_KEY_NIC")).as_deref(),
            Some("nic")
        );
    }

    #[test]
    fn usage_query_from_flags() {
        let p = usage_path(
            &["--by", "vm,nic", "--from", "2026-10-01T00:00:00Z", "--csv"],
            Some("pa".into()),
        );
        assert!(p.starts_with("/usage?project=pa&granularity=month&from=2026-10-01T00%3A00%3A00Z&group_by=vm%2Cnic"), "{p}");
        assert!(p.ends_with("&format=csv"), "{p}");
        assert_eq!(
            usage_path(&["--granularity", "day"], None),
            "/usage?granularity=day"
        );
    }
}
