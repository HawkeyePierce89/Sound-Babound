//! The virtual device the app creates itself (on Linux), kept in step with
//! the mic path.
//!
//! Creating, checking and unloading the device run `pactl`, which can take
//! seconds (each call is bounded by a timeout, but the bound is long), so
//! they run on a [`DeviceWorker`] thread that owns the device. The tray's
//! event loop keeps the mic path's state in [`WithVirtualMic`], sends the
//! worker [`DeviceRequest`]s and hands its [`DeviceReply`]s back to
//! [`WithVirtualMic::on_reply`].

use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use babound_engine::{MicSettings, Sound};

use crate::actions::AudioControl;
use crate::platform;

/// How often [`WithVirtualMic::restore_device`] asks whether the virtual
/// device still exists. Each check runs `pactl` on the device worker.
pub const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// How long [`DeviceWorker::finish_within`] waits for the worker at
/// shutdown. It bounds how long quitting and turning the mic off may freeze
/// the event loop; it deliberately does not cover the worst case. A create is
/// a `list`, up to two `unload`s and two `load`s at 3 s each, about 15 to
/// 18 s, and a `Release` queued behind it brings that to about 27 s. When the
/// timeout fires, the worker is left running, the device may stay loaded, and
/// the next start unloads it as stale.
pub const WORKER_FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// A virtual device the app created itself, which can vanish behind its
/// back: a sound server restart drops the modules behind the Linux one.
pub trait VirtualDevice {
    /// Whether the device still exists; true when that cannot be told.
    fn is_present(&mut self) -> bool;
}

impl VirtualDevice for platform::VirtualMic {
    fn is_present(&mut self) -> bool {
        platform::VirtualMic::is_present(self)
    }
}

/// What the device worker is asked to do. It handles requests in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRequest {
    /// Create the device, or reuse the one the worker holds. Replies
    /// [`DeviceReply::Created`] with the same epoch.
    Create { epoch: u64 },
    /// Ask whether the held device still exists. Replies
    /// [`DeviceReply::Checked`] with the same epoch.
    Check { epoch: u64 },
    /// Drop the held device, which unloads it. Replies
    /// [`DeviceReply::Released`].
    Release,
    /// Forget the held device without unloading it, for a shutdown that left
    /// a mic stream running (the next start unloads it as stale). No reply.
    Forget,
}

/// The device worker's answer to a [`DeviceRequest`].
#[derive(Debug)]
pub enum DeviceReply {
    /// The answer to `Create`: whether a device exists (false where the user
    /// installs it), or why it could not be created.
    Created { epoch: u64, result: Result<bool> },
    /// The answer to `Check`.
    Checked { epoch: u64, present: bool },
    /// The answer to `Release`.
    Released,
}

/// Where [`WithVirtualMic`] sends its requests: the [`DeviceWorker`] in
/// production, a recording fake in tests.
pub trait DeviceRequests {
    /// Queues a request; never blocks.
    fn send(&mut self, request: DeviceRequest);
    /// Lets the requests sent so far finish, waiting a bounded time.
    fn finish(self);
}

/// The device side of the worker: it owns the device and the function that
/// creates it, and answers requests. Pure, so tests can run it inline.
struct DeviceHost<G, F> {
    device: Option<G>,
    create: F,
}

impl<G, F> DeviceHost<G, F>
where
    G: VirtualDevice,
    F: FnMut() -> Result<Option<G>>,
{
    fn new(create: F) -> Self {
        DeviceHost {
            device: None,
            create,
        }
    }

    fn handle(&mut self, request: DeviceRequest) -> Option<DeviceReply> {
        match request {
            DeviceRequest::Create { epoch } => {
                let result = if self.device.is_some() {
                    Ok(true)
                } else {
                    (self.create)().map(|device| {
                        self.device = device;
                        self.device.is_some()
                    })
                };
                Some(DeviceReply::Created { epoch, result })
            }
            DeviceRequest::Check { epoch } => {
                let present = self.device.as_mut().is_some_and(VirtualDevice::is_present);
                Some(DeviceReply::Checked { epoch, present })
            }
            DeviceRequest::Release => {
                self.device = None;
                Some(DeviceReply::Released)
            }
            DeviceRequest::Forget => {
                if let Some(device) = self.device.take() {
                    std::mem::forget(device);
                }
                None
            }
        }
    }
}

/// A thread that owns the virtual device and runs its `pactl` work off the
/// event loop. Requests run in the order they were sent; every reply goes to
/// the callback given at spawn.
pub struct DeviceWorker {
    requests: mpsc::Sender<DeviceRequest>,
    /// Disconnects when the thread ends.
    done: mpsc::Receiver<()>,
    thread: JoinHandle<()>,
}

impl DeviceWorker {
    /// Starts the worker. `create` makes the device (`Ok(None)` where the
    /// user installs it); `reply` receives every answer, on the worker
    /// thread.
    pub fn spawn<G, F, R>(create: F, reply: R) -> Result<DeviceWorker>
    where
        G: VirtualDevice + Send + 'static,
        F: FnMut() -> Result<Option<G>> + Send + 'static,
        R: Fn(DeviceReply) + Send + 'static,
    {
        let (requests, receiver) = mpsc::channel::<DeviceRequest>();
        let (done_sender, done) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("virtual-mic".into())
            .spawn(move || {
                let _done = done_sender;
                let mut host = DeviceHost::new(create);
                for request in receiver {
                    if let Some(answer) = host.handle(request) {
                        reply(answer);
                    }
                }
                // A device still held here is dropped with the host.
            })
            .context("failed to start the virtual mic worker thread")?;
        Ok(DeviceWorker {
            requests,
            done,
            thread,
        })
    }

    /// Closes the request channel and waits at most `timeout` for the worker
    /// to finish the requests already sent. Returns whether it ended; one
    /// still blocked in `pactl` is left to finish on its own.
    pub fn finish_within(self, timeout: Duration) -> bool {
        let DeviceWorker {
            requests,
            done,
            thread,
        } = self;
        drop(requests);
        match done.recv_timeout(timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(
                    "the virtual mic worker did not finish in time; leaving it running, and the virtual mic may stay loaded until the next start unloads it as stale"
                );
                false
            }
            _ => {
                if thread.join().is_err() {
                    tracing::error!("the virtual mic worker panicked");
                }
                true
            }
        }
    }
}

impl DeviceRequests for DeviceWorker {
    fn send(&mut self, request: DeviceRequest) {
        if self.requests.send(request).is_err() {
            tracing::error!(?request, "the virtual mic worker is gone; request dropped");
        }
    }

    fn finish(self) {
        self.finish_within(WORKER_FINISH_TIMEOUT);
    }
}

/// Where the virtual device stands, as the event loop knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceState {
    /// The worker holds no device, or a queued `Release` drops it.
    Absent,
    /// A `Create` with this epoch is on its way.
    Creating { epoch: u64 },
    /// The device exists; `owned` is false where the user installs it (the
    /// worker made none).
    Present { owned: bool },
    /// The mic path stopped while a mic stream may still run, so the worker
    /// keeps the device (see [`WithVirtualMic::release_retired`]). `vanished`
    /// is true when the device's modules are already gone (a sound server
    /// restart): it is never reused, and is released and created again once
    /// the stream ended.
    Retired { vanished: bool },
}

/// Why the mic path waits while the mic is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceWait<'a> {
    /// The device is being created, or a stopped mic stream has yet to end.
    Pending,
    /// Creating the device failed (the pattern is carried); it is retried.
    Unavailable(&'a str),
}

/// An [`AudioControl`] that keeps the virtual device the app creates itself
/// (on Linux) in step with the mic path.
///
/// Turning the mic path on asks the worker to create the device first, if it
/// does not exist yet, and starts the path once a current
/// [`DeviceReply::Created`] arrives; turning it off (or a mic path that
/// fails to start) asks the worker to drop the device after the engine has
/// stopped using it. If the engine's stop timed out (a mic supervisor is
/// still blocked in a device query and may still hold its stream), the
/// device and its lock are kept as retired until
/// [`AudioControl::mic_stopped`] says the stream has ended
/// ([`WithVirtualMic::release_retired`]); unloading it under a live stream
/// would let the server move the stream, the live microphone included, to
/// the speakers. A mic turned on again meanwhile reuses it, unless the
/// device vanished (a [`DeviceReply::Checked`] found it gone while the stream
/// still ran): that one is released and created again with a new epoch once
/// the stream has ended.
///
/// The mic path runs only while this process owns the device: a device that
/// cannot be created is logged and the mic path stays stopped, waiting for
/// it ([`WithVirtualMic::waiting_for_device`], shown as not found), and
/// [`WithVirtualMic::restore_device`] tries again; the path starts once the
/// device exists. The device's names are fixed, so a mic path started
/// without it could find another instance's device (creating it fails on
/// that instance's lock) and play the live microphone into it, or, once that
/// instance unloads it, onto the speakers the server moves the stream to.
/// The worker's `create` returns `Ok(None)` on the OSes where the user
/// installs the device; the mic path then starts after one round trip.
///
/// Every mic-on from no device starts a new epoch; a reply from an older
/// one is ignored. A mic turned off while the device is created sends
/// `Release` at once, which the worker runs after the pending create.
pub struct WithVirtualMic<C, R> {
    control: C,
    requests: R,
    state: DeviceState,
    /// The epoch of the latest `Create`.
    epoch: u64,
    /// The epoch of a `Check` still waiting for its reply.
    checking: Option<u64>,
    /// The settings of the enabled mic; `None` while the mic is off.
    settings: Option<MicSettings>,
    /// Whether the engine's mic path runs. False while `settings` is set
    /// means the mic path waits for the device.
    running: bool,
    /// The last attempt to create the device failed (so repeats log at
    /// debug, and the wait shows as not found).
    create_failed: bool,
    /// When [`WithVirtualMic::restore_device`] may next check the device.
    next_check: Option<Instant>,
}

impl<C, R: DeviceRequests> WithVirtualMic<C, R> {
    pub fn new(control: C, requests: R) -> Self {
        WithVirtualMic {
            control,
            requests,
            state: DeviceState::Absent,
            epoch: 0,
            checking: None,
            settings: None,
            running: false,
            create_failed: false,
            next_check: None,
        }
    }

    /// The wrapped control.
    pub fn control(&self) -> &C {
        &self.control
    }

    /// Why the mic path waits while the mic is on; `None` while it runs or
    /// the mic is off.
    pub fn waiting_for_device(&self) -> Option<DeviceWait<'_>> {
        let settings = self.settings.as_ref()?;
        if self.running {
            None
        } else if self.create_failed {
            Some(DeviceWait::Unavailable(&settings.device_match))
        } else {
            Some(DeviceWait::Pending)
        }
    }

    /// Whether the virtual device currently exists for the mic path.
    #[cfg(test)]
    fn has_device(&self) -> bool {
        self.state == DeviceState::Present { owned: true }
    }

    /// Whether a retired device waits for a mic stream to end.
    pub fn has_retired_device(&self) -> bool {
        matches!(self.state, DeviceState::Retired { .. })
    }

    /// Hands the control to `stop` (the engine shuts down, returning whether
    /// every stream ended), then asks the worker to drop the virtual device,
    /// so no stream is left writing to it, and waits for the worker. If a
    /// stream may still run, the device is forgotten instead and stays
    /// loaded (the next start unloads it as stale): the process ends right
    /// after, and that ends the stream without moving it.
    pub fn shutdown(self, stop: impl FnOnce(C) -> bool) {
        let WithVirtualMic {
            control,
            mut requests,
            state,
            ..
        } = self;
        let holds_device = matches!(
            state,
            DeviceState::Present { owned: true }
                | DeviceState::Retired { .. }
                | DeviceState::Creating { .. }
        );
        if stop(control) || !holds_device {
            requests.send(DeviceRequest::Release);
        } else {
            tracing::warn!(
                "a mic stream did not stop in time; the virtual mic stays loaded until the next start"
            );
            requests.send(DeviceRequest::Forget);
        }
        requests.finish();
    }

    /// Starts a new epoch and asks the worker for the device.
    fn begin_create(&mut self) {
        self.epoch += 1;
        self.state = DeviceState::Creating { epoch: self.epoch };
        self.requests
            .send(DeviceRequest::Create { epoch: self.epoch });
    }
}

impl<C, R> WithVirtualMic<C, R>
where
    C: AudioControl,
    R: DeviceRequests,
{
    /// Handles a reply from the device worker; one from an older epoch, or
    /// for a mic turned off meanwhile, is ignored.
    pub fn on_reply(&mut self, reply: DeviceReply) {
        match reply {
            DeviceReply::Created { epoch, result } => self.created(epoch, result),
            DeviceReply::Checked { epoch, present } => self.checked(epoch, present),
            DeviceReply::Released => tracing::debug!("the virtual mic is released"),
        }
    }

    /// Starts the mic path once the current `Create` made the device; a
    /// failure is logged (at error for the first of a streak, at debug for
    /// repeats) and the path keeps waiting.
    fn created(&mut self, epoch: u64, result: Result<bool>) {
        if self.state != (DeviceState::Creating { epoch }) {
            tracing::debug!(epoch, "ignoring a stale virtual mic create reply");
            return;
        }
        match result {
            Ok(owned) => {
                if self.create_failed {
                    tracing::info!("the virtual mic is created");
                }
                self.create_failed = false;
                self.state = DeviceState::Present { owned };
                if let Err(err) = self.start_path() {
                    tracing::error!("{err:#}; the mic path did not start");
                }
            }
            Err(err) => {
                if self.create_failed {
                    tracing::debug!("{err:#}; the virtual mic is still unavailable");
                } else {
                    tracing::error!(
                        "{err:#}; the virtual mic is unavailable, the mic path waits for it"
                    );
                }
                self.create_failed = true;
                self.state = DeviceState::Absent;
            }
        }
    }

    /// A vanished device stops the mic path before it is released, so the
    /// path never runs without it, and is created again. If the stop left a
    /// mic stream running, the device goes through the retire decision like
    /// any other: it is kept as retired (vanished), nothing is sent, and
    /// [`WithVirtualMic::release_retired`] releases it and creates it again
    /// once the stream ended.
    fn checked(&mut self, epoch: u64, present: bool) {
        if self.checking == Some(epoch) {
            self.checking = None;
        }
        let current = self.settings.is_some()
            && epoch == self.epoch
            && self.state == DeviceState::Present { owned: true };
        if !current || present {
            return;
        }
        tracing::warn!(
            "the virtual mic is gone (did the sound server restart?); creating it again"
        );
        if let Err(err) = self.stop_path() {
            tracing::error!("{err:#}; could not stop the mic path");
        }
        if self.control.mic_stopped() {
            // Its modules are gone already, so releasing it unloads nothing.
            self.requests.send(DeviceRequest::Release);
            self.begin_create();
        } else {
            tracing::warn!(
                "a mic stream did not stop in time; the virtual mic is created again once it does"
            );
            self.state = DeviceState::Retired { vanished: true };
        }
    }

    /// Starts (or updates) the mic path with the stored settings. If that
    /// fails, the mic is treated as off and the device is dropped (or
    /// retired).
    fn start_path(&mut self) -> Result<()> {
        let result = self.control.set_mic(self.settings.clone());
        self.running = result.is_ok();
        if result.is_err() {
            self.settings = None;
            self.drop_device();
            self.create_failed = false;
        }
        result
    }

    /// Asks the worker to drop the device once the mic path is stopped, or
    /// retires it if a mic stream may still run (see
    /// [`WithVirtualMic::release_retired`]). A device still being created is
    /// released after the create.
    fn drop_device(&mut self) {
        match self.state {
            DeviceState::Present { owned: true } => {
                if self.control.mic_stopped() {
                    self.requests.send(DeviceRequest::Release);
                    self.state = DeviceState::Absent;
                } else {
                    tracing::warn!(
                        "a mic stream did not stop in time; keeping the virtual mic until it does"
                    );
                    self.state = DeviceState::Retired { vanished: false };
                }
            }
            DeviceState::Present { owned: false } => self.state = DeviceState::Absent,
            DeviceState::Creating { .. } => {
                self.requests.send(DeviceRequest::Release);
                self.state = DeviceState::Absent;
            }
            DeviceState::Absent | DeviceState::Retired { .. } => {}
        }
    }

    /// Once every mic stream has ended, starts the mic path on the retired
    /// device if the mic path waits for it (the mic was turned on again
    /// meanwhile), and otherwise releases it. A vanished device is never
    /// taken back: it is released, and created again (a new epoch) if the mic
    /// is on. The tray calls it every mic poll while there is one.
    pub fn release_retired(&mut self) {
        let DeviceState::Retired { vanished } = self.state else {
            return;
        };
        if !self.control.mic_stopped() {
            return;
        }
        if vanished {
            tracing::info!("the stopped mic stream has ended; replacing the vanished virtual mic");
            self.requests.send(DeviceRequest::Release);
            if self.settings.is_some() {
                self.begin_create();
            } else {
                self.state = DeviceState::Absent;
            }
        } else if self.settings.is_some() && !self.running {
            tracing::info!("the stopped mic stream has ended; starting the mic path");
            self.state = DeviceState::Present { owned: true };
            if let Err(err) = self.start_path() {
                tracing::error!("{err:#}; the mic path did not start");
            }
        } else {
            tracing::info!("the stopped mic stream has ended; dropping the virtual mic");
            self.requests.send(DeviceRequest::Release);
            self.state = DeviceState::Absent;
        }
    }

    /// Stops the mic path if it runs; the settings stay, so it waits for the
    /// device.
    fn stop_path(&mut self) -> Result<()> {
        if !self.running {
            return Ok(());
        }
        self.running = false;
        self.control.set_mic(None)
    }

    /// Asks the worker whether the virtual device still exists (for example,
    /// a sound server restart drops its modules) and, through
    /// [`WithVirtualMic::on_reply`], creates it again if it is gone; or asks
    /// for the device again if the mic path waits for it because creating it
    /// failed (for example, the sound server was not up yet). Does nothing
    /// with the mic off, no device to look after (the user installs it), a
    /// create or check already on its way, or a retired device, and acts at
    /// most every [`DEVICE_CHECK_INTERVAL`]. The tray calls it while the mic
    /// status is not found.
    pub fn restore_device(&mut self, now: Instant) {
        if self.settings.is_none() {
            return;
        }
        let check = match self.state {
            DeviceState::Present { owned: true } if self.checking != Some(self.epoch) => true,
            DeviceState::Absent => false,
            _ => return,
        };
        if self.next_check.is_some_and(|next| now < next) {
            return;
        }
        self.next_check = Some(now + DEVICE_CHECK_INTERVAL);
        if check {
            self.checking = Some(self.epoch);
            self.requests
                .send(DeviceRequest::Check { epoch: self.epoch });
        } else {
            self.begin_create();
        }
    }
}

impl<C, R> AudioControl for WithVirtualMic<C, R>
where
    C: AudioControl,
    R: DeviceRequests,
{
    fn play(&self, sound: &Sound, gain: f32) -> bool {
        self.control.play(sound, gain)
    }

    fn set_master_gain(&self, gain: f32) -> bool {
        self.control.set_master_gain(gain)
    }

    fn mic_stopped(&mut self) -> bool {
        self.control.mic_stopped()
    }

    /// Like a failed start, a failed deferred start turns the mic off and
    /// drops (or retires) the device.
    fn resume_mic(&mut self) -> Result<()> {
        let result = self.control.resume_mic();
        if result.is_err() {
            self.running = false;
            self.settings = None;
            self.drop_device();
            self.create_failed = false;
        }
        result
    }

    /// Turning the mic on asks for the device and returns at once; the path
    /// starts when the device exists (see [`WithVirtualMic::on_reply`]).
    /// With the device there, the path starts (or changes) right away.
    fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
        let Some(settings) = settings else {
            let result = self.control.set_mic(None);
            self.running = false;
            self.settings = None;
            self.drop_device();
            self.create_failed = false;
            return result;
        };
        self.settings = Some(settings);
        match self.state {
            DeviceState::Present { .. } => self.start_path(),
            // Starts with the latest settings once the device exists.
            DeviceState::Creating { .. } => Ok(()),
            DeviceState::Absent => {
                self.begin_create();
                Ok(())
            }
            DeviceState::Retired { vanished } => {
                if self.control.mic_stopped() {
                    if vanished {
                        self.requests.send(DeviceRequest::Release);
                        self.begin_create();
                        Ok(())
                    } else {
                        self.state = DeviceState::Present { owned: true };
                        self.start_path()
                    }
                } else {
                    // A new mic path never runs beside the old one;
                    // `release_retired` starts it once the old stream ended.
                    tracing::debug!(
                        "the stopped mic stream has not ended; the mic path waits for it"
                    );
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::actions::{ActionMap, apply_config, load_board, mic_settings, toggle_mic};
    use crate::config::{MicConfig, ResolvedConfig};
    use crate::test_support::TempDir;

    fn sample_config() -> ResolvedConfig {
        ResolvedConfig {
            master_gain: 0.75,
            stop_all_hotkey: None,
            sounds: Vec::new(),
            mic: MicConfig::default(),
        }
    }

    fn mic_on(device: &str) -> MicConfig {
        MicConfig {
            enabled: true,
            virtual_device: device.into(),
            device_prefer: None,
            sound_gain: 0.5,
            mic_gain: 2.0,
        }
    }

    fn write_config(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    const MIC_CONFIG: &str = "# my comment\nmaster_gain = 0.5\n\n\
                              [mic]\nenabled = false # keep me\nvirtual_device = \"Cable\"\n";

    type Log = Rc<RefCell<Vec<&'static str>>>;

    /// Records the requests [`WithVirtualMic`] sends; they wait in `queue`
    /// until the test runs them (see [`run`]) or answers them by hand.
    #[derive(Clone, Default)]
    struct FakeRequests {
        queue: Rc<RefCell<VecDeque<DeviceRequest>>>,
        sent: Rc<RefCell<Vec<DeviceRequest>>>,
        finished: Rc<Cell<bool>>,
    }

    impl FakeRequests {
        fn pop(&self) -> Option<DeviceRequest> {
            self.queue.borrow_mut().pop_front()
        }

        fn sent(&self) -> Vec<DeviceRequest> {
            self.sent.borrow().clone()
        }

        fn queued(&self) -> Vec<DeviceRequest> {
            self.queue.borrow().iter().copied().collect()
        }
    }

    impl DeviceRequests for FakeRequests {
        fn send(&mut self, request: DeviceRequest) {
            self.queue.borrow_mut().push_back(request);
            self.sent.borrow_mut().push(request);
        }

        fn finish(self) {
            self.finished.set(true);
        }
    }

    /// Runs the queued requests, in order, on `host` (the worker, inline) and
    /// hands every reply back, until nothing is queued.
    fn run<C, G, F>(audio: &mut WithVirtualMic<C, FakeRequests>, host: &mut DeviceHost<G, F>)
    where
        C: AudioControl,
        G: VirtualDevice,
        F: FnMut() -> Result<Option<G>>,
    {
        while let Some(request) = audio.requests.pop() {
            if let Some(reply) = host.handle(request) {
                audio.on_reply(reply);
            }
        }
    }

    /// Runs what a shutdown left queued; the replies go nowhere, as with
    /// the real worker after the event loop has ended.
    fn drain<G, F>(requests: &FakeRequests, host: &mut DeviceHost<G, F>)
    where
        G: VirtualDevice,
        F: FnMut() -> Result<Option<G>>,
    {
        while let Some(request) = requests.pop() {
            host.handle(request);
        }
    }

    /// Logs `set_mic` calls into a shared log, so their order against the
    /// virtual device's creation and drop can be checked; keeps the last
    /// settings.
    struct LoggingAudio {
        log: Log,
        fail_mic: bool,
        last: Option<MicSettings>,
    }

    impl LoggingAudio {
        fn new(log: &Log, fail_mic: bool) -> LoggingAudio {
            LoggingAudio {
                log: log.clone(),
                fail_mic,
                last: None,
            }
        }
    }

    impl AudioControl for LoggingAudio {
        fn play(&self, _sound: &Sound, _gain: f32) -> bool {
            self.log.borrow_mut().push("play");
            true
        }

        fn set_master_gain(&self, _gain: f32) -> bool {
            self.log.borrow_mut().push("gain");
            true
        }

        fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
            self.log.borrow_mut().push(if settings.is_some() {
                "mic on"
            } else {
                "mic off"
            });
            self.last = settings;
            if self.fail_mic {
                anyhow::bail!("no thread for you");
            }
            Ok(())
        }
    }

    /// Logs its drop.
    struct FakeDevice(Log);

    /// The wrapper under test and the worker's device side, run inline.
    type Rig<C, F> = (WithVirtualMic<C, FakeRequests>, DeviceHost<FakeDevice, F>);

    impl VirtualDevice for FakeDevice {
        fn is_present(&mut self) -> bool {
            true
        }
    }

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            self.0.borrow_mut().push("device dropped");
        }
    }

    fn fake_create(log: &Log) -> impl FnMut() -> Result<Option<FakeDevice>> + use<> {
        let log = log.clone();
        move || {
            log.borrow_mut().push("device created");
            Ok(Some(FakeDevice(log.clone())))
        }
    }

    fn with_device(
        log: &Log,
        fail_mic: bool,
    ) -> Rig<LoggingAudio, impl FnMut() -> Result<Option<FakeDevice>> + use<>> {
        (
            WithVirtualMic::new(LoggingAudio::new(log, fail_mic), FakeRequests::default()),
            DeviceHost::new(fake_create(log)),
        )
    }

    fn on() -> Option<MicSettings> {
        mic_settings(&mic_on("Cable"), true)
    }

    fn on_with_gain(mic_gain: f32) -> Option<MicSettings> {
        mic_settings(
            &MicConfig {
                mic_gain,
                ..mic_on("Cable")
            },
            true,
        )
    }

    /// A control whose deferred mic start fails.
    struct FailingResumeAudio(Log);

    impl AudioControl for FailingResumeAudio {
        fn play(&self, _sound: &Sound, _gain: f32) -> bool {
            true
        }

        fn set_master_gain(&self, _gain: f32) -> bool {
            true
        }

        fn set_mic(&mut self, _settings: Option<MicSettings>) -> Result<()> {
            self.0.borrow_mut().push("mic deferred");
            Ok(())
        }

        fn resume_mic(&mut self) -> Result<()> {
            self.0.borrow_mut().push("mic resumed");
            anyhow::bail!("no thread for you");
        }
    }

    #[test]
    fn a_failed_deferred_start_turns_the_mic_off_and_drops_the_device() {
        let log = Log::default();
        let mut audio =
            WithVirtualMic::new(FailingResumeAudio(log.clone()), FakeRequests::default());
        let mut host = DeviceHost::new(fake_create(&log));
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(audio.resume_mic().is_err());
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic deferred",
                "mic resumed",
                "device dropped"
            ]
        );
    }

    #[test]
    fn the_virtual_device_is_created_before_the_mic_starts_and_dropped_after_it_stops() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        // A gains-only change keeps the existing device.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        // Off again: nothing to create or drop.
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic on",
                "mic on",
                "mic off",
                "device dropped",
                "mic off"
            ]
        );
        assert_eq!(
            audio.requests.sent(),
            [DeviceRequest::Create { epoch: 1 }, DeviceRequest::Release]
        );
    }

    #[test]
    fn the_path_never_starts_before_the_device_is_created() {
        let log = Log::default();
        let (mut audio, _host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        // Asking for the device returns at once; nothing runs yet.
        assert!(log.borrow().is_empty());
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Create { epoch: 1 }]
        );
        // A check answer or a release is not a create answer.
        audio.on_reply(DeviceReply::Checked {
            epoch: 1,
            present: true,
        });
        audio.on_reply(DeviceReply::Released);
        assert!(log.borrow().is_empty());
        // A failed create keeps it stopped and shows as not found.
        audio.on_reply(DeviceReply::Created {
            epoch: 1,
            result: Err(anyhow::anyhow!("pactl did not finish within 3s")),
        });
        assert!(log.borrow().is_empty());
        assert_eq!(
            audio.waiting_for_device(),
            Some(DeviceWait::Unavailable("Cable"))
        );
        audio.restore_device(Instant::now());
        assert_eq!(
            audio.requests.sent().last(),
            Some(&DeviceRequest::Create { epoch: 2 })
        );
        // Retrying still shows as not found, so the status does not flicker.
        assert_eq!(
            audio.waiting_for_device(),
            Some(DeviceWait::Unavailable("Cable"))
        );
        // Only the current create starts the path.
        audio.on_reply(DeviceReply::Created {
            epoch: 1,
            result: Ok(true),
        });
        assert!(log.borrow().is_empty());
        audio.on_reply(DeviceReply::Created {
            epoch: 2,
            result: Ok(true),
        });
        assert_eq!(*log.borrow(), ["mic on"]);
        assert_eq!(audio.waiting_for_device(), None);
    }

    #[test]
    fn a_create_answered_after_mic_off_starts_nothing_and_is_released() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        audio.set_mic(None).unwrap();
        // The release is queued right away, behind the create.
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Create { epoch: 1 }, DeviceRequest::Release]
        );
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(
            *log.borrow(),
            ["mic off", "device created", "device dropped"]
        );
    }

    #[test]
    fn a_failed_create_answered_after_mic_off_leaves_no_failure() {
        let log = Log::default();
        let (mut audio, _host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        audio.set_mic(None).unwrap();
        audio.on_reply(DeviceReply::Created {
            epoch: 1,
            result: Err(anyhow::anyhow!("pactl did not finish within 3s")),
        });
        assert!(!audio.create_failed);
        assert_eq!(audio.waiting_for_device(), None);
        // The next mic-on shows as starting, not as not found.
        audio.set_mic(on()).unwrap();
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));
    }

    #[test]
    fn a_vanished_device_answered_after_mic_off_is_ignored() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.restore_device(Instant::now());
        audio.set_mic(None).unwrap();
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Check { epoch: 1 }, DeviceRequest::Release]
        );
        // The check is answered as gone, after the mic went off.
        assert_eq!(
            audio.requests.pop(),
            Some(DeviceRequest::Check { epoch: 1 })
        );
        audio.on_reply(DeviceReply::Checked {
            epoch: 1,
            present: false,
        });
        run(&mut audio, &mut host);
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "mic off", "device dropped"]
        );
        assert!(
            !audio
                .requests
                .sent()
                .contains(&DeviceRequest::Create { epoch: 2 })
        );

        // A check from an older epoch is ignored as well.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.on_reply(DeviceReply::Checked {
            epoch: 1,
            present: false,
        });
        assert!(audio.has_device());
        assert_eq!(log.borrow().last(), Some(&"mic on"));
    }

    #[test]
    fn on_off_on_starts_the_path_once_with_the_latest_settings() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on_with_gain(1.0)).unwrap();
        audio.set_mic(None).unwrap();
        audio.set_mic(on_with_gain(1.5)).unwrap();
        // A change while the device is created needs no second create.
        audio.set_mic(on_with_gain(0.5)).unwrap();
        assert_eq!(
            audio.requests.queued(),
            [
                DeviceRequest::Create { epoch: 1 },
                DeviceRequest::Release,
                DeviceRequest::Create { epoch: 2 }
            ]
        );
        run(&mut audio, &mut host);
        assert_eq!(
            *log.borrow(),
            [
                "mic off",
                "device created",
                "device dropped",
                "device created",
                "mic on"
            ]
        );
        assert_eq!(audio.control.last, on_with_gain(0.5));
        assert!(audio.has_device());
    }

    #[test]
    fn a_mic_path_that_fails_to_start_drops_the_device() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, true);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "device dropped"]
        );
    }

    #[test]
    fn a_device_that_cannot_be_created_keeps_the_mic_path_stopped() {
        let log = Log::default();
        let attempts = Rc::new(RefCell::new(0));
        let counter = attempts.clone();
        let mut audio =
            WithVirtualMic::new(LoggingAudio::new(&log, false), FakeRequests::default());
        let mut host = DeviceHost::new(move || -> Result<Option<FakeDevice>> {
            *counter.borrow_mut() += 1;
            anyhow::bail!("another Sound-Babound instance owns the virtual mic")
        });
        // Another instance's device may exist under the same name: the mic
        // path must not start and find it.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(
            audio.waiting_for_device(),
            Some(DeviceWait::Unavailable("Cable"))
        );
        assert_eq!(*attempts.borrow(), 2);
        assert!(log.borrow().is_empty());
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(*log.borrow(), ["mic off"]);
    }

    /// A device whose presence the test controls; logs its checks and drop.
    struct VanishingDevice {
        log: Log,
        present: Rc<Cell<bool>>,
    }

    impl VirtualDevice for VanishingDevice {
        fn is_present(&mut self) -> bool {
            self.log.borrow_mut().push("device checked");
            self.present.get()
        }
    }

    impl Drop for VanishingDevice {
        fn drop(&mut self) {
            self.log.borrow_mut().push("device dropped");
        }
    }

    #[test]
    fn a_vanished_device_is_created_again_at_most_every_check_interval() {
        let log = Log::default();
        let present = Rc::new(Cell::new(true));
        let (device_log, device_present) = (log.clone(), present.clone());
        let mut audio =
            WithVirtualMic::new(LoggingAudio::new(&log, false), FakeRequests::default());
        let mut host = DeviceHost::new(move || -> Result<Option<VanishingDevice>> {
            device_log.borrow_mut().push("device created");
            device_present.set(true);
            Ok(Some(VanishingDevice {
                log: device_log.clone(),
                present: device_present.clone(),
            }))
        });
        let start = Instant::now();
        // With the mic off there is nothing to check.
        audio.restore_device(start);
        assert!(audio.requests.sent().is_empty());
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.restore_device(start);
        run(&mut audio, &mut host);
        present.set(false);
        // Too early for another check.
        audio.restore_device(start + DEVICE_CHECK_INTERVAL / 2);
        audio.restore_device(start + DEVICE_CHECK_INTERVAL);
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        // The mic path stops before the vanished device is dropped and
        // starts again once the new one exists.
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic on",
                "device checked",
                "device checked",
                "mic off",
                "device dropped",
                "device created",
                "mic on"
            ]
        );
    }

    #[test]
    fn a_check_waits_for_the_answer_to_the_last_one() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        let start = Instant::now();
        audio.restore_device(start);
        // The worker is busy: no second check piles up behind the first.
        audio.restore_device(start + DEVICE_CHECK_INTERVAL * 2);
        assert_eq!(audio.requests.queued(), [DeviceRequest::Check { epoch: 1 }]);
        run(&mut audio, &mut host);
        audio.restore_device(start + DEVICE_CHECK_INTERVAL * 3);
        assert_eq!(audio.requests.queued(), [DeviceRequest::Check { epoch: 1 }]);
    }

    /// A create function for [`VanishingDevice`] whose calls fail where
    /// `fails` says so (by call index); logs every attempt.
    fn flaky_create(
        log: &Log,
        present: &Rc<Cell<bool>>,
        fails: &'static [usize],
    ) -> impl FnMut() -> Result<Option<VanishingDevice>> + use<> {
        let (log, present) = (log.clone(), present.clone());
        let mut calls = 0;
        move || {
            let call = calls;
            calls += 1;
            if fails.contains(&call) {
                log.borrow_mut().push("create failed");
                anyhow::bail!("pactl did not finish within 3s");
            }
            log.borrow_mut().push("device created");
            present.set(true);
            Ok(Some(VanishingDevice {
                log: log.clone(),
                present: present.clone(),
            }))
        }
    }

    #[test]
    fn a_device_that_cannot_be_created_again_is_retried_at_the_next_check() {
        let log = Log::default();
        let present = Rc::new(Cell::new(true));
        let mut audio =
            WithVirtualMic::new(LoggingAudio::new(&log, false), FakeRequests::default());
        let mut host = DeviceHost::new(flaky_create(&log, &present, &[1]));
        let start = Instant::now();
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        present.set(false);
        audio.restore_device(start);
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(
            audio.waiting_for_device(),
            Some(DeviceWait::Unavailable("Cable"))
        );
        // Too early for another attempt.
        audio.restore_device(start + DEVICE_CHECK_INTERVAL / 2);
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        audio.restore_device(start + DEVICE_CHECK_INTERVAL);
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic on",
                "device checked",
                "mic off",
                "device dropped",
                "create failed",
                "device created",
                "mic on"
            ]
        );
    }

    #[test]
    fn a_device_that_failed_to_create_at_mic_on_is_retried_until_the_mic_goes_off() {
        let log = Log::default();
        let present = Rc::new(Cell::new(true));
        let mut audio =
            WithVirtualMic::new(LoggingAudio::new(&log, false), FakeRequests::default());
        let mut host = DeviceHost::new(flaky_create(&log, &present, &[0, 1, 3]));
        let start = Instant::now();
        // The sound server is not up yet: the mic path waits for the device.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(
            audio.waiting_for_device(),
            Some(DeviceWait::Unavailable("Cable"))
        );
        audio.restore_device(start);
        run(&mut audio, &mut host);
        audio.restore_device(start + DEVICE_CHECK_INTERVAL / 2);
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        audio.restore_device(start + DEVICE_CHECK_INTERVAL);
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        // A failure while the mic is on is forgotten once it goes off.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        audio.restore_device(start + DEVICE_CHECK_INTERVAL * 3);
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert!(!audio.create_failed);
        assert_eq!(
            *log.borrow(),
            [
                "create failed",
                "create failed",
                "device created",
                "mic on",
                "mic off",
                "device dropped",
                "create failed",
                "mic off"
            ]
        );
    }

    #[test]
    fn no_device_is_made_where_the_user_installs_it() {
        let log = Log::default();
        let mut audio =
            WithVirtualMic::new(LoggingAudio::new(&log, false), FakeRequests::default());
        let mut host = DeviceHost::new(|| -> Result<Option<FakeDevice>> { Ok(None) });
        audio.set_mic(on()).unwrap();
        // One round trip, then the path starts.
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        // There is nothing to check.
        audio.restore_device(Instant::now());
        audio.set_mic(None).unwrap();
        assert_eq!(*log.borrow(), ["mic on", "mic off"]);
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);
    }

    #[test]
    fn startup_with_the_mic_enabled_creates_the_device_before_applying() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        let config = ResolvedConfig {
            mic: mic_on("Cable"),
            ..sample_config()
        };
        apply_config(&mut audio, &ActionMap::new(&config), true);
        run(&mut audio, &mut host);
        assert_eq!(*log.borrow(), ["gain", "device created", "mic on"]);
    }

    #[test]
    fn toggling_through_the_wrapper_creates_and_drops_the_device() {
        let dir = TempDir::new("virtual-mic-toggle-device");
        let path = write_config(&dir, MIC_CONFIG);
        let mut board = load_board(&path).unwrap();
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        toggle_mic(&mut audio, &mut board, Some(&path), |_| true);
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        toggle_mic(&mut audio, &mut board, Some(&path), |_| true);
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "mic off", "device dropped"]
        );
    }

    #[test]
    fn shutdown_stops_the_engine_before_dropping_the_device() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        let requests = audio.requests.clone();
        let stop_log = log.clone();
        audio.shutdown(move |control| {
            stop_log.borrow_mut().push("engine shut down");
            drop(control);
            true
        });
        assert_eq!(requests.queued(), [DeviceRequest::Release]);
        assert!(requests.finished.get());
        drain(&requests, &mut host);
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic on",
                "engine shut down",
                "device dropped"
            ]
        );
    }

    #[test]
    fn shutdown_while_the_device_is_created_releases_it() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        let requests = audio.requests.clone();
        audio.shutdown(|_control| true);
        drain(&requests, &mut host);
        assert_eq!(*log.borrow(), ["device created", "device dropped"]);
    }

    #[test]
    fn shutdown_while_the_device_is_created_forgets_it_when_a_stream_is_left_running() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        let requests = audio.requests.clone();
        audio.shutdown(|_control| false);
        assert_eq!(
            requests.queued(),
            [DeviceRequest::Create { epoch: 1 }, DeviceRequest::Forget]
        );
        drain(&requests, &mut host);
        assert_eq!(*log.borrow(), ["device created"]);
    }

    /// A control whose mic stop can leave a stream running, as the engine's
    /// does when a supervisor is blocked in a device query.
    struct SlowStopAudio {
        inner: LoggingAudio,
        stopped: Rc<Cell<bool>>,
    }

    impl AudioControl for SlowStopAudio {
        fn play(&self, sound: &Sound, gain: f32) -> bool {
            self.inner.play(sound, gain)
        }

        fn set_master_gain(&self, gain: f32) -> bool {
            self.inner.set_master_gain(gain)
        }

        fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
            self.inner.set_mic(settings)
        }

        fn mic_stopped(&mut self) -> bool {
            self.stopped.get()
        }
    }

    fn with_slow_stop(
        log: &Log,
        stopped: &Rc<Cell<bool>>,
    ) -> Rig<SlowStopAudio, impl FnMut() -> Result<Option<FakeDevice>> + use<>> {
        (
            WithVirtualMic::new(
                SlowStopAudio {
                    inner: LoggingAudio::new(log, false),
                    stopped: stopped.clone(),
                },
                FakeRequests::default(),
            ),
            DeviceHost::new(fake_create(log)),
        )
    }

    #[test]
    fn a_timed_out_mic_stop_keeps_the_device_until_the_stream_ends() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        stopped.set(false);
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        assert!(!audio.has_device());
        assert!(audio.has_retired_device());
        assert_eq!(*log.borrow(), ["device created", "mic on", "mic off"]);

        // Still running: kept, and nothing is asked of the worker.
        audio.release_retired();
        run(&mut audio, &mut host);
        assert!(audio.has_retired_device());
        assert_eq!(log.borrow().len(), 3);
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);

        stopped.set(true);
        audio.release_retired();
        assert!(!audio.has_retired_device());
        assert_eq!(audio.requests.queued(), [DeviceRequest::Release]);
        run(&mut audio, &mut host);
        assert_eq!(log.borrow().last(), Some(&"device dropped"));
    }

    #[test]
    fn a_mic_turned_on_again_waits_for_the_old_stream_then_reuses_the_retired_device() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(false));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        assert!(audio.has_retired_device());

        // The old stream still runs: no second mic path beside it.
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));
        assert!(audio.has_retired_device());
        assert!(!audio.has_device());
        // Waiting for the old stream is not a failure to create.
        audio.restore_device(Instant::now());
        audio.release_retired();
        run(&mut audio, &mut host);
        assert!(audio.waiting_for_device().is_some());
        assert_eq!(*log.borrow(), ["device created", "mic on", "mic off"]);

        // It ended: the mic path starts on the same device.
        stopped.set(true);
        audio.release_retired();
        run(&mut audio, &mut host);
        assert!(audio.waiting_for_device().is_none());
        assert!(audio.has_device());
        assert!(!audio.has_retired_device());
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "mic off", "mic on"]
        );
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);
    }

    #[test]
    fn a_mic_turned_on_after_the_old_stream_ended_reuses_the_retired_device_at_once() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(false));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        stopped.set(true);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        assert!(!audio.has_retired_device());
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "mic off", "mic on"]
        );
    }

    #[test]
    fn a_failed_start_after_a_timed_out_stop_retires_the_device() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        // A restart whose old path did not stop in time, then failed.
        stopped.set(false);
        audio.control.inner.fail_mic = true;
        let mut changed = on();
        if let Some(settings) = &mut changed {
            settings.capture = false;
        }
        assert!(audio.set_mic(changed).is_err());
        run(&mut audio, &mut host);
        assert!(audio.has_retired_device());
        assert!(!log.borrow().contains(&"device dropped"));
    }

    /// A mic on a device that is created (epoch 1) and a stop that may not
    /// finish, ready for a `Checked { present: false }` reply.
    fn running_on_slow_stop(
        log: &Log,
        stopped: &Rc<Cell<bool>>,
    ) -> Rig<SlowStopAudio, impl FnMut() -> Result<Option<FakeDevice>> + use<>> {
        let (mut audio, mut host) = with_slow_stop(log, stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        (audio, host)
    }

    fn gone(epoch: u64) -> DeviceReply {
        DeviceReply::Checked {
            epoch,
            present: false,
        }
    }

    #[test]
    fn a_vanished_device_with_a_stream_still_running_is_retired_and_created_again_later() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = running_on_slow_stop(&log, &stopped);
        stopped.set(false);
        audio.on_reply(gone(1));
        // The path stopped, the device is kept, and nothing is sent.
        assert!(audio.has_retired_device());
        assert!(!audio.has_device());
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);
        assert_eq!(*log.borrow(), ["device created", "mic on", "mic off"]);
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));

        // The stream still runs: it is not released, nor taken back.
        audio.release_retired();
        audio.restore_device(Instant::now());
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);

        stopped.set(true);
        audio.release_retired();
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Release, DeviceRequest::Create { epoch: 2 }]
        );
        assert!(!audio.has_retired_device());
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(
            *log.borrow(),
            [
                "device created",
                "mic on",
                "mic off",
                "device dropped",
                "device created",
                "mic on"
            ]
        );
    }

    #[test]
    fn a_vanished_device_with_every_stream_ended_is_released_and_created_at_once() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = running_on_slow_stop(&log, &stopped);
        audio.on_reply(gone(1));
        assert!(!audio.has_retired_device());
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Release, DeviceRequest::Create { epoch: 2 }]
        );
        run(&mut audio, &mut host);
        assert!(audio.has_device());
    }

    #[test]
    fn a_vanished_retired_device_is_only_released_when_the_mic_went_off() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = running_on_slow_stop(&log, &stopped);
        stopped.set(false);
        audio.on_reply(gone(1));
        audio.set_mic(None).unwrap();
        assert!(audio.has_retired_device());
        stopped.set(true);
        audio.release_retired();
        assert!(!audio.has_retired_device());
        assert_eq!(
            audio.requests.sent(),
            [DeviceRequest::Create { epoch: 1 }, DeviceRequest::Release]
        );
        run(&mut audio, &mut host);
        assert_eq!(audio.waiting_for_device(), None);
        assert_eq!(log.borrow().last(), Some(&"device dropped"));
    }

    #[test]
    fn a_mic_turned_on_again_never_reuses_a_vanished_retired_device() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = running_on_slow_stop(&log, &stopped);
        stopped.set(false);
        audio.on_reply(gone(1));
        // The old stream still runs: the new path waits for it.
        audio.set_mic(on()).unwrap();
        assert!(audio.has_retired_device());
        assert_eq!(audio.requests.sent(), [DeviceRequest::Create { epoch: 1 }]);
        // It ended: a fresh device is created rather than the vanished one.
        stopped.set(true);
        audio.set_mic(on()).unwrap();
        assert_eq!(
            audio.requests.queued(),
            [DeviceRequest::Release, DeviceRequest::Create { epoch: 2 }]
        );
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        assert_eq!(log.borrow().last(), Some(&"mic on"));
    }

    #[test]
    fn a_vanished_answer_from_an_older_epoch_after_mic_off_on_changes_nothing() {
        let log = Log::default();
        let (mut audio, mut host) = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        audio.set_mic(on()).unwrap();
        assert_eq!(
            audio.requests.sent(),
            [
                DeviceRequest::Create { epoch: 1 },
                DeviceRequest::Release,
                DeviceRequest::Create { epoch: 2 }
            ]
        );
        let offs = log.borrow().iter().filter(|e| **e == "mic off").count();
        // A check of the first device is answered late, as gone.
        audio.on_reply(gone(1));
        assert_eq!(
            audio.requests.sent(),
            [
                DeviceRequest::Create { epoch: 1 },
                DeviceRequest::Release,
                DeviceRequest::Create { epoch: 2 }
            ]
        );
        assert_eq!(
            log.borrow().iter().filter(|e| **e == "mic off").count(),
            offs
        );
        assert_eq!(audio.waiting_for_device(), Some(DeviceWait::Pending));

        // Once epoch 2's device exists, the same late answer still changes
        // nothing: only its epoch tells it apart.
        run(&mut audio, &mut host);
        assert!(audio.has_device());
        let sent = audio.requests.sent().len();
        audio.on_reply(gone(1));
        assert_eq!(audio.requests.sent().len(), sent);
        assert_eq!(
            log.borrow().iter().filter(|e| **e == "mic off").count(),
            offs
        );
        assert!(audio.has_device());
        assert_eq!(log.borrow().last(), Some(&"mic on"));
    }

    #[test]
    fn shutdown_leaves_the_device_loaded_when_a_stream_is_left_running() {
        let log = Log::default();
        let stopped = Rc::new(Cell::new(true));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        let requests = audio.requests.clone();
        audio.shutdown(|_control| false);
        assert_eq!(requests.queued(), [DeviceRequest::Forget]);
        drain(&requests, &mut host);
        assert_eq!(*log.borrow(), ["device created", "mic on"]);

        // A retired device is left loaded too.
        let log = Log::default();
        let stopped = Rc::new(Cell::new(false));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        let requests = audio.requests.clone();
        audio.shutdown(|_control| false);
        drain(&requests, &mut host);
        assert!(!log.borrow().contains(&"device dropped"));

        // Once everything stopped, the retired device is dropped.
        let log = Log::default();
        let stopped = Rc::new(Cell::new(false));
        let (mut audio, mut host) = with_slow_stop(&log, &stopped);
        audio.set_mic(on()).unwrap();
        run(&mut audio, &mut host);
        audio.set_mic(None).unwrap();
        run(&mut audio, &mut host);
        let requests = audio.requests.clone();
        audio.shutdown(|_control| true);
        drain(&requests, &mut host);
        assert_eq!(log.borrow().last(), Some(&"device dropped"));
    }

    /// A device for the worker thread tests: counts its drops.
    struct CountedDevice(Arc<AtomicUsize>);

    impl VirtualDevice for CountedDevice {
        fn is_present(&mut self) -> bool {
            true
        }
    }

    impl Drop for CountedDevice {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The reply, without the error (which is not comparable).
    fn summary(reply: &DeviceReply) -> String {
        match reply {
            DeviceReply::Created { epoch, result } => match result {
                Ok(owned) => format!("created {epoch} {owned}"),
                Err(_) => format!("create failed {epoch}"),
            },
            DeviceReply::Checked { epoch, present } => format!("checked {epoch} {present}"),
            DeviceReply::Released => "released".to_string(),
        }
    }

    #[test]
    fn the_worker_answers_in_order_and_releases_or_forgets_the_device() {
        let creates = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let replies = Arc::new(Mutex::new(Vec::new()));
        let (counter, dropped, sink) = (creates.clone(), drops.clone(), replies.clone());
        let mut worker = DeviceWorker::spawn(
            move || -> Result<Option<CountedDevice>> {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(Some(CountedDevice(dropped.clone())))
            },
            move |reply| sink.lock().unwrap().push(summary(&reply)),
        )
        .unwrap();
        for request in [
            DeviceRequest::Create { epoch: 1 },
            // The held device is reused.
            DeviceRequest::Create { epoch: 2 },
            DeviceRequest::Check { epoch: 2 },
            DeviceRequest::Release,
            DeviceRequest::Check { epoch: 2 },
            DeviceRequest::Create { epoch: 3 },
            DeviceRequest::Forget,
        ] {
            worker.send(request);
        }
        assert!(worker.finish_within(Duration::from_secs(5)));
        assert_eq!(
            *replies.lock().unwrap(),
            [
                "created 1 true",
                "created 2 true",
                "checked 2 true",
                "released",
                "checked 2 false",
                "created 3 true"
            ]
        );
        assert_eq!(creates.load(Ordering::SeqCst), 2);
        // Released once; the forgotten one never dropped.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_worker_drops_a_device_it_still_holds_when_it_ends() {
        let drops = Arc::new(AtomicUsize::new(0));
        let dropped = drops.clone();
        let mut worker = DeviceWorker::spawn(
            move || -> Result<Option<CountedDevice>> { Ok(Some(CountedDevice(dropped.clone()))) },
            |_reply| {},
        )
        .unwrap();
        worker.send(DeviceRequest::Create { epoch: 1 });
        DeviceRequests::finish(worker);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_worker_reports_a_failed_create() {
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sink = replies.clone();
        let mut worker = DeviceWorker::spawn(
            || -> Result<Option<CountedDevice>> { anyhow::bail!("no server") },
            move |reply| sink.lock().unwrap().push(summary(&reply)),
        )
        .unwrap();
        worker.send(DeviceRequest::Create { epoch: 7 });
        assert!(worker.finish_within(Duration::from_secs(5)));
        assert_eq!(*replies.lock().unwrap(), ["create failed 7"]);
    }

    #[test]
    fn finish_returns_within_its_bound_when_a_create_blocks() {
        let (unblock, blocked) = mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        let mut worker = DeviceWorker::spawn(
            move || -> Result<Option<CountedDevice>> {
                // Blocks like `pactl` on a server that stopped answering,
                // until the test lets it go.
                let _ = blocked.lock().unwrap().recv();
                anyhow::bail!("pactl did not finish")
            },
            |_reply| {},
        )
        .unwrap();
        worker.send(DeviceRequest::Create { epoch: 1 });
        let start = Instant::now();
        assert!(!worker.finish_within(Duration::from_millis(100)));
        assert!(start.elapsed() < Duration::from_secs(2));
        // Lets the leftover thread end.
        drop(unblock);
    }
}
