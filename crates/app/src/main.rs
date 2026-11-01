#![cfg_attr(windows, windows_subsystem = "windows")]

mod cli;
mod logging;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init();
    tracing::debug!(engine = babound_engine::VERSION, "starting");

    match cli.command {
        None => run_tray(),
        Some(Command::Play { file }) => run_play(file),
    }
}

/// Tray mode. Stub until the tray application is implemented.
fn run_tray() -> anyhow::Result<()> {
    tracing::info!("tray mode is not implemented yet");
    Ok(())
}

/// Headless playback. Stub until the engine is implemented.
fn run_play(file: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    match file {
        Some(path) => tracing::info!(file = %path.display(), "play is not implemented yet"),
        None => tracing::info!("play (embedded demo) is not implemented yet"),
    }
    Ok(())
}
