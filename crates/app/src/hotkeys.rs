//! Hotkey parsing and config validation.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use global_hotkey::hotkey::HotKey;

use crate::config::{RawConfig, ResolvedConfig, ResolvedSound, SoundSource};

/// Lowest accepted gain (per-sound and master).
pub const MIN_GAIN: f32 = 0.0;
/// Highest accepted gain (per-sound and master).
pub const MAX_GAIN: f32 = 4.0;

/// Maps spellings the `global-hotkey` parser does not know to ones it does.
/// Compared case-insensitively.
const ALIASES: &[(&str, &str)] = &[
    ("win", "Super"),
    ("windows", "Super"),
    ("opt", "Alt"),
    ("return", "Enter"),
];

/// Trims whitespace around every token and rewrites [`ALIASES`].
fn normalize(spec: &str) -> String {
    spec.trim()
        .split('+')
        .map(|token| {
            let token = token.trim();
            ALIASES
                .iter()
                .find(|(alias, _)| token.eq_ignore_ascii_case(alias))
                .map_or(token, |(_, canonical)| canonical)
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Parses a hotkey such as `CmdOrCtrl+Shift+1`.
pub fn parse_hotkey(spec: &str) -> Result<HotKey, String> {
    let normalized = normalize(spec);
    HotKey::from_str(&normalized).map_err(|err| format!("invalid hotkey {spec:?}: {err}"))
}

/// Clamps a gain to [`MIN_GAIN`, `MAX_GAIN`], warning when it changes.
/// NaN becomes 1.0.
fn clamp_gain(value: f32, what: &str, warnings: &mut Vec<String>) -> f32 {
    if value.is_nan() {
        warnings.push(format!("{what} is NaN, using 1.0"));
        return 1.0;
    }
    let clamped = value.clamp(MIN_GAIN, MAX_GAIN);
    if clamped != value {
        warnings.push(format!(
            "{what} {value} is outside [{MIN_GAIN}, {MAX_GAIN}], clamped to {clamped}"
        ));
    }
    clamped
}

/// Tracks hotkeys already taken, keyed by the hotkey's id.
struct HotkeyRegistry {
    taken: HashMap<u32, String>,
}

impl HotkeyRegistry {
    /// Parses `spec` for `owner`. Empty specs mean "no hotkey". Unparsable
    /// and duplicate hotkeys are skipped with a warning.
    fn claim(
        &mut self,
        spec: Option<&str>,
        owner: &str,
        warnings: &mut Vec<String>,
    ) -> Option<HotKey> {
        let spec = spec.map(str::trim).filter(|s| !s.is_empty())?;
        let hotkey = match parse_hotkey(spec) {
            Ok(hotkey) => hotkey,
            Err(err) => {
                warnings.push(format!("{owner}: {err}; hotkey skipped"));
                return None;
            }
        };
        if let Some(first) = self.taken.get(&hotkey.id()) {
            warnings.push(format!(
                "{owner}: hotkey {spec:?} is already used by {first}; hotkey skipped"
            ));
            return None;
        }
        self.taken.insert(hotkey.id(), owner.to_string());
        Some(hotkey)
    }
}

/// Validates a raw config.
///
/// - Hotkeys that fail to parse, and every repeat of a hotkey already taken
///   (stop-all first, then sounds in file order), are skipped with a
///   warning; the sound itself stays in the list so the tray can play it.
/// - Gains (per-sound and master) are clamped to [`MIN_GAIN`, `MAX_GAIN`].
/// - Entries whose path cannot be resolved are dropped with a warning.
pub fn resolve(raw: &RawConfig, config_dir: &Path) -> (ResolvedConfig, Vec<String>) {
    let mut warnings = Vec::new();
    let mut registry = HotkeyRegistry {
        taken: HashMap::new(),
    };

    let master_gain = clamp_gain(raw.master_gain, "master_gain", &mut warnings);
    let stop_all_hotkey = registry.claim(
        raw.stop_all_hotkey.as_deref(),
        "stop_all_hotkey",
        &mut warnings,
    );

    let mut sounds = Vec::with_capacity(raw.sounds.len());
    for (index, entry) in raw.sounds.iter().enumerate() {
        let owner = format!("sound #{} {:?}", index + 1, entry.name);
        let source = match SoundSource::resolve(&entry.path, config_dir) {
            Ok(source) => source,
            Err(err) => {
                warnings.push(format!("{owner}: {err}; entry skipped"));
                continue;
            }
        };
        let hotkey = registry.claim(entry.hotkey.as_deref(), &owner, &mut warnings);
        let gain = clamp_gain(entry.gain, &format!("{owner} gain"), &mut warnings);
        sounds.push(ResolvedSound {
            name: entry.name.clone(),
            source,
            hotkey,
            gain,
        });
    }

    (
        ResolvedConfig {
            master_gain,
            stop_all_hotkey,
            sounds,
        },
        warnings,
    )
}

#[cfg(test)]
mod tests {
    use global_hotkey::hotkey::{Code, Modifiers};

    use super::*;
    use crate::config::{self, BUILTIN_DEMO, DEFAULT_CONFIG, RawSound};

    fn sound(name: &str, path: &str, hotkey: Option<&str>, gain: f32) -> RawSound {
        RawSound {
            name: name.into(),
            path: path.into(),
            hotkey: hotkey.map(Into::into),
            gain,
        }
    }

    /// The modifier `CmdOrCtrl` stands for on this OS, as the parser sees it.
    /// `cmd_or_ctrl_is_cmd_or_ctrl` pins it to one of the two, so shared code
    /// needs no OS-specific cfg.
    fn cmd_or_ctrl() -> Modifiers {
        parse_hotkey("CmdOrCtrl+A").unwrap().mods
    }

    #[test]
    fn cmd_or_ctrl_is_cmd_or_ctrl() {
        let mods = cmd_or_ctrl();
        assert!(
            mods == Modifiers::META || mods == Modifiers::CONTROL,
            "{mods:?}"
        );
        assert_eq!(
            parse_hotkey("CmdOrCtrl+Shift+1").unwrap(),
            HotKey::new(Some(mods | Modifiers::SHIFT), Code::Digit1)
        );
    }

    #[test]
    fn documented_examples_parse() {
        let cases = [
            (
                "Ctrl+Shift+1",
                Modifiers::CONTROL | Modifiers::SHIFT,
                Code::Digit1,
            ),
            ("Alt+F5", Modifiers::ALT, Code::F5),
            (
                "Cmd+Shift+A",
                Modifiers::META | Modifiers::SHIFT,
                Code::KeyA,
            ),
            (
                "CmdOrCtrl+Shift+1",
                cmd_or_ctrl() | Modifiers::SHIFT,
                Code::Digit1,
            ),
            (
                "CmdOrCtrl+Shift+0",
                cmd_or_ctrl() | Modifiers::SHIFT,
                Code::Digit0,
            ),
            ("Ctrl+Alt+F5", Modifiers::CONTROL | Modifiers::ALT, Code::F5),
            ("Alt+Space", Modifiers::ALT, Code::Space),
            ("Super+Escape", Modifiers::META, Code::Escape),
        ];
        for (spec, mods, key) in cases {
            assert_eq!(
                parse_hotkey(spec).unwrap(),
                HotKey::new(Some(mods), key),
                "{spec}"
            );
        }
    }

    #[test]
    fn spelling_is_case_and_whitespace_insensitive() {
        let expected = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyA);
        assert_eq!(parse_hotkey("ctrl+shift+a").unwrap(), expected);
        assert_eq!(parse_hotkey(" Ctrl + Shift + A ").unwrap(), expected);
        assert_eq!(parse_hotkey(" F5 ").unwrap(), HotKey::new(None, Code::F5));
    }

    #[test]
    fn the_key_must_come_last() {
        assert!(parse_hotkey("Shift+Ctrl+1").is_ok());
        assert!(parse_hotkey("1+Ctrl").is_err());
        assert!(parse_hotkey("Ctrl+C+Shift").is_err());
    }

    #[test]
    fn aliases_are_normalized() {
        assert_eq!(
            parse_hotkey("Win+Return").unwrap(),
            HotKey::new(Some(Modifiers::META), Code::Enter)
        );
        assert_eq!(
            parse_hotkey("Opt+B").unwrap(),
            HotKey::new(Some(Modifiers::ALT), Code::KeyB)
        );
    }

    #[test]
    fn garbage_hotkeys_are_errors_not_panics() {
        for spec in [
            "",
            "+",
            "Ctrl+",
            "Ctrl+Shift",
            "Ctrl+A+B",
            "Hyper+Q",
            "💥",
            "Ctrl++1",
        ] {
            assert!(parse_hotkey(spec).is_err(), "{spec:?} should not parse");
        }
    }

    #[test]
    fn default_template_resolves_without_warnings() {
        let raw = config::parse(DEFAULT_CONFIG).unwrap();
        let (resolved, warnings) = resolve(&raw, Path::new("/cfg"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(resolved.master_gain, 1.0);
        assert_eq!(
            resolved.stop_all_hotkey,
            Some(HotKey::new(
                Some(cmd_or_ctrl() | Modifiers::SHIFT),
                Code::Digit0
            ))
        );
        assert_eq!(resolved.sounds.len(), 1);
        assert_eq!(resolved.sounds[0].source, SoundSource::BuiltinDemo);
        assert_eq!(
            resolved.sounds[0].hotkey,
            Some(HotKey::new(
                Some(cmd_or_ctrl() | Modifiers::SHIFT),
                Code::Digit1
            ))
        );
    }

    #[test]
    fn garbage_hotkey_warns_and_keeps_the_sound() {
        let raw = RawConfig {
            stop_all_hotkey: Some("Nope+Nope".into()),
            sounds: vec![sound("a", BUILTIN_DEMO, Some("Ctrl+Banana"), 1.0)],
            ..RawConfig::default()
        };
        let (resolved, warnings) = resolve(&raw, Path::new("/cfg"));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("stop_all_hotkey"));
        assert!(warnings[1].contains("Ctrl+Banana"));
        assert_eq!(resolved.stop_all_hotkey, None);
        assert_eq!(resolved.sounds.len(), 1);
        assert_eq!(resolved.sounds[0].hotkey, None);
    }

    #[test]
    fn duplicate_hotkeys_keep_the_first_and_skip_the_rest() {
        let raw = RawConfig {
            stop_all_hotkey: Some("Ctrl+Shift+0".into()),
            sounds: vec![
                sound("a", BUILTIN_DEMO, Some("Ctrl+Shift+1"), 1.0),
                // Same hotkey, different spelling.
                sound("b", BUILTIN_DEMO, Some("shift+control+1"), 1.0),
                sound("c", BUILTIN_DEMO, Some("Ctrl+Shift+0"), 1.0),
                sound("d", BUILTIN_DEMO, Some("Ctrl+Shift+1"), 1.0),
            ],
            ..RawConfig::default()
        };
        let (resolved, warnings) = resolve(&raw, Path::new("/cfg"));
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("\"b\"") && warnings[0].contains("\"a\""));
        assert!(warnings[1].contains("\"c\"") && warnings[1].contains("stop_all_hotkey"));
        assert!(warnings[2].contains("\"d\""));
        let hotkeys: Vec<_> = resolved.sounds.iter().map(|s| s.hotkey.is_some()).collect();
        assert_eq!(hotkeys, [true, false, false, false]);
        assert!(resolved.stop_all_hotkey.is_some());
    }

    #[test]
    fn empty_hotkey_means_none_without_warning() {
        let raw = RawConfig {
            stop_all_hotkey: Some("  ".into()),
            sounds: vec![
                sound("a", BUILTIN_DEMO, Some(""), 1.0),
                sound("b", BUILTIN_DEMO, None, 1.0),
            ],
            ..RawConfig::default()
        };
        let (resolved, warnings) = resolve(&raw, Path::new("/cfg"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(resolved.stop_all_hotkey, None);
        assert!(resolved.sounds.iter().all(|s| s.hotkey.is_none()));
    }

    #[test]
    fn gains_are_clamped_with_warnings() {
        let raw = RawConfig {
            master_gain: 9.0,
            sounds: vec![
                sound("loud", BUILTIN_DEMO, None, 5.0),
                sound("negative", BUILTIN_DEMO, None, -1.0),
                sound("ok", BUILTIN_DEMO, None, 0.5),
                sound("nan", BUILTIN_DEMO, None, f32::NAN),
            ],
            ..RawConfig::default()
        };
        let (resolved, warnings) = resolve(&raw, Path::new("/cfg"));
        assert_eq!(resolved.master_gain, MAX_GAIN);
        let gains: Vec<f32> = resolved.sounds.iter().map(|s| s.gain).collect();
        assert_eq!(gains, [MAX_GAIN, MIN_GAIN, 0.5, 1.0]);
        assert_eq!(warnings.len(), 4, "{warnings:?}");
    }

    #[test]
    fn unresolvable_path_drops_the_entry_without_claiming_its_hotkey() {
        let raw = RawConfig {
            sounds: vec![
                sound("bad", "builtin:nope", Some("Ctrl+1"), 1.0),
                sound("good", "a.wav", Some("Ctrl+1"), 1.0),
            ],
            ..RawConfig::default()
        };
        let dir = Path::new("/cfg");
        let (resolved, warnings) = resolve(&raw, dir);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("builtin:nope"));
        assert_eq!(resolved.sounds.len(), 1);
        assert_eq!(resolved.sounds[0].name, "good");
        assert_eq!(
            resolved.sounds[0].source,
            SoundSource::File(dir.join("a.wav"))
        );
        // The dropped entry did not claim its hotkey.
        assert!(resolved.sounds[0].hotkey.is_some());
    }

    #[test]
    fn paths_resolve_in_all_three_forms() {
        let dir = std::env::temp_dir().join("sb-cfg");
        let abs = std::env::temp_dir().join("abs.wav");
        let raw = RawConfig {
            sounds: vec![
                sound("demo", BUILTIN_DEMO, None, 1.0),
                sound("abs", abs.to_str().unwrap(), None, 1.0),
                sound("rel", "sounds/rel.wav", None, 1.0),
            ],
            ..RawConfig::default()
        };
        let (resolved, warnings) = resolve(&raw, &dir);
        assert!(warnings.is_empty());
        let sources: Vec<_> = resolved.sounds.into_iter().map(|s| s.source).collect();
        assert_eq!(
            sources,
            [
                SoundSource::BuiltinDemo,
                SoundSource::File(abs),
                SoundSource::File(dir.join("sounds/rel.wav")),
            ]
        );
    }
}
