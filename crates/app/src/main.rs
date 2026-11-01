#![cfg_attr(windows, windows_subsystem = "windows")]

mod cli;
mod config;
mod hotkeys;
mod logging;
mod paths;
mod platform;
#[cfg(test)]
mod test_support;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    // Any argument means CLI mode (including --help and --version): attach to
    // the launching terminal on Windows so the output is visible there.
    if std::env::args_os().len() > 1 {
        platform::attach_parent_console();
    }
    let cli = Cli::parse();
    logging::init();
    tracing::debug!(engine = babound_engine::VERSION, "starting");

    match cli.command {
        None => run_tray(),
        Some(Command::Play { file }) => cli::run_play(file),
    }
}

/// Tray mode. Until the tray application is implemented this only creates,
/// loads and validates the config, decodes its sounds and logs the result.
fn run_tray() -> anyhow::Result<()> {
    let path = paths::ensure_default_config()?;
    let raw = config::load(&path)?;
    let config_dir = path.parent().unwrap_or(std::path::Path::new("."));
    let (resolved, warnings) = hotkeys::resolve(&raw, config_dir);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }
    for entry in &resolved.sounds {
        match entry.source.load() {
            Ok(sound) => tracing::info!(
                name = %entry.name,
                hotkey = ?entry.hotkey.map(|h| h.to_string()),
                gain = entry.gain,
                frames = sound.frames(),
                "sound ready"
            ),
            Err(err) => tracing::warn!(name = %entry.name, "{err}"),
        }
    }
    tracing::info!(
        sounds = resolved.sounds.len(),
        master_gain = resolved.master_gain,
        stop_all = ?resolved.stop_all_hotkey.map(|h| h.to_string()),
        "config loaded; tray mode is not implemented yet"
    );
    Ok(())
}
