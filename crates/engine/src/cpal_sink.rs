//! The cpal output sink.
//!
//! [`CpalOutputFactory`] builds the output streams of both output roles:
//!
//! - [`OutputTarget::Default`], the monitor: the system default output
//!   device of [`platform::monitor_host`].
//! - [`OutputTarget::Named`], the mic sink: the first output device of
//!   [`platform::mic_host`] whose name or id contains the pattern.
//!
//! It holds no device, host or identity between calls. Every build and every
//! identity query re-queries the host (and, for a named target, re-enumerates
//! its output devices), so a rebuild always lands on the live device.
//!
//! The helpers shared with the capture (error mapping, device identity and
//! name matching) live here too.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, I24, OutputStreamTimestamp, SampleFormat, StreamConfig, U24};

use crate::convert::{OutputSample, fill_silence, write_samples};
use crate::error::BuildError;
use crate::mixer::Mixer;
use crate::platform;
use crate::sink::{
    ActiveStream, BuiltStream, DeviceIdentity, StreamFactory, StreamInfo, StreamRole,
};
use crate::supervisor::{StreamErrorKind, SupervisorMsg};

/// How long the backend may take to open a stream before the build fails
/// (and the supervisor retries). Not every backend honors it.
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Which output device a [`CpalOutputFactory`] builds on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// The current default output device (the monitor).
    Default,
    /// The first output device whose display name or id contains this
    /// pattern, ignoring case (the mic sink).
    Named(String),
}

/// Builds cpal output streams on the device its [`OutputTarget`] picks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpalOutputFactory {
    target: OutputTarget,
}

impl CpalOutputFactory {
    /// The monitor: the default output device.
    pub fn monitor() -> CpalOutputFactory {
        CpalOutputFactory {
            target: OutputTarget::Default,
        }
    }

    /// The mic sink: the first output device matching `pattern`.
    pub fn mic_sink(pattern: impl Into<String>) -> CpalOutputFactory {
        CpalOutputFactory {
            target: OutputTarget::Named(pattern.into()),
        }
    }

    /// The target this factory builds on.
    #[cfg(test)]
    pub fn target(&self) -> &OutputTarget {
        &self.target
    }

    /// Resolve the device a build would use now, without building a stream.
    fn resolve(&self) -> Result<cpal::Device, BuildError> {
        match &self.target {
            OutputTarget::Default => platform::monitor_host()
                .default_output_device()
                .ok_or(BuildError::NoDevice),
            OutputTarget::Named(pattern) => {
                let devices = platform::mic_host().output_devices().map_err(build_error)?;
                first_match(devices, pattern, device_labels)
                    .ok_or_else(|| BuildError::DeviceNotFound(pattern.clone()))
            }
        }
    }

    /// Resolve the device and its default config without building a stream,
    /// for `mic-check`.
    pub fn probe(&self) -> Result<StreamInfo, BuildError> {
        let device = self.resolve()?;
        let supported = device.default_output_config().map_err(build_error)?;
        Ok(stream_info(&device, &supported))
    }
}

/// A running cpal stream. Dropping it stops the stream.
pub(crate) struct CpalStream {
    pub(crate) _stream: cpal::Stream,
}

impl ActiveStream for CpalStream {}

impl StreamFactory for CpalOutputFactory {
    type Target = Arc<Mutex<Mixer>>;

    fn role(&self) -> StreamRole {
        match self.target {
            OutputTarget::Default => StreamRole::Monitor,
            OutputTarget::Named(_) => StreamRole::MicSink,
        }
    }

    fn build(
        &mut self,
        mixer: Arc<Mutex<Mixer>>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltStream, BuildError> {
        let device = self.resolve()?;
        let supported = device.default_output_config().map_err(build_error)?;
        let info = stream_info(&device, &supported);
        let sample_format = supported.sample_format();
        let config = supported.config();

        // No stream exists yet, so this lock cannot contend with a callback.
        mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .configure(config.sample_rate, config.channels);

        let stream = match sample_format {
            SampleFormat::F32 => build_typed::<f32>(&device, &config, mixer, on_error),
            SampleFormat::F64 => build_typed::<f64>(&device, &config, mixer, on_error),
            SampleFormat::I8 => build_typed::<i8>(&device, &config, mixer, on_error),
            SampleFormat::I16 => build_typed::<i16>(&device, &config, mixer, on_error),
            SampleFormat::I24 => build_typed::<I24>(&device, &config, mixer, on_error),
            SampleFormat::I32 => build_typed::<i32>(&device, &config, mixer, on_error),
            SampleFormat::I64 => build_typed::<i64>(&device, &config, mixer, on_error),
            SampleFormat::U8 => build_typed::<u8>(&device, &config, mixer, on_error),
            SampleFormat::U16 => build_typed::<u16>(&device, &config, mixer, on_error),
            SampleFormat::U24 => build_typed::<U24>(&device, &config, mixer, on_error),
            SampleFormat::U32 => build_typed::<u32>(&device, &config, mixer, on_error),
            SampleFormat::U64 => build_typed::<u64>(&device, &config, mixer, on_error),
            // DSD bitstreams are not PCM; the f32 mix cannot be written to them.
            other => return Err(BuildError::UnsupportedFormat(other.to_string())),
        }?;
        stream.play().map_err(build_error)?;
        Ok((Box::new(CpalStream { _stream: stream }), info))
    }

    fn current_identity(&mut self) -> Option<DeviceIdentity> {
        self.resolve().ok().map(|device| identity_of(&device))
    }
}

/// What a stream on `device` with `supported` runs at.
pub(crate) fn stream_info(
    device: &cpal::Device,
    supported: &cpal::SupportedStreamConfig,
) -> StreamInfo {
    StreamInfo {
        identity: identity_of(device),
        device_name: device.to_string(),
        sample_rate: supported.sample_rate(),
        channels: supported.channels(),
        sample_format: supported.sample_format().to_string(),
    }
}

/// The backend's stable device id, or the device name when there is none.
pub(crate) fn identity_of(device: &cpal::Device) -> DeviceIdentity {
    match device.id() {
        Ok(id) => DeviceIdentity::new(id.to_string()),
        Err(_) => DeviceIdentity::new(device.to_string()),
    }
}

/// The display name and, when the backend has one, the id of `device`: the
/// strings a device pattern is matched against.
pub(crate) fn device_labels(device: &cpal::Device) -> (String, Option<String>) {
    (
        device.to_string(),
        device.id().ok().map(|id| id.to_string()),
    )
}

/// Whether `pattern` names a device with display name `name` and id `id`: a
/// case-insensitive substring of either. A blank pattern names nothing, so
/// it can never pick an arbitrary device.
pub(crate) fn name_matches(pattern: &str, name: &str, id: Option<&str>) -> bool {
    let pattern = pattern.trim().to_lowercase();
    if pattern.is_empty() {
        return false;
    }
    name.to_lowercase().contains(&pattern)
        || id.is_some_and(|id| id.to_lowercase().contains(&pattern))
}

/// The first of `devices`, in enumeration order, that `pattern` names.
pub(crate) fn first_match<D>(
    devices: impl IntoIterator<Item = D>,
    pattern: &str,
    labels: impl Fn(&D) -> (String, Option<String>),
) -> Option<D> {
    devices.into_iter().find(|device| {
        let (name, id) = labels(device);
        name_matches(pattern, &name, id.as_deref())
    })
}

pub(crate) fn build_error(err: cpal::Error) -> BuildError {
    match err.kind() {
        ErrorKind::DeviceNotAvailable => BuildError::NoDevice,
        _ => BuildError::Backend(err.to_string()),
    }
}

/// The error callback of every cpal stream: forward the errors that need a
/// rebuild to the supervisor. It only sends on a channel, which never
/// blocks.
pub(crate) fn forward_errors(
    on_error: Sender<SupervisorMsg>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    move |err: cpal::Error| {
        if needs_rebuild(err.kind()) {
            let _ = on_error.send(SupervisorMsg::StreamError {
                kind: stream_error_kind(err.kind()),
                message: err.to_string(),
            });
        }
    }
}

/// Whether a stream error should make the supervisor rebuild.
///
/// Xruns and a refused realtime promotion leave the stream running, and a
/// rebuild would not fix them, only add a gap. Everything else (including
/// `DeviceChanged`, where the backend rerouted to a new default that may
/// have a different rate) is forwarded.
fn needs_rebuild(kind: ErrorKind) -> bool {
    !matches!(kind, ErrorKind::Xrun | ErrorKind::RealtimeDenied)
}

/// How the supervisor classifies a forwarded stream error. Only the log
/// level depends on it; every forwarded error rebuilds.
fn stream_error_kind(kind: ErrorKind) -> StreamErrorKind {
    match kind {
        ErrorKind::DeviceChanged => StreamErrorKind::DeviceChanged,
        _ => StreamErrorKind::Other,
    }
}

fn build_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: Arc<Mutex<Mixer>>,
    on_error: Sender<SupervisorMsg>,
) -> Result<cpal::Stream, BuildError>
where
    T: OutputSample + Send + 'static,
{
    device
        .build_output_stream::<T, _, _>(
            *config,
            move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
                fill_from_mixer(&mixer, out, callback_latency(info.timestamp()));
            },
            forward_errors(on_error),
            Some(OPEN_TIMEOUT),
        )
        .map_err(build_error)
}

/// How long the samples written in this callback take to reach the output.
/// The backends fold their buffering and the device's own latency into
/// `playback`, so this covers large buffers and wireless outputs too.
fn callback_latency(timestamp: OutputStreamTimestamp) -> Duration {
    timestamp
        .playback
        .saturating_duration_since(timestamp.callback)
}

/// The data-callback body: record the output latency and render the mix
/// into `out`, or silence if the mixer is locked. Realtime-safe: `try_lock`
/// only, no allocation, no I/O.
///
/// `latency` is when the first frame of `out` is played. The recorded latency
/// adds the duration of `out` itself, so it covers the last frame written
/// here, which is the one a drain must wait for.
pub(crate) fn fill_from_mixer<T>(mixer: &Mutex<Mixer>, out: &mut [T], latency: Duration)
where
    T: OutputSample,
{
    let mut guard = match mixer.try_lock() {
        Ok(guard) => guard,
        // A panic elsewhere does not leave the mixer in a state it cannot render.
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            fill_silence(out);
            return;
        }
    };
    let buffer = buffer_duration(out.len(), guard.channels(), guard.sample_rate());
    guard.record_output_latency(latency.saturating_add(buffer));
    render_into(&mut guard, out);
}

/// How long `samples` interleaved samples play at `rate` with `channels`.
/// Zero for an unconfigured mixer. Realtime-safe: integer math only.
fn buffer_duration(samples: usize, channels: u16, rate: u32) -> Duration {
    let frames = samples.checked_div(usize::from(channels)).unwrap_or(0);
    let frames = u64::try_from(frames).unwrap_or(u64::MAX);
    match u64::from(rate) {
        0 => Duration::ZERO,
        rate => Duration::from_nanos(frames.saturating_mul(1_000_000_000) / rate),
    }
}

/// Render `out` in chunks of the mixer's preallocated scratch size and
/// convert each chunk. Anything that cannot be rendered (an unconfigured
/// mixer, a trailing partial frame) is filled with silence.
pub(crate) fn render_into<T>(mixer: &mut Mixer, out: &mut [T])
where
    T: OutputSample,
{
    let channels = usize::from(mixer.channels());
    let mut rest = out;
    // `checked_div` is `None` for an unconfigured mixer (0 channels).
    while let Some(frames @ 1..) = rest.len().checked_div(channels) {
        let mix = mixer.render_scratch(frames);
        if mix.is_empty() {
            break;
        }
        let written = mix.len().min(rest.len());
        let (head, tail) = std::mem::take(&mut rest).split_at_mut(written);
        write_samples(mix, head);
        rest = tail;
    }
    fill_silence(rest);
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::mixer::{Command, MixerShared, SCRATCH_FRAMES};
    use crate::sound::Sound;

    fn tone(frames: usize, rate: u32) -> Sound {
        let samples = (0..frames)
            .map(|i| (i as f32 * 0.013).sin() * 0.7)
            .collect();
        Sound::from_interleaved(samples, rate, 1, "tone").unwrap()
    }

    fn playing_mixer(sound: &Sound, rate: u32, channels: u16) -> Mixer {
        let mut mixer = Mixer::new(Arc::new(MixerShared::new()));
        mixer.configure(rate, channels);
        mixer
            .shared()
            .push(Command::Play {
                sound: sound.clone(),
                gain: 0.9,
            })
            .unwrap();
        mixer
    }

    #[test]
    fn chunked_render_matches_single_render() {
        let sound = tone(10_000, 44_100);
        let mut single = playing_mixer(&sound, 48_000, 2);
        let mut chunked = playing_mixer(&sound, 48_000, 2);

        let frames = SCRATCH_FRAMES * 3 + 77;
        let mut expected = vec![0.0f32; frames * 2];
        single.render(&mut expected);

        let mut got = vec![9.0f32; frames * 2];
        render_into(&mut chunked, &mut got);

        assert!(expected.iter().any(|s| s.abs() > 0.1));
        for (i, (a, b)) in expected.iter().zip(&got).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} vs {b}");
        }
        assert!(
            chunked.shared().callback_heartbeat() > 1,
            "rendered in several chunks"
        );
    }

    #[test]
    fn chunked_render_converts_to_i16() {
        let sound = tone(10_000, 48_000);
        let mut single = playing_mixer(&sound, 48_000, 1);
        let mut chunked = playing_mixer(&sound, 48_000, 1);

        let frames = SCRATCH_FRAMES * 2 + 5;
        let mut mix = vec![0.0f32; frames];
        single.render(&mut mix);
        let mut expected = vec![0i16; frames];
        write_samples(&mix, &mut expected);

        let mut got = vec![0i16; frames];
        render_into(&mut chunked, &mut got);
        assert_eq!(expected, got);
    }

    #[test]
    fn trailing_partial_frame_is_silent() {
        let sound = tone(1_000, 48_000);
        let mut mixer = playing_mixer(&sound, 48_000, 2);
        let mut out = [5u16; 7];
        render_into(&mut mixer, &mut out);
        assert_eq!(out[6], 32_768);
    }

    #[test]
    fn unconfigured_mixer_renders_silence() {
        let mut mixer = Mixer::new(Arc::new(MixerShared::new()));
        let mut out = [3i16; 16];
        render_into(&mut mixer, &mut out);
        assert_eq!(out, [0; 16]);
    }

    #[test]
    fn locked_mixer_gives_silence_without_rendering() {
        let sound = tone(1_000, 48_000);
        let mixer = Mutex::new(playing_mixer(&sound, 48_000, 2));
        let held = mixer.lock().unwrap();
        // 480 stereo frames at 48 kHz: a 10 ms buffer.
        let mut out = [0.5f32; 960];
        fill_from_mixer(&mixer, &mut out, Duration::from_millis(40));
        assert_eq!(out, [0.0; 960]);
        assert_eq!(held.shared().callback_heartbeat(), 0);
        assert_eq!(held.shared().output_latency(), Duration::ZERO);
        drop(held);

        fill_from_mixer(&mixer, &mut out, Duration::from_millis(40));
        assert!(out.iter().any(|s| *s != 0.0));
        let guard = mixer.lock().unwrap();
        assert_eq!(guard.shared().callback_heartbeat(), 1);
        // The reported latency plus the buffer just written.
        assert_eq!(guard.shared().output_latency(), Duration::from_millis(50));
    }

    #[test]
    fn buffer_duration_counts_frames_at_the_rate() {
        assert_eq!(buffer_duration(960, 2, 48_000), Duration::from_millis(10));
        assert_eq!(buffer_duration(44_100, 1, 44_100), Duration::from_secs(1));
        // A large buffer is not hidden by the fixed drain margin.
        assert_eq!(
            buffer_duration(48_000, 2, 48_000),
            Duration::from_millis(500)
        );
        assert_eq!(buffer_duration(960, 0, 48_000), Duration::ZERO);
        assert_eq!(buffer_duration(960, 2, 0), Duration::ZERO);
    }

    #[test]
    fn callback_latency_is_playback_minus_callback() {
        let callback = cpal::StreamInstant::from_millis(1_000);
        let playback = cpal::StreamInstant::from_millis(3_250);
        let latency =
            |callback, playback| callback_latency(OutputStreamTimestamp { callback, playback });
        assert_eq!(latency(callback, playback), Duration::from_millis(2_250));
        // A backend that predicts playback before the callback reports zero.
        assert_eq!(latency(playback, callback), Duration::ZERO);
    }

    #[test]
    fn only_recoverable_errors_are_swallowed() {
        assert!(!needs_rebuild(ErrorKind::Xrun));
        assert!(!needs_rebuild(ErrorKind::RealtimeDenied));
        assert!(needs_rebuild(ErrorKind::DeviceChanged));
        assert!(needs_rebuild(ErrorKind::DeviceNotAvailable));
        assert!(needs_rebuild(ErrorKind::StreamInvalidated));
        assert!(needs_rebuild(ErrorKind::BackendError));
    }

    #[test]
    fn device_changed_maps_to_its_own_kind() {
        assert_eq!(
            stream_error_kind(ErrorKind::DeviceChanged),
            StreamErrorKind::DeviceChanged
        );
        for kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::BackendError,
            ErrorKind::DeviceBusy,
        ] {
            assert_eq!(stream_error_kind(kind), StreamErrorKind::Other, "{kind:?}");
        }
    }

    #[test]
    fn device_loss_maps_to_no_device() {
        assert!(matches!(
            build_error(cpal::Error::new(ErrorKind::DeviceNotAvailable)),
            BuildError::NoDevice
        ));
        assert!(matches!(
            build_error(cpal::Error::new(ErrorKind::DeviceBusy)),
            BuildError::Backend(_)
        ));
    }

    #[test]
    fn names_match_case_insensitively_by_display_name_or_id() {
        assert!(name_matches("blackhole", "BlackHole 2ch", None));
        assert!(name_matches("BLACKHOLE", "BlackHole 2ch", None));
        assert!(name_matches(
            "sound_babound_sink",
            "Sound-Babound",
            Some("pulseaudio:sound_babound_sink")
        ));
        assert!(name_matches(
            "  cable input ",
            "CABLE Input (VB-Audio)",
            None
        ));
        assert!(!name_matches(
            "BlackHole",
            "MacBook Pro Speakers",
            Some("BuiltIn")
        ));
        // A blank pattern never picks a device.
        assert!(!name_matches("", "BlackHole 2ch", None));
        assert!(!name_matches("   ", "BlackHole 2ch", Some("id")));
    }

    #[test]
    fn the_first_match_in_enumeration_order_wins() {
        let devices = [
            ("Speakers", "spk"),
            ("BlackHole 16ch", "bh16"),
            ("BlackHole 2ch", "bh2"),
        ];
        let labels = |d: &(&str, &str)| (d.0.to_string(), Some(d.1.to_string()));
        assert_eq!(
            first_match(devices, "blackhole", labels),
            Some(("BlackHole 16ch", "bh16"))
        );
        assert_eq!(
            first_match(devices, "BH2", labels),
            Some(("BlackHole 2ch", "bh2"))
        );
        assert_eq!(first_match(devices, "cable", labels), None);
    }

    #[test]
    fn output_targets_map_to_roles() {
        let monitor = CpalOutputFactory::monitor();
        assert_eq!(monitor.role(), StreamRole::Monitor);
        assert_eq!(monitor.target(), &OutputTarget::Default);
        let mic = CpalOutputFactory::mic_sink("BlackHole");
        assert_eq!(mic.role(), StreamRole::MicSink);
        assert_eq!(mic.target(), &OutputTarget::Named("BlackHole".into()));
    }

    /// Needs real devices: `cargo test -p babound-engine -- --ignored`.
    ///
    /// Builds the monitor and, when `BABOUND_MIC_DEVICE` or the platform's
    /// default virtual device pattern resolves, the mic sink and the
    /// capture. It plays nothing: both mixers have no voices and no
    /// passthrough, and nothing reads the captured audio.
    #[test]
    #[ignore = "needs real audio devices"]
    fn smoke_builds_real_streams() {
        let mut monitor = CpalOutputFactory::monitor();
        let shared = Arc::new(MixerShared::new());
        let mixer = Arc::new(Mutex::new(Mixer::new(Arc::clone(&shared))));
        let (tx, _rx) = mpsc::channel();
        let (sink, info) = monitor.build(Arc::clone(&mixer), tx.clone()).unwrap();
        assert!(info.sample_rate > 0);
        assert!(info.channels > 0);
        assert_eq!(monitor.current_identity(), Some(info.identity.clone()));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            shared.callback_heartbeat() > 0,
            "monitor callback is running"
        );

        let pattern = std::env::var("BABOUND_MIC_DEVICE")
            .unwrap_or_else(|_| crate::platform::SMOKE_MIC_PATTERN.to_string());
        let mut mic = CpalOutputFactory::mic_sink(pattern.clone());
        let Ok(probe) = mic.probe() else {
            eprintln!("no output device matches {pattern:?}; mic streams skipped");
            return;
        };
        let mic_shared = Arc::new(MixerShared::new());
        let mic_mixer = Arc::new(Mutex::new(Mixer::new(Arc::clone(&mic_shared))));
        let (mic_sink, mic_info) = mic.build(mic_mixer, tx.clone()).unwrap();
        assert_eq!(mic_info, probe);
        assert_ne!(
            mic_info.identity, info.identity,
            "mic sink is not the monitor"
        );
        assert_eq!(mic.current_identity(), Some(mic_info.identity.clone()));

        let mut capture = crate::capture::CpalCaptureFactory::new(vec![pattern]);
        let capture_probe = capture.probe().unwrap();
        let passthrough = Arc::new(crate::passthrough::PassthroughShared::new());
        let (capture_stream, capture_info) = capture.build(Arc::clone(&passthrough), tx).unwrap();
        assert_eq!(capture_info, capture_probe);
        assert!(capture_info.sample_rate > 0);
        assert!(capture_info.channels > 0);
        assert_eq!(passthrough.capture_rate(), capture_info.sample_rate);
        assert_eq!(
            capture.current_identity(),
            Some(capture_info.identity.clone())
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            mic_shared.callback_heartbeat() > 0,
            "mic sink callback is running"
        );
        assert!(passthrough.heartbeat() > 0, "capture callback is running");

        drop(capture_stream);
        drop(mic_sink);
        drop(sink);
    }
}
