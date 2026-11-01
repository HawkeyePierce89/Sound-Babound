//! The settings window's egui view. It only renders the form and the status
//! report and forwards edits: the form state and validation live in
//! `form.rs`, the key mapping in `keys.rs` and the file writing in
//! `save.rs`.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;
use std::sync::mpsc::Receiver;

use eframe::egui;

use crate::config;
use crate::hotkeys;
use crate::platform;
use crate::settings::StatusText;
use crate::settings::audition::{Auditions, ExitReport, ProcessAudition};
use crate::settings::form::{AUDIO_EXTENSIONS, SettingsForm};
use crate::settings::keys::{self, KeyCapture, Mods};
use crate::settings::save;

/// An edit made while the rows were drawn, applied once they are.
enum Edit {
    Rename(usize, String),
    Remove(usize),
    Capture(usize),
    Audition(usize),
}

pub struct SettingsApp {
    path: PathBuf,
    form: SettingsForm,
    status: StatusText,
    /// The row whose hotkey field waits for a key press.
    capturing: Option<usize>,
    /// The last failed save or add.
    message: Option<String>,
    /// Whether the window has asked for focus (once, at start: the macOS
    /// bundle is an agent app, which the OS does not bring to the front).
    focused: bool,
    /// The running audition, if any (a child process on the default output).
    auditions: Auditions<ProcessAudition>,
    /// The exits of audition children, reported by their waiter threads.
    exits: Receiver<(u64, ExitReport)>,
}

impl SettingsApp {
    pub fn new(
        path: PathBuf,
        form: SettingsForm,
        status: StatusText,
        auditions: Auditions<ProcessAudition>,
        exits: Receiver<(u64, ExitReport)>,
    ) -> SettingsApp {
        SettingsApp {
            path,
            form,
            status,
            auditions,
            exits,
            capturing: None,
            message: None,
            focused: false,
        }
    }

    /// Takes the exits the waiter threads have reported and shows a failed
    /// audition's message.
    fn drain_exits(&mut self) {
        while let Ok((id, report)) = self.exits.try_recv() {
            if let Some(message) = self.auditions.exited(id, report) {
                self.message = Some(message);
            }
        }
    }

    /// Stops the playing row, or plays row `index` (stopping another one).
    fn toggle_audition(&mut self, index: usize) {
        let Some(row) = self.form.rows().get(index) else {
            return;
        };
        let config_dir = self.path.parent().unwrap_or(Path::new(""));
        match self
            .auditions
            .toggle(index, &row.name, &row.path, config_dir)
        {
            // A sound that plays makes an earlier message stale.
            Ok(()) if self.auditions.playing().is_some() => self.message = None,
            Ok(()) => {}
            Err(message) => self.message = Some(message),
        }
    }

    /// Hands the next key press to the capturing row's hotkey field and
    /// keeps every key event from the other widgets while it waits.
    fn capture_key(&mut self, ctx: &egui::Context) {
        let Some(row) = self.capturing else {
            return;
        };
        let mut captured = None;
        ctx.input_mut(|input| {
            input.events.retain(|event| match event {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                } => {
                    if *pressed && !*repeat && captured.is_none() {
                        // macOS hotkeys are key positions, Windows and X11
                        // ones follow the layout: the platform says which
                        // of the two keys to record.
                        let mods = Mods::from_ui(
                            modifiers.command,
                            modifiers.ctrl,
                            modifiers.mac_cmd,
                            modifiers.alt,
                            modifiers.shift,
                        );
                        match keys::capture_text(
                            platform::hotkey_keys(),
                            mods,
                            key.name(),
                            physical_key.map(|key| key.name()),
                        ) {
                            KeyCapture::Ignore => {}
                            capture => captured = Some(capture),
                        }
                    }
                    false
                }
                // egui-winit turns the clipboard shortcuts (CmdOrCtrl+C, X
                // and V, and Shift+Delete, Ctrl+Insert and Shift+Insert on
                // Windows) into these instead of key events. They stay out
                // of the field on purpose, since a global hotkey on them
                // would take copy and paste from every app, and out of the
                // other widgets while the field waits.
                egui::Event::Text(_)
                | egui::Event::Copy
                | egui::Event::Cut
                | egui::Event::Paste(_)
                | egui::Event::PasteImage(_) => false,
                _ => true,
            });
        });
        if let Some(capture) = captured {
            self.form.set_hotkey(row, capture);
            self.capturing = None;
        }
    }

    fn status_section(&self, ui: &mut egui::Ui) {
        ui.heading("Virtual mic");
        let text = self
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match text {
            Some(text) => ui.monospace(text.trim_end()),
            None => ui.weak("checking…"),
        };
    }

    fn sounds_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Sounds");
        if let Some(error) = self.form.error() {
            ui.colored_label(ui.visuals().error_fg_color, error);
            return;
        }
        let problems = self.form.validate();
        let mut edits = Vec::new();
        egui::Grid::new("sounds")
            .num_columns(5)
            .striped(true)
            .show(ui, |ui| {
                ui.strong("Name");
                ui.strong("File");
                ui.strong("Hotkey");
                ui.end_row();
                for (index, row) in self.form.rows().iter().enumerate() {
                    let mut name = row.name.clone();
                    if ui.text_edit_singleline(&mut name).changed() {
                        edits.push(Edit::Rename(index, name));
                    }
                    ui.label(&row.path).on_hover_text(&row.path);
                    let label = if self.capturing == Some(index) {
                        "press a key…"
                    } else {
                        hotkeys::hotkey_spec(row.hotkey.as_deref()).unwrap_or("none")
                    };
                    if ui
                        .button(label)
                        .on_hover_text("Click, then press the hotkey; Escape clears it")
                        .clicked()
                    {
                        edits.push(Edit::Capture(index));
                    }
                    let playing = self.auditions.playing() == Some(index);
                    if ui
                        .button(if playing { "Stop" } else { "Play" })
                        .on_hover_text("Plays the sound on the default output, never in the call")
                        .clicked()
                    {
                        edits.push(Edit::Audition(index));
                    }
                    if ui.button("Remove").clicked() {
                        edits.push(Edit::Remove(index));
                    }
                    ui.end_row();
                    for problem in problems.get(index).into_iter().flatten() {
                        ui.label("");
                        ui.colored_label(ui.visuals().error_fg_color, self.form.describe(problem));
                        ui.end_row();
                    }
                }
            });
        for edit in edits {
            match edit {
                Edit::Rename(index, name) => self.form.set_name(index, &name),
                Edit::Remove(index) => {
                    self.form.remove(index);
                    self.auditions.row_removed(index);
                    self.capturing = None;
                }
                Edit::Capture(index) => {
                    self.capturing = (self.capturing != Some(index)).then_some(index);
                }
                Edit::Audition(index) => self.toggle_audition(index),
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Add sound…").clicked() {
                self.add_sounds();
            }
            ui.menu_button("Add built-in…", |ui| {
                for entry in config::BUILTIN_SOUNDS {
                    if ui.button(entry.label).clicked() {
                        if let Err(err) = self.form.add_builtin(entry.name) {
                            self.message = Some(format!("{err:#}"));
                        }
                        ui.close();
                    }
                }
            });
        });
        if let Some(hotkey) = hotkeys::hotkey_spec(self.form.stop_all_hotkey()) {
            ui.weak(format!(
                "\"Stop all\" hotkey: {hotkey} (set in the config file)"
            ));
        }
    }

    fn add_sounds(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter("Audio", AUDIO_EXTENSIONS)
            .pick_files()
            .unwrap_or_default();
        for path in picked {
            if let Err(err) = self.form.add(&path) {
                self.message = Some(format!("{err:#}"));
            }
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let can_save = self.form.can_save();
            if ui
                .add_enabled(can_save, egui::Button::new("Save"))
                .clicked()
            {
                match save::save_sounds(&self.path, self.form.loaded(), self.form.rows()) {
                    Ok(()) => ui.send_viewport_cmd(egui::ViewportCommand::Close),
                    Err(err) => self.message = Some(format!("{err:#}")),
                }
            }
            if ui.button("Cancel").clicked() {
                ui.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        if let Some(message) = &self.message {
            ui.colored_label(ui.visuals().error_fg_color, message);
        }
    }
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.focused {
            self.focused = true;
            ui.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        self.drain_exits();
        self.capture_key(ui.ctx());
        egui::Panel::bottom("actions").show(ui, |ui| {
            ui.add_space(4.0);
            self.actions(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.status_section(ui);
                ui.separator();
                self.sounds_section(ui);
            });
        });
    }

    /// Save, Cancel and the close button all end here: a playing audition
    /// is cut off.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.auditions.close();
    }
}

#[cfg(test)]
mod tests {
    use crate::settings::keys::{UNSUPPORTED_UI_KEYS, config_key_name};

    use super::{egui, hotkeys};

    /// Every egui key either maps to a config key name that a hotkey can use
    /// or is listed as unsupported, so a key egui renames or adds shows up
    /// here instead of being silently ignored by the window.
    #[test]
    fn every_egui_key_is_mapped_or_listed_as_unsupported() {
        let mut wrong = Vec::new();
        for key in egui::Key::ALL {
            let name = key.name();
            match config_key_name(name) {
                Some(_) if UNSUPPORTED_UI_KEYS.contains(&name) => {
                    wrong.push(format!("{name} is both mapped and listed as unsupported"));
                }
                Some(config) => {
                    if hotkeys::parse_hotkey(&format!("CmdOrCtrl+Shift+{config}")).is_err() {
                        wrong.push(format!(
                            "{name} maps to {config}, which parse_hotkey rejects"
                        ));
                    }
                }
                None if UNSUPPORTED_UI_KEYS.contains(&name) => {}
                None => wrong.push(format!(
                    "{name} is neither mapped nor listed as unsupported"
                )),
            }
        }
        assert!(
            wrong.is_empty(),
            "egui keys out of step:\n{}",
            wrong.join("\n")
        );
    }
}
