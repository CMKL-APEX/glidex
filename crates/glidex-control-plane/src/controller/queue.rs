//! The controllers' work queue (spec/reconciliation.md §9.3): one queue
//! for every kind, keyed by `(kind, id)`, deduplicating. A key is queued at
//! most once and processed by at most one worker at a time; a key added
//! while it is being processed is processed again afterwards. Failures
//! back off per key (1 s doubling to 300 s); an explicit "requeue after"
//! is not a failure.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Vm(String),
    Disk(String),
    Image(String),
    Network(String),
}

impl Key {
    pub fn kind(&self) -> &'static str {
        match self {
            Key::Vm(_) => "vm",
            Key::Disk(_) => "disk",
            Key::Image(_) => "image",
            Key::Network(_) => "network",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Key::Vm(s) | Key::Disk(s) | Key::Image(s) | Key::Network(s) => s,
        }
    }
}

pub const BACKOFF_MIN: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);

#[derive(Default)]
struct State {
    ready: VecDeque<Key>,
    queued: HashSet<Key>,
    active: HashSet<Key>,
    dirty: HashSet<Key>,
    delayed: HashMap<Key, Instant>,
    failures: HashMap<Key, u32>,
}

#[derive(Default)]
pub struct WorkQueue {
    st: Mutex<State>,
    notify: Notify,
}

impl WorkQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue `key` now (no-op if it already is).
    pub fn add(&self, key: Key) {
        let mut st = self.st.lock().unwrap();
        st.delayed.remove(&key);
        Self::push(&mut st, key);
        drop(st);
        self.notify.notify_one();
    }

    fn push(st: &mut State, key: Key) {
        if st.active.contains(&key) {
            st.dirty.insert(key);
        } else if st.queued.insert(key.clone()) {
            st.ready.push_back(key);
        }
    }

    /// Queue `key` after `after`, unless it is due earlier already.
    pub fn add_after(&self, key: Key, after: Duration) {
        if after.is_zero() {
            return self.add(key);
        }
        let at = Instant::now() + after;
        let mut st = self.st.lock().unwrap();
        if st.queued.contains(&key) {
            return;
        }
        let e = st.delayed.entry(key).or_insert(at);
        if at < *e {
            *e = at;
        }
        drop(st);
        self.notify.notify_one();
    }

    /// The next key to process; marks it active until [`Self::done`].
    pub async fn next(&self) -> Key {
        loop {
            let wait = {
                let mut st = self.st.lock().unwrap();
                let now = Instant::now();
                let due: Vec<Key> = st.delayed.iter().filter(|(_, t)| **t <= now).map(|(k, _)| k.clone()).collect();
                for k in due {
                    st.delayed.remove(&k);
                    Self::push(&mut st, k);
                }
                if let Some(k) = st.ready.pop_front() {
                    st.queued.remove(&k);
                    st.active.insert(k.clone());
                    return k;
                }
                st.delayed.values().min().map(|t| t.saturating_duration_since(now))
            };
            match wait {
                Some(d) => {
                    let _ = tokio::time::timeout(d.max(Duration::from_millis(5)), self.notify.notified()).await;
                }
                None => self.notify.notified().await,
            }
        }
    }

    /// Finished processing `key`: requeue it if it was added meanwhile.
    pub fn done(&self, key: &Key) {
        let mut st = self.st.lock().unwrap();
        st.active.remove(key);
        if st.dirty.remove(key) {
            Self::push(&mut st, key.clone());
            drop(st);
            self.notify.notify_one();
        }
    }

    /// A failed round: the next delay (1 s doubling to 300 s).
    pub fn failed(&self, key: &Key) -> Duration {
        let mut st = self.st.lock().unwrap();
        let n = st.failures.entry(key.clone()).or_insert(0);
        *n = n.saturating_add(1);
        backoff(*n)
    }

    /// A successful round: reset the key's backoff.
    pub fn succeeded(&self, key: &Key) {
        self.st.lock().unwrap().failures.remove(key);
    }

    /// `(pending, in_flight)` for `GET /system/reconcile`.
    pub fn stats(&self) -> (usize, usize) {
        let st = self.st.lock().unwrap();
        (st.ready.len() + st.delayed.len(), st.active.len())
    }
}

/// 1 s × 2^(n-1), capped at 300 s.
pub fn backoff(n: u32) -> Duration {
    let secs = 1u64.checked_shl(n.saturating_sub(1)).unwrap_or(u64::MAX);
    Duration::from_secs(secs).clamp(BACKOFF_MIN, BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(s: &str) -> Key {
        Key::Vm(s.into())
    }

    #[tokio::test]
    async fn dedupes_and_never_runs_a_key_twice_at_once() {
        let q = WorkQueue::new();
        q.add(vm("a"));
        q.add(vm("a"));
        q.add(vm("b"));
        assert_eq!(q.next().await, vm("a"));
        // Added while active: comes back after done, not before.
        q.add(vm("a"));
        assert_eq!(q.next().await, vm("b"));
        assert!(tokio::time::timeout(Duration::from_millis(50), q.next()).await.is_err());
        q.done(&vm("a"));
        assert_eq!(q.next().await, vm("a"));
        assert_eq!(q.stats(), (0, 2));
    }

    #[tokio::test]
    async fn delayed_keys_come_when_due() {
        let q = WorkQueue::new();
        q.add_after(vm("a"), Duration::from_millis(80));
        q.add_after(vm("a"), Duration::from_millis(30));
        let t = Instant::now();
        assert_eq!(q.next().await, vm("a"));
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(25) && waited < Duration::from_millis(75), "{waited:?}");
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!([1, 2, 3, 9, 10, 64].map(|n| backoff(n).as_secs()), [1, 2, 4, 256, 300, 300]);
        let q = WorkQueue::new();
        assert_eq!(q.failed(&vm("a")), Duration::from_secs(1));
        assert_eq!(q.failed(&vm("a")), Duration::from_secs(2));
        q.succeeded(&vm("a"));
        assert_eq!(q.failed(&vm("a")), Duration::from_secs(1));
    }
}
