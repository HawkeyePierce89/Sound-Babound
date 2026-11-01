//! The settings window's form: the sound rows being edited and their
//! validation. Plain Rust; the view only renders it and forwards edits.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};

use crate::config::{self, RawSound};
use crate::hotkeys;
use crate::settings::keys::KeyCapture;
use crate::settings::save::SoundRow;

/// The file extensions the file picker offers: the formats the engine
/// decodes.
pub const AUDIO_EXTENSIONS: &[&str] = &["wav", "flac", "ogg", "mp3"];

/// What else uses a hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyOwner {
    /// The config's `stop_all_hotkey`.
    StopAll,
    /// The row at this index.
    Row(usize),
}

/// Something that blocks saving a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// The name is empty or only whitespace.
    BlankName,
    /// The hotkey (from the file) does not parse; carries the parser's
    /// message.
    InvalidHotkey(String),
    /// The same hotkey is used elsewhere: by these other owners.
    DuplicateHotkey(Vec<HotkeyOwner>),
}

/// The sound list being edited, built from the config file's text.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsForm {
    /// The sounds as the file had them when the form was built; the save
    /// refuses if the file's sounds no longer equal these.
    loaded: Vec<RawSound>,
    /// The file's `stop_all_hotkey`, read only to detect duplicates.
    stop_all_hotkey: Option<String>,
    rows: Vec<SoundRow>,
    /// Why the file could not be read; editing and saving are off then.
    error: Option<String>,
}

impl SettingsForm {
    /// Builds the form from the config file's text. A file that does not
    /// parse gives a form with no rows and an error.
    pub fn from_text(text: &str) -> SettingsForm {
        match config::parse(text) {
            Ok(raw) => {
                let rows = raw
                    .sounds
                    .iter()
                    .enumerate()
                    .map(|(index, sound)| SoundRow {
                        name: sound.name.clone(),
                        path: sound.path.clone(),
                        hotkey: sound.hotkey.clone(),
                        original: Some(index),
                    })
                    .collect();
                SettingsForm {
                    loaded: raw.sounds,
                    stop_all_hotkey: raw.stop_all_hotkey,
                    rows,
                    error: None,
                }
            }
            Err(err) => SettingsForm::failed(format!("{err:#}")),
        }
    }

    /// A form that cannot be edited, showing `error`.
    pub fn failed(error: String) -> SettingsForm {
        SettingsForm {
            loaded: Vec::new(),
            stop_all_hotkey: None,
            rows: Vec::new(),
            error: Some(error),
        }
    }

    /// Why the file could not be read, if it could not.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the rows can be edited (the file was read).
    pub fn editable(&self) -> bool {
        self.error.is_none()
    }

    /// The sounds the file had when the form was built.
    pub fn loaded(&self) -> &[RawSound] {
        &self.loaded
    }

    /// The file's `stop_all_hotkey`.
    pub fn stop_all_hotkey(&self) -> Option<&str> {
        self.stop_all_hotkey.as_deref()
    }

    /// The rows, in display and save order.
    pub fn rows(&self) -> &[SoundRow] {
        &self.rows
    }

    /// Adds a row for the audio file at `path`, stored as given, with the
    /// file stem as its name and no hotkey. Refuses a path that is not
    /// UTF-8, which the config file cannot hold.
    pub fn add(&mut self, path: &Path) -> Result<()> {
        if !self.editable() {
            return Ok(());
        }
        let text = path
            .to_str()
            .ok_or_else(|| anyhow!("{} is not a UTF-8 path", path.display()))?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.rows.push(SoundRow {
            name,
            path: text.to_owned(),
            hotkey: None,
            original: None,
        });
        Ok(())
    }

    /// Removes row `index`; an index past the end does nothing.
    pub fn remove(&mut self, index: usize) {
        if index < self.rows.len() {
            self.rows.remove(index);
        }
    }

    /// Renames row `index`; an index past the end does nothing.
    pub fn set_name(&mut self, index: usize, name: &str) {
        if let Some(row) = self.rows.get_mut(index) {
            name.clone_into(&mut row.name);
        }
    }

    /// Applies a captured key press to row `index`'s hotkey; an index past
    /// the end does nothing.
    pub fn set_hotkey(&mut self, index: usize, capture: KeyCapture) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        match capture {
            KeyCapture::Set(text) => row.hotkey = Some(text),
            KeyCapture::Clear => row.hotkey = None,
            KeyCapture::Ignore => {}
        }
    }

    /// The problems of every row, indexed like [`rows`](Self::rows): a
    /// blank name, a hotkey that does not parse (a blank one means none, as
    /// in the tray), and a hotkey that another
    /// row or `stop_all_hotkey` also uses (compared by [`HotKey::id`], so
    /// two spellings of one combination count as the same). Every row that
    /// shares a hotkey is marked. A `stop_all_hotkey` that does not parse
    /// is not the window's to fix and is left out.
    ///
    /// [`HotKey::id`]: global_hotkey::hotkey::HotKey::id
    pub fn validate(&self) -> Vec<Vec<Problem>> {
        let mut problems: Vec<Vec<Problem>> = vec![Vec::new(); self.rows.len()];
        let mut owners: HashMap<u32, Vec<HotkeyOwner>> = HashMap::new();
        let stop_all = hotkeys::hotkey_spec(self.stop_all_hotkey.as_deref());
        if let Some(Ok(hotkey)) = stop_all.map(hotkeys::parse_hotkey) {
            owners
                .entry(hotkey.id())
                .or_default()
                .push(HotkeyOwner::StopAll);
        }
        for (index, row) in self.rows.iter().enumerate() {
            if row.name.trim().is_empty() {
                problems[index].push(Problem::BlankName);
            }
            match hotkeys::hotkey_spec(row.hotkey.as_deref()).map(hotkeys::parse_hotkey) {
                Some(Ok(hotkey)) => owners
                    .entry(hotkey.id())
                    .or_default()
                    .push(HotkeyOwner::Row(index)),
                Some(Err(err)) => problems[index].push(Problem::InvalidHotkey(err)),
                None => {}
            }
        }
        for (index, row_problems) in problems.iter_mut().enumerate() {
            let me = HotkeyOwner::Row(index);
            if let Some(shared) = owners.values().find(|owners| owners.contains(&me))
                && shared.len() > 1
            {
                let others = shared.iter().copied().filter(|o| *o != me).collect();
                row_problems.push(Problem::DuplicateHotkey(others));
            }
        }
        problems
    }

    /// Whether Save is allowed: the file was read and no row has a
    /// problem.
    pub fn can_save(&self) -> bool {
        self.editable() && self.validate().iter().all(Vec::is_empty)
    }

    /// A one-line description of `problem`, naming the other owners of a
    /// duplicate hotkey.
    pub fn describe(&self, problem: &Problem) -> String {
        match problem {
            Problem::BlankName => "The name is empty.".to_owned(),
            Problem::InvalidHotkey(err) => format!("The hotkey is not valid: {err}."),
            Problem::DuplicateHotkey(others) => {
                let names: Vec<String> = others.iter().map(|o| self.owner_name(*o)).collect();
                format!("The hotkey is also used by {}.", names.join(", "))
            }
        }
    }

    fn owner_name(&self, owner: HotkeyOwner) -> String {
        match owner {
            HotkeyOwner::StopAll => "\"Stop all\"".to_owned(),
            HotkeyOwner::Row(index) => match self.rows.get(index) {
                Some(row) if !row.name.trim().is_empty() => format!("{:?}", row.name),
                _ => format!("sound #{}", index + 1),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::keys::{Mods, hotkey_text};
    use std::path::PathBuf;

    const TWO_SOUNDS: &str = r#"
stop_all_hotkey = "CmdOrCtrl+Shift+0"

[[sound]]
name = "Chime"
path = "builtin:demo"
hotkey = "CmdOrCtrl+Shift+1"
gain = 0.5

[[sound]]
name = "Airhorn"
path = "sounds/airhorn.mp3"
"#;

    fn form() -> SettingsForm {
        let form = SettingsForm::from_text(TWO_SOUNDS);
        assert!(form.editable(), "{:?}", form.error());
        form
    }

    fn duplicate(others: &[HotkeyOwner]) -> Problem {
        Problem::DuplicateHotkey(others.to_vec())
    }

    #[test]
    fn the_form_starts_from_the_file() {
        let form = form();
        assert_eq!(form.loaded(), config::parse(TWO_SOUNDS).unwrap().sounds);
        assert_eq!(form.stop_all_hotkey(), Some("CmdOrCtrl+Shift+0"));
        assert_eq!(
            form.rows(),
            [
                SoundRow {
                    name: "Chime".to_owned(),
                    path: "builtin:demo".to_owned(),
                    hotkey: Some("CmdOrCtrl+Shift+1".to_owned()),
                    original: Some(0),
                },
                SoundRow {
                    name: "Airhorn".to_owned(),
                    path: "sounds/airhorn.mp3".to_owned(),
                    hotkey: None,
                    original: Some(1),
                },
            ]
        );
        assert!(form.can_save());
        assert_eq!(form.validate(), vec![Vec::new(); 2]);
    }

    #[test]
    fn the_default_template_is_valid() {
        let form = SettingsForm::from_text(config::DEFAULT_CONFIG);
        assert_eq!(form.rows().len(), 1);
        assert!(form.can_save());
    }

    #[test]
    fn adding_a_file_prefills_its_stem() {
        let mut form = form();
        let path = PathBuf::from("/home/me/sounds/rim shot.final.ogg");
        form.add(&path).unwrap();
        assert_eq!(
            form.rows()[2],
            SoundRow {
                name: "rim shot.final".to_owned(),
                path: "/home/me/sounds/rim shot.final.ogg".to_owned(),
                hotkey: None,
                original: None,
            }
        );
        assert!(form.can_save());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_that_is_not_utf8_is_refused() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let mut form = form();
        let path = Path::new(OsStr::from_bytes(b"/tmp/\xff.wav"));
        assert!(form.add(path).is_err());
        assert_eq!(form.rows().len(), 2);
    }

    #[test]
    fn remove_rename_and_set_hotkey_edit_the_rows() {
        let mut form = form();
        form.set_name(1, "Horn");
        form.set_hotkey(1, KeyCapture::Set("F5".to_owned()));
        assert_eq!(form.rows()[1].name, "Horn");
        assert_eq!(form.rows()[1].hotkey.as_deref(), Some("F5"));
        form.set_hotkey(1, KeyCapture::Ignore);
        assert_eq!(form.rows()[1].hotkey.as_deref(), Some("F5"));
        form.set_hotkey(1, KeyCapture::Clear);
        assert_eq!(form.rows()[1].hotkey, None);

        form.remove(0);
        assert_eq!(form.rows().len(), 1);
        assert_eq!(form.rows()[0].original, Some(1));

        // Indices past the end change nothing.
        let before = form.clone();
        form.remove(5);
        form.set_name(5, "x");
        form.set_hotkey(5, KeyCapture::Clear);
        assert_eq!(form, before);
    }

    #[test]
    fn duplicates_between_two_sounds_mark_both() {
        let mut form = form();
        let mods = Mods {
            cmd_or_ctrl: true,
            shift: true,
            ..Mods::default()
        };
        form.set_hotkey(1, hotkey_text(mods, "1"));
        assert_eq!(
            form.validate(),
            vec![
                vec![duplicate(&[HotkeyOwner::Row(1)])],
                vec![duplicate(&[HotkeyOwner::Row(0)])],
            ]
        );
        assert!(!form.can_save());
        assert_eq!(
            form.describe(&form.validate()[0][0]),
            "The hotkey is also used by \"Airhorn\"."
        );
    }

    #[test]
    fn two_spellings_of_one_combination_are_duplicates() {
        let mut form = form();
        form.set_hotkey(1, KeyCapture::Set("cmdorctrl + shift + Digit1".to_owned()));
        assert!(!form.can_save());
    }

    #[test]
    fn a_duplicate_of_stop_all_is_marked() {
        let mut form = form();
        form.set_hotkey(1, KeyCapture::Set("CmdOrCtrl+Shift+0".to_owned()));
        assert_eq!(
            form.validate(),
            vec![Vec::new(), vec![duplicate(&[HotkeyOwner::StopAll])]]
        );
        assert!(!form.can_save());
        assert_eq!(
            form.describe(&form.validate()[1][0]),
            "The hotkey is also used by \"Stop all\"."
        );
    }

    #[test]
    fn three_owners_of_one_hotkey_are_all_marked() {
        let mut form = form();
        form.add(Path::new("/s/a.wav")).unwrap();
        for index in 0..3 {
            form.set_hotkey(index, KeyCapture::Set("CmdOrCtrl+Shift+0".to_owned()));
        }
        let problems = form.validate();
        assert_eq!(
            problems[0],
            vec![duplicate(&[
                HotkeyOwner::StopAll,
                HotkeyOwner::Row(1),
                HotkeyOwner::Row(2)
            ])]
        );
        assert_eq!(
            form.describe(&problems[0][0]),
            "The hotkey is also used by \"Stop all\", \"Airhorn\", \"a\"."
        );
    }

    #[test]
    fn clearing_one_duplicate_unblocks_save() {
        let mut form = form();
        form.set_hotkey(1, KeyCapture::Set("CmdOrCtrl+Shift+1".to_owned()));
        assert!(!form.can_save());
        form.set_hotkey(0, hotkey_text(Mods::default(), "Escape"));
        assert_eq!(form.validate(), vec![Vec::new(); 2]);
        assert!(form.can_save());
    }

    #[test]
    fn a_blank_name_blocks_save() {
        let mut form = form();
        form.set_name(0, "   ");
        assert_eq!(form.validate()[0], vec![Problem::BlankName]);
        assert!(!form.can_save());
        assert_eq!(form.describe(&Problem::BlankName), "The name is empty.");
        form.set_name(0, "Chime");
        assert!(form.can_save());
    }

    #[test]
    fn a_blank_name_is_shown_by_position_in_a_duplicate() {
        let mut form = form();
        form.set_name(1, "");
        form.set_hotkey(1, KeyCapture::Set("CmdOrCtrl+Shift+1".to_owned()));
        assert_eq!(
            form.describe(&form.validate()[0][0]),
            "The hotkey is also used by sound #2."
        );
    }

    #[test]
    fn a_hotkey_from_the_file_that_does_not_parse_is_marked() {
        let text = r#"
stop_all_hotkey = "Nope+Q"

[[sound]]
name = "Chime"
path = "builtin:demo"
hotkey = "Ctrl+Bogus"
"#;
        let form = SettingsForm::from_text(text);
        assert!(form.editable());
        let problems = form.validate();
        assert_eq!(problems.len(), 1);
        assert!(
            matches!(&problems[0][..], [Problem::InvalidHotkey(err)] if err.contains("Ctrl+Bogus")),
            "{problems:?}"
        );
        assert!(!form.can_save());
        assert!(
            form.describe(&problems[0][0])
                .starts_with("The hotkey is not valid: ")
        );
    }

    #[test]
    fn a_blank_hotkey_from_the_file_means_none() {
        let form = SettingsForm::from_text(
            "stop_all_hotkey = \"\"\n\
             [[sound]]\nname = \"a\"\npath = \"a.wav\"\nhotkey = \"\"\n\
             [[sound]]\nname = \"b\"\npath = \"b.wav\"\nhotkey = \"  \"\n\
             [[sound]]\nname = \"c\"\npath = \"c.wav\"\nhotkey = \" F5 \"\n",
        );
        assert_eq!(form.validate(), vec![Vec::new(); 3]);
        assert!(form.can_save());
    }

    #[test]
    fn an_invalid_stop_all_hotkey_is_left_out() {
        let form = SettingsForm::from_text("stop_all_hotkey = \"Nope+Q\"\n");
        assert!(form.validate().is_empty());
        assert!(form.can_save());
    }

    #[test]
    fn a_malformed_file_gives_a_form_error() {
        let mut form = SettingsForm::from_text("[[sound]\nname = ");
        assert!(!form.editable());
        assert!(form.error().is_some_and(|e| e.contains("invalid config")));
        assert!(form.rows().is_empty());
        assert!(!form.can_save());
        // Editing is off.
        form.add(Path::new("/s/a.wav")).unwrap();
        assert!(form.rows().is_empty());

        let wrong_type = SettingsForm::from_text("[[sound]]\nname = 3\npath = \"x\"\n");
        assert!(!wrong_type.editable());
        assert!(!wrong_type.can_save());
    }

    #[test]
    fn every_offered_extension_decodes() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let testdata = manifest.join("../engine/testdata");
        let fixtures = [
            manifest.join("assets/demo.wav"),
            testdata.join("tone-22050-mono.flac"),
            testdata.join("tone-22050-stereo.ogg"),
            testdata.join("tone-22050-mono.mp3"),
        ];
        for extension in AUDIO_EXTENSIONS {
            let fixture = fixtures
                .iter()
                .find(|path| path.extension().is_some_and(|e| e == *extension))
                .unwrap_or_else(|| panic!("no fixture for {extension}"));
            babound_engine::Sound::load_file(fixture)
                .unwrap_or_else(|err| panic!("{extension} does not decode: {err}"));
        }
    }
}
