//! netd's side of the agent channel (spec/31-dpu.md §10.2).

use crate::proto::*;
use crate::server::Agent;
use rustls::pki_types::ServerName;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// A connection per call, as netd's own client makes: the agent keeps no
/// session state, and a call after an agent restart just works.
pub struct AgentClient {
    addr: SocketAddr,
    config: Arc<rustls::ClientConfig>,
    timeout: Duration,
}

impl AgentClient {
    /// `agent` is `tcp://<address>:<port>` (`dpu.json`); the agent's
    /// certificate must name that address.
    pub fn new(agent: &str, config: Arc<rustls::ClientConfig>) -> Result<Self, String> {
        let addr = agent
            .strip_prefix("tcp://")
            .ok_or_else(|| format!("agent `{agent}` must be tcp://<address>:<port>"))?
            .parse()
            .map_err(|e| format!("agent `{agent}`: {e}"))?;
        Ok(Self {
            addr,
            config,
            timeout: Duration::from_secs(60),
        })
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn exchange(
        stream: &mut BufReader<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>,
        id: u64,
        op: AgentOp,
    ) -> Result<Value, AgentError> {
        let mut line = serde_json::to_vec(&AgentRequest { id, op })
            .map_err(|e| AgentError::Protocol(e.to_string()))?;
        line.push(b'\n');
        let w = stream.get_mut();
        w.write_all(&line)
            .and_then(|_| w.flush())
            .map_err(|e| AgentError::Unavailable(e.to_string()))?;
        let mut answer = String::new();
        let n = stream
            .by_ref()
            .take(MAX_LINE as u64 + 1)
            .read_line(&mut answer)
            .map_err(|e| AgentError::Unavailable(e.to_string()))?;
        if n == 0 {
            return Err(AgentError::Unavailable(
                "the agent closed the connection".into(),
            ));
        }
        let resp: AgentResponse =
            serde_json::from_str(&answer).map_err(|e| AgentError::Protocol(e.to_string()))?;
        match (resp.ok, resp.error) {
            (_, Some(e)) => Err(AgentError::Remote(e)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }
}

impl AgentApi for AgentClient {
    fn call(&self, op: AgentOp) -> Result<Value, AgentError> {
        let unavailable = |e: String| AgentError::Unavailable(format!("{}: {e}", self.addr));
        let sock = TcpStream::connect_timeout(&self.addr, Duration::from_secs(5))
            .map_err(|e| unavailable(e.to_string()))?;
        let _ = sock.set_read_timeout(Some(self.timeout));
        let _ = sock.set_write_timeout(Some(self.timeout));
        let name = ServerName::IpAddress(self.addr.ip().into());
        let conn = rustls::ClientConnection::new(self.config.clone(), name)
            .map_err(|e| unavailable(e.to_string()))?;
        let mut tls = rustls::StreamOwned::new(conn, sock);
        while tls.conn.is_handshaking() {
            tls.conn
                .complete_io(&mut tls.sock)
                .map_err(|e| unavailable(format!("TLS: {e}")))?;
        }
        let mut stream = BufReader::new(tls);
        let hello: HelloResult = serde_json::from_value(Self::exchange(
            &mut stream,
            1,
            AgentOp::Hello {
                protocol: AGENT_PROTOCOL_VERSION,
            },
        )?)
        .map_err(|e| AgentError::Protocol(e.to_string()))?;
        if hello.protocol < AGENT_PROTOCOL_VERSION {
            return Err(AgentError::Remote(ErrorBody {
                code: "unsupported".into(),
                message: format!("agent protocol {}", hello.protocol),
            }));
        }
        if matches!(op, AgentOp::Hello { .. }) {
            return serde_json::to_value(hello).map_err(|e| AgentError::Protocol(e.to_string()));
        }
        Self::exchange(&mut stream, 2, op)
    }
}

/// The agent in process: netd's tests drive the real agent code on a
/// `RecordingExec` with no TLS in between.
impl AgentApi for Agent {
    fn call(&self, op: AgentOp) -> Result<Value, AgentError> {
        self.handle(op)
            .map_err(|e| AgentError::Remote(ErrorBody::from(&e)))
    }
}
