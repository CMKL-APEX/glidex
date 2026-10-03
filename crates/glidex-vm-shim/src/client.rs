//! Blocking `shim.sock` client, used by the control plane (run it in
//! `spawn_blocking`). One connection per call.

use crate::proto::{HelloResult, Op, Request, Response, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION};
use crate::state::InstanceFile;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    /// No socket, connection refused, or the connection dropped.
    #[error("shim unavailable: {0}")]
    Unavailable(String),
    #[error("shim protocol error: {0}")]
    Protocol(String),
    #[error("shim refused: {code}: {message}")]
    Remote { code: String, message: String },
}

pub struct ShimClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    pub hello: HelloResult,
}

impl ShimClient {
    pub fn connect(path: &Path) -> Result<Self, ShimError> {
        let stream = UnixStream::connect(path).map_err(|e| ShimError::Unavailable(format!("{}: {}", path.display(), e)))?;
        stream.set_read_timeout(Some(Duration::from_secs(15))).ok();
        stream.set_write_timeout(Some(Duration::from_secs(15))).ok();
        let writer = stream.try_clone().map_err(|e| ShimError::Unavailable(e.to_string()))?;
        let mut c = ShimClient {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
            hello: HelloResult { protocol: 0, shim_version: String::new() },
        };
        let hello: HelloResult = serde_json::from_value(c.call(Op::Hello { protocol: PROTOCOL_VERSION })?)
            .map_err(|e| ShimError::Protocol(e.to_string()))?;
        if hello.protocol < MIN_PROTOCOL_VERSION || hello.protocol > PROTOCOL_VERSION {
            return Err(ShimError::Protocol(format!("shim speaks protocol {}", hello.protocol)));
        }
        c.hello = hello;
        Ok(c)
    }

    fn call(&mut self, op: Op) -> Result<serde_json::Value, ShimError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request { id, op }).map_err(|e| ShimError::Protocol(e.to_string()))?;
        line.push(b'\n');
        self.writer.write_all(&line).map_err(|e| ShimError::Unavailable(e.to_string()))?;
        let mut resp = String::new();
        if self.reader.read_line(&mut resp).map_err(|e| ShimError::Unavailable(e.to_string()))? == 0 {
            return Err(ShimError::Unavailable("connection closed".into()));
        }
        let resp: Response = serde_json::from_str(&resp).map_err(|e| ShimError::Protocol(e.to_string()))?;
        if resp.id != id {
            return Err(ShimError::Protocol(format!("reply to {} for request {}", resp.id, id)));
        }
        match (resp.ok, resp.error) {
            (_, Some(e)) => Err(ShimError::Remote { code: e.code, message: e.message }),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(serde_json::Value::Null),
        }
    }

    pub fn status(&mut self) -> Result<InstanceFile, ShimError> {
        serde_json::from_value(self.call(Op::Status)?).map_err(|e| ShimError::Protocol(e.to_string()))
    }

    pub fn stop(&mut self, grace_secs: u64) -> Result<(), ShimError> {
        self.call(Op::Stop { grace_secs }).map(|_| ())
    }

    pub fn kill(&mut self) -> Result<(), ShimError> {
        self.call(Op::Kill).map(|_| ())
    }

    pub fn release(&mut self) -> Result<(), ShimError> {
        self.call(Op::Release).map(|_| ())
    }
}
