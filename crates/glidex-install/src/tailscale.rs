//! The web UI's Tailscale certificate (spec/tailscale-tls.md §4.6).
//!
//! Opt-in. The choice is the flag, else the saved one, else a prompt
//! (interactive runs on a node that reports certificate domains), else off.
//! Turning it on grants `glidex-ui` `TS_PERMIT_CERT_UID` in a `tailscaled`
//! drop-in, unless `tailscaled` already has it (nothing to do) or has it set
//! to another user: then nothing changes and the UI stays off for this run.
//! The UI's own drop-in sets `GLIDEX_UI_TLS_TAILSCALE=on`. Turning it off
//! removes only these two drop-ins.

use crate::{argv, run_capture, run_capture_sudo, sudo, write_if_changed};
use anyhow::Result;
use colored::Colorize;
use std::path::Path;

pub const UI_DROPIN: &str = "/etc/systemd/system/glidex-ui.service.d/50-tailscale-cert.conf";
pub const TAILSCALED_DROPIN: &str = "/etc/systemd/system/tailscaled.service.d/50-glidex-cert.conf";
const TAILSCALED: &str = "tailscaled.service";
const PERMIT: &str = "TS_PERMIT_CERT_UID";

pub fn ui_dropin() -> String {
    format!(
        "# {} (--tailscale-cert): serve the node's Tailscale certificate\n\
         # to its ts.net names (spec/tailscale-tls.md).\n\
         [Service]\nEnvironment=GLIDEX_UI_TLS_TAILSCALE=on\n",
        crate::PAM_MARKER
    )
}

pub fn tailscaled_dropin() -> String {
    format!(
        "# {} (--tailscale-cert): lets glidex-ui fetch this node's\n\
         # certificate (spec/tailscale-tls.md). TS_PERMIT_CERT_UID holds one user.\n\
         [Service]\nEnvironment={}={}\n",
        crate::PAM_MARKER,
        PERMIT,
        crate::UI_USER
    )
}

/// The setting for this run, and what to save: the flag or saved choice
/// (`choice`), else the operator's answer when one can be asked, else off
/// and nothing saved, so a later interactive run still asks.
pub fn decide(
    choice: Option<bool>,
    can_ask: bool,
    ask: impl FnOnce() -> bool,
) -> (bool, Option<bool>) {
    match choice {
        Some(v) => (v, Some(v)),
        None if can_ask => {
            let v = ask();
            (v, Some(v))
        }
        None => (false, None),
    }
}

/// What to do about `TS_PERMIT_CERT_UID`, given its configured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permit {
    /// Already `glidex-ui` (by our drop-in or by hand).
    Ours,
    /// Unset: write the drop-in.
    Grant,
    /// Another user holds it; it takes one, so leave it alone.
    Refuse(String),
}

pub fn permit_action(configured: Option<&str>, ui_user: &str, ui_uid: Option<u32>) -> Permit {
    match configured.map(str::trim).filter(|v| !v.is_empty()) {
        None => Permit::Grant,
        Some(v) if v == ui_user || (ui_uid.is_some() && v.parse::<u32>().ok() == ui_uid) => {
            Permit::Ours
        }
        Some(v) => Permit::Refuse(v.to_string()),
    }
}

/// `KEY=VALUE` lines of an environment file (`#` comments, optional
/// `export `, quotes stripped).
fn env_file_assignments(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), unquote(v.trim())))
        })
        .collect()
}

/// `systemctl show -p Environment --value`: space-separated assignments.
fn systemd_assignments(value: &str) -> Vec<(String, String)> {
    value
        .split_whitespace()
        .filter_map(|a| {
            let (k, v) = a.split_once('=')?;
            Some((unquote(k), unquote(v)))
        })
        .collect()
}

fn unquote(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// `TS_PERMIT_CERT_UID` as `tailscaled`'s unit sets it: `Environment=`,
/// overridden by the `EnvironmentFile=`s, later files winning (as systemd
/// applies them).
pub fn configured_permit(environment: &str, env_files: &[String]) -> Option<String> {
    let mut v = systemd_assignments(environment)
        .into_iter()
        .filter(|(k, _)| k == PERMIT)
        .map(|(_, v)| v)
        .next_back();
    for text in env_files {
        if let Some(f) = env_file_assignments(text)
            .into_iter()
            .filter(|(k, _)| k == PERMIT)
            .map(|(_, v)| v)
            .next_back()
        {
            v = Some(f);
        }
    }
    v
}

/// The paths in `systemctl show -p EnvironmentFiles --value`
/// (`/etc/default/tailscaled (ignore_errors=no)`, one or more).
fn env_file_paths(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .filter(|t| t.starts_with('/'))
        .map(String::from)
        .collect()
}

/// The node as `tailscale status --json` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    pub running: bool,
    /// Certificate domains, wildcards left out.
    pub domains: Vec<String>,
}

pub fn parse_status(json: &str) -> Option<Node> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    Some(Node {
        running: v["BackendState"] == "Running",
        domains: v["CertDomains"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str())
                    .filter(|d| !d.starts_with("*."))
                    .map(|d| d.trim_end_matches('.').to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// `None` without `tailscale` or a reachable `tailscaled`.
pub fn node() -> Option<Node> {
    parse_status(&run_capture("tailscale", &["status", "--json"]).ok()?)
}

fn show(prop: &str) -> String {
    run_capture("systemctl", &["show", TAILSCALED, "-p", prop, "--value"]).unwrap_or_default()
}

fn host_configured_permit() -> Option<String> {
    let files: Vec<String> = env_file_paths(&show("EnvironmentFiles"))
        .iter()
        .filter_map(|p| {
            std::fs::read_to_string(p)
                .ok()
                .or_else(|| run_capture_sudo(&["cat", p]))
        })
        .collect();
    configured_permit(&show("Environment"), &files)
}

/// Whether the running `tailscaled` has the permission (its environment
/// is root's to read).
fn running_has_permit() -> bool {
    let pid = show("MainPID");
    if pid.trim().is_empty() || pid.trim() == "0" {
        return false;
    }
    run_capture_sudo(&["cat", &format!("/proc/{}/environ", pid.trim())]).is_some_and(|env| {
        env.split('\0')
            .any(|kv| kv == format!("{PERMIT}={}", crate::UI_USER))
    })
}

fn ui_uid() -> Option<u32> {
    run_capture("id", &["-u", crate::UI_USER])
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn ours(path: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.contains(crate::PAM_MARKER))
}

/// Remove `path` if it is ours, and its directory if that leaves it empty.
fn remove_ours(path: &str) -> Result<bool> {
    if !ours(path) {
        return Ok(false);
    }
    sudo(&argv(&["rm", "-f", path]))?;
    if let Some(dir) = Path::new(path).parent() {
        let _ = sudo(&argv(&[
            "rmdir",
            "--ignore-fail-on-non-empty",
            &dir.to_string_lossy(),
        ]));
    }
    Ok(true)
}

/// What `apply` changed, for `install_services` to reload and restart.
#[derive(Debug, Default)]
pub struct Applied {
    /// The feature is on for this run.
    pub on: bool,
    pub ui_dropin_changed: bool,
    /// A unit drop-in was written or removed: `daemon-reload`.
    pub reload: bool,
    /// `tailscaled` needs a restart (after `daemon-reload`).
    pub restart_tailscaled: bool,
    /// Asked for but not enabled this run, and why: another user holds
    /// the permission, or there is no `tailscaled`.
    pub refused: Option<String>,
}

/// Bring the drop-ins in line with `want`. Writes files only; the caller
/// reloads systemd and restarts.
pub fn apply(want: bool, node: Option<&Node>) -> Result<Applied> {
    let mut a = Applied::default();
    if want && node.is_none() {
        println!(
            "{} tailscaled is not installed or not reachable (`tailscale status` fails). \
             The web UI keeps its usual certificate; re-run once Tailscale is set up.",
            "Tailscale certificate not enabled:".yellow()
        );
        a.refused = Some("no tailscaled".into());
    } else if want {
        match permit_action(
            host_configured_permit().as_deref(),
            crate::UI_USER,
            ui_uid(),
        ) {
            Permit::Refuse(holder) => {
                println!(
                    "{} tailscaled already gives TS_PERMIT_CERT_UID to '{}', and it holds only one user. \
                     Left unchanged; the web UI keeps its usual certificate. Resolve it, then re-run.",
                    "Tailscale certificate not enabled:".yellow(),
                    holder
                );
                a.refused = Some(format!("TS_PERMIT_CERT_UID is '{holder}'"));
            }
            Permit::Grant => {
                a.reload = write_if_changed(TAILSCALED_DROPIN, &tailscaled_dropin())?;
                a.restart_tailscaled = a.reload || !running_has_permit();
            }
            Permit::Ours => a.restart_tailscaled = !running_has_permit(),
        }
    }
    if want && a.refused.is_none() {
        a.on = true;
        a.ui_dropin_changed = write_if_changed(UI_DROPIN, &ui_dropin())?;
    } else {
        a.ui_dropin_changed = remove_ours(UI_DROPIN)?;
        // Only ours; one set by hand stays. Refused, ours wasn't in force
        // (another assignment overrides it), so no restart for it.
        let removed = remove_ours(TAILSCALED_DROPIN)?;
        a.reload |= removed;
        a.restart_tailscaled |= removed && a.refused.is_none();
    }
    a.reload |= a.ui_dropin_changed;
    Ok(a)
}

/// Restart `tailscaled` and wait (up to 30 s) until it is running again.
pub fn restart_tailscaled() -> Result<()> {
    println!(
        "{} tailscaled (the tailnet connection drops for a few seconds)",
        "Restarting:".green()
    );
    sudo(&argv(&["systemctl", "restart", TAILSCALED]))?;
    for _ in 0..30 {
        if node().is_some_and(|n| n.running) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    println!(
        "{} tailscaled is not running yet; the web UI keeps retrying",
        "Note:".yellow()
    );
    Ok(())
}

/// Whether the UI on `addr` serves `domain` a certificate the system
/// trusts, as a browser would see it; waits up to `wait`.
pub fn served_trusted(addr: &str, domain: &str, wait: std::time::Duration) -> bool {
    use glidex_tls::tokio_rustls::rustls;
    let Ok(client) = glidex_tls::ClientTls::new(&[], &[b"http/1.1"]) else {
        return false;
    };
    let deadline = std::time::Instant::now() + wait;
    loop {
        let ok = (|| {
            let name = rustls::pki_types::ServerName::try_from(domain.to_string()).ok()?;
            let mut conn = rustls::ClientConnection::new(client.config.clone(), name).ok()?;
            let mut tcp = std::net::TcpStream::connect(addr).ok()?;
            tcp.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok()?;
            while conn.is_handshaking() {
                conn.complete_io(&mut tcp).ok()?;
            }
            Some(())
        })()
        .is_some();
        if ok || std::time::Instant::now() >= deadline {
            return ok;
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_flag_then_saved_then_prompt_then_off() {
        let never = || -> bool { panic!("must not ask") };
        assert_eq!(decide(Some(true), true, never), (true, Some(true)));
        assert_eq!(decide(Some(false), true, never), (false, Some(false)));
        assert_eq!(decide(None, true, || true), (true, Some(true)));
        assert_eq!(decide(None, true, || false), (false, Some(false)));
        // Non-interactive (or no certificate domains) with nothing saved:
        // off, and nothing saved so a later run still asks.
        assert_eq!(decide(None, false, never), (false, None));
    }

    #[test]
    fn permit_unset_ours_or_another_users() {
        assert_eq!(permit_action(None, "glidex-ui", Some(995)), Permit::Grant);
        assert_eq!(
            permit_action(Some(" "), "glidex-ui", Some(995)),
            Permit::Grant
        );
        assert_eq!(
            permit_action(Some("glidex-ui"), "glidex-ui", Some(995)),
            Permit::Ours
        );
        assert_eq!(
            permit_action(Some("995"), "glidex-ui", Some(995)),
            Permit::Ours
        );
        assert_eq!(
            permit_action(Some("caddy"), "glidex-ui", Some(995)),
            Permit::Refuse("caddy".into())
        );
        assert_eq!(
            permit_action(Some("33"), "glidex-ui", None),
            Permit::Refuse("33".into())
        );
    }

    /// As systemd applies them: Environment=, then each EnvironmentFile=,
    /// a later assignment winning.
    #[test]
    fn configured_permit_from_unit_and_files() {
        assert_eq!(configured_permit("", &[]), None);
        let ubuntu = "# Set the port\nPORT=\"41641\"\n\nFLAGS=\"\"\n".to_string();
        assert_eq!(configured_permit("", std::slice::from_ref(&ubuntu)), None);
        assert_eq!(
            configured_permit(
                "TS_PERMIT_CERT_UID=glidex-ui",
                std::slice::from_ref(&ubuntu)
            ),
            Some("glidex-ui".into())
        );
        assert_eq!(
            configured_permit(
                "A=1 TS_PERMIT_CERT_UID=glidex-ui",
                &["export TS_PERMIT_CERT_UID='caddy'\n".into()]
            ),
            Some("caddy".into())
        );
        assert_eq!(
            configured_permit(
                "",
                &[
                    "TS_PERMIT_CERT_UID=a\n".into(),
                    "TS_PERMIT_CERT_UID=b\n".into()
                ]
            ),
            Some("b".into())
        );
        assert_eq!(
            env_file_paths("/etc/default/tailscaled (ignore_errors=no) /etc/x (ignore_errors=yes)"),
            ["/etc/default/tailscaled", "/etc/x"]
        );
    }

    #[test]
    fn status_domains() {
        let n = parse_status(
            r#"{"BackendState":"Running","CertDomains":["Node.Tail.ts.net.","*.node.tail.ts.net"]}"#,
        )
        .unwrap();
        assert_eq!(
            n,
            Node {
                running: true,
                domains: vec!["node.tail.ts.net".into()]
            }
        );
        let off = parse_status(r#"{"BackendState":"Stopped","CertDomains":null}"#).unwrap();
        assert!(!off.running && off.domains.is_empty());
        assert!(parse_status("not json").is_none());
    }

    #[test]
    fn dropins_are_marked_ours() {
        for d in [ui_dropin(), tailscaled_dropin()] {
            assert!(d.contains(crate::PAM_MARKER), "{d}");
            assert!(d.contains("[Service]\n"), "{d}");
        }
        assert!(ui_dropin().contains("Environment=GLIDEX_UI_TLS_TAILSCALE=on\n"));
        assert!(tailscaled_dropin().contains("Environment=TS_PERMIT_CERT_UID=glidex-ui\n"));
    }
}
