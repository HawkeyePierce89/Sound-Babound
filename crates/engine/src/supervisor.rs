//! The stream supervisor.
//!
//! Every stream the engine runs (see [`StreamRole`]) gets its own instance of
//! one generic supervisor, [`spawn_supervisor`]; only the [`StreamFactory`],
//! the [`Endpoint`] and the role's rebuild policy differ. Each instance runs
//! on a dedicated thread that owns its stream and starts its own device
//! watcher. It listens for device events and stream errors on one channel,
//! debounces them, decides whether they affect its stream and rebuilds the
//! stream on the device the role targets now when they do. Voices keep
//! playing across a rebuild of an output because the mixer, and with it every
//! voice position, outlives the stream.
//!
//! The decision logic is the pure [`SupervisorCore`], driven by explicit
//! [`Instant`]s so it can be tested with a fake clock. [`spawn_supervisor`]
//! wraps it in the thread.
//!
//! # Rebuild policy
//!
//! A rebuild is an audible gap, so not every event triggers one. Each event
//! is classified for the supervisor's role by `DeviceEvent::rebuild_policy`.
//! After the trailing-edge debounce window:
//!
//! - a [`RebuildPolicy::Always`] event or a stream error rebuilds;
//! - a window holding only [`RebuildPolicy::IfIdentityChanged`] events yields
//!   [`Action::CheckIdentity`]: the identity of the device the role would pick
//!   now is re-queried and the stream is rebuilt only if it differs from the
//!   identity the stream was built on;
//! - [`RebuildPolicy::Ignore`] events do nothing.
//!
//! While no stream is running (initial start, retry backoff) every
//! non-ignored event rebuilds. A failed build is retried after a backoff, and
//! a stream whose callback heartbeat stops advancing is rebuilt. Repeated
//! build failures are logged through [`FailureLog`], so a missing device does
//! not warn once a second.
//!
//! # Status
//!
//! Each supervisor publishes a [`StreamStatus`] for UI reads. It sits behind a
//! mutex that only the supervisor thread and control threads touch; no
//! stream callback ever does.
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

use crate::error::{BuildError, BuildErrorKind};
use crate::sink::{ActiveStream, DeviceIdentity, Endpoint, StreamFactory, StreamInfo, StreamRole};
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
    /// the expected result of switching devices, not a fault.
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

/// What a supervisor's stream is doing, for UI reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamStatus {
    /// The first build, or a rebuild of a stream that was running, is in
    /// progress.
    Starting,
    /// A stream runs on this device.
    Running(StreamInfo),
    /// The last build failed; it is retried after a backoff. A rebuild
    /// started from this state keeps it until a build succeeds.
    Failed {
        /// The build error's message.
        error: String,
        /// The build error's kind.
        kind: BuildErrorKind,
    },
}

/// Throttles the build-failure log of one supervisor.
///
/// A device that is missing for minutes fails a build every retry. Only the
/// first failure of a streak, and a failure whose message differs from the
/// previous one, are warnings; repeats are debug. The first success after a
/// streak is logged at info.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureLog {
    streak: usize,
    last: Option<String>,
}

impl FailureLog {
    /// Record a failed build with `message`; returns the level to log it at.
    pub fn on_failure(&mut self, message: &str) -> Level {
        self.streak += 1;
        if self.last.as_deref() == Some(message) {
            return Level::DEBUG;
        }
        self.last = Some(message.to_owned());
        Level::WARN
    }

    /// Record a successful build. Returns the number of failures before it
    /// when it ends a streak (log the recovery at info), otherwise `None`.
    pub fn on_success(&mut self) -> Option<usize> {
        self.last = None;
        match std::mem::take(&mut self.streak) {
            0 => None,
            failures => Some(failures),
        }
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
    /// How long a shutdown waits for the supervisor thread. A device query
    /// with no timeout of its own (a PulseAudio server that stops answering)
    /// can block the thread inside a build; past this the thread is left to
    /// finish on its own, so stopping the mic or the app never hangs. Longer
    /// than the poll watcher's stop timeout, so a slow watcher stop still
    /// joins.
    pub stop_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            debounce: Duration::from_millis(200),
            retry: Duration::from_secs(1),
            stall: Duration::from_secs(2),
            startup_grace: Duration::from_secs(10),
            play_grace: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(3),
        }
    }
}

/// What the supervisor should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Drop the stream and build a new one on the device the role targets.
    Rebuild,
    /// Re-query the identity of the device a build would pick now and pass
    /// it to [`SupervisorCore::resolve_check`].
    CheckIdentity,
}

/// The result of [`SupervisorCore::resolve_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The target device changed: rebuild.
    Rebuild,
    /// The event did not affect the active stream: drop it.
    Drop,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    deadline: Instant,
    /// Holds an `Always` event or a stream error.
    force: bool,
}

#[derive(Debug, Clone)]
struct LiveStream {
    identity: DeviceIdentity,
    built_at: Instant,
    /// Whether the heartbeat has moved past the value read before the build.
    started: bool,
    /// The baseline until `started`, then the last value seen.
    last_beat: u64,
    /// When `last_beat` was first seen; meaningful once `started`.
    last_beat_at: Instant,
}

impl LiveStream {
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
    stream: Option<LiveStream>,
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
            RebuildPolicy::IfIdentityChanged => self.open_window(false, now),
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
                self.stream = Some(LiveStream {
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

    /// Record the callback heartbeat as sampled at `now`.
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
            return Some(Action::CheckIdentity);
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

    /// Decide an [`Action::CheckIdentity`] given the identity of the device a
    /// build would pick now: rebuild if it differs from the identity the
    /// stream was built on (including `None` against `Some`), otherwise drop
    /// the event.
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
    role: StreamRole,
    sender: Sender<SupervisorMsg>,
    status: Arc<Mutex<StreamStatus>>,
    thread: Option<JoinHandle<()>>,
    /// Disconnects when the thread ends. In a mutex only so the handle (and
    /// the engine) stays `Sync`; `stop` has `&mut self` and never locks it.
    done: Mutex<Receiver<()>>,
    stop_timeout: Duration,
    /// Build failures logged at warn, so tests can check the throttle.
    #[cfg(test)]
    failure_warnings: Arc<std::sync::atomic::AtomicUsize>,
}

impl SupervisorHandle {
    /// What the supervised stream is doing now.
    pub fn status(&self) -> StreamStatus {
        lock_status(&self.status).clone()
    }

    /// How many build failures were logged at warn (see [`FailureLog`]).
    #[cfg(test)]
    pub fn failure_warnings(&self) -> usize {
        self.failure_warnings
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Stop the stream, unregister the watcher and join the thread. Waits
    /// at most [`Timings::stop_timeout`]: a thread still blocked in a device
    /// query then is left to stop on its own once the query returns (the
    /// shutdown message stays queued for it). The engine stops its handles
    /// with [`SupervisorHandle::shutdown_all`]; this is for tests.
    #[cfg(test)]
    pub fn shutdown(mut self) {
        self.stop();
    }

    /// Shut down every handle in `handles` together (stop the stream,
    /// unregister the watcher, join the thread): each thread is told to stop
    /// first, then all are waited for against one deadline, so they
    /// take at most one [`Timings::stop_timeout`] between them rather than
    /// one each.
    ///
    /// Returns the threads that did not stop in time: each is still blocked
    /// in a device query, may still hold its stream, and stops on its own
    /// once the query returns. [`JoinHandle::is_finished`] tells when.
    pub fn shutdown_all(
        handles: impl IntoIterator<Item = SupervisorHandle>,
    ) -> Vec<JoinHandle<()>> {
        let mut handles: Vec<SupervisorHandle> = handles.into_iter().collect();
        for handle in &handles {
            handle.signal();
        }
        let timeout = handles
            .iter()
            .map(|handle| handle.stop_timeout)
            .max()
            .unwrap_or_default();
        let deadline = Instant::now() + timeout;
        handles
            .iter_mut()
            .filter_map(|handle| handle.wait(deadline))
            .collect()
    }

    fn stop(&mut self) {
        self.signal();
        // A thread left running stops on its own; nobody waits for it.
        let _ = self.wait(Instant::now() + self.stop_timeout);
    }

    /// Tell a running thread to stop.
    fn signal(&self) {
        if self.thread.is_some() {
            // A send error means the thread already exited; it is joined
            // anyway.
            let _ = self.sender.send(SupervisorMsg::Shutdown);
        }
    }

    /// Join the thread if it ends by `deadline`; otherwise leave it and
    /// return it.
    fn wait(&mut self, deadline: Instant) -> Option<JoinHandle<()>> {
        let thread = self.thread.take()?;
        let done = self.done.get_mut().unwrap_or_else(PoisonError::into_inner);
        let left = deadline.saturating_duration_since(Instant::now());
        if let Err(RecvTimeoutError::Timeout) = done.recv_timeout(left) {
            warn!(
                role = %self.role,
                "supervisor did not stop in time (a device query is not returning); \
                 leaving its thread to stop on its own"
            );
            return Some(thread);
        }
        if thread.join().is_err() {
            error!("supervisor thread panicked");
        }
        None
    }
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn lock_status(status: &Mutex<StreamStatus>) -> std::sync::MutexGuard<'_, StreamStatus> {
    status.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Start a supervisor thread for `factory`'s role.
///
/// The thread starts its own `watcher`, builds the first stream on
/// `endpoint` immediately and then supervises it until shutdown.
pub(crate) fn spawn_supervisor<F, E, W>(
    factory: F,
    endpoint: E,
    watcher: W,
    timings: Timings,
) -> std::io::Result<SupervisorHandle>
where
    F: StreamFactory<Target = E::Target>,
    E: Endpoint,
    W: DeviceWatcher,
{
    let (sender, receiver) = mpsc::channel();
    let (done_tx, done) = mpsc::channel::<()>();
    let role = factory.role();
    let status = Arc::new(Mutex::new(StreamStatus::Starting));
    let thread_sender = sender.clone();
    let thread_status = Arc::clone(&status);
    #[cfg(test)]
    let failure_warnings = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    #[cfg(test)]
    let thread_warnings = Arc::clone(&failure_warnings);
    let thread = thread::Builder::new()
        .name(format!("babound-{role}"))
        .spawn(move || {
            // Dropped when the thread ends, even by a panic.
            let _done = done_tx;
            let worker = Worker::new(factory, endpoint, timings, thread_sender, thread_status);
            #[cfg(test)]
            let worker = Worker {
                failure_warnings: thread_warnings,
                ..worker
            };
            worker.run(watcher, receiver);
        })?;
    Ok(SupervisorHandle {
        role,
        sender,
        status,
        thread: Some(thread),
        done: Mutex::new(done),
        stop_timeout: timings.stop_timeout,
        #[cfg(test)]
        failure_warnings,
    })
}

/// The supervisor thread's state.
struct Worker<F, E> {
    factory: F,
    endpoint: E,
    role: StreamRole,
    sender: Sender<SupervisorMsg>,
    status: Arc<Mutex<StreamStatus>>,
    core: SupervisorCore,
    failures: FailureLog,
    play_grace: Duration,
    stream: Option<Box<dyn ActiveStream>>,
    /// Shared with [`SupervisorHandle::failure_warnings`].
    #[cfg(test)]
    failure_warnings: Arc<std::sync::atomic::AtomicUsize>,
}

impl<F, E> Worker<F, E>
where
    F: StreamFactory<Target = E::Target>,
    E: Endpoint,
{
    fn new(
        factory: F,
        endpoint: E,
        timings: Timings,
        sender: Sender<SupervisorMsg>,
        status: Arc<Mutex<StreamStatus>>,
    ) -> Worker<F, E> {
        Worker {
            role: factory.role(),
            factory,
            endpoint,
            sender,
            status,
            core: SupervisorCore::new(timings, Instant::now()),
            failures: FailureLog::default(),
            play_grace: timings.play_grace,
            stream: None,
            #[cfg(test)]
            failure_warnings: Arc::default(),
        }
    }

    fn run<W: DeviceWatcher>(mut self, mut watcher: W, receiver: Receiver<SupervisorMsg>) {
        let role = self.role;
        let guard = match watcher.start(EventSender::new(self.sender.clone())) {
            Ok(guard) => Some(guard),
            Err(err) => {
                warn!(%role, error = %err, "device watcher unavailable; relying on stream errors");
                None
            }
        };

        loop {
            let now = Instant::now();
            self.core.on_heartbeat(self.endpoint.heartbeat(), now);
            match self.core.poll(now) {
                Some(Action::Rebuild) => self.rebuild(),
                Some(Action::CheckIdentity) => self.check_identity(),
                None => {}
            }
            if self.stream.is_none() {
                self.purge_offline();
            }
            self.endpoint.drain_garbage();

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
                        debug!(%role, error = %message, "device changed; rebuilding");
                    } else {
                        warn!(%role, error = %message, "stream error");
                    }
                    self.core.on_stream_error(Instant::now());
                }
                Ok(SupervisorMsg::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }

        drop(self.stream.take());
        self.endpoint.close_play_grace();
        self.endpoint.set_active(false);
        drop(guard);
        self.endpoint.drain_garbage();
        debug!(%role, "supervisor stopped");
    }

    fn on_device_event(&mut self, event: DeviceEvent) {
        let role = self.role;
        let policy = event.rebuild_policy(role);
        if policy == RebuildPolicy::Ignore {
            debug!(%role, ?event, "device event ignored");
        } else {
            debug!(%role, ?event, ?policy, "device event");
        }
        self.core.on_event(policy, Instant::now());
    }

    fn check_identity(&mut self) {
        let role = self.role;
        let current = self.factory.current_identity();
        match self.core.resolve_check(current.clone()) {
            CheckOutcome::Rebuild => {
                info!(
                    %role,
                    device = current.as_ref().map_or("<none>", DeviceIdentity::as_str),
                    "target device changed; rebuilding"
                );
                self.rebuild();
            }
            CheckOutcome::Drop => {
                debug!(%role, "device change did not affect the stream; ignored");
            }
        }
    }

    /// With no stream: drop the queued plays unless the play grace window
    /// from a live teardown is still open, in which case the queue is kept
    /// in order for the replacement stream.
    fn purge_offline(&mut self) {
        let dropped = self.endpoint.purge_offline(Instant::now());
        if dropped > 0 {
            info!(role = %self.role, dropped, "no stream; dropped queued plays");
        }
    }

    fn set_status(&self, status: StreamStatus) {
        *lock_status(&self.status) = status;
    }

    /// Drop the stream, then build a new one. The factory configures the
    /// target while no stream exists, so the callback never contends for it.
    ///
    /// When a stream existed, the queue is kept on purpose: its plays were
    /// accepted while that stream ran, and the new stream applies them. The
    /// play grace window opens before the stream is marked inactive, so plays
    /// stay accepted without a gap. When no stream existed, plays queued
    /// since the last loop's purge are dropped first, so the new stream never
    /// starts a press made offline, unless the grace window is still open.
    fn rebuild(&mut self) {
        match self.stream.take() {
            Some(stream) => {
                self.endpoint
                    .open_play_grace(Instant::now() + self.play_grace);
                drop(stream);
                self.set_status(StreamStatus::Starting);
            }
            None => self.purge_offline(),
        }
        self.endpoint.set_active(false);
        // Read before the build, so a callback that runs before the next
        // sample still counts as the stream starting.
        let baseline = self.endpoint.heartbeat();
        match self
            .factory
            .build(self.endpoint.target(), self.sender.clone())
        {
            Ok((stream, info)) => self.on_built(stream, info, baseline),
            Err(err) => self.on_failed(&err, baseline),
        }
    }

    fn on_built(&mut self, stream: Box<dyn ActiveStream>, info: StreamInfo, baseline: u64) {
        let role = self.role;
        if let Some(failures) = self.failures.on_success() {
            info!(%role, failures, "stream build recovered");
        }
        info!(
            %role,
            device = %info.device_name,
            rate = info.sample_rate,
            channels = info.channels,
            format = %info.sample_format,
            "stream started"
        );
        self.stream = Some(stream);
        self.endpoint.set_active(true);
        self.core
            .on_build_result(Some(info.identity.clone()), baseline, Instant::now());
        self.set_status(StreamStatus::Running(info));
    }

    fn on_failed(&mut self, err: &BuildError, baseline: u64) {
        let role = self.role;
        let message = err.to_string();
        if self.failures.on_failure(&message) == Level::WARN {
            #[cfg(test)]
            self.failure_warnings
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            warn!(%role, error = %message, "stream build failed; will retry");
        } else {
            debug!(%role, error = %message, "stream build failed again; will retry");
        }
        self.core.on_build_result(None, baseline, Instant::now());
        self.set_status(StreamStatus::Failed {
            error: message,
            kind: err.kind(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::{Command, MixerShared};
    use crate::sink::MixerEndpoint;
    use crate::sound::Sound;
    use crate::test_support::{FakeSinkFactory, FakeWatcher, wait_until};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn id(name: &str) -> DeviceIdentity {
        DeviceIdentity::new(name)
    }

    /// A fresh status cell for a worker built directly in a test.
    fn status() -> Arc<Mutex<StreamStatus>> {
        Arc::new(Mutex::new(StreamStatus::Starting))
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
                Some(Action::CheckIdentity) => {
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
        core.on_event(RebuildPolicy::IfIdentityChanged, t0 + ms(100));
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
                DeviceEvent::DefaultInputChanged.rebuild_policy(StreamRole::Monitor),
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
            DeviceEvent::DeviceListChanged.rebuild_policy(StreamRole::Monitor),
            t0 + ms(100),
        );
        assert_eq!(core.poll(t0 + ms(299)), None);
        assert_eq!(core.poll(t0 + ms(300)), Some(Action::CheckIdentity));
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
        core.on_event(
            DeviceEvent::DeviceListChanged.rebuild_policy(StreamRole::Monitor),
            t0,
        );
        assert_eq!(core.poll(t0 + ms(200)), Some(Action::CheckIdentity));
        assert_eq!(core.resolve_check(Some(id("hp"))), CheckOutcome::Rebuild);
        assert!(!core.has_stream());
    }

    #[test]
    fn removal_of_the_last_device_rebuilds() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(
            DeviceEvent::DeviceRemoved.rebuild_policy(StreamRole::Monitor),
            t0,
        );
        assert_eq!(core.poll(t0 + ms(200)), Some(Action::CheckIdentity));
        assert_eq!(core.resolve_check(None), CheckOutcome::Rebuild);
    }

    #[test]
    fn mixed_burst_rebuilds_without_check() {
        let t0 = Instant::now();
        let mut core = running(t0, "spk");
        core.on_event(
            DeviceEvent::DeviceListChanged.rebuild_policy(StreamRole::Monitor),
            t0,
        );
        core.on_event(
            DeviceEvent::DefaultOutputChanged.rebuild_policy(StreamRole::Monitor),
            t0 + ms(20),
        );
        core.on_event(
            DeviceEvent::DeviceStateChanged.rebuild_policy(StreamRole::Monitor),
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
            stop_timeout: Duration::from_secs(30),
        }
    }

    const WAIT: Duration = Duration::from_secs(5);

    fn spawn(factory: &FakeSinkFactory, watcher: &FakeWatcher) -> SupervisorHandle {
        spawn_supervisor(
            factory.clone(),
            MixerEndpoint::new(Arc::new(MixerShared::new())),
            watcher.clone(),
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
            MixerEndpoint::new(Arc::new(MixerShared::new())),
            FakeWatcher::default(),
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
            MixerEndpoint::new(Arc::clone(&shared)),
            watcher.clone(),
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
            MixerEndpoint::new(Arc::clone(&shared)),
            watcher.clone(),
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
        let mut worker = Worker::new(
            factory.clone(),
            MixerEndpoint::new(Arc::clone(&shared)),
            fast(),
            sender,
            status(),
        );

        // A press lands after the loop's purge but before the scheduled build.
        let sound = Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "late").unwrap();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        worker.rebuild();
        assert!(worker.stream.is_some());
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
        let mut worker = Worker::new(
            factory.clone(),
            MixerEndpoint::new(Arc::clone(&shared)),
            timings,
            sender,
            status(),
        );
        worker.rebuild();
        assert!(worker.stream.is_some());
        // A successful build leaves no window behind.
        assert!(!shared.play_grace_open(Instant::now()));

        // The live stream goes away and its replacement fails.
        factory.fail_next(usize::MAX);
        worker.rebuild();
        assert!(worker.stream.is_none());
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
        assert!(worker.stream.is_some());
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
        let mut worker = Worker::new(
            factory.clone(),
            MixerEndpoint::new(Arc::clone(&shared)),
            timings,
            sender,
            status(),
        );
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
            MixerEndpoint::new(Arc::clone(&shared)),
            FakeWatcher::default(),
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
            MixerEndpoint::new(Arc::clone(&shared)),
            watcher.clone(),
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
            MixerEndpoint::new(Arc::new(MixerShared::new())),
            watcher.clone(),
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
    fn shutdown_gives_up_on_a_hung_build_and_the_thread_stops_later() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let watcher = FakeWatcher::default();
        factory.hold_builds();
        let handle = spawn_supervisor(
            factory.clone(),
            MixerEndpoint::new(Arc::new(MixerShared::new())),
            watcher.clone(),
            Timings {
                stop_timeout: ms(100),
                ..fast()
            },
        )
        .unwrap();
        assert!(wait_until(WAIT, || factory.building()));

        let start = Instant::now();
        handle.shutdown();
        let waited = start.elapsed();
        assert!(waited >= ms(100) && waited < ms(1000), "{waited:?}");
        assert_eq!(watcher.guards_dropped(), 0);

        // Once the build returns, the left-behind thread stops on its own.
        factory.release_builds();
        assert!(wait_until(WAIT, || watcher.guards_dropped() == 1));
        assert!(wait_until(WAIT, || factory.live_sinks() == 0));
    }

    #[test]
    fn shutting_down_hung_supervisors_together_costs_one_timeout() {
        let watcher = FakeWatcher::default();
        let factories: Vec<_> = (0..3)
            .map(|_| FakeSinkFactory::new(48_000, 2, "spk"))
            .collect();
        let handles: Vec<_> = factories
            .iter()
            .map(|factory| {
                factory.hold_builds();
                spawn_supervisor(
                    factory.clone(),
                    MixerEndpoint::new(Arc::new(MixerShared::new())),
                    watcher.clone(),
                    Timings {
                        stop_timeout: ms(300),
                        ..fast()
                    },
                )
                .unwrap()
            })
            .collect();
        for factory in &factories {
            assert!(wait_until(WAIT, || factory.building()));
        }

        let start = Instant::now();
        let left = SupervisorHandle::shutdown_all(handles);
        let waited = start.elapsed();
        // One timeout, not three.
        assert!(waited >= ms(300) && waited < ms(800), "{waited:?}");
        // The hung threads are handed back, still running.
        assert_eq!(left.len(), 3);
        assert!(left.iter().all(|thread| !thread.is_finished()));

        for factory in &factories {
            factory.release_builds();
        }
        assert!(wait_until(WAIT, || watcher.guards_dropped() == 3));
        for factory in &factories {
            assert!(wait_until(WAIT, || factory.live_sinks() == 0));
        }
        assert!(wait_until(WAIT, || left
            .iter()
            .all(|thread| thread.is_finished())));
    }

    #[test]
    fn shutting_down_together_joins_threads_that_stop() {
        let watcher = FakeWatcher::default();
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        let handles: Vec<_> = (0..2)
            .map(|_| {
                spawn_supervisor(
                    factory.clone(),
                    MixerEndpoint::new(Arc::new(MixerShared::new())),
                    watcher.clone(),
                    fast(),
                )
                .unwrap()
            })
            .collect();
        assert!(wait_until(WAIT, || factory.live_sinks() == 2));
        assert!(SupervisorHandle::shutdown_all(handles).is_empty());
        // Joined: everything is torn down by the time it returns.
        assert_eq!(factory.live_sinks(), 0);
        assert_eq!(watcher.guards_dropped(), 2);
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

    // Build-failure log throttling.

    #[test]
    fn failure_log_warns_once_per_streak() {
        let mut log = FailureLog::default();
        assert_eq!(log.on_success(), None);
        assert_eq!(log.on_failure("no output device"), Level::WARN);
        assert_eq!(log.on_failure("no output device"), Level::DEBUG);
        assert_eq!(log.on_failure("no output device"), Level::DEBUG);
        assert_eq!(log.on_success(), Some(3));
        // A new streak warns again, even with the same message.
        assert_eq!(log.on_failure("no output device"), Level::WARN);
        assert_eq!(log.on_success(), Some(1));
        assert_eq!(log.on_success(), None);
    }

    #[test]
    fn failure_log_warns_when_the_error_changes() {
        let mut log = FailureLog::default();
        assert_eq!(log.on_failure("no output device"), Level::WARN);
        assert_eq!(log.on_failure("audio backend error: busy"), Level::WARN);
        assert_eq!(log.on_failure("audio backend error: busy"), Level::DEBUG);
        assert_eq!(log.on_failure("no output device"), Level::WARN);
        assert_eq!(log.on_success(), Some(4));
    }

    // Per-role policies in the core.

    #[test]
    fn mic_sink_core_ignores_default_output_changes() {
        let t0 = Instant::now();
        let mut core = running(t0, "cable");
        for event in [
            DeviceEvent::DefaultOutputChanged,
            DeviceEvent::DefaultInputChanged,
        ] {
            core.on_event(event.rebuild_policy(StreamRole::MicSink), t0);
        }
        let mut beat = 0;
        assert_eq!(
            drive(&mut core, t0, t0 + ms(3000), Some("cable"), &mut beat),
            (0, 0)
        );
        core.on_event(
            DeviceEvent::DeviceRemoved.rebuild_policy(StreamRole::MicSink),
            t0 + ms(3000),
        );
        assert_eq!(core.poll(t0 + ms(3200)), Some(Action::CheckIdentity));
    }

    #[test]
    fn capture_core_rebuilds_on_default_input_change() {
        let t0 = Instant::now();
        let mut core = running(t0, "mic");
        core.on_event(
            DeviceEvent::DefaultOutputChanged.rebuild_policy(StreamRole::Capture),
            t0,
        );
        assert_eq!(core.poll(t0 + ms(300)), None);
        core.on_event(
            DeviceEvent::DefaultInputChanged.rebuild_policy(StreamRole::Capture),
            t0 + ms(300),
        );
        assert_eq!(core.poll(t0 + ms(500)), Some(Action::Rebuild));
    }

    // Status.

    #[test]
    fn status_goes_from_starting_to_running_and_back_on_rebuild() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.hold_builds();
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.building()));
        assert_eq!(handle.status(), StreamStatus::Starting);

        factory.release_builds();
        assert!(wait_until(WAIT, || matches!(
            handle.status(),
            StreamStatus::Running(_)
        )));
        let StreamStatus::Running(info) = handle.status() else {
            unreachable!()
        };
        assert_eq!(info.identity, id("spk"));

        // A rebuild of a running stream is Starting until the new build returns.
        factory.hold_builds();
        factory.set_default(Some("hp"));
        assert!(watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(WAIT, || factory.building()));
        assert_eq!(handle.status(), StreamStatus::Starting);
        factory.release_builds();
        assert!(wait_until(WAIT, || matches!(
            handle.status(),
            StreamStatus::Running(ref info) if info.identity == id("hp")
        )));
        handle.shutdown();
    }

    #[test]
    fn status_reports_a_failed_build_until_one_succeeds() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.set_default(None);
        let watcher = FakeWatcher::default();
        let handle = spawn(&factory, &watcher);
        assert!(wait_until(WAIT, || factory.attempts() >= 1));
        let failed = StreamStatus::Failed {
            error: BuildError::NoDevice.to_string(),
            kind: BuildErrorKind::NoDevice,
        };
        assert!(wait_until(WAIT, || handle.status() == failed));

        // Retries hold the build at the gate; the status stays Failed while
        // they run, so a UI does not flicker through Starting.
        factory.hold_builds();
        assert!(wait_until(WAIT, || factory.building()));
        assert_eq!(handle.status(), failed);
        factory.set_default(Some("spk"));
        factory.release_builds();
        assert!(wait_until(WAIT, || matches!(
            handle.status(),
            StreamStatus::Running(_)
        )));
        handle.shutdown();
    }

    #[test]
    fn status_reports_the_error_kind() {
        let factory = FakeSinkFactory::new(48_000, 2, "spk");
        factory.fail_next(usize::MAX);
        let handle = spawn(&factory, &FakeWatcher::default());
        assert!(wait_until(WAIT, || matches!(
            handle.status(),
            StreamStatus::Failed {
                kind: BuildErrorKind::Backend,
                ..
            }
        )));
        handle.shutdown();
    }

    // Several supervisors.

    #[test]
    fn one_fake_watcher_feeds_two_supervisors() {
        let monitor = FakeSinkFactory::new(48_000, 2, "spk");
        let mic = FakeSinkFactory::for_role(StreamRole::MicSink, 48_000, 2, "cable");
        let watcher = FakeWatcher::default();
        let monitor_handle = spawn(&monitor, &watcher);
        let mic_handle = spawn(&mic, &watcher);
        assert!(wait_until(WAIT, || monitor.builds().len() == 1));
        assert!(wait_until(WAIT, || mic.builds().len() == 1));
        assert_eq!(watcher.started(), 2);

        // A default-output change rebuilds the monitor only.
        monitor.set_default(Some("hp"));
        assert!(watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(WAIT, || monitor.builds().len() == 2));
        // A later list change reaches the mic sink: once it has been checked,
        // the earlier default-output event has been handled too.
        assert!(watcher.send(DeviceEvent::DeviceListChanged));
        assert!(wait_until(WAIT, || mic.identity_queries() == 1));
        assert!(wait_until(WAIT, || monitor.identity_queries() == 1));
        thread::sleep(ms(200));
        assert_eq!(mic.attempts(), 1);
        assert_eq!(monitor.builds().len(), 2);
        assert_eq!(mic.live_sinks(), 1);
        assert_eq!(monitor.live_sinks(), 1);

        // The mic device goes away: only the mic sink rebuilds (and fails).
        mic.set_default(None);
        assert!(watcher.send(DeviceEvent::DeviceRemoved));
        assert!(wait_until(WAIT, || mic.attempts() >= 2));
        assert!(wait_until(WAIT, || matches!(
            mic_handle.status(),
            StreamStatus::Failed {
                kind: BuildErrorKind::NoDevice,
                ..
            }
        )));
        assert_eq!(monitor.builds().len(), 2);
        assert_eq!(monitor.live_sinks(), 1);
        assert!(matches!(monitor_handle.status(), StreamStatus::Running(_)));

        mic_handle.shutdown();
        assert_eq!(watcher.guards_dropped(), 1);
        // The monitor still hears events after the mic sink is gone.
        assert!(watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(WAIT, || monitor.builds().len() == 3));
        monitor_handle.shutdown();
        assert_eq!(watcher.guards_dropped(), 2);
        assert!(!watcher.send(DeviceEvent::DefaultOutputChanged));
    }

    #[test]
    fn supervisor_threads_are_named_by_role() {
        let factory = FakeSinkFactory::for_role(StreamRole::Capture, 48_000, 1, "mic");
        let handle = spawn(&factory, &FakeWatcher::default());
        let name = handle
            .thread
            .as_ref()
            .and_then(|t| t.thread().name().map(str::to_owned));
        assert_eq!(name.as_deref(), Some("babound-capture"));
        handle.shutdown();
    }
}
