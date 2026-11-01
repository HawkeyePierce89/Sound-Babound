//! The device supervisor.
//!
//! A dedicated thread owns the output sink. It listens for device events and
//! stream errors on one channel, debounces them, decides whether they affect
//! the active output and rebuilds the stream on the current default device
//! when they do. Voices keep playing across a rebuild because the mixer, and
//! with it every voice position, outlives the stream.
//!
//! The decision logic is the pure [`SupervisorCore`], driven by explicit
//! [`Instant`]s so it can be tested with a fake clock. [`spawn_supervisor`]
//! wraps it in the thread.
//!
//! # Rebuild policy
//!
//! A rebuild is an audible gap, so not every event triggers one. After the
//! trailing-edge debounce window:
//!
//! - a [`RebuildPolicy::Always`] event or a stream error rebuilds;
//! - a window holding only [`RebuildPolicy::IfDefaultOutputChanged`] events
//!   yields [`Action::CheckDefault`]: the live default output identity is
//!   re-queried and the stream is rebuilt only if it differs from the identity
//!   the stream was built on;
//! - [`RebuildPolicy::Ignore`] events do nothing.
//!
//! While no stream is running (initial start, retry backoff) every
//! non-ignored event rebuilds. A failed build is retried after a backoff, and
//! a stream whose callback heartbeat stops advancing is rebuilt.
//!
//! # Stall watchdog
//!
//! The heartbeat is read just before every build; that value is the
//! baseline. Until the heartbeat first moves past it, the stream is still
//! starting (some devices take seconds to deliver their first callback), and
//! it is rebuilt only if [`Timings::startup_grace`] passes since the build.
//! Once it has moved, the stream is rebuilt if the heartbeat stands still for
//! [`Timings::stall`].
//!
//! # Plays across a rebuild
//!
//! `Engine::play` accepts plays while a stream runs and, after the supervisor
//! tears down a live stream for a rebuild, for [`Timings::play_grace`] more
//! (see `MixerShared::accepts_plays`). The initial start and a teardown with
//! no stream open no window. A rebuild from a live stream keeps the command
//! queue, and while the window is open the offline purge is skipped, so plays
//! accepted while the old stream ran, or during the switch, keep their order
//! and are applied by the new stream, like the voices it continues. Once the
//! window has closed with no stream, queued plays are dropped (see
//! `Mixer::apply_offline`), as is a play that passed the check just before the
//! stream went away.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tracing::{Level, debug, error, info, warn};

use crate::mixer::{Mixer, MixerShared};
use crate::sink::{ActiveSink, DeviceIdentity, SinkFactory};
use crate::watcher::{DeviceEvent, DeviceWatcher, EventSender, RebuildPolicy};

/// Longest the supervisor thread sleeps between garbage drains.
pub const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// A message for the supervisor thread.
#[derive(Debug)]
pub enum SupervisorMsg {
    /// A device event from the watcher.
    Device(DeviceEvent),
    /// The active stream reported an error. Every kind rebuilds; the kind
    /// only picks the log level (see [`stream_error_level`]).
    StreamError {
        kind: StreamErrorKind,
        message: String,
    },
    /// Stop the stream and exit the thread.
    Shutdown,
}

/// What kind of error a stream reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// The backend rerouted the stream because the device changed. This is
    /// the expected result of switching outputs, not a fault.
    DeviceChanged,
    /// Any other error that needs a rebuild.
    Other,
}

/// The level a stream error is logged at. A device change is routine and
/// logged at debug; anything else is a warning. Both rebuild.
pub(crate) fn stream_error_level(kind: StreamErrorKind) -> Level {
    match kind {
        StreamErrorKind::DeviceChanged => Level::DEBUG,
        StreamErrorKind::Other => Level::WARN,
    }
}

/// The supervisor's time constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    /// Quiet time after the last event before acting on a window.
    pub debounce: Duration,
    /// Delay before retrying a failed build.
    pub retry: Duration,
    /// How long the callback heartbeat may stand still before a rebuild,
    /// once the stream has started.
    pub stall: Duration,
    /// How long a new stream may take to deliver its first callback before a
    /// rebuild.
    pub startup_grace: Duration,
    /// How long after a live stream is torn down for a rebuild plays are
    /// still accepted and kept for the replacement stream.
    pub play_grace: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            debounce: Duration::from_millis(200),
            retry: Duration::from_secs(1),
            stall: Duration::from_secs(2),
            startup_grace: Duration::from_secs(10),
            play_grace: Duration::from_secs(1),
        }
    }
}

/// What the supervisor should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Drop the stream and build a new one on the current default device.
    Rebuild,
    /// Re-query the default output identity and pass it to
    /// [`SupervisorCore::resolve_check`].
    CheckDefault,
}

/// The result of [`SupervisorCore::resolve_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The default output changed: rebuild.
    Rebuild,
    /// The event did not affect the active output: drop it.
    Drop,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    deadline: Instant,
    /// Holds an `Always` event or a stream error.
    force: bool,
}

#[derive(Debug, Clone)]
struct ActiveStream {
    identity: DeviceIdentity,
    built_at: Instant,
    /// Whether the heartbeat has moved past the value read before the build.
    started: bool,
    /// The baseline until `started`, then the last value seen.
    last_beat: u64,
    /// When `last_beat` was first seen; meaningful once `started`.
    last_beat_at: Instant,
}

impl ActiveStream {
    /// When the watchdog rebuilds this stream if the heartbeat stays put.
    fn stall_deadline(&self, timings: &Timings) -> Instant {
        if self.started {
            self.last_beat_at + timings.stall
        } else {
            self.built_at + timings.startup_grace
        }
    }
}

/// The supervisor's pure decision logic. See the module docs.
#[derive(Debug, Clone)]
pub struct SupervisorCore {
    timings: Timings,
    stream: Option<ActiveStream>,
    window: Option<Window>,
    build_at: Option<Instant>,
}

impl SupervisorCore {
    /// A core with `timings` that wants to build at `now`.
    pub fn new(timings: Timings, now: Instant) -> SupervisorCore {
        SupervisorCore {
            timings,
            stream: None,
            window: None,
            build_at: Some(now),
        }
    }

    /// Record a device event classified by `policy`.
    pub fn on_event(&mut self, policy: RebuildPolicy, now: Instant) {
        match policy {
            RebuildPolicy::Ignore => {}
            RebuildPolicy::Always => self.open_window(true, now),
            RebuildPolicy::IfDefaultOutputChanged => self.open_window(false, now),
        }
    }

    /// Record a stream error. It always leads to a rebuild.
    pub fn on_stream_error(&mut self, now: Instant) {
        self.open_window(true, now);
    }

    /// Record the result of a build: `Some` with the identity the stream was
    /// built on, or `None` if the build failed (retried after the backoff).
    ///
    /// `baseline` is the heartbeat read just before the build. The stream
    /// counts as started once a sample differs from it, so a callback that
    /// runs between the build and the first sample is not missed.
    pub fn on_build_result(
        &mut self,
        identity: Option<DeviceIdentity>,
        baseline: u64,
        now: Instant,
    ) {
        match identity {
            Some(identity) => {
                self.stream = Some(ActiveStream {
                    identity,
                    built_at: now,
                    started: false,
                    last_beat: baseline,
                    last_beat_at: now,
                });
                self.build_at = None;
            }
            None => {
                self.stream = None;
                self.build_at = Some(now + self.timings.retry);
            }
        }
    }

    /// Record the mixer's callback heartbeat as sampled at `now`.
    ///
    /// The first change from the build's baseline marks the stream as
    /// started; from then on the stall clock restarts whenever the value
    /// changes.
    pub fn on_heartbeat(&mut self, value: u64, now: Instant) {
        if let Some(stream) = &mut self.stream
            && stream.last_beat != value
        {
            stream.started = true;
            stream.last_beat = value;
            stream.last_beat_at = now;
        }
    }

    /// The action due at `now`, if any. Returning [`Action::Rebuild`] marks
    /// the stream as gone; report the outcome with
    /// [`on_build_result`](SupervisorCore::on_build_result).
    pub fn poll(&mut self, now: Instant) -> Option<Action> {
        if let Some(window) = self.window
            && now >= window.deadline
        {
            self.window = None;
            if window.force || self.stream.is_none() {
                return Some(self.begin_rebuild());
            }
            return Some(Action::CheckDefault);
        }
        if self.build_at.is_some_and(|at| now >= at) {
            return Some(self.begin_rebuild());
        }
        if let Some(stream) = &self.stream
            && now >= stream.stall_deadline(&self.timings)
        {
            return Some(self.begin_rebuild());
        }
        None
    }

    /// Decide an [`Action::CheckDefault`] given the live default output
    /// identity: rebuild if it differs from the identity the stream was built
    /// on (including `None` against `Some`), otherwise drop the event.
    pub fn resolve_check(&mut self, current: Option<DeviceIdentity>) -> CheckOutcome {
        match &self.stream {
            Some(stream) if current.as_ref() == Some(&stream.identity) => CheckOutcome::Drop,
            _ => {
                self.begin_rebuild();
                CheckOutcome::Rebuild
            }
        }
    }

    /// The next instant at which [`poll`](SupervisorCore::poll) may return
    /// an action.
    pub fn next_deadline(&self) -> Option<Instant> {
        let stall = self
            .stream
            .as_ref()
            .map(|s| s.stall_deadline(&self.timings));
        [self.window.map(|w| w.deadline), self.build_at, stall]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether a stream is supposed to be running.
    #[cfg(test)]
    pub fn has_stream(&self) -> bool {
        self.stream.is_some()
    }

    /// The identity the running stream was built on.
    #[cfg(test)]
    pub fn built_on(&self) -> Option<&DeviceIdentity> {
        self.stream.as_ref().map(|s| &s.identity)
    }

    fn open_window(&mut self, force: bool, now: Instant) {
        let force = force || self.window.is_some_and(|w| w.force);
        self.window = Some(Window {
            deadline: now + self.timings.debounce,
            force,
        });
    }

    fn begin_rebuild(&mut self) -> Action {
        self.window = None;
        self.build_at = None;
        self.stream = None;
        Action::Rebuild
    }
}

/// A running supervisor thread. Dropping it shuts the thread down.
#[derive(Debug)]
pub struct SupervisorHandle {
    sender: Sender<SupervisorMsg>,
    thread: Option<JoinHandle<()>>,
}

impl SupervisorHandle {
    /// Stop the stream, unregister the watcher and join the thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // A send error means the thread already exited; join anyway.
            let _ = self.sender.send(SupervisorMsg::Shutdown);
            if thread.join().is_err() {
                error!("supervisor thread panicked");
            }
        }
    }
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start the supervisor thread.
///
/// The thread creates the mixer on `shared`, starts `watcher`, builds the
/// first stream immediately and then supervises it until shutdown.
pub(crate) fn spawn_supervisor<F, W>(
    factory: F,
    watcher: W,
    shared: Arc<MixerShared>,
    timings: Timings,
) -> std::io::Result<SupervisorHandle>
where
    F: SinkFactory,
    W: DeviceWatcher,
{
    let (sender, receiver) = mpsc::channel();
    let thread_sender = sender.clone();
    let thread = thread::Builder::new()
        .name("babound-supervisor".into())
        .spawn(move || {
            Worker::new(factory, shared, timings, thread_sender).run(watcher, receiver);
        })?;
    Ok(SupervisorHandle {
        sender,
        thread: Some(thread),
    })
}

/// The supervisor thread's state.
struct Worker<F> {
    factory: F,
    shared: Arc<MixerShared>,
    mixer: Arc<Mutex<Mixer>>,
    sender: Sender<SupervisorMsg>,
    core: SupervisorCore,
    play_grace: Duration,
    sink: Option<Box<dyn ActiveSink>>,
}

impl<F: SinkFactory> Worker<F> {
    fn new(
        factory: F,
        shared: Arc<MixerShared>,
        timings: Timings,
        sender: Sender<SupervisorMsg>,
    ) -> Worker<F> {
        Worker {
            factory,
            mixer: Arc::new(Mutex::new(Mixer::new(Arc::clone(&shared)))),
            shared,
            sender,
            core: SupervisorCore::new(timings, Instant::now()),
            play_grace: timings.play_grace,
            sink: None,
        }
    }

    fn run<W: DeviceWatcher>(mut self, mut watcher: W, receiver: Receiver<SupervisorMsg>) {
        let guard = match watcher.start(EventSender::new(self.sender.clone())) {
            Ok(guard) => Some(guard),
            Err(err) => {
                warn!(error = %err, "device watcher unavailable; relying on stream errors");
                None
            }
        };

        loop {
            let now = Instant::now();
            self.core
                .on_heartbeat(self.shared.callback_heartbeat(), now);
            match self.core.poll(now) {
                Some(Action::Rebuild) => self.rebuild(),
                Some(Action::CheckDefault) => self.check_default(),
                None => {}
            }
            if self.sink.is_none() {
                self.purge_offline();
            }
            self.shared.drain_garbage();

            let wait = self
                .core
                .next_deadline()
                .map_or(DRAIN_INTERVAL, |at| {
                    at.saturating_duration_since(Instant::now())
                })
                .min(DRAIN_INTERVAL);
            match receiver.recv_timeout(wait) {
                Ok(SupervisorMsg::Device(event)) => self.on_device_event(event),
                Ok(SupervisorMsg::StreamError { kind, message }) => {
                    if stream_error_level(kind) == Level::DEBUG {
                        debug!(error = %message, "output device changed; rebuilding");
                    } else {
                        warn!(error = %message, "output stream error");
                    }
                    self.core.on_stream_error(Instant::now());
                }
                Ok(SupervisorMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        drop(self.sink.take());
        self.shared.close_play_grace();
        self.shared.set_output_active(false);
        drop(guard);
        self.shared.drain_garbage();
        debug!("supervisor stopped");
    }

    fn on_device_event(&mut self, event: DeviceEvent) {
        let policy = event.rebuild_policy();
        if policy == RebuildPolicy::Ignore {
            debug!(?event, "input device event ignored");
        } else {
            debug!(?event, ?policy, "device event");
        }
        self.core.on_event(policy, Instant::now());
    }

    fn check_default(&mut self) {
        let current = self.factory.current_default_identity();
        match self.core.resolve_check(current.clone()) {
            CheckOutcome::Rebuild => {
                info!(
                    default = current.as_ref().map_or("<none>", DeviceIdentity::as_str),
                    "default output changed; rebuilding"
                );
                self.rebuild();
            }
            CheckOutcome::Drop => {
                debug!("device change did not affect the default output; ignored");
            }
        }
    }

    /// With no stream: drop the queued plays unless the play grace window
    /// from a live teardown is still open, in which case the queue is kept
    /// in order for the replacement stream.
    fn purge_offline(&mut self) {
        if !self.shared.play_grace_open(Instant::now()) {
            self.drop_offline_plays();
        }
    }

    /// Drop the plays queued while no stream exists, and apply the other
    /// commands. No stream means no callback, so the lock cannot contend.
    fn drop_offline_plays(&mut self) {
        let dropped = self
            .mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .apply_offline();
        if dropped > 0 {
            info!(dropped, "no output stream; dropped queued plays");
        }
    }

    /// Drop the stream, then build a new one. The factory configures the
    /// mixer while no stream exists, so the callback never contends for it.
    ///
    /// When a stream existed, the queue is kept on purpose: its plays were
    /// accepted while that stream ran, and the new stream applies them. The
    /// play grace window opens before output is marked inactive, so plays
    /// stay accepted without a gap. When no stream existed, plays queued
    /// since the last loop's purge are dropped first, so the new stream never
    /// starts a press made offline, unless the grace window is still open.
    fn rebuild(&mut self) {
        match self.sink.take() {
            Some(sink) => {
                self.shared
                    .open_play_grace(Instant::now() + self.play_grace);
                drop(sink);
            }
            None => self.purge_offline(),
        }
        self.shared.set_output_active(false);
        // Read before the build, so a callback that runs before the next
        // sample still counts as the stream starting.
        let baseline = self.shared.callback_heartbeat();
        match self
            .factory
            .build(Arc::clone(&self.mixer), self.sender.clone())
        {
            Ok((sink, info)) => {
                info!(
                    device = %info.device_name,
                    rate = info.sample_rate,
                    channels = info.channels,
                    format = %info.sample_format,
                    "output stream started"
                );
                self.sink = Some(sink);
                self.shared.set_output_active(true);
                self.shared.close_play_grace();
                self.core
                    .on_build_result(Some(info.identity), baseline, Instant::now());
            }
            Err(err) => {
                warn!(error = %err, "output stream build failed; will retry");
                self.core.on_build_result(None, baseline, Instant::now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::Command;
    use crate::sound::Sound;
    use crate::test_support::{FakeSinkFactory, FakeWatcher, wait_until};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn id(name: &str) -> DeviceIdentity {
        DeviceIdentity::new(name)
    }

    /// A core with a stream built on `name` at `t0`.
    fn running(t0: Instant, name: &str) -> SupervisorCore {
        let mut core = SupervisorCore::new(Timings::default(), t0);
        assert_eq!(core.poll(t0), Some(Action::Rebuild));
        core.on_build_result(Some(id(name)), 0, t0);
        core
    }

    /// Step the core from `from` to `to` in 10 ms steps with a live heartbeat,
    /// answering checks with `current` and builds with success on `current`.
    /// Returns (rebuilds, checks).
    fn drive(
        core: &mut SupervisorCore,
        from: Instant,
        to: Instant,
        current: Option<&str>,
        beat: &mut u64,
    ) -> (usize, usize) {
        let (mut rebuilds, mut checks) = (0, 0);
        let mut t = from;
        while t <= to {
            *beat += 1;
            core.on_heartbeat(*beat, t);
            match core.poll(t) {
                Some(Action::Rebuild) => {
                    rebuilds += 1;
                    core.on_build_result(current.map(id), *beat, t);
                }
                Some(Action::CheckDefault) => {
                    checks += 1;
                    if core.resolve_check(current.map(id)) == CheckOutcome::Rebuild {
                        rebuilds += 1;
                        core.on_build_result(current.map(id), *beat, t);
                    }
                }
                None => {}
            }
            t += ms(10);
        }
        (rebuilds, checks)
    }

    #[test]
    fn builds_immediately_on_start() {
        let t0 = Instant::now();
        let mut core = SupervisorCore::new(Timings::default(), t0);
        assert!(!core.has_stream());
        assert_eq!(core.next_deadline(), Some(t0));
        assert_eq!(core.poll(t0), Some(Action::Rebuild));
        core.on_build_result(Some(id("spk")), 0, t0);
        assert!(core.has_stream());
        assert_eq!(core.built_on(), Some(&id("spk")));
        assert_eq!(core.poll(t0 + ms(10)), None);
    }

    #[test]
    fn burst_of_five_events_rebuilds_once() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        let t1 = t0 + ms(500);
        for i in 0..5 {
            core.on_event(RebuildPolicy::Always, t1 + ms(40 * i));
        }
        assert_eq!(core.next_deadline(), Some(t1 + ms(160 + 200)));
        let mut beat = 0;
        assert_eq!(
            drive(&mut core, t1, t1 + ms(350), Some("hp"), &mut beat),
            (0, 0)
        );
        assert_eq!(
            drive(
                &mut core,
                t1 + ms(360),
                t1 + ms(1500),
                Some("hp"),
                &mut beat
            ),
            (1, 0)
        );
        assert_eq!(core.built_on(), Some(&id("hp")));
    }

    #[test]
    fn failed_builds_retry_until_success() {
        let t0 = Instant::now();
        let mut core = SupervisorCore::new(Timings::default(), t0);
        assert_eq!(core.poll(t0), Some(Action::Rebuild));
        core.on_build_result(None, 0, t0);
        assert!(!core.has_stream());
        assert_eq!(core.next_deadline(), Some(t0 + ms(1000)));
        assert_eq!(core.poll(t0 + ms(999)), None);
        assert_eq!(core.poll(t0 + ms(1000)), Some(Action::Rebuild));
        core.on_build_result(None, 0, t0 + ms(1000));
        assert_eq!(core.poll(t0 + ms(1999)), None);
        assert_eq!(core.poll(t0 + ms(2000)), Some(Action::Rebuild));
        core.on_build_result(Some(id("spk")), 0, t0 + ms(2000));

        let mut beat = 0;
        assert_eq!(
            drive(
                &mut core,
                t0 + ms(2010),
                t0 + ms(6000),
                Some("spk"),
                &mut beat
            ),
            (0, 0)
        );
    }

    #[test]
    fn events_during_backoff_still_debounce_and_rebuild() {
        let t0 = Instant::now();
        let mut core = SupervisorCore::new(Timings::default(), t0);
        assert_eq!(core.poll(t0), Some(Action::Rebuild));
        core.on_build_result(None, 0, t0);
        core.on_event(RebuildPolicy::IfDefaultOutputChanged, t0 + ms(100));
        assert_eq!(core.poll(t0 + ms(299)), None);
        // No stream is active, so even an identity-checked event rebuilds.
        assert_eq!(core.poll(t0 + ms(300)), Some(Action::Rebuild));
    }

    #[test]
    fn stalled_heartbeat_rebuilds() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_heartbeat(5, t0 + ms(100));
        core.on_heartbeat(6, t0 + ms(500));
        core.on_heartbeat(6, t0 + ms(1500));
        assert_eq!(core.next_deadline(), Some(t0 + ms(2500)));
        assert_eq!(core.poll(t0 + ms(2499)), None);
        assert_eq!(core.poll(t0 + ms(2500)), Some(Action::Rebuild));
        assert!(!core.has_stream());
    }

    /// A core with a stream built on "spk" at `t0` on heartbeat `baseline`.
    fn built_with_baseline(t0: Instant, baseline: u64) -> SupervisorCore {
        let mut core = SupervisorCore::new(Timings::default(), t0);
        assert_eq!(core.poll(t0), Some(Action::Rebuild));
        core.on_build_result(Some(id("spk")), baseline, t0);
        core
    }

    #[test]
    fn heartbeat_that_never_moves_after_build_rebuilds() {
        let t0 = Instant::now();
        let mut core = built_with_baseline(t0, 3);
        core.on_heartbeat(3, t0 + ms(100));
        core.on_heartbeat(3, t0 + ms(9900));
        assert_eq!(core.next_deadline(), Some(t0 + ms(10_000)));
        assert_eq!(core.poll(t0 + ms(9999)), None);
        assert_eq!(core.poll(t0 + ms(10_000)), Some(Action::Rebuild));
    }

    #[test]
    fn first_callback_at_five_seconds_does_not_rebuild() {
        let t0 = Instant::now();
        let mut core = built_with_baseline(t0, 0);
        // Well past the 2 s stall time, but inside the startup grace.
        let mut t = t0;
        while t < t0 + ms(5000) {
            core.on_heartbeat(0, t);
            assert_eq!(core.poll(t), None, "{:?}", t - t0);
            t += ms(10);
        }
        // The first callback lands; from now on the stall clock applies.
        let mut beat = 0;
        assert_eq!(
            drive(&mut core, t, t0 + ms(15_000), Some("spk"), &mut beat),
            (0, 0)
        );
        assert!(core.has_stream());
    }

    #[test]
    fn no_callback_for_the_startup_grace_rebuilds() {
        let t0 = Instant::now();
        let mut core = built_with_baseline(t0, 0);
        let mut t = t0;
        while t < t0 + ms(10_000) {
            core.on_heartbeat(0, t);
            assert_eq!(core.poll(t), None, "{:?}", t - t0);
            t += ms(10);
        }
        core.on_heartbeat(0, t);
        assert_eq!(core.poll(t), Some(Action::Rebuild));
        assert!(!core.has_stream());
    }

    #[test]
    fn running_stream_that_stops_still_rebuilds_after_the_stall_time() {
        let t0 = Instant::now();
        let mut core = built_with_baseline(t0, 0);
        let mut beat = 0;
        assert_eq!(
            drive(&mut core, t0, t0 + ms(1000), Some("spk"), &mut beat),
            (0, 0)
        );
        // The heartbeat stops at 1 s, long before the startup grace ends.
        core.on_heartbeat(beat, t0 + ms(1500));
        assert_eq!(core.next_deadline(), Some(t0 + ms(3000)));
        assert_eq!(core.poll(t0 + ms(2999)), None);
        assert_eq!(core.poll(t0 + ms(3000)), Some(Action::Rebuild));
    }

    #[test]
    fn callback_before_the_first_sample_counts_as_started() {
        let t0 = Instant::now();
        // The heartbeat was 7 before the build; a callback ran before the
        // supervisor's first sample, which already reads 8.
        let mut core = built_with_baseline(t0, 7);
        core.on_heartbeat(8, t0 + ms(100));
        core.on_heartbeat(8, t0 + ms(1000));
        // Started, so the stall time applies instead of the startup grace.
        assert_eq!(core.next_deadline(), Some(t0 + ms(2100)));
        assert_eq!(core.poll(t0 + ms(2099)), None);
        assert_eq!(core.poll(t0 + ms(2100)), Some(Action::Rebuild));
    }

    #[test]
    fn every_build_gets_a_fresh_startup_grace() {
        let t0 = Instant::now();
        let mut core = built_with_baseline(t0, 0);
        assert_eq!(core.poll(t0 + ms(10_000)), Some(Action::Rebuild));
        let t1 = t0 + ms(10_000);
        core.on_build_result(Some(id("spk")), 0, t1);
        assert_eq!(core.next_deadline(), Some(t1 + ms(10_000)));
        assert_eq!(core.poll(t1 + ms(9999)), None);
    }

    #[test]
    fn stream_error_always_rebuilds() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        let mut beat = 0;
        core.on_stream_error(t0 + ms(100));
        // The default is unchanged, but a stream error never goes through the check.
        assert_eq!(
            drive(
                &mut core,
                t0 + ms(100),
                t0 + ms(1000),
                Some("spk"),
                &mut beat
            ),
            (1, 0)
        );
    }

    #[test]
    fn input_only_events_do_not_rebuild() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        for i in 0..5 {
            core.on_event(
                DeviceEvent::DefaultInputChanged.rebuild_policy(),
                t0 + ms(10 * i),
            );
        }
        // Only the stall watchdog is pending, still in its startup grace.
        assert_eq!(core.next_deadline(), Some(t0 + ms(10_000)));
        let mut beat = 0;
        assert_eq!(
            drive(&mut core, t0, t0 + ms(3000), Some("spk"), &mut beat),
            (0, 0)
        );
    }

    #[test]
    fn unrelated_device_with_same_default_is_dropped() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(
            DeviceEvent::DeviceListChanged.rebuild_policy(),
            t0 + ms(100),
        );
        assert_eq!(core.poll(t0 + ms(299)), None);
        assert_eq!(core.poll(t0 + ms(300)), Some(Action::CheckDefault));
        assert_eq!(core.resolve_check(Some(id("spk"))), CheckOutcome::Drop);
        assert!(core.has_stream());
        assert_eq!(core.built_on(), Some(&id("spk")));
        core.on_heartbeat(1, t0 + ms(300));
        assert_eq!(core.poll(t0 + ms(400)), None);
    }

    #[test]
    fn list_change_with_new_default_rebuilds() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(DeviceEvent::DeviceListChanged.rebuild_policy(), t0);
        assert_eq!(core.poll(t0 + ms(200)), Some(Action::CheckDefault));
        assert_eq!(core.resolve_check(Some(id("hp"))), CheckOutcome::Rebuild);
        assert!(!core.has_stream());
    }

    #[test]
    fn removal_of_the_last_device_rebuilds() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(DeviceEvent::DeviceRemoved.rebuild_policy(), t0);
        assert_eq!(core.poll(t0 + ms(200)), Some(Action::CheckDefault));
        assert_eq!(core.resolve_check(None), CheckOutcome::Rebuild);
    }

    #[test]
    fn mixed_burst_rebuilds_without_check() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(DeviceEvent::DeviceListChanged.rebuild_policy(), t0);
        core.on_event(
            DeviceEvent::DefaultOutputChanged.rebuild_policy(),
            t0 + ms(20),
        );
        core.on_event(
            DeviceEvent::DeviceStateChanged.rebuild_policy(),
            t0 + ms(40),
        );
        assert_eq!(core.poll(t0 + ms(239)), None);
        assert_eq!(core.poll(t0 + ms(240)), Some(Action::Rebuild));
    }

    // Thread-level tests.

    fn fast() -> Timings {
        Timings {
            debounce: ms(50),
            retry: ms(50),
            stall: Duration::from_secs(3600),
            startup_grace: Duration::from_secs(3600),
            play_grace: ms(50),
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    fn spawn(factory: &FakeSinkFactory, watcher: &FakeWatcher) -> SupervisorHandle {
        spawn_supervisor(
            factory.clone(),
            watcher.clone(),
            Arc::new(MixerShared::new()),
            fast(),
        )
        .unwrap()
    }

    #[test]
    fn default_output_change_rebuilds_with_new_format() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert_eq!(
            factory.mixer().unwrap().lock().unwrap().sample_rate(),
            48_000
        );

        factory.set_format(44_100, 2);
        factory.set_default(Some("hp"));
        assert!(watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));

        let builds = factory.builds();
        assert_eq!(builds[1].sample_rate, 44_100);
        assert_eq!(builds[1].identity, id("hp"));
        assert_eq!(
            factory.mixer().unwrap().lock().unwrap().sample_rate(),
            44_100
        );
        assert_eq!(factory.overlapping_builds(), 0);
        handle.shutdown();
    }

    #[test]
    fn list_change_with_unchanged_default_does_not_rebuild() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));

        assert!(watcher.send(DeviceEvent::DeviceListChanged));
        assert!(wait_until(WAIT, || factory.identity_queries() == 1));
        thread::sleep(ms(200));
        assert_eq!(factory.attempts(), 1);
        assert_eq!(factory.builds().len(), 1);

        // Control: the same event rebuilds once the default really changed.
        factory.set_default(Some("usb"));
        assert!(watcher.send(DeviceEvent::DeviceListChanged));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));
        assert_eq!(factory.builds()[1].identity, id("usb"));
        handle.shutdown();
    }

    #[test]
    fn input_events_do_not_rebuild_or_query() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(watcher.send(DeviceEvent::DefaultInputChanged));
        // Events are handled in order: once the later list change has been
        // checked, the input event has been handled too.
        assert!(watcher.send(DeviceEvent::DeviceListChanged));
        assert!(wait_until(WAIT, || factory.identity_queries() == 1));
        thread::sleep(ms(200));
        // Only the list change queried the default; nothing rebuilt.
        assert_eq!(factory.identity_queries(), 1);
        assert_eq!(factory.attempts(), 1);
        handle.shutdown();
    }

    fn spawn_with_stall(factory: &FakeSinkFactory, stall: Duration) -> SupervisorHandle {
        spawn_supervisor(
            factory.clone(),
            FakeWatcher::default(),
            Arc::new(MixerShared::new()),
            Timings {
                stall,
                startup_grace: stall,
                ..fast()
            },
        )
        .unwrap()
    }

    #[test]
    fn stalled_callback_rebuilds_on_the_thread() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        // Nothing renders, so the heartbeat never advances.
        let handle = spawn_with_stall(&factory, ms(300));
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(wait_until(WAIT, || factory.builds().len() >= 2));
        assert_eq!(factory.overlapping_builds(), 0);
        handle.shutdown();
    }

    #[test]
    fn live_callback_does_not_rebuild_on_the_thread() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        // Longer than DRAIN_INTERVAL, so the thread samples the heartbeat
        // (seeing it move past the build's baseline) before the deadline.
        let handle = spawn_with_stall(&factory, ms(300));
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        let mixer = factory.mixer().unwrap();
        // Stand in for a running stream callback for well over the stall time.
        let mut out = vec![0.0; 64];
        let start = Instant::now();
        while start.elapsed() < ms(1200) {
            mixer.lock().unwrap().render(&mut out);
            thread::sleep(ms(10));
        }
        assert_eq!(factory.builds().len(), 1);
        handle.shutdown();
    }

    #[test]
    fn voice_keeps_position_across_rebuild() {
        let factory = FakeSinkFactory::new(48_000, 1, "spk");
        let watcher = FakeWatcher::default();
        let shared = Arc::new(MixerShared::new());
        let handle = spawn_supervisor(
            factory.clone(),
            watcher.clone(),
            Arc::clone(&shared),
            fast(),
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.builds().len() == 1));

        let sound = Sound::from_interleaved(vec![0.25; 48_000], 48_000, 1, "long").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        let mixer = factory.mixer().unwrap();
        // Stand in for the stream callback.
        let mut out = vec![0.0; 480];
        mixer.lock().unwrap().render(&mut out);
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6));
        let before: Vec<f64> = mixer.lock().unwrap().voice_positions().collect();
        assert_eq!(before, vec![480.0]);

        factory.set_format(44_100, 2);
        assert!(watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));

        let mut m = mixer.lock().unwrap();
        assert_eq!(m.sample_rate(), 44_100);
        assert_eq!(m.voice_positions().collect::<Vec<_>>(), before);
        let mut out = vec![0.0; 441 * 2];
        m.render(&mut out);
        let after: Vec<f64> = m.voice_positions().collect();
        assert_eq!(after.len(), 1);
        assert!((after[0] - 960.0).abs() < 1e-6, "{after:?}");
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6));
        drop(m);
        handle.shutdown();
    }

    #[test]
    fn failed_builds_are_retried_on_the_thread() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.fail_next(2);
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert_eq!(factory.attempts(), 3);
        handle.shutdown();
    }

    #[test]
    fn plays_queued_without_output_are_dropped() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.fail_next(usize::MAX);
        let watcher = FakeWatcher::default();
        let shared = Arc::new(MixerShared::new());
        let handle = spawn_supervisor(
            factory.clone(),
            watcher.clone(),
            Arc::clone(&shared),
            fast(),
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.attempts() >= 1));
        assert!(!shared.output_active());

        let sound = Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "press").unwrap();
        for _ in 0..5 {
            shared
                .push(Command::Play {
                    sound: sound.clone(),
                    gain: 1.0,
                })
                .unwrap();
        }
        shared.push(Command::SetMasterGain(0.5)).unwrap();
        // The plays are dropped off the realtime thread; the gain applies.
        assert!(wait_until(WAIT, || sound.handle_count() == 1));
        assert!(wait_until(WAIT, || shared.commands_applied() == 1));

        // A device comes back: nothing stale starts, new plays do.
        factory.fail_next(0);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(shared.output_active());
        let mixer = factory.mixer().unwrap();
        assert_eq!(mixer.lock().unwrap().master_gain(), 0.5);
        let mut out = vec![0.0; 64];
        mixer.lock().unwrap().render(&mut out);
        assert_eq!(shared.active_voices(), 0);
        handle.shutdown();
        assert!(!shared.output_active());
    }

    #[test]
    fn rebuild_from_offline_drops_plays_queued_since_the_last_purge() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let shared = Arc::new(MixerShared::new());
        let (sender, _receiver) = mpsc::channel();
        let mut worker = Worker::new(factory.clone(), Arc::clone(&shared), fast(), sender);

        // A press lands after the loop's purge but before the scheduled build.
        let sound = Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "late").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        worker.rebuild();
        assert!(worker.sink.is_some());
        assert_eq!(sound.handle_count(), 1);

        let mixer = factory.mixer().unwrap();
        let mut out = vec![0.0; 64];
        mixer.lock().unwrap().render(&mut out);
        assert_eq!(shared.active_voices(), 0);
        assert!(out.iter().all(|s| *s == 0.0));

        // A rebuild of a running stream keeps queued plays: they are live.
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        worker.rebuild();
        mixer.lock().unwrap().render(&mut out);
        assert_eq!(shared.active_voices(), 1);
        drop(worker);
    }

    #[test]
    fn failed_rebuild_from_live_keeps_plays_while_the_grace_window_is_open() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let shared = Arc::new(MixerShared::new());
        let (sender, _receiver) = mpsc::channel();
        let timings = Timings {
            play_grace: Duration::from_secs(3600),
            ..fast()
        };
        let mut worker = Worker::new(factory.clone(), Arc::clone(&shared), timings, sender);
        worker.rebuild();
        assert!(worker.sink.is_some());
        // A successful build leaves no window behind.
        assert!(!shared.play_grace_open(Instant::now()));

        // The live stream goes away and its replacement fails.
        factory.fail_next(usize::MAX);
        worker.rebuild();
        assert!(worker.sink.is_none());
        assert!(!shared.output_active());
        assert!(shared.accepts_plays(Instant::now()));

        let sound = Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "switch").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        shared.push(Command::SetMasterGain(0.5)).unwrap();
        // Neither the loop's purge nor a retry from offline touches the queue
        // while the window is open.
        worker.purge_offline();
        worker.rebuild();
        assert_eq!(sound.handle_count(), 2);
        assert_eq!(shared.commands_applied(), 0);

        // The replacement stream applies both, in order.
        factory.fail_next(0);
        worker.rebuild();
        assert!(worker.sink.is_some());
        assert!(!shared.play_grace_open(Instant::now()));
        let mixer = factory.mixer().unwrap();
        let mut out = vec![0.0; 64];
        mixer.lock().unwrap().render(&mut out);
        assert_eq!(shared.active_voices(), 1);
        assert_eq!(shared.commands_applied(), 2);
        assert_eq!(mixer.lock().unwrap().master_gain(), 0.5);
        drop(worker);
    }

    #[test]
    fn closed_grace_window_drops_plays_on_the_offline_purge() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let shared = Arc::new(MixerShared::new());
        let (sender, _receiver) = mpsc::channel();
        let timings = Timings {
            play_grace: Duration::ZERO,
            ..fast()
        };
        let mut worker = Worker::new(factory.clone(), Arc::clone(&shared), timings, sender);
        worker.rebuild();
        factory.fail_next(usize::MAX);
        worker.rebuild();
        assert!(!shared.accepts_plays(Instant::now()));

        let sound = Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "late").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        worker.purge_offline();
        assert_eq!(sound.handle_count(), 1);
        drop(worker);
    }

    #[test]
    fn initial_start_and_shutdown_open_no_grace_window() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.fail_next(usize::MAX);
        let shared = Arc::new(MixerShared::new());
        let handle = spawn_supervisor(
            factory.clone(),
            FakeWatcher::default(),
            Arc::clone(&shared),
            Timings {
                play_grace: Duration::from_secs(3600),
                ..fast()
            },
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.attempts() >= 2));
        assert!(!shared.accepts_plays(Instant::now()));

        factory.fail_next(0);
        assert!(wait_until(WAIT, || shared.output_active()));
        handle.shutdown();
        assert!(!shared.accepts_plays(Instant::now()));
    }

    #[test]
    fn stream_error_rebuilds_on_the_thread() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(factory.send_stream_error(StreamErrorKind::Other));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));
        assert_eq!(factory.overlapping_builds(), 0);
        handle.shutdown();
    }

    #[test]
    fn device_changed_stream_error_still_rebuilds() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(factory.send_stream_error(StreamErrorKind::DeviceChanged));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));
        assert_eq!(factory.overlapping_builds(), 0);
        assert_eq!(factory.live_sinks(), 1);
        handle.shutdown();
    }

    #[test]
    fn device_changed_logs_at_debug_and_others_at_warn() {
        assert_eq!(
            stream_error_level(StreamErrorKind::DeviceChanged),
            Level::DEBUG
        );
        assert_eq!(stream_error_level(StreamErrorKind::Other), Level::WARN);
    }

    #[test]
    fn supervisor_drains_garbage() {
        let factory = FakeSinkFactory::new(48_000, 1, "spk");
        let watcher = FakeWatcher::default();
        let shared = Arc::new(MixerShared::new());
        let handle = spawn_supervisor(
            factory.clone(),
            watcher.clone(),
            Arc::clone(&shared),
            fast(),
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.builds().len() == 1));

        let sound = Sound::from_interleaved(vec![0.5; 16], 48_000, 1, "short").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        let mixer = factory.mixer().unwrap();
        let mut out = vec![0.0; 64];
        mixer.lock().unwrap().render(&mut out);
        assert_eq!(shared.active_voices(), 0);
        assert!(wait_until(WAIT, || sound.handle_count() == 1));
        handle.shutdown();
    }

    #[test]
    fn shutdown_joins_promptly_and_cleans_up() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn_supervisor(
            factory.clone(),
            watcher.clone(),
            Arc::new(MixerShared::new()),
            Timings::default(),
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert_eq!(watcher.started(), 1);
        assert_eq!(factory.live_sinks(), 1);

        let start = Instant::now();
        handle.shutdown();
        assert!(start.elapsed() < ms(500), "{:?}", start.elapsed());
        assert_eq!(factory.live_sinks(), 0);
        assert_eq!(watcher.guards_dropped(), 1);
        assert!(!watcher.send(DeviceEvent::DefaultOutputChanged));
    }

    #[test]
    fn dropping_the_handle_shuts_down() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        drop(handle);
        assert_eq!(factory.live_sinks(), 0);
        assert_eq!(watcher.guards_dropped(), 1);
    }

    #[test]
    fn watcher_failure_is_not_fatal() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        watcher.fail_start();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.builds().len() == 1));
        assert!(factory.send_stream_error(StreamErrorKind::Other));
        assert!(wait_until(WAIT, || factory.builds().len() == 2));
        handle.shutdown();
    }
}
