//! Cloud Hypervisor: the whole VM config on its command line (D9,
//! spec/reconciliation.md §8.2), and the runtime operations over its HTTP
//! API (`glidex-hv-client`).
//!
//! The command line is the `vm.create` payload of earlier releases, option
//! for option. Verified against the pinned v53.0 (spec §4, F1/F2):
//! `image_type=` takes lower-case names (`vhd`, not the API's `FixedVhd`),
//! `mtu=` is accepted although `--help` does not list it, and a value
//! containing `,` must be double-quoted.

use super::{needs_shared_memory, resolve_binary, vfio_device_id, vm_disks, HypervisorDriver, HypervisorError, HypervisorType, LaunchArgs};
use crate::images::qemu_img::ImageType;
use crate::models::VmConfig;
use glidex_hv_client::{ch::ChClient, Observed};
use glidex_ovs::vm_port::VmPortBinding;

/// Where `glidex-install` puts Cloud-Hypervisor's EDK2 UEFI firmware:
/// `~/.glidex/CLOUDHV.fd` on x86_64, `~/.glidex/CLOUDHV_EFI.fd` on aarch64.
/// Returns the path even if the file has not been downloaded.
pub fn default_firmware_path() -> Option<std::path::PathBuf> {
    let name = if cfg!(target_arch = "aarch64") {
        "CLOUDHV_EFI.fd"
    } else {
        "CLOUDHV.fd"
    };
    dirs::home_dir().map(|home| home.join(".glidex").join(name))
}

/// A value inside CH's `key=value,…` options: quoted when it holds a
/// comma (F2). Admission refuses `"` in paths (`check_command_line_path`).
fn q(value: &str) -> String {
    if value.contains(',') {
        format!("\"{}\"", value)
    } else {
        value.to_string()
    }
}

/// CH's command-line name of an image type.
fn image_type_name(t: ImageType) -> &'static str {
    match t {
        ImageType::Raw => "raw",
        ImageType::Qcow2 => "qcow2",
        ImageType::Vhdx => "vhdx",
        ImageType::FixedVhd => "vhd",
    }
}

/// `--disk` values, `[root, data disks…, seed]` (`vm_disks`).
fn disk_args(config: &VmConfig) -> Vec<String> {
    vm_disks(config)
        .into_iter()
        .map(|d| {
            let mut v = format!("path={}", q(&d.path));
            if d.read_only {
                v.push_str(",readonly=on");
            }
            // CH deprecated image-type auto-detection in v52; `None`
            // (unreadable image) leaves it out so CH reports the open error.
            if let Some(t) = d.format {
                v.push_str(",image_type=");
                v.push_str(image_type_name(t));
            }
            // CH refuses qcow2 backing files unless asked; only set for
            // linked disks glidex created.
            if d.backing_files {
                v.push_str(",backing_files=on");
            }
            v
        })
        .collect()
}

/// `--net` values.
fn net_args(config: &VmConfig) -> Vec<String> {
    config
        .nic_bindings
        .iter()
        .map(|nic| {
            let mut v = format!("id={},mac={},num_queues={}", nic.id, nic.mac, 2 * nic.queue_pairs.max(1) as u16);
            if let Some(mtu) = nic.mtu {
                v.push_str(&format!(",mtu={}", mtu));
            }
            match &nic.binding {
                VmPortBinding::Tap { ifname } => v.push_str(&format!(",tap={}", ifname)),
                // OVS's dpdkvhostuserclient is the client, so CH serves.
                VmPortBinding::VhostUser { socket } => {
                    v.push_str(&format!(
                        ",vhost_user=on,socket={},vhost_mode=server,queue_size={}",
                        q(&socket.to_string_lossy()),
                        glidex_ovs::tuning::VHOST_QUEUE_SIZE
                    ))
                }
            }
            v
        })
        .collect()
}

/// The command line after the binary: everything `vm.create` used to get.
pub(crate) fn args(config: &VmConfig, api_socket: &str) -> Vec<String> {
    let mut a: Vec<String> = vec!["--api-socket".into(), format!("path={}", q(api_socket))];
    a.push("--cpus".into());
    a.push(format!("boot={0},max={0}", config.vcpu_count));
    let mut mem = format!("size={}M", config.mem_size_mib);
    if needs_shared_memory(config) {
        mem.push_str(",shared=on");
    }
    if config.hugepages {
        mem.push_str(",hugepages=on");
    }
    a.push("--memory".into());
    a.push(mem);

    // Firmware boot hands off to the disk's own bootloader; distro cloud
    // images put their console on ttyS0, so the serial port is the console
    // there instead of hvc0. `tty` is CH's stdio: the shim's PTY.
    let (console, serial) = match &config.firmware_path {
        Some(fw) => {
            a.push("--firmware".into());
            a.push(fw.clone());
            ("off", "tty")
        }
        None => {
            a.push("--kernel".into());
            a.push(config.kernel_image_path.clone());
            a.push("--cmdline".into());
            a.push(config.kernel_args.clone());
            ("tty", "off")
        }
    };

    let disks = disk_args(config);
    if !disks.is_empty() {
        a.push("--disk".into());
        a.extend(disks);
    }
    let nets = net_args(config);
    if !nets.is_empty() {
        a.push("--net".into());
        a.extend(nets);
    }
    if !config.vfio_devices.is_empty() {
        a.push("--device".into());
        a.extend(config.vfio_devices.iter().map(|p| format!("path={},id={}", q(p), vfio_device_id(p))));
    }
    a.extend(["--console".into(), console.into(), "--serial".into(), serial.into()]);
    a
}

pub struct CloudHypervisorDriver;

impl HypervisorDriver for CloudHypervisorDriver {
    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::CloudHypervisor
    }

    fn launch_args(&self, config: &VmConfig, api_socket: &str) -> Result<LaunchArgs, HypervisorError> {
        let bin = resolve_binary("cloud-hypervisor").ok_or_else(|| {
            HypervisorError::ProcessStart(std::io::Error::new(std::io::ErrorKind::NotFound, "cloud-hypervisor is not installed"))
        })?;
        let mut argv = vec![bin.to_string_lossy().into_owned()];
        argv.extend(args(config, api_socket));
        Ok(LaunchArgs { argv, fallback: None })
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
        Ok(ChClient::new(api_socket).remove_device(&vfio_device_id(device_path))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::qemu_img::{detect_image_type, DiskFormat};
    use crate::models::{DiskBinding, NicBinding};

    fn config(extra: serde_json::Value) -> VmConfig {
        let mut base = serde_json::json!({
            "vcpu_count": 2, "mem_size_mib": 1024, "rootfs_path": "/d", "kernel_args": "console=hvc0 root=/dev/vda",
            "kernel_image_path": "/k"
        });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    /// The values that follow `flag` up to the next flag.
    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        let Some(i) = args.iter().position(|a| a == flag) else { return Vec::new() };
        args[i + 1..].iter().take_while(|a| !a.starts_with("--")).map(String::as_str).collect()
    }

    #[test]
    fn kernel_boot_command_line() {
        let a = args(&config(serde_json::json!({})), "/run/glidex-cp/vms/x/api.sock");
        assert_eq!(values(&a, "--api-socket"), ["path=/run/glidex-cp/vms/x/api.sock"]);
        assert_eq!(values(&a, "--cpus"), ["boot=2,max=2"]);
        assert_eq!(values(&a, "--memory"), ["size=1024M"]);
        assert_eq!(values(&a, "--kernel"), ["/k"]);
        assert_eq!(values(&a, "--cmdline"), ["console=hvc0 root=/dev/vda"]);
        assert!(values(&a, "--firmware").is_empty());
        // Kernel boot: the virtio console is the guest console.
        assert_eq!((values(&a, "--console"), values(&a, "--serial")), (vec!["tty"], vec!["off"]));
        assert!(!a.iter().any(|x| x == "--net" || x == "--device"));
    }

    #[test]
    fn firmware_boot_uses_the_serial_port() {
        let a = args(&config(serde_json::json!({"firmware_path": "/fw/CLOUDHV.fd"})), "/s");
        assert_eq!(values(&a, "--firmware"), ["/fw/CLOUDHV.fd"]);
        assert!(values(&a, "--kernel").is_empty() && values(&a, "--cmdline").is_empty());
        assert_eq!((values(&a, "--console"), values(&a, "--serial")), (vec!["off"], vec!["tty"]));
    }

    #[test]
    fn managed_disks_use_recorded_format_and_seed_stays_last() {
        let mut c = config(serde_json::json!({"cloud_init_path": "/run/x,y/seed.img"}));
        c.root_disk_binding = Some(DiskBinding { path: "/disks/root.qcow2".into(), format: DiskFormat::Qcow2, backing_files: true });
        c.data_disk_bindings = vec![DiskBinding { path: "/disks/data.raw".into(), format: DiskFormat::Raw, backing_files: false }];
        assert_eq!(values(&args(&c, "/s"), "--disk"), [
            "path=/disks/root.qcow2,image_type=qcow2,backing_files=on",
            "path=/disks/data.raw,image_type=raw",
            "path=\"/run/x,y/seed.img\",readonly=on,image_type=raw",
        ]);
        // An unreadable user-supplied rootfs: CH reports the open error.
        let c = config(serde_json::json!({"rootfs_path": "/nonexistent/root.img"}));
        assert_eq!(values(&args(&c, "/s"), "--disk"), ["path=/nonexistent/root.img"]);
    }

    #[test]
    fn image_type_names_are_chs_cli_names() {
        assert_eq!(
            [ImageType::Raw, ImageType::Qcow2, ImageType::Vhdx, ImageType::FixedVhd].map(image_type_name),
            ["raw", "qcow2", "vhdx", "vhd"]
        );
    }

    #[test]
    fn nics_hugepages_and_vfio() {
        use glidex_ovs::vm_port::VmPortBinding;
        let mut c = config(serde_json::json!({"hugepages": true, "vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]}));
        c.nic_bindings = vec![
            NicBinding { id: "net0".into(), mac: "02:00:00:00:00:01".into(), binding: VmPortBinding::Tap { ifname: "gxabc-0".into() }, queue_pairs: 1, mtu: None },
            NicBinding { id: "net1".into(), mac: "02:00:00:00:00:02".into(), binding: VmPortBinding::VhostUser { socket: "/run/glidex/vhost/x.net1.sock".into() }, queue_pairs: 2, mtu: Some(9000) },
        ];
        let a = args(&c, "/s");
        assert_eq!(values(&a, "--net"), [
            "id=net0,mac=02:00:00:00:00:01,num_queues=2,tap=gxabc-0",
            "id=net1,mac=02:00:00:00:00:02,num_queues=4,mtu=9000,vhost_user=on,socket=/run/glidex/vhost/x.net1.sock,vhost_mode=server,queue_size=1024",
        ]);
        // vhost-user needs shared guest memory.
        assert_eq!(values(&a, "--memory"), ["size=1024M,shared=on,hugepages=on"]);
        assert_eq!(values(&a, "--device"), ["path=/sys/bus/pci/devices/0000:41:00.0,id=_vfio_0000_41_00_0"]);
    }

    #[test]
    fn detect_image_type_from_magic() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_string_lossy().into_owned()
        };
        let mut qcow2 = b"QFI\xfb\x00\x00\x00\x03".to_vec();
        qcow2.resize(4096, 0);
        assert_eq!(detect_image_type(&write("a.qcow2", &qcow2)), Some(ImageType::Qcow2));
        let mut vhdx = b"vhdxfile".to_vec();
        vhdx.resize(4096, 0);
        assert_eq!(detect_image_type(&write("a.vhdx", &vhdx)), Some(ImageType::Vhdx));
        let mut vhd = vec![0u8; 4096];
        let footer = vhd.len() - 512;
        vhd[footer..footer + 8].copy_from_slice(b"conectix");
        vhd[footer + 60..footer + 64].copy_from_slice(&2u32.to_be_bytes());
        assert_eq!(detect_image_type(&write("a.vhd", &vhd)), Some(ImageType::FixedVhd));
        assert_eq!(detect_image_type(&write("a.raw", &[0u8; 4096])), Some(ImageType::Raw));
        assert_eq!(detect_image_type("/nonexistent/disk.img"), None);
    }

    /// F1: the pinned CH parses the whole command line (it fails only
    /// when opening the missing files). Skipped without cloud-hypervisor.
    #[test]
    fn cloud_hypervisor_parses_the_full_command_line() {
        use glidex_ovs::vm_port::VmPortBinding;
        let Some(bin) = resolve_binary("cloud-hypervisor") else { return };
        let dir = tempfile::tempdir().unwrap();
        let mut c = config(serde_json::json!({
            "firmware_path": "/nonexistent/CLOUDHV.fd", "cloud_init_path": "/nonexistent/a,b/seed.img",
            "vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]
        }));
        c.root_disk_binding = Some(DiskBinding { path: "/nonexistent/root.qcow2".into(), format: DiskFormat::Qcow2, backing_files: true });
        c.nic_bindings = vec![NicBinding { id: "net0".into(), mac: "02:00:00:00:00:01".into(), binding: VmPortBinding::Tap { ifname: "gxnope-0".into() }, queue_pairs: 2, mtu: Some(1500) }];
        let api = dir.path().join("api.sock");
        let out = std::process::Command::new(bin).args(args(&c, api.to_str().unwrap())).output().unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(!err.contains("ParsingConfig") && !err.contains("required arguments"), "{err}");
    }
}
