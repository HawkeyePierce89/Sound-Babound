//! macOS device watcher built on CoreAudio property listeners.
//!
//! Listeners are registered on `kAudioObjectSystemObject` for the default
//! output device, the default input device and the device list. CoreAudio
//! calls them on one of its own threads; the listener only maps the changed
//! selectors to [`DeviceEvent`]s and sends them, which never blocks.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, AudioObjectRemovePropertyListener,
    kAudioHardwarePropertyDefaultInputDevice, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use tracing::warn;

use super::{DeviceEvent, DeviceWatcher, EventSender, WatcherGuard};
use crate::error::WatcherError;

/// The properties of the system object the watcher listens to.
const SELECTORS: [AudioObjectPropertySelector; 3] = [
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDevices,
];

const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// Map a changed system-object property to the event it reports.
///
/// `Devices` covers both plugging and unplugging; it maps to
/// [`DeviceEvent::DeviceListChanged`], which the supervisor only acts on when
/// the default output's identity changed.
// The CoreAudio constant names are matched as-is.
#[allow(non_upper_case_globals)]
pub(crate) fn event_for_selector(selector: AudioObjectPropertySelector) -> Option<DeviceEvent> {
    match selector {
        kAudioHardwarePropertyDefaultOutputDevice => Some(DeviceEvent::DefaultOutputChanged),
        kAudioHardwarePropertyDefaultInputDevice => Some(DeviceEvent::DefaultInputChanged),
        kAudioHardwarePropertyDevices => Some(DeviceEvent::DeviceListChanged),
        _ => None,
    }
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// The CoreAudio listener. `client_data` is the `EventSender` boxed by
/// [`CoreAudioWatcher::start`], alive until the guard has removed every
/// listener.
unsafe extern "C-unwind" fn listener(
    _object: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    // SAFETY: client_data is the `Box<EventSender>` pointer registered in
    // `start`; the guard frees it only after removing all listeners.
    let events = unsafe { &*(client_data as *const EventSender) };
    // SAFETY: CoreAudio passes `count` valid addresses.
    let addresses = unsafe { std::slice::from_raw_parts(addresses.as_ptr(), count as usize) };
    for address in addresses {
        if let Some(event) = event_for_selector(address.mSelector) {
            // A send failure means the supervisor is shutting down; the guard
            // will unregister shortly.
            let _ = events.send(event);
        }
    }
    0
}

/// Watches CoreAudio's system object for device changes.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoreAudioWatcher;

struct CoreAudioGuard {
    /// The boxed `EventSender` passed to CoreAudio as client data.
    client_data: *mut EventSender,
    /// The selectors whose listener was added successfully.
    registered: Vec<AudioObjectPropertySelector>,
}

impl CoreAudioGuard {
    fn remove_all(&mut self) {
        for selector in self.registered.drain(..) {
            let address = address(selector);
            // SAFETY: the address is valid for the call, and the listener and
            // client data are the pair that was registered.
            let status = unsafe {
                AudioObjectRemovePropertyListener(
                    SYSTEM_OBJECT,
                    NonNull::from(&address),
                    Some(listener),
                    self.client_data.cast(),
                )
            };
            if status != 0 {
                warn!(selector, status, "removing CoreAudio listener failed");
            }
        }
    }
}

impl WatcherGuard for CoreAudioGuard {}

impl Drop for CoreAudioGuard {
    fn drop(&mut self) {
        self.remove_all();
        if !self.client_data.is_null() {
            // SAFETY: created by `Box::into_raw` in `start` and freed only here,
            // after every listener using it was removed.
            drop(unsafe { Box::from_raw(self.client_data) });
            self.client_data = std::ptr::null_mut();
        }
    }
}

impl DeviceWatcher for CoreAudioWatcher {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        let mut guard = CoreAudioGuard {
            client_data: Box::into_raw(Box::new(events)),
            registered: Vec::with_capacity(SELECTORS.len()),
        };
        for selector in SELECTORS {
            let address = address(selector);
            // SAFETY: the address is valid for the call; the client data stays
            // alive until the guard removes this listener.
            let status = unsafe {
                AudioObjectAddPropertyListener(
                    SYSTEM_OBJECT,
                    NonNull::from(&address),
                    Some(listener),
                    guard.client_data.cast(),
                )
            };
            if status != 0 {
                // Dropping the guard removes the listeners added so far.
                return Err(WatcherError(format!(
                    "AudioObjectAddPropertyListener({selector:#x}) failed with status {status}"
                )));
            }
            guard.registered.push(selector);
        }
        Ok(Box::new(guard))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::RebuildPolicy;
    use crate::watcher::tests::table_event;

    #[test]
    fn selectors_map_to_events() {
        assert_eq!(
            event_for_selector(kAudioHardwarePropertyDefaultOutputDevice),
            Some(DeviceEvent::DefaultOutputChanged)
        );
        assert_eq!(
            event_for_selector(kAudioHardwarePropertyDefaultInputDevice),
            Some(DeviceEvent::DefaultInputChanged)
        );
        assert_eq!(
            event_for_selector(kAudioHardwarePropertyDevices),
            Some(DeviceEvent::DeviceListChanged)
        );
        assert_eq!(event_for_selector(0), None);
    }

    #[test]
    fn selectors_match_the_platform_event_table() {
        let named = [
            (
                kAudioHardwarePropertyDefaultOutputDevice,
                "macos:DefaultOutputDevice",
            ),
            (
                kAudioHardwarePropertyDefaultInputDevice,
                "macos:DefaultInputDevice",
            ),
            (kAudioHardwarePropertyDevices, "macos:Devices"),
        ];
        assert_eq!(named.len(), SELECTORS.len());
        for (selector, source) in named {
            assert!(SELECTORS.contains(&selector));
            assert_eq!(event_for_selector(selector), Some(table_event(source)));
        }
        assert_eq!(
            event_for_selector(kAudioHardwarePropertyDevices).map(DeviceEvent::rebuild_policy),
            Some(RebuildPolicy::IfDefaultOutputChanged)
        );
    }

    #[test]
    fn listener_forwards_every_address() {
        let (tx, rx) = std::sync::mpsc::channel();
        let client = Box::into_raw(Box::new(EventSender::new(tx)));
        let addresses = [
            address(kAudioHardwarePropertyDefaultOutputDevice),
            address(0x1234),
            address(kAudioHardwarePropertyDevices),
        ];
        let status = unsafe {
            listener(
                SYSTEM_OBJECT,
                addresses.len() as u32,
                NonNull::from(&addresses[0]),
                client.cast(),
            )
        };
        assert_eq!(status, 0);
        drop(unsafe { Box::from_raw(client) });
        let got: Vec<_> = rx
            .try_iter()
            .map(|msg| match msg {
                crate::supervisor::SupervisorMsg::Device(event) => event,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                DeviceEvent::DefaultOutputChanged,
                DeviceEvent::DeviceListChanged
            ]
        );
    }

    #[test]
    fn start_and_drop_guard() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = CoreAudioWatcher;
        let guard = watcher.start(EventSender::new(tx)).unwrap();
        drop(guard);
        // The guard freed the boxed sender, so the channel is disconnected.
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn start_twice_and_drop_in_any_order() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut watcher = CoreAudioWatcher;
        let first = watcher.start(EventSender::new(tx.clone())).unwrap();
        let second = watcher.start(EventSender::new(tx)).unwrap();
        drop(first);
        drop(second);
    }
}
