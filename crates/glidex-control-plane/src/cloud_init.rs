//! Default cloud-init NoCloud seed image for firmware-booted guests.
//!
//! Distro cloud images (e.g. Ubuntu's `*-server-cloudimg-*.img`) ship with
//! no usable login and wait for cloud-init to provision one. cloud-init's
//! NoCloud datasource picks up a FAT volume labelled `CIDATA` containing
//! `meta-data`, `user-data` and `network-config`, which is what we build
//! here with `mkdosfs` + `mcopy` (the same approach as cloud-hypervisor's
//! `create-cloud-init.sh`).
//!
//! Credentials are never baked in. The guest user `cloud` gets:
//! - the host's SSH public keys (`~/.ssh/*.pub` of the control-plane user),
//! - a password only if `GLIDEX_CLOUD_INIT_PASSWD_HASH` holds a crypt(3)
//!   hash (e.g. from `openssl passwd -6`); otherwise password login is locked.

use crate::hypervisor::HypervisorError;
use std::path::Path;
use std::process::Command;

/// Environment variable holding the crypt(3) password hash for the guest user.
pub const PASSWD_HASH_ENV: &str = "GLIDEX_CLOUD_INIT_PASSWD_HASH";

/// Name of the user created in the guest.
pub const DEFAULT_USER: &str = "cloud";

/// Size of the seed volume in KiB (matches `create-cloud-init.sh`).
const SEED_SIZE_KIB: &str = "8192";

/// Inputs for the generated seed.
#[derive(Debug, Clone, Default)]
pub struct SeedConfig {
    pub instance_id: String,
    pub hostname: String,
    pub ssh_authorized_keys: Vec<String>,
    pub passwd_hash: Option<String>,
}

impl SeedConfig {
    /// Build the default seed for a VM from the host environment.
    pub fn for_vm(vm_id: &str, vm_name: &str) -> Self {
        Self {
            instance_id: vm_id.to_string(),
            hostname: sanitize_hostname(vm_name),
            ssh_authorized_keys: host_ssh_public_keys(),
            passwd_hash: std::env::var(PASSWD_HASH_ENV)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    pub fn meta_data(&self) -> String {
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            yaml_quote(&self.instance_id),
            yaml_quote(&self.hostname)
        )
    }

    pub fn network_config(&self) -> String {
        "version: 2\n\
         ethernets:\n\
         \x20 all-en:\n\
         \x20   match:\n\
         \x20     name: \"en*\"\n\
         \x20   dhcp4: true\n"
            .to_string()
    }

    pub fn user_data(&self) -> String {
        let mut s = String::from("#cloud-config\nusers:\n");
        s.push_str(&format!("  - name: {}\n", DEFAULT_USER));
        s.push_str("    sudo: ALL=(ALL) NOPASSWD:ALL\n");
        s.push_str("    shell: /bin/bash\n");
        match &self.passwd_hash {
            Some(hash) => {
                s.push_str(&format!("    passwd: {}\n", yaml_quote(hash)));
                s.push_str("    lock_passwd: false\n");
            }
            None => s.push_str("    lock_passwd: true\n"),
        }
        if !self.ssh_authorized_keys.is_empty() {
            s.push_str("    ssh_authorized_keys:\n");
            for key in &self.ssh_authorized_keys {
                s.push_str(&format!("      - {}\n", yaml_quote(key)));
            }
        }
        // `users[].passwd` is ignored when the user already exists (e.g. a
        // rootfs reused from an earlier boot); chpasswd applies either way.
        if let Some(hash) = &self.passwd_hash {
            s.push_str("chpasswd:\n  expire: false\n  users:\n");
            s.push_str(&format!("    - name: {}\n", DEFAULT_USER));
            s.push_str(&format!("      password: {}\n", yaml_quote(hash)));
            s.push_str("      type: hash\n");
        }
        s.push_str(&format!("ssh_pwauth: {}\n", self.passwd_hash.is_some()));
        s
    }
}

/// (Re)generate the seed image at `path`. Built next to `path` and renamed
/// into place so a failed build never leaves a half-written image behind.
pub fn write_seed_image(path: &str, seed: &SeedConfig) -> Result<(), HypervisorError> {
    let staging = format!("{}.d", path);
    let tmp_image = format!("{}.tmp", path);
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&tmp_image);

    let io_err = |e: std::io::Error| HypervisorError::CloudInit(format!("{}: {}", path, e));
    let result = (|| {
        std::fs::create_dir_all(&staging).map_err(io_err)?;
        let files = [
            ("meta-data", seed.meta_data()),
            ("user-data", seed.user_data()),
            ("network-config", seed.network_config()),
        ];
        for (name, contents) in &files {
            std::fs::write(Path::new(&staging).join(name), contents).map_err(io_err)?;
        }

        run_tool(
            Command::new("mkdosfs")
                .args(["-n", "CIDATA", "-C", &tmp_image, SEED_SIZE_KIB]),
        )?;
        for (name, _) in &files {
            let src = Path::new(&staging).join(name);
            run_tool(
                Command::new("mcopy")
                    .arg("-oi")
                    .arg(&tmp_image)
                    .arg(&src)
                    .arg("::"),
            )?;
        }

        std::fs::rename(&tmp_image, path).map_err(io_err)?;
        Ok(())
    })();

    let _ = std::fs::remove_dir_all(&staging);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_image);
    }
    result
}

fn run_tool(cmd: &mut Command) -> Result<(), HypervisorError> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let output = cmd.output().map_err(|e| {
        HypervisorError::CloudInit(format!(
            "failed to run {} (install dosfstools/mtools): {}",
            program, e
        ))
    })?;
    if !output.status.success() {
        return Err(HypervisorError::CloudInit(format!(
            "{} failed: {}",
            program,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Public keys from the control-plane user's `~/.ssh/*.pub`.
fn host_ssh_public_keys() -> Vec<String> {
    let Some(dir) = dirs::home_dir().map(|h| h.join(".ssh")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "pub"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|s| {
            s.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Reduce a VM name to a valid RFC 1123 hostname label.
fn sanitize_hostname(name: &str) -> String {
    let label: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let label = label.trim_matches('-');
    let label = &label[..label.len().min(63)];
    let label = label.trim_end_matches('-');
    if label.is_empty() {
        DEFAULT_USER.to_string()
    } else {
        label.to_string()
    }
}

/// Single-quoted YAML scalar (only `'` needs escaping, by doubling).
fn yaml_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_is_sanitized() {
        assert_eq!(sanitize_hostname("My_VM.01"), "my-vm-01");
        assert_eq!(sanitize_hostname("--"), "cloud");
        assert_eq!(sanitize_hostname(&"a".repeat(80)).len(), 63);
    }

    #[test]
    fn user_data_locks_password_without_hash() {
        let seed = SeedConfig {
            instance_id: "id".into(),
            hostname: "h".into(),
            ssh_authorized_keys: vec!["ssh-ed25519 AAAA test".into()],
            passwd_hash: None,
        };
        let ud = seed.user_data();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("lock_passwd: true"));
        assert!(!ud.contains("passwd: '"));
        assert!(!ud.contains("chpasswd"));
        assert!(ud.contains("- 'ssh-ed25519 AAAA test'"));
        assert!(ud.contains("ssh_pwauth: false"));
    }

    #[test]
    fn user_data_uses_hash_when_given() {
        let seed = SeedConfig {
            passwd_hash: Some("$6$salt$it's".into()),
            ..Default::default()
        };
        let ud = seed.user_data();
        assert!(ud.contains("passwd: '$6$salt$it''s'"));
        assert!(ud.contains("password: '$6$salt$it''s'\n      type: hash"));
        assert!(ud.contains("lock_passwd: false"));
        assert!(ud.contains("ssh_pwauth: true"));
        assert!(!ud.contains("ssh_authorized_keys"));
    }
}
