//! Writes the settings window's sound list back to the config file.
//!
//! The window owns only the `name`, `path` and `hotkey` of the `[[sound]]`
//! entries. Everything else in the file (`gain`, unknown keys, `[mic]`,
//! `master_gain`, `stop_all_hotkey` and every comment) is left as it is.

use std::path::Path;

use anyhow::{Result, anyhow, bail};
use toml_edit::{ArrayOfTables, DocumentMut, Item, RawString, Table, Value};

use crate::config::{self, RawSound};

/// The config key of the sounds' array of tables.
const SOUND_KEY: &str = "sound";

/// One sound in the settings form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundRow {
    pub name: String,
    pub path: String,
    pub hotkey: Option<String>,
    /// The index of the `[[sound]]` entry this row came from, into the
    /// sounds the window loaded; `None` for a row added in the window.
    pub original: Option<usize>,
}

/// Writes `rows` as the `[[sound]]` entries of the config file at `path`,
/// through [`config::update_config`] and [`apply_sounds`]. `loaded` are the
/// sounds the window read when it opened.
pub fn save_sounds(path: &Path, loaded: &[RawSound], rows: &[SoundRow]) -> Result<()> {
    config::update_config(path, |doc| apply_sounds(doc, loaded, rows))
}

/// Replaces the `[[sound]]` entries of `doc` with `rows`, in row order.
///
/// Refuses, leaving the decision to the caller, when the document's sounds
/// no longer equal `loaded` (the file's sounds changed since the window
/// opened). A change elsewhere, such as the tray toggling `[mic] enabled`,
/// is kept.
///
/// A row from the file updates `name`, `path` and `hotkey` in its original
/// table, which keeps `gain`, unknown keys, comments and layout; an
/// unchanged value keeps its exact text, and no hotkey removes the key. A
/// new row becomes a new table. A removed row's table goes with its
/// comments, except the comment above the first entry, which describes the
/// whole list: it moves to whichever entry now comes first. The entries are
/// written where the first one stood (at the end of the file if there was
/// none), and an inline `sound = [...]` array becomes `[[sound]]` tables.
pub fn apply_sounds(doc: &mut DocumentMut, loaded: &[RawSound], rows: &[SoundRow]) -> Result<()> {
    let current = config::parse(&doc.to_string())?.sounds;
    if current != loaded {
        bail!(
            "the sounds in the config file changed since the settings window opened; \
             close the window and open it again to edit them"
        );
    }
    let mut originals: Vec<Option<Table>> = take_sound_tables(doc)?.into_iter().map(Some).collect();
    if originals.len() != loaded.len() {
        bail!("the config file's `sound` entries do not match its sounds");
    }

    // The comment above the first entry belongs to the list, not the entry.
    let first_original = originals.first_mut().and_then(Option::as_mut);
    let position = first_original.as_ref().and_then(|table| table.position());
    let section = match first_original {
        Some(table) if rows.first().and_then(|row| row.original) != Some(0) => {
            let section = prefix_of(table).to_owned();
            // A kept first entry that is no longer first gets a plain header.
            table.decor_mut().set_prefix("\n");
            Some(section)
        }
        _ => None,
    };
    let position = position.unwrap_or_else(|| max_position(doc.as_table()) + 1);

    let mut tables = ArrayOfTables::new();
    for row in rows {
        let mut table = match row.original {
            Some(index) => {
                let slot = originals
                    .get_mut(index)
                    .ok_or_else(|| anyhow!("sound {index} is not in the config file"))?;
                let mut table = slot
                    .take()
                    .ok_or_else(|| anyhow!("sound {index} appears twice"))?;
                update_entry(&mut table, row);
                table
            }
            None => new_entry(row),
        };
        set_positions(&mut table, position);
        tables.push(table);
    }
    if let (Some(section), Some(first)) = (section, tables.get_mut(0)) {
        let own = without_leading_blank_lines(prefix_of(first)).to_owned();
        first.decor_mut().set_prefix(section + &own);
    }

    if tables.is_empty() {
        doc.remove(SOUND_KEY);
    } else {
        let item = doc.entry(SOUND_KEY).or_insert(Item::None);
        *item = Item::ArrayOfTables(tables);
    }
    Ok(())
}

/// Takes the `[[sound]]` tables out of `doc`, leaving `Item::None` behind
/// (an inline array of inline tables is converted).
fn take_sound_tables(doc: &mut DocumentMut) -> Result<Vec<Table>> {
    let Some(item) = doc.get_mut(SOUND_KEY) else {
        return Ok(Vec::new());
    };
    match std::mem::take(item) {
        Item::None => Ok(Vec::new()),
        Item::ArrayOfTables(tables) => Ok(tables.into_iter().collect()),
        Item::Value(Value::Array(array)) => array
            .into_iter()
            .map(|value| match value {
                Value::InlineTable(table) => Ok(table.into_table()),
                _ => Err(anyhow!("`sound` holds a value that is not a table")),
            })
            .collect(),
        other => {
            *item = other;
            bail!("`sound` is not an array of tables")
        }
    }
}

/// Writes `row`'s name, path and hotkey into its original `table`.
fn update_entry(table: &mut Table, row: &SoundRow) {
    set_string(table, "name", &row.name);
    set_string(table, "path", &row.path);
    match &row.hotkey {
        Some(hotkey) => set_string(table, "hotkey", hotkey),
        None => {
            table.remove("hotkey");
        }
    }
}

/// Sets `key` to the string `text`. A value that already equals `text`
/// keeps its exact text; a changed one keeps its spacing and comment.
fn set_string(table: &mut Table, key: &str, text: &str) {
    match table.get_mut(key).and_then(Item::as_value_mut) {
        Some(value) if value.as_str() == Some(text) => {}
        Some(value) => config::replace_value(value, Value::from(text)),
        None => {
            table.insert(key, toml_edit::value(text));
        }
    }
}

/// A new `[[sound]]` table for `row`.
fn new_entry(row: &SoundRow) -> Table {
    let mut table = Table::new();
    table.insert("name", toml_edit::value(row.name.as_str()));
    table.insert("path", toml_edit::value(row.path.as_str()));
    if let Some(hotkey) = &row.hotkey {
        table.insert("hotkey", toml_edit::value(hotkey.as_str()));
    }
    table
}

/// The text before `table`'s header (its comments and blank lines).
fn prefix_of(table: &Table) -> &str {
    table
        .decor()
        .prefix()
        .and_then(RawString::as_str)
        .unwrap_or("")
}

/// `text` from its first line with anything but whitespace on it; empty if
/// it has none.
fn without_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    while let Some(end) = rest.find('\n') {
        if !rest[..end].trim().is_empty() {
            return rest;
        }
        rest = &rest[end + 1..];
    }
    if rest.trim().is_empty() { "" } else { rest }
}

/// Places `table` and the tables nested in it at document position
/// `position`. The document prints tables sorted by position, keeping their
/// order on a tie, so every entry at one position prints in array order.
fn set_positions(table: &mut Table, position: isize) {
    table.set_position(Some(position));
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(nested) => set_positions(nested, position),
            Item::ArrayOfTables(nested) => {
                for nested in nested.iter_mut() {
                    set_positions(nested, position);
                }
            }
            _ => {}
        }
    }
}

/// The largest document position of `table` and the tables nested in it.
fn max_position(table: &Table) -> isize {
    let mut max = table.position().unwrap_or(0);
    for (_, item) in table.iter() {
        let nested = match item {
            Item::Table(nested) => max_position(nested),
            Item::ArrayOfTables(nested) => nested.iter().map(max_position).max().unwrap_or(0),
            _ => continue,
        };
        max = max.max(nested);
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_CONFIG;
    use crate::test_support::TempDir;
    use std::fs;
    use std::path::PathBuf;

    fn write_config(dir: &TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        fs::write(&path, text).unwrap();
        path
    }

    /// The rows the window would start with for `text`.
    fn rows_of(text: &str) -> (Vec<RawSound>, Vec<SoundRow>) {
        let loaded = config::parse(text).unwrap().sounds;
        let rows = loaded
            .iter()
            .enumerate()
            .map(|(index, sound)| SoundRow {
                name: sound.name.clone(),
                path: sound.path.clone(),
                hotkey: sound.hotkey.clone(),
                original: Some(index),
            })
            .collect();
        (loaded, rows)
    }

    fn new_row(name: &str, path: &str, hotkey: Option<&str>) -> SoundRow {
        SoundRow {
            name: name.to_owned(),
            path: path.to_owned(),
            hotkey: hotkey.map(str::to_owned),
            original: None,
        }
    }

    /// `text` with its sounds replaced by `rows`, through a document.
    fn applied(text: &str, loaded: &[RawSound], rows: &[SoundRow]) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        apply_sounds(&mut doc, loaded, rows).unwrap();
        doc.to_string()
    }

    /// `(name, path, hotkey, gain)` of every sound in `text`.
    fn sounds_in(text: &str) -> Vec<(String, String, Option<String>, f32)> {
        config::parse(text)
            .unwrap()
            .sounds
            .into_iter()
            .map(|s| (s.name, s.path, s.hotkey, s.gain))
            .collect()
    }

    const DEMO_ENTRY: &str = "[[sound]]\nname = \"Demo chime\"\npath = \"builtin:demo\"\n\
                              hotkey = \"CmdOrCtrl+Shift+1\"\ngain = 1.0\n";

    #[test]
    fn unchanged_rows_keep_the_file_byte_identical() {
        let (loaded, rows) = rows_of(DEFAULT_CONFIG);
        assert_eq!(applied(DEFAULT_CONFIG, &loaded, &rows), DEFAULT_CONFIG);
    }

    #[test]
    fn round_trip_on_the_template_edits_only_the_sounds() {
        let dir = TempDir::new("save-template");
        let path = write_config(&dir, DEFAULT_CONFIG);
        assert_eq!(DEFAULT_CONFIG.matches(DEMO_ENTRY).count(), 1);

        // Add a sound with a hotkey.
        let (loaded, mut rows) = rows_of(DEFAULT_CONFIG);
        rows.push(new_row(
            "Horn",
            "/sounds/horn.wav",
            Some("CmdOrCtrl+Shift+2"),
        ));
        save_sounds(&path, &loaded, &rows).unwrap();
        let added = fs::read_to_string(&path).unwrap();
        let horn = "\n[[sound]]\nname = \"Horn\"\npath = \"/sounds/horn.wav\"\n\
                    hotkey = \"CmdOrCtrl+Shift+2\"\n";
        assert_eq!(
            added,
            DEFAULT_CONFIG.replace(DEMO_ENTRY, &format!("{DEMO_ENTRY}{horn}")),
        );

        // Rename the demo and clear its hotkey, change the horn's hotkey, add
        // a sound without one, then remove the horn.
        let (loaded, mut rows) = rows_of(&added);
        rows[0].name = "Chime".to_owned();
        rows[0].hotkey = None;
        rows[1].hotkey = Some("Ctrl+Alt+F5".to_owned());
        rows.push(new_row("Bell", "/sounds/bell.ogg", None));
        save_sounds(&path, &loaded, &rows).unwrap();
        let edited = fs::read_to_string(&path).unwrap();
        let expected = DEFAULT_CONFIG.replace(
            DEMO_ENTRY,
            "[[sound]]\nname = \"Chime\"\npath = \"builtin:demo\"\ngain = 1.0\n\
             \n[[sound]]\nname = \"Horn\"\npath = \"/sounds/horn.wav\"\n\
             hotkey = \"Ctrl+Alt+F5\"\n\
             \n[[sound]]\nname = \"Bell\"\npath = \"/sounds/bell.ogg\"\n",
        );
        assert_eq!(edited, expected);

        let (loaded, mut rows) = rows_of(&edited);
        rows.remove(1);
        save_sounds(&path, &loaded, &rows).unwrap();
        let removed = fs::read_to_string(&path).unwrap();
        assert_eq!(
            removed,
            DEFAULT_CONFIG.replace(
                DEMO_ENTRY,
                "[[sound]]\nname = \"Chime\"\npath = \"builtin:demo\"\ngain = 1.0\n\
                 \n[[sound]]\nname = \"Bell\"\npath = \"/sounds/bell.ogg\"\n",
            )
        );

        let raw = config::parse(&removed).unwrap();
        assert_eq!(raw.master_gain, 1.0);
        assert_eq!(raw.stop_all_hotkey.as_deref(), Some("CmdOrCtrl+Shift+0"));
        assert_eq!(raw.mic, config::parse(DEFAULT_CONFIG).unwrap().mic);
        assert_eq!(
            sounds_in(&removed),
            [
                ("Chime".into(), "builtin:demo".into(), None, 1.0),
                ("Bell".into(), "/sounds/bell.ogg".into(), None, 1.0),
            ]
        );
    }

    #[test]
    fn removing_the_first_sound_keeps_the_list_comment() {
        let text = "master_gain = 1.0\n\n# The sounds.\n[[sound]]\nname = \"a\"\npath = \"a.wav\"\n\
                    \n# The second one.\n[[sound]]\nname = \"b\"\npath = \"b.wav\"\n";
        let (loaded, mut rows) = rows_of(text);
        rows.remove(0);
        assert_eq!(
            applied(text, &loaded, &rows),
            "master_gain = 1.0\n\n# The sounds.\n# The second one.\n[[sound]]\n\
             name = \"b\"\npath = \"b.wav\"\n"
        );

        // A new sound in the first place takes the comment as well.
        let (loaded, _) = rows_of(text);
        let rows = [new_row("c", "c.wav", None)];
        assert_eq!(
            applied(text, &loaded, &rows),
            "master_gain = 1.0\n\n# The sounds.\n[[sound]]\nname = \"c\"\npath = \"c.wav\"\n"
        );
    }

    #[test]
    fn rows_are_written_in_row_order_where_the_sounds_stood() {
        let text = "[[sound]]\nname = \"a\"\npath = \"a.wav\"\n\n[mic]\nenabled = true\n\n\
                    [[sound]]\nname = \"b\"\npath = \"b.wav\"\n";
        let (loaded, mut rows) = rows_of(text);
        rows.swap(0, 1);
        rows.insert(1, new_row("c", "c.wav", None));
        let out = applied(text, &loaded, &rows);
        assert_eq!(
            out,
            "[[sound]]\nname = \"b\"\npath = \"b.wav\"\n\
             \n[[sound]]\nname = \"c\"\npath = \"c.wav\"\n\
             \n[[sound]]\nname = \"a\"\npath = \"a.wav\"\n\
             \n[mic]\nenabled = true\n"
        );
        assert!(config::parse(&out).unwrap().mic.enabled);
    }

    #[test]
    fn a_per_sound_gain_unknown_keys_and_comments_survive() {
        let text = "[[sound]]\nname = 'horn' # loud\npath = \"horn.wav\"\n\
                    hotkey = \"CmdOrCtrl+Shift+2\" # the usual\ngain = 0.5\ncolor = \"red\"\n";
        let (loaded, mut rows) = rows_of(text);
        rows[0].path = "/abs/horn.wav".to_owned();
        rows[0].hotkey = Some("CmdOrCtrl+Shift+3".to_owned());
        let out = applied(text, &loaded, &rows);
        // The unchanged name keeps its literal string; the changed values keep
        // their comments.
        assert_eq!(
            out,
            "[[sound]]\nname = 'horn' # loud\npath = \"/abs/horn.wav\"\n\
             hotkey = \"CmdOrCtrl+Shift+3\" # the usual\ngain = 0.5\ncolor = \"red\"\n"
        );
        assert_eq!(
            sounds_in(&out),
            [(
                "horn".into(),
                "/abs/horn.wav".into(),
                Some("CmdOrCtrl+Shift+3".into()),
                0.5
            )]
        );
    }

    #[test]
    fn a_file_without_sounds_gets_the_array() {
        let text = "# Mine.\nmaster_gain = 0.5\n\n[mic]\nenabled = false\n";
        let rows = [new_row("horn", "/horn.wav", Some("F5"))];
        let out = applied(text, &[], &rows);
        assert_eq!(
            out,
            format!("{text}\n[[sound]]\nname = \"horn\"\npath = \"/horn.wav\"\nhotkey = \"F5\"\n")
        );
        assert_eq!(sounds_in(&out).len(), 1);

        assert_eq!(
            applied("", &[], &rows),
            "[[sound]]\nname = \"horn\"\npath = \"/horn.wav\"\nhotkey = \"F5\"\n"
        );
    }

    #[test]
    fn removing_every_sound_removes_the_array() {
        let text = "master_gain = 0.5\n\n[[sound]]\nname = \"a\"\npath = \"a.wav\"\n";
        let (loaded, _) = rows_of(text);
        let out = applied(text, &loaded, &[]);
        assert_eq!(out, "master_gain = 0.5\n");
        assert!(sounds_in(&out).is_empty());
    }

    #[test]
    fn an_inline_sound_array_becomes_tables() {
        let text = "sound = [{ name = \"a\", path = \"a.wav\", gain = 0.5 }]\nmaster_gain = 1.0\n";
        let (loaded, mut rows) = rows_of(text);
        rows.push(new_row("b", "b.wav", None));
        let out = applied(text, &loaded, &rows);
        assert_eq!(
            sounds_in(&out),
            [
                ("a".into(), "a.wav".into(), None, 0.5),
                ("b".into(), "b.wav".into(), None, 1.0),
            ]
        );
        assert!(out.starts_with("master_gain = 1.0\n"), "{out}");
    }

    #[test]
    fn a_concurrent_mic_toggle_is_kept() {
        let dir = TempDir::new("save-mic");
        let path = write_config(&dir, DEFAULT_CONFIG);
        let (loaded, mut rows) = rows_of(DEFAULT_CONFIG);
        // The tray turns the mic on while the window is open.
        config::set_mic_enabled(&path, true).unwrap();
        rows[0].name = "Chime".to_owned();
        save_sounds(&path, &loaded, &rows).unwrap();
        let out = fs::read_to_string(&path).unwrap();
        assert_eq!(
            out,
            DEFAULT_CONFIG
                .replace("enabled = false", "enabled = true")
                .replace("name = \"Demo chime\"", "name = \"Chime\"")
        );
    }

    #[test]
    fn a_changed_sound_list_is_refused_and_the_file_stays_untouched() {
        let dir = TempDir::new("save-changed");
        let (loaded, mut rows) = rows_of(DEFAULT_CONFIG);
        rows[0].name = "Chime".to_owned();
        let changed =
            DEFAULT_CONFIG.replace("gain = 1.0\n\n# [[sound]]", "gain = 0.5\n\n# [[sound]]");
        assert_ne!(changed, DEFAULT_CONFIG);
        for text in [
            changed.as_str(),
            &DEFAULT_CONFIG.replace(DEMO_ENTRY, ""),
            &format!("{DEFAULT_CONFIG}\n[[sound]]\nname = \"x\"\npath = \"x.wav\"\n"),
        ] {
            let path = write_config(&dir, text);
            let err = save_sounds(&path, &loaded, &rows).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains("changed since the settings window opened"),
                "{msg}"
            );
            assert!(msg.contains("config.toml"), "{msg}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn an_invalid_file_stays_untouched() {
        let dir = TempDir::new("save-invalid");
        let rows = [new_row("a", "a.wav", None)];
        for text in [
            "master_gain = [\n",
            "master_gain = \"loud\"\n",
            "sound = 5\n",
        ] {
            let path = write_config(&dir, text);
            let err = save_sounds(&path, &[], &rows).unwrap_err();
            assert!(format!("{err:#}").contains("config.toml"), "{err:#}");
            assert_eq!(fs::read_to_string(&path).unwrap(), text);
        }
    }

    #[test]
    fn bad_row_indices_are_refused() {
        let text = "[[sound]]\nname = \"a\"\npath = \"a.wav\"\n";
        let (loaded, rows) = rows_of(text);
        for bad in [
            vec![rows[0].clone(), rows[0].clone()],
            vec![SoundRow {
                original: Some(1),
                ..rows[0].clone()
            }],
        ] {
            let mut doc: DocumentMut = text.parse().unwrap();
            assert!(apply_sounds(&mut doc, &loaded, &bad).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_save_through_a_symlink_writes_the_target() {
        let dir = TempDir::new("save-symlink");
        let target = write_config(&dir, DEFAULT_CONFIG);
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (loaded, mut rows) = rows_of(DEFAULT_CONFIG);
        rows[0].name = "Chime".to_owned();
        save_sounds(&link, &loaded, &rows).unwrap();
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            DEFAULT_CONFIG.replace("name = \"Demo chime\"", "name = \"Chime\"")
        );
    }
}
