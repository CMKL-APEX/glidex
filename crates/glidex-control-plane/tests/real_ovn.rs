//! Against a real `ovsdb-server` with the OVN northbound schema, over SSL with
//! certificates from the cluster PKI (spec/clustering.md §11.1, §12.1).
//! Ignored: it needs `ovsdb-server`, `ovsdb-tool` and `ovn-nbctl`
//! (`cargo test -p glidex-control-plane --test real_ovn -- --ignored`).
//! It starts its own database in a temporary directory and stops it after.

use glidex_control_plane::cluster::pki::{Ca, NodeKey};
use glidex_ovn::*;
use glidex_ovs::exec::SystemExec;
use std::process::Command;

struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs ovsdb-server, ovsdb-tool, ovn-nbctl and /usr/share/ovn/ovn-nb.ovsschema"]
fn the_northbound_database_takes_cluster_certificates_and_a_sync_over_ssl() {
    let dir = tempfile::tempdir().unwrap();
    let p = |f: &str| dir.path().join(f);
    // The cluster CA, and one server certificate for the database and the client.
    let ca = Ca::generate("test-cluster").unwrap();
    let key = NodeKey::generate().unwrap();
    let csr = key.csr(&["127.0.0.1".parse().unwrap()], &[]).unwrap();
    let (cert, _) = ca.sign_node(&csr, "node-1", true, 30).unwrap();
    std::fs::write(p("ca.crt"), &ca.cert_pem).unwrap();
    std::fs::write(p("node.crt"), &cert).unwrap();
    std::fs::write(p("node.key"), key.key_pem().as_bytes()).unwrap();

    let schema = "/usr/share/ovn/ovn-nb.ovsschema";
    assert!(Command::new("ovsdb-tool").args(["create", p("nb.db").to_str().unwrap(), schema]).status().unwrap().success());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _server = Server(
        Command::new("ovsdb-server")
            .arg(format!("--remote=pssl:{port}:127.0.0.1"))
            .arg(format!("--private-key={}", p("node.key").display()))
            .arg(format!("--certificate={}", p("node.crt").display()))
            .arg(format!("--ca-cert={}", p("ca.crt").display()))
            .arg(format!("--unixctl={}", p("nb.ctl").display()))
            .arg(format!("--log-file={}", p("nb.log").display()))
            .arg(p("nb.db"))
            .spawn()
            .expect("ovsdb-server"),
    );
    std::thread::sleep(std::time::Duration::from_millis(800));

    let exec = SystemExec::new();
    let conn = NbConn { db: vec![format!("ssl:127.0.0.1:{port}")], key: p("node.key"), cert: p("node.crt"), ca: p("ca.crt"), daemon: None };
    let nb = Nb::new(&exec, conn.clone());
    let desired = Desired {
        networks: vec![NetworkSpec { name: "gxssl".into(), kind: NetKind::Isolated, cidr: Some("10.89.230.0/24".parse().unwrap()), dns: vec![], mtu: 1442, router: None }],
        ..Default::default()
    };
    let first = sync(&nb, &desired).unwrap_or_else(|e| panic!("sync over SSL: {e:?}\nserver log:\n{}", std::fs::read_to_string(p("nb.log")).unwrap_or_default()));
    assert!(!first.changed.is_empty());
    assert!(sync(&nb, &desired).unwrap().changed.is_empty());

    // A certificate the cluster CA didn't sign is refused by the database.
    let other = Ca::generate("other").unwrap();
    let k2 = NodeKey::generate().unwrap();
    let (c2, _) = other.sign_node(&k2.csr(&["127.0.0.1".parse().unwrap()], &[]).unwrap(), "intruder", true, 30).unwrap();
    std::fs::write(p("x.crt"), &c2).unwrap();
    std::fs::write(p("x.key"), k2.key_pem().as_bytes()).unwrap();
    let out = Command::new("ovn-nbctl")
        .args(["--timeout=5", &format!("--db=ssl:127.0.0.1:{port}"), "-p", p("x.key").to_str().unwrap(), "-c", p("x.crt").to_str().unwrap(), "-C", p("ca.crt").to_str().unwrap(), "ls-list"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an outsider's certificate got in");
}

/// `ovn-ctl` with the options glidex writes to `/etc/default/ovn-central`
/// (`central_opts`), sandboxed: two servers on 127.0.0.1 and 127.0.0.2 form
/// the NB and SB Raft clusters over SSL with cluster certificates, and a
/// client reaches the pair.
#[test]
#[ignore = "needs ovn-ctl (ovn-central) and the 127.0.0.2 loopback address"]
fn two_servers_form_the_ovn_database_clusters_with_glidex_options() {
    use glidex_ovs::ovn::{central_opts, CentralSpec};
    let root = tempfile::tempdir().unwrap();
    let ca = Ca::generate("test-cluster").unwrap();
    let ips: [std::net::IpAddr; 2] = ["127.0.0.1".parse().unwrap(), "127.0.0.2".parse().unwrap()];
    let mut dirs = Vec::new();
    for (i, ip) in ips.iter().enumerate() {
        let d = root.path().join(format!("s{i}"));
        for sub in ["run", "db", "log", "certs"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let key = NodeKey::generate().unwrap();
        let (cert, _) = ca.sign_node(&key.csr(&[*ip], &[]).unwrap(), &format!("node-{i}"), true, 30).unwrap();
        std::fs::write(d.join("certs/chassis.key"), key.key_pem().as_bytes()).unwrap();
        std::fs::write(d.join("certs/chassis.crt"), &cert).unwrap();
        std::fs::write(d.join("certs/ca.crt"), &ca.cert_pem).unwrap();
        dirs.push(d);
    }
    let mut stop = Vec::new();
    for (i, ip) in ips.iter().enumerate() {
        let d = &dirs[i];
        let spec = CentralSpec { local_ip: *ip, join: (i > 0).then(|| ips[0]), servers: ips.to_vec() };
        let line = central_opts(&spec, &d.join("certs"));
        let opts = line.trim().trim_start_matches("OVN_CTL_OPTS=\"").trim_end_matches('"').to_string();
        let file_opts = format!(
            "--db-nb-file={0}/db/nb.db --db-sb-file={0}/db/sb.db --db-nb-sock={0}/run/nb.sock --db-sb-sock={0}/run/sb.sock --ovn-nb-logfile={0}/log/nb.log --ovn-sb-logfile={0}/log/sb.log",
            d.display()
        );
        for what in ["start_nb_ovsdb", "start_sb_ovsdb"] {
            let out = Command::new("sh")
                .arg("-c")
                .arg(format!("/usr/share/ovn/scripts/ovn-ctl {opts} {file_opts} {what}"))
                .env("OVN_RUNDIR", d.join("run"))
                .env("OVN_DBDIR", d.join("db"))
                .env("OVN_LOGDIR", d.join("log"))
                .output()
                .unwrap();
            assert!(out.status.success(), "server {i} {what}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        }
        stop.push(d.clone());
    }
    struct Stop(Vec<std::path::PathBuf>);
    impl Drop for Stop {
        fn drop(&mut self) {
            for d in &self.0 {
                for pid in std::fs::read_dir(d.join("run")).into_iter().flatten().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pid")) {
                    if let Ok(p) = std::fs::read_to_string(pid.path()) {
                        let _ = Command::new("kill").arg(p.trim()).status();
                    }
                }
            }
        }
    }
    let _stop = Stop(stop);
    std::thread::sleep(std::time::Duration::from_secs(2));
    // The client listeners, as `ensure_central` makes them (on the joined member:
    // the Connection rows are replicated).
    let exec0 = SystemExec::new();
    for cmd in glidex_ovs::ovn::central_connection_cmds(dirs[1].join("run/nb.sock").to_str().unwrap(), dirs[1].join("run/sb.sock").to_str().unwrap()) {
        glidex_ovs::exec::Exec::check(&exec0, &cmd).unwrap_or_else(|e| {
            let logs: String = dirs.iter().map(|d| format!("--- {}\n{}{}", d.display(), std::fs::read_to_string(d.join("log/nb.log")).unwrap_or_default(), std::fs::read_dir(d.join("run")).map(|r| r.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ")).unwrap_or_default())).collect();
            panic!("{}: {e:?}\n{logs}", cmd.display())
        });
    }
    // Both NB members answer, as one cluster, through SSL with glidex's client.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let c = &dirs[1].join("certs");
    let conn = NbConn {
        db: ips.iter().map(|ip| format!("ssl:{ip}:{}", glidex_ovs::ovn::NB_PORT)).collect(),
        key: c.join("chassis.key"),
        cert: c.join("chassis.crt"),
        ca: c.join("ca.crt"),
        daemon: None,
    };
    let exec = SystemExec::new();
    let nb = Nb::new(&exec, conn);
    let desired = Desired { networks: vec![NetworkSpec { name: "gxha".into(), kind: NetKind::Isolated, cidr: Some("10.89.231.0/24".parse().unwrap()), dns: vec![], mtu: 1442, router: None }], ..Default::default() };
    sync(&nb, &desired).unwrap_or_else(|e| panic!("sync to the pair: {e:?}\n{}", std::fs::read_to_string(dirs[0].join("log/nb.log")).unwrap_or_default()));
    for (i, d) in dirs.iter().enumerate() {
        let st = Command::new("ovs-appctl").args(["-t", d.join("run/ovnnb_db.ctl").to_str().unwrap(), "cluster/status", "OVN_Northbound"]).output().unwrap();
        let text = String::from_utf8_lossy(&st.stdout);
        assert!(text.contains("Status: cluster member"), "server {i}: {text}{}", String::from_utf8_lossy(&st.stderr));
        assert!(text.contains("ssl:127.0.0.1:6643") && text.contains("ssl:127.0.0.2:6643"), "server {i}: {text}");
    }
    // northd on the joined member, through its own sockets, compiles the NB.
    let northd = Server(
        Command::new("ovn-northd")
            // As `central_opts` points it: every server, NB and the SB's northd listener.
            .arg(format!("--ovnnb-db=ssl:127.0.0.1:{0},ssl:127.0.0.2:{0}", glidex_ovs::ovn::NB_PORT))
            .arg(format!("--ovnsb-db=ssl:127.0.0.1:{0},ssl:127.0.0.2:{0}", glidex_ovs::ovn::SB_NORTHD_PORT))
            .arg(format!("--private-key={}", dirs[1].join("certs/chassis.key").display()))
            .arg(format!("--certificate={}", dirs[1].join("certs/chassis.crt").display()))
            .arg(format!("--ca-cert={}", dirs[1].join("certs/ca.crt").display()))
            .arg(format!("--unixctl={}", dirs[1].join("run/northd.ctl").display()))
            .arg(format!("--log-file={}", dirs[1].join("log/northd.log").display()))
            .arg("-vfile:info")
            .stderr(std::fs::File::create(dirs[1].join("log/northd.err")).unwrap())
            .spawn()
            .unwrap(),
    );
    let sbctl = |args: &[&str]| Command::new("ovn-sbctl").arg(format!("--db=unix:{}", dirs[0].join("run/sb.sock").display())).args(args).output().unwrap();
    let mut flows = String::new();
    for _ in 0..100 {
        flows = String::from_utf8_lossy(&sbctl(&["lflow-list", "gx-gxha"]).stdout).into_owned();
        if flows.contains("ls_in_") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    assert!(flows.contains("ls_in_"), "northd made no logical flows: {}{}", std::fs::read_to_string(dirs[1].join("log/northd.log")).unwrap_or_default(), std::fs::read_to_string(dirs[1].join("log/northd.err")).unwrap_or_default());
    drop(northd);
    let conns = String::from_utf8_lossy(&sbctl(&["--columns=target,role", "list", "Connection"]).stdout).into_owned();
    assert!(conns.contains("pssl:6642") && conns.contains("pssl:6648"), "{conns}");
    // SB RBAC: a chassis certificate (CN node:<id>) may add only its own chassis.
    let k = NodeKey::generate().unwrap();
    let (cc, _) = ca.sign_node(&k.csr(&["127.0.0.9".parse().unwrap()], &[]).unwrap(), "agent-9", false, 30).unwrap();
    std::fs::write(root.path().join("a.crt"), &cc).unwrap();
    std::fs::write(root.path().join("a.key"), k.key_pem().as_bytes()).unwrap();
    let as_agent = |args: &[&str]| {
        Command::new("ovn-sbctl")
            .args(["--timeout=10", &format!("--db=ssl:127.0.0.1:{}", glidex_ovs::ovn::SB_PORT), "-p", root.path().join("a.key").to_str().unwrap(), "-c", root.path().join("a.crt").to_str().unwrap(), "-C", dirs[0].join("certs/ca.crt").to_str().unwrap()])
            .args(args)
            .output()
            .unwrap()
    };
    let own = glidex_ovs::ovn::chassis_name("agent-9");
    let ok = as_agent(&["chassis-add", &own, "geneve", "127.0.0.9"]);
    assert!(ok.status.success(), "a chassis adds itself: {}", String::from_utf8_lossy(&ok.stderr));
    let other = as_agent(&["chassis-add", "node:someone-else", "geneve", "127.0.0.10"]);
    assert!(!other.status.success(), "RBAC let a chassis add another");
    // The member that joined sees what was written through the pair.
    let out = Command::new("ovn-nbctl").args([&format!("--db=unix:{}", dirs[1].join("run/nb.sock").display()), "--no-leader-only", "ls-list"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("gx-gxha"), "{}", String::from_utf8_lossy(&out.stderr));
}
