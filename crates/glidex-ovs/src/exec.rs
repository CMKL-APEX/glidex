//! Running host commands.
//!
//! Everything glidex-ovs does to the host goes through [`Exec`], so tests
//! can swap in [`RecordingExec`] and assert the exact commands. Programs
//! come from the closed [`Program`] enum and arguments are passed as a
//! vector, never through a shell, so no caller can make netd run an
//! arbitrary program or inject shell syntax.

use crate::OvsError;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The only programs glidex-ovs ever runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Program {
    OvsVsctl,
    OvsAppctl,
    OvsVswitchd,
    OvnNbctl,
    OvnSbctl,
    Ip,
    Nft,
    Iptables,
    Sysctl,
    AptGet,
    Dnf,
    UpdateAlternatives,
    Getcap,
    Systemctl,
    Dnsmasq,
    Ping,
    Ss,
    DpkgQuery,
    // Pinned source build (spec §6.3).
    Curl,
    Sha256sum,
    Tar,
    Meson,
    Ninja,
    Make,
    /// `./configure` in the command's working directory (never on PATH).
    Configure,
}

impl Program {
    pub fn name(self) -> &'static str {
        match self {
            Program::OvsVsctl => "ovs-vsctl",
            Program::OvsAppctl => "ovs-appctl",
            Program::OvsVswitchd => "ovs-vswitchd",
            Program::OvnNbctl => "ovn-nbctl",
            Program::OvnSbctl => "ovn-sbctl",
            Program::Ip => "ip",
            Program::Nft => "nft",
            Program::Iptables => "iptables",
            Program::Sysctl => "sysctl",
            Program::AptGet => "apt-get",
            Program::Dnf => "dnf",
            Program::UpdateAlternatives => "update-alternatives",
            Program::Getcap => "getcap",
            Program::Systemctl => "systemctl",
            Program::Dnsmasq => "dnsmasq",
            Program::Ping => "ping",
            Program::Ss => "ss",
            Program::DpkgQuery => "dpkg-query",
            Program::Curl => "curl",
            Program::Sha256sum => "sha256sum",
            Program::Tar => "tar",
            Program::Meson => "meson",
            Program::Ninja => "ninja",
            Program::Make => "make",
            Program::Configure => "./configure",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: Program,
    pub args: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub cwd: Option<PathBuf>,
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
            stdin: None,
            env: Vec::new(),
            timeout: Duration::from_secs(30),
            cwd: None,
        }
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
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
    fn run(&self, cmd: &Cmd) -> Result<Output, OvsError>;
    fn read_file(&self, path: &Path) -> Result<String, OvsError>;
    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), OvsError>;
    fn remove_file(&self, path: &Path) -> Result<(), OvsError>;
    /// Create a directory (and parents) and set its mode.
    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), OvsError>;
    fn exists(&self, path: &Path) -> bool;

    /// Use OVS tools from `dir` (a source-built prefix) from now on.
    fn set_ovs_bin_dir(&self, _dir: Option<PathBuf>) {}

    /// Run and require exit status 0.
    fn check(&self, cmd: &Cmd) -> Result<Output, OvsError> {
        let out = self.run(cmd)?;
        if out.status == 0 {
            Ok(out)
        } else {
            Err(OvsError::command_failed(cmd, &out))
        }
    }
}

/// Runs real commands. `ovs_bin_dir` lets a source-built OVS prefix take
/// precedence over `PATH` for OVS tools. With `sudo`, every command runs
/// through `sudo` (the installer, which runs as the user); file writes are
/// then refused, since nothing in that path needs them.
pub struct SystemExec {
    ovs_bin_dir: Mutex<Option<PathBuf>>,
    sudo: bool,
}

impl SystemExec {
    pub fn new() -> Self {
        Self {
            ovs_bin_dir: Mutex::new(None),
            sudo: false,
        }
    }

    pub fn with_ovs_bin_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            ovs_bin_dir: Mutex::new(Some(dir.into())),
            sudo: false,
        }
    }

    /// Run commands via `sudo`. Call `sudo -v` first so a password prompt
    /// doesn't count against a command's timeout.
    pub fn sudo() -> Self {
        Self {
            ovs_bin_dir: Mutex::new(None),
            sudo: true,
        }
    }

    fn resolve(&self, cmd: &Cmd) -> Result<PathBuf, OvsError> {
        let program = cmd.program;
        if program == Program::Configure {
            // Absolute, so it can only ever be the build tree's script.
            return cmd
                .cwd
                .as_ref()
                .map(|d| d.join("configure"))
                .ok_or_else(|| OvsError::Io("./configure needs a working directory".into()));
        }
        if matches!(program, Program::OvsVsctl | Program::OvsAppctl | Program::OvsVswitchd) {
            if let Some(dir) = self.ovs_bin_dir.lock().unwrap().as_ref() {
                // ovs-vswitchd lives in sbin/ of a source prefix.
                let sbin = dir.parent().map(|p| p.join("sbin").join(program.name()));
                return Ok(match (program, sbin) {
                    (Program::OvsVswitchd, Some(p)) => p,
                    _ => dir.join(program.name()),
                });
            }
        }
        Ok(PathBuf::from(program.name()))
    }
}

impl Default for SystemExec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec for SystemExec {
    fn run(&self, cmd: &Cmd) -> Result<Output, OvsError> {
        let program = self.resolve(cmd)?;
        let mut command = if self.sudo {
            // sudo resets the environment; pass variables through `env`.
            let mut c = Command::new("sudo");
            if !cmd.env.is_empty() {
                c.arg("env");
                c.args(cmd.env.iter().map(|(k, v)| format!("{}={}", k, v)));
            }
            c.arg(&program);
            c
        } else {
            let mut c = Command::new(&program);
            c.envs(cmd.env.iter().map(|(k, v)| (k, v)));
            c
        };
        if let Some(dir) = &cmd.cwd {
            command.current_dir(dir);
        }
        let mut child = command
            .args(&cmd.args)
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| OvsError::Io(format!("{}: {}", cmd.program.name(), e)))?;

        if let Some(data) = &cmd.stdin {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin
                .write_all(data)
                .map_err(|e| OvsError::Io(format!("{} stdin: {}", cmd.program.name(), e)))?;
        }

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
                    return Err(OvsError::Io(format!(
                        "{} timed out after {:?}",
                        cmd.display(),
                        cmd.timeout
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(e) => return Err(OvsError::Io(format!("{}: {}", cmd.program.name(), e))),
            }
        };

        Ok(Output {
            status: status.code().unwrap_or(-1),
            stdout: out_thread.join().unwrap_or_default(),
            stderr: err_thread.join().unwrap_or_default(),
        })
    }

    fn read_file(&self, path: &Path) -> Result<String, OvsError> {
        std::fs::read_to_string(path).map_err(|e| OvsError::Io(format!("{}: {}", path.display(), e)))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), OvsError> {
        if self.sudo {
            return Err(OvsError::Io(format!("refusing to write {} via sudo", path.display())));
        }
        std::fs::write(path, data).map_err(|e| OvsError::Io(format!("{}: {}", path.display(), e)))
    }

    fn remove_file(&self, path: &Path) -> Result<(), OvsError> {
        if self.sudo {
            return Err(OvsError::Io(format!("refusing to remove {} via sudo", path.display())));
        }
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(OvsError::Io(format!("{}: {}", path.display(), e))),
        }
    }

    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), OvsError> {
        use std::os::unix::fs::PermissionsExt;
        if self.sudo {
            return Err(OvsError::Io(format!("refusing to create {} via sudo", path.display())));
        }
        let err = |e: std::io::Error| OvsError::Io(format!("{}: {}", path.display(), e));
        std::fs::create_dir_all(path).map_err(err)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(err)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn set_ovs_bin_dir(&self, dir: Option<PathBuf>) {
        *self.ovs_bin_dir.lock().unwrap() = dir;
    }
}

/// Test double: returns scripted outputs and records every call.
///
/// Responses are matched by the longest registered prefix of the command
/// line (`"ovs-vsctl br-exists gxbr"`); unmatched commands succeed with
/// empty output, so tests only script what they care about.
#[derive(Default)]
pub struct RecordingExec {
    responses: Mutex<Vec<(String, VecDeque<Output>)>>,
    files: Mutex<Vec<(PathBuf, String)>>,
    calls: Mutex<Vec<String>>,
    writes: Mutex<Vec<(PathBuf, Vec<u8>)>>,
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

    /// Replace any queued outputs for `prefix` with this one.
    pub fn set(&self, prefix: &str, output: Output) -> &Self {
        let mut responses = self.responses.lock().unwrap();
        responses.retain(|(p, _)| p != prefix);
        responses.push((prefix.to_string(), VecDeque::from([output])));
        self
    }

    pub fn file(&self, path: &str, contents: &str) -> &Self {
        self.files
            .lock()
            .unwrap()
            .push((PathBuf::from(path), contents.to_string()));
        self
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn writes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        self.writes.lock().unwrap().clone()
    }

    pub fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
}

impl Exec for RecordingExec {
    fn run(&self, cmd: &Cmd) -> Result<Output, OvsError> {
        let line = cmd.display();
        let mut recorded = line.clone();
        if let Some(dir) = &cmd.cwd {
            recorded.push_str(&format!(" (in {})", dir.display()));
        }
        if let Some(stdin) = &cmd.stdin {
            recorded.push_str(" <<< ");
            recorded.push_str(&String::from_utf8_lossy(stdin));
        }
        self.calls.lock().unwrap().push(recorded);

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

    fn read_file(&self, path: &Path) -> Result<String, OvsError> {
        self.files
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
            .ok_or_else(|| OvsError::Io(format!("{}: not found", path.display())))
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), OvsError> {
        self.writes
            .lock()
            .unwrap()
            .push((path.to_path_buf(), data.to_vec()));
        self.files
            .lock()
            .unwrap()
            .push((path.to_path_buf(), String::from_utf8_lossy(data).into_owned()));
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> Result<(), OvsError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("rm {}", path.display()));
        self.files.lock().unwrap().retain(|(p, _)| p != path);
        Ok(())
    }

    fn create_dir(&self, path: &Path, mode: u32) -> Result<(), OvsError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("mkdir -m {:o} {}", mode, path.display()));
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().iter().any(|(p, _)| p == path)
    }
}
