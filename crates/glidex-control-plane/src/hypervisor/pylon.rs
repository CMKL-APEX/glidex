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
use std::path::Path;

/// Derive the shared memory path for a Pylon VM from its api socket path.
/// E.g. `/run/glidex-cp/vms/<id>/api.sock` -> `/run/glidex-cp/vms/<id>/pylon.shm`.
pub fn pylon_shm_path(api_socket: &str) -> String {
    let p = Path::new(api_socket);
    if let Some(parent) = p.parent() {
        parent.join("pylon.shm").to_string_lossy().into_owned()
    } else {
        format!("{}.pylon.shm", api_socket)
    }
}

/// Derive a deterministic 64-bit VM ID for Pylon from the VM directory or UUID.
pub fn pylon_vm_id(api_socket: &str) -> u64 {
    let p = Path::new(api_socket);
    p.parent()
        .and_then(|d| d.file_name())
        .and_then(|s| s.to_str())
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .map(|u| {
            let (high, low) = u.as_u64_pair();
            high ^ low
        })
        .filter(|&id| id != 0)
        .unwrap_or(1)
}

/// Default pylond socket path on the host.
pub const DEFAULT_PYLOND_SOCKET: &str = "/run/pylon/pylond.sock";

/// Build the command line arguments for Pylon.
pub(crate) fn args(config: &VmConfig, api_socket: &str) -> Vec<String> {
    let mut a = cloud_hypervisor::args(config, api_socket);

    let shm = pylon_shm_path(api_socket);
    let vm_id = pylon_vm_id(api_socket);
    let vcpu_count = config.vcpu_count;

    let pylon_opt = format!(
        "shm={},socket={},gpa=0xfedf0000,initial_active_vcpus={},min_cores=1,max_cores={},priority_weight=100,vm_id={}",
        shm, DEFAULT_PYLOND_SOCKET, vcpu_count, vcpu_count, vm_id
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
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        argv.extend(args(config, api_socket));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pylon_paths_and_ids() {
        let sock = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000042/api.sock";
        assert_eq!(
            pylon_shm_path(sock),
            "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000042/pylon.shm"
        );
        assert_eq!(pylon_vm_id(sock), 0x42);
    }

    #[test]
    fn pylon_args_contain_pylon_flag() {
        let c = serde_json::from_value(serde_json::json!({
            "vcpu_count": 4, "mem_size_mib": 1024, "rootfs_path": "/r",
            "kernel_image_path": "/k", "kernel_args": "console=hvc0 root=/dev/vda",
        }))
        .unwrap();
        let api = "/run/glidex-cp/vms/00000000-0000-0000-0000-000000000001/api.sock";
        let a = args(&c, api);
        let pos = a.iter().position(|x| x == "--pylon").expect("--pylon flag");
        let val = &a[pos + 1];
        assert!(val.contains("shm=/run/glidex-cp/vms/00000000-0000-0000-0000-000000000001/pylon.shm"));
        assert!(val.contains("socket=/run/pylon/pylond.sock"));
        assert!(val.contains("initial_active_vcpus=4"));
        assert!(val.contains("vm_id=1"));
    }
}
