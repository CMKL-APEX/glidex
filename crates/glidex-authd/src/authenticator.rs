//! The two host lookups glidex-authd depends on, behind traits so tests can
//! replace them: password verification (PAM) and the account database
//! (NSS: passwd and group).

use nix::unistd::{getgrouplist, Group, User};
use std::ffi::CString;

/// Why a password check failed. The detail is for the daemon's log only;
/// callers always get the same `denied`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthFailure {
    /// Wrong password, expired or locked account, ... (counted as a failure).
    #[error("denied: {0}")]
    Denied(String),
    /// PAM itself is broken (not counted against the user).
    #[error("internal: {0}")]
    Internal(String),
}

pub trait Authenticator: Send + Sync {
    /// Verify `password` for `user` with PAM service `service`, including
    /// account checks (`pam_acct_mgmt`).
    fn authenticate(&self, service: &str, user: &str, password: &str) -> Result<(), AuthFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    /// Names of every group the user is in, primary group first.
    pub groups: Vec<String>,
}

pub trait Accounts: Send + Sync {
    /// `None` if the user does not exist.
    fn lookup(&self, user: &str) -> Option<Account>;
}

/// The host's account database through NSS (`getpwnam`, `getgrouplist`).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemAccounts;

impl Accounts for SystemAccounts {
    fn lookup(&self, user: &str) -> Option<Account> {
        let u = User::from_name(user).ok().flatten()?;
        let name = CString::new(u.name.as_str()).ok()?;
        let gids = getgrouplist(&name, u.gid).ok()?;
        let mut groups: Vec<String> = Vec::with_capacity(gids.len());
        for gid in std::iter::once(u.gid).chain(gids) {
            if let Ok(Some(g)) = Group::from_gid(gid) {
                if !groups.contains(&g.name) {
                    groups.push(g.name);
                }
            }
        }
        Some(Account {
            uid: u.uid.as_raw(),
            groups,
        })
    }
}

/// Resolve a user name to its uid.
pub fn user_uid(name: &str) -> Option<u32> {
    User::from_name(name).ok().flatten().map(|u| u.uid.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_lookup_of_root() {
        let a = SystemAccounts.lookup("root").expect("root exists");
        assert_eq!(a.uid, 0);
        assert_eq!(a.groups.first().map(String::as_str), Some("root"));
        assert!(SystemAccounts.lookup("glidex-no-such-user-xyz").is_none());
        assert_eq!(user_uid("root"), Some(0));
    }
}
