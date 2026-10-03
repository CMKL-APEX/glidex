//! Cloud Hypervisor's HTTP API over its Unix socket. Message framing is
//! hand-rolled to match upstream's `api_client` exactly.

use crate::{connect, GuestState, HvError, Observed};
use std::io::{Read, Write};

pub struct ChClient {
    socket_path: String,
}

/// Find the end of HTTP headers (position after the \r\n\r\n separator).
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Parse Content-Length from raw HTTP header bytes.
fn parse_content_length(headers: &[u8]) -> usize {
    let header_str = String::from_utf8_lossy(headers).to_lowercase();
    for line in header_str.lines() {
        if let Some(val) = line.strip_prefix("content-length:") {
            return val.trim().parse().unwrap_or(0);
        }
    }
    0
}

impl ChClient {
    pub fn new(socket_path: &str) -> Self {
        Self { socket_path: socket_path.to_string() }
    }

    /// One request; returns the status code and body.
    fn send_request(&self, method: &str, path: &str, body: Option<&str>) -> Result<(u16, Option<String>), HvError> {
        let mut stream = connect(&self.socket_path)?;

        // Request format of the official cloud-hypervisor api_client:
        //   {METHOD} /api/v1/{path} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n
        // With body: add Content-Type and Content-Length headers.
        let request = match body {
            Some(body_str) => format!(
                "{} /api/v1{} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                method,
                path,
                body_str.len(),
                body_str
            ),
            None => format!("{} /api/v1{} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n", method, path),
        };
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(header_end) = find_header_end(&raw) {
                        let content_len = parse_content_length(&raw[..header_end]);
                        if raw.len() >= header_end + content_len {
                            break;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                    return Err(HvError::Connect(format!("{}: {}", self.socket_path, e)))
                }
                Err(e) => return Err(e.into()),
            }
        }
        if raw.is_empty() {
            return Err(HvError::Connect(format!("{}: no response", self.socket_path)));
        }

        let response = String::from_utf8_lossy(&raw);
        let status_code = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        let body = response.find("\r\n\r\n").and_then(|pos| {
            let b = &response[pos + 4..];
            (!b.is_empty()).then(|| b.to_string())
        });
        Ok((status_code, body))
    }

    /// Check that the response status indicates success (2xx).
    fn expect_success(&self, method: &str, path: &str, body: Option<&str>) -> Result<Option<String>, HvError> {
        let (status, response_body) = self.send_request(method, path, body)?;
        if (200..300).contains(&status) {
            Ok(response_body)
        } else {
            Err(HvError::Api(format!(
                "{} /api/v1{} failed with status {}: {}",
                method,
                path,
                status,
                response_body.unwrap_or_default()
            )))
        }
    }

    /// The VMM answers (launch health check).
    pub fn ping(&self) -> Result<(), HvError> {
        self.expect_success("GET", "/vmm.ping", None).map(|_| ())
    }

    /// The guest's state and its device ids (`GET /vm.info`).
    pub fn observe(&self) -> Result<Observed, HvError> {
        let (status, body) = self.send_request("GET", "/vm.info", None)?;
        if !(200..300).contains(&status) {
            // No VM created yet: CH answers 4xx/5xx with "VM is not created".
            return Ok(Observed { guest: GuestState::NotCreated, device_ids: Vec::new() });
        }
        let v: serde_json::Value = serde_json::from_str(body.as_deref().unwrap_or("{}"))
            .map_err(|e| HvError::Api(format!("vm.info: {}", e)))?;
        Ok(parse_vm_info(&v))
    }

    pub fn pause(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.pause", None).map(|_| ())
    }

    pub fn resume(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.resume", None).map(|_| ())
    }

    /// Shut the VMM down cleanly (the VM's disks are closed first); it then
    /// exits on its own. A bare SIGKILL can leave a qcow2 disk that CH
    /// refuses to open again.
    pub fn shutdown_vmm(&self) -> Result<(), HvError> {
        match self.expect_success("PUT", "/vmm.shutdown", None) {
            // It may exit before answering.
            Err(HvError::Connect(_)) => Ok(()),
            other => other.map(|_| ()),
        }
    }

    pub fn power_button(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.power-button", None).map(|_| ())
    }

    /// Hot-plug a VFIO device under a deterministic id.
    pub fn add_device(&self, device_path: &str, id: &str) -> Result<(), HvError> {
        let body = serde_json::json!({ "path": device_path, "iommu": false, "id": id }).to_string();
        self.expect_success("PUT", "/vm.add-device", Some(&body)).map(|_| ())
    }

    pub fn remove_device(&self, id: &str) -> Result<(), HvError> {
        let body = serde_json::json!({ "id": id }).to_string();
        self.expect_success("PUT", "/vm.remove-device", Some(&body)).map(|_| ())
    }
}

fn parse_vm_info(v: &serde_json::Value) -> Observed {
    let guest = match v.get("state").and_then(|s| s.as_str()).unwrap_or("") {
        "Created" => GuestState::Created,
        "Running" => GuestState::Running,
        "Paused" => GuestState::Paused,
        "Shutdown" => GuestState::Shutdown,
        _ => GuestState::NotCreated,
    };
    let device_ids = v
        .pointer("/config/devices")
        .and_then(|d| d.as_array())
        .map(|a| a.iter().filter_map(|d| d.get("id").and_then(|i| i.as_str()).map(String::from)).collect())
        .unwrap_or_default();
    Observed { guest, device_ids }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Answer one request on `path` with `response`, returning the request.
    fn serve_once(path: &std::path::Path, response: &'static str) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read the whole request: headers, then Content-Length bytes.
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                if let Some(end) = find_header_end(&req) {
                    if n == 0 || req.len() >= end + parse_content_length(&req[..end]) {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            s.write_all(response.as_bytes()).unwrap();
            String::from_utf8_lossy(&req).into_owned()
        })
    }

    #[test]
    fn vm_info_state_and_devices() {
        let v = serde_json::json!({
            "state": "Running",
            "config": { "devices": [ { "path": "/sys/bus/pci/devices/0000:41:00.0", "id": "_vfio_0000_41_00_0" } ] }
        });
        assert_eq!(parse_vm_info(&v), Observed { guest: GuestState::Running, device_ids: vec!["_vfio_0000_41_00_0".into()] });
        assert_eq!(parse_vm_info(&serde_json::json!({"state": "Paused"})).guest, GuestState::Paused);
        assert_eq!(parse_vm_info(&serde_json::json!({})).guest, GuestState::NotCreated);
    }

    #[test]
    fn requests_use_the_api_client_framing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(&sock, "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        ChClient::new(sock.to_str().unwrap()).add_device("/sys/bus/pci/devices/0000:41:00.0", "_vfio_0000_41_00_0").unwrap();
        let req = server.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.add-device HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\n"), "{req}");
        assert!(req.ends_with(r#"{"id":"_vfio_0000_41_00_0","iommu":false,"path":"/sys/bus/pci/devices/0000:41:00.0"}"#), "{req}");
    }

    #[test]
    fn error_status_is_an_api_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(&sock, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nnope");
        let err = ChClient::new(sock.to_str().unwrap()).pause().unwrap_err();
        server.join().unwrap();
        assert!(matches!(err, HvError::Api(ref m) if m.contains("500") && m.contains("nope")), "{err}");
    }

    #[test]
    fn missing_socket_is_a_connect_error() {
        assert!(matches!(ChClient::new("/nonexistent/api.sock").ping(), Err(HvError::Connect(_))));
    }
}
