//! The settings window (`sound-babound settings`). Only `view.rs` knows the
//! UI library; the form state, key mapping and file writing are plain Rust.

pub mod form;
pub mod keys;
pub mod save;
mod view;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use eframe::egui;

use crate::mic_report::{format_mic_check, status_check};
use crate::paths;
use crate::platform;
use crate::settings::form::SettingsForm;

/// The window title.
const TITLE: &str = "Sound-Babound settings";

/// The window's starting size, in points.
const START_SIZE: [f32; 2] = [640.0, 560.0];

/// How often the status thread refreshes the virtual mic report.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

/// The latest virtual mic report, written by the status thread and read by
/// the view; `None` until the first check is done.
pub type StatusText = Arc<Mutex<Option<String>>>;

/// Runs the settings window until it closes and returns the process exit
/// code: failure when the window could not be opened.
pub fn run() -> ExitCode {
    match run_window() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("the settings window failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_window() -> Result<()> {
    let path = paths::ensure_default_config()?;
    let form = load_form(&path);
    let status: StatusText = Arc::new(Mutex::new(None));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size(START_SIZE)
            .with_active(true),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        TITLE,
        options,
        Box::new(move |cc| {
            spawn_status_thread(path.clone(), Arc::downgrade(&status), cc.egui_ctx.clone())?;
            Ok(Box::new(view::SettingsApp::new(path, form, status)))
        }),
    )
    .map_err(|err| anyhow!("{err}"))
}

/// The form for the config file at `path`; a file that cannot be read
/// gives a form that shows why and cannot be edited.
fn load_form(path: &Path) -> SettingsForm {
    match fs::read_to_string(path) {
        Ok(text) => SettingsForm::from_text(&text),
        Err(err) => SettingsForm::failed(format!("cannot read {}: {err}", path.display())),
    }
}

/// Starts the thread that checks the virtual mic every
/// [`STATUS_INTERVAL`] and publishes the report to `status`, asking `ctx`
/// to repaint. The probe never runs on the UI thread, since a PulseAudio
/// query can block. The thread ends once the window has dropped `status`;
/// one blocked in a query is left to the process exit.
fn spawn_status_thread(
    path: PathBuf,
    status: Weak<Mutex<Option<String>>>,
    ctx: egui::Context,
) -> Result<()> {
    thread::Builder::new()
        .name("settings-status".into())
        .spawn(move || {
            loop {
                let check = status_check(&path);
                let report = format_mic_check(&check, platform::virtual_device_missing_hint());
                let Some(status) = status.upgrade() else {
                    return;
                };
                *status.lock().unwrap_or_else(PoisonError::into_inner) = Some(report.text);
                drop(status);
                ctx.request_repaint();
                thread::sleep(STATUS_INTERVAL);
            }
        })
        .context("cannot start the status thread")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    #[test]
    fn a_readable_file_gives_an_editable_form() {
        let dir = TempDir::new("settings-form");
        let path = dir.path().join("config.toml");
        fs::write(&path, crate::config::DEFAULT_CONFIG).unwrap();
        let form = load_form(&path);
        assert!(form.editable());
        assert_eq!(form.error(), None);
    }

    #[test]
    fn an_unreadable_file_gives_a_form_that_says_why() {
        let dir = TempDir::new("settings-missing");
        let path = dir.path().join("missing.toml");
        let form = load_form(&path);
        assert!(!form.editable());
        assert!(form.error().unwrap().contains("cannot read"));
    }
}
