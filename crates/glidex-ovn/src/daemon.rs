//! The leader's `ovn-nbctl` daemon (spec/14-clustering.md §11.1).
//!
//! Without it, each command is a new `ovn-nbctl`: a TLS connection to a
//! server picked at random (and another when that one isn't the leader),
//! and a fresh download of the tables the command needs. A reconcile pass
//! runs a few dozen. The daemon keeps one connection to the leader and an
//! in-memory replica; commands reach it through its control socket
//! (`OVN_NB_DAEMON`).
//!
//! It is used only while the database answers: the first pass, and every
//! pass after one that failed, runs its commands directly, and the daemon
//! starts once a pass succeeds. A daemon whose database is gone doesn't
//! fail a command, it waits for the database to come back, so a pass
//! through it would hang until the command's timeout.

use crate::{Nb, NbConn};
use glidex_ovs::OvsError;
use glidex_ovs::exec::{Cmd, Exec, Program};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One `ovn-nbctl --detach`, started after a pass succeeded and again when
/// it died or what it connects with changed.
pub struct NbDaemon {
    rundir: PathBuf,
    running: Option<Running>,
    /// The last pass succeeded: the database answers.
    answered: bool,
    /// The last start failure, so a failure that repeats each pass is
    /// reported once.
    failed: Option<String>,
    warning: Option<String>,
}

struct Running {
    started_for: Identity,
    ctl: PathBuf,
}

/// What a daemon was started with: its servers and certificate files, and
/// the files' modification times, so a renewed certificate or a rotated CA
/// gets a new daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    conn: NbConn,
    modified: [Option<SystemTime>; 3],
}

impl Identity {
    fn of(conn: &NbConn) -> Self {
        let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        Identity {
            conn: NbConn {
                daemon: None,
                ..conn.clone()
            },
            modified: [m(&conn.key), m(&conn.cert), m(&conn.ca)],
        }
    }
}

impl NbDaemon {
    /// A daemon whose control socket and pidfile go in `rundir`.
    pub fn new(rundir: impl Into<PathBuf>) -> Self {
        NbDaemon {
            rundir: rundir.into(),
            running: None,
            answered: false,
            failed: None,
            warning: None,
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The connection for a pass: `conn` itself until a pass succeeded
    /// ([`NbDaemon::passed`]), then through the daemon, started (again) if
    /// there is none, its control socket is gone, or `conn` or its
    /// certificate files changed. When it can't start, `conn` itself, and
    /// [`NbDaemon::take_warning`] says why.
    pub fn conn(&mut self, exec: &dyn Exec, conn: NbConn) -> NbConn {
        if !self.answered {
            return conn;
        }
        let id = Identity::of(&conn);
        if let Some(r) = &self.running
            && r.started_for == id
            && r.ctl.exists()
        {
            return NbConn {
                daemon: Some(r.ctl.clone()),
                ..conn
            };
        }
        self.stop(exec);
        match self.start(exec, &conn) {
            Ok(ctl) => {
                if self.failed.take().is_some() {
                    self.warning = Some("ovn-nbctl daemon started".into());
                }
                self.running = Some(Running {
                    started_for: id,
                    ctl: ctl.clone(),
                });
                NbConn {
                    daemon: Some(ctl),
                    ..conn
                }
            }
            Err(e) => {
                let e = e.to_string();
                if self.failed.as_deref() != Some(e.as_str()) {
                    self.warning = Some(format!(
                        "starting the ovn-nbctl daemon: {e}; running commands directly"
                    ));
                    self.failed = Some(e);
                }
                conn
            }
        }
    }

    fn start(&self, exec: &dyn Exec, conn: &NbConn) -> Result<PathBuf, OvsError> {
        std::fs::create_dir_all(&self.rundir)
            .map_err(|e| OvsError::Io(format!("creating {}: {e}", self.rundir.display())))?;
        Nb::start_daemon(exec, conn, &self.rundir)
    }

    /// How the pass went. After a failure the daemon stops and commands run
    /// directly until a pass succeeds again.
    pub fn passed(&mut self, exec: &dyn Exec, ok: bool) {
        self.answered = ok;
        if !ok {
            self.stop(exec);
        }
    }

    /// Stop the daemon, if one runs (this server stopped leading, or OVN
    /// was turned off).
    pub fn stop(&mut self, exec: &dyn Exec) {
        if let Some(r) = self.running.take() {
            let exit = Cmd::new(
                Program::OvsAppctl,
                ["-t".to_string(), r.ctl.display().to_string(), "exit".into()],
            );
            // A daemon that died left its socket; nothing answers there.
            if exec.check(&exit).is_err() {
                let _ = std::fs::remove_file(&r.ctl);
            }
        }
    }

    /// A start failure not reported yet, or the recovery after one.
    pub fn take_warning(&mut self) -> Option<String> {
        self.warning.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glidex_ovs::exec::{Output, RecordingExec};
    use std::fs::File;
    use std::time::Duration;

    struct Fixture {
        dir: tempfile::TempDir,
        exec: RecordingExec,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            for f in ["key", "cert", "ca"] {
                File::create(dir.path().join(f)).unwrap();
            }
            Fixture {
                dir,
                exec: RecordingExec::new(),
            }
        }
        fn conn(&self, db: &[&str]) -> NbConn {
            NbConn {
                db: db.iter().map(|s| s.to_string()).collect(),
                key: self.dir.path().join("key"),
                cert: self.dir.path().join("cert"),
                ca: self.dir.path().join("ca"),
                daemon: None,
            }
        }
        fn rundir(&self) -> PathBuf {
            self.dir.path().join("run")
        }
        /// The next start prints `name` as its socket, which then exists.
        fn will_start(&self, name: &str) -> PathBuf {
            let ctl = self.dir.path().join(name);
            File::create(&ctl).unwrap();
            self.exec.set(
                "ovn-nbctl --detach",
                Output::ok(format!("{}\n", ctl.display())),
            );
            ctl
        }
        fn starts(&self) -> usize {
            self.exec
                .calls()
                .iter()
                .filter(|c| c.starts_with("ovn-nbctl --detach"))
                .count()
        }
        fn exits(&self) -> Vec<String> {
            self.exec
                .calls()
                .into_iter()
                .filter(|c| c.starts_with("ovs-appctl"))
                .collect()
        }
    }

    const DB: &[&str] = &["ssl:192.0.2.11:6641", "ssl:192.0.2.12:6641"];

    #[test]
    fn the_daemon_is_used_only_while_the_database_answers() {
        let f = Fixture::new();
        let ctl = f.will_start("nbctl.1.ctl");
        let mut d = NbDaemon::new(f.rundir());
        // The first pass runs directly: a daemon would wait for a database
        // that isn't there.
        assert_eq!(d.conn(&f.exec, f.conn(DB)), f.conn(DB));
        d.passed(&f.exec, false);
        assert_eq!(d.conn(&f.exec, f.conn(DB)), f.conn(DB));
        assert_eq!(f.starts(), 0);

        d.passed(&f.exec, true);
        assert_eq!(d.conn(&f.exec, f.conn(DB)).daemon, Some(ctl.clone()));

        // A failed pass stops it; the next runs directly.
        d.passed(&f.exec, false);
        assert!(!d.is_running());
        assert_eq!(f.exits(), [format!("ovs-appctl -t {} exit", ctl.display())]);
        assert_eq!(d.conn(&f.exec, f.conn(DB)), f.conn(DB));
        assert_eq!(f.starts(), 1);
    }

    #[test]
    fn commands_go_through_one_daemon_while_it_runs() {
        let f = Fixture::new();
        let ctl = f.will_start("nbctl.1.ctl");
        let mut d = NbDaemon::new(f.rundir());
        d.passed(&f.exec, true);
        for _ in 0..3 {
            let c = d.conn(&f.exec, f.conn(DB));
            assert_eq!(c.daemon.as_deref(), Some(ctl.as_path()));
            assert_eq!(c.db, f.conn(DB).db);
        }
        assert_eq!(f.starts(), 1);
        assert!(f.rundir().is_dir());
        let start = &f.exec.calls()[0];
        let pidfile = format!("--pidfile={}", f.rundir().join("ovn-nbctl.pid").display());
        for arg in [
            pidfile.as_str(),
            "--overwrite-pidfile",
            "--db=ssl:192.0.2.11:6641,ssl:192.0.2.12:6641",
        ] {
            assert!(start.contains(arg), "{start}");
        }
        assert_eq!(d.take_warning(), None);
    }

    #[test]
    fn a_dead_daemon_or_a_changed_connection_gets_a_new_one() {
        let f = Fixture::new();
        let first = f.will_start("nbctl.1.ctl");
        let mut d = NbDaemon::new(f.rundir());
        d.passed(&f.exec, true);
        d.conn(&f.exec, f.conn(DB));

        // Its socket gone: it died.
        std::fs::remove_file(&first).unwrap();
        let second = f.will_start("nbctl.2.ctl");
        assert_eq!(d.conn(&f.exec, f.conn(DB)).daemon, Some(second.clone()));
        assert_eq!(
            f.exits(),
            [format!("ovs-appctl -t {} exit", first.display())]
        );

        // A server joined.
        let third = f.will_start("nbctl.3.ctl");
        let more = [DB[0], DB[1], "ssl:192.0.2.13:6641"];
        assert_eq!(d.conn(&f.exec, f.conn(&more)).daemon, Some(third.clone()));

        // The certificate was renewed.
        let fourth = f.will_start("nbctl.4.ctl");
        let later = SystemTime::now() + Duration::from_secs(60);
        File::options()
            .write(true)
            .open(f.dir.path().join("cert"))
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(d.conn(&f.exec, f.conn(&more)).daemon, Some(fourth));
        assert_eq!(f.starts(), 4);
        assert_eq!(f.exits().len(), 3);
    }

    #[test]
    fn without_a_daemon_commands_run_directly_and_the_failure_is_said_once() {
        let f = Fixture::new();
        f.exec.set(
            "ovn-nbctl --detach",
            Output::failed(1, "ovn-nbctl: no such file"),
        );
        let mut d = NbDaemon::new(f.rundir());
        d.passed(&f.exec, true);
        assert_eq!(d.conn(&f.exec, f.conn(DB)), f.conn(DB));
        assert!(
            d.take_warning()
                .is_some_and(|w| w.contains("running commands directly"))
        );
        assert_eq!(d.conn(&f.exec, f.conn(DB)), f.conn(DB));
        assert_eq!(d.take_warning(), None);
        assert!(!d.is_running());

        let ctl = f.will_start("nbctl.1.ctl");
        assert_eq!(d.conn(&f.exec, f.conn(DB)).daemon, Some(ctl));
        assert_eq!(
            d.take_warning().as_deref(),
            Some("ovn-nbctl daemon started")
        );
    }

    #[test]
    fn stopping_asks_the_daemon_to_exit_and_clears_a_dead_ones_socket() {
        let f = Fixture::new();
        let ctl = f.will_start("nbctl.1.ctl");
        let mut d = NbDaemon::new(f.rundir());
        d.passed(&f.exec, true);
        d.conn(&f.exec, f.conn(DB));
        d.stop(&f.exec);
        assert!(!d.is_running());
        assert_eq!(f.exits(), [format!("ovs-appctl -t {} exit", ctl.display())]);
        assert!(ctl.exists(), "a daemon that exits removes its own socket");
        d.stop(&f.exec);
        assert_eq!(f.exits().len(), 1);

        let ctl = f.will_start("nbctl.2.ctl");
        d.conn(&f.exec, f.conn(DB));
        f.exec
            .set("ovs-appctl", Output::failed(1, "connection refused"));
        d.stop(&f.exec);
        assert!(!ctl.exists());
    }
}
