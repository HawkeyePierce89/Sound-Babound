//! Device-change notifications.
//!
//! A [`DeviceWatcher`] registers with the OS for audio device events and
//! forwards them to the supervisor as [`DeviceEvent`]s. The supervisor decides
//! what each event means through [`DeviceEvent::rebuild_policy`].
//!
//! Platform watchers live in this module's platform submodules and are picked
//! by [`platform_watcher`]. Until they exist, every platform uses the no-op
//! watcher; stream errors still reach the supervisor through the sink.

use std::sync::mpsc::Sender;

use crate::error::WatcherError;
use crate::supervisor::SupervisorMsg;

/// An audio device change reported by the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceEvent {
    /// The system default output device changed.
    DefaultOutputChanged,
    /// The system default input device changed.
    DefaultInputChanged,
    /// A device was added, or the device list changed in some other way.
    DeviceListChanged,
    /// A device was removed.
    DeviceRemoved,
    /// A device changed state (enabled, disabled, unplugged, ...).
    DeviceStateChanged,
}

impl DeviceEvent {
    /// Every variant, for exhaustive tests.
    pub const ALL: [DeviceEvent; 5] = [
        DeviceEvent::DefaultOutputChanged,
        DeviceEvent::DefaultInputChanged,
        DeviceEvent::DeviceListChanged,
        DeviceEvent::DeviceRemoved,
        DeviceEvent::DeviceStateChanged,
    ];

    /// Whether this event should rebuild the output stream.
    pub fn rebuild_policy(self) -> RebuildPolicy {
        match self {
            DeviceEvent::DefaultOutputChanged => RebuildPolicy::Always,
            DeviceEvent::DeviceListChanged
            | DeviceEvent::DeviceStateChanged
            | DeviceEvent::DeviceRemoved => RebuildPolicy::IfDefaultOutputChanged,
            DeviceEvent::DefaultInputChanged => RebuildPolicy::Ignore,
        }
    }
}

/// How the supervisor reacts to a [`DeviceEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebuildPolicy {
    /// Rebuild after the debounce window.
    Always,
    /// After the debounce window, rebuild only if the identity of the current
    /// default output differs from the one the active stream was built on.
    IfDefaultOutputChanged,
    /// Never rebuild (input-device events).
    Ignore,
}

/// The sending half a watcher uses to report events to the supervisor.
#[derive(Debug, Clone)]
pub struct EventSender(Sender<SupervisorMsg>);

impl EventSender {
    pub(crate) fn new(sender: Sender<SupervisorMsg>) -> EventSender {
        EventSender(sender)
    }

    /// Report an event. Never blocks. Returns false once the supervisor is gone.
    pub fn send(&self, event: DeviceEvent) -> bool {
        self.0.send(SupervisorMsg::Device(event)).is_ok()
    }
}

/// Keeps a watcher registered. Dropping it unregisters.
pub trait WatcherGuard {}

/// A source of device-change notifications.
///
/// `start` runs on the supervisor thread, which also owns and drops the guard.
pub trait DeviceWatcher: Send + 'static {
    /// Register for notifications, delivering them through `events`.
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError>;
}

impl<T: DeviceWatcher + ?Sized> DeviceWatcher for Box<T> {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        (**self).start(events)
    }
}

/// A watcher that never reports anything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWatcher;

struct NoopGuard;

impl WatcherGuard for NoopGuard {}

impl DeviceWatcher for NoopWatcher {
    fn start(&mut self, _events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        Ok(Box::new(NoopGuard))
    }
}

/// The watcher for the current platform.
pub fn platform_watcher() -> Box<dyn DeviceWatcher> {
    Box::new(NoopWatcher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_maps_to_its_policy() {
        for event in DeviceEvent::ALL {
            let expected = match event {
                DeviceEvent::DefaultOutputChanged => RebuildPolicy::Always,
                DeviceEvent::DefaultInputChanged => RebuildPolicy::Ignore,
                DeviceEvent::DeviceListChanged => RebuildPolicy::IfDefaultOutputChanged,
                DeviceEvent::DeviceRemoved => RebuildPolicy::IfDefaultOutputChanged,
                DeviceEvent::DeviceStateChanged => RebuildPolicy::IfDefaultOutputChanged,
            };
            assert_eq!(event.rebuild_policy(), expected, "{event:?}");
        }
    }

    #[test]
    fn only_default_output_change_always_rebuilds() {
        let always: Vec<_> = DeviceEvent::ALL
            .into_iter()
            .filter(|e| e.rebuild_policy() == RebuildPolicy::Always)
            .collect();
        assert_eq!(always, vec![DeviceEvent::DefaultOutputChanged]);
    }

    #[test]
    fn event_sender_delivers_and_reports_disconnect() {
        let (tx, rx) = std::sync::mpsc::channel();
        let sender = EventSender::new(tx);
        assert!(sender.send(DeviceEvent::DeviceRemoved));
        assert!(matches!(
            rx.recv().unwrap(),
            SupervisorMsg::Device(DeviceEvent::DeviceRemoved)
        ));
        drop(rx);
        assert!(!sender.send(DeviceEvent::DeviceRemoved));
    }

    #[test]
    fn platform_watcher_starts_and_stops() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut watcher = platform_watcher();
        let guard = watcher.start(EventSender::new(tx)).unwrap();
        drop(guard);
    }
}
