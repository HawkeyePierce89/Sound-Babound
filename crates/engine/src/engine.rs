//! The public engine handle.
//!
//! [`Engine`] owns the shared mixer state and the supervisor thread. Every
//! control method is a non-blocking queue push or an atomic read, so the
//! engine can be shared between threads behind an `Arc` and called from UI
//! and hotkey handlers without waiting on the audio thread.

use std::sync::Arc;

use crate::cpal_sink::CpalSinkFactory;
use crate::error::StartError;
use crate::mixer::{Command, MixerShared};
use crate::sink::SinkFactory;
use crate::sound::Sound;
use crate::supervisor::{SupervisorHandle, spawn_supervisor};
use crate::watcher::{DeviceWatcher, platform_watcher};

/// A running audio engine.
///
/// The output stream is built on the supervisor thread, so starting the
/// engine succeeds even when there is no output device yet: the supervisor
/// keeps retrying and playback starts once a device appears.
///
/// Dropping the engine shuts it down, like [`Engine::shutdown`].
#[derive(Debug)]
pub struct Engine {
    shared: Arc<MixerShared>,
    supervisor: Option<SupervisorHandle>,
}

impl Engine {
    /// Start the engine on the system default output device, following
    /// device changes reported by the platform watcher.
    pub fn start() -> Result<Engine, StartError> {
        Engine::start_with(CpalSinkFactory::new(), platform_watcher())
    }

    /// Start the engine with a custom sink factory and watcher.
    pub(crate) fn start_with<F, W>(factory: F, watcher: W) -> Result<Engine, StartError>
    where
        F: SinkFactory,
        W: DeviceWatcher,
    {
        let shared = Arc::new(MixerShared::new());
        let supervisor =
            spawn_supervisor(factory, watcher, Arc::clone(&shared)).map_err(StartError::Spawn)?;
        Ok(Engine {
            shared,
            supervisor: Some(supervisor),
        })
    }

    /// Queue a new voice playing `sound` at `gain`. Plays overlap.
    ///
    /// Returns false if the command queue is full; the play is dropped and
    /// the caller decides whether to log it.
    pub fn play(&self, sound: &Sound, gain: f32) -> bool {
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

    /// Total number of commands the mixer has applied.
    pub fn commands_applied(&self) -> u64 {
        self.shared.commands_applied()
    }

    /// Stop the stream, unregister the watcher and join the supervisor thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn push(&self, command: Command) -> bool {
        // A rejected command (and the sound it holds) is dropped here, on the
        // caller's thread, never on the realtime thread.
        self.shared.push(command).is_ok()
    }

    fn stop(&mut self) {
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.shutdown();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};
    use std::thread;
    use std::time::Duration;

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
    fn concurrent_play_and_stop_do_not_panic() {
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
}
