//! VM instances from the control plane's side (spec/reconciliation.md §8):
//! where the shim runs ([`runner`]) and whether an instance is live
//! ([`liveness`]).

pub mod runner;

use crate::models::{InstanceRef, Vm};
use glidex_vm_shim::state::InstanceFile;
use glidex_vm_shim::util::{boot_id, same_process, socket_accepts};
use std::path::Path;

/// The answer to "is there an instance of this VM?" (§8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    Live,
    /// Every applicable check was made and none showed life.
    Dead,
    /// A check could not be made; never act on this.
    Unknown(String),
}

/// What the liveness checks saw, for the VM controller.
#[derive(Debug, Clone)]
pub struct Seen {
    pub liveness: Liveness,
    /// The shim's `instance.json`, if any.
    pub file: Option<InstanceFile>,
    /// This boot's id.
    pub boot_id: String,
}

/// §8.7. `unit` is the systemd unit's state (`Some(Ok(true))` = active,
/// activating, deactivating or reloading); `None` for the detached runner.
/// Blocking (reads `/proc` and connects to sockets).
pub fn liveness(vm: &Vm, unit: Option<Result<bool, String>>) -> Seen {
    let paths = vm.paths();
    let boot = match boot_id() {
        Ok(b) => b,
        Err(e) => {
            return Seen { liveness: Liveness::Unknown(format!("boot id: {}", e)), file: None, boot_id: String::new() }
        }
    };
    let file = match InstanceFile::read(&paths.instance) {
        Ok(f) => f,
        Err(e) => {
            return Seen { liveness: Liveness::Unknown(format!("{}: {}", paths.instance.display(), e)), file: None, boot_id: boot }
        }
    };
    let liveness = check(vm.status.instance.as_ref(), file.as_ref(), &boot, unit, &paths.api_socket, &paths.shim_socket);
    Seen { liveness, file, boot_id: boot }
}

fn check(
    inst: Option<&InstanceRef>,
    file: Option<&InstanceFile>,
    boot: &str,
    unit: Option<Result<bool, String>>,
    api_socket: &str,
    shim_socket: &str,
) -> Liveness {
    // 1. the unit
    match unit {
        Some(Ok(true)) => return Liveness::Live,
        Some(Err(e)) => return Liveness::Unknown(format!("systemd: {}", e)),
        Some(Ok(false)) | None => {}
    }
    // 2. and 3. the shim and hypervisor processes, this boot only
    let mut procs: Vec<(u32, u64)> = Vec::new();
    if let Some(i) = inst.filter(|i| i.boot_id == boot) {
        procs.extend(i.shim_pid.zip(i.shim_starttime));
        procs.extend(i.hypervisor_pid.zip(i.hypervisor_starttime));
    }
    if let Some(f) = file.filter(|f| f.boot_id == boot) {
        procs.push((f.shim_pid, f.shim_starttime));
        procs.extend(f.hypervisor_pid.zip(f.hypervisor_starttime));
    }
    for (pid, start) in procs {
        match same_process(pid, start) {
            Ok(true) => return Liveness::Live,
            Ok(false) => {}
            Err(e) => return Liveness::Unknown(format!("/proc/{}: {}", pid, e)),
        }
    }
    // 4. the sockets
    for sock in [api_socket, shim_socket] {
        match socket_accepts(Path::new(sock)) {
            Ok(true) => return Liveness::Live,
            Ok(false) => {}
            Err(e) => return Liveness::Unknown(format!("{}: {}", sock, e)),
        }
    }
    Liveness::Dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Runner;
    use glidex_vm_shim::state::Phase;

    fn inst(pid: u32, start: u64, boot: &str) -> InstanceRef {
        InstanceRef {
            instance_id: "i".into(),
            runner: Runner::Detached,
            boot_id: boot.into(),
            launched_generation: 1,
            disks: vec![],
            vfio_devices: vec![],
            shim_pid: Some(pid),
            shim_starttime: Some(start),
            hypervisor_pid: None,
            hypervisor_starttime: None,
            launched_at: 0,
            growpart_disk: None,
        }
    }

    fn file(pid: u32, start: u64, boot: &str) -> InstanceFile {
        InstanceFile {
            version: 1,
            vm_id: "v".into(),
            instance_id: "i".into(),
            boot_id: boot.into(),
            shim_pid: pid,
            shim_starttime: start,
            hypervisor_pid: None,
            hypervisor_starttime: None,
            phase: Phase::Running,
            launched_at: 0,
            stop: None,
            exit: None,
        }
    }

    #[test]
    fn each_check_alone_shows_life() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("api.sock");
        let shim = dir.path().join("shim.sock");
        let (a, s) = (api.to_str().unwrap(), shim.to_str().unwrap());
        let me = std::process::id();
        let st = glidex_vm_shim::util::proc_starttime(me).unwrap().unwrap();

        assert_eq!(check(None, None, "b", None, a, s), Liveness::Dead);
        assert_eq!(check(None, None, "b", Some(Ok(false)), a, s), Liveness::Dead);
        // 1. unit
        assert_eq!(check(None, None, "b", Some(Ok(true)), a, s), Liveness::Live);
        assert!(matches!(check(None, None, "b", Some(Err("no bus".into())), a, s), Liveness::Unknown(_)));
        // 2./3. processes, recorded or from instance.json, this boot only
        assert_eq!(check(Some(&inst(me, st, "b")), None, "b", None, a, s), Liveness::Live);
        assert_eq!(check(None, Some(&file(me, st, "b")), "b", None, a, s), Liveness::Live);
        assert_eq!(check(Some(&inst(me, st, "old")), Some(&file(me, st, "old")), "b", None, a, s), Liveness::Dead);
        assert_eq!(check(Some(&inst(me, st + 1, "b")), None, "b", None, a, s), Liveness::Dead);
        // 4. sockets
        let l = std::os::unix::net::UnixListener::bind(&api).unwrap();
        assert_eq!(check(None, None, "b", None, a, s), Liveness::Live);
        drop(l);
        assert_eq!(check(None, None, "b", None, a, s), Liveness::Dead);
    }
}
