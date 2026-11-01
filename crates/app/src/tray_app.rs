//! Tray mode: a menu bar / notification area icon whose menu lists the
//! sounds, plus global hotkeys.
//!
//! All UI objects live on the main thread inside the tao event loop. Menu
//! and hotkey events are forwarded into the loop as user events through an
//! [`EventLoopProxy`]; what they do is decided by [`actions`].

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use babound_engine::Engine;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

use crate::actions::{
    self, Action, ActionMap, MENU_OPEN_CONFIG, MENU_QUIT, MENU_RELOAD, MENU_STOP_ALL, Soundboard,
};
use crate::{paths, platform};

/// Tooltip of the tray icon.
const TOOLTIP: &str = "Sound-Babound";

/// Events forwarded into the event loop.
#[derive(Debug)]
enum UserEvent {
    Menu(MenuId),
    HotKey(u32),
}

/// Runs the tray app until "Quit".
///
/// Startup: create the default config if missing, load it (the built-in
/// default config is used if the file is unreadable or malformed), start the
/// engine, apply the config's master gain, then run the event loop. The tray
/// and the hotkeys are created once the loop has started. The config path is
/// kept even after a fallback, so "Reload config" and "Open config folder"
/// work and a fixed file can be reloaded.
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
    actions::apply_config(&engine, board.map());

    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    platform::prepare_event_loop(&mut event_loop);
    forward_events(event_loop.create_proxy());

    let mut app = TrayApp {
        engine: Some(engine),
        board,
        config_path,
        hotkeys: Hotkeys::disabled(),
        tray: None,
    };
    event_loop.run(move |event, _target, control_flow| match event {
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
        _ => {}
    })
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

/// Builds the tray menu: the sounds, then the fixed items.
fn build_menu(map: &ActionMap) -> Result<Menu> {
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
    for (id, label) in [
        (MENU_STOP_ALL, "Stop all"),
        (MENU_RELOAD, "Reload config"),
        (MENU_OPEN_CONFIG, "Open config folder"),
    ] {
        menu.append(&MenuItem::with_id(id, label, true, None))?;
    }
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(MENU_QUIT, "Quit", true, None))?;
    Ok(menu)
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
}

impl<M> Hotkeys<M> {
    fn disabled() -> Hotkeys<M> {
        Hotkeys {
            manager: None,
            registered: Vec::new(),
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
            Ok(manager) => Hotkeys {
                manager: Some(manager),
                registered: Vec::new(),
            },
            Err(err) => {
                tracing::warn!("cannot start global hotkeys: {err}; use the tray menu");
                Hotkeys::disabled()
            }
        }
    }
}

impl<M: HotkeyBackend> Hotkeys<M> {
    /// Registers each hotkey; a failure is logged and that hotkey skipped.
    fn register(&mut self, hotkeys: &[HotKey]) {
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
}

/// Everything the event loop owns.
struct TrayApp {
    /// `None` once the app has quit.
    engine: Option<Engine>,
    board: Soundboard,
    /// `None` if the config file location is unknown; reload is then
    /// unavailable. Kept when startup fell back to the built-in config
    /// because the file was malformed, so the fixed file can be reloaded.
    config_path: Option<PathBuf>,
    hotkeys: Hotkeys,
    /// Kept alive for as long as the icon should be shown.
    tray: Option<TrayIcon>,
}

impl TrayApp {
    /// Creates the tray icon and registers the hotkeys. Runs once the event
    /// loop has started, as the tray requires on macOS.
    fn init(&mut self) -> Result<()> {
        let menu = build_menu(self.board.map())?;
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
                if let Some(engine) = &self.engine {
                    self.board.play(index, engine);
                }
            }
            Action::StopAll => {
                if let Some(engine) = &self.engine
                    && !engine.stop_all()
                {
                    tracing::warn!("engine command queue is full; stop all dropped");
                }
            }
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
        let Some(engine) = &self.engine else {
            return;
        };
        let hotkeys = &mut self.hotkeys;
        let tray = &self.tray;
        let result = actions::reload(engine, &path, |board| {
            let menu = build_menu(board.map())?;
            hotkeys.replace(board.map().hotkeys());
            if let Some(tray) = tray {
                tray.set_menu(Some(Box::new(menu)));
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
            }
            Err(err) => tracing::error!("{err:#}; keeping the previous config"),
        }
    }

    /// Unregisters the hotkeys, shuts the engine down, removes the icon and
    /// leaves the event loop with `code`.
    fn quit(&mut self, control_flow: &mut ControlFlow, code: i32) {
        self.hotkeys.unregister_all();
        if let Some(engine) = self.engine.take() {
            engine.shutdown();
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
        Hotkeys {
            manager: Some(backend),
            registered: Vec::new(),
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
    fn disabled_hotkeys_register_nothing() {
        let mut hotkeys = Hotkeys::<FakeBackend>::disabled();
        hotkeys.register(&[key(Code::Digit1)]);
        assert!(hotkeys.registered.is_empty());
        hotkeys.replace(&[key(Code::Digit2)]);
        assert!(hotkeys.registered.is_empty());
    }
}
