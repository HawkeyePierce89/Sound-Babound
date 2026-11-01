//! The public engine handle.
//!
//! [`Engine`] owns the monitor path (the local output) and, while the mic is
//! on, the mic path: the mic sink output to the virtual cable and the
//! microphone capture passed through to it. Each stream runs under its own
//! supervisor thread (see `supervisor`). Every sound command fans out to
//! each path's mixer.
//!
//! Every per-press control method is a non-blocking queue push or an atomic
//! read, so the engine can be called from UI and hotkey handlers without
//! waiting on an audio thread. Only [`Engine::set_mic`], which starts and
//! stops the mic path's supervisors, takes `&mut self` and may block briefly
//! while a supervisor thread is joined.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::warn;

use crate::capture::CpalCaptureFactory;
use crate::cpal_sink::CpalOutputFactory;
use crate::error::{BuildError, BuildErrorKind, StartError};
use crate::mixer::{Command, Mixer, MixerShared};
use crate::passthrough::PassthroughShared;
use crate::sink::{CaptureEndpoint, MixerEndpoint, StreamFactory, StreamInfo};
use crate::sound::Sound;
use crate::supervisor::{StreamStatus, SupervisorHandle, Timings, spawn_supervisor};
use crate::watcher::{DeviceWatcher, platform_watcher};

/// A factory for an output role (monitor or mic sink).
pub(crate) type OutputFactory = Box<dyn StreamFactory<Target = Arc<Mutex<Mixer>>>>;

/// A factory for the capture role.
pub(crate) type CaptureFactory = Box<dyn StreamFactory<Target = Arc<PassthroughShared>>>;

/// Where the engine gets its stream factories and device watchers from:
/// cpal and the platform watcher in production ([`CpalBackends`]), fakes in
/// tests. Each supervisor gets its own factory and its own watcher.
pub(crate) trait Backends: fmt::Debug + Send + Sync + 'static {
    /// The monitor: the default output device.
    fn monitor(&self) -> OutputFactory;

    /// The mic sink: the first output device matching `pattern`.
    fn mic_sink(&self, pattern: &str) -> OutputFactory;

    /// The capture: the default input, never one `exclusions` names.
    fn capture(&self, exclusions: &[String]) -> CaptureFactory;

    /// A device watcher for one supervisor.
    fn watcher(&self) -> Box<dyn DeviceWatcher>;
}

/// The production backends: cpal streams and the platform device watcher.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CpalBackends;

impl Backends for CpalBackends {
    fn monitor(&self) -> OutputFactory {
        Box::new(CpalOutputFactory::monitor())
    }

    fn mic_sink(&self, pattern: &str) -> OutputFactory {
        Box::new(CpalOutputFactory::mic_sink(pattern))
    }

    fn capture(&self, exclusions: &[String]) -> CaptureFactory {
        Box::new(CpalCaptureFactory::new(exclusions.to_vec()))
    }

    fn watcher(&self) -> Box<dyn DeviceWatcher> {
        Box::new(platform_watcher())
    }
}

/// How the mic path runs.
#[derive(Debug, Clone, PartialEq)]
pub struct MicSettings {
    /// The mic sink device: the first output whose display name or id
    /// contains this, ignoring case. A blank pattern matches nothing.
    pub device_match: String,
    /// Inputs the capture must never open (the virtual device's own input
    /// side or monitor), matched like `device_match`. `device_match` itself
    /// is always excluded too.
    pub excluded_inputs: Vec<String>,
    /// Gain on every sound sent to the mic sink, on top of the voice and
    /// master gains.
    pub sound_gain: f32,
    /// Gain on the microphone passthrough.
    pub mic_gain: f32,
    /// Whether to capture the microphone and pass it through. Without it the
    /// mic sink carries only the sounds.
    pub capture: bool,
}

impl MicSettings {
    /// Whether `other` differs from `self` in its gains only, so the
    /// running mic path can be kept.
    fn same_streams(&self, other: &MicSettings) -> bool {
        self.device_match == other.device_match
            && self.excluded_inputs == other.excluded_inputs
            && self.capture == other.capture
    }

    /// What the capture refuses to open: the configured exclusions plus the
    /// mic sink's own device, so the mic path never feeds itself.
    fn capture_exclusions(&self) -> Vec<String> {
        let mut exclusions: Vec<String> = self
            .excluded_inputs
            .iter()
            .filter(|p| !p.trim().is_empty())
            .cloned()
            .collect();
        if !self.device_match.trim().is_empty() && !exclusions.contains(&self.device_match) {
            exclusions.push(self.device_match.clone());
        }
        exclusions
    }

    fn path_gains(&self) -> Command {
        Command::SetPathGains {
            sound: self.sound_gain,
            input: self.mic_gain,
        }
    }
}

/// What the mic path is doing, for UI display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicStatus {
    /// The mic path is off.
    Off,
    /// The mic sink's first build, or a rebuild, is in progress.
    Starting,
    /// No output device matches the pattern; the mic sink keeps retrying.
    NotFound(String),
    /// The mic sink failed to build for another reason; it keeps retrying.
    Failed(String),
    /// The mic sink runs on `device`.
    Active {
        /// The mic sink's device name.
        device: String,
        /// What the microphone capture is doing.
        capture: CaptureStatus,
    },
}

/// What the microphone capture is doing while the mic sink runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureStatus {
    /// Capturing from this input device.
    Running(String),
    /// The capture's first build, or a rebuild, is in progress.
    Starting,
    /// Capture is off in the settings (for example, no microphone
    /// permission); the mic sink carries only the sounds.
    Disabled,
    /// The capture cannot run now, for this reason; it keeps retrying.
    Unavailable(String),
}

/// The mic status for these supervisor statuses. Pure. `capture` is `None`
/// when capture is disabled.
fn mic_status_of(
    settings: &MicSettings,
    sink: StreamStatus,
    capture: Option<StreamStatus>,
) -> MicStatus {
    match sink {
        StreamStatus::Starting => MicStatus::Starting,
        StreamStatus::Failed {
            kind: BuildErrorKind::DeviceNotFound,
            ..
        } => MicStatus::NotFound(settings.device_match.clone()),
        StreamStatus::Failed { error, .. } => MicStatus::Failed(error),
        StreamStatus::Running(info) => MicStatus::Active {
            device: info.device_name,
            capture: match capture {
                None => CaptureStatus::Disabled,
                Some(StreamStatus::Starting) => CaptureStatus::Starting,
                Some(StreamStatus::Running(info)) => CaptureStatus::Running(info.device_name),
                Some(StreamStatus::Failed { error, .. }) => CaptureStatus::Unavailable(error),
            },
        },
    }
}

/// What `mic-check` found, without building any stream.
#[derive(Debug)]
pub struct MicProbe {
    /// The device the mic sink would open, with its default config.
    pub sink: Result<StreamInfo, BuildError>,
    /// The input the capture would open, with its default config; `None`
    /// when capture is disabled.
    pub capture: Option<Result<StreamInfo, BuildError>>,
}

/// Resolve the devices the mic path would use with `settings`, and their
/// default configs, without building a stream. Nothing is played or
/// captured.
pub fn probe_mic(settings: &MicSettings) -> MicProbe {
    MicProbe {
        sink: CpalOutputFactory::mic_sink(settings.device_match.clone()).probe(),
        capture: settings
            .capture
            .then(|| CpalCaptureFactory::new(settings.capture_exclusions()).probe()),
    }
}

/// Push `command` onto one path's queue. A rejected command (and the sound
/// it holds) is dropped here, on the caller's thread, never on the realtime
/// thread.
fn push(shared: &MixerShared, command: Command) -> bool {
    shared.push(command).is_ok()
}

/// Queue a play on one path if it accepts plays at `now`.
fn push_play(shared: &MixerShared, sound: &Sound, gain: f32, now: Instant) -> bool {
    shared.accepts_plays(now)
        && push(
            shared,
            Command::Play {
                sound: sound.clone(),
                gain,
            },
        )
}

/// The running mic path: the mic sink and, when enabled, the capture.
#[derive(Debug)]
struct MicPath {
    settings: MicSettings,
    shared: Arc<MixerShared>,
    // Declared before `sink`, so dropping the path stops the capture first.
    capture: Option<SupervisorHandle>,
    sink: SupervisorHandle,
}

impl MicPath {
    fn status(&self) -> MicStatus {
        mic_status_of(
            &self.settings,
            self.sink.status(),
            self.capture.as_ref().map(SupervisorHandle::status),
        )
    }

    fn shutdown(self) {
        if let Some(capture) = self.capture {
            capture.shutdown();
        }
        self.sink.shutdown();
    }
}

/// A running audio engine.
///
/// The output streams are built on supervisor threads, so starting the
/// engine succeeds even when there is no output device yet: the supervisor
/// keeps retrying and playback starts once a device appears.
///
/// Plays are accepted while an output stream runs, and for a short grace
/// window (1 s) after a running stream is torn down for a rebuild, so a press
/// made while the output switches devices is played by the replacement
/// stream. Otherwise a press made with no stream (no device, a failed build,
/// or the first build still in progress) is rejected rather than held, so
/// presses do not all start at once when a device comes back. This holds per
/// path: with the mic on, each play goes to every path that accepts it. Use
/// [`Engine::has_output`] to wait for a stream before a play that must be
/// heard. Stop and gain commands always apply.
///
/// The mic path is off until [`Engine::set_mic`] turns it on; while it is
/// off, no mic sink or capture is built and the engine runs the monitor
/// alone.
///
/// Dropping the engine shuts it down, like [`Engine::shutdown`] (through the
/// supervisor handles' `Drop`).
#[derive(Debug)]
pub struct Engine {
    backends: Box<dyn Backends>,
    timings: Timings,
    // Declared before the monitor, so dropping the engine stops the mic path
    // first.
    mic: Option<MicPath>,
    shared: Arc<MixerShared>,
    supervisor: SupervisorHandle,
}

impl Engine {
    /// Start the engine on the system default output device, following
    /// device changes reported by the platform watcher. The mic path is off.
    pub fn start() -> Result<Engine, StartError> {
        Engine::start_with_backends(CpalBackends, Timings::default())
    }

    /// Start the engine on fakes: `factory` for the monitor, `watcher` for
    /// every supervisor.
    #[cfg(test)]
    pub(crate) fn start_with(
        factory: crate::test_support::FakeSinkFactory,
        watcher: crate::test_support::FakeWatcher,
    ) -> Result<Engine, StartError> {
        Engine::start_with_timings(factory, watcher, Timings::default())
    }

    /// [`Engine::start_with`] with custom supervisor timings.
    #[cfg(test)]
    pub(crate) fn start_with_timings(
        factory: crate::test_support::FakeSinkFactory,
        watcher: crate::test_support::FakeWatcher,
        timings: Timings,
    ) -> Result<Engine, StartError> {
        Engine::start_with_backends(
            crate::test_support::FakeBackends::new(factory, watcher),
            timings,
        )
    }

    /// Start the engine's monitor path on `backends` with `timings`.
    pub(crate) fn start_with_backends(
        backends: impl Backends,
        timings: Timings,
    ) -> Result<Engine, StartError> {
        let shared = Arc::new(MixerShared::new());
        let endpoint = MixerEndpoint::new(Arc::clone(&shared));
        let supervisor =
            spawn_supervisor(backends.monitor(), endpoint, backends.watcher(), timings)
                .map_err(StartError::Spawn)?;
        Ok(Engine {
            backends: Box::new(backends),
            timings,
            mic: None,
            shared,
            supervisor,
        })
    }

    /// Turn the mic path on, off, or change it.
    ///
    /// - `None` stops the mic sink and capture supervisors and joins them.
    /// - `Some` starts them if the mic is off.
    /// - A change to the gains only is applied to the running mic path
    ///   without a restart.
    /// - Any other change restarts the mic path (not the monitor).
    ///
    /// The monitor is never touched. Fails only if a supervisor thread
    /// cannot be spawned; the mic path is then off.
    pub fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<(), StartError> {
        let Some(settings) = settings else {
            if let Some(mic) = self.mic.take() {
                mic.shutdown();
            }
            return Ok(());
        };
        if let Some(mic) = self.mic.as_mut()
            && mic.settings.same_streams(&settings)
        {
            if mic.settings != settings && !push(&mic.shared, settings.path_gains()) {
                warn!("mic command queue is full; mic gains not applied");
            }
            mic.settings = settings;
            return Ok(());
        }
        if let Some(mic) = self.mic.take() {
            mic.shutdown();
        }
        self.mic = Some(self.start_mic(settings)?);
        Ok(())
    }

    /// Start the mic sink and, if enabled, the capture.
    fn start_mic(&self, settings: MicSettings) -> Result<MicPath, StartError> {
        let shared = Arc::new(MixerShared::new());
        // Applied before the first build (offline) or by the first render.
        // The queue is empty, so the push cannot fail.
        let _ = push(&shared, settings.path_gains());

        let (endpoint, passthrough) = if settings.capture {
            let passthrough = Arc::new(PassthroughShared::new());
            let endpoint =
                MixerEndpoint::with_passthrough(Arc::clone(&shared), passthrough.reader());
            (endpoint, Some(passthrough))
        } else {
            (MixerEndpoint::new(Arc::clone(&shared)), None)
        };
        let sink = spawn_supervisor(
            self.backends.mic_sink(&settings.device_match),
            endpoint,
            self.backends.watcher(),
            self.timings,
        )
        .map_err(StartError::Spawn)?;
        let capture = match passthrough {
            Some(passthrough) => Some(
                spawn_supervisor(
                    self.backends.capture(&settings.capture_exclusions()),
                    CaptureEndpoint::new(passthrough),
                    self.backends.watcher(),
                    self.timings,
                )
                // Dropping `sink` on the way out stops it.
                .map_err(StartError::Spawn)?,
            ),
            None => None,
        };
        Ok(MicPath {
            settings,
            shared,
            capture,
            sink,
        })
    }

    /// Queue a new voice playing `sound` at `gain` on every path that
    /// accepts plays now. Plays overlap.
    ///
    /// Returns true if any path accepted the play, false if every path
    /// dropped it, because it does not accept plays now or its command queue
    /// is full; the caller decides whether to log it. A path accepts plays
    /// while its stream runs and inside the play grace window
    /// (`Timings::play_grace`, 1 s) that opens when its supervisor tears down
    /// a running stream for a rebuild. The check happens before the push, so
    /// a play made with no stream and no open window (no device, a failed
    /// build, or a build started with no stream) is never queued on that
    /// path. The queue is kept across a rebuild from a live stream, so the
    /// replacement stream applies an accepted play in order, just as it
    /// applies plays the old callback had not popped yet and continues the
    /// voices already playing. If no stream comes up before the window
    /// closes, the supervisor drops the play (see `Mixer::apply_offline`).
    pub fn play(&self, sound: &Sound, gain: f32) -> bool {
        let now = Instant::now();
        let monitor = push_play(&self.shared, sound, gain, now);
        let mic = self
            .mic
            .as_ref()
            .is_some_and(|mic| push_play(&mic.shared, sound, gain, now));
        monitor || mic
    }

    /// Queue a stop of every voice on every path. Returns false if a path's
    /// queue is full.
    pub fn stop_all(&self) -> bool {
        self.push_all(|| Command::StopAll)
    }

    /// Queue a new master gain on every path. Non-finite values mean
    /// silence, negative values clamp to 0. Returns false if a path's queue
    /// is full.
    pub fn set_master_gain(&self, gain: f32) -> bool {
        self.push_all(|| Command::SetMasterGain(gain))
    }

    /// Voices audible as of the last render, summed over the paths.
    pub fn active_voices(&self) -> usize {
        self.shared.active_voices()
            + self
                .mic
                .as_ref()
                .map_or(0, |mic| mic.shared.active_voices())
    }

    /// Total number of commands the mixers have applied, summed over the
    /// paths. Plays dropped for lack of an output stream are not counted.
    pub fn commands_applied(&self) -> u64 {
        self.shared.commands_applied()
            + self
                .mic
                .as_ref()
                .map_or(0, |mic| mic.shared.commands_applied())
    }

    /// Whether the monitor output stream is running, so a play would be
    /// heard locally.
    pub fn has_output(&self) -> bool {
        self.shared.output_active()
    }

    /// Whether the mic sink stream is running, so a play would reach the
    /// call. False while the mic is off.
    pub fn has_mic_output(&self) -> bool {
        self.mic
            .as_ref()
            .is_some_and(|mic| mic.shared.output_active())
    }

    /// What the monitor output stream is doing: starting, running on a
    /// device, or failing to build (with the error). For UI display.
    pub fn output_status(&self) -> StreamStatus {
        self.supervisor.status()
    }

    /// What the mic path is doing. For UI display.
    pub fn mic_status(&self) -> MicStatus {
        self.mic.as_ref().map_or(MicStatus::Off, MicPath::status)
    }

    /// How long until the last frame the latest callback wrote is played,
    /// the longest over the paths. It includes the backend buffer, the
    /// device's own latency and the callback's buffer. Zero before the first
    /// callback.
    pub fn output_latency(&self) -> Duration {
        let monitor = self.shared.output_latency();
        self.mic
            .as_ref()
            .map_or(monitor, |mic| monitor.max(mic.shared.output_latency()))
    }

    /// Stop every stream, unregister the watchers and join the supervisor
    /// threads.
    pub fn shutdown(mut self) {
        if let Some(mic) = self.mic.take() {
            mic.shutdown();
        }
        // `self` drops here: the monitor supervisor shuts down.
    }

    /// Push a command built by `command` to every path. Every path gets it,
    /// even after one rejected it.
    fn push_all(&self, command: impl Fn() -> Command) -> bool {
        let monitor = push(&self.shared, command());
        let mic = self
            .mic
            .as_ref()
            .is_none_or(|mic| push(&mic.shared, command()));
        monitor && mic
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::thread;

    use super::*;
    use crate::mixer::{COMMAND_CAPACITY, Mixer};
    use crate::sink::DeviceIdentity;
    use crate::test_support::{FakeBackends, FakeSinkFactory, FakeWatcher, wait_until};
    use crate::{BuildError, DeviceEvent};

    const TIMEOUT: Duration = Duration::from_secs(5);

    const _: () = {
        const fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Engine>();
    };

    fn tone() -> Sound {
        Sound::from_interleaved(vec![0.5; 4800], 48_000, 1, "tone").unwrap()
    }

    /// Start an engine on fakes and wait for its first build.
    fn started() -> (Engine, FakeSinkFactory, FakeWatcher, Arc<Mutex<Mixer>>) {
        let factory = FakeSinkFactory::new(48_000, 2, "speakers");
        let watcher = FakeWatcher::default();
        let engine = Engine::start_with(factory.clone(), watcher.clone()).unwrap();
        assert!(wait_until(TIMEOUT, || factory.builds().len() == 1));
        assert!(wait_until(TIMEOUT, || engine.has_output()));
        let mixer = factory.mixer().unwrap();
        (engine, factory, watcher, mixer)
    }

    /// Render one block through the fake sink's handle to the mixer, as the
    /// stream callback would.
    fn render(mixer: &Mutex<Mixer>) -> Vec<f32> {
        let mut out = vec![0.0; 256 * 2];
        mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .render(&mut out);
        out
    }

    fn is_silent(block: &[f32]) -> bool {
        block.iter().all(|s| *s == 0.0)
    }

    #[test]
    fn play_then_render_produces_output() {
        let (engine, _factory, _watcher, mixer) = started();
        assert!(engine.play(&tone(), 1.0));
        let block = render(&mixer);
        assert!(!is_silent(&block));
        assert_eq!(engine.active_voices(), 1);
        assert_eq!(engine.commands_applied(), 1);
        engine.shutdown();
    }

    #[test]
    fn output_status_follows_the_monitor_stream() {
        let factory = FakeSinkFactory::new(48_000, 2, "speakers");
        factory.set_default(None);
        let engine = Engine::start_with(factory.clone(), FakeWatcher::default()).unwrap();
        assert!(wait_until(TIMEOUT, || matches!(
            engine.output_status(),
            StreamStatus::Failed { .. }
        )));
        factory.set_default(Some("speakers"));
        assert!(wait_until(TIMEOUT, || matches!(
            engine.output_status(),
            StreamStatus::Running(ref info) if info.device_name == "speakers"
        )));
        engine.shutdown();
    }

    #[test]
    fn play_without_output_is_rejected() {
        let factory = FakeSinkFactory::new(48_000, 2, "speakers");
        factory.fail_next(usize::MAX);
        let engine = Engine::start_with(factory.clone(), FakeWatcher::default()).unwrap();
        assert!(wait_until(TIMEOUT, || factory.attempts() >= 1));
        assert!(!engine.has_output());

        let sound = tone();
        assert!(!engine.play(&sound, 1.0));
        // Nothing was queued: the rejected handle was dropped on this thread.
        assert_eq!(sound.handle_count(), 1);
        // Stop and gain still queue.
        assert!(engine.stop_all());
        assert!(engine.set_master_gain(0.5));

        // Once a stream runs, plays are accepted again.
        factory.fail_next(0);
        assert!(wait_until(TIMEOUT, || engine.has_output()));
        assert!(engine.play(&sound, 1.0));
        engine.shutdown();
    }

    #[test]
    fn stop_all_silences() {
        let (engine, _factory, _watcher, mixer) = started();
        assert!(engine.play(&tone(), 1.0));
        assert!(!is_silent(&render(&mixer)));
        assert!(engine.stop_all());
        assert!(is_silent(&render(&mixer)));
        assert_eq!(engine.active_voices(), 0);
        assert_eq!(engine.commands_applied(), 2);
    }

    #[test]
    fn zero_master_gain_silences() {
        let (engine, _factory, _watcher, mixer) = started();
        assert!(engine.set_master_gain(0.0));
        assert!(engine.play(&tone(), 1.0));
        assert!(is_silent(&render(&mixer)));
        assert_eq!(engine.active_voices(), 1);

        assert!(engine.set_master_gain(1.0));
        assert!(!is_silent(&render(&mixer)));
    }

    #[test]
    fn full_queue_rejects_play_without_panicking() {
        let (engine, _factory, _watcher, _mixer) = started();
        let sound = tone();
        for _ in 0..COMMAND_CAPACITY {
            assert!(engine.play(&sound, 1.0));
        }
        assert!(!engine.play(&sound, 1.0));
        assert!(!engine.stop_all());
        assert!(!engine.set_master_gain(0.5));
        // The engine plus the queued commands hold handles; the rejected
        // ones were dropped on this thread.
        assert_eq!(sound.handle_count(), 1 + COMMAND_CAPACITY);
    }

    #[test]
    fn concurrent_play_and_stop_release_every_sound() {
        let (engine, _factory, _watcher, mixer) = started();
        let engine = Arc::new(engine);
        let sound = tone();
        let renderer = {
            let mixer = Arc::clone(&mixer);
            let engine = Arc::clone(&engine);
            thread::spawn(move || {
                for _ in 0..200 {
                    render(&mixer);
                    let _ = engine.active_voices();
                }
            })
        };
        let controllers: Vec<_> = (0..4)
            .map(|i| {
                let engine = Arc::clone(&engine);
                let sound = sound.clone();
                thread::spawn(move || {
                    for n in 0..200 {
                        if (n + i) % 7 == 0 {
                            engine.stop_all();
                        } else {
                            engine.play(&sound, 0.5);
                        }
                        let _ = engine.commands_applied();
                    }
                })
            })
            .collect();
        for handle in controllers {
            handle.join().unwrap();
        }
        renderer.join().unwrap();
        assert!(engine.commands_applied() > 0);

        // A final stop, applied after every play, leaves no voice holding the
        // sound; the supervisor frees every released handle off this thread.
        assert!(wait_until(TIMEOUT, || {
            render(&mixer);
            engine.stop_all()
        }));
        assert!(wait_until(TIMEOUT, || {
            render(&mixer);
            sound.handle_count() == 1
        }));
        assert_eq!(engine.active_voices(), 0);
        Arc::into_inner(engine).unwrap().shutdown();
    }

    #[test]
    fn shutdown_stops_the_stream_and_watcher() {
        let (engine, factory, watcher, _mixer) = started();
        assert_eq!(factory.live_sinks(), 1);
        engine.shutdown();
        assert_eq!(factory.live_sinks(), 0);
        assert_eq!(watcher.guards_dropped(), 1);
    }

    #[test]
    fn drop_shuts_down() {
        let (engine, factory, watcher, _mixer) = started();
        drop(engine);
        assert_eq!(factory.live_sinks(), 0);
        assert_eq!(watcher.guards_dropped(), 1);
    }

    #[test]
    fn engine_follows_a_default_output_change() {
        let (engine, factory, watcher, mixer) = started();
        assert!(engine.play(&tone(), 1.0));
        render(&mixer);
        factory.set_format(44_100, 2);
        factory.set_default(Some("headset"));
        assert!(watcher.send(crate::DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(TIMEOUT, || factory.builds().len() == 2));
        assert_eq!(factory.builds()[1].sample_rate, 44_100);
        // The voice survived the rebuild.
        assert!(!is_silent(&render(&mixer)));
    }

    fn grace_timings(play_grace: Duration) -> Timings {
        Timings {
            debounce: Duration::from_millis(20),
            retry: Duration::from_millis(20),
            stall: Duration::from_secs(3600),
            startup_grace: Duration::from_secs(3600),
            play_grace,
        }
    }

    #[test]
    fn play_during_a_live_rebuild_is_kept_for_the_new_stream() {
        let factory = FakeSinkFactory::new(48_000, 2, "speakers");
        let watcher = FakeWatcher::default();
        let engine = Engine::start_with_timings(
            factory.clone(),
            watcher.clone(),
            grace_timings(Duration::from_secs(30)),
        )
        .unwrap();
        assert!(wait_until(TIMEOUT, || engine.has_output()));
        let mixer = factory.mixer().unwrap();

        factory.hold_builds();
        factory.set_format(44_100, 2);
        factory.set_default(Some("headset"));
        assert!(watcher.send(crate::DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(TIMEOUT, || factory.building()));
        assert!(!engine.has_output());

        let sound = tone();
        assert!(engine.play(&sound, 1.0));
        assert_eq!(sound.handle_count(), 2);

        factory.release_builds();
        assert!(wait_until(TIMEOUT, || engine.has_output()));
        assert_eq!(factory.builds().len(), 2);
        assert_eq!(factory.builds()[1].sample_rate, 44_100);
        let block = render(&mixer);
        assert!(!is_silent(&block));
        assert_eq!(engine.active_voices(), 1);
        assert_eq!(engine.commands_applied(), 1);
        engine.shutdown();
    }

    #[test]
    fn play_inside_the_window_is_dropped_when_no_stream_returns() {
        let factory = FakeSinkFactory::new(48_000, 2, "speakers");
        let watcher = FakeWatcher::default();
        let engine = Engine::start_with_timings(
            factory.clone(),
            watcher.clone(),
            grace_timings(Duration::from_millis(500)),
        )
        .unwrap();
        assert!(wait_until(TIMEOUT, || engine.has_output()));

        factory.fail_next(usize::MAX);
        factory.hold_builds();
        assert!(watcher.send(crate::DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(TIMEOUT, || factory.building()));
        assert!(!engine.has_output());

        // Inside the window: accepted and queued.
        let sound = tone();
        assert!(engine.play(&sound, 1.0));
        assert_eq!(sound.handle_count(), 2);

        // Every build fails; once the window closes the play is dropped.
        factory.release_builds();
        assert!(wait_until(TIMEOUT, || sound.handle_count() == 1));
        assert!(!engine.has_output());

        // After the window: rejected and never queued.
        assert!(!engine.play(&sound, 1.0));
        assert_eq!(sound.handle_count(), 1);
        assert_eq!(engine.commands_applied(), 0);
        engine.shutdown();
    }

    // The mic path.

    fn fast() -> Timings {
        grace_timings(Duration::from_secs(1))
    }

    fn mic_settings() -> MicSettings {
        MicSettings {
            device_match: "cable".into(),
            excluded_inputs: vec!["cable monitor".into()],
            sound_gain: 1.0,
            mic_gain: 1.0,
            capture: true,
        }
    }

    /// An engine on fake backends with its monitor running and the mic off.
    fn mic_rig() -> (Engine, FakeBackends) {
        let backends = FakeBackends::new(
            FakeSinkFactory::new(48_000, 2, "speakers"),
            FakeWatcher::default(),
        );
        let engine = Engine::start_with_backends(backends.clone(), fast()).unwrap();
        assert!(wait_until(TIMEOUT, || engine.has_output()));
        (engine, backends)
    }

    /// Turn the mic on with `settings` and wait until every mic stream runs.
    fn mic_on(engine: &mut Engine, backends: &FakeBackends, settings: MicSettings) {
        let capture = settings.capture;
        engine.set_mic(Some(settings)).unwrap();
        assert!(wait_until(TIMEOUT, || engine.has_mic_output()));
        if capture {
            assert!(wait_until(TIMEOUT, || matches!(
                engine.mic_status(),
                MicStatus::Active {
                    capture: CaptureStatus::Running(_),
                    ..
                }
            )));
        }
        assert!(backends.mic_sink.mixer().is_some());
    }

    /// The monitor was built once and runs untouched.
    fn assert_monitor_untouched(backends: &FakeBackends) {
        assert_eq!(backends.monitor.builds().len(), 1);
        assert_eq!(backends.monitor.live_sinks(), 1);
        assert_eq!(backends.monitor_requests(), 1);
    }

    #[test]
    fn mic_off_builds_no_mic_streams() {
        let (engine, backends) = mic_rig();
        assert!(engine.play(&tone(), 1.0));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(backends.mic_sink.attempts(), 0);
        assert_eq!(backends.capture.attempts(), 0);
        assert!(backends.mic_sink_requests().is_empty());
        assert!(backends.capture_requests().is_empty());
        assert_eq!(backends.watcher_requests(), 1);
        assert_eq!(backends.watcher.started(), 1);
        assert_eq!(engine.mic_status(), MicStatus::Off);
        assert!(!engine.has_mic_output());
        assert_monitor_untouched(&backends);
        engine.shutdown();
        assert_eq!(backends.watcher.guards_dropped(), 1);
    }

    #[test]
    fn set_mic_on_and_off_starts_and_stops_only_the_mic_path() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());
        assert_eq!(backends.mic_sink_requests(), ["cable"]);
        assert_eq!(
            backends.capture_requests(),
            [vec!["cable monitor".to_string(), "cable".to_string()]]
        );
        assert_eq!(backends.watcher.started(), 3);
        assert_eq!(
            engine.mic_status(),
            MicStatus::Active {
                device: "cable".into(),
                capture: CaptureStatus::Running("mic".into()),
            }
        );
        assert_eq!(backends.mic_sink.live_sinks(), 1);
        assert_eq!(backends.capture.live_sinks(), 1);

        engine.set_mic(None).unwrap();
        assert_eq!(engine.mic_status(), MicStatus::Off);
        assert!(!engine.has_mic_output());
        assert_eq!(backends.mic_sink.live_sinks(), 0);
        assert_eq!(backends.capture.live_sinks(), 0);
        assert_eq!(backends.watcher.guards_dropped(), 2);
        assert!(engine.has_output());
        assert_monitor_untouched(&backends);

        // Off again is a no-op.
        engine.set_mic(None).unwrap();
        assert_eq!(backends.watcher.guards_dropped(), 2);
        engine.shutdown();
        assert_eq!(backends.watcher.guards_dropped(), 3);
    }

    #[test]
    fn gain_only_change_keeps_the_mic_streams() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());
        let mixer = backends.mic_sink.mixer().unwrap();
        render(&mixer);
        assert_eq!(mixer.lock().unwrap().path_gains(), (1.0, 1.0));

        let quieter = MicSettings {
            sound_gain: 0.5,
            mic_gain: 0.25,
            ..mic_settings()
        };
        engine.set_mic(Some(quieter.clone())).unwrap();
        render(&mixer);
        assert_eq!(mixer.lock().unwrap().path_gains(), (0.5, 0.25));
        // Same settings again: nothing is queued or restarted.
        let applied = engine.commands_applied();
        engine.set_mic(Some(quieter)).unwrap();
        render(&mixer);
        assert_eq!(engine.commands_applied(), applied);

        assert_eq!(backends.mic_sink_requests().len(), 1);
        assert_eq!(backends.capture_requests().len(), 1);
        assert_eq!(backends.mic_sink.builds().len(), 1);
        assert_eq!(backends.capture.builds().len(), 1);
        assert_eq!(backends.watcher.guards_dropped(), 0);
        assert_monitor_untouched(&backends);
        engine.shutdown();
    }

    #[test]
    fn initial_gains_reach_the_new_mic_mixer() {
        let (mut engine, backends) = mic_rig();
        let settings = MicSettings {
            sound_gain: 0.5,
            mic_gain: 2.0,
            ..mic_settings()
        };
        mic_on(&mut engine, &backends, settings);
        let mixer = backends.mic_sink.mixer().unwrap();
        render(&mixer);
        assert_eq!(mixer.lock().unwrap().path_gains(), (0.5, 2.0));
        engine.shutdown();
    }

    #[test]
    fn device_change_restarts_only_the_mic_path() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());

        let other = MicSettings {
            device_match: "loopback".into(),
            ..mic_settings()
        };
        engine.set_mic(Some(other)).unwrap();
        assert!(wait_until(TIMEOUT, || backends.mic_sink.builds().len() == 2));
        assert!(wait_until(TIMEOUT, || backends.capture.builds().len() == 2));
        assert_eq!(backends.mic_sink_requests(), ["cable", "loopback"]);
        assert_eq!(
            backends.capture_requests()[1],
            ["cable monitor".to_string(), "loopback".to_string()]
        );
        assert_eq!(backends.mic_sink.live_sinks(), 1);
        assert_eq!(backends.capture.live_sinks(), 1);
        assert_eq!(backends.mic_sink.overlapping_builds(), 0);
        assert_eq!(backends.watcher.guards_dropped(), 2);

        // Turning capture off restarts the mic sink without a capture.
        let no_capture = MicSettings {
            capture: false,
            ..mic_settings()
        };
        mic_on(&mut engine, &backends, no_capture);
        assert!(wait_until(TIMEOUT, || backends.mic_sink.builds().len() == 3));
        assert_eq!(backends.capture.live_sinks(), 0);
        assert_eq!(backends.capture_requests().len(), 2);
        assert!(wait_until(TIMEOUT, || engine.mic_status()
            == MicStatus::Active {
                device: "cable".into(),
                capture: CaptureStatus::Disabled,
            }));
        assert_monitor_untouched(&backends);
        engine.shutdown();
    }

    #[test]
    fn a_play_reaches_both_mixers() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());
        let monitor = backends.monitor.mixer().unwrap();
        let mic = backends.mic_sink.mixer().unwrap();
        let sound = tone();
        assert!(engine.play(&sound, 1.0));
        assert_eq!(sound.handle_count(), 3);
        assert!(!is_silent(&render(&monitor)));
        assert!(!is_silent(&render(&mic)));
        assert_eq!(engine.active_voices(), 2);
        assert_eq!(engine.commands_applied(), 3);

        // Stop and gain go to both paths.
        assert!(engine.set_master_gain(0.0));
        assert!(is_silent(&render(&monitor)));
        assert!(is_silent(&render(&mic)));
        assert!(engine.stop_all());
        render(&monitor);
        render(&mic);
        assert_eq!(engine.active_voices(), 0);
        engine.shutdown();
    }

    #[test]
    fn a_full_mic_queue_drops_only_the_mic_copy() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());
        let mic = engine.mic.as_ref().unwrap();
        let filler = tone();
        while mic.shared.push(Command::StopAll).is_ok() {}
        assert!(engine.play(&filler, 1.0));
        // The monitor queued it; the mic copy was dropped on this thread.
        assert_eq!(filler.handle_count(), 2);
        assert!(!engine.stop_all());
        assert!(!engine.set_master_gain(0.5));
        drop(backends);
        engine.shutdown();
    }

    #[test]
    fn the_passthrough_reaches_the_mic_mixer_but_not_the_monitor() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());
        let monitor = backends.monitor.mixer().unwrap();
        let mic = backends.mic_sink.mixer().unwrap();
        let passthrough = backends.capture.passthrough().unwrap();
        assert_eq!(passthrough.capture_rate(), 48_000);

        // 100 ms of capture, past the 30 ms priming target.
        passthrough.write(&[0.25f32; 4_800], 1);
        let mic_block = render(&mic);
        assert!(mic_block.iter().any(|s| (s - 0.25).abs() < 1e-3));
        assert!(is_silent(&render(&monitor)));
        assert_eq!(engine.active_voices(), 0);
        engine.shutdown();
    }

    #[test]
    fn switching_the_default_input_rebuilds_only_the_capture() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());

        backends.capture.set_default(Some("headset mic"));
        assert!(backends.watcher.send(DeviceEvent::DefaultInputChanged));
        assert!(wait_until(TIMEOUT, || backends.capture.builds().len() == 2));
        assert!(wait_until(TIMEOUT, || matches!(
            engine.mic_status(),
            MicStatus::Active {
                capture: CaptureStatus::Running(ref name),
                ..
            } if name == "headset mic"
        )));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(backends.mic_sink.attempts(), 1);
        assert_eq!(backends.mic_sink.live_sinks(), 1);
        assert_eq!(backends.capture.live_sinks(), 1);
        assert_monitor_untouched(&backends);
        engine.shutdown();
    }

    #[test]
    fn switching_the_default_output_rebuilds_only_the_monitor() {
        let (mut engine, backends) = mic_rig();
        mic_on(&mut engine, &backends, mic_settings());

        backends.monitor.set_default(Some("headset"));
        assert!(backends.watcher.send(DeviceEvent::DefaultOutputChanged));
        assert!(wait_until(TIMEOUT, || backends.monitor.builds().len() == 2));
        thread::sleep(Duration::from_millis(200));
        assert_eq!(backends.monitor.live_sinks(), 1);
        assert_eq!(backends.mic_sink.attempts(), 1);
        assert_eq!(backends.mic_sink.live_sinks(), 1);
        assert_eq!(backends.capture.attempts(), 1);
        assert_eq!(backends.capture.live_sinks(), 1);
        assert!(engine.has_mic_output());
        engine.shutdown();
    }

    #[test]
    fn a_missing_mic_sink_retries_while_the_monitor_plays() {
        let (mut engine, backends) = mic_rig();
        backends.mic_sink.set_not_found(Some("cable"));
        engine.set_mic(Some(mic_settings())).unwrap();
        assert!(wait_until(TIMEOUT, || backends.mic_sink.attempts() >= 4));
        assert_eq!(engine.mic_status(), MicStatus::NotFound("cable".into()));
        // Only the first failure of the streak was a warning.
        let sink = &engine.mic.as_ref().unwrap().sink;
        assert_eq!(sink.failure_warnings(), 1);
        assert!(!engine.has_mic_output());

        // The monitor plays meanwhile; the mic path takes no play.
        let monitor = backends.monitor.mixer().unwrap();
        let sound = tone();
        assert!(engine.play(&sound, 1.0));
        assert_eq!(sound.handle_count(), 2);
        assert!(!is_silent(&render(&monitor)));
        assert_monitor_untouched(&backends);

        // The device appears: the mic sink comes up on a retry.
        backends.mic_sink.set_not_found(None);
        assert!(wait_until(TIMEOUT, || engine.has_mic_output()));
        assert!(matches!(engine.mic_status(), MicStatus::Active { .. }));
        engine.shutdown();
    }

    #[test]
    fn mic_status_maps_the_supervisor_statuses() {
        let settings = mic_settings();
        let info = |name: &str| StreamInfo {
            identity: DeviceIdentity::new(name),
            device_name: name.into(),
            sample_rate: 48_000,
            channels: 2,
            sample_format: "f32".into(),
        };
        let failed = |err: BuildError| StreamStatus::Failed {
            error: err.to_string(),
            kind: err.kind(),
        };
        assert_eq!(
            mic_status_of(&settings, StreamStatus::Starting, None),
            MicStatus::Starting
        );
        assert_eq!(
            mic_status_of(
                &settings,
                failed(BuildError::DeviceNotFound("cable".into())),
                None
            ),
            MicStatus::NotFound("cable".into())
        );
        assert_eq!(
            mic_status_of(&settings, failed(BuildError::Backend("boom".into())), None),
            MicStatus::Failed("audio backend error: boom".into())
        );
        let active = |capture| MicStatus::Active {
            device: "cable".into(),
            capture,
        };
        let running = || StreamStatus::Running(info("cable"));
        assert_eq!(
            mic_status_of(&settings, running(), None),
            active(CaptureStatus::Disabled)
        );
        assert_eq!(
            mic_status_of(&settings, running(), Some(StreamStatus::Starting)),
            active(CaptureStatus::Starting)
        );
        assert_eq!(
            mic_status_of(
                &settings,
                running(),
                Some(StreamStatus::Running(info("mic")))
            ),
            active(CaptureStatus::Running("mic".into()))
        );
        assert_eq!(
            mic_status_of(
                &settings,
                running(),
                Some(failed(BuildError::FeedbackExcluded("cable".into())))
            ),
            active(CaptureStatus::Unavailable(
                BuildError::FeedbackExcluded("cable".into()).to_string()
            ))
        );
    }

    #[test]
    fn capture_exclusions_add_the_mic_device_once() {
        let settings = mic_settings();
        assert_eq!(settings.capture_exclusions(), ["cable monitor", "cable"]);
        let listed = MicSettings {
            excluded_inputs: vec!["cable".into(), " ".into()],
            ..mic_settings()
        };
        assert_eq!(listed.capture_exclusions(), ["cable"]);
        let blank = MicSettings {
            device_match: "  ".into(),
            excluded_inputs: Vec::new(),
            ..mic_settings()
        };
        assert!(blank.capture_exclusions().is_empty());
    }

    #[test]
    fn only_gain_changes_keep_the_streams() {
        let base = mic_settings();
        let gains = MicSettings {
            sound_gain: 0.1,
            mic_gain: 3.0,
            ..mic_settings()
        };
        assert!(base.same_streams(&gains));
        for changed in [
            MicSettings {
                device_match: "other".into(),
                ..mic_settings()
            },
            MicSettings {
                excluded_inputs: Vec::new(),
                ..mic_settings()
            },
            MicSettings {
                capture: false,
                ..mic_settings()
            },
        ] {
            assert!(!base.same_streams(&changed));
        }
    }
}
