//! What a host's KVM offers a guest, and whether a VM booted on one host
//! fits another (spec/32-live-migration.md §9, decisions 4 and 11).
//!
//! A root oneshot at boot (`glidex-vm-shim --kvm-cpuid`, from
//! `glidex-kvm-cpuid.service`) asks KVM for `KVM_GET_SUPPORTED_CPUID` and
//! writes [`KvmCpuidFile`] to [`KVM_CPUID_FILE`]; the control plane, which
//! has no `/dev/kvm`, reads the file into the node's status.
//!
//! Compatibility follows Cloud Hypervisor's own check on a migration's
//! target (`CpuidFeatureEntry::checked_feature_entry_list` in v54.1): the
//! same registers and the same comparisons, so glidex refuses what Cloud
//! Hypervisor would and names the register before any memory is sent.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Where the boot step writes what KVM offers.
pub const KVM_CPUID_FILE: &str = "/run/glidex/kvm-cpuid.json";

/// One CPUID leaf and subleaf as KVM reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

/// The host's CPU and kernel, for people: they don't decide a fit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCpu {
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microcode: Option<String>,
    /// The running kernel's release, whose KVM the entries come from.
    pub kernel: String,
}

/// The file the boot step writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KvmCpuidFile {
    pub cpu: HostCpu,
    pub entries: Vec<CpuidEntry>,
}

impl KvmCpuidFile {
    pub fn read(path: &Path) -> std::io::Result<KvmCpuidFile> {
        let raw = std::fs::read(path)?;
        serde_json::from_slice(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Write atomically, readable by all: the control plane runs as
    /// another user.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        std::fs::rename(&tmp, path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reg {
    Eax,
    Ebx,
    Ecx,
    Edx,
}

impl Reg {
    fn of(self, e: &CpuidEntry) -> u32 {
        match self {
            Reg::Eax => e.eax,
            Reg::Ebx => e.ebx,
            Reg::Ecx => e.ecx,
            Reg::Edx => e.edx,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Reg::Eax => "eax",
            Reg::Ebx => "ebx",
            Reg::Ecx => "ecx",
            Reg::Edx => "edx",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// Every bit the VM has, the target has.
    Subset,
    Equal,
    /// The VM's value, as a number, is at most the target's.
    NotGreater,
}

/// Cloud Hypervisor v54.1's list: leaf, subleaf, register, comparison.
const CHECKED: &[(u32, u32, Reg, Check)] = &[
    (0x1, 0, Reg::Ecx, Check::Subset),
    (0x1, 0, Reg::Edx, Check::Subset),
    (0x7, 0, Reg::Eax, Check::NotGreater),
    (0x7, 0, Reg::Ebx, Check::Subset),
    (0x7, 0, Reg::Ecx, Check::Subset),
    (0x7, 0, Reg::Edx, Check::Subset),
    (0x7, 1, Reg::Eax, Check::Subset),
    (0x7, 1, Reg::Ecx, Check::Subset),
    (0x7, 1, Reg::Edx, Check::Subset),
    (0x7, 2, Reg::Edx, Check::Subset),
    (0xd, 0, Reg::Eax, Check::Subset),
    (0xd, 0, Reg::Edx, Check::Subset),
    (0xd, 1, Reg::Eax, Check::Subset),
    (0xd, 1, Reg::Ecx, Check::Subset),
    (0xd, 1, Reg::Edx, Check::Subset),
    (0x1d, 0, Reg::Eax, Check::NotGreater),
    (0x1e, 0, Reg::Eax, Check::NotGreater),
    (0x1e, 1, Reg::Eax, Check::Subset),
    (0x8000_0001, 0, Reg::Ecx, Check::Subset),
    (0x8000_0001, 0, Reg::Edx, Check::Subset),
    (0x4000_0000, 0, Reg::Eax, Check::NotGreater),
    (0x4000_0000, 0, Reg::Ebx, Check::Equal),
    (0x4000_0000, 0, Reg::Ecx, Check::Equal),
    (0x4000_0000, 0, Reg::Edx, Check::Equal),
    (0x4000_0001, 0, Reg::Eax, Check::Subset),
    (0x4000_0001, 0, Reg::Ebx, Check::Subset),
    (0x4000_0001, 0, Reg::Ecx, Check::Subset),
    (0x4000_0001, 0, Reg::Edx, Check::Subset),
];

/// One checked register's value: what node and VM status keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuidValue {
    pub leaf: u32,
    pub subleaf: u32,
    pub reg: Reg,
    pub value: u32,
}

/// The checked registers of `entries`, in the list's order; a leaf KVM
/// doesn't report counts as zero, as Cloud Hypervisor counts it.
pub fn checked_values(entries: &[CpuidEntry]) -> Vec<CpuidValue> {
    CHECKED
        .iter()
        .map(|&(leaf, subleaf, reg, _)| CpuidValue {
            leaf,
            subleaf,
            reg,
            value: entries
                .iter()
                .find(|e| e.function == leaf && e.index == subleaf)
                .map(|e| reg.of(e))
                .unwrap_or(0),
        })
        .collect()
}

/// The first register where a VM doesn't fit a target.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.describe())]
pub struct Misfit {
    pub leaf: u32,
    pub subleaf: u32,
    pub reg: Reg,
    pub vm: u32,
    pub target: u32,
}

impl Misfit {
    fn describe(&self) -> String {
        let at = format!(
            "CPUID leaf {:#x} subleaf {} {}",
            self.leaf,
            self.subleaf,
            self.reg.name()
        );
        let missing = self.vm & !self.target;
        if missing != 0 && missing != self.vm {
            format!(
                "{at}: the node lacks bits {missing:#010x} the VM booted with ({:#010x} against {:#010x})",
                self.vm, self.target
            )
        } else {
            format!(
                "{at}: the VM booted with {:#010x}, the node offers {:#010x}",
                self.vm, self.target
            )
        }
    }
}

/// Whether a VM that booted with `vm` can run where KVM offers `target`.
/// A value absent from either side counts as zero.
pub fn fits(vm: &[CpuidValue], target: &[CpuidValue]) -> Result<(), Misfit> {
    let get = |of: &[CpuidValue], leaf, subleaf, reg| {
        of.iter()
            .find(|v| v.leaf == leaf && v.subleaf == subleaf && v.reg == reg)
            .map(|v| v.value)
            .unwrap_or(0)
    };
    for &(leaf, subleaf, reg, check) in CHECKED {
        let (v, t) = (get(vm, leaf, subleaf, reg), get(target, leaf, subleaf, reg));
        let ok = match check {
            Check::Subset => v & !t == 0,
            Check::Equal => v == t,
            Check::NotGreater => v <= t,
        };
        if !ok {
            return Err(Misfit {
                leaf,
                subleaf,
                reg,
                vm: v,
                target: t,
            });
        }
    }
    Ok(())
}

/// Ask KVM what it offers, and describe the host: what the boot step
/// writes. Needs read access to `/dev/kvm`.
#[cfg(target_arch = "x86_64")]
pub fn read_host(kvm: &Path) -> std::io::Result<KvmCpuidFile> {
    Ok(KvmCpuidFile {
        cpu: host_cpu(),
        entries: kvm_supported_cpuid(kvm)?,
    })
}

#[cfg(not(target_arch = "x86_64"))]
pub fn read_host(_kvm: &Path) -> std::io::Result<KvmCpuidFile> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "CPUID is x86-only",
    ))
}

/// `KVM_GET_SUPPORTED_CPUID`: `_IOWR(KVMIO, 0x05, struct kvm_cpuid2)`.
#[cfg(target_arch = "x86_64")]
const KVM_GET_SUPPORTED_CPUID: libc::c_ulong = 0xC008_AE05;

#[cfg(target_arch = "x86_64")]
fn kvm_supported_cpuid(kvm: &Path) -> std::io::Result<Vec<CpuidEntry>> {
    use std::os::fd::AsRawFd;
    // struct kvm_cpuid2 { u32 nent; u32 padding; struct kvm_cpuid_entry2
    // entries[]; }, each entry { function, index, flags, eax, ebx, ecx,
    // edx, padding[3] }: ten u32s.
    // A few fields are the calling CPU's (its APIC id in leaf 0x1 EBX and
    // leaf 0xb EDX), so the file differs run to run; none is compared.
    const ENTRY_WORDS: usize = 10;
    let dev = std::fs::File::open(kvm)?;
    let mut nent = 128usize;
    loop {
        let mut buf = vec![0u32; 2 + nent * ENTRY_WORDS];
        buf[0] = nent as u32;
        // SAFETY: `buf` is a kvm_cpuid2 with room for `nent` entries, as
        // its first word says; KVM writes at most that many.
        let r = unsafe { libc::ioctl(dev.as_raw_fd(), KVM_GET_SUPPORTED_CPUID, buf.as_mut_ptr()) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::E2BIG) && nent < 4096 {
                nent *= 2;
                continue;
            }
            return Err(e);
        }
        let n = (buf[0] as usize).min(nent);
        return Ok(buf[2..2 + n * ENTRY_WORDS]
            .as_chunks::<ENTRY_WORDS>()
            .0
            .iter()
            .map(|w| CpuidEntry {
                function: w[0],
                index: w[1],
                eax: w[3],
                ebx: w[4],
                ecx: w[5],
                edx: w[6],
            })
            .collect());
    }
}

#[cfg(target_arch = "x86_64")]
fn host_cpu() -> HostCpu {
    use std::arch::x86_64::__cpuid;
    let (l0, l1) = (__cpuid(0), __cpuid(1));
    let vendor: Vec<u8> = [l0.ebx, l0.edx, l0.ecx]
        .iter()
        .flat_map(|r| r.to_le_bytes())
        .collect();
    let base_family = (l1.eax >> 8) & 0xf;
    let base_model = (l1.eax >> 4) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((l1.eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0x6 || base_family == 0xf {
        base_model | (((l1.eax >> 16) & 0xf) << 4)
    } else {
        base_model
    };
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let field = |name: &str| {
        cpuinfo.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == name).then(|| v.trim().to_string())
        })
    };
    HostCpu {
        vendor: String::from_utf8_lossy(&vendor).into_owned(),
        family,
        model,
        stepping: l1.eax & 0xf,
        model_name: field("model name").unwrap_or_default(),
        microcode: field("microcode"),
        kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(function: u32, index: u32, regs: [u32; 4]) -> CpuidEntry {
        CpuidEntry {
            function,
            index,
            eax: regs[0],
            ebx: regs[1],
            ecx: regs[2],
            edx: regs[3],
        }
    }

    /// The spike's two hosts (spec/32 §0 #7): the same CPU model on
    /// kernels 6.8 and 6.17, whose KVMs differ in leaf 0x7 EDX bit 26.
    fn hosts() -> (Vec<CpuidValue>, Vec<CpuidValue>) {
        let base = |edx7| {
            vec![
                entry(0x1, 0, [0x830f10, 0x800, 0xf6d8320b, 0x178bfbff]),
                entry(0x7, 0, [0x1, 0x219c07a9, 0x40069c, edx7]),
                entry(0x8000_0001, 0, [0x830f10, 0, 0x75c237ff, 0x2fd3fbff]),
            ]
        };
        (
            checked_values(&base(0xac00_0000)),
            checked_values(&base(0xa800_0000)),
        )
    }

    #[test]
    fn compatibility_is_one_way() {
        let (old_kernel, new_kernel) = hosts();
        let m = fits(&old_kernel, &new_kernel).unwrap_err();
        assert_eq!((m.leaf, m.subleaf, m.reg), (0x7, 0, Reg::Edx));
        assert_eq!(m.vm & !m.target, 1 << 26);
        assert!(m.to_string().contains("lacks bits 0x04000000"), "{m}");
        assert_eq!(fits(&new_kernel, &old_kernel), Ok(()));
        assert_eq!(fits(&old_kernel, &old_kernel), Ok(()));
    }

    #[test]
    fn each_comparison_is_cloud_hypervisors() {
        let one = |leaf, subleaf, reg, value| {
            vec![CpuidValue {
                leaf,
                subleaf,
                reg,
                value,
            }]
        };
        // Leaf 0x7 EAX is the highest subleaf: a number, not bits.
        assert!(fits(&one(0x7, 0, Reg::Eax, 1), &one(0x7, 0, Reg::Eax, 2)).is_ok());
        assert!(fits(&one(0x7, 0, Reg::Eax, 2), &one(0x7, 0, Reg::Eax, 1)).is_err());
        // The KVM signature must be the same.
        let sig = |v| one(0x4000_0000, 0, Reg::Ebx, v);
        assert!(fits(&sig(0x4b4d564b), &sig(0x4b4d564b)).is_ok());
        assert!(fits(&sig(0x4b4d564b), &sig(0)).is_err());
        assert!(fits(&sig(0), &sig(0x4b4d564b)).is_err());
        // A register nobody lists counts as zero on both sides.
        assert!(fits(&[], &[]).is_ok());
        assert!(fits(&one(0x7, 0, Reg::Ebx, 1), &[]).is_err());
    }

    #[test]
    fn checked_values_keep_the_list_and_count_missing_leaves_as_zero() {
        let v = checked_values(&[entry(0x7, 0, [0, 0, 0, 0xac00_0000])]);
        assert_eq!(v.len(), CHECKED.len());
        assert!(
            v.iter()
                .any(|c| c.leaf == 0x7 && c.reg == Reg::Edx && c.value == 0xac00_0000)
        );
        assert!(v.iter().filter(|c| c.leaf != 0x7).all(|c| c.value == 0));
    }

    #[test]
    fn the_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kvm-cpuid.json");
        let f = KvmCpuidFile {
            cpu: HostCpu {
                vendor: "AuthenticAMD".into(),
                family: 0x17,
                model: 0x31,
                stepping: 0,
                model_name: "AMD EPYC 7742 64-Core Processor".into(),
                microcode: Some("0x830107c".into()),
                kernel: "6.17.0-42-generic".into(),
            },
            entries: vec![entry(0x7, 0, [1, 2, 3, 4])],
        };
        f.write(&path).unwrap();
        assert_eq!(KvmCpuidFile::read(&path).unwrap(), f);
    }
}
