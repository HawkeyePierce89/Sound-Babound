//! The public engine handle.
//!
//! [`Engine`] owns the shared mixer state and the supervisor thread. Every
//! control method is a non-blocking queue push or an atomic read, so the
//! engine can be shared between threads behind an `Arc` and called from UI
//! and hotkey handlers without waiting on the audio thread.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::cpal_sink::CpalSinkFactory;
use crate::error::StartError;
use crate::mixer::{Command, MixerShared};
use crate::sink::SinkFactory;
use crate::sound::Sound;
use crate::supervisor::{SupervisorHandle, Timings, spawn_supervisor};
use crate::watcher::{DeviceWatcher, platform_watcher};

/// A running audio engine.
///
/// The output stream is built on the supervisor thread, so starting the
/// engine succeeds even when there is no output device yet: the supervisor
/// keeps retrying and playback starts once a device appears.
///
/// Plays are accepted while an output stream runs, and for a short grace
/// window (1 s) after a running stream is torn down for a rebuild, so a press
/// made while the output switches devices is played by the replacement
/// stream. Otherwise a press made with no stream (no device, a failed build,
/// or the first build still in progress) is rejected rather than held, so
/// presses do not all start at once when a device comes back. Use
/// [`Engine::has_output`] to wait for a stream before a play that must be
/// heard. Stop and gain commands always apply.
///
/// Dropping the engine shuts it down, like [`Engine::shutdown`] (through the
/// supervisor handle's `Drop`).
#[derive(Debug)]
pub struct Engine {
    shared: Arc<MixerShared>,
    supervisor: SupervisorHandle,
}

impl Engine {
    /// Start the engine on the system default output device, following
    /// device changes reported by the platform watcher.
    pub fn start() -> Result<Engine, StartError> {
        Engine::start_with(CpalSinkFactory, platform_watcher())
    }

    /// Start the engine with a custom sink factory and watcher.
    pub(crate) fn start_with<F, W>(factory: F, watcher: W) -> Result<Engine, StartError>
    where
        F: SinkFactory,
        W: DeviceWatcher,
    {
        Engine::start_with_timings(factory, watcher, Timings::default())
    }

    /// Start the engine with a custom sink factory, watcher and supervisor
    /// timings.
    pub(crate) fn start_with_timings<F, W>(
        factory: F,
        watcher: W,
        timings: Timings,
    ) -> Result<Engine, StartError>
    where
        F: SinkFactory,
        W: DeviceWatcher,
    {
        let shared = Arc::new(MixerShared::new());
        let supervisor = spawn_supervisor(factory, watcher, Arc::clone(&shared), timings)
            .map_err(StartError::Spawn)?;
        Ok(Engine { shared, supervisor })
    }

    /// Queue a new voice playing `sound` at `gain`. Plays overlap.
    ///
    /// Returns false if the play was dropped, because plays are not accepted
    /// now or the command queue is full; the caller decides whether to log
    /// it. Plays are accepted while a stream runs and inside the play grace
    /// window (`Timings::play_grace`, 1 s) that opens when the supervisor
    /// tears down a running stream for a rebuild. The check happens before
    /// the push, so a play made with no stream and no open window (no device,
    /// a failed build, or a build started with no stream) is never queued.
    /// The queue is kept across a rebuild from a live stream, so the
    /// replacement stream applies an accepted play in order, just as it
    /// applies plays the old callback had not popped yet and continues the
    /// voices already playing. If no stream comes up before the window
    /// closes, the supervisor drops the play (see `Mixer::apply_offline`).
    pub fn play(&self, sound: &Sound, gain: f32) -> bool {
        if !self.shared.accepts_plays(Instant::now()) {
            return false;
        }
        self.push(Command::Play {
            sound: sound.clone(),
            gain,
        })
    }

    /// Queue a stop of every voice. Returns false if the queue is full.
    pub fn stop_all(&self) -> bool {
        self.push(Command::StopAll)
    }

    /// Queue a new master gain. Non-finite values mean silence, negative
    /// values clamp to 0. Returns false if the queue is full.
    pub fn set_master_gain(&self, gain: f32) -> bool {
        self.push(Command::SetMasterGain(gain))
    }

    /// Voices audible as of the last render.
    pub fn active_voices(&self) -> usize {
        self.shared.active_voices()
    }

    /// Total number of commands the mixer has applied. Plays dropped for
    /// lack of an output stream are not counted.
    pub fn commands_applied(&self) -> u64 {
        self.shared.commands_applied()
    }

    /// Whether an output stream is running, so a play would be heard.
    pub fn has_output(&self) -> bool {
        self.shared.output_active()
    }

    /// How long until the last frame the latest callback wrote is played. It
    /// includes the backend buffer, the device's own latency and the
    /// callback's buffer. Zero before the first callback.
    pub fn output_latency(&self) -> Duration {
        self.shared.output_latency()
    }

    /// Stop the stream, unregister the watcher and join the supervisor thread.
    pub fn shutdown(self) {
        self.supervisor.shutdown();
    }

    fn push(&self, command: Command) -> bool {
        // A rejected command (and the sound it holds) is dropped here, on the
        // caller's thread, never on the realtime thread.
        self.shared.push(command).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::thread;

    use super::*;
    use crate::mixer::{COMMAND_CAPACITY, Mixer};
    use crate::test_support::{FakeSinkFactory, FakeWatcher, wait_until};

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
}
