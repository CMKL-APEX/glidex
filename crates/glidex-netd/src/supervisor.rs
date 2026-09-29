//! Supervision of per-NAT dnsmasq processes.

use glidex_ovs::OvsError;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub trait Supervisor: Send + Sync {
    /// (Re)start the dnsmasq for `bridge` with `args`.
    fn start(&self, bridge: &str, args: Vec<String>) -> Result<(), OvsError>;
    fn stop(&self, bridge: &str);
    /// Re-read the hosts file (SIGHUP).
    fn reload(&self, bridge: &str);
    fn running(&self, bridge: &str) -> bool;
}

struct Proc {
    child: Child,
    args: Vec<String>,
    restarts: u32,
    last_start: Instant,
}

/// Spawns `dnsmasq` children and restarts them if they exit.
pub struct DnsmasqSupervisor {
    procs: Arc<Mutex<HashMap<String, Proc>>>,
}

fn spawn(args: &[String]) -> Result<Child, OvsError> {
    Command::new("dnsmasq")
        .args(args)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| OvsError::Io(format!("dnsmasq: {}", e)))
}

impl DnsmasqSupervisor {
    pub fn new() -> Self {
        let procs: Arc<Mutex<HashMap<String, Proc>>> = Arc::default();
        let watched = procs.clone();
        std::thread::Builder::new()
            .name("dnsmasq-watch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_secs(2));
                let mut procs = watched.lock().unwrap();
                for (bridge, p) in procs.iter_mut() {
                    if let Ok(Some(status)) = p.child.try_wait() {
                        // Exponential back-off (2, 4, 8 … 60 s) while it keeps
                        // failing; a run that lasted a minute resets it.
                        let backoff = Duration::from_secs((1u64 << p.restarts.min(6)).min(60));
                        if p.last_start.elapsed() < backoff {
                            continue;
                        }
                        if p.last_start.elapsed() > Duration::from_secs(60) + backoff {
                            p.restarts = 0;
                        }
                        tracing::warn!(bridge, %status, "dnsmasq exited; restarting");
                        match spawn(&p.args) {
                            Ok(child) => {
                                p.child = child;
                                p.restarts += 1;
                                p.last_start = Instant::now();
                            }
                            Err(e) => tracing::error!(bridge, error = %e, "dnsmasq restart failed"),
                        }
                    }
                }
            })
            .expect("spawn dnsmasq watcher");
        Self { procs }
    }
}

impl Default for DnsmasqSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor for DnsmasqSupervisor {
    fn start(&self, bridge: &str, args: Vec<String>) -> Result<(), OvsError> {
        self.stop(bridge);
        let child = spawn(&args)?;
        self.procs.lock().unwrap().insert(
            bridge.to_string(),
            Proc {
                child,
                args,
                restarts: 0,
                last_start: Instant::now(),
            },
        );
        Ok(())
    }

    fn stop(&self, bridge: &str) {
        if let Some(mut p) = self.procs.lock().unwrap().remove(bridge) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p.child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = p.child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = p.child.kill();
            let _ = p.child.wait();
        }
    }

    fn reload(&self, bridge: &str) {
        if let Some(p) = self.procs.lock().unwrap().get(bridge) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(p.child.id() as i32),
                nix::sys::signal::Signal::SIGHUP,
            );
        }
    }

    fn running(&self, bridge: &str) -> bool {
        self.procs
            .lock()
            .unwrap()
            .get_mut(bridge)
            .is_some_and(|p| matches!(p.child.try_wait(), Ok(None)))
    }
}

/// Test double.
#[derive(Default)]
pub struct FakeSupervisor {
    pub events: Mutex<Vec<String>>,
    running: Mutex<std::collections::HashSet<String>>,
}

impl Supervisor for FakeSupervisor {
    fn start(&self, bridge: &str, args: Vec<String>) -> Result<(), OvsError> {
        self.events.lock().unwrap().push(format!("start {} {}", bridge, args.join(" ")));
        self.running.lock().unwrap().insert(bridge.to_string());
        Ok(())
    }
    fn stop(&self, bridge: &str) {
        self.events.lock().unwrap().push(format!("stop {}", bridge));
        self.running.lock().unwrap().remove(bridge);
    }
    fn reload(&self, bridge: &str) {
        self.events.lock().unwrap().push(format!("reload {}", bridge));
    }
    fn running(&self, bridge: &str) -> bool {
        self.running.lock().unwrap().contains(bridge)
    }
}
