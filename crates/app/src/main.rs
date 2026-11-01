#![cfg_attr(windows, windows_subsystem = "windows")]

mod actions;
mod cli;
mod config;
mod hotkeys;
mod icon;
mod logging;
mod paths;
mod platform;
#[cfg(test)]
mod test_support;
mod tray_app;

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
        None => tray_app::run(),
        Some(Command::Play { file }) => cli::run_play(file),
    }
}
