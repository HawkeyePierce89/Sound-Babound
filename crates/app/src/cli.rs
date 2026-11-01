//! Command-line interface.
//!
//! With no subcommand the app runs in tray mode. `play [FILE]` plays a single
//! sound headlessly and exits; without `FILE` it plays the embedded demo.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "sound-babound", version, about)]
pub struct Cli {
    /// Subcommand to run. Omit it to start the tray soundboard.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Play a sound through the default output device and exit.
    Play {
        /// Audio file to play (WAV, FLAC, OGG Vorbis, MP3). Omit it to play
        /// the embedded demo chime.
        file: Option<PathBuf>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_means_tray_mode() {
        let cli = Cli::try_parse_from(["sound-babound"]).unwrap();
        assert_eq!(cli.command, None);
    }

    #[test]
    fn play_without_file_uses_demo() {
        let cli = Cli::try_parse_from(["sound-babound", "play"]).unwrap();
        assert_eq!(cli.command, Some(Command::Play { file: None }));
    }

    #[test]
    fn play_with_file() {
        let cli = Cli::try_parse_from(["sound-babound", "play", "path.wav"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Command::Play {
                file: Some(PathBuf::from("path.wav"))
            })
        );
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["sound-babound", "bogus"]).is_err());
    }

    #[test]
    fn play_rejects_extra_positional() {
        assert!(Cli::try_parse_from(["sound-babound", "play", "a.wav", "b.wav"]).is_err());
    }
}
