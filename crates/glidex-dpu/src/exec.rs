//! Running commands on the DPU (or, for netd, on the host).
//!
//! Everything glidex-dpu does goes through [`Exec`], so tests can swap in
//! [`RecordingExec`] and assert the exact commands. Programs come from the
//! closed [`Program`] enum, the agent's allow-list (spec/31-dpu.md §10.2),
//! and arguments are a vector, never a shell.

use crate::DpuError;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The only programs glidex-dpu ever runs. No OVN central program is here:
/// NB and SB work happens on the cluster's servers (§10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Program {
    OvsVsctl,
    OvsAppctl,
    /// The virtio-net controller's CLI.
    Virtnet,
    Ip,
    Devlink,
    /// Query only: [`SystemExec`] refuses anything else (decision 9).
    Mlxconfig,
    Systemctl,
    /// `--version` only, for the version rule (§8.3).
    OvnController,
    /// `connection-status` of the DPU's `ovn-controller`.
    OvnAppctl,
}

impl Program {
    pub fn name(self) -> &'static str {
        match self {
            Program::OvsVsctl => "ovs-vsctl",
            Program::OvsAppctl => "ovs-appctl",
            Program::Virtnet => "virtnet",
            Program::Ip => "ip",
            Program::Devlink => "devlink",
            Program::Mlxconfig => "mlxconfig",
            Program::Systemctl => "systemctl",
            Program::OvnController => "ovn-controller",
            Program::OvnAppctl => "ovn-appctl",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: Program,
    pub args: Vec<String>,
    pub timeout: Duration,
}

impl Cmd {
    pub fn new<I, S>(program: Program, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program,
            args: args.into_iter().map(Into::into).collect(),
            timeout: Duration::from_secs(30),
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `program arg1 arg2 …` for logs and test assertions.
    pub fn display(&self) -> String {
        std::iter::once(self.program.name().to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    pub fn failed(status: i32, stderr: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            stdout: Vec::new(),
            stderr: stderr.into(),
        }
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

pub trait Exec: Send + Sync {
    /// Run a command. A non-zero exit is returned as `Ok(Output)`; use
    /// [`Exec::check`] to turn it into an error.
    fn run(&self, cmd: &Cmd) -> Result<Output, DpuError>;
    fn read_file(&self, path: &Path) -> Result<String, DpuError>;
    /// Sysfs and procfs writes (hugepages, `sriov_numvfs`, driver binding).
    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), DpuError>;
    fn exists(&self, path: &Path) -> bool;
    /// The names in a directory; empty when it can't be read.
    fn list_dir(&self, _path: &Path) -> Vec<String> {
        Vec::new()
    }
    /// The file name a symlink points at (`driver` → `vfio-pci`).
    fn link_name(&self, _path: &Path) -> Option<String> {
        None
    }

    /// Run and require exit status 0.
    fn check(&self, cmd: &Cmd) -> Result<Output, DpuError> {
        let out = self.run(cmd)?;
        if out.status == 0 {
            Ok(out)
        } else {
            Err(DpuError::command_failed(cmd, &out))
        }
    }
}

/// Runs real commands.
#[derive(Default)]
pub struct SystemExec;

impl SystemExec {
    pub fn new() -> Self {
        Self
    }
}

/// `mlxconfig` changes firmware, which glidex never does (decision 9): only
/// a query (`q`/`query`) with no `set`/`reset`/`-y` gets through.
fn mlxconfig_is_query(args: &[String]) -> bool {
    let has_query = args.iter().any(|a| a == "q" || a == "query");
    let has_write = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "set" | "s" | "reset" | "r" | "-y" | "--yes" | "set_raw" | "apply"
        )
    });
    has_query && !has_write
}

impl Exec for SystemExec {
    fn run(&self, cmd: &Cmd) -> Result<Output, DpuError> {
        if cmd.program == Program::Mlxconfig && !mlxconfig_is_query(&cmd.args) {
            return Err(DpuError::invalid(format!(
                "refusing to run `{}`: mlxconfig is query-only",
                cmd.display()
            )));
        }
        let mut child = Command::new(cmd.program.name())
            .args(&cmd.args)
            .env_remove("NOTIFY_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| DpuError::Io(format!("{}: {}", cmd.program.name(), e)))?;

        // Drain output on threads so a chatty child can't block on a full pipe.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let deadline = Instant::now() + cmd.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DpuError::Io(format!(
                        "{} timed out after {:?}",
                        cmd.display(),
                        cmd.timeout
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(DpuError::Io(format!("{}: {}", cmd.program.name(), e))),
            }
        };

        Ok(Output {
            status: status.code().unwrap_or(-1),
            stdout: out_thread.join().unwrap_or_default(),
            stderr: err_thread.join().unwrap_or_default(),
        })
    }

    fn read_file(&self, path: &Path) -> Result<String, DpuError> {
        std::fs::read_to_string(path)
            .map_err(|e| DpuError::Io(format!("{}: {}", path.display(), e)))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), DpuError> {
        // Sysfs attributes want a single write(2) of the whole value.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| DpuError::Io(format!("{}: {}", path.display(), e)))?;
        f.write_all(data)
            .map_err(|e| DpuError::Io(format!("{}: {}", path.display(), e)))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list_dir(&self, path: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn link_name(&self, path: &Path) -> Option<String> {
        std::fs::read_link(path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    }
}

/// Test double: returns scripted outputs and records every call.
///
/// Responses are matched by the longest registered prefix of the command
/// line (`"virtnet list"`); unmatched commands succeed with empty output,
/// so tests only script what they care about.
#[derive(Default)]
pub struct RecordingExec {
    responses: Mutex<Vec<(String, VecDeque<Output>)>>,
    files: Mutex<Vec<(PathBuf, String)>>,
    links: Mutex<Vec<(PathBuf, String)>>,
    calls: Mutex<Vec<String>>,
}

impl RecordingExec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue an output for commands starting with `prefix`. Several
    /// outputs for one prefix are returned in order; the last one repeats.
    pub fn on(&self, prefix: &str, output: Output) -> &Self {
        let mut responses = self.responses.lock().unwrap();
        match responses.iter_mut().find(|(p, _)| p == prefix) {
            Some((_, queue)) => queue.push_back(output),
            None => responses.push((prefix.to_string(), VecDeque::from([output]))),
        }
        self
    }

    pub fn file(&self, path: &str, contents: &str) -> &Self {
        self.files
            .lock()
            .unwrap()
            .push((PathBuf::from(path), contents.to_string()));
        self
    }

    /// A symlink at `path` whose target's file name is `name`.
    pub fn link(&self, path: &str, name: &str) -> &Self {
        self.links
            .lock()
            .unwrap()
            .push((PathBuf::from(path), name.to_string()));
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

impl Exec for RecordingExec {
    fn run(&self, cmd: &Cmd) -> Result<Output, DpuError> {
        let line = cmd.display();
        self.calls.lock().unwrap().push(line.clone());
        let mut responses = self.responses.lock().unwrap();
        let best = responses
            .iter_mut()
            .filter(|(prefix, _)| line.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len());
        Ok(match best {
            Some((_, queue)) if queue.len() > 1 => queue.pop_front().unwrap(),
            Some((_, queue)) => queue.front().cloned().unwrap_or_default(),
            None => Output::default(),
        })
    }

    fn read_file(&self, path: &Path) -> Result<String, DpuError> {
        self.files
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| DpuError::Io(format!("{}: not found", path.display())))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), DpuError> {
        let data = String::from_utf8_lossy(data).into_owned();
        self.calls
            .lock()
            .unwrap()
            .push(format!("write {} {}", path.display(), data.trim_end()));
        self.files.lock().unwrap().push((path.to_path_buf(), data));
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().iter().any(|(p, _)| p == path)
            || self.links.lock().unwrap().iter().any(|(p, _)| p == path)
    }

    /// The direct children of `path` among the scripted files and links.
    fn list_dir(&self, path: &Path) -> Vec<String> {
        let files = self.files.lock().unwrap();
        let links = self.links.lock().unwrap();
        let mut names: Vec<String> = files
            .iter()
            .map(|(p, _)| p)
            .chain(links.iter().map(|(p, _)| p))
            .filter_map(|p| p.strip_prefix(path).ok())
            .filter_map(|rest| rest.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn link_name(&self, path: &Path) -> Option<String> {
        self.links
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(p, _)| p == path)
            .map(|(_, n)| n.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn mlxconfig_runs_only_as_a_query() {
        assert!(mlxconfig_is_query(&args(
            "-d /dev/mst/mt41692_pciconf0 q SRIOV_EN"
        )));
        assert!(!mlxconfig_is_query(&args(
            "-d /dev/mst/mt41692_pciconf0 -y set SRIOV_EN=1"
        )));
        assert!(!mlxconfig_is_query(&args("-d x reset")));
        assert!(!mlxconfig_is_query(&args("-d x q -y")));
        let err = SystemExec::new()
            .run(&Cmd::new(Program::Mlxconfig, args("-d x set SRIOV_EN=1")))
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument");
    }
}
