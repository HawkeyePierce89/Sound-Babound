//! The settings window's egui view. It only renders the form and the status
//! report and forwards edits: the form state and validation live in
//! `form.rs`, the key mapping in `keys.rs` and the file writing in
//! `save.rs`.

use std::path::PathBuf;
use std::sync::PoisonError;

use eframe::egui;

use crate::settings::StatusText;
use crate::settings::form::{AUDIO_EXTENSIONS, SettingsForm};
use crate::settings::keys::{self, KeyCapture, Mods};
use crate::settings::save;

/// An edit made while the rows were drawn, applied once they are.
enum Edit {
    Rename(usize, String),
    Remove(usize),
    Capture(usize),
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
}

impl SettingsApp {
    pub fn new(path: PathBuf, form: SettingsForm, status: StatusText) -> SettingsApp {
        SettingsApp {
            path,
            form,
            status,
            capturing: None,
            message: None,
            focused: false,
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
                        // Global hotkeys are physical keys, so prefer the
                        // physical key over the layout's.
                        let name = physical_key.unwrap_or(*key).name();
                        let mods = Mods::from_ui(
                            modifiers.command,
                            modifiers.ctrl,
                            modifiers.mac_cmd,
                            modifiers.alt,
                            modifiers.shift,
                        );
                        match keys::hotkey_text(mods, name) {
                            KeyCapture::Ignore => {}
                            capture => captured = Some(capture),
                        }
                    }
                    false
                }
                egui::Event::Text(_) => false,
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
            .num_columns(4)
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
                        row.hotkey.as_deref().unwrap_or("none")
                    };
                    if ui
                        .button(label)
                        .on_hover_text("Click, then press the hotkey; Escape clears it")
                        .clicked()
                    {
                        edits.push(Edit::Capture(index));
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
                    self.capturing = None;
                }
                Edit::Capture(index) => {
                    self.capturing = (self.capturing != Some(index)).then_some(index);
                }
            }
        }
        if ui.button("Add sound…").clicked() {
            self.add_sounds();
        }
        if let Some(hotkey) = self.form.stop_all_hotkey() {
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
}

#[cfg(test)]
mod tests {
    use crate::hotkeys;
    use crate::settings::keys::{UNSUPPORTED_UI_KEYS, config_key_name};

    use super::egui;

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
