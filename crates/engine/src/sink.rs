//! Output sinks.
//!
//! A [`SinkFactory`] builds an [`ActiveSink`] (a running output stream) on
//! the current default device. The supervisor owns both, drops the sink to
//! stop the stream and asks the factory for a new one when the device changes.

use std::fmt;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use crate::error::BuildError;
use crate::mixer::Mixer;
use crate::supervisor::SupervisorMsg;

/// An opaque, comparable identity of an output device.
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

/// What a sink was built on.
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

/// A running output stream. Dropping it stops the stream.
///
/// It is not `Send`: the supervisor thread builds, owns and drops it.
pub trait ActiveSink {}

/// A successful [`SinkFactory::build`].
pub type BuiltSink = (Box<dyn ActiveSink>, StreamInfo);

/// Builds output streams on the current default device.
pub trait SinkFactory: Send + 'static {
    /// Build and start a stream on the current default output device.
    ///
    /// It is called only while no stream exists, so the factory may lock
    /// `mixer` freely before starting the stream; it must call
    /// [`Mixer::configure`] with the stream's rate and channel count before
    /// the stream starts. Once the stream runs, only its callback touches the
    /// mixer (with `try_lock`). Stream errors are reported by sending
    /// [`SupervisorMsg::StreamError`] on `on_error`.
    fn build(
        &mut self,
        mixer: Arc<Mutex<Mixer>>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltSink, BuildError>;

    /// Re-query the live default output device and return its identity.
    /// `None` means there is no output device. Nothing is cached.
    fn current_default_identity(&mut self) -> Option<DeviceIdentity>;
}

impl<T: SinkFactory + ?Sized> SinkFactory for Box<T> {
    fn build(
        &mut self,
        mixer: Arc<Mutex<Mixer>>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltSink, BuildError> {
        (**self).build(mixer, on_error)
    }

    fn current_default_identity(&mut self) -> Option<DeviceIdentity> {
        (**self).current_default_identity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_compare_by_value() {
        let a = DeviceIdentity::new("speakers");
        assert_eq!(a, DeviceIdentity::new(String::from("speakers")));
        assert_ne!(a, DeviceIdentity::new("headset"));
        assert_eq!(a.as_str(), "speakers");
        assert_eq!(a.to_string(), "speakers");
    }
}
