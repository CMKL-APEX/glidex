//! Joining the host to OVN (spec/clustering.md §11.3): the chassis settings
//! in `Open_vSwitch`, `br-int`, the certificates and `ovn-controller`. The
//! logical network (switches, routers, ports) is the control plane's, written
//! to the OVN northbound database; netd only makes this host a chassis and
//! plugs VM ports into `br-int`.

use crate::bridge::Datapath;
use crate::exec::{Cmd, Exec, Program};
use crate::names::{validate_name, MAX_IFNAME};
use crate::vsctl::{self, owned_tags, tag_args, OWNER_KEY, OWNER_VALUE, ROLE_KEY};
use crate::OvsError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The integration bridge OVN owns the flows of.
pub const BR_INT: &str = "br-int";
/// Where a chassis keeps its OVN certificates (root, 0600).
pub const CERT_DIR: &str = "/etc/glidex/ovn";
/// Marks the `Open_vSwitch` settings glidex wrote.
pub const CHASSIS_TAG: &str = "glidex-ovn";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisSpec {
    /// The node id: the chassis name (`system-id`).
    pub chassis: String,
    /// `ssl:<ip>:6642` of the southbound database servers.
    pub sb_remotes: Vec<String>,
    /// Geneve endpoint of this host.
    pub encap_ip: IpAddr,
    /// physnet → bridge (§11.3); provider networks only.
    #[serde(default)]
    pub bridge_mappings: BTreeMap<String, String>,
    /// `br-int`'s datapath: `netdev` for vhost-user, else `system`.
    pub datapath: Datapath,
}

/// PEM material for the chassis (written 0600 under [`CERT_DIR`]).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisCerts {
    pub key_pem: String,
    pub cert_pem: String,
    pub ca_pem: String,
}

impl std::fmt::Debug for ChassisCerts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChassisCerts(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OvnPort {
    pub lport: String,
    pub ovn_installed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OvnStatus {
    pub chassis: Option<String>,
    pub controller_running: bool,
    pub sb_remotes: Vec<String>,
    pub br_int: bool,
    pub datapath: Option<String>,
    pub ports: Vec<OvnPort>,
}

impl ChassisSpec {
    pub fn validate(&self) -> Result<(), OvsError> {
        // `node:<id>`: the CN of the node's certificate, as SB RBAC wants.
        match self.chassis.strip_prefix("node:") {
            Some(id) => validate_name("chassis", id, 64)?,
            None => validate_name("chassis", &self.chassis, 64)?,
        }
        if self.sb_remotes.is_empty() {
            return Err(OvsError::invalid("at least one southbound remote is needed"));
        }
        for r in &self.sb_remotes {
            let ok = r.strip_prefix("ssl:").is_some_and(|hp| {
                hp.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok() && h.chars().all(|c| c.is_ascii_hexdigit() || ".:[]-".contains(c)))
            });
            if !ok {
                return Err(OvsError::invalid(format!("remote '{r}' must look like ssl:<ip>:<port>")));
            }
        }
        for (net, br) in &self.bridge_mappings {
            validate_name("physnet", net, 15)?;
            validate_name("bridge", br, MAX_IFNAME)?;
        }
        Ok(())
    }

    fn mappings(&self) -> String {
        self.bridge_mappings.iter().map(|(n, b)| format!("{n}:{b}")).collect::<Vec<_>>().join(",")
    }
}

fn cert_paths(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (dir.join("chassis.key"), dir.join("chassis.crt"), dir.join("ca.crt"))
}

/// Make this host a chassis. Idempotent: every step sets what it wants and
/// nothing else, and the controller only restarts when its trust changed.
pub fn ensure_chassis(exec: &dyn Exec, spec: &ChassisSpec, certs: &ChassisCerts, dir: &Path) -> Result<OvnStatus, OvsError> {
    spec.validate()?;
    // br-int: ours, with the datapath asked for. A different datapath is
    // refused while VM ports sit on it.
    match vsctl::find_by_name(exec, "Bridge", BR_INT, &["name", "datapath_type", "external_ids", "ports"])? {
        Some(row) => {
            if !row.owned_by_glidex() && row.str("datapath_type").is_some() && !row.map("external_ids").contains_key(ROLE_KEY) {
                // OVN's own br-int (created by ovn-controller) is fine to adopt: tag it.
                vsctl::run(exec, [vec!["set".into(), "Bridge".into(), BR_INT.into()], tag_args(&owned_tags("ovn-int", &[]))].concat())?;
            }
            let have = row.str("datapath_type").unwrap_or_default();
            let want = spec.datapath.as_str();
            let have = if have.is_empty() { "system" } else { have.as_str() };
            if have != want {
                let ports = list_br_int_ports(exec)?;
                if ports.iter().any(|p| p.starts_with("gx")) {
                    return Err(OvsError::Conflict { message: format!("br-int has {have} datapath and VM ports on it; move them before changing it to {want}") });
                }
                vsctl::run(exec, vec!["set".into(), "Bridge".into(), BR_INT.into(), format!("datapath_type={want}")])?;
            }
        }
        None => {
            let mut args = vec![
                "--may-exist".to_string(),
                "add-br".into(),
                BR_INT.into(),
                "--".into(),
                "set".into(),
                "Bridge".into(),
                BR_INT.into(),
                format!("datapath_type={}", spec.datapath.as_str()),
                "fail_mode=secure".into(),
                "other-config:disable-in-band=true".into(),
            ];
            args.extend(tag_args(&owned_tags("ovn-int", &[])));
            vsctl::run(exec, args)?;
        }
    }

    // Certificates: root-only files that ovn-controller reads.
    exec.create_dir(dir, 0o700)?;
    let (key, crt, ca) = cert_paths(dir);
    let mut changed = false;
    for (path, data) in [(&key, &certs.key_pem), (&crt, &certs.cert_pem), (&ca, &certs.ca_pem)] {
        if exec.read_file(path).ok().as_deref() != Some(data.as_str()) {
            exec.write_file(path, data.as_bytes())?;
            changed = true;
        }
    }
    vsctl::run(exec, vec!["set-ssl".into(), key.display().to_string(), crt.display().to_string(), ca.display().to_string()])?;

    // The chassis settings.
    let remote = spec.sb_remotes.join(",");
    let mut args = vec![
        "set".to_string(),
        "open_vswitch".into(),
        ".".into(),
        format!("external_ids:system-id={}", spec.chassis),
        format!("external_ids:ovn-remote={remote}"),
        "external_ids:ovn-encap-type=geneve".into(),
        format!("external_ids:ovn-encap-ip={}", spec.encap_ip),
        format!("external_ids:{CHASSIS_TAG}=1"),
    ];
    let m = spec.mappings();
    if m.is_empty() {
        vsctl::run(exec, vec!["--if-exists".into(), "remove".into(), "open_vswitch".into(), ".".into(), "external_ids".into(), "ovn-bridge-mappings".into()])?;
    } else {
        args.push(format!("external_ids:ovn-bridge-mappings={m}"));
    }
    vsctl::run(exec, args)?;

    // ovn-controller: start it; restart only when its certificates changed
    // (it keeps forwarding on the flows it installed while it restarts).
    // `ovn-controller.service` is static (Debian/Ubuntu): `ovn-host` is what is
    // enabled, and it wants the controller.
    exec.check(&Cmd::new(Program::Systemctl, ["enable", "--now", "ovn-host"]))?;
    exec.check(&Cmd::new(Program::Systemctl, ["start", "ovn-controller"]))?;
    if changed {
        exec.check(&Cmd::new(Program::Systemctl, ["try-reload-or-restart", "ovn-controller"]))?;
    }
    status(exec)
}

fn list_br_int_ports(exec: &dyn Exec) -> Result<Vec<String>, OvsError> {
    let out = exec.check(&Cmd::new(Program::OvsVsctl, ["list-ports", BR_INT]))?.stdout_str();
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
}

/// What this chassis looks like now. Read-only.
pub fn status(exec: &dyn Exec) -> Result<OvnStatus, OvsError> {
    let ovs = vsctl::list(exec, "Open_vSwitch", &["external_ids"])?;
    let ids = ovs.first().map(|r| r.map("external_ids")).unwrap_or_default();
    let controller_running = exec.run(&Cmd::new(Program::Systemctl, ["is-active", "--quiet", "ovn-controller"])).map(|o| o.status == 0).unwrap_or(false);
    let br = vsctl::find_by_name(exec, "Bridge", BR_INT, &["name", "datapath_type", "external_ids"])?;
    let ports = vsctl::list(exec, "Interface", &["name", "external_ids"])?
        .into_iter()
        .filter_map(|r| {
            let ids = r.map("external_ids");
            Some(OvnPort { lport: ids.get("iface-id")?.clone(), ovn_installed: ids.get("ovn-installed").map(String::as_str) == Some("true") })
        })
        .collect();
    Ok(OvnStatus {
        chassis: ids.get("system-id").cloned(),
        controller_running,
        sb_remotes: ids.get("ovn-remote").map(|r| r.split(',').map(String::from).collect()).unwrap_or_default(),
        br_int: br.is_some(),
        datapath: br.and_then(|b| b.str("datapath_type")).map(|d| if d.is_empty() { "system".into() } else { d }),
        ports,
    })
}

/// Leave OVN: remove the chassis settings, stop the controller, delete the
/// certificates. Refused while a glidex VM port is on `br-int`. `br-int`
/// itself stays, as OVN leaves it.
pub fn leave(exec: &dyn Exec, confirm: bool, dir: &Path) -> Result<(), OvsError> {
    if !confirm {
        return Err(OvsError::ConfirmationRequired { impact: "this host stops being an OVN chassis".into() });
    }
    let owned = crate::vm_port::list_owned(exec)?;
    if let Some((vm, nic, port)) = owned.iter().find(|(_, _, p)| list_br_int_ports(exec).map(|l| l.contains(p)).unwrap_or(false)) {
        return Err(OvsError::Conflict { message: format!("VM {vm} nic {nic} ({port}) is on br-int: move or detach it first") });
    }
    exec.check(&Cmd::new(Program::Systemctl, ["disable", "--now", "ovn-host"]))?;
    exec.check(&Cmd::new(Program::Systemctl, ["stop", "ovn-controller"]))?;
    for k in ["system-id", "ovn-remote", "ovn-encap-type", "ovn-encap-ip", "ovn-bridge-mappings", CHASSIS_TAG] {
        vsctl::run(exec, vec!["--if-exists".into(), "remove".into(), "open_vswitch".into(), ".".into(), "external_ids".into(), k.into()])?;
    }
    vsctl::run(exec, vec!["del-ssl".into()])?;
    let (key, crt, ca) = cert_paths(dir);
    for p in [key, crt, ca] {
        exec.remove_file(&p)?;
    }
    let _ = (OWNER_KEY, OWNER_VALUE);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const FIND_BR: &str = "ovs-vsctl --format=json --columns=name,datapath_type,external_ids,ports find Bridge name=br-int";

    fn spec() -> ChassisSpec {
        ChassisSpec {
            chassis: "node-1".into(),
            sb_remotes: vec!["ssl:192.0.2.11:6642".into(), "ssl:192.0.2.12:6642".into()],
            encap_ip: "192.0.2.21".parse().unwrap(),
            bridge_mappings: BTreeMap::from([("uplink".to_string(), "gxbr-lan".to_string())]),
            datapath: Datapath::System,
        }
    }

    fn certs() -> ChassisCerts {
        ChassisCerts { key_pem: "KEY".into(), cert_pem: "CERT".into(), ca_pem: "CA".into() }
    }

    fn empty_find() -> Output {
        Output::ok(r#"{"data":[],"headings":["name","datapath_type","external_ids","ports"]}"#)
    }

    #[test]
    fn joining_sets_the_chassis_and_starts_the_controller() {
        let exec = RecordingExec::new();
        exec.on(FIND_BR, empty_find());
        exec.on("ovs-vsctl --format=json --columns=name,datapath_type,external_ids find Bridge", Output::ok(r#"{"data":[["br-int","system",["map",[]]]],"headings":["name","datapath_type","external_ids"]}"#));
        exec.on("ovs-vsctl --format=json --columns=external_ids list Open_vSwitch", Output::ok(r#"{"data":[],"headings":["external_ids"]}"#));
        exec.on("ovs-vsctl --format=json --columns=name,external_ids list Interface", Output::ok(r#"{"data":[],"headings":["name","external_ids"]}"#));
        let dir = Path::new("/etc/glidex/ovn");
        ensure_chassis(&exec, &spec(), &certs(), dir).unwrap();
        let calls = exec.calls();
        assert!(calls.iter().any(|c| c.starts_with("ovs-vsctl --may-exist add-br br-int -- set Bridge br-int datapath_type=system fail_mode=secure")), "{calls:?}");
        assert!(calls.contains(&"ovs-vsctl set-ssl /etc/glidex/ovn/chassis.key /etc/glidex/ovn/chassis.crt /etc/glidex/ovn/ca.crt".to_string()), "{calls:?}");
        let set = calls.iter().find(|c| c.starts_with("ovs-vsctl set open_vswitch .")).unwrap();
        for want in [
            "external_ids:system-id=node-1",
            "external_ids:ovn-remote=ssl:192.0.2.11:6642,ssl:192.0.2.12:6642",
            "external_ids:ovn-encap-type=geneve",
            "external_ids:ovn-encap-ip=192.0.2.21",
            "external_ids:ovn-bridge-mappings=uplink:gxbr-lan",
        ] {
            assert!(set.contains(want), "{set}");
        }
        assert!(calls.contains(&"systemctl enable --now ovn-host".to_string()) && calls.contains(&"systemctl start ovn-controller".to_string()));
        assert!(calls.contains(&"systemctl try-reload-or-restart ovn-controller".to_string()), "new certificates restart it");
        assert_eq!(exec.read_file(&dir.join("chassis.key")).unwrap(), "KEY");
        // Again, with the same certificates: no restart.
        let exec2 = RecordingExec::new();
        exec2.on(FIND_BR, empty_find());
        exec2.on("ovs-vsctl --format=json --columns=name,datapath_type,external_ids find Bridge", Output::ok(r#"{"data":[["br-int","system",["map",[]]]],"headings":["name","datapath_type","external_ids"]}"#));
        exec2.on("ovs-vsctl --format=json --columns=external_ids list Open_vSwitch", Output::ok(r#"{"data":[],"headings":["external_ids"]}"#));
        exec2.on("ovs-vsctl --format=json --columns=name,external_ids list Interface", Output::ok(r#"{"data":[],"headings":["name","external_ids"]}"#));
        for (f, d) in [("chassis.key", "KEY"), ("chassis.crt", "CERT"), ("ca.crt", "CA")] {
            exec2.file(&dir.join(f).display().to_string(), d);
        }
        ensure_chassis(&exec2, &spec(), &certs(), dir).unwrap();
        assert!(!exec2.calls().iter().any(|c| c.contains("restart")), "{:?}", exec2.calls());
    }

    #[test]
    fn bad_remotes_and_names_are_refused() {
        let mut s = spec();
        s.sb_remotes = vec!["tcp:1.2.3.4:6642".into()];
        assert!(s.validate().is_err());
        let mut s = spec();
        s.sb_remotes = vec!["ssl:1.2.3.4:6642; rm -rf /".into()];
        assert!(s.validate().is_err());
        let mut s = spec();
        s.chassis = "bad name".into();
        assert!(s.validate().is_err());
        let mut s = spec();
        s.sb_remotes.clear();
        assert!(s.validate().is_err());
    }

    #[test]
    fn leaving_needs_confirmation_and_an_empty_br_int() {
        let exec = RecordingExec::new();
        assert!(matches!(leave(&exec, false, Path::new("/x")), Err(OvsError::ConfirmationRequired { .. })));
        let exec = RecordingExec::new();
        exec.on(
            "ovs-vsctl --format=json --columns=name,external_ids list Interface",
            Output::ok(r#"{"data":[["gx1a2b3c4d-0",["map",[["glidex-owner","glidex"],["glidex-role","vm"],["glidex-vm-id","1a2b3c4d-5e6f-4a1b-9c2d-0123456789ab"],["glidex-nic","0"]]]]],"headings":["name","external_ids"]}"#),
        );
        exec.on("ovs-vsctl list-ports br-int", Output::ok("gx1a2b3c4d-0\npatch-x\n"));
        assert!(matches!(leave(&exec, true, Path::new("/x")), Err(OvsError::Conflict { .. })));
    }
}

/// Where `ovn-central` reads its options on Debian and Ubuntu.
pub const CENTRAL_DEFAULTS: &str = "/etc/default/ovn-central";
pub const NB_PORT: u16 = 6641;
pub const SB_PORT: u16 = 6642;
pub const NB_RAFT_PORT: u16 = 6643;
pub const SB_RAFT_PORT: u16 = 6644;
/// The SB listener without an RBAC role, for `ovn-northd` on the servers
/// (it must reach the SB leader, wherever that is). Servers only: the
/// firewall should keep it from agents, as for the Raft ports.
pub const SB_NORTHD_PORT: u16 = 6648;

/// This server's part of OVN's own Raft groups (§4: the northbound and
/// southbound databases, separate from glidex's).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CentralSpec {
    /// The address the databases listen on (and cluster over).
    pub local_ip: IpAddr,
    /// An existing server to join; `None` for the first, which makes the clusters.
    pub join: Option<IpAddr>,
    /// Every server's address: northd talks to all of them.
    pub servers: Vec<IpAddr>,
}

fn remote_list(servers: &[IpAddr], port: u16) -> String {
    servers.iter().map(|ip| match ip {
        IpAddr::V4(a) => format!("ssl:{a}:{port}"),
        IpAddr::V6(a) => format!("ssl:[{a}]:{port}"),
    }).collect::<Vec<_>>().join(",")
}

/// The `OVN_CTL_OPTS` line for `spec`, with certificates under `dir`.
pub fn central_opts(spec: &CentralSpec, dir: &Path) -> String {
    let (key, crt, ca) = cert_paths(dir);
    let mut o = vec![
        format!("--db-nb-addr={}", spec.local_ip),
        format!("--db-nb-port={NB_PORT}"),
        format!("--db-sb-addr={}", spec.local_ip),
        format!("--db-sb-port={SB_PORT}"),
        format!("--db-nb-cluster-local-addr={}", spec.local_ip),
        format!("--db-nb-cluster-local-port={NB_RAFT_PORT}"),
        // Raft between the servers over SSL too (the default is plain TCP).
        "--db-nb-cluster-local-proto=ssl".to_string(),
        "--db-sb-cluster-local-proto=ssl".to_string(),
        format!("--db-sb-cluster-local-addr={}", spec.local_ip),
        format!("--db-sb-cluster-local-port={SB_RAFT_PORT}"),
        "--db-nb-create-insecure-remote=no".to_string(),
        "--db-sb-create-insecure-remote=no".to_string(),
        // northd follows the leader, so it talks to every server; the SB's
        // chassis listener only grants the `ovn-controller` role, so northd
        // has a listener of its own.
        format!("--ovn-northd-nb-db={}", remote_list(&spec.servers, NB_PORT)),
        format!("--ovn-northd-sb-db={}", remote_list(&spec.servers, SB_NORTHD_PORT)),
    ];
    if let Some(j) = spec.join {
        o.push(format!("--db-nb-cluster-remote-addr={j}"));
        o.push(format!("--db-nb-cluster-remote-port={NB_RAFT_PORT}"));
        o.push(format!("--db-sb-cluster-remote-addr={j}"));
        o.push(format!("--db-sb-cluster-remote-port={SB_RAFT_PORT}"));
        o.push("--db-nb-cluster-remote-proto=ssl".to_string());
        o.push("--db-sb-cluster-remote-proto=ssl".to_string());
    }
    for (what, flag) in [("nb-db", "ovn-nb-db"), ("sb-db", "ovn-sb-db"), ("northd", "ovn-northd")] {
        let _ = what;
        o.push(format!("--{flag}-ssl-key={}", key.display()));
        o.push(format!("--{flag}-ssl-cert={}", crt.display()));
        o.push(format!("--{flag}-ssl-ca-cert={}", ca.display()));
    }
    format!("OVN_CTL_OPTS=\"{}\"\n", o.join(" "))
}

/// Where Debian's `ovn-ctl` puts the local database sockets.
pub const CENTRAL_NB_SOCK: &str = "/var/run/ovn/ovnnb_db.sock";
pub const CENTRAL_SB_SOCK: &str = "/var/run/ovn/ovnsb_db.sock";

/// The listeners clients reach (§11.1): `ovn-ctl` only makes the Raft
/// listeners; the client ones come from each database's `Connection` table,
/// which is replicated, so setting it on any member is enough. The NB is for
/// the control plane; the SB's role limits a chassis to its own rows (its
/// certificate's CN must be its chassis name).
pub fn central_connection_cmds(nb_sock: &str, sb_sock: &str) -> Vec<Cmd> {
    vec![
        // `--no-leader-only`: this member may be a follower; it forwards the write.
        Cmd::new(Program::OvnNbctl, [format!("--db=unix:{nb_sock}"), "--no-leader-only".into(), "--timeout=20".into(), "set-connection".into(), format!("pssl:{NB_PORT}")]),
        Cmd::new(Program::OvnSbctl, [format!("--db=unix:{sb_sock}"), "--no-leader-only".into(), "--timeout=20".into(), "set-connection".into(), "role=ovn-controller".into(), format!("pssl:{SB_PORT}"), "role=".into(), format!("pssl:{SB_NORTHD_PORT}")]),
    ]
}

/// The chassis name of a node: its certificate's CN, as SB RBAC requires.
pub fn chassis_name(node_id: &str) -> String {
    format!("node:{node_id}")
}

/// Make this host run `ovn-central` (the NB and SB databases and northd) as
/// `spec` says. Idempotent; the services restart only when their options or
/// certificates changed. A server whose database file is gone must not be
/// restarted into the group under the same identity (D18): that is the
/// caller's rule, enforced before this runs.
pub fn ensure_central(exec: &dyn Exec, spec: &CentralSpec, certs: &ChassisCerts, dir: &Path) -> Result<bool, OvsError> {
    if spec.servers.is_empty() || !spec.servers.contains(&spec.local_ip) {
        return Err(OvsError::invalid("servers must list this server's own address"));
    }
    exec.create_dir(dir, 0o700)?;
    let (key, crt, ca) = cert_paths(dir);
    let mut changed = false;
    for (path, data) in [(&key, &certs.key_pem), (&crt, &certs.cert_pem), (&ca, &certs.ca_pem)] {
        if exec.read_file(path).ok().as_deref() != Some(data.as_str()) {
            exec.write_file(path, data.as_bytes())?;
            changed = true;
        }
    }
    let opts = central_opts(spec, dir);
    if exec.read_file(Path::new(CENTRAL_DEFAULTS)).ok().as_deref() != Some(opts.as_str()) {
        exec.write_file(Path::new(CENTRAL_DEFAULTS), opts.as_bytes())?;
        changed = true;
    }
    exec.check(&Cmd::new(Program::Systemctl, ["enable", "ovn-central"]))?;
    let active = exec.run(&Cmd::new(Program::Systemctl, ["is-active", "--quiet", "ovn-central"]))?.status == 0;
    if !active {
        exec.check(&Cmd::new(Program::Systemctl, ["start", "ovn-central"]))?;
    } else if changed {
        // PartOf= carries the restart to the NB, SB and northd units.
        exec.check(&Cmd::new(Program::Systemctl, ["restart", "ovn-central"]))?;
    }
    // The client listeners (idempotent: the same rows each time).
    for cmd in central_connection_cmds(CENTRAL_NB_SOCK, CENTRAL_SB_SOCK) {
        exec.check(&cmd)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod central_tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    fn spec(join: Option<&str>) -> CentralSpec {
        CentralSpec { local_ip: "192.0.2.12".parse().unwrap(), join: join.map(|j| j.parse().unwrap()), servers: vec!["192.0.2.11".parse().unwrap(), "192.0.2.12".parse().unwrap()] }
    }

    fn certs() -> ChassisCerts {
        ChassisCerts { key_pem: "K".into(), cert_pem: "C".into(), ca_pem: "A".into() }
    }

    #[test]
    fn the_first_server_makes_the_clusters_and_later_ones_join() {
        let first = central_opts(&spec(None), Path::new("/etc/glidex/ovn"));
        assert!(first.contains("--db-nb-cluster-local-addr=192.0.2.12") && !first.contains("cluster-remote"), "{first}");
        assert!(first.contains("--ovn-northd-sb-db=ssl:192.0.2.11:6648,ssl:192.0.2.12:6648") && first.contains("--db-nb-cluster-local-proto=ssl"), "{first}");
        assert!(first.contains("--db-nb-create-insecure-remote=no"));
        let later = central_opts(&spec(Some("192.0.2.11")), Path::new("/etc/glidex/ovn"));
        assert!(later.contains("--db-nb-cluster-remote-addr=192.0.2.11 --db-nb-cluster-remote-port=6643"), "{later}");
        assert!(later.contains("--db-sb-cluster-remote-addr=192.0.2.11 --db-sb-cluster-remote-port=6644"), "{later}");
        assert!(later.contains("--db-sb-cluster-remote-proto=ssl"), "{later}");
        assert!(later.contains("--ovn-sb-db-ssl-ca-cert=/etc/glidex/ovn/ca.crt"), "{later}");
    }

    #[test]
    fn central_is_started_once_and_restarted_only_on_change() {
        let dir = Path::new("/etc/glidex/ovn");
        let exec = RecordingExec::new();
        exec.on("systemctl is-active --quiet ovn-central", Output::failed(3, ""));
        assert!(ensure_central(&exec, &spec(None), &certs(), dir).unwrap());
        assert!(exec.calls().contains(&"systemctl start ovn-central".to_string()), "{:?}", exec.calls());
        assert!(exec.calls().iter().any(|c| c.contains("ovn-sbctl --db=unix:/var/run/ovn/ovnsb_db.sock --no-leader-only --timeout=20 set-connection role=ovn-controller pssl:6642 role= pssl:6648")), "{:?}", exec.calls());
        // The same again, running: nothing to do.
        let exec2 = RecordingExec::new();
        for (f, d) in [("chassis.key", "K"), ("chassis.crt", "C"), ("ca.crt", "A")] {
            exec2.file(&dir.join(f).display().to_string(), d);
        }
        exec2.file(CENTRAL_DEFAULTS, &central_opts(&spec(None), dir));
        assert!(!ensure_central(&exec2, &spec(None), &certs(), dir).unwrap());
        assert!(!exec2.calls().iter().any(|c| c.contains("restart") || c.contains("start ovn")), "{:?}", exec2.calls());
        // A different option set restarts it.
        let exec3 = RecordingExec::new();
        exec3.file(CENTRAL_DEFAULTS, "OVN_CTL_OPTS=\"old\"\n");
        ensure_central(&exec3, &spec(None), &certs(), dir).unwrap();
        assert!(exec3.calls().contains(&"systemctl restart ovn-central".to_string()));
        // A server that isn't in its own list is a mistake.
        let mut bad = spec(None);
        bad.servers.pop();
        assert!(ensure_central(&exec, &bad, &certs(), dir).is_err());
    }
}

const NB_CTL: &str = "/var/run/ovn/ovnnb_db.ctl";
const SB_CTL: &str = "/var/run/ovn/ovnsb_db.ctl";
const SB_SOCK: &str = "unix:/var/run/ovn/ovnsb_db.sock";

/// The id of the server at `address` in the output of `cluster/status`:
/// `    7bd6 (7bd6 at ssl:192.0.2.11:6643) next_index=…`.
fn cluster_server_id(status: &str, address: &str) -> Option<String> {
    status.lines().skip_while(|l| !l.starts_with("Servers:")).skip(1).find_map(|l| {
        let l = l.trim();
        let (id, rest) = l.split_once(' ')?;
        (rest.contains(&format!("ssl:{address}:")) || rest.contains(&format!("ssl:[{address}]:"))).then(|| id.to_string())
    })
}

/// Take the host at `address` out of OVN (§5.5 steps 3–4), run on a
/// remaining server: kick it from the NB and SB clusters if it was a
/// server, and delete its chassis. Idempotent: a member already gone is fine.
pub fn forget_member(exec: &dyn Exec, address: &str, chassis: &str, server: bool) -> Result<(), OvsError> {
    if server {
        for (ctl, db) in [(NB_CTL, "OVN_Northbound"), (SB_CTL, "OVN_Southbound")] {
            let out = exec.check(&Cmd::new(Program::OvsAppctl, ["-t", ctl, "cluster/status", db]))?;
            if let Some(id) = cluster_server_id(&String::from_utf8_lossy(&out.stdout), address) {
                exec.check(&Cmd::new(Program::OvsAppctl, ["-t", ctl, "cluster/kick", db, id.as_str()]))?;
            }
        }
    }
    exec.check(&Cmd::new(Program::OvnSbctl, [&format!("--db={SB_SOCK}"), "--no-leader-only", "--if-exists", "chassis-del", chassis]))?;
    Ok(())
}

#[cfg(test)]
mod forget_tests {
    use super::*;
    use crate::exec::{Output, RecordingExec};

    const STATUS: &str = "Name: OVN_Northbound\nServer ID: 7bd6 (7bd6aaaa)\nServers:\n    7bd6 (7bd6 at ssl:192.0.2.11:6643) (self) next_index=3 match_index=9\n    c5e1 (c5e1 at ssl:192.0.2.12:6643) next_index=10 match_index=9\n";

    #[test]
    fn a_server_is_kicked_from_both_clusters_and_its_chassis_deleted() {
        let exec = RecordingExec::new();
        exec.on("ovs-appctl -t /var/run/ovn/ovnnb_db.ctl cluster/status OVN_Northbound", Output::ok(STATUS));
        exec.on("ovs-appctl -t /var/run/ovn/ovnsb_db.ctl cluster/status OVN_Southbound", Output::ok(STATUS.replace("Northbound", "Southbound").replace("6643", "6644").as_str()));
        forget_member(&exec, "192.0.2.12", "node-1", true).unwrap();
        let calls = exec.calls();
        assert!(calls.contains(&"ovs-appctl -t /var/run/ovn/ovnnb_db.ctl cluster/kick OVN_Northbound c5e1".to_string()), "{calls:?}");
        assert!(calls.contains(&"ovs-appctl -t /var/run/ovn/ovnsb_db.ctl cluster/kick OVN_Southbound c5e1".to_string()), "{calls:?}");
        assert!(calls.contains(&"ovn-sbctl --db=unix:/var/run/ovn/ovnsb_db.sock --no-leader-only --if-exists chassis-del node-1".to_string()), "{calls:?}");
    }

    #[test]
    fn an_agent_only_loses_its_chassis_and_a_member_already_gone_is_fine() {
        let exec = RecordingExec::new();
        forget_member(&exec, "192.0.2.50", "agent-1", false).unwrap();
        assert_eq!(exec.calls(), vec!["ovn-sbctl --db=unix:/var/run/ovn/ovnsb_db.sock --no-leader-only --if-exists chassis-del agent-1".to_string()]);
        let exec = RecordingExec::new();
        exec.on("ovs-appctl -t /var/run/ovn/ovnnb_db.ctl cluster/status OVN_Northbound", Output::ok(STATUS));
        exec.on("ovs-appctl -t /var/run/ovn/ovnsb_db.ctl cluster/status OVN_Southbound", Output::ok(STATUS));
        forget_member(&exec, "192.0.2.99", "gone", true).unwrap();
        assert!(!exec.calls().iter().any(|c| c.contains("cluster/kick")), "{:?}", exec.calls());
    }
}
