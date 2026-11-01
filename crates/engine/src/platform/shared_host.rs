//! A host connection kept for the whole process.
//!
//! The Linux mic host is a PulseAudio client. Its reactor thread blocks on
//! the server socket and is not woken when the last client handle drops, so
//! every dropped client leaves a thread and a server connection behind. The
//! mic paths therefore share one host for the whole process: every build,
//! probe, identity query and default-input poll uses it, so starting and
//! stopping the mic, rebuilds and failed-build retries never open (and leak)
//! another connection. Only the connection is kept; devices are still
//! re-queried every time.
//!
//! A host whose connection died (the server restarted) is replaced. Its
//! reactor thread has already ended with the connection, so nothing leaks.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// One process-wide host, reconnected only when its connection is dead.
pub(crate) struct SharedHost<H> {
    slot: Mutex<Option<Arc<H>>>,
}

impl<H> SharedHost<H> {
    /// An empty slot: the first [`SharedHost::get`] connects.
    pub(crate) const fn new() -> SharedHost<H> {
        SharedHost {
            slot: Mutex::new(None),
        }
    }

    /// The kept host if `alive` says its connection still answers, otherwise
    /// a new one from `connect`, which is kept in its place. `None` if there
    /// is no live host and `connect` fails; the next call tries again.
    ///
    /// `alive` runs without the lock held, so a slow server delays only its
    /// caller. `connect` runs with it held, so concurrent callers that find
    /// the host dead connect once, not once each.
    pub(crate) fn get(
        &self,
        alive: impl FnOnce(&H) -> bool,
        connect: impl FnOnce() -> Option<H>,
    ) -> Option<Arc<H>> {
        let kept = self.lock().clone();
        if let Some(host) = &kept
            && alive(host)
        {
            return kept;
        }
        let mut slot = self.lock();
        // Another caller may have connected since the check.
        if let Some(current) = slot.as_ref()
            && !kept.as_ref().is_some_and(|dead| Arc::ptr_eq(current, dead))
        {
            return Some(Arc::clone(current));
        }
        // The dead host (if any) is dropped here; its reactor has ended.
        *slot = connect().map(Arc::new);
        slot.clone()
    }

    /// Keep `host` unless a host is already kept, and return the kept one.
    /// For a host that was connected some other way and must not be dropped
    /// while it lives. If another caller kept one first, `host` is dropped:
    /// that needs two connects racing, both on the fallback path.
    pub(crate) fn adopt(&self, host: H) -> Arc<H> {
        let mut slot = self.lock();
        Arc::clone(slot.get_or_insert_with(|| Arc::new(host)))
    }

    fn lock(&self) -> MutexGuard<'_, Option<Arc<H>>> {
        // The slot holds no invariant a panic could break.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A fake host with a switchable connection.
    struct FakeHost {
        id: usize,
        alive: AtomicBool,
    }

    fn connector(count: &AtomicUsize) -> impl FnOnce() -> Option<FakeHost> + '_ {
        move || {
            let id = count.fetch_add(1, Ordering::SeqCst);
            Some(FakeHost {
                id,
                alive: AtomicBool::new(true),
            })
        }
    }

    fn is_alive(host: &FakeHost) -> bool {
        host.alive.load(Ordering::SeqCst)
    }

    #[test]
    fn a_live_host_is_reused() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let first = shared.get(is_alive, connector(&connects));
        let second = shared.get(is_alive, connector(&connects));
        let (Some(first), Some(second)) = (first, second) else {
            panic!("no host");
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_dead_host_is_replaced_once() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let first = shared.get(is_alive, connector(&connects)).expect("host");
        first.alive.store(false, Ordering::SeqCst);
        let second = shared.get(is_alive, connector(&connects)).expect("host");
        assert_eq!(second.id, 1);
        let third = shared.get(is_alive, connector(&connects)).expect("host");
        assert!(Arc::ptr_eq(&second, &third));
        assert_eq!(connects.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_connect_is_retried_next_time() {
        let shared: SharedHost<FakeHost> = SharedHost::new();
        assert!(shared.get(is_alive, || None).is_none());
        let connects = AtomicUsize::new(0);
        assert!(shared.get(is_alive, connector(&connects)).is_some());
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_reconnect_drops_the_dead_host() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let first = shared.get(is_alive, connector(&connects)).expect("host");
        first.alive.store(false, Ordering::SeqCst);
        assert!(shared.get(is_alive, || None).is_none());
        // The dead host is gone: the next call connects without checking it.
        let next = shared
            .get(|_| panic!("nothing to check"), connector(&connects))
            .expect("host");
        assert_eq!(next.id, 1);
    }

    #[test]
    fn an_adopted_host_is_kept_and_reused() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let adopted = shared.adopt(FakeHost {
            id: 7,
            alive: AtomicBool::new(true),
        });
        let next = shared.get(is_alive, connector(&connects)).expect("host");
        assert!(Arc::ptr_eq(&adopted, &next));
        assert_eq!(connects.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn adopt_keeps_an_already_kept_host() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let kept = shared.get(is_alive, connector(&connects)).expect("host");
        let adopted = shared.adopt(FakeHost {
            id: 7,
            alive: AtomicBool::new(true),
        });
        assert!(Arc::ptr_eq(&kept, &adopted));
        assert_eq!(adopted.id, 0);
    }

    #[test]
    fn a_host_connected_by_another_caller_is_taken_as_is() {
        let shared = SharedHost::new();
        let connects = AtomicUsize::new(0);
        let first = shared.get(is_alive, connector(&connects)).expect("host");
        // While this caller finds the first host dead, another replaces it.
        let other = shared
            .get(
                |_| {
                    shared.get(|_| false, connector(&connects));
                    false
                },
                || panic!("must not connect again"),
            )
            .expect("host");
        assert!(!Arc::ptr_eq(&first, &other));
        assert_eq!(other.id, 1);
        assert_eq!(connects.load(Ordering::SeqCst), 2);
    }
}
