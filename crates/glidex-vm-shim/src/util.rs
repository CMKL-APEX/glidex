//! Host facts the shim and the control plane both need to agree on.

use std::path::Path;

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
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim().to_string())
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
            let zombie = stat.rfind(')').and_then(|i| stat[i + 1..].split_whitespace().next()) == Some("Z");
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
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
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
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ECONNREFUSED) | Some(libc::ENOTDIR)) => Ok(false),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_survives_odd_command_names() {
        let stat = "1234 (a) b (c)) S 1 1234 1234 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
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
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
