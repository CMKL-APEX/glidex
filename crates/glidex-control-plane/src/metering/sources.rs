//! Where readings come from (spec/metering.md §5): a VM unit's cgroup,
//! or `/proc` for detached instances. Paths are rooted so tests can point
//! them at fixtures.

use std::path::{Path, PathBuf};

/// CPU and memory of one instance at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmUsage {
    /// Cumulative CPU time, µs.
    pub cpu_usec: u64,
    /// Memory in use, bytes (excluding hugepages, §5.1).
    pub memory_bytes: u64,
    /// `true` when read from `/proc` rather than a cgroup (`source_proc`).
    pub from_proc: bool,
}

/// Root of the host's filesystems (`/` in production).
#[derive(Debug, Clone)]
pub struct Host {
    pub proc_root: PathBuf,
    pub cgroup_root: PathBuf,
}

impl Default for Host {
    fn default() -> Self {
        Self { proc_root: "/proc".into(), cgroup_root: "/sys/fs/cgroup".into() }
    }
}

impl Host {
    /// The cgroup v2 path of `pid` (`0::/glidex.slice/…` in
    /// `/proc/<pid>/cgroup`), as a directory under `cgroup_root`.
    ///
    /// Read from the shim's own entry rather than the unit's D-Bus
    /// `ControlGroup` property: same answer, no D-Bus round trip, and the
    /// pid is already verified by start time (§5.1).
    pub fn cgroup_of(&self, pid: u32) -> Option<PathBuf> {
        let text = std::fs::read_to_string(self.proc_root.join(pid.to_string()).join("cgroup")).ok()?;
        let path = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        let rel = path.trim_start_matches('/');
        if rel.is_empty() || rel.split('/').any(|c| c == "..") {
            return None;
        }
        Some(self.cgroup_root.join(rel))
    }

    /// CPU and memory of a systemd-run instance, from its unit's cgroup.
    pub fn cgroup_usage(&self, cgroup: &Path) -> Option<VmUsage> {
        let cpu = std::fs::read_to_string(cgroup.join("cpu.stat")).ok()?;
        let cpu_usec = cpu.lines().find_map(|l| l.strip_prefix("usage_usec "))?.trim().parse().ok()?;
        let memory_bytes = read_u64(&cgroup.join("memory.current")).unwrap_or(0);
        Some(VmUsage { cpu_usec, memory_bytes, from_proc: false })
    }

    /// CPU and memory of one process (a detached instance's hypervisor),
    /// if it is still `starttime` (pid reuse check).
    pub fn proc_usage(&self, pid: u32, starttime: Option<u64>) -> Option<VmUsage> {
        let dir = self.proc_root.join(pid.to_string());
        let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
        // Fields after the command name, which may contain spaces and ')'.
        let rest = &stat[stat.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        // rest[0] is field 3 (state): utime = 14, stime = 15, starttime = 22.
        let field = |n: usize| f.get(n - 3)?.parse::<u64>().ok();
        if let Some(want) = starttime {
            if field(22)? != want {
                return None;
            }
        }
        let ticks = field(14)? + field(15)?;
        let hz = clock_ticks();
        let status = std::fs::read_to_string(dir.join("status")).ok()?;
        let rss_kib: u64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0);
        Some(VmUsage { cpu_usec: ticks * 1_000_000 / hz, memory_bytes: rss_kib * 1024, from_proc: true })
    }
}

fn read_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz > 0 { hz as u64 } else { 100 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (tempfile::TempDir, Host) {
        let dir = tempfile::TempDir::new().unwrap();
        let host = Host { proc_root: dir.path().join("proc"), cgroup_root: dir.path().join("cg") };
        (dir, host)
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn cgroup_from_proc_and_usage() {
        let (_d, h) = host();
        write(&h.proc_root.join("42/cgroup"), "0::/glidex.slice/glidex-vms.slice/glidex-vm@abc.service\n");
        let cg = h.cgroup_of(42).unwrap();
        assert_eq!(cg, h.cgroup_root.join("glidex.slice/glidex-vms.slice/glidex-vm@abc.service"));
        write(&cg.join("cpu.stat"), "usage_usec 123456789\nuser_usec 100000000\nsystem_usec 23456789\nnr_periods 0\n");
        write(&cg.join("memory.current"), "536870912\n");
        assert_eq!(h.cgroup_usage(&cg), Some(VmUsage { cpu_usec: 123456789, memory_bytes: 512 << 20, from_proc: false }));
    }

    #[test]
    fn cgroup_paths_are_confined() {
        let (_d, h) = host();
        write(&h.proc_root.join("7/cgroup"), "0::/../../etc\n");
        assert_eq!(h.cgroup_of(7), None);
        write(&h.proc_root.join("8/cgroup"), "0::/\n");
        assert_eq!(h.cgroup_of(8), None);
        assert_eq!(h.cgroup_of(9), None);
    }

    /// On a host with running VMs: `cargo test -- --ignored live_vm_cgroups`.
    #[test]
    #[ignore = "needs running glidex VMs"]
    fn live_vm_cgroups() {
        let h = Host::default();
        let mut seen = 0;
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
            let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else { continue };
            if comm.trim() != "glidex-vm-shim" {
                continue;
            }
            let cg = h.cgroup_of(pid).expect("shim cgroup");
            let u = h.cgroup_usage(&cg).expect("cgroup usage");
            println!("{}: cpu {} s, memory {} MiB", cg.file_name().unwrap().to_string_lossy(), u.cpu_usec / 1_000_000, u.memory_bytes >> 20);
            assert!(u.cpu_usec > 0 && u.memory_bytes > 0);
            seen += 1;
        }
        assert!(seen > 0, "no glidex-vm-shim running");
    }

    #[test]
    fn proc_usage_checks_starttime_and_parses_odd_names() {
        let (_d, h) = host();
        let hz = clock_ticks();
        // utime 300, stime 100 ticks; starttime 5555; comm with spaces and ')'.
        let stat = format!("99 (qemu sys) x) S 1 99 99 0 -1 4194560 0 0 0 0 300 100 0 0 20 0 3 0 5555 0 0");
        write(&h.proc_root.join("99/stat"), &stat);
        write(&h.proc_root.join("99/status"), "Name:\tqemu\nVmRSS:\t  204800 kB\n");
        let u = h.proc_usage(99, Some(5555)).unwrap();
        assert_eq!(u, VmUsage { cpu_usec: 400 * 1_000_000 / hz, memory_bytes: 200 << 20, from_proc: true });
        assert_eq!(h.proc_usage(99, Some(1)), None, "pid reused");
        assert_eq!(h.proc_usage(100, None), None);
    }
}
