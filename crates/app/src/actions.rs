//! What the tray menu and the global hotkeys do.
//!
//! This module is UI-free: it maps menu item ids and hotkey ids to
//! [`Action`]s, owns the decoded sounds, and applies a config to anything
//! implementing [`AudioControl`]. The tray only wires events to it.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use babound_engine::{CaptureStatus, Engine, MicSettings, MicStatus, Sound};
use global_hotkey::hotkey::HotKey;

use crate::config::{self, DEFAULT_CONFIG, MicConfig, RawConfig, ResolvedConfig};
use crate::platform::MicrophoneAccess;
use crate::{hotkeys, platform};

/// Something the tray can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Play the sound at this index of the [`ActionMap`]'s sound list.
    Play(usize),
    StopAll,
    /// Turn the virtual mic on or off, and save that to the config.
    ToggleMic,
    Reload,
    OpenConfigFolder,
    Quit,
}

/// Menu id of the "Stop all" item.
pub const MENU_STOP_ALL: &str = "stop-all";
/// Menu id of the "Mic to call" item.
pub const MENU_TOGGLE_MIC: &str = "toggle-mic";
/// Menu id of the "Reload config" item.
pub const MENU_RELOAD: &str = "reload";
/// Menu id of the "Open config folder" item.
pub const MENU_OPEN_CONFIG: &str = "open-config";
/// Menu id of the "Quit" item.
pub const MENU_QUIT: &str = "quit";

/// Menu id of the sound at `index`.
pub fn sound_menu_id(index: usize) -> String {
    format!("sound-{index}")
}

/// A sound as listed in the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundItem {
    pub menu_id: String,
    pub label: String,
}

/// Maps menu item ids and hotkey ids to actions for one config.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionMap {
    sound_items: Vec<SoundItem>,
    menu: HashMap<String, Action>,
    hotkeys: HashMap<u32, Action>,
    hotkey_list: Vec<HotKey>,
    master_gain: f32,
    mic: MicConfig,
}

impl ActionMap {
    /// Builds the map. Sound `i` of `config` becomes [`Action::Play`]`(i)`.
    /// Every sound gets a menu item, including those whose hotkey was
    /// skipped during validation.
    pub fn new(config: &ResolvedConfig) -> ActionMap {
        let mut menu = HashMap::new();
        let mut hotkeys = HashMap::new();
        let mut hotkey_list = Vec::new();
        let mut sound_items = Vec::with_capacity(config.sounds.len());

        for (index, sound) in config.sounds.iter().enumerate() {
            let menu_id = sound_menu_id(index);
            menu.insert(menu_id.clone(), Action::Play(index));
            sound_items.push(SoundItem {
                menu_id,
                label: sound.name.clone(),
            });
            if let Some(hotkey) = sound.hotkey {
                hotkeys.insert(hotkey.id(), Action::Play(index));
                hotkey_list.push(hotkey);
            }
        }
        if let Some(hotkey) = config.stop_all_hotkey {
            hotkeys.insert(hotkey.id(), Action::StopAll);
            hotkey_list.push(hotkey);
        }
        for (id, action) in [
            (MENU_STOP_ALL, Action::StopAll),
            (MENU_TOGGLE_MIC, Action::ToggleMic),
            (MENU_RELOAD, Action::Reload),
            (MENU_OPEN_CONFIG, Action::OpenConfigFolder),
            (MENU_QUIT, Action::Quit),
        ] {
            menu.insert(id.to_string(), action);
        }

        ActionMap {
            sound_items,
            menu,
            hotkeys,
            hotkey_list,
            master_gain: config.master_gain,
            mic: config.mic.clone(),
        }
    }

    /// The action of a menu item, if the id is known.
    pub fn for_menu_id(&self, id: &str) -> Option<Action> {
        self.menu.get(id).copied()
    }

    /// The action of a hotkey, if the id is known.
    pub fn for_hotkey(&self, id: u32) -> Option<Action> {
        self.hotkeys.get(&id).copied()
    }

    /// The sounds in menu order.
    pub fn sound_items(&self) -> &[SoundItem] {
        &self.sound_items
    }

    /// Every hotkey to register: sound hotkeys in order, then stop-all.
    pub fn hotkeys(&self) -> &[HotKey] {
        &self.hotkey_list
    }

    /// The validated (clamped) master gain.
    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    /// The validated `[mic]` table.
    pub fn mic(&self) -> &MicConfig {
        &self.mic
    }
}

/// The audio controls the tray needs from the engine.
pub trait AudioControl {
    /// Queues a play. Returns false if the engine rejected it (no output
    /// stream, or a full command queue).
    fn play(&self, sound: &Sound, gain: f32) -> bool;
    /// Queues a master gain change. Returns false if the engine rejected it.
    fn set_master_gain(&self, gain: f32) -> bool;
    /// Turns the mic path on (`Some`), off (`None`) or changes it. Fails if
    /// the mic path cannot be started; it is then off.
    fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()>;
}

impl AudioControl for Engine {
    fn play(&self, sound: &Sound, gain: f32) -> bool {
        Engine::play(self, sound, gain)
    }

    fn set_master_gain(&self, gain: f32) -> bool {
        Engine::set_master_gain(self, gain)
    }

    fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
        Engine::set_mic(self, settings).context("failed to start the mic path")
    }
}

/// The engine's mic settings for `mic`: `None` when the mic is off.
/// See [`settings_for`].
pub fn mic_settings(mic: &MicConfig, capture: bool) -> Option<MicSettings> {
    mic.enabled.then(|| settings_for(mic, capture))
}

/// The engine's mic settings for `mic`, whether or not it is enabled.
/// The capture refuses the platform's virtual inputs for the configured
/// device. `capture` is false when the microphone may not be opened, so the
/// mic sink carries the sounds only.
pub fn settings_for(mic: &MicConfig, capture: bool) -> MicSettings {
    MicSettings {
        device_match: mic.virtual_device.clone(),
        excluded_inputs: platform::virtual_input_exclusions(&mic.virtual_device),
        sound_gain: mic.sound_gain,
        mic_gain: mic.mic_gain,
        capture,
    }
}

/// Turns the OS microphone permission into the `capture` flag of
/// [`mic_settings`], and warns once when a denial keeps the capture off.
///
/// The warning is logged only while the mic is enabled, and again only after
/// the permission stopped being denied in between.
#[derive(Debug, Default)]
pub struct CaptureGate {
    denial_logged: bool,
    /// The last decision, which the caller applied.
    decided: Option<bool>,
}

impl CaptureGate {
    /// Whether the capture may be opened for `mic` under `access`.
    pub fn capture(&mut self, access: MicrophoneAccess, mic: &MicConfig) -> bool {
        let allowed = access.allows_capture();
        self.decided = Some(allowed);
        if allowed {
            self.denial_logged = false;
        } else if mic.enabled && !self.denial_logged {
            self.denial_logged = true;
            tracing::warn!(
                "microphone access is denied; the virtual mic carries the sounds only. \
                 Allow Sound-Babound under System Settings > Privacy & Security > Microphone"
            );
        }
        allowed
    }
}

/// Re-reads the microphone permission (`access`) while `mic` is enabled and,
/// if it changed the capture flag since the gate's last decision, re-applies
/// the mic path with the new flag. The tray calls it on every mic poll, so a
/// permission granted or denied while the app runs (including the answer to
/// the first-run prompt) takes effect without a toggle or a reload. Returns
/// whether the mic path was re-applied.
pub fn recheck_capture(
    control: &mut impl AudioControl,
    mic: &MicConfig,
    gate: &mut CaptureGate,
    access: MicrophoneAccess,
) -> bool {
    if !mic.enabled {
        return false;
    }
    let before = gate.decided;
    let capture = gate.capture(access, mic);
    if before == Some(capture) {
        return false;
    }
    apply_mic(control, mic, capture);
    true
}

/// Applies the engine-wide parts of a config: the master gain and the mic
/// path (see [`mic_settings`] for `capture`). Called at startup and after
/// every successful reload. A mic path that fails to start is logged, and
/// the app runs without it.
pub fn apply_config(control: &mut impl AudioControl, map: &ActionMap, capture: bool) {
    if !control.set_master_gain(map.master_gain()) {
        tracing::warn!(
            master_gain = map.master_gain(),
            "engine command queue is full; master gain not applied"
        );
    }
    apply_mic(control, map.mic(), capture);
}

/// Turns the mic path on, off or changes it to match `mic`. A failure is
/// logged; the mic path is then off.
fn apply_mic(control: &mut impl AudioControl, mic: &MicConfig, capture: bool) {
    if let Err(err) = control.set_mic(mic_settings(mic, capture)) {
        tracing::error!("{err:#}; running without the mic path");
    }
}

/// The tray's "Mic to call" toggle.
///
/// Flips `[mic] enabled` on `board`, saves it to the config file at `path`
/// and applies the mic path. A save that fails (or no known config file) is
/// logged, and the change still applies until the app quits. `capture`
/// decides the capture flag from the flipped `[mic]` config. Returns whether
/// the mic is now enabled.
pub fn toggle_mic(
    control: &mut impl AudioControl,
    board: &mut Soundboard,
    path: Option<&Path>,
    capture: impl FnOnce(&MicConfig) -> bool,
) -> bool {
    let enabled = !board.map.mic.enabled;
    board.map.mic.enabled = enabled;
    match path {
        Some(path) => {
            if let Err(err) = config::set_mic_enabled(path, enabled) {
                tracing::warn!("{err:#}; the mic setting applies until the app quits");
            }
        }
        None => tracing::warn!(
            "the config file location is unknown; the mic setting applies until the app quits"
        ),
    }
    let mic = board.map().mic();
    let capture = capture(mic);
    apply_mic(control, mic, capture);
    tracing::info!(enabled, "virtual mic toggled");
    enabled
}

/// The text of the tray's "Mic to call" item.
pub fn mic_toggle_label(enabled: bool) -> &'static str {
    if enabled {
        "Mic to call: on"
    } else {
        "Mic to call: off"
    }
}

/// The text of the tray's mic status line. `not_found` is the platform's
/// note for a virtual device that no output matches (see
/// `platform::virtual_mic_not_found`). The capture state is appended only
/// when the microphone cannot be heard.
pub fn mic_status_label(status: &MicStatus, not_found: &str) -> String {
    match status {
        MicStatus::Off => "Virtual mic: off".to_string(),
        MicStatus::Starting => "Virtual mic: starting".to_string(),
        MicStatus::NotFound(_) => format!("Virtual mic: {not_found}"),
        MicStatus::Failed(_) => "Virtual mic: error, see log".to_string(),
        MicStatus::Active { device, capture } => match capture {
            CaptureStatus::Running(_) | CaptureStatus::Starting => {
                format!("Virtual mic: {device} (active)")
            }
            CaptureStatus::Disabled => {
                format!("Virtual mic: {device} (active, microphone access denied)")
            }
            CaptureStatus::Unavailable(_) => {
                format!("Virtual mic: {device} (active, microphone unavailable)")
            }
        },
    }
}

/// An [`AudioControl`] that keeps the virtual device the app creates itself
/// (on Linux) in step with the mic path.
///
/// Turning the mic path on creates the device first, if it does not exist
/// yet; turning it off (or a mic path that fails to start) drops the device
/// after the engine has stopped using it. A device that cannot be created is
/// logged, and the mic path starts anyway: its status then reports the
/// device as not found. `create` returns `Ok(None)` on the OSes where the
/// user installs the device.
pub struct WithVirtualMic<C, G, F> {
    control: C,
    device: Option<G>,
    create: F,
}

impl<C, G, F> WithVirtualMic<C, G, F>
where
    F: FnMut() -> Result<Option<G>>,
{
    pub fn new(control: C, create: F) -> Self {
        WithVirtualMic {
            control,
            device: None,
            create,
        }
    }

    /// The wrapped control.
    pub fn control(&self) -> &C {
        &self.control
    }

    /// Whether the virtual device currently exists.
    #[cfg(test)]
    fn has_device(&self) -> bool {
        self.device.is_some()
    }

    /// Hands the control to `stop` (the engine shuts down), then drops the
    /// virtual device, so no stream is left writing to it.
    pub fn shutdown(self, stop: impl FnOnce(C)) {
        let WithVirtualMic {
            control, device, ..
        } = self;
        stop(control);
        drop(device);
    }
}

impl<C, G, F> AudioControl for WithVirtualMic<C, G, F>
where
    C: AudioControl,
    F: FnMut() -> Result<Option<G>>,
{
    fn play(&self, sound: &Sound, gain: f32) -> bool {
        self.control.play(sound, gain)
    }

    fn set_master_gain(&self, gain: f32) -> bool {
        self.control.set_master_gain(gain)
    }

    fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
        let on = settings.is_some();
        if on && self.device.is_none() {
            match (self.create)() {
                Ok(device) => self.device = device,
                Err(err) => tracing::error!("{err:#}; the virtual mic is unavailable"),
            }
        }
        let result = self.control.set_mic(settings);
        if !on || result.is_err() {
            self.device = None;
        }
        result
    }
}

/// A decoded sound with its per-sound gain.
#[derive(Debug, Clone)]
struct LoadedSound {
    sound: Sound,
    gain: f32,
}

/// The decoded sounds of one config and its [`ActionMap`].
#[derive(Debug, Clone)]
pub struct Soundboard {
    map: ActionMap,
    sounds: Vec<LoadedSound>,
}

impl Soundboard {
    /// Decodes every sound of `config` synchronously. An entry that fails to
    /// decode is logged and dropped, so indices in the resulting map only
    /// refer to playable sounds.
    pub fn load(mut config: ResolvedConfig) -> Soundboard {
        let mut sounds = Vec::with_capacity(config.sounds.len());
        config.sounds.retain(|entry| match entry.source.load() {
            Ok(sound) => {
                tracing::debug!(name = %entry.name, frames = sound.frames(), "sound decoded");
                sounds.push(LoadedSound {
                    sound,
                    gain: entry.gain,
                });
                true
            }
            Err(err) => {
                tracing::warn!(name = %entry.name, "{err}; sound skipped");
                false
            }
        });
        Soundboard {
            map: ActionMap::new(&config),
            sounds,
        }
    }

    pub fn map(&self) -> &ActionMap {
        &self.map
    }

    /// Plays sound `index` at its gain. Unknown indices are ignored.
    pub fn play(&self, index: usize, control: &impl AudioControl) {
        let Some(entry) = self.sounds.get(index) else {
            tracing::warn!(index, "no sound at this index");
            return;
        };
        if !control.play(&entry.sound, entry.gain) {
            tracing::warn!(
                sound = entry.sound.label(),
                "no output stream or engine command queue is full; play dropped"
            );
        }
    }
}

/// Validates `raw` as a config living in `config_dir`, logs the warnings
/// and decodes the sounds.
fn board_from_raw(raw: &RawConfig, config_dir: &Path) -> Soundboard {
    let (resolved, warnings) = hotkeys::resolve(raw, config_dir);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    Soundboard::load(resolved)
}

/// Parses `text` and builds its board, see [`board_from_raw`].
fn board_from_text(text: &str, config_dir: &Path) -> Result<Soundboard> {
    Ok(board_from_raw(&config::parse(text)?, config_dir))
}

fn config_dir_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// Reads, validates and decodes the config at `path`.
pub fn load_board(path: &Path) -> Result<Soundboard> {
    let raw = config::load(path)?;
    Ok(board_from_raw(&raw, config_dir_of(path)))
}

/// The board of the built-in default config (the embedded demo only).
pub fn fallback_board() -> Result<Soundboard> {
    board_from_text(DEFAULT_CONFIG, Path::new(".")).context("built-in default config is invalid")
}

/// The board to start with. Falls back to the built-in default config if
/// the config file is unknown, unreadable or malformed, so the tray always
/// starts. A malformed file is logged at error level with its path; the
/// caller keeps the path, so the fixed file can be loaded with a reload.
pub fn startup_board(path: Option<&Path>) -> Result<Soundboard> {
    let Some(path) = path else {
        tracing::warn!("no config file; using the built-in default config");
        return fallback_board();
    };
    match fs::read_to_string(path) {
        Ok(text) => match board_from_text(&text, config_dir_of(path)) {
            Ok(board) => Ok(board),
            Err(err) => {
                tracing::error!(
                    path = %path.display(),
                    "failed to parse config: {err:#}; using the built-in default config \
                     until the file is fixed and reloaded"
                );
                fallback_board()
            }
        },
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                "cannot read config ({err}); using the built-in default config"
            );
            fallback_board()
        }
    }
}

/// Reloads the config at `path`.
///
/// On success `install` receives the new board (to swap hotkeys and the
/// menu), then [`apply_config`] sends the new master gain and mic settings to
/// `control`, and the board is returned for the caller to keep. `capture`
/// decides the capture flag from the new `[mic]` config. If loading or
/// `install` fails, `control` is not touched and the error is returned.
pub fn reload(
    control: &mut impl AudioControl,
    path: &Path,
    capture: impl FnOnce(&MicConfig) -> bool,
    install: impl FnOnce(&Soundboard) -> Result<()>,
) -> Result<Soundboard> {
    let board = load_board(path)?;
    install(&board)?;
    let capture = capture(board.map().mic());
    apply_config(control, board.map(), capture);
    Ok(board)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use global_hotkey::hotkey::{Code, Modifiers};

    use super::*;
    use crate::config::{ResolvedSound, SoundSource};
    use crate::test_support::TempDir;

    /// Records every call; `accept` decides the return value of the queue
    /// pushes and `fail_mic` makes `set_mic` fail.
    struct FakeAudio {
        plays: RefCell<Vec<(String, f32)>>,
        gains: RefCell<Vec<f32>>,
        mics: Vec<Option<MicSettings>>,
        accept: bool,
        fail_mic: bool,
    }

    impl FakeAudio {
        fn new() -> FakeAudio {
            FakeAudio {
                plays: RefCell::new(Vec::new()),
                gains: RefCell::new(Vec::new()),
                mics: Vec::new(),
                accept: true,
                fail_mic: false,
            }
        }
    }

    impl AudioControl for FakeAudio {
        fn play(&self, sound: &Sound, gain: f32) -> bool {
            self.plays
                .borrow_mut()
                .push((sound.label().to_string(), gain));
            self.accept
        }

        fn set_master_gain(&self, gain: f32) -> bool {
            self.gains.borrow_mut().push(gain);
            self.accept
        }

        fn set_mic(&mut self, settings: Option<MicSettings>) -> Result<()> {
            self.mics.push(settings);
            if self.fail_mic {
                anyhow::bail!("no thread for you");
            }
            Ok(())
        }
    }

    fn hk(code: Code) -> HotKey {
        HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), code)
    }

    fn entry(name: &str, hotkey: Option<HotKey>) -> ResolvedSound {
        ResolvedSound {
            name: name.into(),
            source: SoundSource::BuiltinDemo,
            hotkey,
            gain: 1.0,
        }
    }

    fn sample_config() -> ResolvedConfig {
        ResolvedConfig {
            master_gain: 0.75,
            stop_all_hotkey: Some(hk(Code::Digit0)),
            sounds: vec![
                entry("one", Some(hk(Code::Digit1))),
                // Hotkey skipped during validation: still in the menu.
                entry("no hotkey", None),
                entry("three", Some(hk(Code::Digit3))),
            ],
            mic: MicConfig::default(),
        }
    }

    fn mic_on(device: &str) -> MicConfig {
        MicConfig {
            enabled: true,
            virtual_device: device.into(),
            sound_gain: 0.5,
            mic_gain: 2.0,
        }
    }

    fn write_config(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn sounds_map_to_play_indices_in_order() {
        let map = ActionMap::new(&sample_config());
        let labels: Vec<_> = map.sound_items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["one", "no hotkey", "three"]);
        for (index, item) in map.sound_items().iter().enumerate() {
            assert_eq!(item.menu_id, sound_menu_id(index));
            assert_eq!(map.for_menu_id(&item.menu_id), Some(Action::Play(index)));
        }
    }

    #[test]
    fn hotkeys_map_to_actions() {
        let map = ActionMap::new(&sample_config());
        assert_eq!(map.for_hotkey(hk(Code::Digit1).id()), Some(Action::Play(0)));
        assert_eq!(map.for_hotkey(hk(Code::Digit3).id()), Some(Action::Play(2)));
        assert_eq!(map.for_hotkey(hk(Code::Digit0).id()), Some(Action::StopAll));
        assert_eq!(map.for_hotkey(hk(Code::Digit9).id()), None);
        assert_eq!(
            map.hotkeys(),
            [hk(Code::Digit1), hk(Code::Digit3), hk(Code::Digit0)]
        );
    }

    #[test]
    fn fixed_menu_items_map_to_actions() {
        let map = ActionMap::new(&sample_config());
        assert_eq!(map.for_menu_id(MENU_STOP_ALL), Some(Action::StopAll));
        assert_eq!(map.for_menu_id(MENU_RELOAD), Some(Action::Reload));
        assert_eq!(
            map.for_menu_id(MENU_OPEN_CONFIG),
            Some(Action::OpenConfigFolder)
        );
        assert_eq!(map.for_menu_id(MENU_QUIT), Some(Action::Quit));
        assert_eq!(map.for_menu_id("sound-99"), None);
        assert_eq!(map.for_menu_id("bogus"), None);
    }

    #[test]
    fn empty_config_has_only_fixed_items() {
        let map = ActionMap::new(&ResolvedConfig {
            master_gain: 1.0,
            stop_all_hotkey: None,
            sounds: Vec::new(),
            mic: MicConfig::default(),
        });
        assert!(map.sound_items().is_empty());
        assert!(map.hotkeys().is_empty());
        assert_eq!(map.for_menu_id(MENU_QUIT), Some(Action::Quit));
    }

    #[test]
    fn master_gain_is_carried_from_the_config() {
        assert_eq!(ActionMap::new(&sample_config()).master_gain(), 0.75);
    }

    #[test]
    fn master_gain_is_carried_after_clamping() {
        let raw = config::parse("master_gain = 9.0\n").unwrap();
        let (resolved, _) = hotkeys::resolve(&raw, Path::new("/cfg"));
        assert_eq!(ActionMap::new(&resolved).master_gain(), hotkeys::MAX_GAIN);
    }

    #[test]
    fn apply_config_sets_master_gain() {
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, &ActionMap::new(&sample_config()), true);
        assert_eq!(*fake.gains.borrow(), [0.75]);
        assert!(fake.plays.borrow().is_empty());
    }

    #[test]
    fn apply_config_survives_a_full_queue() {
        let mut fake = FakeAudio {
            accept: false,
            ..FakeAudio::new()
        };
        apply_config(&mut fake, &ActionMap::new(&sample_config()), true);
        assert_eq!(*fake.gains.borrow(), [0.75]);
    }

    #[test]
    fn startup_applies_the_config_master_gain() {
        let dir = TempDir::new("actions-startup");
        let path = write_config(&dir, "master_gain = 0.5\n");
        let board = startup_board(Some(&path)).unwrap();
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, board.map(), true);
        assert_eq!(*fake.gains.borrow(), [0.5]);
    }

    #[test]
    fn startup_falls_back_to_demo_when_unreadable() {
        let dir = TempDir::new("actions-unreadable");
        let missing = dir.path().join("missing.toml");
        for board in [
            startup_board(Some(&missing)).unwrap(),
            startup_board(None).unwrap(),
        ] {
            let labels: Vec<_> = board.map().sound_items().iter().map(|i| &i.label).collect();
            assert_eq!(labels, ["Demo chime"]);
            assert_eq!(board.map().master_gain(), 1.0);
        }
    }

    #[test]
    fn startup_falls_back_on_malformed_config() {
        let dir = TempDir::new("actions-malformed");
        let path = write_config(&dir, "master_gain = [\n");
        let board = startup_board(Some(&path)).unwrap();
        let labels: Vec<_> = board.map().sound_items().iter().map(|i| &i.label).collect();
        assert_eq!(labels, ["Demo chime"]);
        assert_eq!(board.map().master_gain(), 1.0);
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, board.map(), true);
        assert_eq!(*fake.gains.borrow(), [1.0]);
    }

    #[test]
    fn reload_after_a_malformed_startup_installs_the_fixed_file() {
        let dir = TempDir::new("actions-malformed-fixed");
        let path = write_config(&dir, "master_gain = [\n");
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, startup_board(Some(&path)).unwrap().map(), true);

        fs::write(
            &path,
            "master_gain = 0.5\n[[sound]]\nname = \"fixed\"\npath = \"builtin:demo\"\n",
        )
        .unwrap();
        let mut installed = Vec::new();
        let board = reload(
            &mut fake,
            &path,
            |_| true,
            |board| {
                installed.extend(board.map().sound_items().iter().map(|i| i.label.clone()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(installed, ["fixed"]);
        assert_eq!(board.map().master_gain(), 0.5);
        assert_eq!(*fake.gains.borrow(), [1.0, 0.5]);
    }

    #[test]
    fn successful_reload_installs_and_applies_the_new_gain() {
        let dir = TempDir::new("actions-reload");
        let path = write_config(&dir, "master_gain = 1.0\n");
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, startup_board(Some(&path)).unwrap().map(), true);

        fs::write(
            &path,
            "master_gain = 2.5\n[[sound]]\nname = \"demo\"\npath = \"builtin:demo\"\n",
        )
        .unwrap();
        let mut installed = Vec::new();
        let board = reload(
            &mut fake,
            &path,
            |_| true,
            |board| {
                installed.push(board.map().sound_items().len());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(installed, [1]);
        assert_eq!(board.map().master_gain(), 2.5);
        assert_eq!(*fake.gains.borrow(), [1.0, 2.5]);
    }

    #[test]
    fn failed_reload_leaves_the_engine_untouched() {
        let dir = TempDir::new("actions-reload-bad");
        let path = write_config(&dir, "master_gain = [\n");
        let mut fake = FakeAudio::new();
        let mut installed = false;
        let err = reload(
            &mut fake,
            &path,
            |_| true,
            |_| {
                installed = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("config"), "{err:#}");
        assert!(!installed);
        assert!(fake.gains.borrow().is_empty());
        assert!(fake.plays.borrow().is_empty());

        // A missing file is a failed reload too.
        fs::remove_file(&path).unwrap();
        assert!(
            reload(
                &mut fake,
                &path,
                |_| true,
                |_| {
                    installed = true;
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!installed);
        assert!(fake.gains.borrow().is_empty());
        assert!(fake.mics.is_empty());
    }

    #[test]
    fn failed_install_does_not_apply_the_config() {
        let dir = TempDir::new("actions-install-bad");
        let path = write_config(&dir, "master_gain = 0.25\n");
        let mut fake = FakeAudio::new();
        let err = reload(&mut fake, &path, |_| true, |_| anyhow::bail!("menu failed")).unwrap_err();
        assert!(err.to_string().contains("menu failed"));
        assert!(fake.gains.borrow().is_empty());
        assert!(fake.mics.is_empty());
    }

    #[test]
    fn undecodable_sounds_are_dropped_and_indices_stay_dense() {
        let dir = TempDir::new("actions-decode");
        fs::write(dir.path().join("junk.wav"), b"not audio").unwrap();
        let path = write_config(
            &dir,
            "[[sound]]\nname = \"a\"\npath = \"builtin:demo\"\n\
             [[sound]]\nname = \"junk\"\npath = \"junk.wav\"\nhotkey = \"Ctrl+Shift+2\"\n\
             [[sound]]\nname = \"c\"\npath = \"builtin:demo\"\ngain = 0.5\n",
        );
        let board = load_board(&path).unwrap();
        let labels: Vec<_> = board.map().sound_items().iter().map(|i| &i.label).collect();
        assert_eq!(labels, ["a", "c"]);
        // The dropped entry's hotkey is not registered.
        assert!(board.map().hotkeys().is_empty());

        let fake = FakeAudio::new();
        board.play(1, &fake);
        board.play(7, &fake);
        assert_eq!(
            *fake.plays.borrow(),
            [(crate::config::DEMO_LABEL.to_string(), 0.5)]
        );
    }

    #[test]
    fn play_survives_a_full_queue() {
        let board = fallback_board().unwrap();
        let fake = FakeAudio {
            accept: false,
            ..FakeAudio::new()
        };
        board.play(0, &fake);
        assert_eq!(fake.plays.borrow().len(), 1);
    }

    #[test]
    fn apply_config_turns_the_mic_off_when_disabled() {
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, &ActionMap::new(&sample_config()), true);
        assert_eq!(fake.mics, [None]);
    }

    #[test]
    fn mic_settings_follow_the_config_and_the_platform_exclusions() {
        assert_eq!(mic_settings(&MicConfig::default(), true), None);
        for capture in [true, false] {
            assert_eq!(
                mic_settings(&mic_on("My Cable"), capture),
                Some(MicSettings {
                    device_match: "My Cable".into(),
                    excluded_inputs: platform::virtual_input_exclusions("My Cable"),
                    sound_gain: 0.5,
                    mic_gain: 2.0,
                    capture,
                })
            );
        }
    }

    #[test]
    fn apply_config_sends_the_mic_settings() {
        let config = ResolvedConfig {
            mic: mic_on("My Cable"),
            ..sample_config()
        };
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, &ActionMap::new(&config), false);
        assert_eq!(*fake.gains.borrow(), [0.75]);
        assert_eq!(fake.mics, [mic_settings(&config.mic, false)]);
        let sent = fake.mics[0].as_ref().unwrap();
        assert!(!sent.capture);
        assert_eq!(sent.device_match, "My Cable");
    }

    #[test]
    fn apply_config_survives_a_mic_that_fails_to_start() {
        let config = ResolvedConfig {
            mic: mic_on("My Cable"),
            ..sample_config()
        };
        let mut fake = FakeAudio {
            fail_mic: true,
            ..FakeAudio::new()
        };
        apply_config(&mut fake, &ActionMap::new(&config), true);
        assert_eq!(*fake.gains.borrow(), [0.75]);
        assert_eq!(fake.mics.len(), 1);
    }

    #[test]
    fn reload_applies_the_mic_table() {
        let dir = TempDir::new("actions-reload-mic");
        let path = write_config(&dir, "master_gain = 1.0\n");
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, startup_board(Some(&path)).unwrap().map(), true);

        fs::write(
            &path,
            "[mic]\nenabled = true\nvirtual_device = \"Cable\"\nmic_gain = 0.25\n",
        )
        .unwrap();
        let board = reload(&mut fake, &path, |_| true, |_| Ok(())).unwrap();
        assert!(board.map().mic().enabled);
        assert_eq!(fake.mics.len(), 2);
        assert_eq!(fake.mics[0], None);
        let sent = fake.mics[1].as_ref().unwrap();
        assert_eq!(sent.device_match, "Cable");
        assert_eq!(sent.mic_gain, 0.25);
        assert_eq!(sent.sound_gain, 1.0);
        assert!(sent.capture);

        // Turning it off again in the file turns the mic path off.
        fs::write(&path, "[mic]\nenabled = false\n").unwrap();
        reload(&mut fake, &path, |_| true, |_| Ok(())).unwrap();
        assert_eq!(fake.mics.last(), Some(&None));
    }

    #[test]
    fn only_a_denied_microphone_turns_the_capture_off() {
        let mic = mic_on("Cable");
        for access in [
            MicrophoneAccess::Granted,
            MicrophoneAccess::Undetermined,
            MicrophoneAccess::Unknown,
        ] {
            assert!(CaptureGate::default().capture(access, &mic), "{access:?}");
        }
        assert!(!CaptureGate::default().capture(MicrophoneAccess::Denied, &mic));
    }

    #[test]
    fn a_denial_is_logged_once_per_change_and_only_with_the_mic_on() {
        let mut gate = CaptureGate::default();
        let off = MicConfig::default();
        assert!(!gate.capture(MicrophoneAccess::Denied, &off));
        assert!(!gate.denial_logged, "no warning while the mic is off");

        let on = mic_on("Cable");
        assert!(!gate.capture(MicrophoneAccess::Denied, &on));
        assert!(gate.denial_logged);
        assert!(!gate.capture(MicrophoneAccess::Denied, &on));
        assert!(gate.denial_logged, "still denied: no second warning");

        assert!(gate.capture(MicrophoneAccess::Granted, &on));
        assert!(!gate.denial_logged, "a grant re-arms the warning");
    }

    #[test]
    fn recheck_reapplies_the_mic_only_when_the_capture_flag_changes() {
        let mut fake = FakeAudio::new();
        let mut gate = CaptureGate::default();
        let on = mic_on("Cable");
        // Startup: the permission is not asked yet, so the capture opens.
        assert!(gate.capture(MicrophoneAccess::Undetermined, &on));

        // Still allowed (the prompt was answered "allow"): nothing to do.
        assert!(!recheck_capture(
            &mut fake,
            &on,
            &mut gate,
            MicrophoneAccess::Granted
        ));
        assert!(fake.mics.is_empty());

        // Denied at the prompt: the mic path restarts without the capture.
        assert!(recheck_capture(
            &mut fake,
            &on,
            &mut gate,
            MicrophoneAccess::Denied
        ));
        assert_eq!(fake.mics, [mic_settings(&on, false)]);
        assert!(gate.denial_logged);
        assert!(!recheck_capture(
            &mut fake,
            &on,
            &mut gate,
            MicrophoneAccess::Denied
        ));
        assert_eq!(fake.mics.len(), 1);

        // Allowed later in System Settings: the capture comes back.
        assert!(recheck_capture(
            &mut fake,
            &on,
            &mut gate,
            MicrophoneAccess::Granted
        ));
        assert_eq!(fake.mics[1], mic_settings(&on, true));
    }

    #[test]
    fn recheck_does_nothing_with_the_mic_off() {
        let mut fake = FakeAudio::new();
        let mut gate = CaptureGate::default();
        let off = MicConfig::default();
        assert!(gate.capture(MicrophoneAccess::Granted, &off));
        assert!(!recheck_capture(
            &mut fake,
            &off,
            &mut gate,
            MicrophoneAccess::Denied
        ));
        assert!(fake.mics.is_empty());
        assert!(!gate.denial_logged);
    }

    #[test]
    fn reload_decides_the_capture_from_the_new_mic_config() {
        let dir = TempDir::new("actions-reload-capture");
        let path = write_config(&dir, "[mic]\nenabled = true\nvirtual_device = \"Cable\"\n");
        let mut fake = FakeAudio::new();
        let mut seen = None;
        reload(
            &mut fake,
            &path,
            |mic| {
                seen = Some(mic.clone());
                false
            },
            |_| Ok(()),
        )
        .unwrap();
        let seen = seen.unwrap();
        assert!(seen.enabled);
        assert_eq!(seen.virtual_device, "Cable");
        assert!(!fake.mics[0].as_ref().unwrap().capture);
    }

    #[test]
    fn the_mic_toggle_maps_to_its_action() {
        let map = ActionMap::new(&sample_config());
        assert_eq!(map.for_menu_id(MENU_TOGGLE_MIC), Some(Action::ToggleMic));
    }

    #[test]
    fn mic_toggle_label_names_the_state() {
        assert_eq!(mic_toggle_label(true), "Mic to call: on");
        assert_eq!(mic_toggle_label(false), "Mic to call: off");
    }

    #[test]
    fn mic_status_labels_cover_every_status() {
        let hint = "not found, install BlackHole";
        let active = |capture| MicStatus::Active {
            device: "BlackHole 2ch".into(),
            capture,
        };
        let cases = [
            (MicStatus::Off, "Virtual mic: off"),
            (MicStatus::Starting, "Virtual mic: starting"),
            (
                MicStatus::NotFound("BlackHole".into()),
                "Virtual mic: not found, install BlackHole",
            ),
            (
                MicStatus::Failed("device busy".into()),
                "Virtual mic: error, see log",
            ),
            (
                active(CaptureStatus::Running("MacBook Mic".into())),
                "Virtual mic: BlackHole 2ch (active)",
            ),
            (
                active(CaptureStatus::Starting),
                "Virtual mic: BlackHole 2ch (active)",
            ),
            (
                active(CaptureStatus::Disabled),
                "Virtual mic: BlackHole 2ch (active, microphone access denied)",
            ),
            (
                active(CaptureStatus::Unavailable("no input".into())),
                "Virtual mic: BlackHole 2ch (active, microphone unavailable)",
            ),
        ];
        for (status, label) in cases {
            assert_eq!(mic_status_label(&status, hint), label, "{status:?}");
        }
        // The platform note is used verbatim, so Linux reads naturally.
        assert_eq!(
            mic_status_label(
                &MicStatus::NotFound("sound_babound_sink".into()),
                "could not create it, see log"
            ),
            "Virtual mic: could not create it, see log"
        );
    }

    const MIC_CONFIG: &str = "# my comment\nmaster_gain = 0.5\n\n\
                              [mic]\nenabled = false # keep me\nvirtual_device = \"Cable\"\n";

    #[test]
    fn toggle_mic_persists_and_applies() {
        let dir = TempDir::new("actions-toggle");
        let path = write_config(&dir, MIC_CONFIG);
        let mut board = load_board(&path).unwrap();
        let mut fake = FakeAudio::new();

        assert!(toggle_mic(&mut fake, &mut board, Some(&path), |_| true));
        assert!(board.map().mic().enabled);
        assert_eq!(fake.mics, [mic_settings(board.map().mic(), true)]);
        assert_eq!(fake.mics[0].as_ref().unwrap().device_match, "Cable");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("enabled = true # keep me"), "{text}");
        assert!(text.contains("# my comment"), "{text}");
        assert!(load_board(&path).unwrap().map().mic().enabled);

        assert!(!toggle_mic(&mut fake, &mut board, Some(&path), |_| true));
        assert!(!board.map().mic().enabled);
        assert_eq!(fake.mics.last(), Some(&None));
        assert!(!load_board(&path).unwrap().map().mic().enabled);
        // Only the mic changed: no gain or play was sent.
        assert!(fake.gains.borrow().is_empty());
        assert!(fake.plays.borrow().is_empty());
    }

    #[test]
    fn toggle_mic_applies_even_when_saving_fails() {
        let dir = TempDir::new("actions-toggle-bad");
        // Startup fell back to the built-in config: the file is malformed.
        let path = write_config(&dir, "master_gain = [\n");
        let mut board = startup_board(Some(&path)).unwrap();
        let mut fake = FakeAudio::new();

        assert!(toggle_mic(&mut fake, &mut board, Some(&path), |_| true));
        assert!(board.map().mic().enabled);
        assert_eq!(fake.mics.len(), 1);
        assert!(fake.mics[0].is_some());
        assert_eq!(fs::read_to_string(&path).unwrap(), "master_gain = [\n");

        // No config file at all: applied too.
        let mut board = fallback_board().unwrap();
        assert!(toggle_mic(&mut fake, &mut board, None, |_| true));
        assert_eq!(fake.mics.len(), 2);
        assert!(fake.mics[1].is_some());
    }

    #[test]
    fn toggle_mic_decides_the_capture_from_the_flipped_config() {
        let dir = TempDir::new("actions-toggle-capture");
        let path = write_config(&dir, MIC_CONFIG);
        let mut board = load_board(&path).unwrap();
        let mut fake = FakeAudio::new();
        let mut seen = None;
        toggle_mic(&mut fake, &mut board, Some(&path), |mic| {
            seen = Some(mic.enabled);
            false
        });
        assert_eq!(seen, Some(true));
        assert!(!fake.mics[0].as_ref().unwrap().capture);
    }

    #[test]
    fn reload_with_a_changed_mic_table_keeps_hotkeys_and_sounds() {
        let dir = TempDir::new("actions-reload-mic-only");
        let sounds = "stop_all_hotkey = \"Ctrl+Shift+0\"\n\
                      [[sound]]\nname = \"demo\"\npath = \"builtin:demo\"\nhotkey = \"Ctrl+Shift+1\"\n";
        let path = write_config(
            &dir,
            &format!("{sounds}[mic]\nenabled = true\nmic_gain = 1.0\n"),
        );
        let before = load_board(&path).unwrap();
        let mut fake = FakeAudio::new();
        apply_config(&mut fake, before.map(), true);

        fs::write(
            &path,
            format!("{sounds}[mic]\nenabled = true\nmic_gain = 0.5\n"),
        )
        .unwrap();
        let mut installed = None;
        let after = reload(
            &mut fake,
            &path,
            |_| true,
            |board| {
                installed = Some((
                    board.map().hotkeys().to_vec(),
                    board.map().sound_items().to_vec(),
                ));
                Ok(())
            },
        )
        .unwrap();
        let (hotkeys, sounds) = installed.unwrap();
        assert_eq!(hotkeys, before.map().hotkeys());
        assert_eq!(sounds, before.map().sound_items());
        assert_eq!(fake.mics.len(), 2);
        assert_eq!(fake.mics[0].as_ref().unwrap().mic_gain, 1.0);
        assert_eq!(fake.mics[1].as_ref().unwrap().mic_gain, 0.5);
        assert_eq!(fake.mics[1], mic_settings(after.map().mic(), true));
        // The master gain is re-sent unchanged.
        assert_eq!(*fake.gains.borrow(), [1.0, 1.0]);
    }

    type Log = std::rc::Rc<RefCell<Vec<&'static str>>>;

    /// Logs `set_mic` calls into a shared log, so their order against the
    /// virtual device's creation and drop can be checked.
    struct LoggingAudio {
        log: Log,
        fail_mic: bool,
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
            if self.fail_mic {
                anyhow::bail!("no thread for you");
            }
            Ok(())
        }
    }

    /// Logs its drop.
    struct FakeDevice(Log);

    impl Drop for FakeDevice {
        fn drop(&mut self) {
            self.0.borrow_mut().push("device dropped");
        }
    }

    fn with_device(
        log: &Log,
        fail_mic: bool,
    ) -> WithVirtualMic<LoggingAudio, FakeDevice, impl FnMut() -> Result<Option<FakeDevice>>> {
        let device_log = log.clone();
        WithVirtualMic::new(
            LoggingAudio {
                log: log.clone(),
                fail_mic,
            },
            move || {
                device_log.borrow_mut().push("device created");
                Ok(Some(FakeDevice(device_log.clone())))
            },
        )
    }

    fn on() -> Option<MicSettings> {
        mic_settings(&mic_on("Cable"), true)
    }

    #[test]
    fn the_virtual_device_is_created_before_the_mic_starts_and_dropped_after_it_stops() {
        let log = Log::default();
        let mut audio = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        // A gains-only change keeps the existing device.
        audio.set_mic(on()).unwrap();
        assert!(audio.has_device());
        audio.set_mic(None).unwrap();
        assert!(!audio.has_device());
        // Off again: nothing to create or drop.
        audio.set_mic(None).unwrap();
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
    }

    #[test]
    fn a_mic_path_that_fails_to_start_drops_the_device() {
        let log = Log::default();
        let mut audio = with_device(&log, true);
        assert!(audio.set_mic(on()).is_err());
        assert!(!audio.has_device());
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "device dropped"]
        );
    }

    #[test]
    fn a_device_that_cannot_be_created_still_starts_the_mic_path_and_is_retried() {
        let log = Log::default();
        let attempts = std::rc::Rc::new(RefCell::new(0));
        let counter = attempts.clone();
        let mut audio = WithVirtualMic::new(
            LoggingAudio {
                log: log.clone(),
                fail_mic: false,
            },
            move || -> Result<Option<FakeDevice>> {
                *counter.borrow_mut() += 1;
                anyhow::bail!("pactl not found")
            },
        );
        audio.set_mic(on()).unwrap();
        audio.set_mic(on()).unwrap();
        assert!(!audio.has_device());
        assert_eq!(*attempts.borrow(), 2);
        assert_eq!(*log.borrow(), ["mic on", "mic on"]);
    }

    #[test]
    fn no_device_is_made_where_the_user_installs_it() {
        let log = Log::default();
        let mut audio = WithVirtualMic::new(
            LoggingAudio {
                log: log.clone(),
                fail_mic: false,
            },
            || -> Result<Option<FakeDevice>> { Ok(None) },
        );
        audio.set_mic(on()).unwrap();
        assert!(!audio.has_device());
        assert_eq!(*log.borrow(), ["mic on"]);
    }

    #[test]
    fn startup_with_the_mic_enabled_creates_the_device_before_applying() {
        let log = Log::default();
        let mut audio = with_device(&log, false);
        let config = ResolvedConfig {
            mic: mic_on("Cable"),
            ..sample_config()
        };
        apply_config(&mut audio, &ActionMap::new(&config), true);
        assert_eq!(*log.borrow(), ["gain", "device created", "mic on"]);
    }

    #[test]
    fn toggling_through_the_wrapper_creates_and_drops_the_device() {
        let dir = TempDir::new("actions-toggle-device");
        let path = write_config(&dir, MIC_CONFIG);
        let mut board = load_board(&path).unwrap();
        let log = Log::default();
        let mut audio = with_device(&log, false);
        toggle_mic(&mut audio, &mut board, Some(&path), |_| true);
        assert!(audio.has_device());
        toggle_mic(&mut audio, &mut board, Some(&path), |_| true);
        assert!(!audio.has_device());
        assert_eq!(
            *log.borrow(),
            ["device created", "mic on", "mic off", "device dropped"]
        );
    }

    #[test]
    fn shutdown_stops_the_engine_before_dropping_the_device() {
        let log = Log::default();
        let mut audio = with_device(&log, false);
        audio.set_mic(on()).unwrap();
        let stop_log = log.clone();
        audio.shutdown(move |control| {
            stop_log.borrow_mut().push("engine shut down");
            drop(control);
        });
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
}
