//! The DPU's OVN chassis, as netd relays it (spec/31-dpu.md §8.3).
//!
//! netd holds a [`DpuChassis`] when the host has a DPU (`dpu.json`): the
//! [`crate::dpu::DpuBackend`], which relays to the DPU's agent. Every call names the DPU by its id
//! in `dpu.json`. No call carries the chassis's private key: the agent
//! makes it on the DPU and hands out only a CSR (decision 12).

use glidex_dpu_agent::AgentOp;
use glidex_dpu_agent::chassis::{ChassisArgs, ChassisStatus};
use glidex_ovs::OvsError;
use glidex_ovs::ovn::OvnStatus;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

/// What the leader signed for the DPU's chassis: the certificate and the
/// cluster CA, public material only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpuChassisCerts {
    pub cert_pem: String,
    pub ca_pem: String,
}

impl DpuChassisCerts {
    /// Refuse anything that looks like a private key: it must never leave
    /// the DPU, so it has no business on this path.
    pub fn validate(&self) -> Result<(), OvsError> {
        for (what, pem) in [("certificate", &self.cert_pem), ("CA", &self.ca_pem)] {
            if pem.contains("PRIVATE KEY") {
                return Err(OvsError::invalid(format!(
                    "the DPU chassis {what} carries a private key; the key stays on the DPU"
                )));
            }
            if !pem.contains("BEGIN CERTIFICATE") {
                return Err(OvsError::invalid(format!(
                    "the DPU chassis {what} is not a PEM certificate"
                )));
            }
        }
        Ok(())
    }
}

/// The DPU side of the chassis ops.
pub trait DpuChassis: Send + Sync {
    /// Make the DPU `id` the chassis `chassis` (`node:<id>-dpu`) of the
    /// cluster's SB at `sb_remotes`: the `Open_vSwitch` settings
    /// (`glidex_dpu::ovs::chassis_cmds`, with the DPU's own `ovn-encap-ip`
    /// from `dpu.json`), `br-int` on `doca`, the certificate, and the
    /// DPU's `ovn-controller` running. Idempotent.
    fn ensure(
        &self,
        id: &str,
        chassis: &str,
        sb_remotes: &[String],
        certs: &DpuChassisCerts,
    ) -> Result<OvnStatus, OvsError>;
    fn status(&self, id: &str) -> Result<OvnStatus, OvsError>;
    fn leave(&self, id: &str) -> Result<(), OvsError>;
    /// A CSR for `chassis` from the key the DPU holds (made on first use).
    fn csr(&self, id: &str, chassis: &str) -> Result<String, OvsError>;
}

/// The real relay: the chassis ops go to the DPU's agent (§8.3, §13).
impl DpuChassis for crate::dpu::DpuBackend {
    fn ensure(
        &self,
        id: &str,
        chassis: &str,
        sb_remotes: &[String],
        certs: &DpuChassisCerts,
    ) -> Result<OvnStatus, OvsError> {
        self.known(id)?;
        certs.validate()?;
        // The certificate first: the controller then starts with it.
        let _: serde_json::Value = self.call(AgentOp::InstallCert {
            cert_pem: certs.cert_pem.clone(),
            ca_pem: certs.ca_pem.clone(),
        })?;
        let st: ChassisStatus = self.call(AgentOp::Chassis(ChassisArgs {
            system_id: chassis.to_string(),
            sb_remotes: sb_remotes.to_vec(),
            encap_ip: self.config.ovn.as_ref().map(|o| o.encap_ip),
            br_int: self.config.bridges.int.clone(),
        }))?;
        Ok(ovn_status(st))
    }

    fn status(&self, id: &str) -> Result<OvnStatus, OvsError> {
        self.known(id)?;
        let st: ChassisStatus = self.call(AgentOp::OvnStatus {
            br_int: self.config.bridges.int.clone(),
        })?;
        Ok(ovn_status(st))
    }

    fn leave(&self, id: &str) -> Result<(), OvsError> {
        self.known(id)?;
        let _: serde_json::Value = self.call(AgentOp::LeaveOvn)?;
        Ok(())
    }

    fn csr(&self, id: &str, chassis: &str) -> Result<String, OvsError> {
        self.known(id)?;
        #[derive(Deserialize)]
        struct Csr {
            csr: String,
        }
        let r: Csr = self.call(AgentOp::Csr {
            chassis: chassis.to_string(),
        })?;
        // A CSR and nothing else: a key in the answer would be a broken agent.
        if r.csr.contains("PRIVATE KEY") || !r.csr.contains("CERTIFICATE REQUEST") {
            return Err(OvsError::Io("the DPU agent's CSR is not a CSR".into()));
        }
        Ok(r.csr)
    }
}

impl crate::dpu::DpuBackend {
    fn known(&self, id: &str) -> Result<(), OvsError> {
        if self.config.id == id {
            Ok(())
        } else {
            Err(OvsError::NotFound {
                message: format!("no DPU {id} in dpu.json"),
            })
        }
    }
}

fn ovn_status(st: ChassisStatus) -> OvnStatus {
    OvnStatus {
        chassis: st.chassis,
        controller_running: st.controller_running,
        sb_remotes: st.sb_remotes,
        br_int: st.br_int,
        datapath: st.datapath,
        ports: vec![],
        tunnels: vec![],
    }
}

/// One `ensure` a [`RecordingDpuChassis`] saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensured {
    pub id: String,
    pub chassis: String,
    pub sb_remotes: Vec<String>,
    pub certs: DpuChassisCerts,
}

/// A test double: remembers what it was asked and answers from what tests
/// set. Public so the control plane's tests can put it behind a netd.
#[derive(Default)]
pub struct RecordingDpuChassis {
    /// The DPU ids it knows; others are `not_found`.
    pub ids: Vec<String>,
    /// What `csr` returns.
    pub csr_pem: Mutex<String>,
    pub calls: Mutex<Vec<String>>,
    /// The chassis and certificates of each `ensure`.
    pub ensured: Mutex<Vec<Ensured>>,
    state: Mutex<Option<OvnStatus>>,
}

impl RecordingDpuChassis {
    pub fn new(ids: &[&str]) -> Self {
        Self {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn known(&self, id: &str) -> Result<(), OvsError> {
        if self.ids.iter().any(|i| i == id) {
            Ok(())
        } else {
            Err(OvsError::NotFound {
                message: format!("no DPU {id} in dpu.json"),
            })
        }
    }
}

impl DpuChassis for RecordingDpuChassis {
    fn ensure(
        &self,
        id: &str,
        chassis: &str,
        sb_remotes: &[String],
        certs: &DpuChassisCerts,
    ) -> Result<OvnStatus, OvsError> {
        self.known(id)?;
        self.calls
            .lock()
            .unwrap()
            .push(format!("ensure {id} {chassis}"));
        self.ensured.lock().unwrap().push(Ensured {
            id: id.to_string(),
            chassis: chassis.to_string(),
            sb_remotes: sb_remotes.to_vec(),
            certs: certs.clone(),
        });
        let st = OvnStatus {
            chassis: Some(chassis.to_string()),
            controller_running: true,
            sb_remotes: sb_remotes.to_vec(),
            br_int: true,
            datapath: Some("doca".into()),
            ports: vec![],
            tunnels: vec![],
        };
        *self.state.lock().unwrap() = Some(st.clone());
        Ok(st)
    }

    fn status(&self, id: &str) -> Result<OvnStatus, OvsError> {
        self.known(id)?;
        Ok(self.state.lock().unwrap().clone().unwrap_or(OvnStatus {
            chassis: None,
            controller_running: false,
            sb_remotes: vec![],
            br_int: false,
            datapath: None,
            ports: vec![],
            tunnels: vec![],
        }))
    }

    fn leave(&self, id: &str) -> Result<(), OvsError> {
        self.known(id)?;
        self.calls.lock().unwrap().push(format!("leave {id}"));
        *self.state.lock().unwrap() = None;
        Ok(())
    }

    fn csr(&self, id: &str, chassis: &str) -> Result<String, OvsError> {
        self.known(id)?;
        self.calls
            .lock()
            .unwrap()
            .push(format!("csr {id} {chassis}"));
        Ok(self.csr_pem.lock().unwrap().clone())
    }
}
