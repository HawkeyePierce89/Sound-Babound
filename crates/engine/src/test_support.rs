//! Fakes shared by the engine's tests.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{BuildError, WatcherError};
use crate::mixer::Mixer;
use crate::sink::{
    ActiveStream, BuiltStream, DeviceIdentity, StreamFactory, StreamInfo, StreamRole,
};
use crate::supervisor::{StreamErrorKind, SupervisorMsg};
use crate::watcher::{DeviceEvent, DeviceWatcher, EventSender, WatcherGuard};

/// Poll `condition` every 5 ms until it holds or `timeout` passes.
pub(crate) fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Default)]
struct WatcherState {
    /// One sender per started supervisor.
    senders: Vec<EventSender>,
    started: usize,
    guards_dropped: usize,
    fail: bool,
}

/// A watcher whose events are sent by the test. Clones share state, so one
/// fake handed to several supervisors broadcasts to all of them, as the OS
/// does to several platform watchers.
#[derive(Clone, Default)]
pub(crate) struct FakeWatcher(Arc<Mutex<WatcherState>>);

impl FakeWatcher {
    /// Deliver an event to every started supervisor, as if the OS reported
    /// it. False if none is started or every one is gone.
    pub(crate) fn send(&self, event: DeviceEvent) -> bool {
        let mut state = lock(&self.0);
        state.senders.retain(|s| s.send(event));
        !state.senders.is_empty()
    }

    /// Make `start` fail.
    pub(crate) fn fail_start(&self) {
        lock(&self.0).fail = true;
    }

    pub(crate) fn started(&self) -> usize {
        lock(&self.0).started
    }

    pub(crate) fn guards_dropped(&self) -> usize {
        lock(&self.0).guards_dropped
    }
}

struct FakeGuard(Arc<Mutex<WatcherState>>);

impl WatcherGuard for FakeGuard {}

impl Drop for FakeGuard {
    fn drop(&mut self) {
        lock(&self.0).guards_dropped += 1;
    }
}

impl DeviceWatcher for FakeWatcher {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        let mut state = lock(&self.0);
        if state.fail {
            return Err(WatcherError("fake watcher failure".into()));
        }
        state.started += 1;
        state.senders.push(events);
        Ok(Box::new(FakeGuard(Arc::clone(&self.0))))
    }
}

struct SinkState {
    role: StreamRole,
    failures_left: usize,
    sample_rate: u32,
    channels: u16,
    default: Option<DeviceIdentity>,
    attempts: usize,
    identity_queries: usize,
    builds: Vec<StreamInfo>,
    mixer: Option<Arc<Mutex<Mixer>>>,
    error_sender: Option<Sender<SupervisorMsg>>,
    live_sinks: usize,
    overlapping_builds: usize,
    /// Builds wait at the gate while this is set.
    hold: bool,
    /// A build is waiting at the gate.
    building: bool,
}

/// Longest a held build waits at the gate.
pub(crate) const BUILD_HOLD_LIMIT: Duration = Duration::from_secs(10);

struct SinkShared {
    state: Mutex<SinkState>,
    /// Signalled when `hold` is cleared.
    gate: Condvar,
}

/// An output factory that records builds instead of opening a device.
/// Clones share state.
#[derive(Clone)]
pub(crate) struct FakeSinkFactory(Arc<SinkShared>);

impl FakeSinkFactory {
    /// A monitor factory whose builds land on the default output `default`.
    pub(crate) fn new(sample_rate: u32, channels: u16, default: &str) -> FakeSinkFactory {
        FakeSinkFactory::for_role(StreamRole::Monitor, sample_rate, channels, default)
    }

    /// A factory for `role` whose builds land on the device `target`: the
    /// default output for the monitor, the matched device for the mic sink.
    pub(crate) fn for_role(
        role: StreamRole,
        sample_rate: u32,
        channels: u16,
        target: &str,
    ) -> FakeSinkFactory {
        FakeSinkFactory(Arc::new(SinkShared {
            state: Mutex::new(SinkState {
                role,
                failures_left: 0,
                sample_rate,
                channels,
                default: Some(DeviceIdentity::new(target)),
                attempts: 0,
                identity_queries: 0,
                builds: Vec::new(),
                mixer: None,
                error_sender: None,
                live_sinks: 0,
                overlapping_builds: 0,
                hold: false,
                building: false,
            }),
            gate: Condvar::new(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, SinkState> {
        lock(&self.0.state)
    }

    /// Make every following build wait until [`release_builds`], or at most
    /// [`BUILD_HOLD_LIMIT`]. Release before shutting the supervisor down, or
    /// its join waits for the limit.
    ///
    /// [`release_builds`]: FakeSinkFactory::release_builds
    pub(crate) fn hold_builds(&self) {
        self.state().hold = true;
    }

    /// Let held and later builds proceed.
    pub(crate) fn release_builds(&self) {
        self.state().hold = false;
        self.0.gate.notify_all();
    }

    /// Whether a build is waiting at the gate.
    pub(crate) fn building(&self) -> bool {
        self.state().building
    }

    /// Fail the next `n` builds.
    pub(crate) fn fail_next(&self, n: usize) {
        self.state().failures_left = n;
    }

    /// Format of the "device" for the next build.
    pub(crate) fn set_format(&self, sample_rate: u32, channels: u16) {
        let mut state = self.state();
        state.sample_rate = sample_rate;
        state.channels = channels;
    }

    /// Identity of the device a build would pick now (the default output for
    /// the monitor); `None` means no device.
    pub(crate) fn set_default(&self, default: Option<&str>) {
        self.state().default = default.map(DeviceIdentity::new);
    }

    /// Build calls, failed ones included.
    pub(crate) fn attempts(&self) -> usize {
        self.state().attempts
    }

    pub(crate) fn identity_queries(&self) -> usize {
        self.state().identity_queries
    }

    /// Successful builds, in order.
    pub(crate) fn builds(&self) -> Vec<StreamInfo> {
        self.state().builds.clone()
    }

    /// The mixer handed to the last build.
    pub(crate) fn mixer(&self) -> Option<Arc<Mutex<Mixer>>> {
        self.state().mixer.clone()
    }

    /// Sinks that have been built and not dropped yet.
    pub(crate) fn live_sinks(&self) -> usize {
        self.state().live_sinks
    }

    /// Builds that started while an older sink was still alive.
    pub(crate) fn overlapping_builds(&self) -> usize {
        self.state().overlapping_builds
    }

    /// Report a stream error the way a real stream callback would.
    pub(crate) fn send_stream_error(&self, kind: StreamErrorKind) -> bool {
        self.state().error_sender.as_ref().is_some_and(|s| {
            s.send(SupervisorMsg::StreamError {
                kind,
                message: "fake stream error".into(),
            })
            .is_ok()
        })
    }
}

struct FakeSink(Arc<SinkShared>);

impl ActiveStream for FakeSink {}

impl Drop for FakeSink {
    fn drop(&mut self) {
        lock(&self.0.state).live_sinks -= 1;
    }
}

impl StreamFactory for FakeSinkFactory {
    type Target = Arc<Mutex<Mixer>>;

    fn role(&self) -> StreamRole {
        self.state().role
    }

    fn build(
        &mut self,
        mixer: Arc<Mutex<Mixer>>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltStream, BuildError> {
        let mut state = self.state();
        state.attempts += 1;
        if state.hold {
            state.building = true;
            // Bounded, so a test that fails while holding a build still
            // unwinds instead of hanging in the supervisor join.
            state = self
                .0
                .gate
                .wait_timeout_while(state, BUILD_HOLD_LIMIT, |s| s.hold)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            state.building = false;
        }
        if state.live_sinks > 0 {
            state.overlapping_builds += 1;
        }
        if state.failures_left > 0 {
            state.failures_left -= 1;
            return Err(BuildError::Backend("fake build failure".into()));
        }
        let Some(identity) = state.default.clone() else {
            return Err(BuildError::NoDevice);
        };
        lock(&mixer).configure(state.sample_rate, state.channels);
        let info = StreamInfo {
            device_name: identity.as_str().to_owned(),
            identity,
            sample_rate: state.sample_rate,
            channels: state.channels,
            sample_format: "f32".into(),
        };
        state.builds.push(info.clone());
        state.mixer = Some(mixer);
        state.error_sender = Some(on_error);
        state.live_sinks += 1;
        Ok((Box::new(FakeSink(Arc::clone(&self.0))), info))
    }

    fn current_identity(&mut self) -> Option<DeviceIdentity> {
        let mut state = self.state();
        state.identity_queries += 1;
        state.default.clone()
    }
}
