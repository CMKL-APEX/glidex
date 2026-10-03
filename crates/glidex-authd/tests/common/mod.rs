//! Shared test harness: an in-process authd on a temp socket with fake PAM
//! and a fake account database.

#![allow(dead_code)]

use glidex_authd::authenticator::{Account, Accounts, AuthFailure, Authenticator};
use glidex_authd::client::AuthdClient;
use glidex_authd::config::Config;
use glidex_authd::server::{self, Authd};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

pub const PASSWORD: &str = "correct horse battery staple";

#[derive(Default)]
pub struct FakePam {
    pub passwords: HashMap<String, String>,
    pub calls: AtomicUsize,
}

impl Authenticator for FakePam {
    fn authenticate(&self, service: &str, user: &str, password: &str) -> Result<(), AuthFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(service, "glidex");
        match self.passwords.get(user) {
            Some(p) if p == password => Ok(()),
            _ => Err(AuthFailure::Denied("bad password".into())),
        }
    }
}

/// Every user has one key: `ssh-ed25519 AAAA…<name>`.
pub struct FakeKeys;

impl glidex_authd::keys::KeyReader for FakeKeys {
    fn public_keys(&self, user: &str) -> Result<Vec<String>, String> {
        Ok(vec![format!("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoq {}@host", user)])
    }
}

pub struct FakeAccounts(pub HashMap<String, Account>);

impl Accounts for FakeAccounts {
    fn lookup(&self, user: &str) -> Option<Account> {
        self.0.get(user).cloned()
    }
}

pub struct Harness {
    pub dir: TempDir,
    pub socket: PathBuf,
    pub pam: Arc<FakePam>,
}

impl Harness {
    pub fn client(&self) -> AuthdClient {
        AuthdClient::new(&self.socket).with_timeout(Duration::from_secs(5))
    }

    pub fn pam_calls(&self) -> usize {
        self.pam.calls.load(Ordering::SeqCst)
    }
}

pub fn my_uid() -> u32 {
    nix::unistd::getuid().as_raw()
}

pub fn test_config() -> Config {
    Config {
        failure_delay: Duration::from_millis(2),
        ..Config::default()
    }
}

/// Users: `alice` (allowed, uid 1000), `mallory` (exists, wrong group),
/// and `user0`..`user9` (allowed). All have password [`PASSWORD`].
pub fn start_with(config: Config, service_uid: Option<u32>) -> Harness {
    let mut accounts = HashMap::new();
    let mut passwords = HashMap::new();
    let mut add = |name: &str, uid: u32, groups: &[&str]| {
        accounts.insert(
            name.to_string(),
            Account {
                uid,
                groups: groups.iter().map(|g| g.to_string()).collect(),
            },
        );
        passwords.insert(name.to_string(), PASSWORD.to_string());
    };
    add("alice", 1000, &["alice", "glidex-users", "staff"]);
    add("mallory", 1001, &["mallory", "staff"]);
    for i in 0..10 {
        add(&format!("user{i}"), 2000 + i, &["glidex-users"]);
    }
    let pam = Arc::new(FakePam {
        passwords,
        calls: AtomicUsize::new(0),
    });
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("auth.sock");
    let listener = server::bind(&socket, 0o660, None).unwrap();
    let authd = Arc::new(
        Authd::new(config, service_uid, pam.clone(), Arc::new(FakeAccounts(accounts))).with_key_reader(Arc::new(FakeKeys)),
    );
    std::thread::spawn(move || authd.serve(listener));
    Harness { dir, socket, pam }
}

pub fn start() -> Harness {
    start_with(test_config(), Some(my_uid()))
}
