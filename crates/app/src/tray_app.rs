//! Tray mode: a menu bar / notification area icon whose menu lists the
//! sounds, plus global hotkeys.
//!
//! All UI objects live on the main thread inside the tao event loop. Menu
//! and hotkey events, and the virtual mic worker's replies, are forwarded
//! into the loop as user events through an [`EventLoopProxy`]; what they do
//! is decided by [`actions`] and [`WithVirtualMic`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use babound_engine::{Engine, MicStatus};
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::actions::{
    self, Action, ActionMap, AudioControl, CaptureGate, MENU_OPEN_CONFIG, MENU_QUIT, MENU_RELOAD,
    MENU_SETTINGS, MENU_STOP_ALL, MENU_TOGGLE_MIC, Soundboard,
};
use crate::settings_process::{
    OpenOutcome, ProcessSpawner, SettingsLauncher, SettingsSpawner, config_snapshot,
};
use crate::virtual_mic::{DeviceReply, DeviceWait, DeviceWorker, WithVirtualMic};
use crate::{paths, platform};

/// Tooltip of the tray icon.
const TOOLTIP: &str = "Sound-Babound";

/// How often the mic status line is refreshed while the mic is enabled.
const MIC_POLL: Duration = Duration::from_secs(1);

/// The engine, with the virtual device the app creates itself on Linux,
/// which a worker thread creates and unloads.
type TrayAudio = WithVirtualMic<Engine, DeviceWorker>;

/// Events forwarded into the event loop.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    HotKey(u32),
    /// A reply from the virtual mic worker thread.
    Device(DeviceReply),
    /// The settings window's process has exited.
    SettingsClosed,
}

/// Runs the tray app until "Quit".
///
/// Startup: create the default config if missing, load it (the built-in
/// default config is used if the file is unreadable or malformed), start the
/// engine, apply the config's master gain and mic settings (creating the
/// Linux virtual device first when the mic is enabled), then run the event
/// loop. The tray and the hotkeys are created once the loop has started. The
/// config path is kept even after a fallback, so "Reload config" and "Open
/// config folder" work and a fixed file can be reloaded.
pub fn run() -> Result<()> {
    let config_path = match paths::ensure_default_config() {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!("{err:#}");
            None
        }
    };
    let board = actions::startup_board(config_path.as_deref())?;
    let engine = Engine::start().context("failed to start the audio engine")?;

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    platform::prepare_event_loop(&mut event_loop);
    forward_events(event_loop.create_proxy());
    // Replies sent before the loop runs wait in its queue. A send fails
    // only once the loop has ended, when nothing is left to tell.
    let reply_proxy = event_loop.create_proxy();
    let worker = DeviceWorker::spawn(platform::create_virtual_mic, move |reply| {
        let _ = reply_proxy.send_event(UserEvent::Device(reply));
    })?;
    let mut audio: TrayAudio = WithVirtualMic::new(engine, worker);
    let mut capture_gate = CaptureGate::default();
    let capture = capture_gate.capture(platform::microphone_access(), board.map().mic());
    actions::apply_config(&mut audio, board.map(), capture);

    // The waiter threads' callback must be `Sync`; the proxy only promises
    // `Send`. A send fails only once the loop has ended (the tray quit and
    // left the window running), when nothing is left to tell.
    let settings_proxy = Mutex::new(event_loop.create_proxy());
    let settings = SettingsLauncher::new(ProcessSpawner::new(Arc::new(move || {
        if let Ok(proxy) = settings_proxy.lock() {
            let _ = proxy.send_event(UserEvent::SettingsClosed);
        }
    })));

    let mut app = TrayApp {
        audio: Some(audio),
        board,
        config_path,
        hotkeys: Hotkeys::disabled(),
        tray: None,
        mic_items: None,
        mic_label: String::new(),
        next_mic_poll: None,
        capture_gate,
        settings,
    };
    app.mic_label = app.current_mic_label();
    event_loop.run(move |event, _target, control_flow| {
        match event {
            Event::NewEvents(StartCause::Init) => {
                *control_flow = ControlFlow::Wait;
                if let Err(err) = app.init() {
                    tracing::error!("{err:#}");
                    app.quit(control_flow, 1);
                }
            }
            Event::UserEvent(UserEvent::Menu(id)) => {
                if let Some(action) = app.board.map().for_menu_id(id.as_ref()) {
                    app.dispatch(action, control_flow);
                }
            }
            Event::UserEvent(UserEvent::HotKey(id)) => {
                if let Some(action) = app.board.map().for_hotkey(id) {
                    app.dispatch(action, control_flow);
                }
            }
            Event::UserEvent(UserEvent::Device(reply)) => app.on_device_reply(reply),
            Event::UserEvent(UserEvent::SettingsClosed) => app.on_settings_closed(),
            _ => {}
        }
        app.schedule_mic_poll(control_flow);
    })
}

/// When the mic status is next due: `(refresh now, next deadline)`. Pure.
///
/// With the mic off there is no deadline, so the event loop just waits.
/// With it on, a missing or passed deadline means refresh now and wake again
/// in [`MIC_POLL`]; otherwise the deadline stands.
fn mic_poll(enabled: bool, next: Option<Instant>, now: Instant) -> (bool, Option<Instant>) {
    if !enabled {
        return (false, None);
    }
    match next {
        Some(deadline) if deadline > now => (false, Some(deadline)),
        _ => (true, Some(now + MIC_POLL)),
    }
}

/// The tray's mic status: the virtual mic's wait state while the mic path
/// waits for its device, otherwise the engine's status (asked only then).
/// Pure.
///
/// A device being created (or a stopped mic stream that has yet to end)
/// shows as starting; a failed create shows as not found for its pattern,
/// which makes the mic poll retry it.
fn tray_mic_status(wait: Option<DeviceWait<'_>>, engine: impl FnOnce() -> MicStatus) -> MicStatus {
    match wait {
        Some(DeviceWait::Pending) => MicStatus::Starting,
        Some(DeviceWait::Unavailable(pattern)) => MicStatus::NotFound(pattern.to_string()),
        None => engine(),
    }
}

/// The event loop's control flow for a mic poll deadline.
fn control_flow_for(next: Option<Instant>) -> ControlFlow {
    next.map_or(ControlFlow::Wait, ControlFlow::WaitUntil)
}

/// Sends menu clicks and hotkey presses into the event loop.
fn forward_events(proxy: EventLoopProxy<UserEvent>) {
    // The handlers must be `Sync`; the proxy only promises `Send`.
    let menu_proxy = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Ok(proxy) = menu_proxy.lock() {
            let _ = proxy.send_event(UserEvent::Menu(event.id));
        }
    }));
    let hotkey_proxy = Mutex::new(proxy);
    GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
        if event.state() == HotKeyState::Pressed
            && let Ok(proxy) = hotkey_proxy.lock()
        {
            let _ = proxy.send_event(UserEvent::HotKey(event.id()));
        }
    }));
}

/// The menu items whose text follows the mic state.
struct MicItems {
    /// The disabled status line.
    status: MenuItem,
    /// "Mic to call: on/off".
    toggle: MenuItem,
}

/// Builds the tray menu: the sounds, the mic status and toggle, then the
/// fixed items. `mic_label` is the status line's text.
fn build_menu(map: &ActionMap, mic_label: &str) -> Result<(Menu, MicItems)> {
    let menu = Menu::new();
    if map.sound_items().is_empty() {
        menu.append(&MenuItem::new("No sounds configured", false, None))?;
    }
    for item in map.sound_items() {
        menu.append(&MenuItem::with_id(
            item.menu_id.as_str(),
            &item.label,
            true,
            None,
        ))?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let mic = MicItems {
        status: MenuItem::new(mic_label, false, None),
        toggle: MenuItem::with_id(
            MENU_TOGGLE_MIC,
            actions::mic_toggle_label(map.mic().enabled),
            true,
            None,
        ),
    };
    menu.append(&mic.status)?;
    menu.append(&mic.toggle)?;
    menu.append(&PredefinedMenuItem::separator())?;
    for (id, label) in [
        (MENU_STOP_ALL, "Stop all"),
        (MENU_SETTINGS, "Settings…"),
        (MENU_RELOAD, "Reload config"),
        (MENU_OPEN_CONFIG, "Open config folder"),
    ] {
        menu.append(&MenuItem::with_id(id, label, true, None))?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(MENU_QUIT, "Quit", true, None))?;
    Ok((menu, mic))
}

/// What [`Hotkeys`] needs from the OS hotkey manager. A recording fake
/// stands in for it in tests.
trait HotkeyBackend {
    fn register(&self, hotkey: HotKey) -> global_hotkey::Result<()>;
    fn unregister(&self, hotkey: HotKey) -> global_hotkey::Result<()>;
}

impl HotkeyBackend for GlobalHotKeyManager {
    fn register(&self, hotkey: HotKey) -> global_hotkey::Result<()> {
        GlobalHotKeyManager::register(self, hotkey)
    }

    fn unregister(&self, hotkey: HotKey) -> global_hotkey::Result<()> {
        GlobalHotKeyManager::unregister(self, hotkey)
    }
}

/// The registered global hotkeys.
struct Hotkeys<M = GlobalHotKeyManager> {
    /// `None` when global hotkeys are unavailable (Wayland, or the manager
    /// failed to start).
    manager: Option<M>,
    registered: Vec<HotKey>,
    /// Set while the settings window is open: nothing is registered, so the
    /// window can see (and re-assign) the tray's combinations.
    paused: bool,
}

impl<M> Hotkeys<M> {
    fn disabled() -> Hotkeys<M> {
        Hotkeys::with_manager(None)
    }

    fn with_manager(manager: Option<M>) -> Hotkeys<M> {
        Hotkeys {
            manager,
            registered: Vec::new(),
            paused: false,
        }
    }
}

impl Hotkeys {
    /// Starts the hotkey manager if the platform supports global hotkeys.
    fn start() -> Hotkeys {
        if !platform::global_hotkeys_supported() {
            tracing::warn!(
                "global hotkeys are not supported in a Wayland session; \
                 sounds can still be played from the tray menu"
            );
            return Hotkeys::disabled();
        }
        match GlobalHotKeyManager::new() {
            Ok(manager) => Hotkeys::with_manager(Some(manager)),
            Err(err) => {
                tracing::warn!("cannot start global hotkeys: {err}; use the tray menu");
                Hotkeys::disabled()
            }
        }
    }
}

impl<M: HotkeyBackend> Hotkeys<M> {
    /// Registers each hotkey; a failure is logged and that hotkey skipped.
    /// Registers nothing while paused.
    fn register(&mut self, hotkeys: &[HotKey]) {
        if self.paused {
            return;
        }
        let Some(manager) = &self.manager else {
            return;
        };
        for &hotkey in hotkeys {
            match manager.register(hotkey) {
                Ok(()) => {
                    tracing::debug!(%hotkey, "hotkey registered");
                    self.registered.push(hotkey);
                }
                Err(err) => tracing::warn!(%hotkey, "cannot register hotkey: {err}; skipped"),
            }
        }
    }

    fn unregister_all(&mut self) {
        let Some(manager) = &self.manager else {
            return;
        };
        for hotkey in self.registered.drain(..) {
            if let Err(err) = manager.unregister(hotkey) {
                tracing::debug!(%hotkey, "cannot unregister hotkey: {err}");
            }
        }
    }

    /// Swaps the registered set for `hotkeys`. Everything old is unregistered
    /// first, so a hotkey kept across a reload registers cleanly.
    fn replace(&mut self, hotkeys: &[HotKey]) {
        self.unregister_all();
        self.register(hotkeys);
    }

    /// Unregisters everything and registers nothing until [`Self::resume`],
    /// a reload meanwhile included.
    fn pause(&mut self) {
        self.unregister_all();
        self.paused = true;
    }

    /// Ends a pause and registers `hotkeys` (the current board's).
    fn resume(&mut self, hotkeys: &[HotKey]) {
        self.paused = false;
        self.replace(hotkeys);
    }
}

/// Starts the settings window and pauses the hotkeys while it is open, so
/// it can see the tray's combinations. `snapshot` is the config file's
/// bytes from before the spawn.
fn open_settings<S: SettingsSpawner, M: HotkeyBackend>(
    launcher: &mut SettingsLauncher<S>,
    hotkeys: &mut Hotkeys<M>,
    snapshot: Option<Vec<u8>>,
) {
    match launcher.open(snapshot) {
        OpenOutcome::Launched => {
            hotkeys.pause();
            tracing::info!("settings window opened; hotkeys are paused until it closes");
        }
        OpenOutcome::AlreadyOpen => tracing::info!("the settings window is already open"),
        OpenOutcome::Failed(err) => tracing::error!("{err:#}"),
    }
}

/// Everything the event loop owns.
struct TrayApp {
    /// `None` once the app has quit.
    audio: Option<TrayAudio>,
    board: Soundboard,
    /// `None` if the config file location is unknown; reload is then
    /// unavailable. Kept when startup fell back to the built-in config
    /// because the file was malformed, so the fixed file can be reloaded.
    config_path: Option<PathBuf>,
    hotkeys: Hotkeys,
    /// Kept alive for as long as the icon should be shown.
    tray: Option<TrayIcon>,
    /// The current menu's mic items; `None` until the tray exists.
    mic_items: Option<MicItems>,
    /// The text the mic status line shows.
    mic_label: String,
    /// When the mic status line is refreshed next; `None` with the mic off.
    next_mic_poll: Option<Instant>,
    /// Turns the microphone permission into the mic path's capture flag.
    capture_gate: CaptureGate,
    /// The settings window: one at a time; the hotkeys pause while it runs.
    settings: SettingsLauncher<ProcessSpawner>,
}

impl TrayApp {
    /// Creates the tray icon and registers the hotkeys. Runs once the event
    /// loop has started, as the tray requires on macOS.
    fn init(&mut self) -> Result<()> {
        let (menu, mic_items) = build_menu(self.board.map(), &self.mic_label)?;
        self.mic_items = Some(mic_items);
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(TOOLTIP);
        let tray = platform::with_tray_icon(builder)?
            .build()
            .context("failed to create the tray icon")?;
        self.tray = Some(tray);
        self.hotkeys = Hotkeys::start();
        self.hotkeys.register(self.board.map().hotkeys());
        tracing::info!(
            sounds = self.board.map().sound_items().len(),
            hotkeys = self.hotkeys.registered.len(),
            "tray ready"
        );
        Ok(())
    }

    fn dispatch(&mut self, action: Action, control_flow: &mut ControlFlow) {
        tracing::debug!(?action, "action");
        match action {
            Action::Play(index) => {
                if let Some(audio) = &self.audio {
                    self.board.play(index, audio);
                }
            }
            Action::StopAll => {
                if let Some(audio) = &self.audio
                    && !audio.control().stop_all()
                {
                    tracing::warn!("engine command queue is full; stop all dropped");
                }
            }
            Action::ToggleMic => self.toggle_mic(),
            Action::OpenSettings => open_settings(
                &mut self.settings,
                &mut self.hotkeys,
                config_snapshot(self.config_path.as_deref()),
            ),
            Action::Reload => self.reload(),
            Action::OpenConfigFolder => open_config_folder(),
            Action::Quit => self.quit(control_flow, 0),
        }
    }

    /// Re-reads the config and swaps hotkeys, menu and sounds. On any error
    /// the previous config stays in effect. Playing voices continue.
    fn reload(&mut self) {
        let Some(path) = self.config_path.clone() else {
            tracing::warn!("the config file location is unknown; nothing to reload");
            return;
        };
        let Some(audio) = &mut self.audio else {
            return;
        };
        let hotkeys = &mut self.hotkeys;
        let tray = &self.tray;
        let mic_label = &self.mic_label;
        let mut new_mic_items = None;
        let capture_gate = &mut self.capture_gate;
        let capture = |mic: &_| capture_gate.capture(platform::microphone_access(), mic);
        // `apply_config` inside `reload` diffs the mic settings: the monitor
        // keeps playing, and the mic path restarts only if its device changed.
        let result = actions::reload(audio, &path, capture, |board| {
            let (menu, mic_items) = build_menu(board.map(), mic_label)?;
            hotkeys.replace(board.map().hotkeys());
            if let Some(tray) = tray {
                tray.set_menu(Some(Box::new(menu)));
                new_mic_items = Some(mic_items);
            }
            Ok(())
        });
        match result {
            Ok(board) => {
                tracing::info!(
                    sounds = board.map().sound_items().len(),
                    hotkeys = self.hotkeys.registered.len(),
                    master_gain = board.map().master_gain(),
                    "config reloaded"
                );
                // The old sounds are released here; voices still playing
                // them hold their own handles, freed via the garbage queue.
                self.board = board;
                if new_mic_items.is_some() {
                    self.mic_items = new_mic_items;
                }
                self.refresh_mic_status();
            }
            Err(err) => tracing::error!("{err:#}; keeping the previous config"),
        }
    }

    /// The settings window closed: reloads the config if the window changed
    /// the file (the hotkeys still paused, so the reload registers none),
    /// then registers the current board's hotkeys again, the new board's
    /// after a reload and the previous one's if it failed or was not needed.
    fn on_settings_closed(&mut self) {
        let current = config_snapshot(self.config_path.as_deref());
        if self.settings.closed(current) {
            self.reload();
        }
        self.hotkeys.resume(self.board.map().hotkeys());
        tracing::info!(
            hotkeys = self.hotkeys.registered.len(),
            "settings window closed; hotkeys registered again"
        );
    }

    /// Flips the mic, saves it, applies it and updates both mic items.
    fn toggle_mic(&mut self) {
        let Some(audio) = &mut self.audio else {
            return;
        };
        let capture_gate = &mut self.capture_gate;
        let capture = |mic: &_| capture_gate.capture(platform::microphone_access(), mic);
        let enabled =
            actions::toggle_mic(audio, &mut self.board, self.config_path.as_deref(), capture);
        if let Some(items) = &self.mic_items {
            items.toggle.set_text(actions::mic_toggle_label(enabled));
        }
        self.refresh_mic_status();
    }

    /// Re-applies the mic path if the microphone permission changed the
    /// capture flag since it was last decided.
    fn recheck_capture(&mut self) {
        if let Some(audio) = &mut self.audio {
            actions::recheck_capture(
                audio,
                self.board.map().mic(),
                &mut self.capture_gate,
                platform::microphone_access(),
            );
        }
    }

    /// Creates the app's own virtual device again if the mic sink cannot
    /// find it because it is gone (a sound server restart drops it), or if
    /// the mic path waits for it because creating it failed.
    fn restore_virtual_mic(&mut self) {
        if !matches!(self.mic_status(), MicStatus::NotFound(_)) {
            return;
        }
        if let Some(audio) = &mut self.audio {
            audio.restore_device(Instant::now());
        }
    }

    /// The engine's current mic status, or not found while the mic path
    /// waits for the virtual device to be created.
    fn mic_status(&self) -> MicStatus {
        self.audio.as_ref().map_or(MicStatus::Off, |audio| {
            tray_mic_status(audio.waiting_for_device(), || audio.control().mic_status())
        })
    }

    /// The status line's text for the engine's current mic status.
    fn current_mic_label(&self) -> String {
        actions::mic_status_label(&self.mic_status(), platform::virtual_mic_not_found())
    }

    /// Updates the status line, only if its text changed. A change is
    /// logged; one to an active mic sink also logs how to use it.
    fn refresh_mic_status(&mut self) {
        let status = self.mic_status();
        let label = actions::mic_status_label(&status, platform::virtual_mic_not_found());
        if label == self.mic_label {
            return;
        }
        tracing::info!("{label}");
        if matches!(status, MicStatus::Active { .. }) {
            tracing::info!("{}", platform::virtual_mic_hint());
        }
        if let Some(items) = &self.mic_items {
            items.status.set_text(&label);
        }
        self.mic_label = label;
    }

    /// Refreshes the mic status when due and sets the event loop to wake
    /// for the next refresh while the mic is enabled, or while a retired
    /// virtual device waits for a mic stream that did not stop in time.
    /// Leaves an exit alone.
    fn schedule_mic_poll(&mut self, control_flow: &mut ControlFlow) {
        if self.audio.is_none() || matches!(control_flow, ControlFlow::ExitWithCode(_)) {
            return;
        }
        let Some(audio) = &self.audio else {
            return;
        };
        let (refresh, next) = mic_poll(
            self.board.map().mic().enabled || audio.has_retired_device(),
            self.next_mic_poll,
            Instant::now(),
        );
        if refresh {
            if let Some(audio) = &mut self.audio {
                audio.release_retired();
                if let Err(err) = audio.resume_mic() {
                    tracing::error!("{err:#}; the mic path did not start");
                }
            }
            self.recheck_capture();
            self.restore_virtual_mic();
            self.refresh_mic_status();
        }
        self.next_mic_poll = next;
        *control_flow = control_flow_for(next);
    }

    /// Hands a virtual mic worker reply to the audio, which starts the mic
    /// path once its device exists, and refreshes the status line. A reply
    /// after quit is dropped.
    fn on_device_reply(&mut self, reply: DeviceReply) {
        if let Some(audio) = &mut self.audio {
            audio.on_reply(reply);
            self.refresh_mic_status();
        }
    }

    /// Unregisters the hotkeys, shuts the engine down and then drops the
    /// virtual device (or leaves it loaded if a mic stream did not stop in
    /// time), removes the icon and leaves the event loop with `code`. An
    /// open settings window keeps running on its own.
    fn quit(&mut self, control_flow: &mut ControlFlow, code: i32) {
        self.hotkeys.unregister_all();
        if let Some(audio) = self.audio.take() {
            audio.shutdown(Engine::shutdown);
        }
        self.tray = None;
        *control_flow = ControlFlow::ExitWithCode(code);
    }
}

/// Opens the config folder in the system file manager.
fn open_config_folder() {
    match paths::config_dir() {
        Ok(dir) => {
            if let Err(err) = opener::open(&dir) {
                tracing::warn!(path = %dir.display(), "cannot open the config folder: {err}");
            }
        }
        Err(err) => tracing::warn!("{err:#}"),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use global_hotkey::hotkey::{Code, Modifiers};

    use super::*;

    /// Tracks what the OS would have registered, and rejects like it does.
    #[derive(Default)]
    struct FakeBackend {
        live: RefCell<HashSet<HotKey>>,
        /// Hotkeys the "OS" refuses (taken by another app).
        taken: HashSet<HotKey>,
    }

    impl HotkeyBackend for FakeBackend {
        fn register(&self, hotkey: HotKey) -> global_hotkey::Result<()> {
            if self.taken.contains(&hotkey) {
                return Err(global_hotkey::Error::FailedToRegister(hotkey.to_string()));
            }
            if !self.live.borrow_mut().insert(hotkey) {
                return Err(global_hotkey::Error::AlreadyRegistered(hotkey));
            }
            Ok(())
        }

        fn unregister(&self, hotkey: HotKey) -> global_hotkey::Result<()> {
            if self.live.borrow_mut().remove(&hotkey) {
                Ok(())
            } else {
                Err(global_hotkey::Error::FailedToUnRegister(hotkey))
            }
        }
    }

    fn key(code: Code) -> HotKey {
        HotKey::new(Some(Modifiers::SHIFT), code)
    }

    fn with_backend(backend: FakeBackend) -> Hotkeys<FakeBackend> {
        Hotkeys::with_manager(Some(backend))
    }

    /// Counts spawns; fails while `fail` is set.
    #[derive(Default)]
    struct FakeSpawner {
        spawns: usize,
        fail: bool,
    }

    impl SettingsSpawner for FakeSpawner {
        fn spawn(&mut self) -> Result<()> {
            if self.fail {
                anyhow::bail!("cannot launch");
            }
            self.spawns += 1;
            Ok(())
        }
    }

    fn live(hotkeys: &Hotkeys<FakeBackend>) -> HashSet<HotKey> {
        hotkeys.manager.as_ref().unwrap().live.borrow().clone()
    }

    #[test]
    fn replace_keeps_a_hotkey_shared_by_old_and_new_config() {
        let (a, b, c) = (key(Code::Digit1), key(Code::Digit2), key(Code::Digit3));
        let mut hotkeys = with_backend(FakeBackend::default());
        hotkeys.register(&[a, b]);
        hotkeys.replace(&[b, c]);
        assert_eq!(hotkeys.registered, vec![b, c]);
        assert_eq!(live(&hotkeys), HashSet::from([b, c]));
    }

    #[test]
    fn failed_registration_is_skipped_and_not_counted() {
        let (a, c) = (key(Code::Digit1), key(Code::Digit3));
        let mut hotkeys = with_backend(FakeBackend {
            taken: HashSet::from([c]),
            ..FakeBackend::default()
        });
        hotkeys.register(&[a, c]);
        assert_eq!(hotkeys.registered, vec![a]);
        assert_eq!(live(&hotkeys), HashSet::from([a]));
        // Quitting later unregisters only what was registered, cleanly.
        hotkeys.unregister_all();
        assert!(hotkeys.registered.is_empty());
        assert!(live(&hotkeys).is_empty());
    }

    #[test]
    fn unregister_all_leaves_nothing_registered() {
        let mut hotkeys = with_backend(FakeBackend::default());
        hotkeys.register(&[key(Code::Digit1), key(Code::Digit2)]);
        hotkeys.unregister_all();
        assert!(hotkeys.registered.is_empty());
        assert!(live(&hotkeys).is_empty());
    }

    #[test]
    fn a_pause_registers_nothing_until_resumed() {
        let (a, b) = (key(Code::Digit1), key(Code::Digit2));
        let mut hotkeys = with_backend(FakeBackend::default());
        hotkeys.register(&[a]);
        hotkeys.pause();
        assert!(hotkeys.registered.is_empty());
        assert!(live(&hotkeys).is_empty());
        // A reload while paused registers nothing either.
        hotkeys.replace(&[a, b]);
        hotkeys.register(&[b]);
        assert!(live(&hotkeys).is_empty());
        hotkeys.resume(&[a, b]);
        assert_eq!(hotkeys.registered, vec![a, b]);
        assert_eq!(live(&hotkeys), HashSet::from([a, b]));
    }

    #[test]
    fn hotkeys_pause_while_the_settings_window_is_open() {
        let (a, b) = (key(Code::Digit1), key(Code::Digit2));
        let board = [a, b];
        let mut hotkeys = with_backend(FakeBackend::default());
        hotkeys.register(&board);
        let mut launcher = SettingsLauncher::new(FakeSpawner::default());
        let snapshot = Some(b"config".to_vec());

        open_settings(&mut launcher, &mut hotkeys, snapshot.clone());
        assert!(launcher.is_open());
        assert!(live(&hotkeys).is_empty());
        // A second click starts nothing and keeps the pause.
        open_settings(&mut launcher, &mut hotkeys, snapshot.clone());
        assert!(live(&hotkeys).is_empty());
        assert_eq!(launcher.spawner().spawns, 1);

        // Closed without changes: no reload, the same hotkeys come back.
        assert!(!launcher.closed(snapshot));
        hotkeys.resume(&board);
        assert_eq!(live(&hotkeys), HashSet::from(board));
    }

    #[test]
    fn a_failed_settings_launch_keeps_the_hotkeys() {
        let a = key(Code::Digit1);
        let mut hotkeys = with_backend(FakeBackend::default());
        hotkeys.register(&[a]);
        let mut launcher = SettingsLauncher::new(FakeSpawner {
            fail: true,
            ..FakeSpawner::default()
        });
        open_settings(&mut launcher, &mut hotkeys, None);
        assert!(!launcher.is_open());
        assert!(!hotkeys.paused);
        assert_eq!(live(&hotkeys), HashSet::from([a]));
    }

    #[test]
    fn mic_poll_waits_with_the_mic_off() {
        let now = Instant::now();
        assert_eq!(mic_poll(false, None, now), (false, None));
        // Turning the mic off drops a pending deadline.
        assert_eq!(mic_poll(false, Some(now + MIC_POLL), now), (false, None));
        assert_eq!(control_flow_for(None), ControlFlow::Wait);
    }

    #[test]
    fn mic_poll_refreshes_every_second_with_the_mic_on() {
        let now = Instant::now();
        // Just turned on: refresh now, then wake in a second.
        assert_eq!(mic_poll(true, None, now), (true, Some(now + MIC_POLL)));
        // An event before the deadline keeps it and refreshes nothing.
        let deadline = now + MIC_POLL;
        let early = now + Duration::from_millis(300);
        assert_eq!(
            mic_poll(true, Some(deadline), early),
            (false, Some(deadline))
        );
        // At or past the deadline: refresh and schedule the next one.
        assert_eq!(
            mic_poll(true, Some(deadline), deadline),
            (true, Some(deadline + MIC_POLL))
        );
        let late = deadline + Duration::from_millis(50);
        assert_eq!(
            mic_poll(true, Some(deadline), late),
            (true, Some(late + MIC_POLL))
        );
        assert_eq!(
            control_flow_for(Some(deadline)),
            ControlFlow::WaitUntil(deadline)
        );
    }

    #[test]
    fn tray_mic_status_maps_the_device_wait() {
        // A device being created shows as starting, without asking the engine.
        let unasked = || -> MicStatus { panic!("the engine is not asked while waiting") };
        assert_eq!(
            tray_mic_status(Some(DeviceWait::Pending), unasked),
            MicStatus::Starting
        );
        // A failed create shows as not found for its pattern.
        assert_eq!(
            tray_mic_status(Some(DeviceWait::Unavailable("Sound-Babound")), unasked),
            MicStatus::NotFound("Sound-Babound".to_string())
        );
        // Without a wait, the engine's status stands.
        for engine in [
            MicStatus::Off,
            MicStatus::Starting,
            MicStatus::NotFound("BlackHole".to_string()),
        ] {
            let expected = engine.clone();
            assert_eq!(tray_mic_status(None, move || engine), expected);
        }
    }

    #[test]
    fn disabled_hotkeys_register_nothing() {
        let mut hotkeys = Hotkeys::<FakeBackend>::disabled();
        hotkeys.register(&[key(Code::Digit1)]);
        assert!(hotkeys.registered.is_empty());
        hotkeys.replace(&[key(Code::Digit2)]);
        assert!(hotkeys.registered.is_empty());
    }
}
