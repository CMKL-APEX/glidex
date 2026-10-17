//! Read-only views of the OVN databases for the web UI
//! (spec/clustering-ui.md §8): who serves the databases and which of them
//! leads, the chassis, and one cluster network's ports and router. Nothing
//! here writes.

use crate::NbConn;
use glidex_ovs::exec::{Cmd, Exec, Program};
use glidex_ovs::vsctl::{self, Row};
use glidex_ovs::OvsError;
use serde::Serialize;
use std::collections::BTreeMap;

/// How long one probe of one database server may take.
const PROBE_SECS: u32 = 3;

/// `ovn-nbctl` / `ovn-sbctl` against `dbs`, with the TLS files of `conn`.
fn ctl(program: Program, conn: &NbConn, dbs: &[String], leader_only: bool, args: &[&str]) -> Cmd {
    let mut all = vec![
        format!("--db={}", dbs.join(",")),
        "-p".to_string(),
        conn.key.display().to_string(),
        "-c".into(),
        conn.cert.display().to_string(),
        "-C".into(),
        conn.ca.display().to_string(),
        format!("--timeout={PROBE_SECS}"),
    ];
    if !leader_only {
        all.push("--no-leader-only".into());
    }
    all.extend(args.iter().map(|a| a.to_string()));
    Cmd::new(program, all)
}

/// The ids in a reference column: `["uuid", id]` or `["set", [["uuid", id], …]]`.
fn refs(r: &Row, col: &str) -> Vec<String> {
    use serde_json::Value;
    fn one(v: &Value) -> Option<String> {
        match v {
            Value::Array(a) if a.first() == Some(&Value::String("uuid".into())) => {
                a.get(1)?.as_str().map(String::from)
            }
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }
    match r.0.get(col) {
        Some(Value::Array(a)) if a.first() == Some(&Value::String("set".into())) => a
            .get(1)
            .and_then(Value::as_array)
            .map(|xs| xs.iter().filter_map(one).collect())
            .unwrap_or_default(),
        Some(v) => one(v).into_iter().collect(),
        None => Vec::new(),
    }
}

/// A row's own id.
fn id(r: &Row) -> Option<String> {
    refs(r, "_uuid").into_iter().next()
}

fn list(
    exec: &dyn Exec,
    program: Program,
    conn: &NbConn,
    dbs: &[String],
    table: &str,
    columns: &[&str],
) -> Result<Vec<Row>, OvsError> {
    let cols = format!("--columns={}", columns.join(","));
    let out = exec
        .check(&ctl(
            program,
            conn,
            dbs,
            false,
            &["--format=json", &cols, "list", table],
        ))?
        .stdout_str();
    vsctl::parse_table(&out)
}

/// One database server: whether it answers, and whether it leads its Raft group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Member {
    pub address: String,
    pub reachable: bool,
    pub leader: bool,
}

/// Probe each server of a clustered database (`ssl:ip:port` each): any
/// answer means it is up; an answer with leader-only on means it leads.
pub fn members(
    exec: &dyn Exec,
    program: Program,
    conn: &NbConn,
    servers: &[String],
) -> Vec<Member> {
    let global = if program == Program::OvnSbctl {
        "SB_Global"
    } else {
        "NB_Global"
    };
    std::thread::scope(|s| {
        let probes: Vec<_> = servers
            .iter()
            .map(|addr| {
                s.spawn(move || {
                    let one = std::slice::from_ref(addr);
                    let ok = |leader_only| {
                        exec.run(&ctl(
                            program,
                            conn,
                            one,
                            leader_only,
                            &["get", global, ".", "_uuid"],
                        ))
                        .is_ok_and(|o| o.status == 0)
                    };
                    let reachable = ok(false);
                    Member {
                        address: addr.clone(),
                        reachable,
                        leader: reachable && ok(true),
                    }
                })
            })
            .collect();
        probes
            .into_iter()
            .map(|p| {
                p.join().unwrap_or(Member {
                    address: String::new(),
                    reachable: false,
                    leader: false,
                })
            })
            .collect()
    })
}

/// A chassis as the southbound database knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChassisView {
    /// `node:<id>` for glidex nodes.
    pub name: String,
    pub hostname: String,
    /// Tunnel endpoints: `geneve 192.0.2.11`.
    pub encaps: Vec<String>,
    /// When its `ovn-controller` last caught up with the northbound database
    /// (ms); `None` until a client waits for the hypervisors (`nb_cfg`).
    pub nb_cfg_timestamp: Option<i64>,
}

pub fn chassis(
    exec: &dyn Exec,
    conn: &NbConn,
    sb: &[String],
) -> Result<Vec<ChassisView>, OvsError> {
    let encaps: BTreeMap<String, String> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Encap",
        &["_uuid", "type", "ip"],
    )?
    .into_iter()
    .filter_map(|r| {
        Some((
            id(&r)?,
            format!(
                "{} {}",
                r.str("type").unwrap_or_default(),
                r.str("ip").unwrap_or_default()
            ),
        ))
    })
    .collect();
    let private: BTreeMap<String, i64> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Chassis_Private",
        &["name", "nb_cfg_timestamp"],
    )?
    .into_iter()
    .filter_map(|r| {
        Some((
            r.str("name")?,
            r.int("nb_cfg_timestamp").filter(|t| *t > 0)?,
        ))
    })
    .collect();
    let mut out: Vec<ChassisView> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Chassis",
        &["name", "hostname", "encaps"],
    )?
    .into_iter()
    .filter_map(|r| {
        let name = r.str("name")?;
        Some(ChassisView {
            hostname: r.str("hostname").unwrap_or_default(),
            encaps: refs(&r, "encaps")
                .iter()
                .filter_map(|u| encaps.get(u).cloned())
                .collect(),
            nb_cfg_timestamp: private.get(&name).copied(),
            name,
        })
    })
    .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A router's gateway: which chassis carries its external port now
/// (`cr-<router>-ext`), and the HA group's chassis by priority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewayView {
    /// `gx-edge` or `gxr-<name>` (the HA group is named after it).
    pub router: String,
    /// The chassis carrying it now, if any.
    pub active: Option<String>,
    /// Chassis by priority, highest first.
    pub candidates: Vec<(String, i64)>,
}

pub fn gateways(
    exec: &dyn Exec,
    conn: &NbConn,
    nb: &[String],
    sb: &[String],
) -> Result<Vec<GatewayView>, OvsError> {
    let chassis_names: BTreeMap<String, String> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Chassis",
        &["_uuid", "name"],
    )?
    .into_iter()
    .filter_map(|r| Some((id(&r)?, r.str("name")?)))
    .collect();
    let active: BTreeMap<String, String> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Port_Binding",
        &["logical_port", "type", "chassis"],
    )?
    .into_iter()
    .filter(|r| r.str("type").as_deref() == Some("chassisredirect"))
    .filter_map(|r| {
        let router = r
            .str("logical_port")?
            .strip_prefix("cr-")?
            .strip_suffix("-ext")?
            .to_string();
        let ch = refs(&r, "chassis").into_iter().next()?;
        Some((router, chassis_names.get(&ch).cloned().unwrap_or(ch)))
    })
    .collect();
    let ha: BTreeMap<String, (String, i64)> = list(
        exec,
        Program::OvnNbctl,
        conn,
        nb,
        "HA_Chassis",
        &["_uuid", "chassis_name", "priority"],
    )?
    .into_iter()
    .filter_map(|r| {
        Some((
            id(&r)?,
            (r.str("chassis_name")?, r.int("priority").unwrap_or(0)),
        ))
    })
    .collect();
    let mut out: Vec<GatewayView> = list(
        exec,
        Program::OvnNbctl,
        conn,
        nb,
        "HA_Chassis_Group",
        &["name", "ha_chassis"],
    )?
    .into_iter()
    .filter_map(|r| {
        let router = r.str("name")?;
        let mut candidates: Vec<(String, i64)> = refs(&r, "ha_chassis")
            .iter()
            .filter_map(|u| ha.get(u).cloned())
            .collect();
        candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
        Some(GatewayView {
            active: active.get(&router).cloned(),
            router,
            candidates,
        })
    })
    .collect();
    out.sort_by(|a, b| a.router.cmp(&b.router));
    Ok(out)
}

/// A logical switch port and where it is bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PortView {
    pub name: String,
    pub addresses: Vec<String>,
    pub up: Option<bool>,
    /// The chassis the port is bound to (`node:<id>`).
    pub chassis: Option<String>,
    /// `router`, `localnet`, or empty for a VM port.
    pub kind: String,
}

/// The ports of logical switch `switch` (`gx-<network>`), with their bindings.
pub fn switch_ports(
    exec: &dyn Exec,
    conn: &NbConn,
    nb: &[String],
    sb: &[String],
    switch: &str,
) -> Result<Vec<PortView>, OvsError> {
    let sw = exec
        .check(&ctl(
            Program::OvnNbctl,
            conn,
            nb,
            false,
            &[
                "--format=json",
                "--columns=ports",
                "find",
                "Logical_Switch",
                &format!("name={switch}"),
            ],
        ))?
        .stdout_str();
    let port_ids: Vec<String> = vsctl::parse_table(&sw)?
        .first()
        .map(|r| refs(r, "ports"))
        .unwrap_or_default();
    let lsps: Vec<Row> = list(
        exec,
        Program::OvnNbctl,
        conn,
        nb,
        "Logical_Switch_Port",
        &["_uuid", "name", "addresses", "up", "type"],
    )?
    .into_iter()
    .filter(|r| id(r).is_some_and(|u| port_ids.contains(&u)))
    .collect();
    let chassis_names: BTreeMap<String, String> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Chassis",
        &["_uuid", "name"],
    )?
    .into_iter()
    .filter_map(|r| Some((id(&r)?, r.str("name")?)))
    .collect();
    let bound: BTreeMap<String, String> = list(
        exec,
        Program::OvnSbctl,
        conn,
        sb,
        "Port_Binding",
        &["logical_port", "chassis"],
    )?
    .into_iter()
    .filter_map(|r| {
        let ch = refs(&r, "chassis").into_iter().next()?;
        Some((
            r.str("logical_port")?,
            chassis_names.get(&ch).cloned().unwrap_or(ch),
        ))
    })
    .collect();
    let mut out: Vec<PortView> = lsps
        .into_iter()
        .filter_map(|r| {
            let name = r.str("name")?;
            Some(PortView {
                addresses: r.set("addresses"),
                up: r.bool("up"),
                chassis: bound.get(&name).cloned(),
                kind: r.str("type").unwrap_or_default(),
                name,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A router's NAT rules (`snat 203.0.113.5 10.89.1.0/24`).
pub fn router_nat(
    exec: &dyn Exec,
    conn: &NbConn,
    nb: &[String],
    router: &str,
) -> Result<Vec<String>, OvsError> {
    let lr = exec
        .check(&ctl(
            Program::OvnNbctl,
            conn,
            nb,
            false,
            &[
                "--format=json",
                "--columns=nat",
                "find",
                "Logical_Router",
                &format!("name={router}"),
            ],
        ))?
        .stdout_str();
    let nat_ids: Vec<String> = vsctl::parse_table(&lr)?
        .first()
        .map(|r| refs(r, "nat"))
        .unwrap_or_default();
    Ok(list(
        exec,
        Program::OvnNbctl,
        conn,
        nb,
        "NAT",
        &["_uuid", "type", "external_ip", "logical_ip"],
    )?
    .into_iter()
    .filter(|r| id(r).is_some_and(|u| nat_ids.contains(&u)))
    .map(|r| {
        format!(
            "{} {} {}",
            r.str("type").unwrap_or_default(),
            r.str("external_ip").unwrap_or_default(),
            r.str("logical_ip").unwrap_or_default()
        )
    })
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidex_ovs::exec::{Output, RecordingExec};

    fn conn() -> NbConn {
        NbConn {
            db: vec![],
            key: "/k".into(),
            cert: "/c".into(),
            ca: "/ca".into(),
            daemon: None,
        }
    }

    const SB: &str = "ovn-sbctl --db=ssl:192.0.2.11:6642 -p /k -c /c -C /ca --timeout=3 --no-leader-only --format=json";

    #[test]
    fn chassis_come_with_their_tunnel_endpoints_and_last_catch_up() {
        let exec = RecordingExec::new();
        exec.on(&format!("{SB} --columns=_uuid,type,ip list Encap"), Output::ok(r#"{"data":[[["uuid","e1"],"geneve","192.0.2.11"],[["uuid","e2"],"geneve","192.0.2.12"]],"headings":["_uuid","type","ip"]}"#));
        exec.on(
            &format!("{SB} --columns=name,nb_cfg_timestamp list Chassis_Private"),
            Output::ok(
                r#"{"data":[["node:a",1700000000000]],"headings":["name","nb_cfg_timestamp"]}"#,
            ),
        );
        exec.on(
            &format!("{SB} --columns=name,hostname,encaps list Chassis"),
            Output::ok(r#"{"data":[["node:b","h2",["uuid","e2"]],["node:a","h1",["set",[["uuid","e1"]]]]],"headings":["name","hostname","encaps"]}"#),
        );
        let c = chassis(&exec, &conn(), &["ssl:192.0.2.11:6642".into()]).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(
            c[0],
            ChassisView {
                name: "node:a".into(),
                hostname: "h1".into(),
                encaps: vec!["geneve 192.0.2.11".into()],
                nb_cfg_timestamp: Some(1700000000000)
            }
        );
        assert_eq!(c[1].encaps, ["geneve 192.0.2.12"]);
        assert_eq!(c[1].nb_cfg_timestamp, None);
        assert!(
            exec.calls()
                .iter()
                .all(|l| !l.contains(" set ") && !l.contains(" add ")),
            "read only"
        );
    }

    #[test]
    fn a_member_that_answers_with_leader_only_on_leads() {
        let exec = RecordingExec::new();
        let base = "ovn-nbctl --db=ssl:192.0.2.11:6641 -p /k -c /c -C /ca --timeout=3";
        exec.on(&format!("{base} --no-leader-only get"), Output::ok("x"));
        exec.on(&format!("{base} get"), Output::failed(1, "not the leader"));
        let lead = "ovn-nbctl --db=ssl:192.0.2.12:6641 -p /k -c /c -C /ca --timeout=3";
        exec.on(lead, Output::ok("x"));
        exec.on(
            "ovn-nbctl --db=ssl:192.0.2.13:6641",
            Output::failed(1, "timeout"),
        );
        let m = members(
            &exec,
            Program::OvnNbctl,
            &conn(),
            &[
                "ssl:192.0.2.11:6641".into(),
                "ssl:192.0.2.12:6641".into(),
                "ssl:192.0.2.13:6641".into(),
            ],
        );
        assert_eq!((m[0].reachable, m[0].leader), (true, false));
        assert_eq!((m[1].reachable, m[1].leader), (true, true));
        assert_eq!((m[2].reachable, m[2].leader), (false, false));
    }
}
