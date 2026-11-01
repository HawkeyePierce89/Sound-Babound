//! Streams, the factories that build them and the endpoints they feed.
//!
//! Every stream the engine runs (the monitor output, the mic sink and the
//! capture) is supervised the same way; only its [`StreamRole`], its
//! [`StreamFactory`] and its [`Endpoint`] differ.
//!
//! - A [`StreamFactory`] builds an [`ActiveStream`] on the device its role
//!   targets right now, and reports that device's identity without building.
//!   It caches no device.
//! - An [`Endpoint`] is the state the stream's callback reads or writes and
//!   the supervisor watches: the build target handed to the factory, the
//!   callback heartbeat, the active flag and the housekeeping done while no
//!   stream exists. [`MixerEndpoint`] serves both output roles.
//!
//! The supervisor owns the factory, the endpoint and the stream. It drops the
//! stream to stop it and asks the factory for a new one when the device
//! changes.

use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use crate::error::BuildError;
use crate::mixer::{Mixer, MixerShared};
use crate::supervisor::SupervisorMsg;

/// Which stream a supervisor runs. Each role has its own rebuild policy (see
/// `DeviceEvent::rebuild_policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamRole {
    /// The local output: the default output device, so the user hears the
    /// sounds.
    Monitor,
    /// The output to the virtual cable that a call app uses as its microphone.
    MicSink,
    /// The microphone capture that is passed through to the mic sink.
    Capture,
}

impl StreamRole {
    /// Every role, for exhaustive tests.
    #[cfg(test)]
    pub const ALL: [StreamRole; 3] = [
        StreamRole::Monitor,
        StreamRole::MicSink,
        StreamRole::Capture,
    ];

    /// A short name for logs and thread names.
    pub fn label(self) -> &'static str {
        match self {
            StreamRole::Monitor => "monitor",
            StreamRole::MicSink => "mic-sink",
            StreamRole::Capture => "capture",
        }
    }
}

impl fmt::Display for StreamRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// An opaque, comparable identity of an audio device.
///
/// It is the backend's stable device id when there is one, otherwise the
/// device name. Identities are only compared with each other, never parsed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeviceIdentity(String);

impl DeviceIdentity {
    /// Wrap a backend id or device name.
    pub fn new(id: impl Into<String>) -> DeviceIdentity {
        DeviceIdentity(id.into())
    }

    /// The raw identity string, for logging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a stream was built on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    /// Identity of the device the stream runs on.
    pub identity: DeviceIdentity,
    /// Human-readable device name.
    pub device_name: String,
    /// Stream sample rate in Hz.
    pub sample_rate: u32,
    /// Stream channel count.
    pub channels: u16,
    /// Device sample format, for logging (for example `f32`).
    pub sample_format: String,
}

/// A running stream. Dropping it stops the stream.
///
/// It is not `Send`: the supervisor thread builds, owns and drops it.
pub trait ActiveStream {}

/// A successful [`StreamFactory::build`].
pub type BuiltStream = (Box<dyn ActiveStream>, StreamInfo);

/// Builds the streams of one role.
pub trait StreamFactory: Send + 'static {
    /// What a build feeds: the mixer for an output, the passthrough for a
    /// capture. It comes from [`Endpoint::target`].
    type Target;

    /// The role this factory builds for.
    fn role(&self) -> StreamRole;

    /// Build and start a stream on the device this role targets right now.
    ///
    /// It is called only while no stream exists, so the factory may lock
    /// `target` freely before starting the stream. An output factory must
    /// call [`Mixer::configure`] with the stream's rate and channel count
    /// before the stream starts. Once the stream runs, only its callback
    /// touches the target (with `try_lock`). Stream errors are reported by
    /// sending [`SupervisorMsg::StreamError`] on `on_error`.
    fn build(
        &mut self,
        target: Self::Target,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltStream, BuildError>;

    /// Re-query the devices and return the identity of the one a build would
    /// pick now. `None` means there is none. Nothing is cached.
    fn current_identity(&mut self) -> Option<DeviceIdentity>;
}

/// The state a stream feeds, as the supervisor sees it.
///
/// The supervisor thread owns the endpoint. Every method runs there.
pub trait Endpoint: Send + 'static {
    /// What the factory builds against.
    type Target;

    /// A handle for the next build.
    fn target(&self) -> Self::Target;

    /// A counter the stream's callback advances; the stall watchdog reads it.
    fn heartbeat(&self) -> u64;

    /// Mark the stream as running (after a build returned) or gone. Marking
    /// it running also closes the play grace window.
    fn set_active(&self, active: bool);

    /// A live stream is being torn down for a rebuild: keep accepting plays
    /// until `until` and keep them for the replacement stream.
    fn open_play_grace(&self, until: Instant);

    /// Close the play grace window (on shutdown).
    fn close_play_grace(&self);

    /// Housekeeping while no stream exists. Drops the plays queued offline,
    /// unless the play grace window is still open at `now`, and applies the
    /// other queued commands. Returns the number of plays dropped.
    fn purge_offline(&self, now: Instant) -> usize;

    /// Free what the callback released, off the realtime thread.
    fn drain_garbage(&self);
}

/// The endpoint of an output role: a mixer the stream callback renders.
#[derive(Debug)]
pub struct MixerEndpoint {
    shared: Arc<MixerShared>,
    mixer: Arc<Mutex<Mixer>>,
}

impl MixerEndpoint {
    /// An endpoint with a new, unconfigured mixer on `shared`.
    pub fn new(shared: Arc<MixerShared>) -> MixerEndpoint {
        MixerEndpoint {
            mixer: Arc::new(Mutex::new(Mixer::new(Arc::clone(&shared)))),
            shared,
        }
    }

    /// The state shared with the control threads.
    #[cfg(test)]
    pub fn shared(&self) -> &Arc<MixerShared> {
        &self.shared
    }
}

impl Endpoint for MixerEndpoint {
    type Target = Arc<Mutex<Mixer>>;

    fn target(&self) -> Arc<Mutex<Mixer>> {
        Arc::clone(&self.mixer)
    }

    fn heartbeat(&self) -> u64 {
        self.shared.callback_heartbeat()
    }

    fn set_active(&self, active: bool) {
        self.shared.set_output_active(active);
        // Closed only after output is marked active, so plays stay accepted
        // without a gap.
        if active {
            self.shared.close_play_grace();
        }
    }

    fn open_play_grace(&self, until: Instant) {
        self.shared.open_play_grace(until);
    }

    fn close_play_grace(&self) {
        self.shared.close_play_grace();
    }

    fn purge_offline(&self, now: Instant) -> usize {
        if self.shared.play_grace_open(now) {
            return 0;
        }
        // No stream means no callback, so the lock cannot contend.
        self.mixer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .apply_offline()
    }

    fn drain_garbage(&self) {
        self.shared.drain_garbage();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::mixer::Command;
    use crate::sound::Sound;

    #[test]
    fn identities_compare_by_value() {
        let a = DeviceIdentity::new("speakers");
        assert_eq!(a, DeviceIdentity::new(String::from("speakers")));
        assert_ne!(a, DeviceIdentity::new("headset"));
        assert_eq!(a.as_str(), "speakers");
        assert_eq!(a.to_string(), "speakers");
    }

    #[test]
    fn roles_have_distinct_labels() {
        let labels: Vec<_> = StreamRole::ALL.iter().map(|r| r.to_string()).collect();
        assert_eq!(labels, ["monitor", "mic-sink", "capture"]);
    }

    fn press() -> Sound {
        Sound::from_interleaved(vec![0.5; 480], 48_000, 1, "press").unwrap()
    }

    #[test]
    fn mixer_endpoint_marks_output_and_closes_grace_when_active() {
        let endpoint = MixerEndpoint::new(Arc::new(MixerShared::new()));
        let shared = Arc::clone(endpoint.shared());
        let now = Instant::now();
        endpoint.open_play_grace(now + Duration::from_secs(60));
        assert!(shared.play_grace_open(now));
        endpoint.set_active(true);
        assert!(shared.output_active());
        assert!(!shared.play_grace_open(now));
        endpoint.set_active(false);
        assert!(!shared.output_active());
    }

    #[test]
    fn mixer_endpoint_purge_respects_the_grace_window() {
        let endpoint = MixerEndpoint::new(Arc::new(MixerShared::new()));
        let shared = Arc::clone(endpoint.shared());
        let sound = press();
        shared
            .push(Command::Play {
                sound: sound.clone(),
                gain: 1.0,
            })
            .unwrap();
        let now = Instant::now();
        endpoint.open_play_grace(now + Duration::from_secs(60));
        assert_eq!(endpoint.purge_offline(now), 0);
        assert_eq!(sound.handle_count(), 2);

        endpoint.close_play_grace();
        assert_eq!(endpoint.purge_offline(now), 1);
        assert_eq!(sound.handle_count(), 1);
    }

    #[test]
    fn mixer_endpoint_target_is_the_one_mixer() {
        let endpoint = MixerEndpoint::new(Arc::new(MixerShared::new()));
        assert!(Arc::ptr_eq(&endpoint.target(), &endpoint.target()));
        assert_eq!(endpoint.heartbeat(), 0);
        endpoint.target().lock().unwrap().configure(48_000, 1);
        let mut out = [0.0; 16];
        endpoint.target().lock().unwrap().render(&mut out);
        assert_eq!(endpoint.heartbeat(), 1);
    }
}
