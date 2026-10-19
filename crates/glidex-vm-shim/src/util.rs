//! Host facts the shim and the control plane both need to agree on.

use std::path::Path;

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Seconds since the epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// This boot's id (`/proc/sys/kernel/random/boot_id`). Pids and start
/// times recorded under another boot id name nothing.
pub fn boot_id() -> std::io::Result<String> {
    if let Ok(id) = std::env::var("GLIDEX_TEST_BOOT_ID") {
        // Tests simulate a host reboot by changing it (spec §19).
        return Ok(id);
    }
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_string())
}

/// The start time of `pid` (field 22 of `/proc/<pid>/stat`, in clock
/// ticks since boot): together with the pid it names one process, even
/// after the pid is reused. `Ok(None)`: no such process.
pub fn proc_starttime(pid: u32) -> std::io::Result<Option<u64>> {
    let stat = match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(parse_starttime(&stat))
}

fn parse_starttime(stat: &str) -> Option<u64> {
    // The command name (field 2) is in parentheses and may contain spaces
    // or parentheses itself, so count fields from the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts with field 3 (state): starttime is field 22.
    rest.split_whitespace().nth(22 - 3)?.parse().ok()
}

/// Whether `pid` is alive and still the process that started at
/// `starttime`. `Err`: `/proc` could not be read (the caller cannot tell).
pub fn same_process(pid: u32, starttime: u64) -> std::io::Result<bool> {
    // A zombie keeps its /proc entry; it is gone as far as we're concerned.
    match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
        Ok(stat) => {
            let zombie = stat
                .rfind(')')
                .and_then(|i| stat[i + 1..].split_whitespace().next())
                == Some("Z");
            Ok(!zombie && parse_starttime(&stat) == Some(starttime))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Write `bytes` to `path` atomically (temporary file + rename), mode 0600.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Whether a Unix socket at `path` accepts a connection. `Err` for
/// anything other than "nobody listens" (ENOENT, ECONNREFUSED): the caller
/// cannot tell (spec §8.7).
pub fn socket_accepts(path: &Path) -> std::io::Result<bool> {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(libc::ENOENT) | Some(libc::ECONNREFUSED) | Some(libc::ENOTDIR)
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// CPU ids a `cpu_set_t` can hold; `launch.json` rejects larger ones.
pub const CPU_SETSIZE: u32 = libc::CPU_SETSIZE as u32;

/// A `cpu_set_t` holding `cpus`. Ids of `CPU_SETSIZE` or more are left
/// out (validation has already refused them).
pub fn cpu_set(cpus: &[u32]) -> libc::cpu_set_t {
    // SAFETY: cpu_set_t is a plain bit array; all zeroes is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &c in cpus.iter().filter(|&&c| c < CPU_SETSIZE) {
        // SAFETY: `c` is below CPU_SETSIZE, so the bit is in the array.
        unsafe { libc::CPU_SET(c as usize, &mut set) };
    }
    set
}

/// `64-71,192-199`: the kernel's cpu list format, for notes and errors.
pub fn cpu_list(cpus: &[u32]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        while i + 1 < sorted.len() && sorted[i + 1] == sorted[i] + 1 {
            i += 1;
        }
        out.push(if sorted[i] == start {
            start.to_string()
        } else {
            format!("{}-{}", start, sorted[i])
        });
        i += 1;
    }
    out.join(",")
}

/// `sched_setaffinity` on one thread (`tid` 0: the caller).
pub fn set_thread_affinity(tid: libc::pid_t, set: &libc::cpu_set_t) -> std::io::Result<()> {
    // SAFETY: `set` is a valid cpu_set_t of the size passed.
    if unsafe { libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), set) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Bind every thread of `pid` to `cpus`; returns how many were bound.
/// A thread that exits meanwhile is skipped. Threads started while we
/// walk the list may have copied the old mask, so walk until no new ones
/// turn up.
pub fn set_process_affinity(pid: u32, cpus: &[u32]) -> std::io::Result<u64> {
    let set = cpu_set(cpus);
    let mut done = std::collections::BTreeSet::new();
    for _ in 0..10 {
        let mut fresh = false;
        for entry in std::fs::read_dir(format!("/proc/{}/task", pid))? {
            let Some(tid) = entry?
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            if done.contains(&tid) {
                continue;
            }
            match set_thread_affinity(tid, &set) {
                Ok(()) => {
                    done.insert(tid);
                    fresh = true;
                }
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                Err(e) => return Err(e),
            }
        }
        if !fresh {
            break;
        }
    }
    Ok(done.len() as u64)
}

/// The CPUs the calling thread may run on.
pub fn own_affinity() -> std::io::Result<Vec<u32>> {
    // SAFETY: as in `cpu_set`; the kernel fills it in.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((0..CPU_SETSIZE)
        // SAFETY: `c` is below CPU_SETSIZE.
        .filter(|&c| unsafe { libc::CPU_ISSET(c as usize, &set) })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_sets_and_lists() {
        let set = cpu_set(&[0, 3, 70, 1023, 1024, 5000]);
        let has = |c: usize| unsafe { libc::CPU_ISSET(c, &set) };
        assert!(has(0) && has(3) && has(70) && has(1023));
        assert!(!has(1) && !has(69));
        assert_eq!(unsafe { libc::CPU_COUNT(&set) }, 4);
        assert_eq!(
            cpu_list(&[71, 64, 65, 66, 67, 68, 69, 70, 192, 194, 193, 5]),
            "5,64-71,192-194"
        );
        assert_eq!(cpu_list(&[3, 3]), "3");
        assert_eq!(cpu_list(&[]), "");
    }

    /// `Cpus_allowed_list` of every thread of `pid`.
    fn allowed_lists(pid: u32) -> Vec<String> {
        std::fs::read_dir(format!("/proc/{}/task", pid))
            .unwrap()
            .map(|t| {
                let status = std::fs::read_to_string(t.unwrap().path().join("status")).unwrap();
                status
                    .lines()
                    .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
                    .unwrap()
                    .trim()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_running_process_is_rebound() {
        let cpu = *own_affinity().unwrap().last().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let r = set_process_affinity(child.id(), &[cpu]);
        let lists = allowed_lists(child.id());
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(r.unwrap(), 1);
        assert_eq!(lists, [cpu.to_string()]);
        assert!(set_process_affinity(u32::MAX - 1, &[cpu]).is_err());
    }

    #[test]
    fn starttime_survives_odd_command_names() {
        let stat =
            "1234 (a) b (c)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_starttime(stat), Some(987654));
        assert_eq!(parse_starttime("garbage"), None);
    }

    #[test]
    fn own_process_is_found_and_reused_pids_are_not() {
        let me = std::process::id();
        let st = proc_starttime(me).unwrap().unwrap();
        assert!(same_process(me, st).unwrap());
        assert!(!same_process(me, st + 1).unwrap());
        assert!(!same_process(u32::MAX - 1, st).unwrap());
    }

    #[test]
    fn sockets_nobody_listens_on() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.sock");
        assert!(!socket_accepts(&p).unwrap());
        let l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        assert!(socket_accepts(&p).unwrap());
        drop(l);
        assert!(!socket_accepts(&p).unwrap());
    }

    #[test]
    fn atomic_writes_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        write_atomic(&p, b"{}").unwrap();
        write_atomic(&p, b"[1]").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"[1]");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
