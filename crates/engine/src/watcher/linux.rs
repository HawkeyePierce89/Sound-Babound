//! Linux device watcher: a no-op.
//!
//! cpal opens ALSA's `default` PCM, which on current desktops routes through
//! PipeWire or PulseAudio. Those servers move the stream when the default sink
//! changes, so there is nothing to rebuild on a default switch. When the
//! device really goes away, the stream reports an error, which still reaches
//! the supervisor through the sink's error callback.

pub use super::NoopWatcher as PlatformWatcher;
