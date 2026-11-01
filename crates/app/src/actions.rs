//! What the tray menu and the global hotkeys do.
//!
//! This module is UI-free: it maps menu item ids and hotkey ids to
//! [`Action`]s, owns the decoded sounds, and applies a config to anything
//! implementing [`AudioControl`]. The tray only wires events to it.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use babound_engine::{Engine, Sound};
use global_hotkey::hotkey::HotKey;

use crate::config::{self, DEFAULT_CONFIG, RawConfig, ResolvedConfig};
use crate::hotkeys;

/// Something the tray can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Play the sound at this index of the [`ActionMap`]'s sound list.
    Play(usize),
    StopAll,
    Reload,
    OpenConfigFolder,
    Quit,
}

/// Menu id of the "Stop all" item.
pub const MENU_STOP_ALL: &str = "stop-all";
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
}

/// The audio controls the tray needs from the engine.
pub trait AudioControl {
    /// Queues a play. Returns false if the engine rejected it (no output
    /// stream, or a full command queue).
    fn play(&self, sound: &Sound, gain: f32) -> bool;
    /// Queues a master gain change. Returns false if the engine rejected it.
    fn set_master_gain(&self, gain: f32) -> bool;
}

impl AudioControl for Engine {
    fn play(&self, sound: &Sound, gain: f32) -> bool {
        Engine::play(self, sound, gain)
    }

    fn set_master_gain(&self, gain: f32) -> bool {
        Engine::set_master_gain(self, gain)
    }
}

/// Applies the engine-wide parts of a config: the master gain. Called at
/// startup and after every successful reload.
pub fn apply_config(control: &impl AudioControl, map: &ActionMap) {
    if !control.set_master_gain(map.master_gain()) {
        tracing::warn!(
            master_gain = map.master_gain(),
            "engine command queue is full; master gain not applied"
        );
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

/// The board to start with. Falls back to the built-in default config only
/// if the config file is unknown or unreadable; a malformed config file is
/// an error.
pub fn startup_board(path: Option<&Path>) -> Result<Soundboard> {
    let Some(path) = path else {
        tracing::warn!("no config file; using the built-in default config");
        return fallback_board();
    };
    match fs::read_to_string(path) {
        Ok(text) => board_from_text(&text, config_dir_of(path))
            .with_context(|| format!("failed to parse config {}", path.display())),
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
/// menu), then [`apply_config`] sends the new master gain to `control`, and
/// the board is returned for the caller to keep. If loading or `install`
/// fails, `control` is not touched and the error is returned.
pub fn reload(
    control: &impl AudioControl,
    path: &Path,
    install: impl FnOnce(&Soundboard) -> Result<()>,
) -> Result<Soundboard> {
    let board = load_board(path)?;
    install(&board)?;
    apply_config(control, board.map());
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

    /// Records every call; `accept` decides the return value.
    struct FakeAudio {
        plays: RefCell<Vec<(String, f32)>>,
        gains: RefCell<Vec<f32>>,
        accept: bool,
    }

    impl FakeAudio {
        fn new() -> FakeAudio {
            FakeAudio {
                plays: RefCell::new(Vec::new()),
                gains: RefCell::new(Vec::new()),
                accept: true,
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
        let fake = FakeAudio::new();
        apply_config(&fake, &ActionMap::new(&sample_config()));
        assert_eq!(*fake.gains.borrow(), [0.75]);
        assert!(fake.plays.borrow().is_empty());
    }

    #[test]
    fn apply_config_survives_a_full_queue() {
        let fake = FakeAudio {
            accept: false,
            ..FakeAudio::new()
        };
        apply_config(&fake, &ActionMap::new(&sample_config()));
        assert_eq!(*fake.gains.borrow(), [0.75]);
    }

    #[test]
    fn startup_applies_the_config_master_gain() {
        let dir = TempDir::new("actions-startup");
        let path = write_config(&dir, "master_gain = 0.5\n");
        let board = startup_board(Some(&path)).unwrap();
        let fake = FakeAudio::new();
        apply_config(&fake, board.map());
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
    fn startup_rejects_a_malformed_config() {
        let dir = TempDir::new("actions-malformed");
        let path = write_config(&dir, "master_gain = [\n");
        let message = format!("{:#}", startup_board(Some(&path)).unwrap_err());
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn successful_reload_installs_and_applies_the_new_gain() {
        let dir = TempDir::new("actions-reload");
        let path = write_config(&dir, "master_gain = 1.0\n");
        let fake = FakeAudio::new();
        apply_config(&fake, startup_board(Some(&path)).unwrap().map());

        fs::write(
            &path,
            "master_gain = 2.5\n[[sound]]\nname = \"demo\"\npath = \"builtin:demo\"\n",
        )
        .unwrap();
        let mut installed = Vec::new();
        let board = reload(&fake, &path, |board| {
            installed.push(board.map().sound_items().len());
            Ok(())
        })
        .unwrap();
        assert_eq!(installed, [1]);
        assert_eq!(board.map().master_gain(), 2.5);
        assert_eq!(*fake.gains.borrow(), [1.0, 2.5]);
    }

    #[test]
    fn failed_reload_leaves_the_engine_untouched() {
        let dir = TempDir::new("actions-reload-bad");
        let path = write_config(&dir, "master_gain = [\n");
        let fake = FakeAudio::new();
        let mut installed = false;
        let err = reload(&fake, &path, |_| {
            installed = true;
            Ok(())
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("config"), "{err:#}");
        assert!(!installed);
        assert!(fake.gains.borrow().is_empty());
        assert!(fake.plays.borrow().is_empty());

        // A missing file is a failed reload too.
        fs::remove_file(&path).unwrap();
        assert!(
            reload(&fake, &path, |_| {
                installed = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!installed);
        assert!(fake.gains.borrow().is_empty());
    }

    #[test]
    fn failed_install_does_not_apply_the_config() {
        let dir = TempDir::new("actions-install-bad");
        let path = write_config(&dir, "master_gain = 0.25\n");
        let fake = FakeAudio::new();
        let err = reload(&fake, &path, |_| anyhow::bail!("menu failed")).unwrap_err();
        assert!(err.to_string().contains("menu failed"));
        assert!(fake.gains.borrow().is_empty());
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
}
