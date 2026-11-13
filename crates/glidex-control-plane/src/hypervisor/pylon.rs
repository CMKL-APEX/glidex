//! Pylon: microsecond-scale core elasticity hypervisor backend.
//!
//! Pylon (CMKL-APEX/pylon) is a Cloud Hypervisor fork with microsecond physical
//! CPU arbitration via shared memory and sched_ext (scx_pylon). It uses the
//! same REST API over `--api-socket` as Cloud Hypervisor (driving it through
//! `ChClient`), but appends `--pylon` with its shared memory page and pylond
//! socket.

use super::{
    HypervisorDriver, HypervisorError, HypervisorType, LaunchArgs, cloud_hypervisor,
    resolve_binary, vfio_device_id,
};
use crate::models::VmConfig;
use glidex_hv_client::{Observed, ch::ChClient};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Derive a deterministic 64-bit VM ID for Pylon from a VM UUID string.
pub fn pylon_vm_id_from_uuid(uuid_str: &str) -> u64 {
    uuid::Uuid::parse_str(uuid_str)
        .ok()
        .map(|u| {
            let (high, low) = u.as_u64_pair();
            high ^ low
        })
        .filter(|&id| id != 0)
        .unwrap_or(1)
}

/// Derive a deterministic 64-bit VM ID for Pylon from the VM directory or UUID.
pub fn pylon_vm_id(api_socket: &str) -> u64 {
    let p = Path::new(api_socket);
    p.parent()
        .and_then(|d| d.file_name())
        .and_then(|s| s.to_str())
        .map(pylon_vm_id_from_uuid)
        .unwrap_or(1)
}

/// Default pylond socket path on the host.
pub const DEFAULT_PYLOND_SOCKET: &str = "/run/pylon/pylond.sock";

/// Pylon's guest agent, installed by `glidex-install --with-pylon`; it goes
/// into Pylon guests' seed disks (spec/30-pylon.md P19).
pub const PYLON_GUEST_AGENT: &str = "/usr/local/share/glidex/pylon-guest-agent";

/// Where the kernel says which `sched_ext` scheduler is attached.
const SCHED_EXT_SYSFS: &str = "/sys/kernel/sched_ext";

/// Where the VMM parks an idle vCPU (`--pylon park=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Park {
    /// Inside KVM: faster, but `pylond` accepts it only with `scx_pylon`.
    Kvm,
    /// In the VMM's vCPU thread: works without `scx_pylon`.
    Vmm,
}

impl Park {
    fn as_str(self) -> &'static str {
        match self {
            Park::Kvm => "kvm",
            Park::Vmm => "vmm",
        }
    }
}

/// `park=kvm` only where `scx_pylon` is attached: `pylond` refuses a VM
/// that parks in KVM without it, and the VM then runs unmanaged
/// (spec/30-pylon.md P15).
pub(crate) fn park_mode_here() -> Park {
    park_mode(Path::new(SCHED_EXT_SYSFS))
}

pub(crate) fn park_mode(sched_ext: &Path) -> Park {
    let read = |f: &str| std::fs::read_to_string(sched_ext.join(f)).unwrap_or_default();
    if read("state").trim() == "enabled" && read("root/ops").trim() == "pylon" {
        Park::Kvm
    } else {
        Park::Vmm
    }
}

/// Build the command line arguments for Pylon.
pub(crate) fn args(config: &VmConfig, api_socket: &str, park: Park) -> Vec<String> {
    let mut a = cloud_hypervisor::args(config, api_socket);

    let max_vcpus = config.effective_max_vcpus();
    let min_cores = config.effective_min_vcpus();
    let priority_weight = config.effective_cpu_weight();
    // Start at the floor only where a parked vCPU wakes on an interrupt
    // (park=kvm, with scx_pylon): pylond gives a managed CPU only to a
    // parked vCPU, but with park=vmm a guest whose vCPUs the agent's first
    // heartbeat parked stalls on work sent to them, and the agent can't
    // report demand to wake them (seen on prism2, 2026-11-13). Elsewhere
    // every vCPU starts and stays active (spec/30-pylon.md P18).
    let initial_active_vcpus = match park {
        Park::Kvm => min_cores.max(1),
        Park::Vmm => max_vcpus,
    };

    // On Pylon, all max_vcpus exist in the guest at boot (boot=max,max=max)
    // and higher vCPUs start parked.
    if let Some(pos) = a.iter().position(|x| x == "--cpus")
        && pos + 1 < a.len()
    {
        let mut cpus = format!("boot={},max={}", max_vcpus, max_vcpus);
        if !config.vcpu_pins.is_empty() {
            let pins: Vec<String> = config
                .vcpu_pins
                .iter()
                .enumerate()
                .map(|(i, c)| format!("{i}@[{c}]"))
                .collect();
            cpus.push_str(&format!(",affinity=[{}]", pins.join(",")));
        }
        a[pos + 1] = cpus;
    }

    let vm_id = pylon_vm_id(api_socket);

    // No `shm=`: since Pylon v0.0.8-rc2 the VMM makes the shared page a
    // sealed memfd itself and refuses a path (spec/30-pylon.md P14).
    let pylon_opt = format!(
        "socket={},gpa=0xfedf0000,initial_active_vcpus={},min_cores={},max_cores={},priority_weight={},vm_id={},park={}",
        DEFAULT_PYLOND_SOCKET,
        initial_active_vcpus,
        min_cores,
        max_vcpus,
        priority_weight,
        vm_id,
        park.as_str()
    );

    a.push("--pylon".into());
    a.push(pylon_opt);
    a
}

pub struct PylonDriver;

impl HypervisorDriver for PylonDriver {
    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::Pylon
    }

    fn launch_args(
        &self,
        config: &VmConfig,
        api_socket: &str,
    ) -> Result<LaunchArgs, HypervisorError> {
        let bin = resolve_binary("pylon").ok_or_else(|| {
            HypervisorError::ProcessStart(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "pylon is not installed",
            ))
        })?;
        cloud_hypervisor::check_mmio64_here(config)?;
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        argv.extend(args(config, api_socket, park_mode_here()));
        Ok(LaunchArgs {
            argv,
            fallback: None,
        })
    }

    fn observe(&self, api_socket: &str) -> Result<Observed, HypervisorError> {
        Ok(ChClient::new(api_socket).observe()?)
    }

    fn pause(&self, api_socket: &str) -> Result<(), HypervisorError> {
        Ok(ChClient::new(api_socket).pause()?)
    }

    fn resume(&self, api_socket: &str) -> Result<(), HypervisorError> {
        Ok(ChClient::new(api_socket).resume()?)
    }

    fn add_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError> {
        Ok(ChClient::new(api_socket).add_device(device_path, &vfio_device_id(device_path))?)
    }

    fn remove_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError> {
        let ch = ChClient::new(api_socket);
        ch.remove_device(&vfio_device_id(device_path))
            .or_else(|_| ch.remove_device(&super::legacy_vfio_device_id(device_path)))?;
        Ok(())
    }

    fn resize_vcpus(&self, api_socket: &str, desired: u8) -> Result<(), HypervisorError> {
        let vm_id = pylon_vm_id(api_socket);
        send_pylon_policy_update(
            DEFAULT_PYLOND_SOCKET,
            vm_id,
            None,
            Some(desired as u32),
            None,
        )
    }

    fn update_policy(
        &self,
        api_socket: &str,
        min_cores: Option<u32>,
        max_cores: Option<u32>,
        priority_weight: Option<u32>,
    ) -> Result<(), HypervisorError> {
        let vm_id = pylon_vm_id(api_socket);
        send_pylon_policy_update(
            DEFAULT_PYLOND_SOCKET,
            vm_id,
            min_cores,
            max_cores,
            priority_weight,
        )
    }
}

#[derive(Debug, Serialize)]
struct VmPolicyUpdateRequest {
    vm_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_cores: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority_weight: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct VmPolicyUpdateResponse {
    accepted: bool,
    message: String,
    #[serde(default)]
    min_cores: Option<u32>,
    #[serde(default)]
    max_cores: Option<u32>,
    #[serde(default)]
    priority_weight: Option<u32>,
}

/// One request to pylond's control socket and its whole answer.
fn pylond_exchange(
    pylond_socket: &str,
    payload: &[u8],
    what: &str,
) -> Result<Vec<u8>, HypervisorError> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let mut stream = UnixStream::connect(pylond_socket).map_err(|e| {
        HypervisorError::SocketConnection(format!(
            "failed to connect to pylond socket {pylond_socket}: {e}"
        ))
    })?;
    // pylond gives each connection 5 s; don't hold a controller thread longer.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    stream.write_all(payload).map_err(|e| {
        HypervisorError::ApiRequest(format!("failed to send {what} to pylond: {e}"))
    })?;
    stream.flush().map_err(|e| {
        HypervisorError::ApiRequest(format!("failed to flush {what} to pylond: {e}"))
    })?;
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut resp_buf = Vec::new();
    stream.read_to_end(&mut resp_buf).map_err(|e| {
        HypervisorError::ApiRequest(format!("failed to read {what} response from pylond: {e}"))
    })?;
    Ok(resp_buf)
}

pub fn send_pylon_policy_update(
    pylond_socket: &str,
    vm_id: u64,
    min_cores: Option<u32>,
    max_cores: Option<u32>,
    priority_weight: Option<u32>,
) -> Result<(), HypervisorError> {
    if !Path::new(pylond_socket).exists() {
        return Ok(());
    }
    let req = VmPolicyUpdateRequest {
        vm_id,
        min_cores,
        max_cores,
        priority_weight,
    };
    let payload = serde_json::to_vec(&req).map_err(|e| {
        HypervisorError::ApiRequest(format!("failed to serialize pylond policy update: {e}"))
    })?;
    let resp_buf = pylond_exchange(pylond_socket, &payload, "policy update")?;
    let resp: VmPolicyUpdateResponse = serde_json::from_slice(&resp_buf).map_err(|e| {
        HypervisorError::ApiRequest(format!(
            "failed to parse pylond policy update response: {e}"
        ))
    })?;
    if !resp.accepted {
        return Err(HypervisorError::ApiRequest(format!(
            "pylond rejected policy update for VM {vm_id}: {}",
            resp.message
        )));
    }
    Ok(())
}

/// What pylond knows of a VM (spec/30-pylon.md P16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PylondRegistration {
    /// pylond lists the VM: it manages its cores.
    Listed,
    /// pylond answers without it: it refused the VM, or restarted since.
    NotListed,
    /// No pylond socket on this host.
    NoPylond,
}

/// Ask pylond whether it has `vm_id`, with the stats request metering
/// sends. pylond answers only for VMs the caller's user registered, root
/// and its own user; the control plane and the VMs run as the same user.
pub fn pylond_registration(
    pylond_socket: &str,
    vm_id: u64,
) -> Result<PylondRegistration, HypervisorError> {
    if !Path::new(pylond_socket).exists() {
        return Ok(PylondRegistration::NoPylond);
    }
    let payload = serde_json::to_vec(&serde_json::json!({ "action": "stats", "vm_id": vm_id }))
        .map_err(|e| HypervisorError::ApiRequest(format!("failed to serialize: {e}")))?;
    let resp_buf = pylond_exchange(pylond_socket, &payload, "stats request")?;

    #[derive(Deserialize)]
    struct StatsVm {
        vm_id: u64,
    }
    #[derive(Deserialize)]
    struct StatsResponse {
        accepted: bool,
        #[serde(default)]
        message: String,
        #[serde(default)]
        vms: Vec<StatsVm>,
    }
    let resp: StatsResponse = serde_json::from_slice(&resp_buf).map_err(|e| {
        HypervisorError::ApiRequest(format!("failed to parse pylond stats response: {e}"))
    })?;
    if !resp.accepted {
        return Err(HypervisorError::ApiRequest(format!(
            "pylond refused the stats request: {}",
            resp.message
        )));
    }
    Ok(if resp.vms.iter().any(|v| v.vm_id == vm_id) {
        PylondRegistration::Listed
    } else {
        PylondRegistration::NotListed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn serve_once(
        path: &std::path::Path,
        response: &'static str,
    ) -> std::thread::JoinHandle<String> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        })
    }

    #[test]
    fn pylon_paths_and_ids() {
        let sock = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000042/api.sock";
        assert_eq!(pylon_vm_id(sock), 0x42);
    }

    #[test]
    fn park_in_kvm_only_with_scx_pylon_attached() {
        let dir = tempfile::tempdir().unwrap();
        let sx = dir.path();
        // No sched_ext at all (Linux 6.8).
        assert_eq!(park_mode(&sx.join("absent")), Park::Vmm);
        std::fs::create_dir_all(sx.join("root")).unwrap();
        std::fs::write(sx.join("state"), "disabled\n").unwrap();
        assert_eq!(park_mode(sx), Park::Vmm);
        // Another sched_ext scheduler.
        std::fs::write(sx.join("state"), "enabled\n").unwrap();
        std::fs::write(sx.join("root/ops"), "rusty\n").unwrap();
        assert_eq!(park_mode(sx), Park::Vmm);
        std::fs::write(sx.join("root/ops"), "pylon\n").unwrap();
        assert_eq!(park_mode(sx), Park::Kvm);

        let c = serde_json::from_value(serde_json::json!({
            "vcpu_count": 2, "max_vcpus": 4, "mem_size_mib": 1024, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        let a = args(&c, "/run/glidex-cp/vms/x/api.sock", Park::Kvm);
        let pos = a.iter().position(|x| x == "--pylon").unwrap();
        assert!(a[pos + 1].ends_with(",park=kvm"), "{}", a[pos + 1]);
    }

    #[test]
    fn pylond_registration_reads_the_stats_answer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("pylond.sock");
        let s = sock.to_str().unwrap();
        assert_eq!(
            pylond_registration(s, 7).unwrap(),
            PylondRegistration::NoPylond
        );

        let h = serve_once(
            &sock,
            r#"{"accepted":true,"message":"stats retrieved","vms":[{"vm_id":7,"current_cores":1}]}"#,
        );
        assert_eq!(
            pylond_registration(s, 7).unwrap(),
            PylondRegistration::Listed
        );
        let req: serde_json::Value = serde_json::from_str(&h.join().unwrap()).unwrap();
        assert_eq!(req, serde_json::json!({ "action": "stats", "vm_id": 7 }));

        // What rc2 answered for a VM it refused at registration.
        let h = serve_once(
            &sock,
            r#"{"accepted":true,"message":"stats retrieved","vms":[]}"#,
        );
        assert_eq!(
            pylond_registration(s, 7).unwrap(),
            PylondRegistration::NotListed
        );
        h.join().unwrap();

        let h = serve_once(&sock, r#"{"accepted":false,"message":"busy"}"#);
        assert!(pylond_registration(s, 7).is_err());
        h.join().unwrap();
    }

    #[test]
    fn pylon_vm_id_edge_cases() {
        // Non-UUID directory path falls back to 1
        assert_eq!(pylon_vm_id("/run/glidex-cp/vms/non-uuid-name/api.sock"), 1);
        // All-zero UUID yields 0, which filters to 1
        assert_eq!(
            pylon_vm_id("/run/glidex-cp/vms/00000000-0000-0000-0000-000000000000/api.sock"),
            1
        );
        // Symmetric UUID where high == low yields 0 XOR, which filters to 1
        assert_eq!(
            pylon_vm_id("/run/glidex-cp/vms/01234567-89ab-cdef-0123-456789abcdef/api.sock"),
            1
        );
        // Valid non-zero UUID
        let id = pylon_vm_id("/run/glidex-cp/vms/a1b2c3d4-e5f6-4a5b-8c9d-0e1f2a3b4c5d/api.sock");
        assert_ne!(id, 0);
    }

    #[test]
    fn pylon_args_contain_pylon_flag() {
        let c = serde_json::from_value(serde_json::json!({
            "vcpu_count": 4, "min_vcpus": 2, "max_vcpus": 8, "cpu_weight": 500,
            "mem_size_mib": 1024, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        let api = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000001/api.sock";
        let a = args(&c, api, Park::Vmm);
        let cpus_pos = a.iter().position(|x| x == "--cpus").expect("--cpus flag");
        assert_eq!(&a[cpus_pos + 1], "boot=8,max=8");
        let pos = a.iter().position(|x| x == "--pylon").expect("--pylon flag");
        let val = &a[pos + 1];
        assert!(!val.contains("shm="), "{val}");
        assert!(val.ends_with(",park=vmm"), "{val}");
        assert!(val.starts_with("socket=/run/pylon/pylond.sock,"), "{val}");
        // park=vmm: every vCPU active, none parked (a parked one stalls the guest).
        assert!(val.contains("initial_active_vcpus=8"));
        assert!(val.contains("min_cores=2"));
        // park=kvm: start at the floor.
        let kvm = args(&c, api, Park::Kvm);
        let pos = kvm.iter().position(|x| x == "--pylon").unwrap();
        assert!(
            kvm[pos + 1].contains("initial_active_vcpus=2"),
            "{}",
            kvm[pos + 1]
        );
        assert!(val.contains("max_cores=8"));
        assert!(val.contains("priority_weight=500"));
        assert!(val.contains("vm_id=1"));
    }

    #[test]
    fn pylon_args_scale_to_zero() {
        let c = serde_json::from_value(serde_json::json!({
            "vcpu_count": 2, "min_vcpus": 0, "max_vcpus": 8, "cpu_weight": 200,
            "mem_size_mib": 1024, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        let api = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000002/api.sock";
        let a = args(&c, api, Park::Vmm);
        let cpus_pos = a.iter().position(|x| x == "--cpus").expect("--cpus flag");
        assert_eq!(&a[cpus_pos + 1], "boot=8,max=8");
        let pos = a.iter().position(|x| x == "--pylon").expect("--pylon flag");
        let val = &a[pos + 1];
        assert!(val.contains("initial_active_vcpus=8"));
        assert!(val.contains("min_cores=0"));
        // With park=kvm, a VM that may park every vCPU still boots on one.
        let kvm = args(&c, api, Park::Kvm);
        let pos = kvm.iter().position(|x| x == "--pylon").unwrap();
        assert!(
            kvm[pos + 1].contains("initial_active_vcpus=1"),
            "{}",
            kvm[pos + 1]
        );
        assert!(val.contains("max_cores=8"));
        assert!(val.contains("priority_weight=200"));
        assert!(val.contains("vm_id=2"));
    }

    #[test]
    fn pylon_args_fixed_size_vm() {
        let c = serde_json::from_value(serde_json::json!({
            "vcpu_count": 4,
            "mem_size_mib": 2048, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        let api = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000003/api.sock";
        let a = args(&c, api, Park::Vmm);
        let cpus_pos = a.iter().position(|x| x == "--cpus").expect("--cpus flag");
        assert_eq!(&a[cpus_pos + 1], "boot=4,max=4");
        let pos = a.iter().position(|x| x == "--pylon").expect("--pylon flag");
        let val = &a[pos + 1];
        assert!(val.contains("initial_active_vcpus=4"));
        assert!(val.contains("min_cores=4"));
        assert!(val.contains("max_cores=4"));
        assert!(val.contains("priority_weight=100"));
        assert!(val.contains("vm_id=3"));
    }

    #[test]
    fn pylon_args_with_vcpu_pins() {
        let mut c: VmConfig = serde_json::from_value(serde_json::json!({
            "vcpu_count": 2,
            "mem_size_mib": 1024, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        c.vcpu_pins = vec![12, 14];
        let api = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000004/api.sock";
        let a = args(&c, api, Park::Vmm);
        let cpus_pos = a.iter().position(|x| x == "--cpus").expect("--cpus flag");
        assert_eq!(&a[cpus_pos + 1], "boot=2,max=2,affinity=[0@[12],1@[14]]");
    }

    #[test]
    fn pylon_driver_methods() {
        let driver = PylonDriver;
        assert_eq!(driver.hypervisor_type(), HypervisorType::Pylon);

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let sock_str = sock.to_str().unwrap();

        // pause
        let s = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        driver.pause(sock_str).unwrap();
        let req = s.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.pause HTTP/1.1"));

        // resume
        let s = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        driver.resume(sock_str).unwrap();
        let req = s.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.resume HTTP/1.1"));

        // resize_vcpus (no-op on pylon to avoid destructive ACPI CPU unplug)
        driver.resize_vcpus(sock_str, 6).unwrap();

        // add_device
        let s = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        driver
            .add_device(sock_str, "/sys/bus/pci/devices/0000:41:00.0")
            .unwrap();
        let req = s.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.add-device HTTP/1.1"));

        // remove_device
        let s = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        driver
            .remove_device(sock_str, "/sys/bus/pci/devices/0000:41:00.0")
            .unwrap();
        let req = s.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.remove-device HTTP/1.1"));
    }
}
