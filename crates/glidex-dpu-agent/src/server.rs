//! The agent: ops on the DPU through `glidex-dpu`, served over mutual TLS
//! (spec/31-dpu.md §10.2). It keeps no state of its own: the tagged OVSDB
//! rows are the record, so it restarts freely.

use crate::proto::*;
use crate::tls;
use glidex_dpu::exec::Exec;
use glidex_dpu::{DpuError, DpuPortSpec, VfId, ovs, probe, virtnet};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Agent {
    exec: Arc<dyn Exec>,
    /// The `mst` device `mlxconfig` queries.
    pub mst_device: String,
    /// How often `attach_rep` looks for the representor.
    pub poll: Duration,
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, DpuError> {
    serde_json::to_value(v).map_err(|e| DpuError::Io(e.to_string()))
}

impl Agent {
    pub fn new(exec: Arc<dyn Exec>) -> Self {
        Self {
            exec,
            mst_device: probe::DEFAULT_MST_DEVICE.to_string(),
            poll: Duration::from_millis(250),
        }
    }

    fn ex(&self) -> &dyn Exec {
        self.exec.as_ref()
    }

    pub fn handle(&self, op: AgentOp) -> Result<Value, DpuError> {
        match op {
            AgentOp::Hello { .. } => to_value(HelloResult {
                protocol: AGENT_PROTOCOL_VERSION,
                agent_version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            AgentOp::Probe => to_value(probe::probe(self.ex(), &self.mst_device)),
            AgentOp::ListPorts => to_value(ovs::list_ports(self.ex())?),
            AgentOp::AttachRep {
                vf,
                spec,
                bridge,
                timeout_secs,
            } => {
                to_value(self.attach_rep(vf, &spec, &bridge, Duration::from_secs(timeout_secs))?)
            }
            AgentOp::DetachRep { vm_id, nic } => {
                Ok(json!({ "removed": self.detach_rep(&vm_id, nic)? }))
            }
        }
    }

    /// §7.1 steps 4–5. Idempotent: every command sets what it wants.
    fn attach_rep(
        &self,
        vf: VfId,
        spec: &DpuPortSpec,
        bridge: &str,
        timeout: Duration,
    ) -> Result<AttachRepResult, DpuError> {
        let list = virtnet::parse_list(&self.ex().check(&virtnet::list_cmd())?.stdout_str())?;
        let max_qp = list
            .vf(vf)
            .and_then(|d| d.max_queue_pairs)
            .map_or(u8::MAX, |q| q.min(u16::from(u8::MAX)) as u8);
        spec.validate(max_qp)?;
        self.ex().check(&virtnet::modify_vf_cmd(vf, spec)?)?;
        // The representor appears once the controller has the VF (within
        // 2 s on the test DPU, finding 35); read its name, don't compute it.
        let deadline = Instant::now() + timeout;
        let (rep, bdf) = loop {
            let list = virtnet::parse_list(&self.ex().check(&virtnet::list_cmd())?.stdout_str())?;
            if let Some(d) = list.vf(vf)
                && let Some(r) = &d.representor
            {
                break (r.clone(), d.bdf.clone());
            }
            if Instant::now() >= deadline {
                return Err(DpuError::DpuPortTimeout {
                    message: format!(
                        "no representor for VF {}/{} after {timeout:?}",
                        vf.pf, vf.vf
                    ),
                });
            }
            std::thread::sleep(self.poll);
        };
        self.ex()
            .check(&ovs::add_representor(bridge, &rep, spec)?)?;
        self.ex().check(&ovs::link_up(&rep)?)?;
        tracing::info!(vm = %spec.vm_id, nic = spec.nic, representor = %rep, "representor attached");
        Ok(AttachRepResult {
            representor: rep,
            bdf,
        })
    }

    fn detach_rep(&self, vm_id: &str, nic: u8) -> Result<bool, DpuError> {
        let Some(port) = ovs::list_ports(self.ex())?
            .into_iter()
            .find(|p| p.vm_id == vm_id && p.nic == nic)
        else {
            return Ok(false);
        };
        ovs::remove_representor(self.ex(), &port.name, vm_id, nic)
    }

    /// One request, as the server answers it (`hello` first, then ops).
    pub fn respond(&self, req: AgentRequest, said_hello: &mut bool) -> AgentResponse {
        let err = |code: &str, message: String| AgentResponse {
            id: req.id,
            ok: None,
            error: Some(ErrorBody {
                code: code.into(),
                message,
            }),
        };
        match (&req.op, *said_hello) {
            (AgentOp::Hello { protocol }, _) if *protocol != AGENT_PROTOCOL_VERSION => {
                return err(
                    "unsupported",
                    format!("protocol {protocol}; this agent speaks {AGENT_PROTOCOL_VERSION}"),
                );
            }
            (AgentOp::Hello { .. }, _) => *said_hello = true,
            (_, false) => return err("protocol_error", "hello first".into()),
            _ => {}
        }
        let name = req.op.name();
        match self.handle(req.op) {
            Ok(v) => AgentResponse {
                id: req.id,
                ok: Some(v),
                error: None,
            },
            Err(e) => {
                tracing::warn!(op = name, error = %e, "agent op failed");
                AgentResponse {
                    id: req.id,
                    ok: None,
                    error: Some(ErrorBody::from(&e)),
                }
            }
        }
    }
}

/// Serve one TCP connection: TLS handshake, the client's identity, then
/// requests until it closes.
pub fn serve_connection(
    agent: &Agent,
    config: Arc<rustls::ServerConfig>,
    client_identity: &str,
    sock: TcpStream,
) -> Result<(), String> {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(300)));
    let conn = rustls::ServerConnection::new(config).map_err(|e| e.to_string())?;
    let mut tls = rustls::StreamOwned::new(conn, sock);
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .map_err(|e| format!("handshake: {e}"))?;
    }
    let cn = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .and_then(|c| tls::common_name(c));
    if cn.as_deref() != Some(client_identity) {
        return Err(format!(
            "client {:?} is not {client_identity}; closing",
            cn.unwrap_or_default()
        ));
    }
    let mut reader = BufReader::new(tls);
    let mut said_hello = false;
    loop {
        let mut line = String::new();
        let n = reader
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_LINE {
            return Err("request line too long".into());
        }
        let resp = match serde_json::from_str::<AgentRequest>(&line) {
            Ok(req) => agent.respond(req, &mut said_hello),
            Err(e) => AgentResponse {
                id: 0,
                ok: None,
                error: Some(ErrorBody {
                    code: "protocol_error".into(),
                    message: e.to_string(),
                }),
            },
        };
        let mut out = serde_json::to_vec(&resp).map_err(|e| e.to_string())?;
        out.push(b'\n');
        let w = reader.get_mut();
        w.write_all(&out).map_err(|e| e.to_string())?;
        w.flush().map_err(|e| e.to_string())?;
    }
}

/// Accept connections forever, one thread each.
pub fn serve(
    agent: Arc<Agent>,
    listener: TcpListener,
    config: Arc<rustls::ServerConfig>,
    client_identity: String,
) {
    let client_identity = Arc::new(client_identity);
    for sock in listener.incoming().flatten() {
        let (agent, config, id) = (agent.clone(), config.clone(), client_identity.clone());
        std::thread::spawn(move || {
            let peer = sock.peer_addr().ok();
            if let Err(e) = serve_connection(&agent, config, &id, sock) {
                tracing::warn!(?peer, "connection: {e}");
            }
        });
    }
}
