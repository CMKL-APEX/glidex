//! The systemd VM runner against a real systemd: the user's own manager
//! (`GLIDEX_SYSTEMD_BUS=session`), so no root and no polkit rule are
//! needed. Covers StartUnit + JobRemoved, the unit's ActiveState in the
//! liveness checks, adoption by a restarted control plane, a requested
//! stop, and a unit stopped outside glidex (`Terminated`, spec
//! reconciliation.md §7.5).
//!
//! Ignored by default: it boots a real guest. Needs a running user systemd
//! manager, KVM, cloud-hypervisor, a built `glidex-vm-shim` and
//! `GLIDEX_TEST_IMAGE`. Its own test binary, because it sets process-wide
//! environment variables.
//!
//! `GLIDEX_TEST_SYSTEMD=system` runs it against the system manager
//! instead, through polkit, with a `glidex-vm@.service` and polkit rule the
//! operator installed for the test user (their run directory given as
//! `GLIDEX_TEST_RUN_DIR`). That also checks the polkit `kill` verb (F6).

use axum::{body::Body, http::Request, http::StatusCode, Router};
use glidex_control_plane::api::create_router;
use glidex_control_plane::models::{ExitCause, PowerState, Runner};
use glidex_control_plane::state::VmManager;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tower::ServiceExt;

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())).unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn user_systemctl(args: &[&str]) -> String {
    let out = Command::new("systemctl").arg("--user").args(args).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The user unit, removed (with any instance) when the test ends.
struct UserUnit(PathBuf);

impl Drop for UserUnit {
    fn drop(&mut self) {
        let _ = Command::new("systemctl").args(["--user", "stop", "glidex-vm@*.service"]).status();
        let _ = std::fs::remove_file(&self.0);
        let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    }
}

fn shim_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let p = exe.parent().unwrap().parent().unwrap().join("glidex-vm-shim");
    assert!(p.exists(), "build the shim first: cargo build -p glidex-vm-shim");
    p
}

async fn manager_on(db: &Path) -> (Router, Arc<VmManager>) {
    let manager = {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match VmManager::with_db_path(db.to_path_buf()) {
                Ok(m) => break m,
                Err(_) if Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(200)).await,
                Err(e) => panic!("{e}"),
            }
        }
    };
    let mut cfg = glidex_control_plane::config::Config::default();
    cfg.reconcile.vm_runner = glidex_control_plane::config::VmRunnerKind::Systemd;
    // Shorter than the test unit's TimeoutStopSec (30 s), as the shipped
    // unit's 330 s covers the 300 s maximum.
    cfg.reconcile.host_shutdown_grace_secs = 5;
    manager.configure(&cfg);
    manager.initialize().await.unwrap();
    manager.start_controllers();
    (create_router(manager.clone()), manager)
}

async fn wait_vm(m: &VmManager, id: &str, what: &str, f: impl Fn(&glidex_control_plane::models::Vm) -> bool) -> glidex_control_plane::models::Vm {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let vm = m.get_vm(id).await.unwrap();
        if f(&vm) {
            return vm;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", vm.status);
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "boots a real VM under the user's systemd; needs KVM, cloud-hypervisor, a built glidex-vm-shim and GLIDEX_TEST_IMAGE"]
async fn systemd_runner_launches_adopts_and_stops() {
    let image = std::env::var("GLIDEX_TEST_IMAGE").expect("set GLIDEX_TEST_IMAGE");
    let firmware = glidex_control_plane::hypervisor::cloud_hypervisor::default_firmware_path().unwrap();
    // Not under /tmp: the systemd runner refuses /tmp paths (VM units
    // have a private /tmp, spec §8.2).
    let tmp = TempDir::new_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let system = std::env::var("GLIDEX_TEST_SYSTEMD").as_deref() == Ok("system");
    let run_dir = match std::env::var("GLIDEX_TEST_RUN_DIR") {
        Ok(d) if system => PathBuf::from(d),
        _ => tmp.path().join("run"),
    };
    std::fs::create_dir_all(&run_dir).unwrap();
    // SAFETY: this test binary has a single test; nothing reads the
    // environment concurrently.
    unsafe {
        std::env::set_var("GLIDEX_RUN_DIR", &run_dir);
        std::env::set_var("GLIDEX_VM_UNIT_RUN_DIR", &run_dir);
        if !system {
            std::env::set_var("GLIDEX_SYSTEMD_BUS", "session");
        }
    }
    let systemctl = |args: &[&str]| -> String {
        if system {
            let out = Command::new("systemctl").args(args).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        } else {
            user_systemctl(args)
        }
    };

    let _unit = if system {
        assert!(Path::new("/etc/systemd/system/glidex-vm@.service").exists(), "install the test unit first");
        None
    } else {
        let unit_dir = dirs::home_dir().unwrap().join(".config/systemd/user");
        std::fs::create_dir_all(&unit_dir).unwrap();
        let unit_path = unit_dir.join("glidex-vm@.service");
        assert!(!unit_path.exists(), "{} exists; not overwriting it", unit_path.display());
        std::fs::write(
            &unit_path,
            format!(
                "[Unit]\nDescription=glidex VM %i (test)\n\n[Service]\nType=notify\nNotifyAccess=main\n\
                 ExecStart={} --vm %i --dir {}/vms/%i\nTimeoutStartSec=60\nKillMode=mixed\nTimeoutStopSec=30\nRestart=no\n",
                shim_binary().display(),
                run_dir.display()
            ),
        )
        .unwrap();
        let u = UserUnit(unit_path);
        user_systemctl(&["daemon-reload"]);
        Some(u)
    };

    let rootfs = tmp.path().join("root.raw");
    assert!(Command::new("qemu-img").args(["convert", "-O", "raw", &image]).arg(&rootfs).status().unwrap().success());
    let db = tmp.path().join("cp.db");

    // Launch through StartUnit; the unit is the instance.
    let (app, manager) = manager_on(&db).await;
    let (status, vm) = request(&app, "POST", "/vms?wait=120", Some(json!({
        "name": "sd-test", "vcpu_count": 1, "mem_size_mib": 1024, "firmware_path": firmware,
        "rootfs_path": rootfs, "power": "running",
    })))
    .await;
    assert_eq!(status, StatusCode::CREATED, "{vm}");
    assert_eq!(vm["state"], "running", "{vm}");
    let id = vm["id"].as_str().unwrap().to_string();
    let unit = format!("glidex-vm@{id}.service");
    assert_eq!(systemctl(&["is-active", &unit]), "active");
    let inst = manager.get_vm(&id).await.unwrap().status.instance.unwrap();
    assert_eq!(inst.runner, Runner::Systemd { unit: unit.clone() });

    // A restarted control plane adopts it.
    manager.stop_controllers().await;
    drop(app);
    drop(manager);
    let (app, manager) = manager_on(&db).await;
    let vm = wait_vm(&manager, &id, "adoption", |v| v.is_converged()).await;
    assert_eq!(vm.status.instance.as_ref().unwrap().instance_id, inst.instance_id);

    // A requested stop: the unit goes away with the instance.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/stop?wait=60"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "stopped");
    assert_ne!(systemctl(&["is-active", &unit]), "active");
    let vm = manager.get_vm(&id).await.unwrap();
    assert_eq!(vm.status.last_exit.unwrap().cause, ExitCause::Requested);

    // The last-resort escalation: KillUnit (polkit verb "kill" in system
    // mode). The shim dies without recording an exit: a crash, restarted.
    let (status, body) = request(&app, "POST", &format!("/vms/{id}/start?wait=120"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let first = manager.get_vm(&id).await.unwrap().status.instance.unwrap().instance_id;
    manager.runner().kill(&id, &[]).await.expect("KillUnit");
    let vm = wait_vm(&manager, &id, "restart after KillUnit", |v| {
        v.status.instance.as_ref().is_some_and(|i| i.instance_id != first) && v.is_converged()
    })
    .await;
    assert!(matches!(vm.status.last_exit.as_ref().unwrap().cause, ExitCause::Lost | ExitCause::Crashed), "{:?}", vm.status.last_exit);
    assert_eq!(vm.status.restart_count, 1);

    // Stopped outside glidex (as at host shutdown): Terminated, and the
    // desired state follows (D12).
    if system {
        // As root, as systemd itself does at shutdown.
        assert!(Command::new("sudo").args(["-n", "systemctl", "stop", &unit]).status().unwrap().success());
    } else {
        systemctl(&["stop", &unit]);
    }
    let vm = wait_vm(&manager, &id, "terminated", |v| v.spec.power == PowerState::Stopped && v.is_converged()).await;
    assert_eq!(vm.status.last_exit.unwrap().cause, ExitCause::Terminated);
    let events = manager.vm_events(&id).unwrap();
    assert!(events.iter().any(|e| e.actor == "systemd" && e.reason == "Terminated"), "{events:?}");

    let (status, _) = request(&app, "DELETE", &format!("/vms/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    manager.stop_controllers().await;
}
