//! Linux device watcher: a default-input poller for the capture, nothing for
//! the outputs.
//!
//! The monitor opens ALSA's `default` PCM, which on current desktops routes
//! through PipeWire or PulseAudio. Those servers move the stream when the
//! default sink changes, so there is nothing to rebuild on a default switch.
//! The mic sink targets a named device and ignores default changes anyway.
//!
//! The capture is different: cpal's PulseAudio host resolves the default
//! source when the stream is built and stays on it. So the capture's watcher
//! asks the PulseAudio host for the default input and the input list every
//! [`POLL_INTERVAL`]. A new default input is reported as
//! [`DeviceEvent::DefaultInputChanged`], which the capture always rebuilds
//! on; any other change to the list as [`DeviceEvent::DeviceListChanged`],
//! which makes the supervisor check the identity a build would pick now.
//! That catches a fallback input that went away (the server would move the
//! stream to the default source, the excluded virtual device, without an
//! error). When a device really goes away, the stream may also report an
//! error, which still reaches the supervisor through the error callback.
//!
//! The poller asks the process-wide PulseAudio host of
//! [`platform::mic_host`] afresh on every poll, so no device is cached. It
//! never opens a host of its own: the pinned PulseAudio client's reactor
//! thread blocks on its socket and is not woken when the client drops, so
//! every dropped host would leave a thread and a connection behind, per poll
//! or per mic restart. It keeps its handle between polls and asks
//! `mic_host` again only after the connection fails (in the same poll, which
//! reconnects the shared host), or while the mic host falls back to a
//! non-PulseAudio host.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::HostId;
use cpal::traits::HostTrait;

use super::inputs::{InputHost, InputsQuery, InputsSnapshot, change_event};
use super::{DeviceWatcher, EventSender, NoopWatcher, WatcherGuard, poll};
use crate::cpal_sink::identity_of;
use crate::error::WatcherError;
use crate::platform;
use crate::sink::StreamRole;

/// How often the capture's watcher re-reads the inputs.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How long the poller's start and stop wait for a query. A PulseAudio
/// request has no timeout of its own, so a server that stops answering would
/// otherwise block the capture supervisor, and with it mic disable and
/// shutdown. Matches cpal's PulseAudio connect timeout.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared by every input poller in the process, so a poller left behind with
/// a hung query makes the next one wait instead of opening another
/// connection: hung pollers never pile up across mic restarts.
static QUERY_GATE: Mutex<()> = Mutex::new(());

/// The watcher of `role`: the input poller for the capture, the no-op
/// watcher for the monitor and the mic sink.
pub fn platform_watcher(role: StreamRole) -> Box<dyn DeviceWatcher> {
    match role {
        StreamRole::Capture => Box::new(InputPoller),
        StreamRole::Monitor | StreamRole::MicSink => Box::new(NoopWatcher),
    }
}

/// Polls the mic host's default input and input list.
#[derive(Debug, Default, Clone, Copy)]
struct InputPoller;

impl DeviceWatcher for InputPoller {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        let mut query = InputsQuery::new(platform::mic_host);
        poll::start(
            POLL_INTERVAL,
            STOP_TIMEOUT,
            &QUERY_GATE,
            move || query.poll(),
            change_event,
            events,
        )
    }
}

impl InputHost for Arc<cpal::Host> {
    /// Only the shared PulseAudio host is kept; a fallback host is not, so
    /// the next poll tries PulseAudio again.
    fn keep(&self) -> bool {
        self.id() == HostId::PulseAudio
    }

    /// A failed input list means the connection is gone.
    fn inputs(&self) -> Option<InputsSnapshot> {
        let inputs = self
            .input_devices()
            .ok()?
            .map(|device| identity_of(&device))
            .collect();
        let default = self
            .default_input_device()
            .map(|device| identity_of(&device));
        Some(InputsSnapshot::new(default, inputs))
    }
}
