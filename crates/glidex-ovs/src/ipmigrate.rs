//! Moving a NIC's IP configuration onto its bridge, with verification and
//! exact rollback (spec §8.5).

use crate::exec::{Cmd, Exec, Program};
use crate::nic::global_addresses;
use crate::OvsError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// `default` or CIDR.
    pub dst: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<u32>,
}

/// What the NIC had before migration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ifname: String,
    pub mac: Option<String>,
    /// `addr/prefix`, IPv4 and IPv6 global.
    pub addresses: Vec<String>,
    /// IPv4 routes via the NIC, except the kernel's own prefix routes.
    pub routes: Vec<Route>,
}

impl Snapshot {
    pub fn default_gateway(&self) -> Option<&str> {
        self.routes
            .iter()
            .find(|r| r.dst == "default")
            .and_then(|r| r.gateway.as_deref())
    }
}

pub fn snapshot(exec: &dyn Exec, ifname: &str) -> Result<Snapshot, OvsError> {
    let addresses = global_addresses(exec, ifname)?
        .into_iter()
        .map(|(ip, p)| format!("{}/{}", ip, p))
        .collect();
    let out = exec.check(&Cmd::new(Program::Ip, ["-4", "-j", "route", "show", "dev", ifname]))?;
    let routes: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let routes = routes
        .iter()
        // Prefix routes appear by themselves when the address is added.
        .filter(|r| r.get("protocol").and_then(Value::as_str) != Some("kernel"))
        .filter_map(|r| {
            Some(Route {
                dst: r.get("dst")?.as_str()?.to_string(),
                gateway: r.get("gateway").and_then(Value::as_str).map(str::to_string),
                metric: r.get("metric").and_then(Value::as_u64).map(|m| m as u32),
            })
        })
        .collect();
    let mac = exec
        .read_file(&std::path::Path::new("/sys/class/net").join(ifname).join("address"))
        .ok()
        .map(|s| s.trim().to_string());
    Ok(Snapshot {
        ifname: ifname.to_string(),
        mac,
        addresses,
        routes,
    })
}

fn route_args(r: &Route, dev: &str) -> Vec<String> {
    let mut args = vec!["-4".to_string(), "route".into(), "replace".into(), r.dst.clone()];
    if let Some(gw) = &r.gateway {
        args.extend(["via".to_string(), gw.clone()]);
    }
    args.extend(["dev".to_string(), dev.to_string()]);
    if let Some(m) = r.metric {
        args.extend(["metric".to_string(), m.to_string()]);
    }
    args
}

fn ip(exec: &dyn Exec, args: Vec<String>) -> Result<(), OvsError> {
    exec.check(&Cmd::new(Program::Ip, args)).map(|_| ())
}

/// Move addresses and routes from `snap.ifname` to `bridge`. The port must
/// already be on the bridge. Best-effort for routes; addresses must move.
pub fn apply(exec: &dyn Exec, snap: &Snapshot, bridge: &str) -> Result<(), OvsError> {
    // Keep the NIC's MAC on the bridge, so neighbours' ARP caches and any
    // DHCP lease keyed on the MAC stay valid.
    if let Some(mac) = &snap.mac {
        crate::vsctl::run(
            exec,
            vec!["set".into(), "Bridge".into(), bridge.into(), format!("other-config:hwaddr={}", mac)],
        )?;
    }
    for a in &snap.addresses {
        // May already be gone (re-applying after a reboot, or a retry).
        let _ = exec.run(&Cmd::new(Program::Ip, ["addr", "del", a.as_str(), "dev", snap.ifname.as_str()]));
    }
    ip(exec, vec!["link".into(), "set".into(), bridge.into(), "up".into()])?;
    for a in &snap.addresses {
        ip(exec, vec!["addr".into(), "replace".into(), a.clone(), "dev".into(), bridge.into()])?;
    }
    for r in &snap.routes {
        ip(exec, route_args(r, bridge))?;
    }
    Ok(())
}

/// Put everything back on the NIC. Keeps going after errors so as much as
/// possible is restored; returns the first error.
pub fn restore(exec: &dyn Exec, snap: &Snapshot, bridge: &str) -> Result<(), OvsError> {
    let mut first_err = None;
    let mut note = |r: Result<(), OvsError>| {
        if let Err(e) = r {
            first_err.get_or_insert(e);
        }
    };
    for a in &snap.addresses {
        // May already be gone from the bridge.
        let _ = exec.run(&Cmd::new(Program::Ip, ["addr", "del", a.as_str(), "dev", bridge]));
    }
    for a in &snap.addresses {
        note(ip(exec, vec!["addr".into(), "replace".into(), a.clone(), "dev".into(), snap.ifname.clone()]));
    }
    note(ip(exec, vec!["link".into(), "set".into(), snap.ifname.clone(), "up".into()]));
    for r in &snap.routes {
        note(ip(exec, route_args(r, &snap.ifname)));
    }
    let _ = crate::vsctl::run(
        exec,
        vec!["remove".into(), "Bridge".into(), bridge.into(), "other-config".into(), "hwaddr".into()],
    );
    first_err.map_or(Ok(()), Err)
}

/// Is `gateway` reachable at layer 2 through `bridge` within `timeout`?
/// Pings to trigger neighbour resolution, then checks the neighbour table,
/// so it works even when ICMP is filtered.
pub fn gateway_reachable(exec: &dyn Exec, gateway: &str, bridge: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let _ = exec.run(
            &Cmd::new(Program::Ping, ["-c", "1", "-W", "1", "-I", bridge, gateway]).timeout(Duration::from_secs(3)),
        );
        if let Ok(out) = exec.check(&Cmd::new(Program::Ip, ["-j", "neigh", "show", gateway, "dev", bridge])) {
            let entries: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
            let resolved = entries.iter().any(|e| {
                e.get("lladdr").is_some()
                    && e.get("state")
                        .and_then(Value::as_array)
                        .is_some_and(|s| s.iter().any(|x| matches!(x.as_str(), Some("REACHABLE" | "STALE" | "DELAY" | "PROBE" | "PERMANENT"))))
            });
            if resolved {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    fn exec_with_nic() -> RecordingExec {
        let exec = RecordingExec::new();
        exec.on("ip -j addr show dev gxup0", Output::ok(r#"[{"addr_info":[{"family":"inet","local":"192.0.2.10","prefixlen":24,"scope":"global"},{"family":"inet6","local":"fe80::1","prefixlen":64,"scope":"link"}]}]"#));
        exec.on("ip -4 -j route show dev gxup0", Output::ok(r#"[{"dst":"default","gateway":"192.0.2.1","protocol":"static","metric":100},{"dst":"192.0.2.0/24","protocol":"kernel","scope":"link"},{"dst":"198.51.100.0/24","gateway":"192.0.2.254","protocol":"static"}]"#));
        exec.file("/sys/class/net/gxup0/address", "52:54:00:12:34:56\n");
        exec
    }

    #[test]
    fn snapshot_skips_link_local_and_kernel_routes() {
        let snap = snapshot(&exec_with_nic(), "gxup0").unwrap();
        assert_eq!(snap.addresses, ["192.0.2.10/24"]);
        assert_eq!(snap.routes.len(), 2);
        assert_eq!(snap.default_gateway(), Some("192.0.2.1"));
        assert_eq!(snap.mac.as_deref(), Some("52:54:00:12:34:56"));
    }

    #[test]
    fn apply_then_restore_commands() {
        let exec = exec_with_nic();
        let snap = snapshot(&exec, "gxup0").unwrap();
        exec.clear_calls();
        apply(&exec, &snap, "gxbr-up").unwrap();
        assert_eq!(
            exec.calls(),
            [
                "ovs-vsctl set Bridge gxbr-up other-config:hwaddr=52:54:00:12:34:56",
                "ip addr del 192.0.2.10/24 dev gxup0",
                "ip link set gxbr-up up",
                "ip addr replace 192.0.2.10/24 dev gxbr-up",
                "ip -4 route replace default via 192.0.2.1 dev gxbr-up metric 100",
                "ip -4 route replace 198.51.100.0/24 via 192.0.2.254 dev gxbr-up",
            ]
        );
        exec.clear_calls();
        restore(&exec, &snap, "gxbr-up").unwrap();
        assert_eq!(
            exec.calls(),
            [
                "ip addr del 192.0.2.10/24 dev gxbr-up",
                "ip addr replace 192.0.2.10/24 dev gxup0",
                "ip link set gxup0 up",
                "ip -4 route replace default via 192.0.2.1 dev gxup0 metric 100",
                "ip -4 route replace 198.51.100.0/24 via 192.0.2.254 dev gxup0",
                "ovs-vsctl remove Bridge gxbr-up other-config hwaddr",
            ]
        );
    }

    #[test]
    fn restore_continues_after_errors() {
        let exec = exec_with_nic();
        let snap = snapshot(&exec, "gxup0").unwrap();
        exec.on("ip addr replace 192.0.2.10/24 dev gxup0", Output::failed(2, "RTNETLINK answers: File exists"));
        assert!(restore(&exec, &snap, "gxbr-up").is_err());
        assert!(exec.calls().iter().any(|c| c.contains("route replace default via 192.0.2.1 dev gxup0")), "routes still restored");
    }

    #[test]
    fn gateway_check_uses_neighbour_state() {
        let exec = RecordingExec::new();
        exec.on("ip -j neigh show 192.0.2.1 dev gxbr-up", Output::ok(r#"[{"dst":"192.0.2.1","lladdr":"aa:bb:cc:dd:ee:ff","state":["REACHABLE"]}]"#));
        assert!(gateway_reachable(&exec, "192.0.2.1", "gxbr-up", Duration::from_secs(1)));

        let exec = RecordingExec::new();
        exec.on("ip -j neigh show 192.0.2.1 dev gxbr-up", Output::ok(r#"[{"dst":"192.0.2.1","state":["FAILED"]}]"#));
        assert!(!gateway_reachable(&exec, "192.0.2.1", "gxbr-up", Duration::from_millis(500)));
    }
}
