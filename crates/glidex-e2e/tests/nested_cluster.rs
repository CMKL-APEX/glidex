//! A two-node glidex cluster with OVN, nested in VMs of this host's glidex
//! (spec/clustering.md §15, the part a single host can't show).
//!
//! 1. This host's glidex (the "outer" one) makes two VMs, `h1` and `h2`.
//! 2. glidex is installed into both from this workspace (`glidex-install
//!    --prebuilt`, with the binaries built here). That is the test cluster.
//! 3. `h1` inits a cluster with OVN; `h2` joins it as an agent. Both become
//!    OVN chassis, with a Geneve tunnel between them.
//! 4. On a cluster network, the test cluster runs one guest on each host
//!    (nested twice). Each guest gets its address from OVN DHCP and pings the
//!    other across the tunnel, then reports on its console.
//! 5. Checked: each guest's logical port is bound to its own host's chassis
//!    and up, OVS marked the interfaces `ovn-installed`, the Geneve tunnel
//!    carried packets both ways, and both guests reached each other.
//!
//! Ignored by default: it needs a glidex on this host whose API socket the
//! user can use, nested KVM, about 9 GiB of memory, internet access from the
//! VMs (packages, the guest image), and 10-20 minutes:
//!
//! ```text
//! cargo test -p glidex-e2e --test nested_cluster -- --ignored --nocapture
//! ```
//!
//! `GLIDEX_E2E_KEEP=1` leaves the VMs running for a look afterwards;
//! `GLIDEX_E2E_API`, `GLIDEX_E2E_IMAGE` (host image, default ubuntu-26.04),
//! `GLIDEX_E2E_GUEST_IMAGE` (default debian-13), `GLIDEX_E2E_NETWORK` (the
//! outer network the hosts use, default `default`) override the defaults.

use glidex_e2e::*;
use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn step(what: &str) {
    eprintln!("\n==> {what}");
}

/// The outer host's VMs and credential, deleted at the end unless kept.
struct Outer {
    api: Api<'static>,
    vms: Vec<String>,
    credential: String,
    keydir: std::path::PathBuf,
    keep: bool,
}

impl Drop for Outer {
    fn drop(&mut self) {
        if self.keep {
            eprintln!("GLIDEX_E2E_KEEP set: leaving VMs {:?} and credential {}", self.vms, self.credential);
            return;
        }
        for id in &self.vms {
            let _ = self.api.call("POST", &format!("/vms/{id}/stop?wait=60"), Some(&json!({})));
            let _ = self.api.call("DELETE", &format!("/vms/{id}?wait=120"), None);
        }
        let _ = self.api.call("DELETE", &format!("/credentials/{}", self.credential), None);
        let _ = std::fs::remove_dir_all(&self.keydir);
    }
}

const BINARIES: [&str; 7] = ["glidex-control-plane", "gxctl", "glidex-ui", "glidex-vm-shim", "glidex-netd", "glidex-authd", "glidex-install"];

/// The binaries the inner installs use, built from this workspace now.
fn build_release() {
    let root = workspace_root();
    let mut c = Command::new(env!("CARGO"));
    c.current_dir(&root).args(["build", "--release"]);
    for p in ["glidex-control-plane", "glidex-netd", "glidex-ui", "glidex-vm-shim", "glidex-authd", "glidex-install"] {
        c.args(["-p", p]);
    }
    let out = run(&mut c);
    assert!(out.status.success(), "cargo build --release:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(root.join("crates/glidex-ui/ui/dist/index.html").exists(), "build the web UI first (cd crates/glidex-ui/ui && bun run build)");
}

/// Make, start and reach one outer VM; returns (id, ssh).
fn make_host(api: &Api, name: &str, credential: &str, key: &std::path::Path) -> (String, Ssh) {
    let body = json!({
        "name": name,
        "vcpu_count": 4,
        "mem_size_mib": 4096,
        "image": env("GLIDEX_E2E_IMAGE", "ubuntu-26.04"),
        "firmware": "cloudhv-edk2",
        "root_disk_size_gib": 30,
        "credential": credential,
        "hypervisor": "cloudhypervisor",
        "networks": [{ "network": env("GLIDEX_E2E_NETWORK", "default") }],
        "kernel_image_path": "",
        "rootfs_path": "",
    });
    let vm = api.ok("POST", "/vms", Some(&body));
    let id = vm["id"].as_str().unwrap().to_string();
    let vm = api.ok("POST", &format!("/vms/{id}/start?wait=120"), Some(&json!({})));
    let ip = wait_for(&format!("an address for {name}"), Duration::from_secs(120), || {
        let v = api.ok("GET", &format!("/vms/{id}"), None);
        v["nics"][0]["ipv4"].as_str().map(String::from)
    });
    let _ = vm;
    let ssh = Ssh { host: ip, user: credential.to_string(), key: key.to_path_buf() };
    wait_for(&format!("SSH to {name}"), Duration::from_secs(300), || ssh.reachable().then_some(()));
    (id, ssh)
}

/// Install glidex into a host from this workspace (`--prebuilt`), as the
/// installer would on a fresh machine. Returns the installer's output.
fn install_inner(h: &Ssh) -> String {
    let root = workspace_root();
    let dest = root.display().to_string();
    // The installer finds its workspace where it was built: the same path.
    h.check(&format!("sudo mkdir -p {d} && sudo chown -R {u}: {d}", d = shell_quote(&dest), u = h.user));
    h.send_tree(&root, &["."], &["./target", "./.git", "*/node_modules"], &dest);
    let bins: Vec<String> = BINARIES.iter().map(|b| format!("target/release/{b}")).collect();
    let bins: Vec<&str> = bins.iter().map(String::as_str).collect();
    h.send_tree(&root, &bins, &[], &dest);
    let out = h.run(&format!("cd {} && ./target/release/glidex-install --prebuilt --ovs-profile kernel --no-qemu 2>&1", shell_quote(&dest)));
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "glidex-install on {} failed:\n{log}", h.host);
    log
}

fn sb(h: &Ssh, args: &str) -> String {
    h.check(&format!("sudo ovn-sbctl --no-leader-only --db=unix:/var/run/ovn/ovnsb_db.sock {args}"))
}

/// `(rx, tx)` packets of the Geneve tunnel ports on a host.
fn geneve_packets(h: &Ssh) -> (u64, u64) {
    let out = h.check(
        r#"for p in $(sudo ovs-vsctl list-ports br-int); do if [ "$(sudo ovs-vsctl get Interface "$p" type)" = geneve ]; then echo "$(sudo ovs-vsctl get Interface "$p" statistics:rx_packets) $(sudo ovs-vsctl get Interface "$p" statistics:tx_packets)"; fi; done"#,
    );
    out.lines().filter_map(|l| l.split_once(' ')).fold((0, 0), |(r, t), (a, b)| (r + a.trim().parse::<u64>().unwrap_or(0), t + b.trim().parse::<u64>().unwrap_or(0)))
}

#[test]
#[ignore = "drives this host's glidex: makes two VMs and installs a nested glidex cluster (10-20 min)"]
fn a_nested_two_node_cluster_binds_vm_ports_and_carries_traffic_over_geneve() {
    let api_path = env("GLIDEX_E2E_API", API_SOCKET);
    let outer_api = Api::Local(api_path.clone().into());
    let tag = uuid::Uuid::new_v4().simple().to_string()[..6].to_string();
    let keep = std::env::var_os("GLIDEX_E2E_KEEP").is_some();

    step("preflight: the outer glidex, nested KVM, release binaries");
    let (code, _) = outer_api.call("GET", "/nodes", None);
    assert_eq!(code, 200, "the outer glidex API at {api_path} is not usable by this user");
    let nested = ["/sys/module/kvm_intel/parameters/nested", "/sys/module/kvm_amd/parameters/nested"]
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .any(|v| matches!(v.trim(), "1" | "Y"));
    assert!(nested, "nested KVM is off on this host: the inner glidex couldn't run its guests");
    let images = outer_api.ok("GET", "/images", None);
    for name in [env("GLIDEX_E2E_IMAGE", "ubuntu-26.04"), "cloudhv-edk2".to_string()] {
        let ready = images.as_array().into_iter().flatten().any(|i| i["name"] == name.as_str() && i["status"]["state"] == "ready");
        assert!(ready, "the outer glidex has no ready image {name}; pull it first (gxctl image pull {name})");
    }
    build_release();

    step("outer: a credential and two host VMs");
    let keydir = tempfile_dir(&tag);
    let (key, public) = ssh_keypair(&keydir);
    let credential = format!("gxe2e{tag}");
    outer_api.ok("POST", "/credentials", Some(&json!({ "username": credential, "ssh_authorized_keys": [public] })));
    let mut outer = Outer { api: Api::Local(api_path.clone().into()), vms: Vec::new(), credential: credential.clone(), keydir: keydir.clone(), keep };
    let (id1, h1) = make_host(&outer_api, &format!("gxe2e-{tag}-h1"), &credential, &key);
    outer.vms.push(id1);
    let (id2, h2) = make_host(&outer_api, &format!("gxe2e-{tag}-h2"), &credential, &key);
    outer.vms.push(id2);
    for h in [&h1, &h2] {
        h.check("test -c /dev/kvm");
    }
    eprintln!("hosts: h1 {} h2 {}", h1.host, h2.host);

    step("inner: install glidex (glidex-test-cluster) on both hosts");
    let logs: Vec<String> = std::thread::scope(|s| {
        let a = s.spawn(|| install_inner(&h1));
        let b = s.spawn(|| install_inner(&h2));
        vec![a.join().unwrap(), b.join().unwrap()]
    });
    for (h, log) in [&h1, &h2].iter().zip(&logs) {
        // A fresh host gets OVS set up (no leftover br-int in the way) and
        // OVN installed with its services off until it is clustered.
        assert!(!log.contains("Skipped: not changing Open vSwitch"), "{}: OVS setup was skipped:\n{log}", h.host);
        assert!(log.contains("OVN is installed with its services off"), "{}:\n{log}", h.host);
        assert_eq!(h.check("systemctl is-active ovn-central ovn-host || true").split_whitespace().collect::<Vec<_>>(), ["inactive", "inactive"]);
    }

    step("inner: h1 inits a cluster with OVN, h2 joins as an agent");
    for h in [&h1, &h2] {
        h.check(r#"echo '{"cluster":{"ovn":{"enabled":true}}}' | sudo install -m 0640 -o root -g glidex /dev/stdin /etc/glidex/control-plane.json && sudo systemctl restart glidex-control-plane"#);
        let api = Api::Remote(h);
        wait_for("the inner control plane", Duration::from_secs(60), || (api.call("GET", "/nodes", None).0 == 200).then_some(()));
    }
    h1.check(&format!("gxctl cluster init --advertise {}:8842 --force", h1.host));
    let token = h1.check("gxctl cluster join-token --role agent 2>/dev/null");
    let joined = h2.run_with_input(
        &format!("umask 077 && cat > ~/join.token && gxctl cluster join --server {}:8842 --role agent --advertise {}:8842 --token-file ~/join.token; s=$?; rm -f ~/join.token; exit $s", h1.host, h2.host),
        token.trim().as_bytes(),
    );
    assert!(joined.status.success(), "join: {}{}", String::from_utf8_lossy(&joined.stdout), String::from_utf8_lossy(&joined.stderr));
    let inner = Api::Remote(&h1);
    let nodes = wait_for_or("both nodes Ready with their capacity", Duration::from_secs(180), || {
        let v = inner.ok("GET", "/nodes", None);
        let all: Vec<Value> = v.as_array().cloned().unwrap_or_default();
        (all.len() == 2 && all.iter().all(|n| n["status"]["ready"] == "True" && n["status"]["capacity"]["cpus"].as_u64().unwrap_or(0) > 0)).then_some(all)
    }, || format!("nodes: {}", inner.call("GET", "/nodes", None).1));
    let node_of = |name: &str| nodes.iter().find(|n| n["spec"]["name"] == name).map(|n| n["meta"]["id"].as_str().unwrap().to_string()).unwrap();
    let (n1, n2) = (node_of(&format!("gxe2e-{tag}-h1")), node_of(&format!("gxe2e-{tag}-h2")));

    step("inner: both hosts become OVN chassis, with a Geneve tunnel");
    let (c1, c2) = (format!("node:{n1}"), format!("node:{n2}"));
    // The southbound database only exists once h1's netd has started ovn-central.
    wait_for("both chassis in the southbound database", Duration::from_secs(300), || {
        let out = h1.run("sudo ovn-sbctl --no-leader-only --db=unix:/var/run/ovn/ovnsb_db.sock show");
        let show = String::from_utf8_lossy(&out.stdout).into_owned();
        (out.status.success() && show.contains(&c1) && show.contains(&c2)).then_some(())
    });
    for (h, peer) in [(&h1, &h2.host), (&h2, &h1.host)] {
        wait_for("a Geneve tunnel to the other host", Duration::from_secs(120), || {
            let show = h.check("sudo ovs-vsctl show");
            (show.contains("type: geneve") && show.contains(&format!("remote_ip=\"{peer}\""))).then_some(())
        });
    }

    step("inner: a cluster network and one guest on each host");
    inner.ok("POST", "/images", Some(&json!({ "catalog": env("GLIDEX_E2E_GUEST_IMAGE", "debian-13") })));
    inner.ok("POST", "/networks", Some(&json!({ "name": "e2e-net", "mode": "isolated", "port_type": "tap", "scope": "cluster", "subnet": "10.89.77.0/24" })));
    // The cluster IPAM hands out the lowest free address: the first guest .2, the second .3.
    for (h, name, peer) in [(&h1, "e2e-a", "10.89.77.3"), (&h2, "e2e-b", "10.89.77.2")] {
        h.check("sudo mkdir -p /var/lib/gxe2e && sudo chmod 0755 /var/lib/gxe2e");
        let out = h.run_with_input("sudo sh -s", ping_seed_script(name, peer, "/var/lib/gxe2e/seed.img").as_bytes());
        assert!(out.status.success(), "seed on {}: {}", h.host, String::from_utf8_lossy(&out.stderr));
    }
    let guest_image = env("GLIDEX_E2E_GUEST_IMAGE", "debian-13");
    let mut guests = Vec::new();
    for (name, node) in [("e2e-a", &n1), ("e2e-b", &n2)] {
        let vm = inner.ok(
            "POST",
            "/vms",
            Some(&json!({
                "name": name, "vcpu_count": 1, "mem_size_mib": 768, "image": guest_image, "firmware": "cloudhv-edk2",
                "cloud_init_path": "/var/lib/gxe2e/seed.img", "hypervisor": "cloudhypervisor", "node": node,
                "networks": [{ "network": "e2e-net" }], "kernel_image_path": "", "rootfs_path": "",
            })),
        );
        assert_eq!(vm["node"].as_str(), Some(node.as_str()), "{vm}");
        guests.push((name, vm["id"].as_str().unwrap().to_string()));
    }
    let before = (geneve_packets(&h1), geneve_packets(&h2));
    for (name, id) in &guests {
        inner.ok("POST", &format!("/vms/{id}/start"), Some(&json!({})));
        wait_for_or(
            &format!("{name} running"),
            Duration::from_secs(900),
            || (inner.ok("GET", &format!("/vms/{id}"), None)["state"] == "running").then_some(()),
            || {
                let vm = inner.call("GET", &format!("/vms/{id}"), None).1;
                let events = inner.call("GET", &format!("/vms/{id}/events"), None).1;
                let disks = inner.call("GET", "/disks", None).1;
                let images = inner.call("GET", "/images", None).1;
                let nodes = inner.call("GET", "/nodes", None).1;
                let log = |h: &Ssh| String::from_utf8_lossy(&h.run(LOG_PROBLEMS).stdout).into_owned();
                format!("vm: {vm}\nevents: {events}\ndisks: {disks}\nimages: {images}\nnodes: {nodes}\nh1 log:\n{}\nh2 log:\n{}", log(&h1), log(&h2))
            },
        );
    }

    step("the guests reach each other across hosts");
    for (name, id) in &guests {
        let line = wait_for(&format!("{name} to report its ping"), Duration::from_secs(600), || {
            let out = h1.run(&format!("curl -s --unix-socket {API_SOCKET} http://glidex/vms/{id}/console/log"));
            let log = String::from_utf8_lossy(&out.stdout).replace('\r', "");
            log.lines().find(|l| l.contains("GXE2E-PING-")).map(String::from)
        });
        eprintln!("{name}: {line}");
        assert!(line.contains("GXE2E-PING-OK"), "{name}: {line}");
    }

    step("each port is bound to its own host's chassis, up and installed");
    let chassis_uuid = |name: &str| sb(&h1, &format!("--bare --columns=_uuid find Chassis name={}", shell_quote(&format!("\"{name}\"")))).trim().to_string();
    for ((name, id), (h, chassis)) in guests.iter().zip([(&h1, &c1), (&h2, &c2)]) {
        let lport = format!("gx{}-0", &id[..8]);
        let binding = sb(&h1, &format!("--bare --columns=chassis,up find Port_Binding logical_port={}", shell_quote(&format!("\"{lport}\""))));
        let fields: Vec<&str> = binding.split_whitespace().collect();
        assert_eq!(fields, [chassis_uuid(chassis).as_str(), "true"], "{name}: binding of {lport}: {binding}");
        let installed = h.check(&format!("sudo ovs-vsctl get Interface {lport} external_ids:ovn-installed"));
        assert_eq!(installed.trim(), "\"true\"", "{name}: {lport} on {}", h.host);
    }
    let after = (geneve_packets(&h1), geneve_packets(&h2));
    eprintln!("geneve packets (rx, tx): h1 {:?} -> {:?}, h2 {:?} -> {:?}", before.0, after.0, before.1, after.1);
    for (b, a, host) in [(before.0, after.0, "h1"), (before.1, after.1, "h2")] {
        assert!(a.0 > b.0 && a.1 > b.1, "{host}: the Geneve tunnel carried nothing: {b:?} -> {a:?}");
    }

    step("the agent's web UI is relayed to the server (spec/clustering-ui.md §3.6)");
    check_agent_ui(&h1, &h2, &credential, &keydir, &n1, &n2, &format!("gxe2e-{tag}-h2"));
    drop(outer);
}

/// One request to a glidex-ui from this host: (status, headers, body). The
/// body goes through stdin, so a password never reaches a command line.
fn ui_curl(jar: &std::path::Path, base: &str, method: &str, path: &str, body: Option<&Value>, stream_secs: Option<u32>) -> (u16, String, String) {
    let mut c = Command::new("curl");
    c.args(["-s", "-k", "-i", "-X", method, "-b"]).arg(jar).arg("-c").arg(jar).args(["-H", &format!("Origin: {base}")]);
    if let Some(t) = stream_secs {
        c.args(["-N", "--max-time", &t.to_string()]);
    } else {
        c.args(["--max-time", "30"]);
    }
    if body.is_some() {
        c.args(["-H", "content-type: application/json", "--data-binary", "@-"]);
    }
    c.arg(format!("{base}{path}")).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().expect("curl");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        if let Some(b) = body {
            stdin.write_all(b.to_string().as_bytes()).unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, head.to_ascii_lowercase(), rest.to_string())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|l| l.strip_prefix(&format!("{name}: "))).map(str::trim)
}

/// Log in through h2's (the agent's) glidex-ui as the test's user, made a PAM
/// user with a throwaway password on h1 (the server that authenticates).
fn check_agent_ui(h1: &Ssh, h2: &Ssh, user: &str, dir: &std::path::Path, n1: &str, n2: &str, h2_name: &str) {
    let password = uuid::Uuid::new_v4().simple().to_string();
    let out = h1.run_with_input("sudo chpasswd", format!("{user}:{password}\n").as_bytes());
    assert!(out.status.success(), "setting a login password on h1");
    let base = format!("https://{}:5173", h2.host);
    let jar = dir.join("ui-cookies");
    wait_for("h2's web UI", Duration::from_secs(60), || (ui_curl(&jar, &base, "GET", "/api/health", None, None).0 == 200).then_some(()));

    let (s, _, body) = ui_curl(&jar, &base, "POST", "/api/auth/login", Some(&json!({ "method": "pam", "username": user, "password": password })), None);
    drop(password);
    assert_eq!(s, 200, "PAM login through the agent's UI: {}", body.chars().take(300).collect::<String>());

    let (s, head, body) = ui_curl(&jar, &base, "GET", "/api/cluster/status", None, None);
    assert_eq!(s, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["node_id"], n1, "cluster reads are the server's");
    assert_eq!(header(&head, "x-glidex-served-by"), Some(n1));

    let (s, head, _) = ui_curl(&jar, &base, "GET", "/api/system/reconcile", None, None);
    assert_eq!(s, 200);
    assert_eq!(header(&head, "x-glidex-served-by"), Some(n2), "host-local reads run on the agent");

    let (s, _, body) = ui_curl(&jar, &base, "GET", "/api/vms", None, None);
    assert_eq!(s, 200, "{body}");
    let vms: Value = serde_json::from_str(&body).unwrap();
    let b = vms.as_array().unwrap().iter().find(|v| v["name"] == "e2e-b").expect("e2e-b listed");
    assert_eq!(b["node"], n2);
    assert_eq!(b["node_name"], h2_name);

    // The live stream comes through the relay as it happens, not at the end.
    let (s, _, body) = ui_curl(&jar, &base, "GET", "/api/watch?kinds=nodes,cluster,policy", None, Some(8));
    assert_eq!(s, 200);
    assert!(body.contains("event: synced") && body.contains("\"kind\":\"node\""), "watch through the relay: {}", body.chars().take(400).collect::<String>());
    let _ = std::fs::remove_file(&jar);
}

/// The control plane's warnings, errors and cluster-link changes, newest last.
const LOG_PROBLEMS: &str =
    "sudo journalctl -u glidex-control-plane --no-pager -o short-iso | grep -E 'WARN|ERROR|cluster servers|liveness|disk created|image (ready|copied)' | grep -v ovn_dbctl | tail -n 80 | cut -c1-300";

fn tempfile_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("glidex-e2e-{tag}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}
