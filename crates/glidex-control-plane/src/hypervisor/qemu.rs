//! QEMU: configured entirely on its command line, now started running
//! (no `-S`, D9), and driven over QMP (`glidex-hv-client`) at run time.

use super::{
    needs_shared_memory, resolve_binary, vfio_bdf, vfio_device_id, vm_disks, HypervisorDriver, HypervisorError,
    HypervisorType, LaunchArgs, VmDisk,
};
use crate::images::qemu_img::ImageType;
use crate::models::{NicBinding, VmConfig};
use glidex_hv_client::{qmp::QmpClient, Observed};
use glidex_ovs::vm_port::VmPortBinding;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// OVMF code images glidex looks for, in order: Debian/Ubuntu (`ovmf`),
/// Fedora/RHEL (`edk2-ovmf`), Arch (`edk2-ovmf`).
pub const OVMF_CODE_CANDIDATES: &[&str] = &[
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/edk2/x64/OVMF_CODE.4m.fd",
    "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
];

/// The host's OVMF code image: the first candidate that exists, else the
/// first candidate (so the error names the path to install).
pub fn default_firmware_path() -> Option<PathBuf> {
    OVMF_CODE_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| OVMF_CODE_CANDIDATES.first().map(PathBuf::from))
}

/// The pristine variable store shipped next to an OVMF code image
/// (`OVMF_CODE_4M.fd` -> `OVMF_VARS_4M.fd`), if there is one.
pub fn ovmf_vars_template(code: &Path) -> Option<PathBuf> {
    let name = code.file_name()?.to_str()?;
    let vars = if name.contains("CODE") {
        name.replace("CODE", "VARS")
    } else if name.contains("code") {
        name.replace("code", "vars")
    } else {
        return None;
    };
    let path = code.with_file_name(vars);
    path.exists().then_some(path)
}

/// Lowest QEMU glidex drives: `server=on` sockets, `memory-backend`.
const MIN_QEMU_VERSION: (u32, u32) = (6, 0);

/// `(major, minor)` from `qemu-system-x86_64 --version`.
fn parse_qemu_version(output: &str) -> Option<(u32, u32)> {
    let rest = output.split("version ").nth(1)?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

fn qemu_version() -> Option<(u32, u32)> {
    static VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *VERSION.get_or_init(|| {
        let out = Command::new("qemu-system-x86_64")
            .arg("--version")
            .output()
            .ok()?;
        parse_qemu_version(&String::from_utf8_lossy(&out.stdout))
    })
}

/// In-kernel virtio-net datapath, if the VM may open `/dev/vhost-net`
/// (usually the `kvm` group). `access(2)`, not an open: the VM's unit runs
/// as this user with these groups, but this unit's sandbox closes /dev.
fn vhost_net_usable() -> bool {
    nix::unistd::access("/dev/vhost-net", nix::unistd::AccessFlags::R_OK | nix::unistd::AccessFlags::W_OK).is_ok()
}

/// Whether the filesystem holding `path` supports `O_DIRECT` (tmpfs and
/// some FUSE filesystems don't).
fn supports_direct_io(path: &str) -> bool {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)
        .is_ok()
}

/// Double the commas in a value for QEMU's `key=value,…` option parser.
fn qopt(value: &str) -> String {
    value.replace(',', ",,")
}

/// How the guest's UEFI firmware is mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Firmware {
    /// Read-only code plus this VM's writable copy of the variable store.
    /// `secure_boot` builds need SMM and a secure flash.
    Pflash {
        code: String,
        vars: String,
        secure_boot: bool,
    },
    /// A combined image (`OVMF.fd`) with no separate variable store.
    Bios(String),
}

/// One `-blockdev` / `virtio-blk-pci` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QemuDisk {
    disk: VmDisk,
    /// `cache.direct=on,aio=native`: bypass the host page cache.
    direct: bool,
}

impl QemuDisk {
    /// The `-blockdev` JSON. JSON rather than `key=value` so a path never
    /// needs escaping.
    fn blockdev(&self, node: &str) -> String {
        let read_only = self.disk.read_only;
        let mut file = serde_json::json!({
            "driver": "file",
            "filename": self.disk.path,
            "read-only": read_only,
        });
        if self.direct {
            file["cache"] = serde_json::json!({"direct": true});
            file["aio"] = "native".into();
        }
        // A disk we couldn't probe is opened raw; never left to QEMU's own
        // probing, which would let a guest that writes a qcow2 header into
        // a raw disk change how it is opened next boot.
        let format = self.disk.format.unwrap_or(ImageType::Raw);
        let mut fmt = serde_json::json!({
            "driver": format.qemu_format(),
            "node-name": node,
            "read-only": read_only,
            "file": file,
        });
        if self.direct {
            fmt["cache"] = serde_json::json!({"direct": true});
        }
        // QEMU follows a qcow2's backing file on its own unless told not to.
        if format == ImageType::Qcow2 && !self.disk.backing_files {
            fmt["backing"] = serde_json::Value::Null;
        }
        fmt.to_string()
    }
}

/// Everything the `qemu-system-x86_64` command line depends on, resolved
/// against the host by `QemuDriver::launch_args`. `args` itself is pure.
#[derive(Debug, Clone)]
struct LaunchSpec<'a> {
    config: &'a VmConfig,
    qmp_socket: String,
    firmware: Option<Firmware>,
    disks: Vec<QemuDisk>,
    /// `-cpu host`; dropped on a retry if the host CPU can't be expressed.
    cpu_host: bool,
    /// Tap NICs use the in-kernel vhost-net datapath.
    vhost_net: bool,
}

impl LaunchSpec<'_> {
    fn args(&self) -> Vec<String> {
        let config = self.config;
        // No default NIC, VGA, floppy or CD-ROM: the guest gets exactly the
        // devices below, as under Cloud-Hypervisor.
        let mut args: Vec<String> =
            ["-nodefaults", "-no-user-config", "-enable-kvm"].map(String::from).to_vec();
        let push = |args: &mut Vec<String>, a: &str, b: String| {
            args.push(a.to_string());
            args.push(b);
        };

        let shared = needs_shared_memory(config) || config.hugepages;
        let secure_boot = matches!(self.firmware, Some(Firmware::Pflash { secure_boot: true, .. }));
        let mut machine = "q35".to_string();
        if shared {
            machine.push_str(",memory-backend=mem");
        }
        if secure_boot {
            machine.push_str(",smm=on");
        }
        push(&mut args, "-machine", machine);
        if self.cpu_host {
            push(&mut args, "-cpu", "host".into());
        }
        push(&mut args, "-m", format!("{}M", config.mem_size_mib));
        push(&mut args, "-smp", config.vcpu_count.to_string());
        if shared {
            // vhost-user: OVS maps guest RAM, so it must be shared.
            push(
                &mut args,
                "-object",
                format!(
                    "memory-backend-memfd,id=mem,size={}M,share=on{}",
                    config.mem_size_mib,
                    if config.hugepages { ",hugetlb=on" } else { "" }
                ),
            );
        }

        match &self.firmware {
            Some(Firmware::Pflash { code, vars, secure_boot }) => {
                if *secure_boot {
                    push(&mut args, "-global", "driver=cfi.pflash01,property=secure,value=on".into());
                }
                push(
                    &mut args,
                    "-drive",
                    format!("if=pflash,format=raw,unit=0,readonly=on,file={}", qopt(code)),
                );
                push(&mut args, "-drive", format!("if=pflash,format=raw,unit=1,file={}", qopt(vars)));
            }
            Some(Firmware::Bios(path)) => push(&mut args, "-bios", path.clone()),
            None => {
                push(&mut args, "-kernel", config.kernel_image_path.clone());
                push(&mut args, "-append", config.kernel_args.clone());
            }
        }

        // Root first, so it stays /dev/vda and is what the firmware boots.
        for (i, disk) in self.disks.iter().enumerate() {
            let node = format!("disk{}", i);
            push(&mut args, "-blockdev", disk.blockdev(&node));
            let boot = if i == 0 { ",bootindex=1" } else { "" };
            push(&mut args, "-device", format!("virtio-blk-pci,drive={},id=vd{}{}", node, i, boot));
        }

        for nic in &config.nic_bindings {
            for (a, b) in nic_args(nic, self.vhost_net) {
                push(&mut args, a, b);
            }
        }

        for device in &config.vfio_devices {
            push(
                &mut args,
                "-device",
                format!("vfio-pci,host={},id={}", vfio_bdf(device), vfio_device_id(device)),
            );
        }

        push(&mut args, "-object", "rng-random,id=rng0,filename=/dev/urandom".into());
        push(&mut args, "-device", "virtio-rng-pci,rng=rng0".into());

        push(&mut args, "-qmp", format!("unix:{},server=on,wait=off", qopt(&self.qmp_socket)));
        push(&mut args, "-serial", "stdio".into());
        push(&mut args, "-display", "none".into());
        args
    }
}

/// `-netdev` / `-device` pairs for one NIC. vhost-user sockets are served
/// by QEMU (OVS's `dpdkvhostuserclient` connects); `wait=off` so QEMU starts
/// without waiting for OVS: the guest sees the link come up once it does.
fn nic_args(nic: &NicBinding, vhost_net: bool) -> Vec<(&'static str, String)> {
    let id = &nic.id;
    let pairs = nic.queue_pairs.max(1);
    let queues = if pairs > 1 { format!(",queues={}", pairs) } else { String::new() };
    let mut out = Vec::new();
    match &nic.binding {
        VmPortBinding::Tap { ifname } => out.push((
            "-netdev",
            format!(
                "tap,id={},ifname={},script=no,downscript=no,vhost={}{}",
                id,
                qopt(ifname),
                if vhost_net { "on" } else { "off" },
                queues
            ),
        )),
        VmPortBinding::VhostUser { socket } => {
            out.push((
                "-chardev",
                format!(
                    "socket,id=chr-{},path={},server=on,wait=off",
                    id,
                    qopt(&socket.to_string_lossy())
                ),
            ));
            out.push(("-netdev", format!("vhost-user,id={},chardev=chr-{}{}", id, id, queues)));
        }
    }
    let mut device = format!("virtio-net-pci,netdev={},id=dev-{},mac={}", id, id, nic.mac);
    if matches!(nic.binding, VmPortBinding::VhostUser { .. }) {
        // Deeper rings absorb bursts between OVS's polls (default 256).
        let n = glidex_ovs::tuning::VHOST_QUEUE_SIZE;
        device.push_str(&format!(",rx_queue_size={},tx_queue_size={}", n, n));
    }
    if let Some(mtu) = nic.mtu {
        device.push_str(&format!(",host_mtu={}", mtu));
    }
    if pairs > 1 {
        // One vector per queue plus config and control.
        device.push_str(&format!(",mq=on,vectors={}", 2 * pairs as u32 + 2));
    }
    out.push(("-device", device));
    out
}

/// Output of a failed launch that means `-cpu host` was the problem: the
/// shim then retries with QEMU's default CPU model (`launch.json`
/// `fallback`).
pub(crate) const CPU_REJECTED_MARKERS: &[&str] = &["cpu", "msr"];

#[cfg(test)]
fn cpu_model_rejected(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    CPU_REJECTED_MARKERS.iter().any(|m| log.contains(m))
}

/// Map the firmware: OVMF code read-only, plus this VM's own variable
/// store, copied from the pristine template on first boot (or when the
/// template changed size, e.g. after a 2M -> 4M OVMF upgrade). The
/// template is the firmware image's own (`template`), else the one shipped
/// next to the code file.
fn prepare_firmware(code: &str, vars: &str, template: Option<&str>) -> Result<Firmware, HypervisorError> {
    let code_path = Path::new(code);
    if !code_path.exists() {
        return Err(HypervisorError::InvalidConfig(format!(
            "UEFI firmware {} not found; pull the ovmf firmware image (Images > Firmware)",
            code
        )));
    }
    let Some(template) = template.map(PathBuf::from).or_else(|| ovmf_vars_template(code_path)) else {
        return Ok(Firmware::Bios(code.to_string()));
    };
    let template_len = std::fs::metadata(&template)?.len();
    let current_len = std::fs::metadata(vars).map(|m| m.len()).ok();
    if current_len != Some(template_len) {
        if let Some(dir) = Path::new(vars).parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(&template, vars)?;
        // The copy takes the template's mode, and a firmware image's
        // template is read-only; the guest writes its own store.
        std::fs::set_permissions(vars, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    }
    let name = code_path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let secure_boot = ["secboot", ".ms.", "snakeoil"].iter().any(|s| name.contains(s));
    Ok(Firmware::Pflash { code: code.to_string(), vars: vars.to_string(), secure_boot })
}

pub struct QemuDriver;

impl HypervisorDriver for QemuDriver {
    fn hypervisor_type(&self) -> HypervisorType {
        HypervisorType::Qemu
    }

    /// Resolve the config against the host into a `LaunchSpec`; the
    /// fallback is the same command line without `-cpu host`.
    fn launch_args(&self, config: &VmConfig, api_socket: &str) -> Result<LaunchArgs, HypervisorError> {
        let bin = resolve_binary("qemu-system-x86_64").ok_or_else(|| {
            HypervisorError::ProcessStart(std::io::Error::new(std::io::ErrorKind::NotFound, "qemu-system-x86_64 is not installed"))
        })?;
        if let Some(version) = qemu_version() {
            if version < MIN_QEMU_VERSION {
                return Err(HypervisorError::Unsupported(format!(
                    "QEMU {}.{} is too old; glidex needs {}.{} or newer",
                    version.0, version.1, MIN_QEMU_VERSION.0, MIN_QEMU_VERSION.1
                )));
            }
        }
        let firmware = match &config.firmware_path {
            Some(code) => {
                let vars = config.firmware_vars_path.clone().ok_or_else(|| {
                    HypervisorError::InvalidConfig("firmware boot needs a firmware variable store path".into())
                })?;
                Some(prepare_firmware(code, &vars, config.firmware_vars_template.as_deref())?)
            }
            None => None,
        };
        let disks = vm_disks(config)
            .into_iter()
            .map(|disk| QemuDisk { direct: supports_direct_io(&disk.path), disk })
            .collect();
        let mut spec = LaunchSpec {
            config,
            qmp_socket: api_socket.to_string(),
            firmware,
            disks,
            cpu_host: true,
            vhost_net: vhost_net_usable(),
        };
        if !spec.vhost_net && config.nic_bindings.iter().any(|n| matches!(n.binding, VmPortBinding::Tap { .. })) {
            tracing::warn!("/dev/vhost-net is not usable; tap NICs fall back to QEMU's userspace virtio-net");
        }
        let bin = bin.to_string_lossy().into_owned();
        let argv = std::iter::once(bin.clone()).chain(spec.args()).collect();
        spec.cpu_host = false;
        let fallback = glidex_vm_shim::Fallback {
            argv: std::iter::once(bin).chain(spec.args()).collect(),
            when_output_matches: CPU_REJECTED_MARKERS.iter().map(|s| s.to_string()).collect(),
        };
        Ok(LaunchArgs { argv, fallback: Some(fallback) })
    }

    fn observe(&self, api_socket: &str) -> Result<Observed, HypervisorError> {
        Ok(QmpClient::new(api_socket).observe()?)
    }

    fn pause(&self, api_socket: &str) -> Result<(), HypervisorError> {
        Ok(QmpClient::new(api_socket).stop()?)
    }

    fn resume(&self, api_socket: &str) -> Result<(), HypervisorError> {
        Ok(QmpClient::new(api_socket).cont()?)
    }

    fn add_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError> {
        Ok(QmpClient::new(api_socket).device_add_vfio(&vfio_bdf(device_path), &vfio_device_id(device_path))?)
    }

    fn remove_device(&self, api_socket: &str, device_path: &str) -> Result<(), HypervisorError> {
        Ok(QmpClient::new(api_socket).device_del(&vfio_device_id(device_path))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::qemu_img::DiskFormat;
    use crate::models::DiskBinding;

    fn config(extra: serde_json::Value) -> VmConfig {
        let mut base = serde_json::json!({
            "vcpu_count": 2, "mem_size_mib": 1024, "rootfs_path": "/disks/root.raw",
            "kernel_image_path": "/boot/vmlinux", "kernel_args": "console=ttyS0"
        });
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn spec(config: &VmConfig) -> LaunchSpec<'_> {
        LaunchSpec {
            config,
            qmp_socket: "/tmp/qmp.sock".into(),
            firmware: None,
            disks: vm_disks(config)
                .into_iter()
                .map(|disk| QemuDisk { disk, direct: false })
                .collect(),
            cpu_host: true,
            vhost_net: true,
        }
    }

    /// The values that follow each occurrence of `flag`.
    fn values<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    fn tap(id: &str, ifname: &str, mac: &str, queue_pairs: u8, mtu: Option<u16>) -> NicBinding {
        NicBinding {
            id: id.into(),
            mac: mac.into(),
            binding: VmPortBinding::Tap { ifname: ifname.into() },
            queue_pairs,
            mtu,
        }
    }

    #[test]
    fn kernel_boot_command_line() {
        let config = config(serde_json::json!({}));
        let args = spec(&config).args();
        assert_eq!(&args[..3], ["-nodefaults", "-no-user-config", "-enable-kvm"]);
        assert_eq!(values(&args, "-machine"), ["q35"]);
        assert_eq!(values(&args, "-cpu"), ["host"]);
        assert_eq!(values(&args, "-m"), ["1024M"]);
        assert_eq!(values(&args, "-smp"), ["2"]);
        assert_eq!(values(&args, "-kernel"), ["/boot/vmlinux"]);
        assert_eq!(values(&args, "-append"), ["console=ttyS0"]);
        assert_eq!(values(&args, "-qmp"), ["unix:/tmp/qmp.sock,server=on,wait=off"]);
        assert!(values(&args, "-netdev").is_empty());
        assert!(!args.iter().any(|a| a == "-no-reboot"));
        // D9: started running, not held at reset.
        assert!(!args.iter().any(|a| a == "-S"));
        assert!(values(&args, "-device").contains(&"virtio-rng-pci,rng=rng0"));
    }

    #[test]
    fn cpu_host_can_be_dropped() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.cpu_host = false;
        assert!(values(&spec.args(), "-cpu").is_empty());
    }

    #[test]
    fn disks_root_data_seed_in_order_and_backing_files_blocked() {
        let mut config = config(serde_json::json!({"cloud_init_path": "/tmp/seed,1.img"}));
        config.root_disk_binding = Some(DiskBinding { path: "/disks/root.qcow2".into(), format: DiskFormat::Qcow2, backing_files: true });
        config.data_disk_bindings = vec![
            DiskBinding { path: "/disks/data.qcow2".into(), format: DiskFormat::Qcow2, backing_files: false },
            DiskBinding { path: "/disks/data.raw".into(), format: DiskFormat::Raw, backing_files: false },
        ];
        let args = spec(&config).args();
        let blockdevs: Vec<serde_json::Value> = values(&args, "-blockdev")
            .iter()
            .map(|b| serde_json::from_str(b).unwrap())
            .collect();
        assert_eq!(blockdevs.len(), 4);
        assert_eq!(blockdevs[0], serde_json::json!({
            "driver": "qcow2", "node-name": "disk0", "read-only": false,
            "file": {"driver": "file", "filename": "/disks/root.qcow2", "read-only": false}
        }));
        // A glidex overlay may open its base image; nothing else may.
        assert_eq!(blockdevs[1]["backing"], serde_json::Value::Null);
        assert!(blockdevs[1].as_object().unwrap().contains_key("backing"));
        assert!(!blockdevs[2].as_object().unwrap().contains_key("backing"));
        assert_eq!(blockdevs[2]["driver"], "raw");
        assert_eq!(blockdevs[3]["file"]["filename"], "/tmp/seed,1.img");
        assert_eq!(blockdevs[3]["read-only"], true);
        let devices: Vec<&str> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-blk-pci"))
            .collect();
        assert_eq!(devices, [
            "virtio-blk-pci,drive=disk0,id=vd0,bootindex=1",
            "virtio-blk-pci,drive=disk1,id=vd1",
            "virtio-blk-pci,drive=disk2,id=vd2",
            "virtio-blk-pci,drive=disk3,id=vd3",
        ]);
    }

    #[test]
    fn user_supplied_qcow2_root_never_follows_backing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evil.qcow2");
        let mut qcow2 = b"QFI\xfb\x00\x00\x00\x03".to_vec();
        qcow2.resize(4096, 0);
        std::fs::write(&path, &qcow2).unwrap();
        let config = config(serde_json::json!({"rootfs_path": path.to_str().unwrap()}));
        let args = spec(&config).args();
        let root: serde_json::Value = serde_json::from_str(values(&args, "-blockdev")[0]).unwrap();
        assert_eq!(root["driver"], "qcow2");
        assert_eq!(root["backing"], serde_json::Value::Null);
    }

    #[test]
    fn direct_io_disks_bypass_the_page_cache() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.disks[0].direct = true;
        let root: serde_json::Value = serde_json::from_str(values(&spec.args(), "-blockdev")[0]).unwrap();
        assert_eq!(root["cache"], serde_json::json!({"direct": true}));
        assert_eq!(root["file"]["aio"], "native");
    }

    #[test]
    fn tap_nics() {
        let mut config = config(serde_json::json!({}));
        config.nic_bindings = vec![
            tap("net0", "gxabc-0", "02:00:00:00:00:01", 1, None),
            tap("net1", "gxabc-1", "02:00:00:00:00:02", 4, Some(9000)),
        ];
        let mut spec = spec(&config);
        let args = spec.args();
        assert_eq!(values(&args, "-netdev"), [
            "tap,id=net0,ifname=gxabc-0,script=no,downscript=no,vhost=on",
            "tap,id=net1,ifname=gxabc-1,script=no,downscript=no,vhost=on,queues=4",
        ]);
        let nics: Vec<&str> = values(&args, "-device")
            .into_iter()
            .filter(|d| d.starts_with("virtio-net-pci"))
            .collect();
        assert_eq!(nics, [
            "virtio-net-pci,netdev=net0,id=dev-net0,mac=02:00:00:00:00:01",
            "virtio-net-pci,netdev=net1,id=dev-net1,mac=02:00:00:00:00:02,host_mtu=9000,mq=on,vectors=10",
        ]);
        // Tap NICs alone don't need shared guest memory.
        assert_eq!(values(&args, "-machine"), ["q35"]);

        spec.vhost_net = false;
        assert!(values(&spec.args(), "-netdev")[0].contains("vhost=off"));
    }

    #[test]
    fn vhost_user_nic_shares_guest_memory() {
        let mut config = config(serde_json::json!({}));
        config.nic_bindings = vec![NicBinding {
            id: "net0".into(),
            mac: "02:00:00:00:00:01".into(),
            binding: VmPortBinding::VhostUser { socket: "/run/glidex/vhost/x.net0.sock".into() },
            queue_pairs: 2,
            mtu: None,
        }];
        let args = spec(&config).args();
        assert_eq!(values(&args, "-machine"), ["q35,memory-backend=mem"]);
        assert!(values(&args, "-object").contains(&"memory-backend-memfd,id=mem,size=1024M,share=on"));
        assert_eq!(values(&args, "-chardev"), ["socket,id=chr-net0,path=/run/glidex/vhost/x.net0.sock,server=on,wait=off"]);
        assert_eq!(values(&args, "-netdev"), ["vhost-user,id=net0,chardev=chr-net0,queues=2"]);
        assert!(values(&args, "-device").contains(&"virtio-net-pci,netdev=net0,id=dev-net0,mac=02:00:00:00:00:01,rx_queue_size=1024,tx_queue_size=1024,mq=on,vectors=6"));
    }

    #[test]
    fn hugepages_back_guest_memory() {
        let config = config(serde_json::json!({"hugepages": true}));
        let args = spec(&config).args();
        assert_eq!(values(&args, "-machine"), ["q35,memory-backend=mem"]);
        assert!(values(&args, "-object").contains(&"memory-backend-memfd,id=mem,size=1024M,share=on,hugetlb=on"));
    }

    #[test]
    fn firmware_boot_maps_code_and_vars() {
        let config = config(serde_json::json!({}));
        let mut spec = spec(&config);
        spec.firmware = Some(Firmware::Pflash {
            code: "/usr/share/OVMF/OVMF_CODE_4M.fd".into(),
            vars: "/var/lib/glidex/a,b.fd".into(),
            secure_boot: false,
        });
        let args = spec.args();
        assert!(values(&args, "-kernel").is_empty());
        assert!(values(&args, "-append").is_empty());
        assert_eq!(values(&args, "-drive"), [
            "if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
            "if=pflash,format=raw,unit=1,file=/var/lib/glidex/a,,b.fd",
        ]);
        assert_eq!(values(&args, "-machine"), ["q35"]);

        spec.firmware = Some(Firmware::Pflash { code: "c".into(), vars: "v".into(), secure_boot: true });
        let args = spec.args();
        assert_eq!(values(&args, "-machine"), ["q35,smm=on"]);
        assert_eq!(values(&args, "-global"), ["driver=cfi.pflash01,property=secure,value=on"]);

        spec.firmware = Some(Firmware::Bios("/usr/share/qemu/OVMF.fd".into()));
        assert_eq!(values(&spec.args(), "-bios"), ["/usr/share/qemu/OVMF.fd"]);
    }

    #[test]
    fn vfio_devices_on_the_command_line() {
        let config = config(serde_json::json!({"vfio_devices": ["/sys/bus/pci/devices/0000:41:00.0"]}));
        assert!(values(&spec(&config).args(), "-device").contains(&"vfio-pci,host=0000:41:00.0,id=_vfio_0000_41_00_0"));
    }

    #[test]
    fn vars_template_sits_next_to_code() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd", "OVMF_CODE.4m.fd", "OVMF.fd", "edk2-x86_64-code.fd"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF_CODE_4M.fd")), Some(dir.path().join("OVMF_VARS_4M.fd")));
        // No OVMF_VARS.4m.fd shipped alongside.
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF_CODE.4m.fd")), None);
        assert_eq!(ovmf_vars_template(&dir.path().join("OVMF.fd")), None);
        assert_eq!(ovmf_vars_template(&dir.path().join("edk2-x86_64-code.fd")), None);
    }

    #[test]
    fn firmware_gets_a_private_vars_copy() {
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("OVMF_CODE_4M.fd");
        std::fs::write(&code, b"code").unwrap();
        std::fs::write(dir.path().join("OVMF_VARS_4M.fd"), b"pristine").unwrap();
        let vars = dir.path().join("vm/vars.fd");
        let vars_s = vars.to_string_lossy().into_owned();

        let fw = prepare_firmware(code.to_str().unwrap(), &vars_s, None).unwrap();
        assert_eq!(fw, Firmware::Pflash {
            code: code.to_string_lossy().into_owned(),
            vars: vars.to_string_lossy().into_owned(),
            secure_boot: false,
        });
        assert_eq!(std::fs::read(&vars).unwrap(), b"pristine");

        // Variables the guest wrote survive the next boot.
        std::fs::write(&vars, b"BOOTVARS").unwrap();
        prepare_firmware(code.to_str().unwrap(), &vars_s, None).unwrap();
        assert_eq!(std::fs::read(&vars).unwrap(), b"BOOTVARS");

        assert!(matches!(
            prepare_firmware("/nonexistent/OVMF_CODE.fd", &vars_s, None),
            Err(HypervisorError::InvalidConfig(_))
        ));
    }

    /// A firmware image's own template (read-only, named by id) is used,
    /// and the VM's copy of it is writable.
    #[test]
    fn firmware_image_template_is_copied_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let code = dir.path().join("img.fd");
        let template = dir.path().join("img.vars.fd");
        std::fs::write(&code, b"code").unwrap();
        std::fs::write(&template, b"pristine").unwrap();
        std::fs::set_permissions(&template, std::fs::Permissions::from_mode(0o444)).unwrap();
        let vars = dir.path().join("vm/vars.fd");
        let vars_s = vars.to_string_lossy().into_owned();
        let fw = prepare_firmware(code.to_str().unwrap(), &vars_s, Some(template.to_str().unwrap())).unwrap();
        assert!(matches!(fw, Firmware::Pflash { .. }));
        assert_eq!(std::fs::read(&vars).unwrap(), b"pristine");
        assert_eq!(std::fs::metadata(&vars).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn version_parsing() {
        assert_eq!(parse_qemu_version("QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.2)\n"), Some((10, 2)));
        assert_eq!(parse_qemu_version("QEMU emulator version 5.2.0\n"), Some((5, 2)));
        assert_eq!(parse_qemu_version("garbage"), None);
        assert!((5, 2) < MIN_QEMU_VERSION && (10, 2) >= MIN_QEMU_VERSION);
    }

    #[test]
    fn cpu_errors_trigger_the_fallback() {
        assert!(cpu_model_rejected("qemu-system-x86_64: error: failed to set MSR 0x345 to 0x2000"));
        assert!(cpu_model_rejected("host doesn't support requested feature: CPUID.80000001H:ECX.svm"));
        assert!(!cpu_model_rejected("Could not open '/disks/root.raw': No such file or directory"));
    }
}
