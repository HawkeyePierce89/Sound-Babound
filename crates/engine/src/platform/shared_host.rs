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
//!
//! A connect that times out does leak: the client gives up on the
//! handshake but leaves its connecting thread blocked on the socket. The
//! Linux host probes the server before each connect, but a server can answer
//! the probe and still stall the connect right after it, so [`Backoff`]
//! holds connects off, longer after each such failure (at most one connect
//! a minute), and gives up on connecting for good after
//! [`MAX_STALLED_CONNECTS`] connects that stalled, so the process never
//! leaks more than that many threads, even against a server that always
//! stalls.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

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

    fn lock(&self) -> MutexGuard<'_, Option<Arc<H>>> {
        // The slot holds no invariant a panic could break.
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The hold-off after the first failed connect; it doubles after each
/// further failure in a row.
const BACKOFF_FIRST: Duration = Duration::from_secs(2);

/// The longest hold-off.
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// A failed connect that took at least this long gave up on a stalled
/// handshake (the client's handshake timeout is 2 s) and left its
/// connecting thread behind; a faster failure leaves nothing.
pub(crate) const STALL_THRESHOLD: Duration = Duration::from_millis(1500);

/// How many stalled connects (one leaked thread each) the process allows;
/// after that, [`Backoff`] never allows a connect again.
pub(crate) const MAX_STALLED_CONNECTS: u32 = 8;

/// Holds connects off after connects that failed although the server
/// answered the probe: [`BACKOFF_FIRST`], doubling with each failure in a
/// row up to [`BACKOFF_MAX`]. A successful connect resets the hold-off, but
/// not the count of connects that stalled ([`STALL_THRESHOLD`]): once that
/// reaches [`MAX_STALLED_CONNECTS`], no connect is allowed again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Backoff {
    failures: u32,
    until: Option<Instant>,
    stalled: u32,
}

impl Backoff {
    pub(crate) const fn new() -> Backoff {
        Backoff {
            failures: 0,
            until: None,
            stalled: 0,
        }
    }

    /// Whether a connect may be tried at `now`.
    pub(crate) fn allows(&self, now: Instant) -> bool {
        !self.exhausted() && self.until.is_none_or(|until| now >= until)
    }

    /// Whether so many connects stalled that none is allowed again.
    pub(crate) fn exhausted(&self) -> bool {
        self.stalled >= MAX_STALLED_CONNECTS
    }

    /// Records a connect that failed at `now` after taking `took`; returns
    /// the hold-off it starts.
    pub(crate) fn failed(&mut self, now: Instant, took: Duration) -> Duration {
        if took >= STALL_THRESHOLD {
            self.stalled = self.stalled.saturating_add(1);
        }
        let delay = BACKOFF_FIRST
            .saturating_mul(1 << self.failures.min(16))
            .min(BACKOFF_MAX);
        self.failures = self.failures.saturating_add(1);
        self.until = Some(now + delay);
        delay
    }

    /// Records a successful connect. Stalled connects stay counted: their
    /// threads are still leaked.
    pub(crate) fn succeeded(&mut self) {
        self.failures = 0;
        self.until = None;
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

    #[test]
    fn backoff_doubles_up_to_its_cap_and_resets_on_success() {
        let start = Instant::now();
        let mut backoff = Backoff::new();
        assert!(backoff.allows(start));

        let delays: Vec<u64> = (0..8)
            .map(|_| backoff.failed(start, Duration::ZERO).as_secs())
            .collect();
        assert_eq!(delays, [2, 4, 8, 16, 32, 60, 60, 60]);
        // Fast failures leak nothing, so they never exhaust it.
        assert!(!backoff.exhausted());

        let mut backoff = Backoff::new();
        backoff.failed(start, Duration::ZERO);
        assert!(!backoff.allows(start));
        assert!(!backoff.allows(start + Duration::from_millis(1999)));
        assert!(backoff.allows(start + BACKOFF_FIRST));

        backoff.succeeded();
        assert!(backoff.allows(start));
        assert_eq!(backoff.failed(start, Duration::ZERO), BACKOFF_FIRST);
    }

    #[test]
    fn backoff_gives_up_after_too_many_stalled_connects_even_across_successes() {
        let start = Instant::now();
        let later = start + Duration::from_secs(3600);
        let mut backoff = Backoff::new();
        for _ in 0..MAX_STALLED_CONNECTS - 1 {
            backoff.failed(start, STALL_THRESHOLD);
            // A success in between resets the hold-off, not the count.
            backoff.succeeded();
            assert!(backoff.allows(start));
        }
        backoff.failed(start, Duration::from_millis(1499));
        assert!(!backoff.exhausted());
        assert!(backoff.allows(later));

        backoff.failed(start, Duration::from_secs(2));
        assert!(backoff.exhausted());
        assert!(!backoff.allows(later));
        backoff.succeeded();
        assert!(!backoff.allows(later));
    }
}
