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

/// A failure while building a stream.
#[derive(Debug, Error)]
pub enum BuildError {
    /// There is no device for the stream (no default output for the
    /// monitor, no default input for the capture).
    #[error("no audio device")]
    NoDevice,
    /// No output device matches the mic sink's device pattern.
    #[error("no output device matches {0:?}")]
    DeviceNotFound(String),
    /// The default input is the virtual mic device (or its monitor) and no
    /// physical input to fall back to is present, so capturing it would feed
    /// the mic path back into itself.
    #[error("default input {0:?} is the virtual mic device; capture refused")]
    FeedbackExcluded(String),
    /// The device's sample format is not supported by the engine.
    #[error("unsupported sample format: {0}")]
    UnsupportedFormat(String),
    /// The audio backend reported an error.
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// What kind of [`BuildError`] a build failed with, without its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuildErrorKind {
    /// [`BuildError::NoDevice`].
    NoDevice,
    /// [`BuildError::DeviceNotFound`].
    DeviceNotFound,
    /// [`BuildError::FeedbackExcluded`].
    FeedbackExcluded,
    /// [`BuildError::UnsupportedFormat`].
    UnsupportedFormat,
    /// [`BuildError::Backend`].
    Backend,
}

impl BuildError {
    /// The kind of this error.
    pub fn kind(&self) -> BuildErrorKind {
        match self {
            BuildError::NoDevice => BuildErrorKind::NoDevice,
            BuildError::DeviceNotFound(_) => BuildErrorKind::DeviceNotFound,
            BuildError::FeedbackExcluded(_) => BuildErrorKind::FeedbackExcluded,
            BuildError::UnsupportedFormat(_) => BuildErrorKind::UnsupportedFormat,
            BuildError::Backend(_) => BuildErrorKind::Backend,
        }
    }
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
        assert_eq!(BuildError::NoDevice.to_string(), "no audio device");
        let not_found = BuildError::DeviceNotFound("BlackHole".into());
        assert!(not_found.to_string().contains("BlackHole"));
        assert_eq!(not_found.kind(), BuildErrorKind::DeviceNotFound);
        let excluded = BuildError::FeedbackExcluded("Sound-Babound Mic".into());
        assert!(excluded.to_string().contains("Sound-Babound Mic"));
        assert_eq!(excluded.kind(), BuildErrorKind::FeedbackExcluded);
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
        assert_eq!(BuildError::NoDevice.kind(), BuildErrorKind::NoDevice);
        assert_eq!(
            BuildError::UnsupportedFormat("i64".into()).kind(),
            BuildErrorKind::UnsupportedFormat
        );
        assert_eq!(
            BuildError::Backend("boom".into()).kind(),
            BuildErrorKind::Backend
        );
        assert!(WatcherError("nope".into()).to_string().contains("nope"));
        assert!(
            StartError::Spawn(std::io::Error::other("no threads"))
                .to_string()
                .contains("supervisor thread")
        );
    }
}
