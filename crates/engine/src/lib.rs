//! Audio engine for Sound-Babound.
//!
//! This crate owns decoding, the realtime mixer, the output sink and the
//! device supervisor that rebuilds the output stream when the default device
//! changes. It has no UI dependencies.

mod convert;
mod cpal_sink;
mod engine;
mod error;
mod mixer;
mod sink;
mod sound;
mod supervisor;
mod watcher;

#[cfg(test)]
mod test_support;

pub use engine::Engine;
pub use error::{BuildError, LoadError, StartError, WatcherError};
pub use sink::StreamInfo;
pub use sound::Sound;
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
