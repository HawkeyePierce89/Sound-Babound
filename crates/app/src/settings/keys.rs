//! Turns a key press in the settings window into a hotkey in the config's
//! syntax. Plain Rust: the view hands over the UI library's key name and
//! modifier flags.

use crate::hotkeys;

/// The letter keys, as both the UI and the config name them.
const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];

/// The digit row, as both the UI and the config name it.
const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];

/// The function keys a hotkey may use, as both the UI and the config name
/// them. These are the only keys accepted without a modifier.
const FUNCTION_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// UI key names that differ from, or only exist next to, the config's. The
/// left side is egui's `Key::name()` vocabulary (arrows are `Up`, `Down`,
/// `Left`, `Right`; punctuation is `Equals`, `Backtick`, `OpenBracket`,
/// `CloseBracket`), plus the W3C code spellings (`ArrowUp`, `Equal`,
/// `Backquote`, `BracketLeft`, `Numpad0`) in case a UI reports those.
const NAMED_KEYS: &[(&str, &str)] = &[
    ("Space", "Space"),
    ("Enter", "Enter"),
    ("Tab", "Tab"),
    ("Backspace", "Backspace"),
    ("Delete", "Delete"),
    ("Insert", "Insert"),
    ("Home", "Home"),
    ("End", "End"),
    ("PageUp", "PageUp"),
    ("PageDown", "PageDown"),
    ("Up", "Up"),
    ("ArrowUp", "Up"),
    ("Down", "Down"),
    ("ArrowDown", "Down"),
    ("Left", "Left"),
    ("ArrowLeft", "Left"),
    ("Right", "Right"),
    ("ArrowRight", "Right"),
    ("Numpad0", "Num0"),
    ("Numpad1", "Num1"),
    ("Numpad2", "Num2"),
    ("Numpad3", "Num3"),
    ("Numpad4", "Num4"),
    ("Numpad5", "Num5"),
    ("Numpad6", "Num6"),
    ("Numpad7", "Num7"),
    ("Numpad8", "Num8"),
    ("Numpad9", "Num9"),
    ("Minus", "Minus"),
    ("Equals", "Equal"),
    ("Equal", "Equal"),
    ("Comma", "Comma"),
    ("Period", "Period"),
    ("Slash", "Slash"),
    ("Semicolon", "Semicolon"),
    ("Quote", "Quote"),
    ("Backtick", "Backquote"),
    ("Backquote", "Backquote"),
    ("OpenBracket", "BracketLeft"),
    ("BracketLeft", "BracketLeft"),
    ("CloseBracket", "BracketRight"),
    ("BracketRight", "BracketRight"),
    ("Backslash", "Backslash"),
];

/// egui key names that never become a hotkey: the modifier keys themselves
/// (egui reports a lone Shift or Ctrl press as a key, which then leaves the
/// field waiting), shifted symbols that share a physical key with another
/// (`Plus` is `Equal`, `Colon` is `Semicolon`), clipboard, browser, media,
/// volume, launch and system keys (Caps Lock, Print Screen, Pause, Num Lock
/// and the like), function keys past F24, and `Escape`, which clears the
/// field instead (see [`hotkey_text`]). Only the tests use it: the view's
/// test checks it against egui's `Key::ALL`.
#[cfg(test)]
pub const UNSUPPORTED_UI_KEYS: &[&str] = &[
    "Escape",
    "Copy",
    "Cut",
    "Paste",
    "Colon",
    "Plus",
    "Pipe",
    "Questionmark",
    "Exclamationmark",
    "OpenCurlyBracket",
    "CloseCurlyBracket",
    "BrowserBack",
    "BrowserForward",
    "BrowserRefresh",
    "BrowserSearch",
    "BrowserHome",
    "BrowserFavorites",
    "BrowserStop",
    "MediaPlayPause",
    "MediaTrackNext",
    "MediaTrackPrevious",
    "MediaStop",
    "AudioVolumeMute",
    "AudioVolumeDown",
    "AudioVolumeUp",
    "LaunchMail",
    "LaunchApp1",
    "LaunchApp2",
    "NumLock",
    "ShiftLeft",
    "ShiftRight",
    "ControlLeft",
    "ControlRight",
    "AltLeft",
    "AltRight",
    "SuperLeft",
    "SuperRight",
    "Fn",
    "IntlBackslash",
    "CapsLock",
    "ScrollLock",
    "PrintScreen",
    "Pause",
    "Menu",
    "Eject",
    "Help",
    "Power",
    "Sleep",
    "Clear",
    "F25",
    "F26",
    "F27",
    "F28",
    "F29",
    "F30",
    "F31",
    "F32",
    "F33",
    "F34",
    "F35",
];

/// The UI key name that clears a hotkey field when pressed alone.
const CLEAR_KEY: &str = "Escape";

/// Maps a UI key name (egui's `Key::name()` vocabulary) to the config's
/// name for that key: letters, digits, F1–F24, Space, Enter, Tab,
/// Backspace, Delete, Insert, Home, End, PageUp, PageDown, the arrows (as
/// Up, Down, Left, Right), numpad digits (as Num0–Num9) and punctuation.
/// `None` for any other key (see [`UNSUPPORTED_UI_KEYS`]).
pub fn config_key_name(ui_key: &str) -> Option<&'static str> {
    LETTERS
        .iter()
        .chain(&DIGITS)
        .chain(&FUNCTION_KEYS)
        .find(|name| **name == ui_key)
        .copied()
        .or_else(|| {
            NAMED_KEYS
                .iter()
                .find(|(ui, _)| *ui == ui_key)
                .map(|(_, config)| *config)
        })
}

/// The modifiers held with a key press.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Mods {
    /// Cmd on macOS, Ctrl elsewhere: written as `CmdOrCtrl`.
    pub cmd_or_ctrl: bool,
    /// A Control press separate from `cmd_or_ctrl`, which exists on macOS
    /// only (elsewhere Control is `cmd_or_ctrl`): written as `Ctrl`.
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// Builds the modifiers from flags in egui's convention: `command` is
    /// Cmd on macOS and Ctrl elsewhere, `ctrl` is the Control key, and
    /// `mac_cmd` is Cmd, set on macOS only. Control counts as separate when
    /// Cmd is held too or Control does not stand for `command`, which is
    /// only ever true on macOS.
    pub fn from_ui(command: bool, ctrl: bool, mac_cmd: bool, alt: bool, shift: bool) -> Mods {
        Mods {
            cmd_or_ctrl: command,
            ctrl: ctrl && (mac_cmd || !command),
            alt,
            shift,
        }
    }

    /// Whether any modifier is held.
    pub fn any(self) -> bool {
        self.cmd_or_ctrl || self.ctrl || self.alt || self.shift
    }
}

/// What a key press does to a hotkey field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCapture {
    /// Set the field to this hotkey, in the config syntax.
    Set(String),
    /// Clear the field.
    Clear,
    /// Leave the field as it is.
    Ignore,
}

/// Turns a key press into a [`KeyCapture`]:
/// - `Escape` alone clears the field;
/// - a supported key with modifiers, or F1–F24 alone, sets it, as
///   `CmdOrCtrl+Ctrl+Alt+Shift+Key` (only the held modifiers, always in
///   that order);
/// - anything else is ignored: an unsupported key, a lone modifier, a
///   non-function key without a modifier, or a combination
///   [`hotkeys::parse_hotkey`] rejects, so the window never writes a
///   hotkey the tray would refuse.
pub fn hotkey_text(mods: Mods, ui_key: &str) -> KeyCapture {
    if ui_key == CLEAR_KEY && !mods.any() {
        return KeyCapture::Clear;
    }
    let Some(key) = config_key_name(ui_key) else {
        return KeyCapture::Ignore;
    };
    if !mods.any() && !FUNCTION_KEYS.contains(&key) {
        return KeyCapture::Ignore;
    }
    let held = [
        (mods.cmd_or_ctrl, "CmdOrCtrl"),
        (mods.ctrl, "Ctrl"),
        (mods.alt, "Alt"),
        (mods.shift, "Shift"),
    ];
    let mut parts: Vec<&str> = held
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
    parts.push(key);
    let text = parts.join("+");
    if hotkeys::parse_hotkey(&text).is_ok() {
        KeyCapture::Set(text)
    } else {
        KeyCapture::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_MODS: Mods = Mods {
        cmd_or_ctrl: true,
        ctrl: true,
        alt: true,
        shift: true,
    };

    fn shift() -> Mods {
        Mods {
            shift: true,
            ..Mods::default()
        }
    }

    /// Every UI name the mapping accepts, with its config name.
    fn supported() -> Vec<(&'static str, &'static str)> {
        LETTERS
            .iter()
            .chain(&DIGITS)
            .chain(&FUNCTION_KEYS)
            .map(|name| (*name, *name))
            .chain(NAMED_KEYS.iter().copied())
            .collect()
    }

    #[test]
    fn the_mapping_table_covers_every_supported_key() {
        let expected: &[(&str, &str)] = &[
            ("A", "A"),
            ("Z", "Z"),
            ("0", "0"),
            ("9", "9"),
            ("F1", "F1"),
            ("F12", "F12"),
            ("F24", "F24"),
            ("Space", "Space"),
            ("Enter", "Enter"),
            ("Tab", "Tab"),
            ("Backspace", "Backspace"),
            ("Delete", "Delete"),
            ("Insert", "Insert"),
            ("Home", "Home"),
            ("End", "End"),
            ("PageUp", "PageUp"),
            ("PageDown", "PageDown"),
            ("Up", "Up"),
            ("Down", "Down"),
            ("Left", "Left"),
            ("Right", "Right"),
            ("ArrowUp", "Up"),
            ("ArrowDown", "Down"),
            ("ArrowLeft", "Left"),
            ("ArrowRight", "Right"),
            ("Numpad0", "Num0"),
            ("Numpad9", "Num9"),
            ("Minus", "Minus"),
            ("Equals", "Equal"),
            ("Comma", "Comma"),
            ("Period", "Period"),
            ("Slash", "Slash"),
            ("Semicolon", "Semicolon"),
            ("Quote", "Quote"),
            ("Backtick", "Backquote"),
            ("OpenBracket", "BracketLeft"),
            ("CloseBracket", "BracketRight"),
            ("Backslash", "Backslash"),
        ];
        for (ui, config) in expected {
            assert_eq!(config_key_name(ui), Some(*config), "{ui}");
        }
        assert_eq!(supported().len(), 26 + 10 + 24 + NAMED_KEYS.len());
    }

    #[test]
    fn every_supported_key_parses_with_a_modifier() {
        for (ui, config) in supported() {
            assert!(
                hotkeys::parse_hotkey(&format!("CmdOrCtrl+{config}")).is_ok(),
                "{ui} -> {config}"
            );
            assert_eq!(
                hotkey_text(shift(), ui),
                KeyCapture::Set(format!("Shift+{config}")),
                "{ui}"
            );
        }
    }

    #[test]
    fn unsupported_keys_map_to_nothing() {
        for ui in UNSUPPORTED_UI_KEYS.iter().chain(&[
            "", "a", "f5", "Shift", "Control", "Alt", "Meta", "F0", "Num0",
        ]) {
            assert_eq!(config_key_name(ui), None, "{ui}");
            assert_eq!(hotkey_text(ALL_MODS, ui), KeyCapture::Ignore, "{ui}");
        }
    }

    #[test]
    fn modifiers_are_written_in_a_fixed_order() {
        assert_eq!(
            hotkey_text(ALL_MODS, "A"),
            KeyCapture::Set("CmdOrCtrl+Ctrl+Alt+Shift+A".to_owned())
        );
        let cmd_shift = Mods {
            cmd_or_ctrl: true,
            shift: true,
            ..Mods::default()
        };
        assert_eq!(
            hotkey_text(cmd_shift, "1"),
            KeyCapture::Set("CmdOrCtrl+Shift+1".to_owned())
        );
        let ctrl_alt = Mods {
            ctrl: true,
            alt: true,
            ..Mods::default()
        };
        assert_eq!(
            hotkey_text(ctrl_alt, "F5"),
            KeyCapture::Set("Ctrl+Alt+F5".to_owned())
        );
    }

    #[test]
    fn control_is_separate_on_macos_only() {
        // macOS: Cmd sets command and mac_cmd; Control sets ctrl alone.
        assert_eq!(
            Mods::from_ui(true, false, true, false, false),
            Mods {
                cmd_or_ctrl: true,
                ..Mods::default()
            }
        );
        assert_eq!(
            Mods::from_ui(false, true, false, false, false),
            Mods {
                ctrl: true,
                ..Mods::default()
            }
        );
        assert_eq!(
            Mods::from_ui(true, true, true, true, true),
            ALL_MODS,
            "Cmd+Control+Option+Shift on macOS"
        );
        // Windows and Linux: Control sets command and ctrl, never mac_cmd.
        assert_eq!(
            Mods::from_ui(true, true, false, false, true),
            Mods {
                cmd_or_ctrl: true,
                shift: true,
                ..Mods::default()
            }
        );
        assert_eq!(
            hotkey_text(Mods::from_ui(true, true, false, false, false), "B"),
            KeyCapture::Set("CmdOrCtrl+B".to_owned())
        );
        assert_eq!(
            hotkey_text(Mods::from_ui(false, true, false, false, false), "B"),
            KeyCapture::Set("Ctrl+B".to_owned())
        );
    }

    #[test]
    fn escape_alone_clears() {
        assert_eq!(hotkey_text(Mods::default(), "Escape"), KeyCapture::Clear);
        assert_eq!(hotkey_text(shift(), "Escape"), KeyCapture::Ignore);
    }

    #[test]
    fn only_function_keys_work_without_a_modifier() {
        assert_eq!(
            hotkey_text(Mods::default(), "F5"),
            KeyCapture::Set("F5".to_owned())
        );
        assert_eq!(
            hotkey_text(Mods::default(), "F24"),
            KeyCapture::Set("F24".to_owned())
        );
        for ui in ["A", "1", "Space", "Enter", "Up", "Numpad1", "Minus", "Tab"] {
            assert_eq!(hotkey_text(Mods::default(), ui), KeyCapture::Ignore, "{ui}");
        }
    }

    #[test]
    fn a_lone_modifier_is_ignored() {
        for ui in [
            "Shift", "Control", "Ctrl", "Alt", "Option", "Command", "Meta",
        ] {
            assert_eq!(hotkey_text(shift(), ui), KeyCapture::Ignore, "{ui}");
            assert_eq!(hotkey_text(Mods::default(), ui), KeyCapture::Ignore, "{ui}");
        }
    }

    #[test]
    fn every_set_output_parses() {
        let combos: Vec<Mods> = (0..16u8)
            .map(|bits| Mods {
                cmd_or_ctrl: bits & 1 != 0,
                ctrl: bits & 2 != 0,
                alt: bits & 4 != 0,
                shift: bits & 8 != 0,
            })
            .collect();
        let mut set = 0;
        for mods in combos {
            for (ui, _) in supported() {
                if let KeyCapture::Set(text) = hotkey_text(mods, ui) {
                    assert!(hotkeys::parse_hotkey(&text).is_ok(), "{text}");
                    set += 1;
                }
            }
        }
        // 15 modifier sets for every key, plus the function keys alone.
        assert_eq!(set, 15 * supported().len() + FUNCTION_KEYS.len());
    }
}
