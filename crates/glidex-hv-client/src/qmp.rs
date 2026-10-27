//! QEMU Machine Protocol over a Unix socket. Each command opens a fresh
//! connection, performs the capabilities handshake, sends the command and
//! waits for its reply, skipping asynchronous events.
//!
//! A QMP monitor serves one client at a time, so a connection attempt that
//! finds it busy is retried for a short while (spec §16).

use crate::{GuestState, HvError, Observed, connect};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub struct QmpClient {
    socket_path: String,
}

/// How long to keep retrying a monitor another client holds.
const BUSY_RETRY: Duration = Duration::from_secs(2);

impl QmpClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
        }
    }

    /// Open a connection and complete the `qmp_capabilities` handshake.
    fn connect(&self) -> Result<(UnixStream, BufReader<UnixStream>), HvError> {
        let deadline = Instant::now() + BUSY_RETRY;
        loop {
            match self.try_connect() {
                Err(HvError::Connect(_))
                    if Instant::now() < deadline
                        && std::path::Path::new(&self.socket_path).exists() =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                other => return other,
            }
        }
    }

    fn try_connect(&self) -> Result<(UnixStream, BufReader<UnixStream>), HvError> {
        let stream = connect(&self.socket_path)?;
        let mut reader = BufReader::new(stream.try_clone()?);

        // QEMU sends a greeting line on connect. While another client holds
        // the monitor, the connection waits in the backlog and no greeting
        // comes: give up quickly and let `connect` retry.
        stream.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut greeting = String::new();
        if reader.read_line(&mut greeting).unwrap_or(0) == 0 || !greeting.contains("QMP") {
            return Err(HvError::Connect(format!(
                "{}: no QMP greeting",
                self.socket_path
            )));
        }
        stream.set_read_timeout(Some(crate::IO_TIMEOUT))?;

        let mut writer = stream.try_clone()?;
        writer.write_all(b"{\"execute\":\"qmp_capabilities\"}\r\n")?;
        writer.flush()?;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Err(HvError::Connect(
                    "QMP connection closed during handshake".to_string(),
                ));
            }
            if line.contains("\"return\"") {
                break;
            }
            if line.contains("\"error\"") {
                return Err(HvError::Api(format!(
                    "QMP handshake failed: {}",
                    line.trim()
                )));
            }
        }
        Ok((stream, reader))
    }

    /// Run one command; returns its `return` value.
    pub fn execute(&self, command: &Value) -> Result<Value, HvError> {
        let (stream, mut reader) = self.connect()?;
        let mut writer = stream.try_clone()?;
        writer.write_all(command.to_string().as_bytes())?;
        writer.write_all(b"\r\n")?;
        writer.flush()?;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                // `quit` closes the connection without replying.
                return Err(HvError::Connect(
                    "QMP connection closed before reply".to_string(),
                ));
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(e) = v.get("error") {
                return Err(HvError::Api(e.to_string()));
            }
            if let Some(r) = v.get("return") {
                return Ok(r.clone());
            }
            // An asynchronous event: keep reading for the reply.
        }
    }

    /// Per-disk block counters (`query-blockstats`; metering §5.2).
    pub fn block_stats(&self) -> Result<Vec<crate::stats::BlockStats>, HvError> {
        let v = self.execute(&serde_json::json!({ "execute": "query-blockstats" }))?;
        Ok(crate::stats::parse_qmp_blockstats(&v))
    }

    /// Each vCPU's `cpu-index` and host thread id (`query-cpus-fast`), for
    /// pinning vCPUs, which QEMU has no option for.
    pub fn query_cpus_fast(&self) -> Result<Vec<(u32, i32)>, HvError> {
        let v = self.execute(&serde_json::json!({ "execute": "query-cpus-fast" }))?;
        parse_cpus_fast(&v)
    }

    fn run(&self, name: &str) -> Result<(), HvError> {
        self.execute(&serde_json::json!({ "execute": name }))
            .map(|_| ())
    }

    /// The monitor answers with its greeting (launch health check).
    pub fn ping(&self) -> Result<(), HvError> {
        self.try_connect().map(|_| ())
    }

    pub fn cont(&self) -> Result<(), HvError> {
        self.run("cont")
    }

    pub fn stop(&self) -> Result<(), HvError> {
        self.run("stop")
    }

    /// Ask QEMU to exit. It may close the connection before answering.
    pub fn quit(&self) -> Result<(), HvError> {
        match self.run("quit") {
            Err(HvError::Connect(_)) => Ok(()),
            other => other,
        }
    }

    pub fn system_powerdown(&self) -> Result<(), HvError> {
        self.run("system_powerdown")
    }

    pub fn device_add_vfio(&self, bdf: &str, id: &str) -> Result<(), HvError> {
        self.execute(&serde_json::json!({
            "execute": "device_add",
            "arguments": { "driver": "vfio-pci", "host": bdf, "id": id }
        }))
        .map(|_| ())
    }

    pub fn device_del(&self, id: &str) -> Result<(), HvError> {
        self.execute(&serde_json::json!({ "execute": "device_del", "arguments": { "id": id } }))
            .map(|_| ())
    }

    /// The guest's state (`query-status`), devices (`qom-list /machine/peripheral`),
    /// and current vCPU count (`query-cpus-fast`).
    pub fn observe(&self) -> Result<Observed, HvError> {
        let status = self.execute(&serde_json::json!({ "execute": "query-status" }))?;
        let peripherals = self.execute(&serde_json::json!({
            "execute": "qom-list", "arguments": { "path": "/machine/peripheral" }
        }))?;
        let cpus = self.execute(&serde_json::json!({ "execute": "query-cpus-fast" })).ok();
        let vcpu_count = cpus
            .as_ref()
            .and_then(|c| c.as_array())
            .map(|a| a.len().min(255) as u8);
        let mut obs = parse_observed(&status, &peripherals);
        obs.vcpu_count = vcpu_count;
        Ok(obs)
    }

    /// Resize active vCPUs to `desired` using ACPI CPU hotplug via QMP
    /// (`query-hotpluggable-cpus`, `device_add`, `device_del`).
    pub fn resize_vcpus(&self, desired: u8) -> Result<(), HvError> {
        let slots = self.execute(&serde_json::json!({ "execute": "query-hotpluggable-cpus" }))?;
        let slots_arr = slots.as_array().ok_or_else(|| {
            HvError::Api("query-hotpluggable-cpus did not return an array".into())
        })?;

        struct CpuSlot {
            core_id: u64,
            populated: bool,
            driver: String,
            props: Value,
            id: String,
        }

        let mut parsed_slots: Vec<CpuSlot> = Vec::new();
        for slot in slots_arr {
            let props = slot.get("props").cloned().unwrap_or(Value::Null);
            let core_id = props.get("core-id").and_then(|c| c.as_u64()).unwrap_or(0);
            let populated = slot.get("qom-path").is_some();
            let driver = slot
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("qemu64-x86_64-cpu")
                .to_string();
            let id = format!("vcpu_{}", core_id);
            parsed_slots.push(CpuSlot {
                core_id,
                populated,
                driver,
                props,
                id,
            });
        }

        parsed_slots.sort_by_key(|s| s.core_id);

        for slot in &parsed_slots {
            if (slot.core_id as u8) < desired {
                if !slot.populated {
                    let mut args = serde_json::Map::new();
                    args.insert("driver".into(), Value::String(slot.driver.clone()));
                    args.insert("id".into(), Value::String(slot.id.clone()));
                    if let Value::Object(props_map) = &slot.props {
                        for (k, v) in props_map {
                            args.insert(k.clone(), v.clone());
                        }
                    }
                    self.execute(&serde_json::json!({
                        "execute": "device_add",
                        "arguments": args
                    }))?;
                }
            } else if slot.populated {
                let _ = self.device_del(&slot.id);
            }
        }

        Ok(())
    }
}

fn parse_cpus_fast(v: &Value) -> Result<Vec<(u32, i32)>, HvError> {
    let bad = || HvError::Api(format!("unexpected query-cpus-fast reply: {}", v));
    v.as_array()
        .ok_or_else(bad)?
        .iter()
        .map(|c| {
            let index = c.get("cpu-index").and_then(|i| i.as_u64());
            let thread = c.get("thread-id").and_then(|t| t.as_i64());
            match (index, thread) {
                (Some(i), Some(t)) => Ok((
                    u32::try_from(i).map_err(|_| bad())?,
                    i32::try_from(t).map_err(|_| bad())?,
                )),
                _ => Err(bad()),
            }
        })
        .collect()
}

fn parse_observed(status: &Value, peripherals: &Value) -> Observed {
    let guest = match status.get("status").and_then(|s| s.as_str()).unwrap_or("") {
        "prelaunch" | "inmigrate" => GuestState::Created,
        "running" | "debug" => GuestState::Running,
        "paused" | "suspended" | "restore-vm" | "save-vm" | "finish-migrate" | "postmigrate"
        | "colo" => GuestState::Paused,
        "shutdown" | "guest-panicked" | "internal-error" | "io-error" | "watchdog" => {
            GuestState::Shutdown
        }
        _ if status.get("running").and_then(|r| r.as_bool()) == Some(true) => GuestState::Running,
        _ => GuestState::Paused,
    };
    let device_ids = peripherals
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|p| {
                    p.get("type")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.starts_with("child<"))
                })
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Observed {
        guest,
        device_ids,
        vcpu_count: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn status_and_peripherals() {
        let o = parse_observed(
            &serde_json::json!({"status": "running", "running": true}),
            &serde_json::json!([
                {"name": "type", "type": "string"},
                {"name": "_vfio_0000_41_00_0", "type": "child<vfio-pci>"},
                {"name": "vd0", "type": "child<virtio-blk-pci>"}
            ]),
        );
        assert_eq!(o.guest, GuestState::Running);
        assert_eq!(o.device_ids, ["_vfio_0000_41_00_0", "vd0"]);
        assert_eq!(
            parse_observed(&serde_json::json!({"status": "prelaunch"}), &Value::Null).guest,
            GuestState::Created
        );
        assert_eq!(
            parse_observed(&serde_json::json!({"status": "paused"}), &Value::Null).guest,
            GuestState::Paused
        );
        assert_eq!(
            parse_observed(&serde_json::json!({"status": "shutdown"}), &Value::Null).guest,
            GuestState::Shutdown
        );
    }

    #[test]
    fn cpus_fast() {
        let v = serde_json::json!([
            {"cpu-index": 0, "thread-id": 4101, "qom-path": "/machine/unattached/device[0]", "target": "x86_64"},
            {"cpu-index": 1, "thread-id": 4102, "qom-path": "/machine/unattached/device[1]", "target": "x86_64"}
        ]);
        assert_eq!(parse_cpus_fast(&v).unwrap(), [(0, 4101), (1, 4102)]);
        assert!(parse_cpus_fast(&serde_json::json!([{"cpu-index": 0}])).is_err());
        assert!(parse_cpus_fast(&serde_json::json!({})).is_err());
    }

    #[test]
    fn handshake_then_command_skipping_events() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("qmp.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            w.write_all(b"{\"QMP\": {\"version\": {}}}\r\n").unwrap();
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            assert!(line.contains("qmp_capabilities"));
            w.write_all(b"{\"return\": {}}\r\n").unwrap();
            line.clear();
            r.read_line(&mut line).unwrap();
            w.write_all(b"{\"event\": \"RESUME\"}\r\n{\"return\": {\"status\": \"running\"}}\r\n")
                .unwrap();
            line
        });
        let r = QmpClient::new(sock.to_str().unwrap())
            .execute(&serde_json::json!({"execute": "query-status"}))
            .unwrap();
        assert_eq!(r["status"], "running");
        assert!(server.join().unwrap().contains("query-status"));
    }

    #[test]
    fn missing_socket_is_a_connect_error() {
        assert!(matches!(
            QmpClient::new("/nonexistent/qmp.sock").ping(),
            Err(HvError::Connect(_))
        ));
    }
}
