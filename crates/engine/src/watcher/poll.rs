//! A watcher that polls a query and reports when its answer changes.
//!
//! For an OS with no change notification: the Linux capture polls the default
//! input and the input list this way, because the PulseAudio stream stays on
//! the source it opened. The query runs on the watcher's own thread, never on the
//! supervisor's, and re-queries the device every time (nothing is cached).
//!
//! [`start`] returns once the first answer (the baseline) is in, so a stream
//! the supervisor builds next is never newer than the baseline, and a change
//! right after the build is still reported. A query that hangs (a server that
//! stops answering) never blocks the supervisor for long: [`start`] and the
//! guard's drop each wait at most the given stop timeout, and a query still
//! running then is left to finish on its own thread, which then exits without
//! sending anything. If [`start`] gave up waiting for the baseline, the
//! supervisor may have built on an older answer than the first one, so that
//! first answer is reported as a change.
//!
//! Every query runs while holding a gate the caller shares between its
//! pollers. A poller whose query is still hung after its guard dropped keeps
//! the gate, so the next poller waits for it (checking its own stop signal
//! every interval) instead of opening another connection that could hang as
//! well. At most one hung query, and its thread, is ever left behind per gate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::warn;

use super::{DeviceEvent, EventSender, WatcherGuard};
use crate::error::WatcherError;

/// Tracks the answers of a polled query. The first answer is the baseline;
/// every later answer that differs from the previous one is a change.
#[derive(Debug)]
pub(crate) struct ChangeDetector<T> {
    last: Option<T>,
}

impl<T: PartialEq> ChangeDetector<T> {
    pub(crate) fn new() -> ChangeDetector<T> {
        ChangeDetector { last: None }
    }

    /// Record `current`. The previous answer, if there was one and it
    /// differs.
    pub(crate) fn observe(&mut self, current: T) -> Option<T> {
        let previous = self.last.replace(current);
        previous.filter(|previous| self.last.as_ref() != Some(previous))
    }

    /// The last answer recorded.
    pub(crate) fn last(&self) -> Option<&T> {
        self.last.as_ref()
    }
}

/// Stops the poll thread when dropped, and joins it unless a query is still
/// running after the stop timeout.
struct PollGuard {
    stop: Option<Sender<()>>,
    /// Disconnects when the thread ends (it also carries the baseline signal).
    done: Receiver<()>,
    stop_timeout: Duration,
    thread: Option<JoinHandle<()>>,
}

impl WatcherGuard for PollGuard {}

impl Drop for PollGuard {
    fn drop(&mut self) {
        // Dropping the sender wakes a waiting thread at once.
        drop(self.stop.take());
        let Some(thread) = self.thread.take() else {
            return;
        };
        if !wait_for_exit(&self.done, self.stop_timeout) {
            warn!("a device poll query did not return; leaving its thread to finish on its own");
            return;
        }
        if thread.join().is_err() {
            warn!("device poll thread panicked");
        }
    }
}

/// Wait up to `timeout` for the poll thread to end. True if it did.
fn wait_for_exit(done: &Receiver<()>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            // A baseline signal that came after `start` stopped waiting.
            Ok(()) => {}
            Err(RecvTimeoutError::Disconnected) => return true,
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

/// Start a thread that runs `query` every `interval` and sends
/// `event(Some(previous), current)` each time the answer differs from the
/// previous one. The first answer is the baseline; this returns once it is
/// in, or after `stop_timeout` if the first query is slow (or waits for
/// `gate`), in which case `event(None, first)` is sent. Each query holds `gate`. The thread ends when the guard
/// drops or the supervisor is gone; the drop waits at most `stop_timeout` for
/// a running query.
pub(crate) fn start<T, Q, E>(
    interval: Duration,
    stop_timeout: Duration,
    gate: &'static Mutex<()>,
    mut query: Q,
    event: E,
    events: EventSender,
) -> Result<Box<dyn WatcherGuard>, WatcherError>
where
    T: PartialEq,
    Q: FnMut() -> T + Send + 'static,
    E: Fn(Option<&T>, &T) -> DeviceEvent + Send + 'static,
{
    let (stop_tx, stop_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    // Set by whichever comes first: the poll thread taking the baseline, or
    // `start` giving up on it. The thread reads it to learn it was late.
    let baseline_settled = Arc::new(AtomicBool::new(false));
    let poller = Poller {
        interval,
        gate,
        events,
        stop: stop_rx,
        baseline_taken: done_tx,
        baseline_settled: Arc::clone(&baseline_settled),
    };
    let thread = thread::Builder::new()
        .name("babound-device-poll".into())
        .spawn(move || poller.run(&mut query, &event))
        .map_err(|err| WatcherError(format!("spawning the poll thread failed: {err}")))?;
    if let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(stop_timeout)
        && !baseline_settled.swap(true, Ordering::SeqCst)
    {
        warn!("the first device poll query is slow; its answer will be reported as a change");
    }
    Ok(Box::new(PollGuard {
        stop: Some(stop_tx),
        done: done_rx,
        stop_timeout,
        thread: Some(thread),
    }))
}

/// The poll thread's state.
struct Poller {
    interval: Duration,
    gate: &'static Mutex<()>,
    events: EventSender,
    stop: Receiver<()>,
    baseline_taken: Sender<()>,
    baseline_settled: Arc<AtomicBool>,
}

impl Poller {
    fn run<T: PartialEq>(
        &self,
        query: &mut impl FnMut() -> T,
        event: &impl Fn(Option<&T>, &T) -> DeviceEvent,
    ) {
        let mut detector = ChangeDetector::new();
        let Some(first) = self.gated_query(query) else {
            return;
        };
        let late = self.baseline_settled.swap(true, Ordering::SeqCst);
        // `start` may have stopped waiting already; that is fine.
        let _ = self.baseline_taken.send(());
        if self.stopped() {
            return;
        }
        // `start` returned without the baseline, so the supervisor may have
        // built on an older answer: report this one.
        if late && !self.events.send(event(None, &first)) {
            return;
        }
        detector.observe(first);
        loop {
            match self.stop.recv_timeout(self.interval) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
            let Some(current) = self.gated_query(query) else {
                return;
            };
            if self.stopped() {
                return;
            }
            if let Some(previous) = detector.observe(current)
                && let Some(current) = detector.last()
                && !self.events.send(event(Some(&previous), current))
            {
                return;
            }
        }
    }

    /// Run `query` while holding the gate. While another poller's query holds
    /// it, check the stop signal every interval. `None` if stopped first.
    fn gated_query<T>(&self, query: &mut impl FnMut() -> T) -> Option<T> {
        loop {
            match self.gate.try_lock() {
                Ok(_held) => return Some(query()),
                // A query that panicked holding the gate does not hold it now.
                Err(TryLockError::Poisoned(poisoned)) => {
                    let _held = poisoned.into_inner();
                    return Some(query());
                }
                Err(TryLockError::WouldBlock) => {}
            }
            match self.stop.recv_timeout(self.interval) {
                Err(RecvTimeoutError::Timeout) => {}
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// True once the guard dropped (it may have while a slow query ran).
    fn stopped(&self) -> bool {
        !matches!(self.stop.try_recv(), Err(TryRecvError::Empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SupervisorMsg;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const STOP_TIMEOUT: Duration = Duration::from_secs(2);

    /// The event of a poller that reports every change the same way.
    fn default_changed<T>(_previous: Option<&T>, _current: &T) -> DeviceEvent {
        DeviceEvent::DefaultInputChanged
    }

    /// A gate of the test's own, so parallel tests do not wait for each other.
    fn gate() -> &'static Mutex<()> {
        Box::leak(Box::new(Mutex::new(())))
    }

    #[test]
    fn detector_takes_the_first_answer_as_the_baseline() {
        let mut detector = ChangeDetector::new();
        assert_eq!(detector.observe(Some("a")), None);
        assert_eq!(detector.observe(Some("a")), None);
        assert_eq!(detector.observe(Some("b")), Some(Some("a")));
        assert_eq!(detector.observe(Some("b")), None);
        assert_eq!(detector.observe(None), Some(Some("b")));
        assert_eq!(detector.observe(None), None);
        assert_eq!(detector.observe(Some("b")), Some(None));
        assert_eq!(detector.last(), Some(&Some("b")));
    }

    #[test]
    fn poller_reports_each_change_once() {
        let answer = Arc::new(Mutex::new("mic-a"));
        let query = {
            let answer = Arc::clone(&answer);
            move || *answer.lock().unwrap()
        };
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            STOP_TIMEOUT,
            gate(),
            query,
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();

        // The baseline and repeats are not changes.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());

        *answer.lock().unwrap() = "mic-b";
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SupervisorMsg::Device(DeviceEvent::DefaultInputChanged)
        ));
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());

        drop(guard);
        // The thread is joined, so the sender is gone.
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn dropping_the_guard_stops_a_slow_poller_at_once() {
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_secs(3600),
            STOP_TIMEOUT,
            gate(),
            || 0,
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        let started = Instant::now();
        drop(guard);
        // Well under the stop timeout: the drop woke the thread, it did not
        // give up on it.
        assert!(started.elapsed() < Duration::from_millis(500));
        // The thread is joined, so the sender is gone.
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn poller_ends_when_the_supervisor_is_gone() {
        let answer = Arc::new(Mutex::new(0));
        let queries = Arc::new(AtomicUsize::new(0));
        let query = {
            let answer = Arc::clone(&answer);
            let queries = Arc::clone(&queries);
            move || {
                queries.fetch_add(1, Ordering::SeqCst);
                *answer.lock().unwrap()
            }
        };
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            STOP_TIMEOUT,
            gate(),
            query,
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        drop(rx);
        *answer.lock().unwrap() = 1;
        // The failed send ends the thread while the guard is still alive:
        // the queries stop.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let before = queries.load(Ordering::SeqCst);
            thread::sleep(Duration::from_millis(100));
            if queries.load(Ordering::SeqCst) == before {
                break;
            }
            assert!(Instant::now() < deadline, "the poller kept running");
        }
        drop(guard);
    }

    #[test]
    fn a_change_event_is_built_from_the_previous_and_current_answers() {
        let answer = Arc::new(Mutex::new("mic-a"));
        let query = {
            let answer = Arc::clone(&answer);
            move || *answer.lock().unwrap()
        };
        let event = |previous: Option<&&str>, current: &&str| match (previous, *current) {
            (Some(&"mic-a"), "mic-b") => DeviceEvent::DeviceListChanged,
            _ => DeviceEvent::DeviceRemoved,
        };
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            STOP_TIMEOUT,
            gate(),
            query,
            event,
            EventSender::new(tx),
        )
        .unwrap();
        *answer.lock().unwrap() = "mic-b";
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SupervisorMsg::Device(DeviceEvent::DeviceListChanged)
        ));
        drop(guard);
    }

    #[test]
    fn start_returns_after_the_baseline_so_a_change_right_after_is_reported() {
        let answer = Arc::new(Mutex::new("mic-a"));
        let queries = Arc::new(AtomicUsize::new(0));
        let query = {
            let answer = Arc::clone(&answer);
            let queries = Arc::clone(&queries);
            move || {
                queries.fetch_add(1, Ordering::SeqCst);
                *answer.lock().unwrap()
            }
        };
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            STOP_TIMEOUT,
            gate(),
            query,
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        // The supervisor would build on "mic-a" now; the baseline is already in.
        assert!(queries.load(Ordering::SeqCst) >= 1);
        *answer.lock().unwrap() = "mic-b";
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SupervisorMsg::Device(DeviceEvent::DefaultInputChanged)
        ));
        drop(guard);
    }

    #[test]
    fn start_does_not_wait_forever_for_a_hung_first_query() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        let started = Instant::now();
        let guard = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate(),
            move || {
                let _ = release_rx.recv();
                0
            },
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        let dropped = Instant::now();
        drop(guard);
        assert!(dropped.elapsed() < Duration::from_secs(2));
        // The late first answer comes after the guard dropped: the thread
        // exits without reporting it.
        drop(release_tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn dropping_the_guard_does_not_wait_for_a_hung_query() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let query = {
            let calls = Arc::clone(&calls);
            move || {
                // The baseline returns at once; the next query hangs until
                // released, then reports a different answer.
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return 0;
                }
                let _ = release_rx.recv();
                1
            }
        };
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate(),
            query,
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "the second query never ran");
            thread::sleep(Duration::from_millis(1));
        }

        let dropped = Instant::now();
        drop(guard);
        assert!(dropped.elapsed() < Duration::from_secs(2));

        // Once the query returns, the left-behind thread exits without
        // reporting the changed answer.
        drop(release_tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn a_late_first_answer_is_reported_as_a_change() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel();
        let guard = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate(),
            move || {
                let _ = release_rx.recv();
                "mic-b"
            },
            // A late first answer has no previous one.
            |previous: Option<&&str>, current: &&str| match (previous, *current) {
                (None, "mic-b") => DeviceEvent::DefaultInputChanged,
                _ => DeviceEvent::DeviceRemoved,
            },
            EventSender::new(tx),
        )
        .unwrap();
        // `start` gave up on the baseline; the supervisor builds on whatever
        // it finds. The first answer, once in, triggers a rebuild.
        drop(release_tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SupervisorMsg::Device(DeviceEvent::DefaultInputChanged)
        ));
        // Later identical answers are not changes.
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
    }

    #[test]
    fn a_poller_waits_behind_a_hung_query_instead_of_running_its_own() {
        let gate = gate();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (hung_tx, _hung_rx) = mpsc::channel();
        let hung = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate,
            move || {
                let _ = release_rx.recv();
                0
            },
            default_changed,
            EventSender::new(hung_tx),
        )
        .unwrap();
        // Left behind, still holding the gate.
        drop(hung);

        let queries = Arc::new(AtomicUsize::new(0));
        let counting = |queries: &Arc<AtomicUsize>| {
            let queries = Arc::clone(queries);
            move || {
                queries.fetch_add(1, Ordering::SeqCst);
                0
            }
        };

        // A poller stopped while it waits ends at once without querying.
        let (tx, rx) = mpsc::channel();
        let waiting = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate,
            counting(&queries),
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        let dropped = Instant::now();
        drop(waiting);
        assert!(dropped.elapsed() < Duration::from_secs(2));
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(RecvTimeoutError::Disconnected)
        ));
        assert_eq!(queries.load(Ordering::SeqCst), 0);

        // A poller still running when the hung query returns takes over; its
        // first answer is late, so it is reported.
        let (tx, rx) = mpsc::channel();
        let next = start(
            Duration::from_millis(5),
            Duration::from_millis(50),
            gate,
            counting(&queries),
            default_changed,
            EventSender::new(tx),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(queries.load(Ordering::SeqCst), 0);
        drop(release_tx);
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            SupervisorMsg::Device(DeviceEvent::DefaultInputChanged)
        ));
        assert!(queries.load(Ordering::SeqCst) >= 1);
        drop(next);
    }
}
