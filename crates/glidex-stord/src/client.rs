//! Blocking client for glidex-stord, used by the control plane (run it in
//! `spawn_blocking`).

use crate::proto::{ErrorBody, HelloResult, Op, PROTOCOL_VERSION, Request, Response};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// LVM can take a while (a busy pool, udev settling).
pub const TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// No socket, connection refused, or the connection dropped.
    #[error("glidex-stord unavailable: {0}")]
    Unavailable(String),
    #[error("glidex-stord protocol error: {0}")]
    Protocol(String),
    #[error("{}", .0.message)]
    Remote(ErrorBody),
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    pub hello: HelloResult,
    /// Logged by stord next to each change.
    pub on_behalf_of: Option<String>,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)
            .map_err(|e| ClientError::Unavailable(format!("{}: {e}", path.display())))?;
        let writer = stream
            .try_clone()
            .map_err(|e| ClientError::Unavailable(e.to_string()))?;
        let mut client = Client {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            hello: HelloResult {
                protocol: 0,
                stord_version: String::new(),
            },
            on_behalf_of: None,
        };
        let hello: HelloResult = client.call(Op::Hello {
            protocol: PROTOCOL_VERSION,
        })?;
        if hello.protocol != PROTOCOL_VERSION {
            return Err(ClientError::Protocol(format!(
                "stord speaks protocol {}, expected {PROTOCOL_VERSION}",
                hello.protocol
            )));
        }
        client.hello = hello;
        Ok(client)
    }

    pub fn call_value(&mut self, op: Op) -> Result<Value, ClientError> {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            id,
            op,
            on_behalf_of: self.on_behalf_of.clone(),
        };
        let mut line =
            serde_json::to_string(&request).map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        let unavailable = |e: std::io::Error| ClientError::Unavailable(e.to_string());
        self.writer
            .set_write_timeout(Some(TIMEOUT))
            .map_err(unavailable)?;
        self.writer
            .write_all(line.as_bytes())
            .map_err(unavailable)?;
        self.reader
            .get_ref()
            .set_read_timeout(Some(TIMEOUT))
            .map_err(unavailable)?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply).map_err(unavailable)? == 0 {
            return Err(ClientError::Unavailable(
                "stord closed the connection".into(),
            ));
        }
        let resp: Response =
            serde_json::from_str(&reply).map_err(|e| ClientError::Protocol(e.to_string()))?;
        if resp.id != id {
            return Err(ClientError::Protocol(format!(
                "response id {} for request {id}",
                resp.id
            )));
        }
        match (resp.ok, resp.error) {
            (_, Some(err)) => Err(ClientError::Remote(err)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(Value::Null),
        }
    }

    pub fn call<T: DeserializeOwned>(&mut self, op: Op) -> Result<T, ClientError> {
        let v = self.call_value(op)?;
        serde_json::from_value(v).map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

/// The socket under a run directory (default `/run/glidex-stord`).
pub fn socket_path(run_dir: &Path) -> std::path::PathBuf {
    run_dir.join(crate::proto::SOCKET_NAME)
}
