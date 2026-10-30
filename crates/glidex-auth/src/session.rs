//! Browser sessions (glidex spec/11-security.md §5.5): a 256-bit random id of
//! which only the SHA-256 is stored, a CSRF value for cookie-authenticated
//! writes, and an idle and an absolute timeout. The application stores
//! the session record; these are the rules it applies to one.

use crate::secrets::{random_token, sha256_hex};

/// Random bytes in a session id.
pub const ID_BYTES: usize = 32;
/// Random bytes in a session's CSRF value.
pub const CSRF_BYTES: usize = 24;
/// `last_seen` (and a token's `last_used_at`) is written back at most this
/// often, so a busy client doesn't write on every request.
pub const TOUCH_SECS: u64 = 60;
/// `age_secs` for principals that can never satisfy step-up.
pub const NEVER_FRESH: i64 = i64::MAX / 4;

/// How long a session lives: idle, and at most in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRules {
    pub idle_secs: u64,
    pub absolute_secs: u64,
}

impl SessionRules {
    pub fn new(idle_minutes: u64, absolute_hours: u64) -> Self {
        SessionRules {
            idle_secs: idle_minutes * 60,
            absolute_secs: absolute_hours * 3600,
        }
    }

    /// Whether a session created at `created_at` and last seen at
    /// `last_seen` is still good at `now`: the boundary seconds count.
    pub fn live(&self, created_at: u64, last_seen: u64, now: u64) -> bool {
        now <= last_seen + self.idle_secs && now <= created_at + self.absolute_secs
    }

    pub fn expired(&self, created_at: u64, last_seen: u64, now: u64) -> bool {
        !self.live(created_at, last_seen, now)
    }
}

/// A new session's secrets: the cookie value, what is stored in its
/// place, and its CSRF value.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The cookie value: given to the browser, never stored.
    pub id: String,
    /// SHA-256 of `id`: the session's key in the store.
    pub hash: String,
    pub csrf: String,
}

pub fn new_session() -> NewSession {
    let id = random_token(ID_BYTES);
    let csrf = random_token(CSRF_BYTES);
    NewSession {
        hash: session_hash(&id),
        id,
        csrf,
    }
}

/// The store key of session cookie `id`.
pub fn session_hash(id: &str) -> String {
    sha256_hex(id.as_bytes())
}

/// Whether a use at `now` should be written back over `last`
/// ([`TOUCH_SECS`]); `None` (never used) always is.
pub fn touch_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|at| now >= at + TOUCH_SECS)
}

/// Seconds since a login at `authenticated_at`, for step-up checks
/// (`context.auth.age_secs`).
pub fn age_secs(authenticated_at: u64, now: u64) -> i64 {
    now.saturating_sub(authenticated_at) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_live_through_their_boundaries() {
        let r = SessionRules::new(30, 12);
        assert_eq!((r.idle_secs, r.absolute_secs), (1800, 43200));
        // Idle: the last second counts, the next doesn't.
        assert!(r.live(1000, 1000, 2800));
        assert!(r.expired(1000, 1000, 2801));
        // Absolute: however recent the use.
        assert!(r.live(1000, 44000, 44200));
        assert!(r.expired(1000, 44200, 44201));
    }

    #[test]
    fn new_sessions_store_only_the_hash() {
        let s = new_session();
        assert_eq!(s.hash, sha256_hex(s.id.as_bytes()));
        assert_eq!(s.hash, session_hash(&s.id));
        assert!(s.id.len() >= 40 && s.csrf.len() >= 30, "{s:?}");
        assert!(
            s.id.bytes()
                .chain(s.csrf.bytes())
                .all(|b| b.is_ascii_alphanumeric())
        );
        assert_ne!(new_session().id, s.id);
    }

    #[test]
    fn touches_are_throttled() {
        assert!(touch_due(None, 5));
        assert!(!touch_due(Some(100), 159));
        assert!(touch_due(Some(100), 160));
    }

    #[test]
    fn age_never_goes_negative() {
        assert_eq!(age_secs(100, 160), 60);
        assert_eq!(age_secs(200, 100), 0);
        const { assert!(NEVER_FRESH > 365 * 86400) };
    }
}
