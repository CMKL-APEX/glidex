//! In-memory failure rate limits (spec/security.md §5.3): a per-user and a
//! global sliding window of failure times.
//!
//! Memory is bounded: a failure is only recorded while the global window is
//! not full, so the per-user map holds at most
//! `global.max * user.window / global.window` timestamps.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub max: usize,
    pub window: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub per_user: Window,
    pub global: Window,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_user: Window {
                max: 5,
                window: Duration::from_secs(15 * 60),
            },
            global: Window {
                max: 30,
                window: Duration::from_secs(60),
            },
        }
    }
}

#[derive(Debug)]
pub struct RateLimiter {
    limits: Limits,
    users: HashMap<String, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

fn prune(q: &mut VecDeque<Instant>, window: Duration, now: Instant) {
    while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= window) {
        q.pop_front();
    }
}

impl RateLimiter {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            users: HashMap::new(),
            global: VecDeque::new(),
        }
    }

    /// Whether `user` may try now.
    pub fn allowed(&mut self, user: &str, now: Instant) -> bool {
        prune(&mut self.global, self.limits.global.window, now);
        if self.global.len() >= self.limits.global.max {
            return false;
        }
        match self.users.get_mut(user) {
            Some(q) => {
                prune(q, self.limits.per_user.window, now);
                if q.is_empty() {
                    self.users.remove(user);
                    true
                } else {
                    q.len() < self.limits.per_user.max
                }
            }
            None => true,
        }
    }

    pub fn record_failure(&mut self, user: Option<&str>, now: Instant) {
        prune(&mut self.global, self.limits.global.window, now);
        if self.global.len() >= self.limits.global.max {
            return;
        }
        self.global.push_back(now);
        if let Some(user) = user {
            let window = self.limits.per_user.window;
            self.users.retain(|_, q| {
                prune(q, window, now);
                !q.is_empty()
            });
            self.users.entry(user.to_string()).or_default().push_back(now);
        }
    }

    /// A successful login clears the user's failures.
    pub fn record_success(&mut self, user: &str) {
        self.users.remove(user);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_window_slides() {
        let mut l = RateLimiter::new(Limits::default());
        let t0 = Instant::now();
        for i in 0..5 {
            assert!(l.allowed("alice", t0 + Duration::from_secs(i)));
            l.record_failure(Some("alice"), t0 + Duration::from_secs(i));
        }
        assert!(!l.allowed("alice", t0 + Duration::from_secs(10)));
        assert!(l.allowed("bob", t0 + Duration::from_secs(10)));
        // The first failure leaves the 15-minute window.
        assert!(l.allowed("alice", t0 + Duration::from_secs(15 * 60)));
    }

    #[test]
    fn success_resets_the_user() {
        let mut l = RateLimiter::new(Limits::default());
        let t0 = Instant::now();
        for _ in 0..4 {
            l.record_failure(Some("alice"), t0);
        }
        l.record_success("alice");
        for _ in 0..4 {
            l.record_failure(Some("alice"), t0);
        }
        assert!(l.allowed("alice", t0));
    }

    #[test]
    fn global_window_slides() {
        let mut l = RateLimiter::new(Limits::default());
        let t0 = Instant::now();
        for i in 0..30 {
            l.record_failure(Some(&format!("u{i}")), t0);
        }
        // Failures past the global cap are not stored.
        l.record_failure(Some("x"), t0);
        assert_eq!(l.users.len(), 30);
        assert!(!l.allowed("fresh", t0 + Duration::from_secs(59)));
        assert!(l.allowed("fresh", t0 + Duration::from_secs(60)));
    }
}
