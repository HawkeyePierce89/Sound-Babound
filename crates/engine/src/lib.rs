//! Audio engine for Sound-Babound.
//!
//! This crate owns decoding, the realtime mixer, the output sink and the
//! device supervisor that rebuilds the output stream when the default device
//! changes. It has no UI dependencies.

pub mod convert;
pub mod cpal_sink;
pub mod engine;
pub mod error;
pub mod mixer;
pub mod sink;
pub mod sound;
pub mod supervisor;
pub mod watcher;

#[cfg(test)]
mod test_support;

pub use cpal_sink::CpalSinkFactory;
pub use engine::Engine;
pub use error::{BuildError, LoadError, StartError, WatcherError};
pub use mixer::{Command, Mixer, MixerShared};
pub use sink::{ActiveSink, BuiltSink, DeviceIdentity, SinkFactory, StreamInfo};
pub use sound::{Sound, SoundData};
pub use supervisor::{SupervisorHandle, SupervisorMsg, spawn_supervisor};
pub use watcher::{DeviceEvent, DeviceWatcher, EventSender, RebuildPolicy, WatcherGuard};

/// Crate version, exposed so the app can log which engine it runs.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_set() {
        assert!(!VERSION.is_empty());
    }
}
