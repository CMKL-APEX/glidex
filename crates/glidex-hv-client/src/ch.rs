//! Cloud Hypervisor's HTTP API over its Unix socket. Message framing is
//! hand-rolled to match upstream's `api_client` exactly.

use crate::{GuestState, HvError, Observed, connect};
use std::io::{Read, Write};

pub struct ChClient {
    socket_path: String,
}

/// How `vm.restore` fills guest memory (spec/33-agent-sandboxes.md §0):
/// `Copy` reads the snapshot into guest RAM before resuming; `OnDemand`
/// faults pages in through userfaultfd (with or without fetching the rest
/// in the background); `CopyOnWrite` maps the snapshot's memory file
/// copy-on-write, sharing its pages with other VMs restored from it. The
/// names are Cloud Hypervisor's own in its JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum MemoryRestoreMode {
    #[default]
    Copy,
    OnDemand,
    OnDemandNoBackgroundFetchPages,
    CopyOnWrite,
}

/// Find the end of HTTP headers (position after the \r\n\r\n separator).
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
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

/// `file://<dir>`, as Cloud Hypervisor's snapshot and restore take it.
fn file_url(dir: &std::path::Path) -> String {
    format!("file://{}", dir.display())
}

impl ChClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// One request; returns the status code and body.
    fn send_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, Option<String>), HvError> {
        self.send_request_on(connect(&self.socket_path)?, method, path, body)
    }

    fn send_request_on(
        &self,
        mut stream: std::os::unix::net::UnixStream,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, Option<String>), HvError> {
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
            None => format!(
                "{} /api/v1{} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n",
                method, path
            ),
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
                    return Err(HvError::Connect(format!("{}: {}", self.socket_path, e)));
                }
                Err(e) => return Err(e.into()),
            }
        }
        if raw.is_empty() {
            return Err(HvError::Connect(format!(
                "{}: no response",
                self.socket_path
            )));
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
    fn expect_success(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<Option<String>, HvError> {
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
            return Ok(Observed {
                guest: GuestState::NotCreated,
                device_ids: Vec::new(),
                vcpu_count: None,
            });
        }
        let v: serde_json::Value = serde_json::from_str(body.as_deref().unwrap_or("{}"))
            .map_err(|e| HvError::Api(format!("vm.info: {}", e)))?;
        Ok(parse_vm_info(&v))
    }

    /// Per-disk block counters (`GET /vm.counters`; metering §5.2).
    pub fn block_stats(&self) -> Result<Vec<crate::stats::BlockStats>, HvError> {
        let body = self.expect_success("GET", "/vm.counters", None)?;
        let v: serde_json::Value = serde_json::from_str(body.as_deref().unwrap_or("{}"))
            .map_err(|e| HvError::Api(format!("vm.counters: {}", e)))?;
        Ok(crate::stats::parse_ch_counters(&v))
    }

    pub fn pause(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.pause", None).map(|_| ())
    }

    pub fn resume(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.resume", None).map(|_| ())
    }

    /// Write the VM's snapshot (`config.json`, `state.json` and its memory)
    /// into `dir`, which must exist and be empty. The VM must be paused.
    /// Writing all of its memory takes about half a second per GiB on a
    /// local disk (spec/33-agent-sandboxes.md §0.1), so the caller gives
    /// the wait.
    pub fn snapshot(
        &self,
        dir: &std::path::Path,
        wait: std::time::Duration,
    ) -> Result<(), HvError> {
        let body = serde_json::json!({ "destination_url": file_url(dir) }).to_string();
        self.expect_success_within("PUT", "/vm.snapshot", Some(&body), wait)
            .map(|_| ())
    }

    /// Restore a VM into this VMM, which has none, from the snapshot in
    /// `dir`, filling its memory as `mode` says, and resume it if `resume`.
    pub fn restore(
        &self,
        dir: &std::path::Path,
        mode: MemoryRestoreMode,
        resume: bool,
        wait: std::time::Duration,
    ) -> Result<(), HvError> {
        let body = serde_json::json!({
            "source_url": file_url(dir),
            "memory_restore_mode": mode,
            "resume": resume,
        })
        .to_string();
        self.expect_success_within("PUT", "/vm.restore", Some(&body), wait)
            .map(|_| ())
    }

    /// Like `expect_success`, waiting up to `wait` for the answer.
    fn expect_success_within(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        wait: std::time::Duration,
    ) -> Result<Option<String>, HvError> {
        let stream = connect(&self.socket_path)?;
        stream.set_read_timeout(Some(wait.max(crate::IO_TIMEOUT)))?;
        let (status, response) = self.send_request_on(stream, method, path, body)?;
        if (200..300).contains(&status) {
            Ok(response)
        } else {
            Err(HvError::Api(format!(
                "{method} /api/v1{path} failed with status {status}: {}",
                response.unwrap_or_default()
            )))
        }
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
        self.expect_success("PUT", "/vm.power-button", None)
            .map(|_| ())
    }

    /// Hot-plug a VFIO device under a deterministic id.
    pub fn add_device(&self, device_path: &str, id: &str) -> Result<(), HvError> {
        let body = serde_json::json!({ "path": device_path, "iommu": false, "id": id }).to_string();
        self.expect_success("PUT", "/vm.add-device", Some(&body))
            .map(|_| ())
    }

    pub fn remove_device(&self, id: &str) -> Result<(), HvError> {
        let body = serde_json::json!({ "id": id }).to_string();
        self.expect_success("PUT", "/vm.remove-device", Some(&body))
            .map(|_| ())
    }

    /// Receive a migrating VM into this VMM, which has none, over
    /// `receiver_url` (`unix:<path>`; spec/32-live-migration.md §4). Cloud
    /// Hypervisor answers only when the migration has ended: `Ok` once the
    /// guest runs here, an error when it failed or was abandoned. Its VMM
    /// answers nothing else meanwhile, so this waits without a timeout:
    /// call it on a thread of its own, and end it by killing the VMM.
    pub fn receive_migration(&self, receiver_url: &str) -> Result<(), HvError> {
        let stream = connect(&self.socket_path)?;
        stream.set_read_timeout(None)?;
        let body = serde_json::json!({ "receiver_url": receiver_url }).to_string();
        let path = "/vm.receive-migration";
        let (status, response) = self.send_request_on(stream, "PUT", path, Some(&body))?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(HvError::Api(format!(
                "PUT /api/v1{path} failed with status {status}: {}",
                response.unwrap_or_default()
            )))
        }
    }

    /// Start sending this VMM's guest to `destination_url` (`unix:<path>`;
    /// spec/32-live-migration.md §4). Cloud Hypervisor v55 answers once the
    /// copy has started: the result is not in the answer. On success its
    /// VMM exits; otherwise it resumes the guest. A copy that hasn't
    /// converged after `timeout_s` is cancelled, keeping the guest here.
    pub fn send_migration(
        &self,
        destination_url: &str,
        downtime_ms: u32,
        timeout_s: u64,
    ) -> Result<(), HvError> {
        let body = serde_json::json!({
            "destination_url": destination_url,
            "downtime_ms": downtime_ms,
            "timeout_s": timeout_s,
            "timeout_strategy": "Cancel",
        })
        .to_string();
        self.expect_success("PUT", "/vm.send-migration", Some(&body))
            .map(|_| ())
    }

    /// Ask the VMM to cancel the migration it is sending; a best effort,
    /// which may come too late to stop it.
    pub fn cancel_migration(&self) -> Result<(), HvError> {
        self.expect_success("PUT", "/vm.cancel-migration", None)
            .map(|_| ())
    }

    /// Resize the running guest's active vCPU count to `desired`.
    pub fn resize_vcpus(&self, desired: u8) -> Result<(), HvError> {
        let body = serde_json::json!({ "desired_vcpus": desired }).to_string();
        self.expect_success("PUT", "/vm.resize", Some(&body))
            .map(|_| ())
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
        .map(|a| {
            a.iter()
                .filter_map(|d| d.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let vcpu_count = v
        .pointer("/config/cpus/boot_vcpus")
        .and_then(|c| c.as_u64())
        .map(|n| n.min(255) as u8);
    Observed {
        guest,
        device_ids,
        vcpu_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// Answer one request on `path` with `response`, returning the request.
    fn serve_once(
        path: &std::path::Path,
        response: &'static str,
    ) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            // Read the whole request: headers, then Content-Length bytes.
            let mut req = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = s.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                if let Some(end) = find_header_end(&req)
                    && (n == 0 || req.len() >= end + parse_content_length(&req[..end]))
                {
                    break;
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
            "config": {
                "devices": [ { "path": "/sys/bus/pci/devices/0000:41:00.0", "id": "_vfio_0000_41_00_0" } ],
                "cpus": { "boot_vcpus": 4 }
            }
        });
        assert_eq!(
            parse_vm_info(&v),
            Observed {
                guest: GuestState::Running,
                device_ids: vec!["_vfio_0000_41_00_0".into()],
                vcpu_count: Some(4),
            }
        );
        assert_eq!(
            parse_vm_info(&serde_json::json!({"state": "Paused"})).guest,
            GuestState::Paused
        );
        assert_eq!(
            parse_vm_info(&serde_json::json!({})).guest,
            GuestState::NotCreated
        );
    }

    #[test]
    fn requests_use_the_api_client_framing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .add_device("/sys/bus/pci/devices/0000:41:00.0", "_vfio_0000_41_00_0")
            .unwrap();
        let req = server.join().unwrap();
        assert!(req.starts_with("PUT /api/v1/vm.add-device HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\nContent-Type: application/json\r\n"), "{req}");
        // Key order depends on serde_json's features, unified across the workspace.
        let body: serde_json::Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"id": "_vfio_0000_41_00_0", "iommu": false, "path": "/sys/bus/pci/devices/0000:41:00.0"}),
            "{req}"
        );
    }

    #[test]
    fn error_status_is_an_api_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\n\r\nnope",
        );
        let err = ChClient::new(sock.to_str().unwrap()).pause().unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(err, HvError::Api(ref m) if m.contains("500") && m.contains("nope")),
            "{err}"
        );
    }

    #[test]
    fn missing_socket_is_a_connect_error() {
        assert!(matches!(
            ChClient::new("/nonexistent/api.sock").ping(),
            Err(HvError::Connect(_))
        ));
    }

    /// A receive names its URL, and a failed one is an error with Cloud
    /// Hypervisor's words.
    #[test]
    fn receiving_a_migration_waits_for_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .receive_migration("unix:/run/glidex/vms/v/migration.sock")
            .unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.receive-migration HTTP/1.1\r\n"),
            "{req}"
        );
        let body: serde_json::Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "receiver_url": "unix:/run/glidex/vms/v/migration.sock" })
        );

        let sock = dir.path().join("api2.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 9\r\n\r\nCPUID bad",
        );
        let err = ChClient::new(sock.to_str().unwrap())
            .receive_migration("unix:/x")
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(err, HvError::Api(ref m) if m.contains("CPUID bad")),
            "{err}"
        );
    }

    /// A send names its destination, the downtime and a timeout that
    /// cancels; a cancel has no body.
    #[test]
    fn sending_a_migration_names_its_limits() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .send_migration("unix:/run/glidex/vms/v/migration-send.sock", 300, 900)
            .unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.send-migration HTTP/1.1\r\n"),
            "{req}"
        );
        let body: serde_json::Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "destination_url": "unix:/run/glidex/vms/v/migration-send.sock",
                "downtime_ms": 300,
                "timeout_s": 900,
                "timeout_strategy": "Cancel",
            })
        );

        let sock = dir.path().join("api2.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .cancel_migration()
            .unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.cancel-migration HTTP/1.1\r\n"),
            "{req}"
        );
    }

    #[test]
    fn resize_vcpus_sends_desired_vcpus() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .resize_vcpus(8)
            .unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.resize HTTP/1.1\r\n"),
            "{req}"
        );
        let body: serde_json::Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body, serde_json::json!({ "desired_vcpus": 8 }), "{req}");
    }

    /// A snapshot names its directory as a `file://` URL; a restore adds
    /// the memory mode under Cloud Hypervisor's own names, and whether to
    /// resume. A refusal is the API's error.
    #[test]
    fn snapshot_and_restore_name_their_directory_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let wait = std::time::Duration::from_secs(5);
        let sock = dir.path().join("api.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        );
        ChClient::new(sock.to_str().unwrap())
            .snapshot(std::path::Path::new("/var/lib/g/checkpoints/c1"), wait)
            .unwrap();
        let req = server.join().unwrap();
        assert!(
            req.starts_with("PUT /api/v1/vm.snapshot HTTP/1.1\r\n"),
            "{req}"
        );
        let body: serde_json::Value =
            serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({ "destination_url": "file:///var/lib/g/checkpoints/c1" })
        );

        for (mode, name) in [
            (MemoryRestoreMode::Copy, "Copy"),
            (MemoryRestoreMode::OnDemand, "OnDemand"),
            (
                MemoryRestoreMode::OnDemandNoBackgroundFetchPages,
                "OnDemandNoBackgroundFetchPages",
            ),
            (MemoryRestoreMode::CopyOnWrite, "CopyOnWrite"),
        ] {
            let sock = dir.path().join(format!("{name}.sock"));
            let server = serve_once(
                &sock,
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
            );
            ChClient::new(sock.to_str().unwrap())
                .restore(
                    std::path::Path::new("/run/g/vms/f/restore"),
                    mode,
                    true,
                    wait,
                )
                .unwrap();
            let req = server.join().unwrap();
            assert!(
                req.starts_with("PUT /api/v1/vm.restore HTTP/1.1\r\n"),
                "{req}"
            );
            let body: serde_json::Value =
                serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(
                body,
                serde_json::json!({
                    "source_url": "file:///run/g/vms/f/restore",
                    "memory_restore_mode": name,
                    "resume": true,
                })
            );
        }

        let sock = dir.path().join("refused.sock");
        let server = serve_once(
            &sock,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\nno such state",
        );
        let err = ChClient::new(sock.to_str().unwrap())
            .restore(
                std::path::Path::new("/x"),
                MemoryRestoreMode::Copy,
                true,
                wait,
            )
            .unwrap_err();
        server.join().unwrap();
        assert!(
            matches!(err, HvError::Api(ref m) if m.contains("no such state")),
            "{err}"
        );
    }
}
