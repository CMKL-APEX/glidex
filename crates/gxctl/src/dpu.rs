//! DPUs (spec/31-dpu.md §12.4): `dpu status [--node N]`, what each node's
//! DPU reports and what its netd probes now.

use crate::client::ApiClient;
use crate::flag_value;
use crate::tenancy::{err, usage};
use colored::Colorize;
use hyper::Method;
use serde_json::Value;

/// `dpu status [--node N]`: one node's DPUs, or every node's that has one.
pub async fn dpu(client: &ApiClient, args: &[&str]) {
    match args.first().copied().unwrap_or("status") {
        "status" => status(client, flag_value(args, "--node")).await,
        _ => usage("Usage: dpu status [--node N]"),
    }
}

async fn status(client: &ApiClient, node: Option<&str>) {
    let nodes: Vec<(String, String)> = match node {
        Some(n) => vec![(n.to_string(), n.to_string())],
        None => match client
            .request_json::<Value>(Method::GET, "/nodes", None)
            .await
        {
            Ok(v) => listed_nodes(&v),
            Err(e) => return err(e),
        },
    };
    let mut shown = 0;
    for (id, name) in &nodes {
        let v: Value = match client
            .request_json(Method::GET, &format!("/nodes/{id}/dpus"), None)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                err(format!("node {name}: {e}"));
                continue;
            }
        };
        for d in v["dpus"].as_array().into_iter().flatten() {
            shown += 1;
            for l in lines(name, d) {
                println!("{l}");
            }
        }
    }
    if shown == 0 {
        println!("{}", "No DPUs reported".yellow());
    }
}

/// Each node's id and name from `GET /nodes`: a `Node` keeps both under
/// `meta`.
fn listed_nodes(v: &Value) -> Vec<(String, String)> {
    v["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let id = n["meta"]["id"].as_str()?;
            let name = n["meta"]["name"].as_str().filter(|s| !s.is_empty());
            Some((id.to_string(), name.unwrap_or(id).to_string()))
        })
        .collect()
}

/// One DPU's block: its reported state, VFs, chassis, and the probe's gaps.
fn lines(node: &str, d: &Value) -> Vec<String> {
    let s = &d["status"];
    let word = |v: &Value| v.as_str().unwrap_or("-").to_string();
    let mut out = vec![
        format!(
            "{} {} on node {}",
            "DPU".cyan(),
            word(&s["id"]).yellow(),
            node
        ),
        format!(
            "  State:      {}{}",
            word(&s["state"]),
            if d["healthy"].as_bool() == Some(true) {
                " (healthy)".green().to_string()
            } else {
                String::new()
            }
        ),
        format!(
            "  VFs:        {} free of {}",
            s["vfs_free"].as_u64().unwrap_or(0),
            s["vfs_total"].as_u64().unwrap_or(0)
        ),
        format!(
            "  Chassis:    {}{}",
            word(&s["chassis"]),
            if s["sb_connected"].as_bool() == Some(true) {
                ", connected"
            } else {
                ""
            }
        ),
    ];
    if let Some(v) = s["ovn_version"].as_str() {
        out.push(format!("  OVN:        {v}"));
    }
    let missing: Vec<String> = d["probe"]["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(String::from))
        .collect();
    if !missing.is_empty() {
        out.push(format!("  Missing:    {}", missing.join(", ").red()));
    }
    if let Some(e) = d["probe_error"].as_str() {
        out.push(format!("  Probe:      {}", e.red()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (_, '\u{1b}') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn nodes_are_read_from_their_meta() {
        let v = serde_json::json!({ "nodes": [
            { "meta": { "id": "n1", "name": "node-a" }, "status": {} },
            { "meta": { "id": "n2", "name": "" } },
            { "status": {} },
        ]});
        assert_eq!(
            listed_nodes(&v),
            vec![
                ("n1".to_string(), "node-a".to_string()),
                ("n2".to_string(), "n2".to_string()),
            ]
        );
    }

    #[test]
    fn a_dpu_block_says_state_vfs_chassis_and_gaps() {
        let d = serde_json::json!({
            "status": { "id": "dpu0", "state": "prepared", "vfs_total": 16, "vfs_free": 15,
                        "chassis": "node:n1-dpu", "sb_connected": true, "ovn_version": "24.03.8" },
            "healthy": true,
            "probe": { "missing": ["link_address"] },
        });
        let l: Vec<String> = lines("n1", &d).iter().map(|l| plain(l)).collect();
        assert_eq!(l[0], "DPU dpu0 on node n1");
        assert_eq!(l[1], "  State:      prepared (healthy)");
        assert_eq!(l[2], "  VFs:        15 free of 16");
        assert_eq!(l[3], "  Chassis:    node:n1-dpu, connected");
        assert_eq!(l[4], "  OVN:        24.03.8");
        assert_eq!(l[5], "  Missing:    link_address");
    }
}
