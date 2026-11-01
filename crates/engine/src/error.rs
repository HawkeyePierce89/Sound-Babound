//! Typed engine errors.

use thiserror::Error;

/// A failure while loading and decoding a sound.
///
/// Every variant carries the label of the source (file path or the label
/// given to [`crate::Sound::load_bytes`]) so the caller can report which
/// sound failed.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The source could not be read.
    #[error("{label}: cannot read: {source}")]
    Io {
        label: String,
        #[source]
        source: std::io::Error,
    },
    /// The container or codec is not supported (or the data is not audio).
    #[error("{label}: unsupported format: {reason}")]
    Unsupported { label: String, reason: String },
    /// The container has no decodable audio track.
    #[error("{label}: no audio track")]
    NoAudioTrack { label: String },
    /// Decoding failed before any audio could be produced.
    #[error("{label}: decode error: {reason}")]
    Decode { label: String, reason: String },
    /// The source decoded to zero audio frames.
    #[error("{label}: no audio frames")]
    Empty { label: String },
}

impl LoadError {
    /// The label of the source that failed.
    pub fn label(&self) -> &str {
        match self {
            LoadError::Io { label, .. }
            | LoadError::Unsupported { label, .. }
            | LoadError::NoAudioTrack { label }
            | LoadError::Decode { label, .. }
            | LoadError::Empty { label } => label,
        }
    }
}

/// A failure while building an output stream.
#[derive(Debug, Error)]
pub enum BuildError {
    /// There is no output device.
    #[error("no output device")]
    NoDevice,
    /// The device's sample format is not supported by the engine.
    #[error("unsupported sample format: {0}")]
    UnsupportedFormat(String),
    /// The audio backend reported an error.
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// A failure while registering for device-change notifications.
#[derive(Debug, Error)]
#[error("device watcher failed: {0}")]
pub struct WatcherError(pub String);

/// A failure while starting the [`crate::Engine`].
#[derive(Debug, Error)]
pub enum StartError {
    /// The supervisor thread could not be spawned.
    #[error("cannot start the audio supervisor thread: {0}")]
    Spawn(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_error_messages_name_the_source() {
        let err = LoadError::Empty {
            label: "clip.wav".into(),
        };
        assert_eq!(err.label(), "clip.wav");
        assert!(err.to_string().contains("clip.wav"));

        let err = LoadError::Io {
            label: "/x/missing.wav".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        assert_eq!(err.label(), "/x/missing.wav");
        assert!(err.to_string().contains("/x/missing.wav"));
    }

    #[test]
    fn build_error_messages() {
        assert_eq!(BuildError::NoDevice.to_string(), "no output device");
        assert!(
            BuildError::UnsupportedFormat("i64".into())
                .to_string()
                .contains("i64")
        );
        assert!(
            BuildError::Backend("boom".into())
                .to_string()
                .contains("boom")
        );
        assert!(WatcherError("nope".into()).to_string().contains("nope"));
        assert!(
            StartError::Spawn(std::io::Error::other("no threads"))
                .to_string()
                .contains("supervisor thread")
        );
    }
}
