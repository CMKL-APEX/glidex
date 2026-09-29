//! Blocking client for glidex-netd, used by the control plane (run it in
//! `spawn_blocking`).

use crate::proto::{ErrorBody, HelloResult, Op, Request, Response, PROTOCOL_VERSION};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No socket, connection refused, or the connection dropped.
    #[error("glidex-netd unavailable: {0}")]
    Unavailable(String),
    #[error("glidex-netd protocol error: {0}")]
    Protocol(String),
    #[error("{}", .0.message)]
    Remote(ErrorBody),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    pub hello: HelloResult,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream =
            UnixStream::connect(path).map_err(|e| ClientError::Unavailable(format!("{}: {}", path.display(), e)))?;
        let writer = stream
            .try_clone()
            .map_err(|e| ClientError::Unavailable(e.to_string()))?;
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            hello: HelloResult {
                protocol: 0,
                netd_version: String::new(),
            },
        };
        let hello: HelloResult = client.call(
            Op::Hello {
                protocol: PROTOCOL_VERSION,
            },
            Duration::from_secs(5),
        )?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(ClientError::Protocol(format!(
                "netd speaks protocol {}, expected {}",
                hello.protocol, PROTOCOL_VERSION
            )));
        }
        client.hello = hello;
        Ok(client)
    }

    pub fn call_value(&mut self, op: Op, timeout: Duration) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_string(&Request { id, op })
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        let unavailable = |e: std::io::Error| ClientError::Unavailable(e.to_string());
        self.writer.set_write_timeout(Some(timeout)).map_err(unavailable)?;
        self.writer.write_all(line.as_bytes()).map_err(unavailable)?;
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .map_err(unavailable)?;
        let mut reply = String::new();
        let n = self.reader.read_line(&mut reply).map_err(unavailable)?;
        if n == 0 {
            return Err(ClientError::Unavailable("netd closed the connection".into()));
        }
        let resp: Response =
            serde_json::from_str(&reply).map_err(|e| ClientError::Protocol(e.to_string()))?;
        if resp.id != id {
            return Err(ClientError::Protocol(format!(
                "response id {} for request {}",
                resp.id, id
            )));
        }
        match (resp.ok, resp.error) {
            (_, Some(err)) => Err(ClientError::Remote(err)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }

    pub fn call<T: DeserializeOwned>(&mut self, op: Op, timeout: Duration) -> Result<T, ClientError> {
        let v = self.call_value(op, timeout)?;
        serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

/// Socket paths under a run directory (default `/run/glidex`).
pub fn socket_paths(run_dir: &Path) -> (PathBuf, PathBuf) {
    (
        run_dir.join(crate::proto::FULL_SOCKET_NAME),
        run_dir.join(crate::proto::STATUS_SOCKET_NAME),
    )
}
