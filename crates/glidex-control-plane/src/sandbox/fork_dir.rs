//! A fork's restore directory (spec/33-agent-sandboxes.md D18).
//!
//! Cloud Hypervisor restores a VM from one directory: `config.json` (its
//! devices), `state.json` (each device's state, found by its id) and the
//! memory. Nothing at restore points a disk, a vhost-user socket, a tap or
//! the seed elsewhere, and a NIC's MAC is in its saved state (§0, finding
//! 9). So each fork gets a directory of its own: the checkpoint's two JSON
//! files rewritten for the fork's devices, and its memory linked. Cloud
//! Hypervisor doesn't check that every device it restores has a saved
//! state, nor glidex's mapping that every parent path was replaced: both
//! are checked here, and a fork that would still touch its parent's
//! devices is refused.

use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// What a fork uses in place of its parent's devices.
#[derive(Debug, Clone, Default)]
pub struct ForkMapping {
    pub parent_vm: String,
    pub fork_vm: String,
    /// Parent's hyperstor volume id → the fork's clone's.
    pub volumes: BTreeMap<String, String>,
    /// The parent's seed disk path and the fork's.
    pub seed: Option<(String, String)>,
    /// Each NIC, by its id in the snapshot (`net0`…).
    pub nics: BTreeMap<String, ForkNic>,
}

/// A fork's NIC: its own backend, and its own MAC (D14).
#[derive(Debug, Clone)]
pub struct ForkNic {
    /// A tap NIC's interface name, or a vhost-user NIC's socket.
    pub backend: NicBackend,
    pub mac: [u8; 6],
}

#[derive(Debug, Clone)]
pub enum NicBackend {
    Tap(String),
    VhostUser(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ForkDirError {
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("the checkpoint's {0} isn't Cloud Hypervisor's: {1}")]
    Format(&'static str, String),
    #[error("refusing the fork: {0}")]
    Refused(String),
}

const VHOST_DIR: &str = "/run/hyperstor/vhost/";

/// Write fork `m.fork_vm`'s restore directory `fork_dir` (which must not
/// exist yet) from checkpoint directory `checkpoint`. The memory files
/// are hard-linked, so both must be on one filesystem.
pub fn write_fork_dir(
    checkpoint: &Path,
    fork_dir: &Path,
    m: &ForkMapping,
) -> Result<(), ForkDirError> {
    let read = |name: &'static str| -> Result<Value, ForkDirError> {
        let p = checkpoint.join(name);
        let text = std::fs::read_to_string(&p)
            .map_err(|e| ForkDirError::Io(p.display().to_string(), e))?;
        serde_json::from_str(&text).map_err(|e| ForkDirError::Format(name, e.to_string()))
    };
    let mut config = read("config.json")?;
    let mut state = read("state.json")?;
    rewrite(&mut config, &mut state, m)?;

    let io = |p: &Path, e| ForkDirError::Io(p.display().to_string(), e);
    std::fs::create_dir(fork_dir).map_err(|e| io(fork_dir, e))?;
    let written = (|| {
        for (name, v) in [("config.json", &config), ("state.json", &state)] {
            let p = fork_dir.join(name);
            write_private(&p, &serde_json::to_vec(v).expect("json")).map_err(|e| io(&p, e))?;
        }
        let entries = std::fs::read_dir(checkpoint).map_err(|e| io(checkpoint, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io(checkpoint, e))?;
            let name = entry.file_name();
            if name == "config.json" || name == "state.json" {
                continue;
            }
            let (from, to) = (entry.path(), fork_dir.join(&name));
            std::fs::hard_link(&from, &to).map_err(|e| io(&from, e))?;
        }
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(fork_dir);
    }
    written
}

fn write_private(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(bytes)
}

/// Rewrite `config` (a snapshot's `config.json`) and `state` (its
/// `state.json`) for the fork, and check the result.
pub fn rewrite(config: &mut Value, state: &mut Value, m: &ForkMapping) -> Result<(), ForkDirError> {
    let refused = |s: String| ForkDirError::Refused(s);
    // Disks: a hyperstor volume by its id in the socket path, the seed by
    // its path; anything else has no fork of its own.
    let mut disk_ids = Vec::new();
    for disk in array_mut(config, "disks")? {
        let id = str_field(disk, "id")?;
        disk_ids.push(id.clone());
        if let Some(sock) = disk.get("vhost_socket").and_then(Value::as_str) {
            let parent = sock
                .strip_prefix(VHOST_DIR)
                .and_then(|s| s.strip_suffix(".sock"))
                .ok_or_else(|| {
                    refused(format!(
                        "disk {id}'s socket {sock} isn't a hyperstor volume's"
                    ))
                })?;
            let fork = m.volumes.get(parent).ok_or_else(|| {
                refused(format!(
                    "disk {id} (volume {parent}) has no clone for the fork"
                ))
            })?;
            disk["vhost_socket"] = Value::from(format!("{VHOST_DIR}{fork}.sock"));
        } else if let Some(path) = disk.get("path").and_then(Value::as_str) {
            match &m.seed {
                Some((parent, fork)) if path == parent => disk["path"] = Value::from(fork.clone()),
                _ => {
                    return Err(refused(format!(
                        "disk {id} at {path} has no copy for the fork"
                    )));
                }
            }
        } else {
            return Err(refused(format!("disk {id} names no volume")));
        }
    }
    // NICs: the fork's backend in the config, its MAC in the saved state
    // (the config's `mac` is ignored at restore, but kept true).
    let mut nic_ids = Vec::new();
    if config.get("net").is_some_and(|n| !n.is_null()) {
        for nic in array_mut(config, "net")? {
            let id = str_field(nic, "id")?;
            let f = m
                .nics
                .get(&id)
                .ok_or_else(|| refused(format!("NIC {id} has nothing for the fork")))?;
            let vhost = nic
                .get("vhost_user")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            match (&f.backend, vhost) {
                (NicBackend::Tap(t), false) => nic["tap"] = Value::from(t.clone()),
                (NicBackend::VhostUser(s), true) => nic["vhost_socket"] = Value::from(s.clone()),
                _ => {
                    return Err(refused(format!(
                        "NIC {id}'s backend kind differs from its parent's"
                    )));
                }
            }
            nic["mac"] = Value::from(mac_string(&f.mac));
            nic_ids.push(id);
        }
    }
    // A Pylon VM names itself to pylond (D16).
    if let Some(p) = config.get_mut("pylon").filter(|p| p.is_object())
        && p.get("vm_id").and_then(Value::as_str) == Some(m.parent_vm.as_str())
    {
        p["vm_id"] = Value::from(m.fork_vm.clone());
    }

    // The saved state: every device the config restores must have one.
    let devices = state
        .pointer_mut("/snapshots/device-manager/snapshots")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ForkDirError::Format("state.json", "no device-manager".into()))?;
    for id in disk_ids.iter().chain(&nic_ids) {
        if !devices.contains_key(id) {
            return Err(refused(format!(
                "device {id} has no saved state: it would start fresh"
            )));
        }
    }
    for id in &nic_ids {
        let mac = m.nics[id].mac;
        edit_state(devices.get_mut(id).expect("checked"), |s| {
            let cfg = s
                .get_mut("config")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| {
                    ForkDirError::Format("state.json", format!("NIC {id} has no config"))
                })?;
            cfg.insert("mac".into(), Value::from(mac.to_vec()));
            Ok(())
        })?;
    }
    if let Some((parent, fork)) = &m.seed {
        for id in &disk_ids {
            edit_state(devices.get_mut(id).expect("checked"), |s| {
                if s.get("disk_path").and_then(Value::as_str) == Some(parent.as_str()) {
                    s["disk_path"] = Value::from(fork.clone());
                }
                Ok(())
            })?;
        }
    }

    // Nothing may still name the parent: its id, its taps (`gx` and its
    // id's first 8 hex digits), or its volumes.
    let mut traces = vec![m.parent_vm.clone()];
    if let Some(short) = m.parent_vm.get(..8) {
        traces.push(format!("gx{short}-"));
    }
    traces.extend(m.volumes.keys().cloned());
    for (name, v) in [("config.json", &*config), ("state.json", &*state)] {
        let text = serde_json::to_string(v).expect("json");
        if let Some(t) = traces
            .iter()
            .find(|t| !t.is_empty() && text.contains(t.as_str()))
        {
            return Err(refused(format!(
                "the fork's {name} still names its parent ({t})"
            )));
        }
    }
    Ok(())
}

/// A saved device state is a JSON document inside a string
/// (`snapshot_data.state`); edit it in place.
fn edit_state(
    device: &mut Value,
    f: impl FnOnce(&mut Value) -> Result<(), ForkDirError>,
) -> Result<(), ForkDirError> {
    let Some(text) = device
        .pointer("/snapshot_data/state")
        .and_then(Value::as_str)
    else {
        return Ok(());
    };
    let mut s: Value = serde_json::from_str(text)
        .map_err(|e| ForkDirError::Format("state.json", e.to_string()))?;
    f(&mut s)?;
    device["snapshot_data"]["state"] = Value::from(serde_json::to_string(&s).expect("json"));
    Ok(())
}

fn array_mut<'a>(v: &'a mut Value, key: &'static str) -> Result<&'a mut Vec<Value>, ForkDirError> {
    v.get_mut(key)
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ForkDirError::Format("config.json", format!("no {key} list")))
}

fn str_field(v: &Value, key: &str) -> Result<String, ForkDirError> {
    v.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| ForkDirError::Format("config.json", format!("a device without {key}")))
}

fn mac_string(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "0f0f0f0f-1111-4222-8333-444455556666";
    const FORK: &str = "5a5a5a5a-7777-4888-9999-aaaabbbbcccc";
    const VOLUME: &str = "1111111111111111111111111111aaaa";
    const CLONE: &str = "3333333333333333333333333333bbbb";
    const MAC: [u8; 6] = [0x02, 0x11, 0x22, 0x33, 0x44, 0x55];

    fn fixture() -> (Value, Value) {
        (
            serde_json::from_str(include_str!("testdata/config.json")).unwrap(),
            serde_json::from_str(include_str!("testdata/state.json")).unwrap(),
        )
    }

    fn mapping() -> ForkMapping {
        ForkMapping {
            parent_vm: PARENT.into(),
            fork_vm: FORK.into(),
            volumes: [(VOLUME.into(), CLONE.into())].into(),
            seed: Some((
                format!("/run/glidex-cp/vms/{PARENT}/cloudinit.img"),
                format!("/run/glidex-cp/vms/{FORK}/cloudinit.img"),
            )),
            nics: [(
                "net0".into(),
                ForkNic {
                    backend: NicBackend::Tap("gx5a5a5a5a-0".into()),
                    mac: MAC,
                },
            )]
            .into(),
        }
    }

    fn saved(state: &Value, id: &str) -> Value {
        let text = state
            .pointer(&format!(
                "/snapshots/device-manager/snapshots/{id}/snapshot_data/state"
            ))
            .and_then(Value::as_str)
            .unwrap();
        serde_json::from_str(text).unwrap()
    }

    /// A real snapshot of a glidex VM, rewritten for a fork: its clone's
    /// socket, its own seed, tap and MAC, in the config and in the saved
    /// NIC state; device ids unchanged; nothing left naming the parent.
    #[test]
    fn a_fork_uses_its_own_devices() {
        let (mut config, mut state) = fixture();
        let ids_before: Vec<Value> = config["disks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect();
        rewrite(&mut config, &mut state, &mapping()).unwrap();
        assert_eq!(
            config["disks"][0]["vhost_socket"],
            format!("/run/hyperstor/vhost/{CLONE}.sock")
        );
        assert_eq!(
            config["disks"][1]["path"],
            format!("/run/glidex-cp/vms/{FORK}/cloudinit.img")
        );
        let ids_after: Vec<Value> = config["disks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].clone())
            .collect();
        assert_eq!(ids_before, ids_after);
        assert_eq!(config["net"][0]["tap"], "gx5a5a5a5a-0");
        assert_eq!(config["net"][0]["mac"], "02:11:22:33:44:55");
        assert_eq!(
            saved(&state, "net0")["config"]["mac"],
            serde_json::json!(MAC)
        );
        assert_eq!(
            saved(&state, "_disk1")["disk_path"],
            format!("/run/glidex-cp/vms/{FORK}/cloudinit.img")
        );
        let text = serde_json::to_string(&(config, state)).unwrap();
        assert!(!text.contains(PARENT) && !text.contains(VOLUME) && !text.contains("gx0f0f0f0f-"));
    }

    /// A disk or NIC with nothing for the fork, or one whose saved state is
    /// missing, refuses the fork rather than restoring it onto the parent's
    /// device or starting it fresh.
    #[test]
    fn a_fork_with_a_device_left_behind_is_refused() {
        let refused = |config: &mut Value, state: &mut Value, m: &ForkMapping, what: &str| {
            let e = rewrite(config, state, m).unwrap_err();
            assert!(matches!(e, ForkDirError::Refused(_)), "{e}");
            assert!(e.to_string().contains(what), "{what}: {e}");
        };
        let (mut c, mut s) = fixture();
        let mut m = mapping();
        m.volumes.clear();
        refused(&mut c, &mut s, &m, "has no clone");
        let (mut c, mut s) = fixture();
        let mut m = mapping();
        m.seed = None;
        refused(&mut c, &mut s, &m, "has no copy");
        let (mut c, mut s) = fixture();
        let mut m = mapping();
        m.nics.clear();
        refused(&mut c, &mut s, &m, "NIC net0");
        let (mut c, mut s) = fixture();
        s["snapshots"]["device-manager"]["snapshots"]
            .as_object_mut()
            .unwrap()
            .remove("net0");
        refused(&mut c, &mut s, &mapping(), "no saved state");
        let (mut c, mut s) = fixture();
        let mut m = mapping();
        m.nics.get_mut("net0").unwrap().backend = NicBackend::VhostUser("/run/x.sock".into());
        refused(&mut c, &mut s, &m, "backend kind");
        // A path the mapping doesn't cover still names the parent.
        let (mut c, mut s) = fixture();
        c["payload"]["kernel"] = Value::from(format!("/run/glidex-cp/vms/{PARENT}/vmlinuz"));
        refused(&mut c, &mut s, &mapping(), "still names its parent");
    }

    /// A vhost-user NIC gets the fork's socket; a Pylon VM's `vm_id` the
    /// fork's id (D16).
    #[test]
    fn vhost_user_nics_and_pylon_vms_are_rewritten() {
        let (mut c, mut s) = fixture();
        let nic = &mut c["net"][0];
        nic.as_object_mut().unwrap().remove("tap");
        nic["vhost_user"] = Value::from(true);
        nic["vhost_socket"] = Value::from(format!("/run/glidex/vhost/{PARENT}.net0.sock"));
        c["pylon"] = serde_json::json!({ "vm_id": PARENT });
        let mut m = mapping();
        m.nics.get_mut("net0").unwrap().backend =
            NicBackend::VhostUser(format!("/run/glidex/vhost/{FORK}.net0.sock"));
        rewrite(&mut c, &mut s, &m).unwrap();
        assert_eq!(
            c["net"][0]["vhost_socket"],
            format!("/run/glidex/vhost/{FORK}.net0.sock")
        );
        assert_eq!(c["pylon"]["vm_id"], FORK);
    }

    /// The directory: the rewritten files (owner-only) and the memory
    /// linked, not copied; a second write to it is refused.
    #[test]
    fn the_fork_directory_links_the_memory() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let ckpt = dir.path().join("checkpoint");
        std::fs::create_dir(&ckpt).unwrap();
        std::fs::write(
            ckpt.join("config.json"),
            include_str!("testdata/config.json"),
        )
        .unwrap();
        std::fs::write(ckpt.join("state.json"), include_str!("testdata/state.json")).unwrap();
        std::fs::write(ckpt.join("memory-ranges"), b"guest memory").unwrap();
        let fork = dir.path().join("fork");
        write_fork_dir(&ckpt, &fork, &mapping()).unwrap();
        let mem = std::fs::metadata(fork.join("memory-ranges")).unwrap();
        assert_eq!(
            mem.ino(),
            std::fs::metadata(ckpt.join("memory-ranges")).unwrap().ino()
        );
        let cfg = std::fs::metadata(fork.join("config.json")).unwrap();
        assert_eq!(cfg.permissions().mode() & 0o777, 0o600);
        let text = std::fs::read_to_string(fork.join("config.json")).unwrap();
        assert!(text.contains(CLONE) && !text.contains(VOLUME));
        assert!(write_fork_dir(&ckpt, &fork, &mapping()).is_err());
        // A refused fork leaves no directory behind.
        let mut m = mapping();
        m.volumes.clear();
        let other = dir.path().join("refused");
        assert!(write_fork_dir(&ckpt, &other, &m).is_err());
        assert!(!other.exists());
    }
}
