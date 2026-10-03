//! A user's own SSH public keys (`~/.ssh/*.pub`), for prefilling guest
//! login credentials (spec/security.md §5.3).
//!
//! authd runs as root, so the files are read in a fresh thread whose
//! filesystem identity (`setfsuid`/`setfsgid`, per thread on Linux) is
//! switched to the user: the kernel checks every path with the user's own
//! permissions. On top of that only regular files owned by the user, opened
//! without following a final symlink, up to 16 KiB each, are read, and only
//! lines that look like public keys are returned.

use nix::unistd::User;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAX_FILE: u64 = 16 * 1024;
const MAX_FILES: usize = 32;
const MAX_KEYS: usize = 20;

/// Key types accepted in a `.pub` line.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

pub trait KeyReader: Send + Sync {
    /// Public key lines from `user`'s `~/.ssh/*.pub`; empty when there are
    /// none.
    fn public_keys(&self, user: &str) -> Result<Vec<String>, String>;
}

/// Reads the host's home directories.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemKeyReader;

impl KeyReader for SystemKeyReader {
    fn public_keys(&self, user: &str) -> Result<Vec<String>, String> {
        let u = User::from_name(user)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "no such user".to_string())?;
        let (uid, gid) = (u.uid.as_raw(), u.gid.as_raw());
        let dir = u.dir.join(".ssh");
        std::thread::spawn(move || {
            // SAFETY: setfsgid/setfsuid only change this thread's
            // filesystem credentials; the thread ends right after.
            unsafe {
                libc::setfsgid(gid);
                libc::setfsuid(uid);
                // Calling again returns the current value: check it took.
                if libc::setfsuid(uid) as u32 != uid || libc::setfsgid(gid) as u32 != gid {
                    return Err("could not switch to the user's file identity".to_string());
                }
            }
            read_keys(&dir, uid)
        })
        .join()
        .map_err(|_| "key reader panicked".to_string())?
    }
}

/// Whether `line` is one public key: `<type> <base64> [comment]`.
pub fn is_public_key_line(line: &str) -> bool {
    let mut parts = line.split_whitespace();
    let (Some(ty), Some(blob)) = (parts.next(), parts.next()) else { return false };
    KEY_TYPES.contains(&ty)
        && blob.len() >= 16
        && blob.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
        && line.len() <= 8 * 1024
        && !line.contains(|c: char| c.is_control())
}

/// Public keys from the `*.pub` files in `dir` owned by `uid`.
pub fn read_keys(dir: &Path, uid: u32) -> Result<Vec<String>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("~/.ssh: {}", e)),
    };
    let mut names: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_str().is_some_and(|n| n.ends_with(".pub") && !n.starts_with('.')))
        .collect();
    names.sort();
    let mut keys = Vec::new();
    for name in names.into_iter().take(MAX_FILES) {
        let path = dir.join(&name);
        let Ok(mut f) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        else {
            continue;
        };
        let Ok(meta) = f.metadata() else { continue };
        if !meta.is_file() || meta.uid() != uid || meta.len() > MAX_FILE {
            continue;
        }
        let mut text = String::new();
        if f.by_ref().take(MAX_FILE).read_to_string(&mut text).is_err() {
            continue;
        }
        for line in text.lines().map(str::trim) {
            if is_public_key_line(line) && !keys.iter().any(|k| k == line) {
                keys.push(line.to_string());
                if keys.len() >= MAX_KEYS {
                    return Ok(keys);
                }
            }
        }
    }
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl alice@host";

    #[test]
    fn key_lines() {
        assert!(is_public_key_line(ED));
        assert!(is_public_key_line("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ"));
        for bad in ["", "-----BEGIN OPENSSH PRIVATE KEY-----", "ssh-ed25519", "ssh-dss AAAAB3NzaC1kc3MAAACBAP", "ssh-ed25519 not*base64*at-all!!", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA\u{1b}[31m"] {
            assert!(!is_public_key_line(bad), "{bad:?}");
        }
    }

    #[test]
    fn reads_only_owned_regular_pub_files() {
        let me = nix::unistd::getuid().as_raw();
        let dir = tempfile::TempDir::new().unwrap();
        let ssh = dir.path();
        std::fs::write(ssh.join("id_ed25519.pub"), format!("{}\n", ED)).unwrap();
        std::fs::write(ssh.join("id_ed25519"), "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n").unwrap();
        std::fs::write(ssh.join("notes.pub"), "not a key\n").unwrap();
        std::fs::write(ssh.join("dup.pub"), format!("{}\n", ED)).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", ssh.join("link.pub")).unwrap();
        std::fs::create_dir(ssh.join("dir.pub")).unwrap();
        std::fs::write(ssh.join("big.pub"), "x".repeat(20_000)).unwrap();
        let keys = read_keys(ssh, me).unwrap();
        assert_eq!(keys, vec![ED.to_string()], "private keys, symlinks, junk and dups are skipped");
        // Files owned by someone else are skipped.
        assert!(read_keys(ssh, me + 1).unwrap().is_empty());
        // No ~/.ssh is no keys, not an error.
        assert!(read_keys(&ssh.join("missing"), me).unwrap().is_empty());
    }
}
