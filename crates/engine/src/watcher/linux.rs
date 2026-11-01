//! Linux device watcher: a default-input poller for the capture, an
//! output-list poller for the mic sink, nothing for the monitor.
//!
//! The monitor opens ALSA's `default` PCM, which on current desktops routes
//! through PipeWire or PulseAudio. Those servers move the stream when the
//! default sink changes, so there is nothing to rebuild on a default switch.
//!
//! The mic sink targets a named device and ignores default changes. But when
//! that device goes away while the sound server keeps running (its module is
//! unloaded), the server moves the mic sink's stream to the default sink
//! without an error, so the call mix, live microphone included, would play
//! on the user's speakers. So the mic sink's watcher asks the PulseAudio
//! host for the output list every [`POLL_INTERVAL`] and reports a change as
//! [`DeviceEvent::DeviceListChanged`]: the supervisor finds the device gone,
//! drops the stream and reports it not found, which lets the app create the
//! device again.
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
//! Each poller asks the process-wide PulseAudio host of
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

use super::inputs::{InputHost, InputsQuery, InputsSnapshot, change_event, list_change_event};
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

/// The same for every output poller. A gate of its own, so the two pollers
/// running side by side never make each other skip a poll.
static OUTPUT_QUERY_GATE: Mutex<()> = Mutex::new(());

/// The watcher of `role`: the input poller for the capture, the output
/// poller for the mic sink and the no-op watcher for the monitor.
pub fn platform_watcher(role: StreamRole) -> Box<dyn DeviceWatcher> {
    match role {
        StreamRole::Capture => Box::new(InputPoller),
        StreamRole::MicSink => Box::new(OutputPoller),
        StreamRole::Monitor => Box::new(NoopWatcher),
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

impl InputHost for Option<Arc<cpal::Host>> {
    /// Only the shared PulseAudio host is kept; a fallback host is not, so
    /// the next poll tries PulseAudio again.
    fn keep(&self) -> bool {
        self.as_ref()
            .is_some_and(|host| host.id() == HostId::PulseAudio)
    }

    /// A failed input list (or no host) means the connection is gone.
    fn inputs(&self) -> Option<InputsSnapshot> {
        let host = self.as_ref()?;
        let inputs = host
            .input_devices()
            .ok()?
            .map(|device| identity_of(&device))
            .collect();
        let default = host
            .default_input_device()
            .map(|device| identity_of(&device));
        Some(InputsSnapshot::new(default, inputs))
    }
}

/// Polls the mic host's output list.
#[derive(Debug, Default, Clone, Copy)]
struct OutputPoller;

impl DeviceWatcher for OutputPoller {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        let mut query = InputsQuery::new(|| OutputList(platform::mic_host()));
        poll::start(
            POLL_INTERVAL,
            STOP_TIMEOUT,
            &OUTPUT_QUERY_GATE,
            move || query.poll(),
            list_change_event,
            events,
        )
    }
}

/// The mic host asked for its outputs: a snapshot with no default whose list
/// holds the outputs.
struct OutputList(Option<Arc<cpal::Host>>);

impl InputHost for OutputList {
    /// Only the shared PulseAudio host is kept, as for the inputs.
    fn keep(&self) -> bool {
        self.0.keep()
    }

    /// A failed output list (or no host) means the connection is gone.
    fn inputs(&self) -> Option<InputsSnapshot> {
        let outputs = self
            .0
            .as_ref()?
            .output_devices()
            .ok()?
            .map(|device| identity_of(&device))
            .collect();
        Some(InputsSnapshot::new(None, outputs))
    }
}
