//! Request handling and socket serving for glidex-stord.

use crate::auth::{self, Peer};
use crate::lvm::{self, Exec};
use crate::names;
use crate::proto::*;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct Config {
    pub run_dir: PathBuf,
    /// Where the VM units' drop-ins go (`/run/systemd/system`).
    pub systemd_dir: PathBuf,
    /// Group of [`SOCKET_NAME`].
    pub group: String,
    /// Group of [`ADMIN_SOCKET_NAME`].
    pub admin_group: String,
    /// Which ops each group may send.
    pub policy: auth::Policy,
    /// The only volume groups stord acts on (spec/24-storage.md S17).
    pub allowed_vgs: Vec<String>,
    /// Who gets the device nodes: the glidex user and group.
    pub owner: (u32, u32),
}

impl Default for Config {
    fn default() -> Self {
        Self {
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
            systemd_dir: PathBuf::from("/run/systemd/system"),
            group: "glidex".into(),
            admin_group: "glidex-admin".into(),
            policy: auth::default_policy("glidex", "glidex-admin"),
            allowed_vgs: Vec::new(),
            owner: (0, 0),
        }
    }
}

pub struct Stord {
    exec: Arc<dyn Exec>,
    pub config: Config,
    /// Serializes every host-changing operation.
    mutate: Mutex<()>,
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, StordError> {
    serde_json::to_value(v).map_err(|e| StordError::Failed(e.to_string()))
}

impl Stord {
    pub fn new(exec: Arc<dyn Exec>, config: Config) -> Self {
        Self {
            exec,
            config,
            mutate: Mutex::new(()),
        }
    }

    fn ex(&self) -> &dyn Exec {
        self.exec.as_ref()
    }

    /// `vg` is one the operator allowed.
    fn allowed(&self, vg: &str) -> Result<(), StordError> {
        names::lvm_name("volume group", vg)?;
        if self.config.allowed_vgs.iter().any(|a| a == vg) {
            Ok(())
        } else {
            Err(StordError::NotAllowed(format!(
                "volume group {vg} is not in stord.json's allowed_vgs"
            )))
        }
    }

    fn activate(&self, vg: &str, name: &str) -> Result<Activated, StordError> {
        let (uid, gid) = self.config.owner;
        lvm::activate(self.ex(), vg, name, uid, gid)
    }

    /// Handle one operation (the peer is already authorized for it).
    pub fn handle(&self, op: Op) -> Result<Value, StordError> {
        let _guard = op.is_mutating().then(|| self.mutate.lock().unwrap());
        match op {
            Op::Hello { .. } => to_value(HelloResult {
                protocol: PROTOCOL_VERSION,
                stord_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            Op::Probe => to_value(lvm::probe(self.ex())),
            Op::PoolStatus { vg, pool } => {
                if let Err(e) = self.allowed(&vg) {
                    return to_value(PoolStatus {
                        reason: "NotAllowed".into(),
                        message: e.to_string(),
                        ..Default::default()
                    });
                }
                names::lvm_name("thin pool", &pool)?;
                to_value(lvm::pool_status(self.ex(), &vg, &pool))
            }
            Op::ListVolumes { vg } => {
                self.allowed(&vg)?;
                to_value(lvm::list_volumes(self.ex(), &vg)?)
            }
            Op::CreateVolume {
                vg,
                pool,
                name,
                size_bytes,
            } => {
                self.allowed(&vg)?;
                names::lvm_name("thin pool", &pool)?;
                names::volume(&name)?;
                lvm::create_volume(self.ex(), &vg, &pool, &name, size_bytes)?;
                to_value(self.activate(&vg, &name)?)
            }
            Op::SnapshotVolume {
                vg,
                origin,
                name,
                size_bytes,
            } => {
                self.allowed(&vg)?;
                names::image_volume(&origin)?;
                names::volume(&name)?;
                lvm::snapshot_volume(self.ex(), &vg, &origin, &name, size_bytes)?;
                to_value(self.activate(&vg, &name)?)
            }
            Op::SealVolume { vg, name } => {
                self.allowed(&vg)?;
                names::image_volume(&name)?;
                lvm::seal_volume(self.ex(), &vg, &name).map(|_| Value::Null)
            }
            Op::ExtendVolume {
                vg,
                name,
                size_bytes,
            } => {
                self.allowed(&vg)?;
                names::volume(&name)?;
                lvm::extend_volume(self.ex(), &vg, &name, size_bytes)?;
                to_value(self.activate(&vg, &name)?)
            }
            Op::Activate { vg, name } => {
                self.allowed(&vg)?;
                names::volume(&name)?;
                to_value(self.activate(&vg, &name)?)
            }
            Op::Deactivate { vg, name } => {
                self.allowed(&vg)?;
                names::volume(&name)?;
                lvm::deactivate(self.ex(), &vg, &name).map(|_| Value::Null)
            }
            Op::RenameVolume { vg, from, to } => {
                self.allowed(&vg)?;
                names::volume(&from)?;
                names::volume(&to)?;
                lvm::rename_volume(self.ex(), &vg, &from, &to)?;
                // The renamed device node is udev's new one (§19, H5).
                to_value(self.activate(&vg, &to)?)
            }
            Op::DeleteVolume { vg, name } => {
                self.allowed(&vg)?;
                names::volume(&name)?;
                lvm::delete_volume(self.ex(), &vg, &name).map(|_| Value::Null)
            }
            Op::AllowDevices { vm_id, devices } => {
                let changed = crate::devices::allow(
                    self.ex(),
                    &self.config.systemd_dir,
                    &vm_id,
                    &devices,
                    &self.config.allowed_vgs,
                )?;
                to_value(serde_json::json!({ "changed": changed }))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Full,
    Admin,
}

fn write_line(w: &mut UnixStream, resp: &Response) -> std::io::Result<()> {
    let mut line = serde_json::to_string(resp).unwrap_or_default();
    line.push('\n');
    w.write_all(line.as_bytes())
}

pub fn serve_connection(stord: &Stord, stream: UnixStream, access: Access, group_gid: Option<u32>) {
    let peer: Peer = match auth::peer(&stream) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "could not read peer credentials");
            return;
        }
    };
    if !auth::authorized(&peer, group_gid) {
        let group = match access {
            Access::Admin => &stord.config.admin_group,
            Access::Full => &stord.config.group,
        };
        tracing::warn!(
            uid = peer.uid,
            pid = peer.pid,
            "rejected peer not in group {group}"
        );
        return;
    }
    let policy_groups = if peer.uid == 0 {
        Vec::new()
    } else {
        auth::policy_groups(&stord.config.policy, &peer)
    };
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut said_hello = false;
    loop {
        let mut line = String::new();
        match reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line)
        {
            Ok(0) | Err(_) => return,
            Ok(n) if n > MAX_LINE => {
                let _ = write_line(
                    &mut writer,
                    &Response::err(0, ErrorBody::new("protocol_error", "request too large")),
                );
                return;
            }
            Ok(_) => {}
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_line(
                    &mut writer,
                    &Response::err(0, ErrorBody::new("protocol_error", e.to_string())),
                );
                continue;
            }
        };
        let resp = if !said_hello && !matches!(req.op, Op::Hello { .. }) {
            Response::err(
                req.id,
                ErrorBody::new("protocol_error", "first request must be hello"),
            )
        } else if !auth::allows(&stord.config.policy, peer.uid, req.op.name(), |g| {
            policy_groups.iter().any(|m| m == g)
        }) {
            tracing::warn!(uid = peer.uid, op = req.op.name(), "denied by policy");
            Response::err(
                req.id,
                ErrorBody::new(
                    "permission_denied",
                    format!(
                        "stord policy does not allow '{}' for uid {}",
                        req.op.name(),
                        peer.uid
                    ),
                ),
            )
        } else {
            if let Op::Hello { protocol } = &req.op {
                if *protocol != PROTOCOL_VERSION {
                    let _ = write_line(
                        &mut writer,
                        &Response::err(
                            req.id,
                            ErrorBody::new(
                                "protocol_error",
                                format!("unsupported protocol {protocol}"),
                            ),
                        ),
                    );
                    return;
                }
                said_hello = true;
            }
            if req.op.is_mutating() {
                // on_behalf_of is the caller's claim, logged for audit only.
                tracing::info!(
                    uid = peer.uid,
                    op = req.op.name(),
                    args = %serde_json::to_string(&req.op).unwrap_or_default(),
                    on_behalf_of = %req.on_behalf_of.as_deref().unwrap_or("-"),
                    "request"
                );
            }
            match stord.handle(req.op) {
                Ok(v) => Response::ok(req.id, v),
                Err(e) => Response::err(req.id, ErrorBody::from(&e)),
            }
        };
        if write_line(&mut writer, &resp).is_err() {
            return;
        }
    }
}

/// Bind a socket, replacing a stale one, with the given mode and group.
pub fn bind(path: &Path, mode: u32, gid: Option<u32>) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    if let Some(gid) = gid {
        // Only root can give the socket to another group; tests run unprivileged.
        let _ = nix::unistd::chown(path, None, Some(nix::unistd::Gid::from_raw(gid)));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

/// Accept connections on `listener` forever, one thread per connection.
pub fn serve(stord: Arc<Stord>, listener: UnixListener, access: Access, group_gid: Option<u32>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let stord = stord.clone();
                std::thread::spawn(move || serve_connection(&stord, stream, access, group_gid));
            }
            Err(e) => tracing::warn!(error = %e, "accept failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Client;
    use crate::lvm::fake::Fake;

    const G: u64 = 1 << 30;

    fn start(f: Arc<Fake>, dir: &Path) -> PathBuf {
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        let group = nix::unistd::Group::from_gid(nix::unistd::getgid())
            .unwrap()
            .unwrap()
            .name;
        let config = Config {
            run_dir: dir.into(),
            systemd_dir: dir.join("systemd"),
            group: group.clone(),
            policy: auth::default_policy(&group, "glidex-admin"),
            allowed_vgs: vec!["vg0".into()],
            owner: (uid, gid),
            ..Default::default()
        };
        let stord = Arc::new(Stord::new(f, config));
        let path = dir.join(SOCKET_NAME);
        let l = bind(&path, 0o660, Some(gid)).unwrap();
        std::thread::spawn(move || serve(stord, l, Access::Full, Some(gid)));
        path
    }

    #[test]
    fn the_control_plane_makes_activates_and_removes_its_volumes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = Arc::new(Fake::with_pool("vg0", "pool", 10 * G));
        let sock = start(f.clone(), tmp.path());
        let mut c = Client::connect(&sock).unwrap();
        assert_eq!(c.hello.protocol, PROTOCOL_VERSION);

        let st: PoolStatus = c
            .call(Op::PoolStatus {
                vg: "vg0".into(),
                pool: "pool".into(),
            })
            .unwrap();
        assert!(st.usable);
        let st: PoolStatus = c
            .call(Op::PoolStatus {
                vg: "ubuntu-vg".into(),
                pool: "pool".into(),
            })
            .unwrap();
        assert_eq!(st.reason, "NotAllowed");

        let a: Activated = c
            .call(Op::CreateVolume {
                vg: "vg0".into(),
                pool: "pool".into(),
                name: "gx-d-1".into(),
                size_bytes: G,
            })
            .unwrap();
        assert_eq!(a.path, "/dev/vg0/gx-d-1");
        let vols: Vec<Volume> = c.call(Op::ListVolumes { vg: "vg0".into() }).unwrap();
        assert_eq!(vols.len(), 1);

        // Only glidex's names, only allowed VGs.
        let refused = |c: &mut Client, op: Op| match c.call_value(op) {
            Err(crate::client::ClientError::Remote(e)) => e.code,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            refused(
                &mut c,
                Op::DeleteVolume {
                    vg: "vg0".into(),
                    name: "pool".into()
                }
            ),
            "invalid_request"
        );
        assert_eq!(
            refused(
                &mut c,
                Op::DeleteVolume {
                    vg: "ubuntu-vg".into(),
                    name: "gx-d-1".into()
                }
            ),
            "not_allowed"
        );
        assert_eq!(
            refused(
                &mut c,
                Op::CreateVolume {
                    vg: "vg0".into(),
                    pool: "pool".into(),
                    name: "root".into(),
                    size_bytes: G
                }
            ),
            "invalid_request"
        );
        assert!(f.lv("vg0", "pool").is_some());

        let vm = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
        let _: Value = c
            .call(Op::AllowDevices {
                vm_id: vm.into(),
                devices: vec![a.path.clone()],
            })
            .unwrap();
        assert_eq!(
            crate::devices::list(&tmp.path().join("systemd")),
            vec![vm.to_string()]
        );

        let _: Value = c
            .call(Op::DeleteVolume {
                vg: "vg0".into(),
                name: "gx-d-1".into(),
            })
            .unwrap();
        assert!(f.lv("vg0", "gx-d-1").is_none());
    }

    #[test]
    fn hello_comes_first() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock = start(Arc::new(Fake::default()), tmp.path());
        let mut s = UnixStream::connect(&sock).unwrap();
        s.write_all(b"{\"id\":1,\"op\":\"probe\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).unwrap();
        assert!(line.contains("first request must be hello"), "{line}");
    }
}
