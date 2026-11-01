//! TOML configuration: raw serde model, default template, path resolution,
//! the validated [`ResolvedConfig`] produced by [`crate::hotkeys::resolve`],
//! and the comment-preserving [`update_config`] editor with its
//! [`set_mic_enabled`] writer.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use babound_engine::{LoadError, Sound};
use global_hotkey::hotkey::HotKey;
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Value};

use crate::platform;

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

# Mic to call: send the sounds, mixed with your microphone, into a voice or
# video call through a virtual audio cable. Off by default. The tray item
# "Mic to call" turns it on and off and saves the choice here.
#   enabled        - true or false (default false)
#   virtual_device - the cable's playback device: the first output whose
#                    name or id contains this text, ignoring case. The default is
#                    "BlackHole" on macOS (preferring "BlackHole 2ch" when
#                    several BlackHole devices are installed), "CABLE Input"
#                    (VB-CABLE) on Windows and "sound_babound_sink" on Linux,
#                    where the app creates the device itself.
#   sound_gain     - volume of the sounds in the call, 0.0 to 4.0 (default 1.0)
#   mic_gain       - volume of your microphone in the call, 0.0 to 4.0
#                    (default 1.0)
[mic]
enabled = false
# virtual_device = "BlackHole"
sound_gain = 1.0
mic_gain = 1.0

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
    #[serde(default)]
    pub mic: RawMic,
}

impl Default for RawConfig {
    fn default() -> Self {
        RawConfig {
            master_gain: default_gain(),
            stop_all_hotkey: None,
            sounds: Vec::new(),
            mic: RawMic::default(),
        }
    }
}

/// The `[mic]` table as written by the user.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RawMic {
    #[serde(default)]
    pub enabled: bool,
    /// `None` selects [`platform::default_virtual_device`].
    #[serde(default)]
    pub virtual_device: Option<String>,
    #[serde(default = "default_gain")]
    pub sound_gain: f32,
    #[serde(default = "default_gain")]
    pub mic_gain: f32,
}

impl Default for RawMic {
    fn default() -> Self {
        RawMic {
            enabled: false,
            virtual_device: None,
            sound_gain: default_gain(),
            mic_gain: default_gain(),
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
        if raw
            .get(..BUILTIN_PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(BUILTIN_PREFIX))
        {
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
    pub mic: MicConfig,
}

/// The validated `[mic]` table.
#[derive(Debug, Clone, PartialEq)]
pub struct MicConfig {
    pub enabled: bool,
    /// The mic sink's device pattern, trimmed and never blank.
    pub virtual_device: String,
    /// Which of the devices `virtual_device` names to prefer: the
    /// platform's preference when `virtual_device` is the platform default
    /// (missing or blank in the file), `None` for a pattern the user set,
    /// which keeps first-match semantics.
    pub device_prefer: Option<String>,
    /// Clamped to [`crate::hotkeys::MIN_GAIN`, `crate::hotkeys::MAX_GAIN`].
    pub sound_gain: f32,
    /// Clamped to [`crate::hotkeys::MIN_GAIN`, `crate::hotkeys::MAX_GAIN`].
    pub mic_gain: f32,
}

impl Default for MicConfig {
    /// Off, on the platform's default virtual device, at unity gains.
    fn default() -> Self {
        MicConfig {
            enabled: false,
            virtual_device: platform::default_virtual_device().to_string(),
            device_prefer: platform::default_virtual_device_preference().map(str::to_owned),
            sound_gain: default_gain(),
            mic_gain: default_gain(),
        }
    }
}

/// Edits the config file at `path` in place: reads it, parses it into a
/// [`DocumentMut`], runs `edit` on the document and replaces the file with
/// the result atomically (a temp file in the same folder, then a rename), so
/// a crash leaves either the old or the new file. A symlink is written
/// through to the file it points at. Every key, comment and blank line that
/// `edit` leaves alone is kept. A file that is not valid TOML, or an `edit`
/// that fails, leaves the file untouched and is reported as an error naming
/// the path.
pub fn update_config(path: &Path, edit: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    // Write through a symlink to the file it points at.
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|err| anyhow!("invalid config: {err}"))
        .with_context(|| format!("failed to update config {}", path.display()))?;
    edit(&mut doc).with_context(|| format!("failed to update config {}", path.display()))?;
    write_atomically(&path, &doc.to_string())
}

/// Sets `[mic] enabled` in the config file at `path`, creating the table if
/// it is missing, through [`update_config`].
pub fn set_mic_enabled(path: &Path, enabled: bool) -> Result<()> {
    update_config(path, |doc| set_enabled(doc, enabled))
}

/// Sets `[mic] enabled` to `enabled` in `doc`.
fn set_enabled(doc: &mut DocumentMut, enabled: bool) -> Result<()> {
    let mic = doc.entry("mic").or_insert_with(toml_edit::table);
    if let Some(table) = mic.as_table_mut() {
        // A table only implied by `[mic.x]` headers would not print its keys.
        table.set_implicit(false);
    }
    let Some(mic) = mic.as_table_like_mut() else {
        bail!("`mic` is not a table");
    };
    match mic.get_mut("enabled").and_then(Item::as_value_mut) {
        Some(value) => replace_value(value, Value::from(enabled)),
        None => {
            mic.insert("enabled", toml_edit::value(enabled));
        }
    }
    Ok(())
}

/// Replaces `value` with `new`, keeping its spacing and any trailing comment
/// on the line.
pub fn replace_value(value: &mut Value, new: Value) {
    let decor = value.decor().clone();
    *value = new;
    *value.decor_mut() = decor;
}

/// Replaces `path` with `text` through a temp file and a rename.
///
/// The temp file has a name no other writer uses (see [`create_temp`]), so
/// two processes saving at once each rename a complete file of their own:
/// the last rename wins and the config is never a mix of both.
fn write_atomically(path: &Path, text: &str) -> Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("config path {} has no file name", path.display()))?;
    let pid = std::process::id();
    let candidates = std::iter::repeat_with(|| TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed))
        .take(TEMP_ATTEMPTS)
        .map(|seq| path.with_file_name(temp_name(name, pid, seq)));
    let (tmp, mut file) = create_temp(candidates)
        .with_context(|| format!("failed to write config {}", path.display()))?;
    let written = (|| -> std::io::Result<()> {
        file.write_all(text.as_bytes())?;
        if let Ok(meta) = fs::metadata(path) {
            file.set_permissions(meta.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("failed to write config {}", path.display()));
    }
    Ok(())
}

/// Numbers the temp files of this process.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// How many temp names a save tries before it gives up.
const TEMP_ATTEMPTS: usize = 16;

/// The temp file name for config file `name`: hidden, in the same directory
/// (so the rename stays on one file system), unique per process and save.
fn temp_name(name: &OsStr, pid: u32, seq: u64) -> OsString {
    let mut tmp = OsString::from(".");
    tmp.push(name);
    tmp.push(format!(".{pid}.{seq}.tmp"));
    tmp
}

/// Creates the first of `candidates` that does not exist yet. Never opens an
/// existing file, so it cannot truncate or write into another writer's temp
/// file (or a stale one a crash left behind).
fn create_temp(candidates: impl IntoIterator<Item = PathBuf>) -> std::io::Result<(PathBuf, File)> {
    let mut last = None;
    for candidate in candidates {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last
        .unwrap_or_else(|| std::io::Error::new(ErrorKind::AlreadyExists, "no free temp file name")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn write_config(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// The names of the files in `dir`, sorted.
    fn files_in(dir: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

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
        assert_eq!(raw.mic, RawMic::default());
    }

    #[test]
    fn no_mic_table_means_mic_off() {
        let raw = parse("master_gain = 0.5\n").unwrap();
        assert_eq!(
            raw.mic,
            RawMic {
                enabled: false,
                virtual_device: None,
                sound_gain: 1.0,
                mic_gain: 1.0,
            }
        );
    }

    #[test]
    fn mic_table_parses_with_partial_keys() {
        let raw = parse("[mic]\nenabled = true\nvirtual_device = \"Cable\"\n").unwrap();
        assert!(raw.mic.enabled);
        assert_eq!(raw.mic.virtual_device.as_deref(), Some("Cable"));
        assert_eq!(raw.mic.sound_gain, 1.0);
        assert_eq!(raw.mic.mic_gain, 1.0);
        let err = parse("[mic]\nenabled = \"yes\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("enabled"), "{err:#}");
    }

    #[test]
    fn mic_config_default_uses_the_platform_device() {
        let mic = MicConfig::default();
        assert!(!mic.enabled);
        assert_eq!(mic.virtual_device, platform::default_virtual_device());
        assert_eq!((mic.sound_gain, mic.mic_gain), (1.0, 1.0));
    }

    #[test]
    fn set_mic_enabled_flips_only_the_enabled_value_of_the_template() {
        let dir = TempDir::new("config-mic-template");
        let path = write_config(&dir, DEFAULT_CONFIG);
        assert_eq!(DEFAULT_CONFIG.matches("enabled = false").count(), 1);

        set_mic_enabled(&path, true).unwrap();
        let on = fs::read_to_string(&path).unwrap();
        assert_eq!(
            on,
            DEFAULT_CONFIG.replace("enabled = false", "enabled = true")
        );
        assert!(parse(&on).unwrap().mic.enabled);

        set_mic_enabled(&path, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        // No temp file is left behind.
        assert_eq!(files_in(&dir), ["config.toml"]);
    }

    #[test]
    fn set_mic_enabled_keeps_a_trailing_comment_and_other_mic_keys() {
        let dir = TempDir::new("config-mic-comment");
        let path = write_config(
            &dir,
            "[mic]\nenabled = false # off for now\nmic_gain = 0.5 # quiet\n",
        );
        set_mic_enabled(&path, true).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[mic]\nenabled = true # off for now\nmic_gain = 0.5 # quiet\n"
        );
    }

    #[test]
    fn set_mic_enabled_adds_the_table_and_keeps_everything_else() {
        let dir = TempDir::new("config-mic-add");
        let text = "# My sounds.\nmaster_gain = 0.5 # a bit quieter\n\n\
                    # The horn.\n[[sound]]\nname = \"horn\"\npath = \"horn.wav\"\n";
        let path = write_config(&dir, text);
        set_mic_enabled(&path, true).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.starts_with(text), "{updated}");
        let raw = parse(&updated).unwrap();
        assert!(raw.mic.enabled);
        assert_eq!(raw.master_gain, 0.5);
        assert_eq!(raw.sounds.len(), 1);
        assert_eq!(raw.sounds[0].name, "horn");

        // A [mic] table without `enabled` gets the key.
        let path = write_config(&dir, "[mic]\nmic_gain = 0.5\n");
        set_mic_enabled(&path, true).unwrap();
        let raw = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.mic.enabled);
        assert_eq!(raw.mic.mic_gain, 0.5);
    }

    #[test]
    fn set_mic_enabled_handles_inline_and_implicit_tables() {
        let dir = TempDir::new("config-mic-inline");
        let path = write_config(&dir, "mic = { enabled = false, mic_gain = 0.5 }\n");
        set_mic_enabled(&path, true).unwrap();
        let raw = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.mic.enabled);
        assert_eq!(raw.mic.mic_gain, 0.5);

        let path = write_config(&dir, "[mic.extra]\nx = 1\n");
        set_mic_enabled(&path, true).unwrap();
        let raw = parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw.mic.enabled);
    }

    #[test]
    fn set_mic_enabled_fails_cleanly_on_a_malformed_file() {
        let dir = TempDir::new("config-mic-malformed");
        for text in ["master_gain = [\n", "mic = 5\n"] {
            let path = write_config(&dir, text);
            let err = set_mic_enabled(&path, true).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("config.toml"), "{msg}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
            assert_eq!(files_in(&dir), ["config.toml"]);
        }
    }

    #[test]
    fn temp_names_differ_per_process_and_save() {
        let name = OsStr::new("config.toml");
        assert_eq!(temp_name(name, 7, 3), ".config.toml.7.3.tmp");
        assert_ne!(temp_name(name, 7, 3), temp_name(name, 7, 4));
        assert_ne!(temp_name(name, 7, 3), temp_name(name, 8, 3));
    }

    #[test]
    fn create_temp_never_opens_an_existing_file() {
        let dir = TempDir::new("config-temp-busy");
        // Another writer's temp file (or a stale one) holds the first name.
        let busy = dir.path().join(".config.toml.1.0.tmp");
        fs::write(&busy, "someone else's").unwrap();
        let free = dir.path().join(".config.toml.1.1.tmp");
        let (tmp, file) = create_temp([busy.clone(), free.clone()]).unwrap();
        drop(file);
        assert_eq!(tmp, free);
        assert_eq!(fs::read_to_string(&busy).unwrap(), "someone else's");

        let err = create_temp([busy.clone()]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&busy).unwrap(), "someone else's");
    }

    #[test]
    fn concurrent_saves_never_mix_and_leave_no_temp_file() {
        let dir = TempDir::new("config-mic-concurrent");
        let path = write_config(&dir, DEFAULT_CONFIG);
        let savers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        set_mic_enabled(&path, i % 2 == 0).unwrap();
                    }
                })
            })
            .collect();
        for saver in savers {
            saver.join().unwrap();
        }
        let text = fs::read_to_string(&path).unwrap();
        // One whole file from one of the writers.
        assert!(
            text == DEFAULT_CONFIG
                || text == DEFAULT_CONFIG.replace("enabled = false", "enabled = true"),
            "{text}"
        );
        assert_eq!(files_in(&dir), ["config.toml"]);
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let dir = TempDir::new("config-rename-fails");
        // A non-empty directory cannot be replaced by a file.
        let target = dir.path().join("config.toml");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), "x").unwrap();
        assert!(write_atomically(&target, "[mic]\n").is_err());
        assert_eq!(files_in(&dir), ["config.toml"]);
    }

    #[test]
    fn set_mic_enabled_leaves_other_temp_files_alone() {
        let dir = TempDir::new("config-mic-other-temp");
        let path = write_config(&dir, DEFAULT_CONFIG);
        // The fixed name the old code truncated.
        let other = dir.path().join(".config.toml.tmp");
        fs::write(&other, "in flight").unwrap();
        set_mic_enabled(&path, true).unwrap();
        assert!(
            parse(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .mic
                .enabled
        );
        assert_eq!(fs::read_to_string(&other).unwrap(), "in flight");
        assert_eq!(files_in(&dir), [".config.toml.tmp", "config.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn set_mic_enabled_writes_through_a_symlink_and_keeps_the_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = TempDir::new("config-mic-symlink");
        let target = dir.path().join("real.toml");
        fs::write(&target, "master_gain = 0.5\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("config.toml");
        symlink(&target, &link).unwrap();

        set_mic_enabled(&link, true).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let raw = parse(&fs::read_to_string(&target).unwrap()).unwrap();
        assert!(raw.mic.enabled);
        assert_eq!(raw.master_gain, 0.5);
        let mode = fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn set_mic_enabled_on_a_missing_file_names_the_path() {
        let dir = TempDir::new("config-mic-missing");
        let path = dir.path().join("missing.toml");
        let err = set_mic_enabled(&path, true).unwrap_err();
        assert!(format!("{err:#}").contains("missing.toml"), "{err:#}");
        assert!(!path.exists());
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
    fn builtin_prefix_is_case_insensitive() {
        let dir = Path::new("/cfg");
        assert_eq!(
            SoundSource::resolve("BUILTIN:DEMO", dir),
            Ok(SoundSource::BuiltinDemo)
        );
        let err = SoundSource::resolve("BUILTIN:airhorn", dir).unwrap_err();
        assert!(err.contains("BUILTIN:airhorn"), "{err}");
        // A non-ASCII path shorter or longer than the prefix is still a path.
        assert!(matches!(
            SoundSource::resolve("é.wav", dir),
            Ok(SoundSource::File(_))
        ));
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
