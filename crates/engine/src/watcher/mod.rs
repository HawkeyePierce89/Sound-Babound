//! Device-change notifications.
//!
//! A [`DeviceWatcher`] registers with the OS for audio device events and
//! forwards them to the supervisor as [`DeviceEvent`]s. Every supervisor
//! starts its own watcher and decides what each event means for its role
//! through [`DeviceEvent::rebuild_policy`].
//!
//! Platform watchers live in this module's platform submodules and are picked
//! by [`platform_watcher`]: CoreAudio property listeners on macOS, an
//! `IMMNotificationClient` on Windows, and the no-op watcher on Linux and every
//! other OS. Stream errors reach the supervisor through the sink either way.

use std::sync::mpsc::Sender;

use crate::error::WatcherError;
use crate::sink::StreamRole;
use crate::supervisor::SupervisorMsg;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use NoopWatcher as PlatformWatcher;
#[cfg(target_os = "linux")]
use linux::PlatformWatcher;
#[cfg(target_os = "macos")]
use macos::CoreAudioWatcher as PlatformWatcher;
#[cfg(target_os = "windows")]
use windows::MmDeviceWatcher as PlatformWatcher;

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

    /// How a supervisor of `role` reacts to this event.
    ///
    /// - Monitor: a default-output change always rebuilds, a default-input
    ///   change is ignored.
    /// - Mic sink: it targets a named device, so default changes of either
    ///   direction are ignored.
    /// - Capture: a default-input change always rebuilds, a default-output
    ///   change is ignored.
    ///
    /// For every role, list, state and removal events are identity-checked.
    pub fn rebuild_policy(self, role: StreamRole) -> RebuildPolicy {
        use RebuildPolicy::{Always, IfIdentityChanged, Ignore};
        match (self, role) {
            (
                DeviceEvent::DeviceListChanged
                | DeviceEvent::DeviceStateChanged
                | DeviceEvent::DeviceRemoved,
                _,
            ) => IfIdentityChanged,
            (DeviceEvent::DefaultOutputChanged, StreamRole::Monitor) => Always,
            (DeviceEvent::DefaultInputChanged, StreamRole::Capture) => Always,
            (DeviceEvent::DefaultOutputChanged, StreamRole::MicSink | StreamRole::Capture)
            | (DeviceEvent::DefaultInputChanged, StreamRole::Monitor | StreamRole::MicSink) => {
                Ignore
            }
        }
    }
}

/// How the supervisor reacts to a [`DeviceEvent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebuildPolicy {
    /// Rebuild after the debounce window.
    Always,
    /// After the debounce window, rebuild only if the identity of the device
    /// the role would pick now differs from the one the active stream was
    /// built on.
    IfIdentityChanged,
    /// Never rebuild.
    Ignore,
}

/// The sending half a watcher uses to report events to the supervisor.
#[derive(Debug, Clone)]
pub struct EventSender(Sender<SupervisorMsg>);

// The no-op watcher never sends, so outside macOS and Windows only tests do.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
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

/// A watcher that never reports anything. The platform watcher everywhere
/// except macOS and Windows.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopWatcher;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct NoopGuard;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl WatcherGuard for NoopGuard {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl DeviceWatcher for NoopWatcher {
    fn start(&mut self, _events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        Ok(Box::new(NoopGuard))
    }
}

/// The watcher for the current platform.
pub fn platform_watcher() -> PlatformWatcher {
    PlatformWatcher
}

#[cfg(test)]
mod tests {
    use super::*;
    use RebuildPolicy::{Always, IfIdentityChanged, Ignore};

    /// The policies of one event, indexed like [`StreamRole::ALL`]:
    /// monitor, mic sink, capture.
    pub(crate) type RolePolicies = [RebuildPolicy; 3];

    const OUTPUT_DEFAULT: RolePolicies = [Always, Ignore, Ignore];
    const INPUT_DEFAULT: RolePolicies = [Ignore, Ignore, Always];
    const IDENTITY: RolePolicies = [IfIdentityChanged, IfIdentityChanged, IfIdentityChanged];

    /// Every OS notification the platform watchers translate, the event it
    /// becomes and the policy that event must have for each role. (Windows'
    /// `OnPropertyValueChanged` is ignored and so emits nothing.) Platform
    /// test modules check their own mapping against this table.
    pub(crate) const PLATFORM_EVENT_TABLE: [(&str, DeviceEvent, RolePolicies); 8] = [
        (
            "macos:DefaultOutputDevice",
            DeviceEvent::DefaultOutputChanged,
            OUTPUT_DEFAULT,
        ),
        (
            "macos:DefaultInputDevice",
            DeviceEvent::DefaultInputChanged,
            INPUT_DEFAULT,
        ),
        ("macos:Devices", DeviceEvent::DeviceListChanged, IDENTITY),
        (
            "windows:OnDefaultDeviceChanged(eRender)",
            DeviceEvent::DefaultOutputChanged,
            OUTPUT_DEFAULT,
        ),
        (
            "windows:OnDefaultDeviceChanged(eCapture)",
            DeviceEvent::DefaultInputChanged,
            INPUT_DEFAULT,
        ),
        (
            "windows:OnDeviceRemoved",
            DeviceEvent::DeviceRemoved,
            IDENTITY,
        ),
        (
            "windows:OnDeviceStateChanged",
            DeviceEvent::DeviceStateChanged,
            IDENTITY,
        ),
        (
            "windows:OnDeviceAdded",
            DeviceEvent::DeviceListChanged,
            IDENTITY,
        ),
    ];

    /// The event a platform notification maps to, according to the table.
    pub(crate) fn table_event(source: &str) -> DeviceEvent {
        PLATFORM_EVENT_TABLE
            .iter()
            .find(|(name, _, _)| *name == source)
            .map(|(_, event, _)| *event)
            .unwrap_or_else(|| panic!("{source} is not in the platform event table"))
    }

    #[test]
    fn table_event_finds_every_source() {
        for (source, event, _) in PLATFORM_EVENT_TABLE {
            assert_eq!(table_event(source), event, "{source}");
        }
    }

    #[test]
    #[should_panic(expected = "not in the platform event table")]
    fn table_event_rejects_unknown_sources() {
        table_event("nowhere:Nothing");
    }

    #[test]
    fn every_platform_event_has_its_policy_for_every_role() {
        for (source, event, policies) in PLATFORM_EVENT_TABLE {
            for (role, policy) in StreamRole::ALL.into_iter().zip(policies) {
                assert_eq!(
                    event.rebuild_policy(role),
                    policy,
                    "{source} -> {event:?} for {role}"
                );
            }
        }
    }

    #[test]
    fn platform_watchers_emit_every_event_variant() {
        for event in DeviceEvent::ALL {
            assert!(
                PLATFORM_EVENT_TABLE.iter().any(|(_, e, _)| *e == event),
                "{event:?} is never emitted"
            );
        }
    }

    #[test]
    fn monitor_policy_is_unchanged_from_stage_one() {
        for event in DeviceEvent::ALL {
            let expected = match event {
                DeviceEvent::DefaultOutputChanged => Always,
                DeviceEvent::DefaultInputChanged => Ignore,
                DeviceEvent::DeviceListChanged => IfIdentityChanged,
                DeviceEvent::DeviceRemoved => IfIdentityChanged,
                DeviceEvent::DeviceStateChanged => IfIdentityChanged,
            };
            assert_eq!(
                event.rebuild_policy(StreamRole::Monitor),
                expected,
                "{event:?}"
            );
        }
    }

    #[test]
    fn mic_sink_ignores_default_changes() {
        for event in DeviceEvent::ALL {
            let expected = match event {
                DeviceEvent::DefaultOutputChanged | DeviceEvent::DefaultInputChanged => Ignore,
                DeviceEvent::DeviceListChanged
                | DeviceEvent::DeviceRemoved
                | DeviceEvent::DeviceStateChanged => IfIdentityChanged,
            };
            assert_eq!(
                event.rebuild_policy(StreamRole::MicSink),
                expected,
                "{event:?}"
            );
        }
    }

    #[test]
    fn capture_follows_the_default_input() {
        for event in DeviceEvent::ALL {
            let expected = match event {
                DeviceEvent::DefaultInputChanged => Always,
                DeviceEvent::DefaultOutputChanged => Ignore,
                DeviceEvent::DeviceListChanged
                | DeviceEvent::DeviceRemoved
                | DeviceEvent::DeviceStateChanged => IfIdentityChanged,
            };
            assert_eq!(
                event.rebuild_policy(StreamRole::Capture),
                expected,
                "{event:?}"
            );
        }
    }

    #[test]
    fn each_role_always_rebuilds_on_at_most_its_own_default() {
        let always = |role| -> Vec<_> {
            DeviceEvent::ALL
                .into_iter()
                .filter(|e| e.rebuild_policy(role) == Always)
                .collect()
        };
        assert_eq!(
            always(StreamRole::Monitor),
            vec![DeviceEvent::DefaultOutputChanged]
        );
        assert_eq!(always(StreamRole::MicSink), vec![]);
        assert_eq!(
            always(StreamRole::Capture),
            vec![DeviceEvent::DefaultInputChanged]
        );
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

    #[test]
    fn several_platform_watchers_run_side_by_side() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let guards: Vec<_> = (0..3)
            .map(|_| {
                platform_watcher()
                    .start(EventSender::new(tx.clone()))
                    .unwrap()
            })
            .collect();
        drop(guards);
    }
}
