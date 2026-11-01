//! Audio engine for Sound-Babound.
//!
//! This crate owns decoding, the realtime mixer, the output streams and the
//! per-stream supervisors that rebuild a stream when its device changes. It
//! has no UI dependencies.

// Used by the engine's mic path once it exists.
#[cfg_attr(not(test), allow(dead_code))]
mod capture;
mod convert;
mod cpal_sink;
mod engine;
mod error;
mod mixer;
// Used by the mic mixer and the capture stream once they exist.
#[cfg_attr(not(test), allow(dead_code))]
mod passthrough;
mod platform;
mod sink;
mod sound;
mod supervisor;
mod watcher;

#[cfg(test)]
mod test_support;

pub use engine::Engine;
pub use error::{BuildError, BuildErrorKind, LoadError, StartError, WatcherError};
pub use sink::{StreamInfo, StreamRole};
pub use sound::Sound;
pub use supervisor::StreamStatus;
pub use watcher::{DeviceEvent, RebuildPolicy};

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
