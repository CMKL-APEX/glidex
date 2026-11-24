//! The DPU as a chassis of the cluster's OVN (spec/31-dpu.md §8.3): its
//! key and CSR, the certificate the leader signed, the `Open_vSwitch`
//! settings and the DPU's `ovn-controller`.
//!
//! The chassis key is made here, on the DPU, and never leaves it: `csr`
//! returns only the request, `install_cert` takes only public material
//! (decision 12).

use glidex_dpu::exec::{Cmd, Exec, Program};
use glidex_dpu::ovs::{self, ChassisSettings};
use glidex_dpu::{DpuError, ovsdb};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Where the agent keeps the chassis key and certificates.
pub const DEFAULT_OVN_DIR: &str = "/etc/glidex/dpu-agent/ovn";

/// The DPU chassis's state, as `ovn_status` reports the host's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisStatus {
    pub chassis: Option<String>,
    pub controller_running: bool,
    pub sb_remotes: Vec<String>,
    pub br_int: bool,
    pub datapath: Option<String>,
    /// Whether `ovn-controller` holds its SB connection: a running
    /// controller whose TLS the SB refuses (a rotated CA) is not. `None`
    /// from an agent that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sb_connected: Option<bool>,
}

/// What netd sends to make the DPU a chassis. `encap_ip` is `dpu.json`'s;
/// when netd has none the agent uses its own configuration's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChassisArgs {
    pub system_id: String,
    pub sb_remotes: Vec<String>,
    #[serde(default)]
    pub encap_ip: Option<IpAddr>,
    pub br_int: String,
}

fn io(path: &Path, e: std::io::Error) -> DpuError {
    DpuError::Io(format!("{}: {e}", path.display()))
}

pub struct Chassis {
    pub dir: PathBuf,
    /// The DPU's underlay address, from the agent's configuration.
    pub encap_ip: Option<IpAddr>,
}

impl Chassis {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            encap_ip: None,
        }
    }

    pub fn key_path(&self) -> PathBuf {
        self.dir.join("chassis.key")
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join("chassis.crt")
    }

    pub fn ca_path(&self) -> PathBuf {
        self.dir.join("ca.crt")
    }

    fn ensure_dir(&self) -> Result<(), DpuError> {
        std::fs::create_dir_all(&self.dir).map_err(|e| io(&self.dir, e))?;
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| io(&self.dir, e))
    }

    /// The chassis key, made on first use (0600, root-only).
    fn key(&self) -> Result<KeyPair, DpuError> {
        let path = self.key_path();
        if let Ok(pem) = std::fs::read_to_string(&path) {
            return KeyPair::from_pem(&pem)
                .map_err(|e| DpuError::Io(format!("{}: {e}", path.display())));
        }
        self.ensure_dir()?;
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
            .map_err(|e| DpuError::Io(format!("chassis key: {e}")))?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| io(&path, e))?;
        f.write_all(key.serialize_pem().as_bytes())
            .map_err(|e| io(&path, e))?;
        Ok(key)
    }

    /// A CSR for `chassis` (`node:<id>-dpu`) from the DPU's key. The leader
    /// sets the subject it signs; the CN here only names the request.
    pub fn csr(&self, chassis: &str) -> Result<String, DpuError> {
        let id = chassis.strip_prefix("node:").unwrap_or(chassis);
        if !id.ends_with("-dpu")
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(DpuError::invalid(format!(
                "chassis `{chassis}` (want `node:<node id>-dpu`)"
            )));
        }
        let key = self.key()?;
        let mut p = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, chassis);
        p.distinguished_name = dn;
        p.serialize_request(&key)
            .and_then(|r| r.pem())
            .map_err(|e| DpuError::Io(format!("chassis CSR: {e}")))
    }

    /// Install the signed certificate and the cluster CA, after checking the
    /// certificate is for the DPU's own key. Returns whether anything changed.
    pub fn install_cert(
        &self,
        exec: &dyn Exec,
        cert_pem: &str,
        ca_pem: &str,
    ) -> Result<bool, DpuError> {
        for (what, pem) in [("certificate", cert_pem), ("CA", ca_pem)] {
            if pem.contains("PRIVATE KEY") {
                return Err(DpuError::invalid(format!(
                    "the chassis {what} carries a private key"
                )));
            }
        }
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
            .map_err(|e| DpuError::invalid(format!("chassis certificate: {e}")))?;
        let cert = pem
            .parse_x509()
            .map_err(|e| DpuError::invalid(format!("chassis certificate: {e}")))?;
        x509_parser::pem::parse_x509_pem(ca_pem.as_bytes())
            .map_err(|e| DpuError::invalid(format!("cluster CA: {e}")))?;
        if !self.key_path().exists() {
            return Err(DpuError::invalid(
                "no chassis key on the DPU: ask for a CSR first",
            ));
        }
        let key = self.key()?;
        if cert.public_key().subject_public_key.data.as_ref() != key.der_bytes() {
            return Err(DpuError::invalid(
                "the certificate is not for this DPU's chassis key",
            ));
        }
        let mut changed = false;
        for (path, data) in [(self.cert_path(), cert_pem), (self.ca_path(), ca_pem)] {
            if std::fs::read_to_string(&path).ok().as_deref() != Some(data) {
                std::fs::write(&path, data).map_err(|e| io(&path, e))?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                    .map_err(|e| io(&path, e))?;
                changed = true;
            }
        }
        exec.check(&ovs::set_ssl(
            &self.key_path().display().to_string(),
            &self.cert_path().display().to_string(),
            &self.ca_path().display().to_string(),
        ))?;
        if changed && controller_running(exec) {
            // It keeps forwarding on its installed flows while it restarts.
            exec.check(&Cmd::new(
                Program::Systemctl,
                ["try-reload-or-restart", "ovn-controller"],
            ))?;
        }
        Ok(changed)
    }

    /// The `Open_vSwitch` settings, `br-int` on `doca` and the DPU's
    /// `ovn-controller` running. Idempotent.
    pub fn ensure(&self, exec: &dyn Exec, args: &ChassisArgs) -> Result<ChassisStatus, DpuError> {
        let encap_ip = args
            .encap_ip
            .or(self.encap_ip)
            .ok_or(DpuError::Unsupported {
                missing: vec!["ovn.encap_ip".into()],
            })?;
        let settings = ChassisSettings {
            system_id: args.system_id.clone(),
            sb_remotes: args.sb_remotes.clone(),
            encap_ip,
            br_int: args.br_int.clone(),
        };
        let cmds = ovs::chassis_cmds(&settings)?;
        exec.check(&ovs::ensure_br_int(&args.br_int)?)?;
        for c in &cmds {
            exec.check(c)?;
        }
        // As on the host (glidex_ovs::ovn): `ovn-controller.service` is
        // static on Debian/Ubuntu; `ovn-host` is the unit that is enabled.
        let enabled = exec
            .run(&Cmd::new(Program::Systemctl, ["is-enabled", "ovn-host"]))
            .map(|o| o.stdout_str().trim() == "enabled")
            .unwrap_or(false);
        if !enabled {
            exec.check(&Cmd::new(Program::Systemctl, ["enable", "ovn-host"]))?;
        }
        exec.check(&Cmd::new(Program::Systemctl, ["start", "ovn-host"]))?;
        exec.check(&Cmd::new(Program::Systemctl, ["start", "ovn-controller"]))?;
        self.status(exec, &args.br_int)
    }

    pub fn status(&self, exec: &dyn Exec, br_int: &str) -> Result<ChassisStatus, DpuError> {
        let out = exec.check(&Cmd::new(
            Program::OvsVsctl,
            [
                "--format=json",
                "--columns=external_ids",
                "list",
                "Open_vSwitch",
            ],
        ))?;
        let rows = ovsdb::json_rows(&out.stdout_str())?;
        let ids = rows
            .first()
            .map(|r| ovsdb::json_map(r, "external_ids"))
            .unwrap_or_default();
        let ours = ids.contains_key(ovs::CHASSIS_TAG);
        let out = exec.check(&Cmd::new(
            Program::OvsVsctl,
            [
                "--format=json".to_string(),
                "--columns=name,datapath_type".into(),
                "find".into(),
                "Bridge".into(),
                format!("name={br_int}"),
            ],
        ))?;
        let bridge = ovsdb::json_rows(&out.stdout_str())?.into_iter().next();
        let running = controller_running(exec);
        Ok(ChassisStatus {
            chassis: ours.then(|| ids.get("system-id").cloned()).flatten(),
            controller_running: running,
            sb_connected: Some(running && sb_connected(exec)),
            sb_remotes: if ours {
                ids.get("ovn-remote")
                    .map(|r| r.split(',').map(str::to_string).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            },
            br_int: bridge.is_some(),
            datapath: bridge.and_then(|b| ovsdb::json_str(&b, "datapath_type")),
        })
    }

    /// Stop being a chassis: the controller stopped, the settings and the
    /// certificates removed. The key goes too, so a later enrolment starts
    /// from a new one.
    pub fn leave(&self, exec: &dyn Exec) -> Result<(), DpuError> {
        exec.check(&Cmd::new(
            Program::Systemctl,
            ["disable", "--now", "ovn-host"],
        ))?;
        exec.check(&Cmd::new(Program::Systemctl, ["stop", "ovn-controller"]))?;
        exec.check(&ovs::leave_cmds())?;
        exec.check(&Cmd::new(Program::OvsVsctl, ["del-ssl"]))?;
        for path in [self.key_path(), self.cert_path(), self.ca_path()] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io(&path, e)),
            }
        }
        Ok(())
    }
}

fn controller_running(exec: &dyn Exec) -> bool {
    exec.run(&Cmd::new(
        Program::Systemctl,
        ["is-active", "ovn-controller"],
    ))
    .map(|o| o.stdout_str().trim() == "active")
    .unwrap_or(false)
}

/// spec/31-dpu.md §9.2: the controller's own word on its SB connection.
fn sb_connected(exec: &dyn Exec) -> bool {
    exec.run(
        &Cmd::new(
            Program::OvnAppctl,
            ["-t", "ovn-controller", "connection-status"],
        )
        .timeout(std::time::Duration::from_secs(5)),
    )
    .map(|o| o.stdout_str().trim() == "connected")
    .unwrap_or(false)
}
