//! Fakes shared by the engine's tests.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{BuildError, WatcherError};
use crate::mixer::Mixer;
use crate::sink::{ActiveSink, BuiltSink, DeviceIdentity, SinkFactory, StreamInfo};
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
    sender: Option<EventSender>,
    started: usize,
    guards_dropped: usize,
    fail: bool,
}

/// A watcher whose events are sent by the test. Clones share state.
#[derive(Clone, Default)]
pub(crate) struct FakeWatcher(Arc<Mutex<WatcherState>>);

impl FakeWatcher {
    /// Deliver an event as if the OS reported it. False if not started or
    /// the supervisor is gone.
    pub(crate) fn send(&self, event: DeviceEvent) -> bool {
        lock(&self.0).sender.as_ref().is_some_and(|s| s.send(event))
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
        state.sender = Some(events);
        Ok(Box::new(FakeGuard(Arc::clone(&self.0))))
    }
}

struct SinkState {
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
}

/// A sink factory that records builds instead of opening a device.
/// Clones share state.
#[derive(Clone)]
pub(crate) struct FakeSinkFactory(Arc<Mutex<SinkState>>);

impl FakeSinkFactory {
    pub(crate) fn new(sample_rate: u32, channels: u16, default: &str) -> FakeSinkFactory {
        FakeSinkFactory(Arc::new(Mutex::new(SinkState {
            failures_left: 0,
            sample_rate,
            channels,
            default: Some(DeviceIdentity::new(default)),
            attempts: 0,
            identity_queries: 0,
            builds: Vec::new(),
            mixer: None,
            error_sender: None,
            live_sinks: 0,
            overlapping_builds: 0,
        })))
    }

    /// Fail the next `n` builds.
    pub(crate) fn fail_next(&self, n: usize) {
        lock(&self.0).failures_left = n;
    }

    /// Format of the "device" for the next build.
    pub(crate) fn set_format(&self, sample_rate: u32, channels: u16) {
        let mut state = lock(&self.0);
        state.sample_rate = sample_rate;
        state.channels = channels;
    }

    /// Identity of the current default output; `None` means no device.
    pub(crate) fn set_default(&self, default: Option<&str>) {
        lock(&self.0).default = default.map(DeviceIdentity::new);
    }

    /// Build calls, failed ones included.
    pub(crate) fn attempts(&self) -> usize {
        lock(&self.0).attempts
    }

    pub(crate) fn identity_queries(&self) -> usize {
        lock(&self.0).identity_queries
    }

    /// Successful builds, in order.
    pub(crate) fn builds(&self) -> Vec<StreamInfo> {
        lock(&self.0).builds.clone()
    }

    /// The mixer handed to the last build.
    pub(crate) fn mixer(&self) -> Option<Arc<Mutex<Mixer>>> {
        lock(&self.0).mixer.clone()
    }

    /// Sinks that have been built and not dropped yet.
    pub(crate) fn live_sinks(&self) -> usize {
        lock(&self.0).live_sinks
    }

    /// Builds that started while an older sink was still alive.
    pub(crate) fn overlapping_builds(&self) -> usize {
        lock(&self.0).overlapping_builds
    }

    /// Report a stream error the way a real stream callback would.
    pub(crate) fn send_stream_error(&self, kind: StreamErrorKind) -> bool {
        lock(&self.0).error_sender.as_ref().is_some_and(|s| {
            s.send(SupervisorMsg::StreamError {
                kind,
                message: "fake stream error".into(),
            })
            .is_ok()
        })
    }
}

struct FakeSink(Arc<Mutex<SinkState>>);

impl ActiveSink for FakeSink {}

impl Drop for FakeSink {
    fn drop(&mut self) {
        lock(&self.0).live_sinks -= 1;
    }
}

impl SinkFactory for FakeSinkFactory {
    fn build(
        &mut self,
        mixer: Arc<Mutex<Mixer>>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltSink, BuildError> {
        let mut state = lock(&self.0);
        state.attempts += 1;
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

    fn current_default_identity(&mut self) -> Option<DeviceIdentity> {
        let mut state = lock(&self.0);
        state.identity_queries += 1;
        state.default.clone()
    }
}
