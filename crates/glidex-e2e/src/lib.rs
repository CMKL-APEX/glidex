//! Helpers for end-to-end tests that drive real glidex installations.
//!
//! The tests run on a host that already runs glidex (the "outer" one). They
//! make VMs with it, install a second glidex into those VMs (the "inner" one),
//! and exercise the inner installation over SSH. Everything goes through the
//! outer API socket and `ssh`/`curl`, so the tests need no privileges beyond
//! being a glidex user on the outer host.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Run `cmd` and return its output; panics only if it can't be started.
pub fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap_or_else(|e| panic!("running {cmd:?}: {e}"))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Poll `f` until it returns `Some`, or panic after `timeout` with `what`.
pub fn wait_for<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < timeout, "timed out after {:?} waiting for {what}", timeout);
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// As [`wait_for`], with `detail()` in the panic message.
pub fn wait_for_or<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>, detail: impl FnOnce() -> String) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        if start.elapsed() >= timeout {
            panic!("timed out after {timeout:?} waiting for {what}; {}", detail());
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// A glidex REST API reached with `curl` over its Unix socket: on this host,
/// or on a remote host through SSH.
pub enum Api<'a> {
    Local(PathBuf),
    Remote(&'a Ssh),
}

pub const API_SOCKET: &str = "/run/glidex-cp/api.sock";

impl Api<'_> {
    /// `(status, body)`; the body is `Null` when it isn't JSON.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let socket = match self {
            Api::Local(p) => p.display().to_string(),
            Api::Remote(_) => API_SOCKET.to_string(),
        };
        let mut args: Vec<String> = vec!["-s".into(), "-X".into(), method.into(), "--unix-socket".into(), socket, "-w".into(), "\n%{http_code}".into()];
        if body.is_some() {
            args.extend(["-H".into(), "content-type: application/json".into(), "--data-binary".into(), "@-".into()]);
        }
        args.push(format!("http://glidex{path}"));
        let input = body.map(|b| b.to_string()).unwrap_or_default();
        let out = match self {
            Api::Local(_) => {
                let mut child = Command::new("curl").args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("curl");
                child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
                child.wait_with_output().unwrap()
            }
            Api::Remote(ssh) => {
                let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
                ssh.run_with_input(&format!("curl {}", quoted.join(" ")), input.as_bytes())
            }
        };
        let s = text(&out.stdout);
        let (body, code) = s.trim_end().rsplit_once('\n').unwrap_or(("", s.trim()));
        (code.trim().parse().unwrap_or(0), serde_json::from_str(body).unwrap_or(Value::Null))
    }

    /// A call that must succeed (2xx); returns the body.
    pub fn ok(&self, method: &str, path: &str, body: Option<&Value>) -> Value {
        let (code, v) = self.call(method, path, body);
        assert!((200..300).contains(&code), "{method} {path} answered {code}: {v}");
        v
    }
}

/// Quote `s` for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// SSH to a guest as `user` with a private key made for the test.
pub struct Ssh {
    pub host: String,
    pub user: String,
    pub key: PathBuf,
}

impl Ssh {
    fn command(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(["-i", &self.key.display().to_string()])
            .args(["-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=5", "-o", "BatchMode=yes"])
            .arg(format!("{}@{}", self.user, self.host));
        c
    }

    pub fn run(&self, cmd: &str) -> Output {
        run(self.command().arg(cmd))
    }

    pub fn run_with_input(&self, cmd: &str, input: &[u8]) -> Output {
        let mut child = self.command().arg(cmd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("ssh");
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    /// Run `cmd`, which must succeed; returns its standard output.
    pub fn check(&self, cmd: &str) -> String {
        let out = self.run(cmd);
        assert!(out.status.success(), "on {}: `{cmd}` failed ({}):\n{}{}", self.host, out.status, text(&out.stdout), text(&out.stderr));
        text(&out.stdout)
    }

    pub fn reachable(&self) -> bool {
        self.run("true").status.success()
    }

    /// Stream `tar -c` of `paths` (relative to `dir`) into `dest` on the guest.
    pub fn send_tree(&self, dir: &Path, paths: &[&str], excludes: &[&str], dest: &str) {
        let mut tar = Command::new("tar");
        tar.arg("-cz").arg("-C").arg(dir);
        for e in excludes {
            tar.arg(format!("--exclude={e}"));
        }
        tar.args(paths).stdout(Stdio::piped());
        let mut tar = tar.spawn().expect("tar");
        let mut ssh = self.command().arg(format!("mkdir -p {dest} && tar -xz -C {dest}")).stdin(tar.stdout.take().unwrap()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("ssh");
        assert!(tar.wait().unwrap().success(), "tar of {paths:?}");
        let out = ssh.wait().unwrap();
        assert!(out.success(), "copying {paths:?} to {}", self.host);
    }
}

/// A fresh SSH key pair in `dir`; returns (private key path, public key).
pub fn ssh_keypair(dir: &Path) -> (PathBuf, String) {
    let key = dir.join("id_ed25519");
    let out = run(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-C", "glidex-e2e", "-f"]).arg(&key));
    assert!(out.status.success(), "ssh-keygen: {}", text(&out.stderr));
    let public = std::fs::read_to_string(key.with_extension("pub")).unwrap().trim().to_string();
    (key, public)
}

/// The workspace root, as the installer binary built from it knows it.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root")
}

/// The NoCloud seed (`cidata`) for a guest named `name` that pings `peer`
/// until it answers and writes `GXE2E-PING-OK` or `GXE2E-PING-FAIL` to its
/// consoles, as a shell script that writes the image to `out` (it needs
/// `mkfs.vfat` and `mcopy`, which glidex installs).
pub fn ping_seed_script(name: &str, peer: &str, out: &str) -> String {
    let report = |what: &str| format!(r#"for t in /dev/ttyS0 /dev/hvc0 /dev/console; do echo "GXE2E-PING-{what} from {name} to {peer} after $i tries; my address: $(hostname -I)" > $t 2>/dev/null; done"#);
    let loop_ = format!(
        "for i in $(seq 300); do if ping -c1 -W1 {peer} >/dev/null 2>&1; then {}; exit 0; fi; sleep 1; done; {}",
        report("OK"),
        report("FAIL")
    );
    format!(
        r#"set -e
D=$(mktemp -d)
printf 'instance-id: {name}\nlocal-hostname: {name}\n' > $D/meta-data
printf 'version: 2\nethernets:\n  nics:\n    match: {{name: "e*"}}\n    dhcp4: true\n' > $D/network-config
cat > $D/user-data <<'USERDATA'
#cloud-config
runcmd:
  - [sh, -c, {}]
USERDATA
rm -f {out}
mkfs.vfat -C -n cidata {out} 2048 >/dev/null 2>&1
mcopy -i {out} $D/meta-data $D/user-data $D/network-config ::
chmod 0644 {out}
rm -rf $D
"#,
        yaml_single_quote(&loop_)
    )
}

fn yaml_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seed_script_writes_a_cidata_image_with_the_ping_loop() {
        let dir = std::env::temp_dir().join(format!("gxe2e-seed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("seed.img");
        let script = ping_seed_script("a", "10.89.77.3", out.to_str().unwrap());
        assert!(script.contains("ping -c1 -W1 10.89.77.3"));
        // Only where the tools are, as on a glidex host.
        let have = |c: &str| Command::new("sh").arg("-c").arg(format!("command -v {c}")).output().map(|o| o.status.success()).unwrap_or(false);
        if have("mkfs.vfat") && have("mcopy") {
            let st = run(Command::new("sh").arg("-c").arg(&script));
            assert!(st.status.success(), "{}", text(&st.stderr));
            let list = run(Command::new("mtype").args(["-i", out.to_str().unwrap(), "::user-data"]));
            let ud = text(&list.stdout);
            assert!(ud.starts_with("#cloud-config") && ud.contains("GXE2E-PING-OK"), "{ud}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_quoting_survives_single_quotes() {
        let out = run(Command::new("sh").arg("-c").arg(format!("printf %s {}", shell_quote("it's \"ok\" $HOME"))));
        assert_eq!(text(&out.stdout), "it's \"ok\" $HOME");
    }
}
