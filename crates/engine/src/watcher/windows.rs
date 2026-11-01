//! Windows device watcher built on `IMMNotificationClient`.
//!
//! A dedicated thread joins the multithreaded apartment, creates the
//! `IMMDeviceEnumerator` and registers the notification client. COM calls the
//! client on its own threads; the client only maps the callback to a
//! [`DeviceEvent`] and sends it, which never blocks.
//!
//! Removal, state and add events are not filtered here. The supervisor's
//! identity check decides whether they rebuild the stream.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use tracing::warn;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows_core::{PCWSTR, implement};

use super::{DeviceEvent, DeviceWatcher, EventSender, WatcherGuard};
use crate::error::WatcherError;

/// Map `OnDefaultDeviceChanged` to an event.
///
/// Windows reports one change per role; only the console role is used so a
/// single switch produces a single event.
pub(crate) fn default_change_event(flow: EDataFlow, role: ERole) -> Option<DeviceEvent> {
    if role != eConsole {
        return None;
    }
    if flow == eRender {
        Some(DeviceEvent::DefaultOutputChanged)
    } else if flow == eCapture {
        Some(DeviceEvent::DefaultInputChanged)
    } else {
        None
    }
}

#[implement(IMMNotificationClient)]
struct NotificationClient {
    events: EventSender,
}

impl NotificationClient {
    fn emit(&self, event: DeviceEvent) {
        // A send failure means the supervisor is shutting down; the guard
        // will unregister shortly.
        let _ = self.events.send(event);
    }
}

impl IMMNotificationClient_Impl for NotificationClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows_core::Result<()> {
        self.emit(DeviceEvent::DeviceStateChanged);
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows_core::Result<()> {
        self.emit(DeviceEvent::DeviceListChanged);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows_core::Result<()> {
        self.emit(DeviceEvent::DeviceRemoved);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows_core::Result<()> {
        if let Some(event) = default_change_event(flow, role) {
            self.emit(event);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows_core::Result<()> {
        Ok(())
    }
}

/// Watches the MMDevice API for endpoint changes.
#[derive(Debug, Default, Clone, Copy)]
pub struct MmDeviceWatcher;

struct MmDeviceGuard {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl WatcherGuard for MmDeviceGuard {}

impl Drop for MmDeviceGuard {
    fn drop(&mut self) {
        // Dropping the sender wakes the thread, which unregisters, releases
        // its COM objects and calls `CoUninitialize`.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("device watcher thread panicked");
        }
    }
}

/// Uninitializes COM on the watcher thread when dropped, after every COM
/// object declared later in the same scope has been released.
struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: paired with the successful `CoInitializeEx` on this thread.
        unsafe { CoUninitialize() };
    }
}

fn watcher_thread(
    events: EventSender,
    ready: Sender<Result<(), WatcherError>>,
    stop: Receiver<()>,
) {
    // SAFETY: called once on a fresh thread.
    let init = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(err) = init.ok() {
        let _ = ready.send(Err(WatcherError(format!("CoInitializeEx failed: {err}"))));
        return;
    }
    let _apartment = ComApartment;

    // SAFETY: COM is initialized on this thread.
    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
            Ok(enumerator) => enumerator,
            Err(err) => {
                let _ = ready.send(Err(WatcherError(format!(
                    "creating IMMDeviceEnumerator failed: {err}"
                ))));
                return;
            }
        };
    let client: IMMNotificationClient = NotificationClient { events }.into();
    // SAFETY: the enumerator and client are valid COM objects.
    if let Err(err) = unsafe { enumerator.RegisterEndpointNotificationCallback(&client) } {
        let _ = ready.send(Err(WatcherError(format!(
            "RegisterEndpointNotificationCallback failed: {err}"
        ))));
        return;
    }
    let _ = ready.send(Ok(()));

    // Blocks until the guard drops its sender (or sends).
    let _ = stop.recv();

    // SAFETY: the same client that was registered above.
    if let Err(err) = unsafe { enumerator.UnregisterEndpointNotificationCallback(&client) } {
        warn!(%err, "UnregisterEndpointNotificationCallback failed");
    }
    drop(client);
    drop(enumerator);
    // `_apartment` drops last and calls `CoUninitialize`.
}

impl DeviceWatcher for MmDeviceWatcher {
    fn start(&mut self, events: EventSender) -> Result<Box<dyn WatcherGuard>, WatcherError> {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("babound-device-watcher".into())
            .spawn(move || watcher_thread(events, ready_tx, stop_rx))
            .map_err(|err| WatcherError(format!("spawning the watcher thread failed: {err}")))?;
        let guard = MmDeviceGuard {
            stop: Some(stop_tx),
            thread: Some(thread),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Box::new(guard)),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(WatcherError(
                "the watcher thread exited before registering".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervisor::SupervisorMsg;
    use crate::watcher::tests::table_event;
    use windows::Win32::Media::Audio::{eAll, eCommunications, eMultimedia};

    #[test]
    fn default_changes_map_only_for_console_role() {
        assert_eq!(
            default_change_event(eRender, eConsole),
            Some(DeviceEvent::DefaultOutputChanged)
        );
        assert_eq!(
            default_change_event(eCapture, eConsole),
            Some(DeviceEvent::DefaultInputChanged)
        );
        assert_eq!(default_change_event(eRender, eMultimedia), None);
        assert_eq!(default_change_event(eRender, eCommunications), None);
        assert_eq!(default_change_event(eAll, eConsole), None);
    }

    #[test]
    fn callbacks_match_the_platform_event_table() {
        assert_eq!(
            default_change_event(eRender, eConsole),
            Some(table_event("windows:OnDefaultDeviceChanged(eRender)"))
        );
        assert_eq!(
            default_change_event(eCapture, eConsole),
            Some(table_event("windows:OnDefaultDeviceChanged(eCapture)"))
        );

        let (tx, rx) = mpsc::channel();
        let client: IMMNotificationClient = NotificationClient {
            events: EventSender::new(tx),
        }
        .into();
        let id = PCWSTR::null();
        unsafe {
            client.OnDeviceRemoved(id).unwrap();
            client.OnDeviceStateChanged(id, DEVICE_STATE(0)).unwrap();
            client.OnDeviceAdded(id).unwrap();
            client
                .OnPropertyValueChanged(id, PROPERTYKEY::default())
                .unwrap();
        }
        drop(client);
        let got: Vec<_> = rx
            .try_iter()
            .map(|msg| match msg {
                SupervisorMsg::Device(event) => event,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            vec![
                table_event("windows:OnDeviceRemoved"),
                table_event("windows:OnDeviceStateChanged"),
                table_event("windows:OnDeviceAdded"),
            ]
        );
    }

    #[test]
    fn start_and_drop_guard() {
        let (tx, _rx) = mpsc::channel();
        let mut watcher = MmDeviceWatcher;
        let guard = watcher.start(EventSender::new(tx)).unwrap();
        drop(guard);
    }
}
