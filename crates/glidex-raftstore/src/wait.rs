//! Waiting from synchronous code that may run on a Tokio worker.
//!
//! A store write can hold a lock across its Raft round trip, which needs
//! the runtime's I/O. A worker that blocks outright on such a lock can
//! leave no thread to drive that I/O, and then neither ever finishes: the
//! control planes on prism2 and prism1 hung this way on 2026-10-31. Wait
//! through these instead: a free lock is taken at once, and a held one is
//! waited for in `block_in_place`, which moves the worker's other tasks
//! (and the I/O) to another thread.

use std::sync::{LockResult, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::sync::{PoisonError, TryLockError};

/// Run `f`, which may wait a long time, without holding up a Tokio
/// worker: in `block_in_place` on a multi-thread runtime's worker, directly
/// anywhere else (a plain thread, `spawn_blocking`, a current-thread runtime).
pub fn blocking<R>(f: impl FnOnce() -> R) -> R {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

fn taken<G>(
    tried: Result<G, TryLockError<G>>,
    wait: impl FnOnce() -> LockResult<G>,
) -> LockResult<G> {
    match tried {
        Ok(g) => Ok(g),
        Err(TryLockError::Poisoned(p)) => Err(PoisonError::new(p.into_inner())),
        Err(TryLockError::WouldBlock) => blocking(wait),
    }
}

/// `m.lock()`, waiting off the worker when it is held.
pub fn lock<T>(m: &Mutex<T>) -> LockResult<MutexGuard<'_, T>> {
    taken(m.try_lock(), || m.lock())
}

/// `l.read()`, waiting off the worker when a writer holds or waits for it.
pub fn read<T>(l: &RwLock<T>) -> LockResult<RwLockReadGuard<'_, T>> {
    taken(l.try_read(), || l.read())
}

/// `l.write()`, waiting off the worker when it is held.
pub fn write<T>(l: &RwLock<T>) -> LockResult<RwLockWriteGuard<'_, T>> {
    taken(l.try_write(), || l.write())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    /// Two tasks on a one-worker runtime, each holding the lock while it
    /// waits on the runtime's timer, as a store write waits on Raft. Taken
    /// with plain `lock()`, the second task blocks the only worker, the
    /// timer never fires, and neither finishes.
    #[test]
    fn a_held_lock_leaves_the_runtime_running() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let m = Arc::new(Mutex::new(0));
        let l = Arc::new(RwLock::new(0));
        let (done, rx) = std::sync::mpsc::channel();
        for i in 0..2 {
            let (m, l, done, h) = (m.clone(), l.clone(), done.clone(), rt.handle().clone());
            rt.spawn(async move {
                let nap = || tokio::time::sleep(Duration::from_millis(50));
                {
                    let mut g = lock(&m).unwrap();
                    crate::raft::wait_on(&h, nap());
                    *g += 1;
                }
                {
                    let mut g = write(&l).unwrap();
                    crate::raft::wait_on(&h, nap());
                    *g += 1;
                }
                let seen = *read(&l).unwrap();
                let _ = done.send((i, seen));
            });
        }
        let results: Vec<_> = (0..2)
            .map(|_| rx.recv_timeout(Duration::from_secs(10)))
            .collect();
        // Dropping a hung runtime would wait on its blocked threads.
        rt.shutdown_background();
        for r in results {
            assert!(r.is_ok(), "a task hung");
        }
        assert_eq!((*m.lock().unwrap(), *l.read().unwrap()), (2, 2));
    }
}
