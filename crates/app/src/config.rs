//! TOML configuration: raw serde model, default template, path resolution and
//! the validated [`ResolvedConfig`] produced by [`crate::hotkeys::resolve`].

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use babound_engine::{LoadError, Sound};
use global_hotkey::hotkey::HotKey;
use serde::Deserialize;

/// Path value that selects the embedded demo chime.
pub const BUILTIN_DEMO: &str = "builtin:demo";

/// Prefix shared by all built-in sound paths.
const BUILTIN_PREFIX: &str = "builtin:";

/// The embedded demo chime (44.1 kHz mono WAV).
pub const DEMO_WAV: &[u8] = include_bytes!("../assets/demo.wav");

/// Label used for the embedded demo when it is decoded.
pub const DEMO_LABEL: &str = "builtin:demo";

/// Commented config template written on first start.
pub const DEFAULT_CONFIG: &str = r#"# Sound-Babound configuration.
#
# Edit this file, then choose "Reload config" in the tray menu.
#
# Hotkey syntax: modifiers and one key joined with "+", for example
#   "CmdOrCtrl+Shift+1", "Ctrl+Alt+F5", "Cmd+Shift+A", "Alt+Space".
# Modifiers: Ctrl (Control), Alt (Option), Shift, Cmd (Command, Super, Meta)
#   and CmdOrCtrl, which is Cmd on macOS and Ctrl elsewhere.
# Keys: A-Z, 0-9, F1-F24, Space, Enter, Tab, Escape, Backspace, Delete,
#   Insert, Home, End, PageUp, PageDown, Up, Down, Left, Right,
#   Num0-Num9 (numpad), and punctuation such as Minus, Equal, Comma, Period,
#   Slash, Semicolon, Quote, Backquote, BracketLeft, BracketRight, Backslash.
# Names are case-insensitive. An invalid or duplicate hotkey is skipped with
# a warning in the log; the sound stays playable from the tray menu.

# Overall volume applied to every sound, from 0.0 (silent) to 4.0.
# Default: 1.0.
master_gain = 1.0

# Hotkey that stops every sound that is playing. Optional.
stop_all_hotkey = "CmdOrCtrl+Shift+0"

# One [[sound]] block per sound.
#   name   - label shown in the tray menu (required)
#   path   - audio file (WAV, FLAC, OGG Vorbis or MP3), required. Absolute
#            paths are used as-is; relative paths are resolved against the
#            folder this file lives in. "builtin:demo" is the embedded chime.
#   hotkey - global hotkey that plays the sound (optional)
#   gain   - per-sound volume from 0.0 to 4.0 (optional, default 1.0)
[[sound]]
name = "Demo chime"
path = "builtin:demo"
hotkey = "CmdOrCtrl+Shift+1"
gain = 1.0

# [[sound]]
# name = "Airhorn"
# path = "sounds/airhorn.mp3"
# hotkey = "CmdOrCtrl+Shift+2"
# gain = 0.8
"#;

fn default_gain() -> f32 {
    1.0
}

/// Config file as written by the user, before validation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawConfig {
    #[serde(default = "default_gain")]
    pub master_gain: f32,
    #[serde(default)]
    pub stop_all_hotkey: Option<String>,
    #[serde(default, rename = "sound")]
    pub sounds: Vec<RawSound>,
}

impl Default for RawConfig {
    fn default() -> Self {
        RawConfig {
            master_gain: default_gain(),
            stop_all_hotkey: None,
            sounds: Vec::new(),
        }
    }
}

/// One `[[sound]]` entry as written by the user.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawSound {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub hotkey: Option<String>,
    #[serde(default = "default_gain")]
    pub gain: f32,
}

/// Parses config text. The error message names the offending line and
/// column.
pub fn parse(text: &str) -> Result<RawConfig> {
    toml::from_str(text).map_err(|err| anyhow::anyhow!("invalid config: {err}"))
}

/// Reads and parses the config file at `path`.
pub fn load(path: &Path) -> Result<RawConfig> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    parse(&text).with_context(|| format!("failed to parse config {}", path.display()))
}

/// Where a sound's audio comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSource {
    /// The embedded demo chime.
    BuiltinDemo,
    /// A file on disk (absolute after resolution).
    File(PathBuf),
}

impl SoundSource {
    /// Resolves a config `path` value. `builtin:demo` maps to the embedded
    /// asset, absolute paths are used as-is and relative paths are joined to
    /// `config_dir`.
    pub fn resolve(raw: &str, config_dir: &Path) -> Result<SoundSource, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("path is empty".to_string());
        }
        if raw.eq_ignore_ascii_case(BUILTIN_DEMO) {
            return Ok(SoundSource::BuiltinDemo);
        }
        if raw.starts_with(BUILTIN_PREFIX) {
            return Err(format!(
                "unknown built-in sound {raw:?} (only {BUILTIN_DEMO:?} exists)"
            ));
        }
        let path = Path::new(raw);
        if path.is_absolute() {
            Ok(SoundSource::File(path.to_path_buf()))
        } else {
            Ok(SoundSource::File(config_dir.join(path)))
        }
    }

    /// Decodes the sound.
    pub fn load(&self) -> Result<Sound, LoadError> {
        match self {
            SoundSource::BuiltinDemo => Sound::load_bytes(DEMO_WAV, DEMO_LABEL),
            SoundSource::File(path) => Sound::load_file(path),
        }
    }
}

/// A validated sound entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSound {
    pub name: String,
    pub source: SoundSource,
    /// `None` when the entry has no hotkey or its hotkey was skipped.
    pub hotkey: Option<HotKey>,
    /// Clamped to [`crate::hotkeys::MIN_GAIN`, `crate::hotkeys::MAX_GAIN`].
    pub gain: f32,
}

/// A validated config, ready for the tray and the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    /// Clamped to [`crate::hotkeys::MIN_GAIN`, `crate::hotkeys::MAX_GAIN`].
    pub master_gain: f32,
    pub stop_all_hotkey: Option<HotKey>,
    pub sounds: Vec<ResolvedSound>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_template_parses_and_references_builtin_demo() {
        let raw = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(raw.master_gain, 1.0);
        assert_eq!(raw.stop_all_hotkey.as_deref(), Some("CmdOrCtrl+Shift+0"));
        assert_eq!(raw.sounds.len(), 1);
        let demo = &raw.sounds[0];
        assert_eq!(demo.path, BUILTIN_DEMO);
        assert_eq!(demo.hotkey.as_deref(), Some("CmdOrCtrl+Shift+1"));
        assert_eq!(demo.gain, 1.0);
    }

    #[test]
    fn missing_optional_fields_take_defaults() {
        let raw = parse("[[sound]]\nname = \"a\"\npath = \"a.wav\"\n").unwrap();
        assert_eq!(raw.master_gain, 1.0);
        assert_eq!(raw.stop_all_hotkey, None);
        assert_eq!(
            raw.sounds,
            vec![RawSound {
                name: "a".into(),
                path: "a.wav".into(),
                hotkey: None,
                gain: 1.0,
            }]
        );
    }

    #[test]
    fn empty_file_is_a_valid_empty_config() {
        assert_eq!(parse("").unwrap(), RawConfig::default());
    }

    #[test]
    fn malformed_toml_gives_readable_error() {
        let err = parse("master_gain = \n[[sound]\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.starts_with("invalid config"), "{msg}");
        assert!(msg.contains("line"), "error should point at a line: {msg}");
    }

    #[test]
    fn wrong_type_gives_readable_error() {
        let err = parse("master_gain = \"loud\"\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("master_gain"), "{msg}");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        let err = parse("[[sound]]\nname = \"a\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("path"));
    }

    #[test]
    fn load_missing_file_names_the_path() {
        let err = load(Path::new("/definitely/not/here/config.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("/definitely/not/here/config.toml"));
    }

    #[test]
    fn resolve_builtin_demo() {
        let dir = Path::new("/cfg");
        assert_eq!(
            SoundSource::resolve("builtin:demo", dir),
            Ok(SoundSource::BuiltinDemo)
        );
        assert_eq!(
            SoundSource::resolve(" BUILTIN:Demo ", dir),
            Ok(SoundSource::BuiltinDemo)
        );
    }

    #[test]
    fn resolve_absolute_path_as_is() {
        let abs = std::env::temp_dir().join("x.wav");
        assert!(abs.is_absolute());
        assert_eq!(
            SoundSource::resolve(abs.to_str().unwrap(), Path::new("/cfg")),
            Ok(SoundSource::File(abs))
        );
    }

    #[test]
    fn resolve_relative_path_joins_config_dir() {
        let dir = std::env::temp_dir().join("cfg");
        assert_eq!(
            SoundSource::resolve("sounds/a.wav", &dir),
            Ok(SoundSource::File(dir.join("sounds/a.wav")))
        );
    }

    #[test]
    fn resolve_rejects_empty_and_unknown_builtin() {
        let dir = Path::new("/cfg");
        assert!(SoundSource::resolve("  ", dir).is_err());
        let err = SoundSource::resolve("builtin:airhorn", dir).unwrap_err();
        assert!(err.contains("builtin:airhorn"));
    }

    #[test]
    fn builtin_demo_decodes() {
        let sound = SoundSource::BuiltinDemo.load().unwrap();
        assert_eq!(sound.sample_rate(), 44_100);
        assert_eq!(sound.channels(), 1);
        assert!(sound.frames() > 0);
    }

    #[test]
    fn missing_file_source_fails_to_load() {
        let src = SoundSource::File(PathBuf::from("/definitely/not/here.wav"));
        assert!(matches!(src.load(), Err(LoadError::Io { .. })));
    }
}
