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
//! `GLIDEX_E2E_PROJECT` (outer project, default `default`),
//! `GLIDEX_E2E_API`, `GLIDEX_E2E_IMAGE` (host image, default ubuntu-26.04),
//! `GLIDEX_E2E_GUEST_IMAGE` (default debian-13), `GLIDEX_E2E_NETWORK` (the
//! outer network the hosts use, default `default`) override the defaults.

use glidex_e2e::*;
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn step(what: &str) {
    eprintln!("\n==> {what}");
}

/// The VM in a `{vm}` answer (`POST /vms`, `GET /vms/{id}`, writes), in the
/// proto3 JSON shape (spec/grpc.md §9.1).
fn vm_of(v: &Value) -> &Value {
    assert!(v["vm"].is_object(), "not a VM answer: {v}");
    &v["vm"]
}

/// The items of a list answer (`{images: [...]}`, `{vms: [...]}`, ...): an
/// empty list is left out, so `{}` is none.
fn items<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or_default()
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
            eprintln!(
                "GLIDEX_E2E_KEEP set: leaving VMs {:?} and credential {}",
                self.vms, self.credential
            );
            return;
        }
        for id in &self.vms {
            let _ = self
                .api
                .call("POST", &format!("/vms/{id}/stop?wait=60"), Some(&json!({})));
            let _ = self
                .api
                .call("DELETE", &format!("/vms/{id}?wait=120"), None);
        }
        let _ = self.api.call(
            "DELETE",
            &format!(
                "/credentials/{}?project={}",
                self.credential,
                env("GLIDEX_E2E_PROJECT", "default")
            ),
            None,
        );
        let _ = std::fs::remove_dir_all(&self.keydir);
    }
}

const BINARIES: [&str; 7] = [
    "glidex-control-plane",
    "gxctl",
    "glidex-ui",
    "glidex-vm-shim",
    "glidex-netd",
    "glidex-authd",
    "glidex-install",
];

/// The binaries the inner installs use, built from this workspace now.
fn build_release() {
    let root = workspace_root();
    let mut c = Command::new(env!("CARGO"));
    c.current_dir(&root).args(["build", "--release"]);
    for p in [
        "glidex-control-plane",
        "glidex-netd",
        "glidex-ui",
        "glidex-vm-shim",
        "glidex-authd",
        "glidex-install",
    ] {
        c.args(["-p", p]);
    }
    let out = run(&mut c);
    assert!(
        out.status.success(),
        "cargo build --release:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        root.join("crates/glidex-ui/ui/dist/index.html").exists(),
        "build the web UI first (cd crates/glidex-ui/ui && bun run build)"
    );
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
        "project": env("GLIDEX_E2E_PROJECT", "default"),
        "hypervisor": "cloudhypervisor",
        "networks": [{ "network": env("GLIDEX_E2E_NETWORK", "default") }],
        "kernel_image_path": "",
        "rootfs_path": "",
    });
    let vm = api.ok("POST", "/vms", Some(&body));
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let vm = api.ok(
        "POST",
        &format!("/vms/{id}/start?wait=120"),
        Some(&json!({})),
    );
    let ip = wait_for(
        &format!("an address for {name}"),
        Duration::from_secs(120),
        || {
            let v = api.ok("GET", &format!("/vms/{id}"), None);
            vm_of(&v)["nics"][0]["ipv4"].as_str().map(String::from)
        },
    );
    let _ = vm;
    let ssh = Ssh {
        host: ip,
        user: credential.to_string(),
        key: key.to_path_buf(),
    };
    wait_for(&format!("SSH to {name}"), Duration::from_secs(300), || {
        ssh.reachable().then_some(())
    });
    (id, ssh)
}

/// Install glidex into a host from this workspace (`--prebuilt`), as the
/// installer would on a fresh machine. Returns the installer's output.
fn install_inner(h: &Ssh) -> String {
    let root = workspace_root();
    let dest = root.display().to_string();
    // The installer finds its workspace where it was built: the same path.
    h.check(&format!(
        "sudo mkdir -p {d} && sudo chown -R {u}: {d}",
        d = shell_quote(&dest),
        u = h.user
    ));
    h.send_tree(
        &root,
        &["."],
        &["./target", "./.git", "*/node_modules"],
        &dest,
    );
    let bins: Vec<String> = BINARIES
        .iter()
        .map(|b| format!("target/release/{b}"))
        .collect();
    let bins: Vec<&str> = bins.iter().map(String::as_str).collect();
    h.send_tree(&root, &bins, &[], &dest);
    let out = h.run(&format!(
        "cd {} && ./target/release/glidex-install --prebuilt --ovs-profile kernel --no-qemu 2>&1",
        shell_quote(&dest)
    ));
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "glidex-install on {} failed:\n{log}",
        h.host
    );
    log
}

fn sb(h: &Ssh, args: &str) -> String {
    h.check(&format!(
        "sudo ovn-sbctl --no-leader-only --db=unix:/var/run/ovn/ovnsb_db.sock {args}"
    ))
}

/// `(rx, tx)` packets of the Geneve tunnel ports on a host.
fn geneve_packets(h: &Ssh) -> (u64, u64) {
    let out = h.check(
        r#"for p in $(sudo ovs-vsctl list-ports br-int); do if [ "$(sudo ovs-vsctl get Interface "$p" type)" = geneve ]; then echo "$(sudo ovs-vsctl get Interface "$p" statistics:rx_packets) $(sudo ovs-vsctl get Interface "$p" statistics:tx_packets)"; fi; done"#,
    );
    out.lines()
        .filter_map(|l| l.split_once(' '))
        .fold((0, 0), |(r, t), (a, b)| {
            (
                r + a.trim().parse::<u64>().unwrap_or(0),
                t + b.trim().parse::<u64>().unwrap_or(0),
            )
        })
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
    assert_eq!(
        code, 200,
        "the outer glidex API at {api_path} is not usable by this user"
    );
    let nested = [
        "/sys/module/kvm_intel/parameters/nested",
        "/sys/module/kvm_amd/parameters/nested",
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .any(|v| matches!(v.trim(), "1" | "Y"));
    assert!(
        nested,
        "nested KVM is off on this host: the inner glidex couldn't run its guests"
    );
    let images = outer_api.ok("GET", "/images", None);
    for name in [
        env("GLIDEX_E2E_IMAGE", "ubuntu-26.04"),
        "cloudhv-edk2".to_string(),
    ] {
        let ready = items(&images, "images")
            .iter()
            .any(|i| i["name"] == name.as_str() && i["status"]["state"] == "IMAGE_STATE_READY");
        assert!(
            ready,
            "the outer glidex has no ready image {name}; pull it first (gxctl image pull {name})"
        );
    }
    build_release();

    step("outer: a credential and two host VMs");
    let keydir = tempfile_dir(&tag);
    let (key, public) = ssh_keypair(&keydir);
    let credential = format!("gxe2e{tag}");
    // In a named project: the caller's default project is whatever they last
    // picked in the UI, and may not reach the outer network.
    outer_api.ok("POST", "/credentials", Some(&json!({ "username": credential, "ssh_authorized_keys": [public], "project": env("GLIDEX_E2E_PROJECT", "default") })));
    let mut outer = Outer {
        api: Api::Local(api_path.clone().into()),
        vms: Vec::new(),
        credential: credential.clone(),
        keydir: keydir.clone(),
        keep,
    };
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
        assert!(
            !log.contains("Skipped: not changing Open vSwitch"),
            "{}: OVS setup was skipped:\n{log}",
            h.host
        );
        assert!(
            log.contains("OVN is installed with its services off"),
            "{}:\n{log}",
            h.host
        );
        assert_eq!(
            h.check("systemctl is-active ovn-central ovn-host || true")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["inactive", "inactive"]
        );
    }

    step("inner: h1 inits a cluster, h2 joins as an agent");
    for h in [&h1, &h2] {
        let api = Api::Remote(h);
        wait_for("the inner control plane", Duration::from_secs(60), || {
            (api.call("GET", "/nodes", None).0 == 200).then_some(())
        });
    }
    h1.check(&format!(
        "gxctl cluster init --advertise {}:8842 --force",
        h1.host
    ));
    let token = h1.check("gxctl cluster join-token --role agent 2>/dev/null");
    // Through the installer, as an agent host is set up for real: it must
    // leave the agent's web UI on, to send browsers to a server
    // (spec/host-page.md W5).
    let root = workspace_root().display().to_string();
    let joined = h2.run_with_input(
        &format!(
            "umask 077 && cat > ~/join.token && sudo {root}/target/release/glidex-install --prebuilt --join {}:8842 --role agent --advertise {}:8842 --token-file ~/join.token; s=$?; rm -f ~/join.token; exit $s",
            h1.host, h2.host
        ),
        token.trim().as_bytes(),
    );
    assert!(
        joined.status.success(),
        "join: {}{}",
        String::from_utf8_lossy(&joined.stdout),
        String::from_utf8_lossy(&joined.stderr)
    );
    assert_eq!(
        h2.check("systemctl is-enabled glidex-ui; systemctl is-active glidex-ui")
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["enabled", "active"],
        "the agent keeps its UI"
    );
    let inner = Api::Remote(&h1);
    let nodes = wait_for_or(
        "both nodes Ready with their capacity",
        Duration::from_secs(180),
        || {
            let v = inner.ok("GET", "/nodes", None);
            let all: Vec<Value> = items(&v, "nodes").to_vec();
            (all.len() == 2
                && all.iter().all(|n| {
                    n["status"]["ready"] == "CONDITION_STATUS_TRUE"
                        && n["status"]["capacity"]["cpus"].as_u64().unwrap_or(0) > 0
                }))
            .then_some(all)
        },
        || format!("nodes: {}", inner.call("GET", "/nodes", None).1),
    );
    let node_of = |name: &str| {
        nodes
            .iter()
            .find(|n| n["spec"]["name"] == name)
            .map(|n| n["meta"]["id"].as_str().unwrap().to_string())
            .unwrap()
    };
    let (n1, n2) = (
        node_of(&format!("gxe2e-{tag}-h1")),
        node_of(&format!("gxe2e-{tag}-h2")),
    );
    // The agent restarted into agent mode (spec/clustering-ui.md §3.6): its
    // UI socket only, no API of its own; gxctl there says where to log in.
    wait_for(
        "h2 in agent mode (no api.sock)",
        Duration::from_secs(60),
        || {
            (h2.run("test -S /run/glidex-cp/ui.sock && ! test -e /run/glidex-cp/api.sock")
                .status
                .success())
            .then_some(())
        },
    );
    let said = String::from_utf8_lossy(&h2.run("gxctl whoami 2>&1").stdout).into_owned();
    assert!(
        said.contains("cluster agent") && said.contains("gxctl auth login"),
        "gxctl on h2: {said}"
    );
    // h2 imported its firmware while standalone; the join left that file
    // unrecorded. It is the same pinned build as the cluster's, so h2
    // adopts it as its copy, and reports nothing left over (clustering.md §9.2.3).
    wait_for_or(
        "h2's firmware adopted as the cluster's",
        Duration::from_secs(180),
        || {
            let imgs = inner.ok("GET", "/images", None);
            let fw = items(&imgs, "images")
                .iter()
                .find(|i| {
                    i["kind"] == "IMAGE_KIND_FIRMWARE"
                        && i["source"]["firmware"]["key"] == "cloudhv-edk2"
                })?
                .clone();
            let on_h2 = fw["copies"]
                .as_array()?
                .iter()
                .any(|c| c["node"] == n2.as_str() && c["state"] == "ready");
            let spare = inner.ok("GET", &format!("/nodes/{n2}"), None)["node"]["status"]["features"]
                ["spare_image_files"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0);
            (on_h2 && spare == 0).then_some(())
        },
        || {
            format!(
                "images: {}\nh2: {}",
                inner.call("GET", "/images", None).1,
                inner.call("GET", &format!("/nodes/{n2}"), None).1["node"]["status"]["features"]
            )
        },
    );

    // OVN is a cluster setting (spec/clustering-ui.md §8a): turned on through
    // the API, no config file and no restart; every node follows.
    step("inner: OVN turned on for the cluster through the API");
    let ovn = wait_for_or(
        "both nodes to report OVN installed",
        Duration::from_secs(180),
        || {
            let v = inner.ok("GET", "/cluster/settings/ovn", None);
            v["nodes"]
                .as_array()
                .is_some_and(|ns| ns.len() == 2 && ns.iter().all(|n| n["missing"].is_null()))
                .then_some(v)
        },
        || format!("{}", inner.call("GET", "/cluster/settings/ovn", None).1),
    );
    assert!(!ovn["enabled"].as_bool().unwrap_or(false), "{ovn}");
    let on = inner.ok(
        "PUT",
        "/cluster/settings/ovn",
        Some(&json!({ "enabled": true })),
    );
    assert_eq!(
        (on["enabled"].as_bool(), on["source"].as_str()),
        (Some(true), Some("OVN_SETTINGS_SOURCE_CLUSTER")),
        "{on}"
    );

    step("inner: both hosts become OVN chassis, with a Geneve tunnel");
    let (c1, c2) = (format!("node:{n1}"), format!("node:{n2}"));
    // The southbound database only exists once h1's netd has started ovn-central.
    wait_for(
        "both chassis in the southbound database",
        Duration::from_secs(300),
        || {
            let out =
                h1.run("sudo ovn-sbctl --no-leader-only --db=unix:/var/run/ovn/ovnsb_db.sock show");
            let show = String::from_utf8_lossy(&out.stdout).into_owned();
            (out.status.success() && show.contains(&c1) && show.contains(&c2)).then_some(())
        },
    );
    for (h, peer) in [(&h1, &h2.host), (&h2, &h1.host)] {
        wait_for(
            "a Geneve tunnel to the other host",
            Duration::from_secs(120),
            || {
                let show = h.check("sudo ovs-vsctl show");
                (show.contains("type: geneve") && show.contains(&format!("remote_ip=\"{peer}\"")))
                    .then_some(())
            },
        );
    }

    step("inner: a cluster network and one guest on each host");
    inner.ok(
        "POST",
        "/images",
        Some(&json!({ "catalog": env("GLIDEX_E2E_GUEST_IMAGE", "debian-13") })),
    );
    inner.ok("POST", "/networks", Some(&json!({ "name": "e2e-net", "mode": "isolated", "port_type": "tap", "scope": "cluster", "subnet": "10.89.77.0/24" })));
    // The cluster IPAM hands out the lowest free address: the first guest .2, the second .3.
    for (h, name, peer) in [(&h1, "e2e-a", "10.89.77.3"), (&h2, "e2e-b", "10.89.77.2")] {
        h.check("sudo mkdir -p /var/lib/gxe2e && sudo chmod 0755 /var/lib/gxe2e");
        let out = h.run_with_input(
            "sudo sh -s",
            ping_seed_script(name, peer, "/var/lib/gxe2e/seed.img").as_bytes(),
        );
        assert!(
            out.status.success(),
            "seed on {}: {}",
            h.host,
            String::from_utf8_lossy(&out.stderr)
        );
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
        assert_eq!(vm_of(&vm)["node"].as_str(), Some(node.as_str()), "{vm}");
        guests.push((name, vm_of(&vm)["id"].as_str().unwrap().to_string()));
    }
    let before = (geneve_packets(&h1), geneve_packets(&h2));
    for (name, id) in &guests {
        inner.ok("POST", &format!("/vms/{id}/start"), Some(&json!({})));
        wait_for_or(
            &format!("{name} running"),
            Duration::from_secs(900),
            || {
                (vm_of(&inner.ok("GET", &format!("/vms/{id}"), None))["state"]
                    == "VM_STATE_RUNNING")
                    .then_some(())
            },
            || {
                let vm = inner.call("GET", &format!("/vms/{id}"), None).1;
                let events = inner.call("GET", &format!("/vms/{id}/events"), None).1;
                let disks = inner.call("GET", "/disks", None).1;
                let images = inner.call("GET", "/images", None).1;
                let nodes = inner.call("GET", "/nodes", None).1;
                let log =
                    |h: &Ssh| String::from_utf8_lossy(&h.run(LOG_PROBLEMS).stdout).into_owned();
                format!(
                    "vm: {vm}\nevents: {events}\ndisks: {disks}\nimages: {images}\nnodes: {nodes}\nh1 log:\n{}\nh2 log:\n{}",
                    log(&h1),
                    log(&h2)
                )
            },
        );
    }

    step("the guests reach each other across hosts");
    for (name, id) in &guests {
        let line = wait_for(
            &format!("{name} to report its ping"),
            Duration::from_secs(600),
            || {
                let out = h1.run(&format!(
                    "curl -s --unix-socket {API_SOCKET} http://glidex/vms/{id}/console/log"
                ));
                let log = String::from_utf8_lossy(&out.stdout).replace('\r', "");
                log.lines()
                    .find(|l| l.contains("GXE2E-PING-"))
                    .map(String::from)
            },
        );
        eprintln!("{name}: {line}");
        assert!(line.contains("GXE2E-PING-OK"), "{name}: {line}");
        // The address glidex shows is the one OVN's DHCP gave the guest.
        let mine = line
            .split("my address:")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or_default()
            .to_string();
        let vm = inner.ok("GET", &format!("/vms/{id}"), None);
        assert_eq!(
            vm_of(&vm)["nics"][0]["ipv4"].as_str(),
            Some(mine.as_str()),
            "{name}: {vm}"
        );
    }

    step("each port is bound to its own host's chassis, up and installed");
    let chassis_uuid = |name: &str| {
        sb(
            &h1,
            &format!(
                "--bare --columns=_uuid find Chassis name={}",
                shell_quote(&format!("\"{name}\""))
            ),
        )
        .trim()
        .to_string()
    };
    for ((name, id), (h, chassis)) in guests.iter().zip([(&h1, &c1), (&h2, &c2)]) {
        let lport = format!("gx{}-0", &id[..8]);
        let binding = sb(
            &h1,
            &format!(
                "--bare --columns=chassis,up find Port_Binding logical_port={}",
                shell_quote(&format!("\"{lport}\""))
            ),
        );
        let fields: Vec<&str> = binding.split_whitespace().collect();
        assert_eq!(
            fields,
            [chassis_uuid(chassis).as_str(), "true"],
            "{name}: binding of {lport}: {binding}"
        );
        let installed = h.check(&format!(
            "sudo ovs-vsctl get Interface {lport} external_ids:ovn-installed"
        ));
        assert_eq!(
            installed.trim(),
            "\"true\"",
            "{name}: {lport} on {}",
            h.host
        );
    }
    let after = (geneve_packets(&h1), geneve_packets(&h2));
    eprintln!(
        "geneve packets (rx, tx): h1 {:?} -> {:?}, h2 {:?} -> {:?}",
        before.0, after.0, before.1, after.1
    );
    for (b, a, host) in [(before.0, after.0, "h1"), (before.1, after.1, "h2")] {
        assert!(
            a.0 > b.0 && a.1 > b.1,
            "{host}: the Geneve tunnel carried nothing: {b:?} -> {a:?}"
        );
    }

    step("cluster views the UI uses, against real OVN (spec/clustering-ui.md §5, §8)");
    let ovn = inner.ok("GET", "/cluster/ovn", None);
    assert_eq!(ovn["enabled"], true, "{ovn}");
    for db in ["nb", "sb"] {
        let ms = ovn[db].as_array().unwrap();
        assert!(
            ms.iter().all(|m| m["reachable"] == true)
                && ms.iter().filter(|m| m["leader"] == true).count() == 1,
            "{db}: {ovn}"
        );
    }
    let chassis = ovn["chassis"].as_array().unwrap();
    for (n, ip) in [(&n1, &h1.host), (&n2, &h2.host)] {
        let c = chassis
            .iter()
            .find(|c| c["node"] == n.as_str())
            .unwrap_or_else(|| panic!("chassis of {n}: {ovn}"));
        assert!(
            c["encaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e.as_str() == Some(&format!("geneve {ip}"))),
            "{c}"
        );
    }
    let net = inner.ok("GET", "/networks/e2e-net/ovn", None);
    for ((name, _), node) in guests.iter().zip([&n1, &n2]) {
        let p = net["ports"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["vm"]["name"] == *name)
            .unwrap_or_else(|| panic!("{name}'s port: {net}"));
        assert_eq!(p["bound"]["node"], node.as_str(), "{p}");
    }
    let img = inner.ok(
        "GET",
        &format!("/images/{}", env("GLIDEX_E2E_GUEST_IMAGE", "debian-13")),
        None,
    );
    let copies = img["image"]["copies"].as_array().unwrap();
    assert!(
        copies.len() == 2 && copies.iter().all(|c| c["state"] == "ready"),
        "image copies: {img}"
    );
    // Each node's Geneve tunnel to the other, from its own Open vSwitch (§8b).
    let tun = inner.ok("GET", "/cluster/ovn/tunnels", None);
    let per = tun["nodes"].as_array().unwrap_or_else(|| panic!("{tun}"));
    for (me, peer) in [(&n1, &n2), (&n2, &n1)] {
        let n = per
            .iter()
            .find(|x| x["node"] == me.as_str())
            .unwrap_or_else(|| panic!("{me}: {tun}"));
        assert_eq!(n["answered"], true, "{n}");
        let t = n["tunnels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["peer"]["node"] == peer.as_str())
            .unwrap_or_else(|| panic!("{me} → {peer}: {n}"));
        assert_eq!(t["state"], "TUNNEL_STATE_PRESENT", "{t}");
        // uint64 counters are JSON strings (proto3 JSON).
        let count = |v: &Value| v.as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        assert!(
            count(&t["rx_packets"]) > 0 && count(&t["tx_packets"]) > 0,
            "the ping went through it: {t}"
        );
        assert!(items(n, "missing").is_empty(), "{n}");
    }
    let pv = inner.ok(
        "POST",
        "/scheduler/preview",
        Some(&json!({ "vcpu_count": 1, "mem_size_mib": 256 })),
    );
    assert_eq!(pv["nodes"].as_array().unwrap().len(), 2, "{pv}");
    assert!(pv["pick"].is_string(), "{pv}");

    step(
        "the agent's web UI sends browsers to the server's, which acts on the agent (spec/host-page.md)",
    );
    let ui = check_agent_ui(
        &h1,
        &h2,
        &credential,
        &keydir,
        &n1,
        &n2,
        &format!("gxe2e-{tag}-h2"),
    );

    step(
        "the agent's only NIC on a bridge with its address, from the server's UI, survives a reboot",
    );
    check_single_nic_uplink_survives_a_reboot(&h2, &n2, &ui);

    // spec/clustering-ui.md §8a: the agent's uplink bridge as the physical
    // network, the suggested shared edge taken as is, and a guest on the
    // other node reaching the LAN's router through it (Geneve, then SNAT on h2).
    step("the suggested shared edge carries a NAT network's traffic out");
    inner.ok(
        "PUT",
        &format!("/nodes/{n2}/ovn"),
        Some(&json!({ "bridge_mappings": { "uplink": "gxbr-up" } })),
    );
    let lan_gw = h2.check("ip -j route show default | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"gateway\"])'").trim().to_string();
    let d = wait_for_or(
        "a complete edge suggestion",
        Duration::from_secs(60),
        || {
            let d = inner.ok("GET", "/cluster/settings/ovn/edge-defaults", None);
            (d["complete"] == true).then_some(d)
        },
        || {
            format!(
                "{}",
                inner
                    .call("GET", "/cluster/settings/ovn/edge-defaults", None)
                    .1
            )
        },
    );
    assert_eq!(
        d["edge"]["gateway_nodes"],
        json!([format!("gxe2e-{tag}-h2")]),
        "{d}"
    );
    assert_eq!(d["edge"]["gateway"], lan_gw.as_str(), "{d}");
    eprintln!("edge suggested: {}", d["edge"]);
    inner.ok(
        "PUT",
        "/cluster/settings/ovn",
        Some(&json!({ "enabled": true, "edge": d["edge"] })),
    );
    inner.ok(
        "POST",
        "/networks",
        Some(&json!({ "name": "e2e-nat", "mode": "nat", "port_type": "tap", "scope": "cluster" })),
    );
    h1.check("sudo mkdir -p /var/lib/gxe2e && sudo chmod 0755 /var/lib/gxe2e");
    let out = h1.run_with_input(
        "sudo sh -s",
        ping_seed_script("e2e-c", &lan_gw, "/var/lib/gxe2e/seed-c.img").as_bytes(),
    );
    assert!(
        out.status.success(),
        "seed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let vm = inner.ok("POST", "/vms", Some(&json!({
        "name": "e2e-c", "vcpu_count": 1, "mem_size_mib": 768, "image": env("GLIDEX_E2E_GUEST_IMAGE", "debian-13"), "firmware": "cloudhv-edk2",
        "cloud_init_path": "/var/lib/gxe2e/seed-c.img", "hypervisor": "cloudhypervisor", "node": n1,
        "networks": [{ "network": "e2e-nat" }], "kernel_image_path": "", "rootfs_path": "", "power": "running",
    })));
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let line = wait_for(
        "e2e-c to report its ping to the LAN router",
        Duration::from_secs(600),
        || {
            let out = h1.run(&format!(
                "curl -s --unix-socket {API_SOCKET} http://glidex/vms/{id}/console/log"
            ));
            let log = String::from_utf8_lossy(&out.stdout).replace('\r', "");
            log.lines()
                .find(|l| l.contains("GXE2E-PING-"))
                .map(String::from)
        },
    );
    eprintln!("e2e-c: {line}");
    assert!(line.contains("GXE2E-PING-OK"), "through the edge: {line}");

    // spec/clustering.md §11.2a: the suggestion also gave the edge a pool;
    // a project's own router takes an address from it, and its network's
    // traffic leaves with that address, not the shared edge's.
    step("a project's own router carries its network out with its own address");
    // Room on h1 for one more guest (it has e2e-a and e2e-c).
    inner.ok("DELETE", &format!("/vms/{id}"), None);
    wait_for("e2e-c gone", Duration::from_secs(120), || {
        (inner.call("GET", &format!("/vms/{id}"), None).0 == 404).then_some(())
    });
    let pool = d["edge"]["external_pool"]
        .as_str()
        .unwrap_or_else(|| panic!("no pool suggested: {d}"))
        .to_string();
    let projects = inner.ok("GET", "/projects", None);
    let project = items(&projects, "projects")
        .iter()
        .find(|p| p["name"] == "default")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = inner.ok(
        "POST",
        &format!("/projects/{project}/routers"),
        Some(&json!({ "name": "e2e-r" })),
    );
    let r = &r["router"];
    let rip = r["status"]["external_ip"]
        .as_str()
        .unwrap_or_else(|| panic!("no address: {r}"))
        .to_string();
    eprintln!("router e2e-r: {rip} from {pool}");
    assert_ne!(
        rip,
        d["edge"]["external_ip"].as_str().unwrap_or_default(),
        "{r}"
    );
    inner.ok("POST", &format!("/projects/{project}/networks"), Some(&json!({ "name": "e2e-vpc", "mode": "nat", "port_type": "tap", "scope": "cluster", "router": "e2e-r" })));
    // On the gateway's wire: the ping, from the router's address.
    h2.check(&format!("sudo sh -c 'timeout 600 tcpdump -lni any -c 1 \"icmp and src host {rip}\" > /tmp/gxe2e-vpc.cap 2>/dev/null &'"));
    let out = h1.run_with_input(
        "sudo sh -s",
        ping_seed_script("e2e-d", &lan_gw, "/var/lib/gxe2e/seed-d.img").as_bytes(),
    );
    assert!(
        out.status.success(),
        "seed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let vm = inner.ok("POST", "/vms", Some(&json!({
        "name": "e2e-d", "vcpu_count": 1, "mem_size_mib": 768, "image": env("GLIDEX_E2E_GUEST_IMAGE", "debian-13"), "firmware": "cloudhv-edk2",
        "cloud_init_path": "/var/lib/gxe2e/seed-d.img", "hypervisor": "cloudhypervisor", "node": n1,
        "networks": [{ "network": "e2e-vpc" }], "kernel_image_path": "", "rootfs_path": "", "power": "running",
    })));
    let id = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let console = |id: &str| {
        String::from_utf8_lossy(
            &h1.run(&format!(
                "curl -s --unix-socket {API_SOCKET} http://glidex/vms/{id}/console/log"
            ))
            .stdout,
        )
        .replace('\r', "")
    };
    let line = wait_for_or(
        "e2e-d to report its ping through its router",
        Duration::from_secs(600),
        || {
            console(&id)
                .lines()
                .find(|l| l.contains("GXE2E-PING-"))
                .map(String::from)
        },
        || {
            let tail: Vec<String> = console(&id)
                .lines()
                .rev()
                .take(40)
                .map(String::from)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let nb = h1.run("sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock show; for r in $(sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock --bare --columns=name list logical_router); do echo \"== $r\"; sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock lr-policy-list $r; sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock lr-nat-list $r; sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock lr-route-list $r; done");
            let sbp = sb(&h1, "--columns=logical_port,chassis,up list port_binding");
            format!(
                "vm: {}\nconsole tail:\n{}\nnb:\n{}\nsb ports:\n{sbp}",
                inner.call("GET", &format!("/vms/{id}"), None).1,
                tail.join("\n"),
                String::from_utf8_lossy(&nb.stdout)
            )
        },
    );
    eprintln!("e2e-d: {line}");
    assert!(line.contains("GXE2E-PING-OK"), "through its router: {line}");
    let cap = wait_for("the ping on h2's wire", Duration::from_secs(30), || {
        let c = h2.check("cat /tmp/gxe2e-vpc.cap");
        c.contains(&rip).then_some(c)
    });
    eprintln!("on h2: {}", cap.trim());

    // An existing network moves onto the router: its port leaves the edge,
    // and its guests then reach the router's other networks.
    step(
        "existing networks attach to the router: a NAT one moves off the edge, an isolated one gets routed",
    );
    let d_ip = line
        .split("my address:")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_default()
        .to_string();
    let nb = |args: &str| {
        h1.check(&format!(
            "sudo ovn-nbctl --no-leader-only --db=unix:/var/run/ovn/ovnnb_db.sock {args}"
        ))
    };
    let rlr = format!("gxr-{}", r["id"].as_str().unwrap());
    // A NAT network on the edge: its port moves to the router.
    // A new network's OVN objects are named after its id (`gx-<id>`,
    // `gx-lrp-<id>`), not its name (spec/tenancy.md D24).
    let p2 = inner.ok(
        "POST",
        &format!("/projects/{project}/networks"),
        Some(&json!({ "name": "e2e-p2", "mode": "nat", "port_type": "tap", "scope": "cluster" })),
    );
    let p2_lrp = format!("gx-lrp-{}", p2["network"]["id"].as_str().unwrap());
    wait_for("e2e-p2's port on the edge", Duration::from_secs(60), || {
        nb("lrp-list gx-edge").contains(&p2_lrp).then_some(())
    });
    inner.ok(
        "PUT",
        &format!("/projects/{project}/networks/e2e-p2/router"),
        Some(&json!({ "router": "e2e-r" })),
    );
    wait_for(
        "e2e-p2's port on the router",
        Duration::from_secs(60),
        || {
            (nb(&format!("lrp-list {rlr}")).contains(&p2_lrp)
                && !nb("lrp-list gx-edge").contains(&p2_lrp))
            .then_some(())
        },
    );
    // An isolated network: no router port until it is attached; then a guest
    // on it, on the other node, reaches the router's other network.
    let p3 = inner.ok("POST", &format!("/projects/{project}/networks"), Some(&json!({ "name": "e2e-p3", "mode": "isolated", "port_type": "tap", "scope": "cluster" })));
    let p3_id = p3["network"]["id"].as_str().unwrap().to_string();
    let p3_lrp = format!("gx-lrp-{p3_id}");
    wait_for("e2e-p3's switch", Duration::from_secs(60), || {
        nb("ls-list").contains(&format!("gx-{p3_id}")).then_some(())
    });
    assert!(!nb(&format!("lrp-list {rlr}")).contains(&p3_lrp));
    inner.ok(
        "PUT",
        &format!("/projects/{project}/networks/e2e-p3/router"),
        Some(&json!({ "router": "e2e-r" })),
    );
    let dhcp_router = || {
        nb(&format!(
            "--bare --columns=options find DHCP_Options external_ids:glidex-network={p3_id}"
        ))
        .contains("router=")
    };
    wait_for(
        "e2e-p3 routed: a port on the router, a gateway in DHCP",
        Duration::from_secs(60),
        || (nb(&format!("lrp-list {rlr}")).contains(&p3_lrp) && dhcp_router()).then_some(()),
    );
    let out = h2.run_with_input(
        "sudo sh -s",
        ping_seed_script("e2e-e", &d_ip, "/var/lib/gxe2e/seed-e.img").as_bytes(),
    );
    assert!(
        out.status.success(),
        "seed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let vm = inner.ok("POST", "/vms", Some(&json!({
        "name": "e2e-e", "vcpu_count": 1, "mem_size_mib": 768, "image": env("GLIDEX_E2E_GUEST_IMAGE", "debian-13"), "firmware": "cloudhv-edk2",
        "cloud_init_path": "/var/lib/gxe2e/seed-e.img", "hypervisor": "cloudhypervisor", "node": n2,
        "networks": [{ "network": "e2e-p3" }], "kernel_image_path": "", "rootfs_path": "", "power": "running",
    })));
    let eid = vm_of(&vm)["id"].as_str().unwrap().to_string();
    let line = wait_for(
        "e2e-e to report its ping to e2e-d",
        Duration::from_secs(600),
        || {
            let out = h1.run(&format!(
                "curl -s --unix-socket {API_SOCKET} http://glidex/vms/{eid}/console/log"
            ));
            String::from_utf8_lossy(&out.stdout)
                .replace('\r', "")
                .lines()
                .find(|l| l.contains("GXE2E-PING-"))
                .map(String::from)
        },
    );
    eprintln!("e2e-e: {line}");
    assert!(
        line.contains("GXE2E-PING-OK"),
        "from an isolated network, across the router: {line}"
    );
    // Detached, it is isolated again: no port on the router, no gateway.
    inner.ok(
        "PUT",
        &format!("/projects/{project}/networks/e2e-p3/router"),
        Some(&json!({ "router": null })),
    );
    wait_for("e2e-p3 isolated again", Duration::from_secs(60), || {
        (!nb(&format!("lrp-list {rlr}")).contains(&p3_lrp) && !dhcp_router()).then_some(())
    });

    // spec/clustering.md §5.5: the agent is removed from the cluster's side,
    // and its host leaves on its own (an agent has no API of its own, and
    // the servers refuse it once removed): standalone, local login, out of OVN.
    step("a removed agent leaves the cluster and is a standalone host again");
    // Empty it first: no VM, and nothing that needs it as a gateway.
    let vms = inner.ok("GET", "/vms", None);
    for v in items(&vms, "vms") {
        inner.ok(
            "DELETE",
            &format!("/vms/{}", v["id"].as_str().unwrap()),
            None,
        );
    }
    wait_for("no VMs left", Duration::from_secs(180), || {
        items(&inner.ok("GET", "/vms", None), "vms")
            .is_empty()
            .then_some(())
    });
    // Copies on chosen nodes (clustering.md §9.2.2): h1 lets its copy of the
    // guest image go (h2 has one), then gets it back from h2.
    let images = inner.ok("GET", "/images", None);
    let guest_img = items(&images, "images")
        .iter()
        .find(|i| i["kind"] == "IMAGE_KIND_DISK")
        .map(|i| i["id"].as_str().unwrap().to_string())
        .expect("the guest image");
    let copy_on = |node: &str| -> Option<String> {
        let v = inner.ok("GET", &format!("/images/{guest_img}"), None);
        v["image"]["copies"]
            .as_array()?
            .iter()
            .find(|c| c["node"] == node)
            .map(|c| c["state"].as_str().unwrap_or_default().to_string())
    };
    wait_for(
        "h2's copy of the guest image",
        Duration::from_secs(60),
        || (copy_on(&n2).as_deref() == Some("ready")).then_some(()),
    );
    inner.ok("DELETE", &format!("/images/{guest_img}/copies/{n1}"), None);
    wait_for("h1's copy gone", Duration::from_secs(120), || {
        copy_on(&n1).is_none().then_some(())
    });
    inner.ok(
        "POST",
        &format!("/images/{guest_img}/copies"),
        Some(&json!({ "node": n1 })),
    );
    wait_for("h1's copy back, from h2", Duration::from_secs(600), || {
        (copy_on(&n1).as_deref() == Some("ready")).then_some(())
    });
    let nets = inner.ok("GET", "/networks", None);
    for n in items(&nets, "networks") {
        inner.call(
            "DELETE",
            &format!("/networks/{}", n["id"].as_str().unwrap()),
            None,
        );
    }
    wait_for("no networks left", Duration::from_secs(180), || {
        items(&inner.ok("GET", "/networks", None), "networks")
            .is_empty()
            .then_some(())
    });
    inner.ok(
        "DELETE",
        &format!("/projects/{project}/routers/e2e-r"),
        None,
    );
    inner.ok(
        "PUT",
        "/cluster/settings/ovn",
        Some(&json!({ "enabled": true, "edge": null })),
    );
    let chassis2 = format!("node:{n2}");
    let removed = inner.call("POST", &format!("/nodes/{n2}/remove"), Some(&json!({})));
    assert_eq!(removed.0, 200, "remove {n2}: {}", removed.1);
    // On the host: the installer leaves offline, once a server confirms it.
    let out = h2.run(&format!(
        "{root}/target/release/glidex-install --leave 2>&1",
        root = shell_quote(&workspace_root().display().to_string())
    ));
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "glidex-install --leave on h2:\n{log}");
    eprintln!(
        "{}",
        log.lines()
            .filter(|l| l.contains("Left the cluster") || l.contains("OVN chassis"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let status = wait_for("h2's own API", Duration::from_secs(60), || {
        let o = h2.run("gxctl cluster status 2>&1");
        let t = String::from_utf8_lossy(&o.stdout).into_owned();
        t.contains("standalone").then_some(t)
    });
    eprintln!("h2: {}", status.trim());
    assert!(
        String::from_utf8_lossy(&h2.run("gxctl whoami 2>&1").stdout).contains("Break-glass"),
        "local login works again on h2"
    );
    let ids = h2.check("sudo ovs-vsctl get open . external_ids");
    assert!(
        !ids.contains("ovn-remote") && !ids.contains("system-id"),
        "h2 left OVN: {ids}"
    );
    assert!(
        !h2.run("systemctl is-active --quiet ovn-controller")
            .status
            .success(),
        "ovn-controller stopped on h2"
    );
    wait_for(
        "h2's chassis gone from the cluster",
        Duration::from_secs(120),
        || (!sb(&h1, "--bare --columns=name list Chassis").contains(&chassis2)).then_some(()),
    );
    drop(outer);
}

/// One request to a glidex-ui from this host: (status, headers, body). The
/// body goes through stdin, so a password never reaches a command line.
fn ui_curl(
    jar: &std::path::Path,
    base: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    stream_secs: Option<u32>,
) -> (u16, String, String) {
    ui_curl_csrf(jar, base, method, path, body, stream_secs, None)
}

/// As `ui_curl`, with the session's CSRF value (writes need it).
fn ui_curl_csrf(
    jar: &std::path::Path,
    base: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    stream_secs: Option<u32>,
    csrf: Option<&str>,
) -> (u16, String, String) {
    ui_curl_on(jar, base, method, path, body, stream_secs, csrf, None)
}

/// As `ui_curl_csrf`, for node `target`'s host (`X-Glidex-Target-Node`,
/// spec/host-page.md W2).
#[allow(clippy::too_many_arguments)]
fn ui_curl_on(
    jar: &std::path::Path,
    base: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    stream_secs: Option<u32>,
    csrf: Option<&str>,
    target: Option<&str>,
) -> (u16, String, String) {
    let mut c = Command::new("curl");
    c.args(["-s", "-k", "-i", "-X", method, "-b"])
        .arg(jar)
        .arg("-c")
        .arg(jar)
        .args(["-H", &format!("Origin: {base}")]);
    if let Some(x) = csrf {
        c.args(["-H", &format!("X-Glidex-CSRF: {x}")]);
    }
    if let Some(t) = target {
        c.args(["-H", &format!("X-Glidex-Target-Node: {t}")]);
    }
    if let Some(t) = stream_secs {
        c.args(["-N", "--max-time", &t.to_string()]);
    } else {
        c.args(["--max-time", "30"]);
    }
    if body.is_some() {
        c.args([
            "-H",
            "content-type: application/json",
            "--data-binary",
            "@-",
        ]);
    }
    c.arg(format!("{base}{path}"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
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
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, head.to_ascii_lowercase(), rest.to_string())
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .find_map(|l| l.strip_prefix(&format!("{name}: ")))
        .map(str::trim)
}

/// spec/host-page.md: h2's (the agent's) glidex-ui sends browsers to h1's
/// (the server's), at h2's Host page, and serves no API. The test's user,
/// made a PAM user with a throwaway password on h1, logs in there; h1's
/// UI runs host-only requests on h2 when they name it (W2).
fn check_agent_ui(
    h1: &Ssh,
    h2: &Ssh,
    user: &str,
    dir: &std::path::Path,
    n1: &str,
    n2: &str,
    h2_name: &str,
) -> (std::path::PathBuf, String, String) {
    let agent = format!("https://{}:5173", h2.host);
    let jar = dir.join("ui-cookies");
    // The agent learns h1's published address within glidex-ui's report
    // interval (30 s), then redirects.
    let location = wait_for(
        "h2's web UI to send browsers to a server",
        Duration::from_secs(120),
        || {
            let (s, head, _) = ui_curl(&jar, &agent, "GET", "/", None, None);
            (s == 307)
                .then(|| header(&head, "location").map(String::from))
                .flatten()
        },
    );
    assert!(
        location.starts_with("https://") && location.ends_with(&format!("/hosts/{n2}")),
        "to h2's Host page on a server: {location}"
    );
    let (s, head, _) = ui_curl(&jar, &agent, "GET", "/vms/x?tab=disks", None, None);
    assert_eq!(s, 307);
    assert!(
        header(&head, "location").is_some_and(|l| l.ends_with("/vms/x?tab=disks")),
        "the path is kept: {head}"
    );
    let (s, _, body) = ui_curl(&jar, &agent, "GET", "/api/vms", None, None);
    assert_eq!(s, 421, "the agent serves no API: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["error"], "use_a_server", "{v}");
    let server_ui = location
        .trim_end_matches(&format!("/hosts/{n2}"))
        .to_string();
    assert_eq!(v["details"]["location"], server_ui.as_str(), "{v}");

    // The published address is h1's name, which the test runner may not
    // resolve: talk to h1's UI by its address, as the browser would by name.
    let base = format!("https://{}:5173", h1.host);
    let password = uuid::Uuid::new_v4().simple().to_string();
    let out = h1.run_with_input("sudo chpasswd", format!("{user}:{password}\n").as_bytes());
    assert!(out.status.success(), "setting a login password on h1");
    wait_for("h1's web UI", Duration::from_secs(60), || {
        (ui_curl(&jar, &base, "GET", "/api/health", None, None).0 == 200).then_some(())
    });
    let (s, _, body) = ui_curl(
        &jar,
        &base,
        "POST",
        "/api/auth/login",
        Some(&json!({ "method": "pam", "username": user, "password": password })),
        None,
    );
    drop(password);
    assert_eq!(
        s,
        200,
        "PAM login on the server's UI: {}",
        body.chars().take(300).collect::<String>()
    );
    let csrf = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["csrf"].as_str().map(String::from))
        .unwrap_or_default();

    let (s, _, body) = ui_curl(&jar, &base, "GET", "/api/cluster/status", None, None);
    assert_eq!(s, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["node_id"], n1, "this node is the server: {v}");

    let (s, head, _) = ui_curl_on(
        &jar,
        &base,
        "GET",
        "/api/system/reconcile",
        None,
        None,
        None,
        Some(n2),
    );
    assert_eq!(s, 200);
    assert_eq!(
        header(&head, "x-glidex-served-by"),
        Some(n2),
        "host-only reads naming the agent run there"
    );
    let (s, head, _) = ui_curl(&jar, &base, "GET", "/api/system/reconcile", None, None);
    assert_eq!(s, 200);
    assert_eq!(
        header(&head, "x-glidex-served-by"),
        None,
        "and otherwise on the server"
    );

    let (s, _, body) = ui_curl(&jar, &base, "GET", "/api/vms", None, None);
    assert_eq!(s, 200, "{body}");
    let vms: Value = serde_json::from_str(&body).unwrap();
    let b = items(&vms, "vms")
        .iter()
        .find(|v| v["name"] == "e2e-b")
        .expect("e2e-b listed");
    assert_eq!(b["node"], n2);
    assert_eq!(b["node_name"], h2_name);
    (jar, base, csrf)
}

/// spec/clustering-ui.md (uplinks): from the server's UI, naming the agent
/// (spec/host-page.md W2), put its only NIC on a bridge with its address,
/// reboot it, and find it reachable with the address re-applied on the
/// bridge (netd's start-up re-apply).
fn check_single_nic_uplink_survives_a_reboot(
    h2: &Ssh,
    n2: &str,
    ui: &(std::path::PathBuf, String, String),
) {
    let (jar, base, csrf) = ui;
    let on = Some(n2);
    let nic = h2.check("ip -j route show default | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"dev\"])'").trim().to_string();
    let addr = h2.check(&format!("ip -4 -j addr show dev {nic} | python3 -c 'import json,sys; print(json.load(sys.stdin)[0][\"addr_info\"][0][\"local\"])'")).trim().to_string();
    let (s, _, b) = ui_curl_on(
        jar,
        base,
        "POST",
        "/api/ovs/bridges",
        Some(&json!({ "name": "gxbr-up", "datapath": "system" })),
        None,
        Some(csrf),
        on,
    );
    assert!(s == 200 || s == 201, "bridge on the agent: {s} {b}");
    // A DHCP-managed address is refused unless accepted (the nested VMs use DHCP).
    let body = json!({ "name": nic, "kind": "kernel", "ifname": nic, "migrate_ip": true, "confirm": true });
    let (s, _, b) = ui_curl_on(
        jar,
        base,
        "POST",
        "/api/ovs/bridges/gxbr-up/uplinks",
        Some(&body),
        None,
        Some(csrf),
        on,
    );
    assert_eq!(s, 409, "a DHCP address needs an explicit yes: {b}");
    let mut body = body;
    body["accept_dhcp"] = json!(true);
    let (s, _, b) = ui_curl_on(
        jar,
        base,
        "POST",
        "/api/ovs/bridges/gxbr-up/uplinks",
        Some(&body),
        None,
        Some(csrf),
        on,
    );
    assert_eq!(s, 202, "pending commit: {b}");
    let res: Value = serde_json::from_str(&b).unwrap();
    let token = res["uplink"]["record"]["pending"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, _, b) = ui_curl_on(
        jar,
        base,
        "POST",
        &format!("/api/ovs/bridges/gxbr-up/uplinks/{nic}/commit"),
        Some(&json!({ "token": token })),
        None,
        Some(csrf),
        on,
    );
    assert_eq!(s, 200, "commit: {b}");
    assert!(
        h2.check("ip -4 -j addr show dev gxbr-up").contains(&addr),
        "the address is on the bridge"
    );

    let _ = h2.run("sudo systemctl reboot");
    std::thread::sleep(Duration::from_secs(15));
    wait_for("h2 back after the reboot", Duration::from_secs(300), || {
        h2.reachable().then_some(())
    });
    wait_for(
        "netd to re-apply the address on the bridge",
        Duration::from_secs(120),
        || {
            h2.check("ip -4 -j addr show dev gxbr-up || true")
                .contains(&addr)
                .then_some(())
        },
    );
    let up = h2.check("sudo ovs-vsctl list-ports gxbr-up");
    assert!(
        up.lines().any(|l| l.trim() == nic),
        "{nic} is back on the bridge: {up}"
    );
    let b = wait_for(
        "the server to reach h2 again",
        Duration::from_secs(120),
        || {
            let (s, _, b) = ui_curl_on(
                jar,
                base,
                "GET",
                "/api/ovs/bridges/gxbr-up/uplinks",
                None,
                None,
                None,
                on,
            );
            (s == 200).then_some(b)
        },
    );
    let list: Value = serde_json::from_str(&b).unwrap();
    assert!(
        list["uplinks"][0]["record"].is_object()
            && list["uplinks"][0]["record"]["boot_rollback"].is_null(),
        "not held: {list}"
    );
    let _ = std::fs::remove_file(jar);
}

/// The control plane's warnings, errors and cluster-link changes, newest last.
const LOG_PROBLEMS: &str = "sudo journalctl -u glidex-control-plane --no-pager -o short-iso | grep -E 'WARN|ERROR|cluster servers|liveness|disk created|image (ready|copied)' | grep -v ovn_dbctl | tail -n 80 | cut -c1-300";

fn tempfile_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("glidex-e2e-{tag}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}
