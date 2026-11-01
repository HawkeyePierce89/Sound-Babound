//! Command-line interface.
//!
//! With no subcommand the app runs in tray mode. `play [FILE]` plays a single
//! sound headlessly and exits; without `FILE` it plays the embedded demo.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use babound_engine::{Engine, Sound};
use clap::{Parser, Subcommand};

use crate::config::SoundSource;

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

/// How often `play` checks whether playback has finished.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long `play` waits for the mixer to pick up the play command. The
/// engine keeps retrying when there is no output device, so without a limit a
/// machine with no audio output would wait forever.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Plays `file` (or the embedded demo) through the default output device and
/// returns once it has finished.
///
/// The sound is decoded before any audio starts, so a bad file fails fast
/// with an error that names it. Ctrl+C terminates the process the default
/// way.
pub fn run_play(file: Option<PathBuf>) -> Result<()> {
    let sound = load_play_sound(file.as_deref())?;
    tracing::info!(
        sound = sound.label(),
        frames = sound.frames(),
        sample_rate = sound.sample_rate(),
        channels = sound.channels(),
        "playing"
    );

    let engine = Engine::start().context("failed to start the audio engine")?;
    if !engine.play(&sound, 1.0) {
        engine.shutdown();
        bail!("the engine rejected the play command");
    }
    let outcome = wait_for_playback(
        || (engine.commands_applied(), engine.active_voices()),
        POLL_INTERVAL,
        START_TIMEOUT,
    );
    engine.shutdown();
    outcome
}

/// Decodes the sound `play` should play: `file` if given, otherwise the
/// embedded demo. This is the same decode path the tray uses.
pub fn load_play_sound(file: Option<&Path>) -> Result<Sound> {
    let source = match file {
        Some(path) => SoundSource::File(path.to_path_buf()),
        None => SoundSource::BuiltinDemo,
    };
    source.load().with_context(|| match file {
        Some(path) => format!("cannot play {}", path.display()),
        None => "cannot decode the embedded demo".to_string(),
    })
}

/// Polls `probe` (returning `(commands_applied, active_voices)`) every
/// `poll` until the play command has been applied and no voice is left.
///
/// Fails if the command has not been applied within `start_timeout`, which
/// means no output stream came up.
fn wait_for_playback(
    probe: impl Fn() -> (u64, usize),
    poll: Duration,
    start_timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let (applied, voices) = probe();
        if applied >= 1 {
            if voices == 0 {
                return Ok(());
            }
        } else if started.elapsed() >= start_timeout {
            bail!(
                "playback did not start within {}s: no usable output device",
                start_timeout.as_secs()
            );
        }
        thread::sleep(poll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    use crate::test_support::TempDir;

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

    #[test]
    fn run_play_on_non_audio_file_names_the_file() {
        let dir = TempDir::new("play-garbage");
        let path = dir.path().join("not-audio.wav");
        std::fs::write(&path, b"this is definitely not a wav file").unwrap();
        // Decoding fails before the engine starts, so no audio is touched.
        let err = run_play(Some(path.clone())).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&path.display().to_string()),
            "message does not name the file: {message}"
        );
    }

    #[test]
    fn run_play_on_missing_file_names_the_file() {
        let dir = TempDir::new("play-missing");
        let path = dir.path().join("missing.flac");
        let message = format!("{:#}", run_play(Some(path.clone())).unwrap_err());
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn embedded_demo_decodes_through_cli_path() {
        let sound = load_play_sound(None).unwrap();
        assert_eq!(sound.label(), crate::config::DEMO_LABEL);
        assert_eq!(sound.sample_rate(), 44_100);
        assert!(sound.frames() > 0);
    }

    #[test]
    fn demo_file_on_disk_decodes_through_cli_path() {
        let dir = TempDir::new("play-demo-file");
        let path = dir.path().join("demo.wav");
        std::fs::write(&path, crate::config::DEMO_WAV).unwrap();
        let from_file = load_play_sound(Some(&path)).unwrap();
        let embedded = load_play_sound(None).unwrap();
        assert_eq!(from_file.frames(), embedded.frames());
        assert_eq!(from_file.channels(), embedded.channels());
    }

    #[test]
    fn wait_returns_once_applied_and_silent() {
        // Not applied, then applied with a voice playing, then finished.
        let states = [(0, 0), (1, 1), (1, 1), (1, 0)];
        let calls = Cell::new(0);
        let probe = || {
            let i = calls.get();
            calls.set(i + 1);
            states[i.min(states.len() - 1)]
        };
        wait_for_playback(probe, Duration::ZERO, Duration::from_secs(60)).unwrap();
        assert_eq!(calls.get(), states.len());
    }

    #[test]
    fn wait_does_not_finish_before_command_is_applied() {
        // Zero voices before the command is applied must not count as done.
        let calls = Cell::new(0u64);
        let probe = || {
            calls.set(calls.get() + 1);
            if calls.get() < 5 { (0, 0) } else { (1, 0) }
        };
        wait_for_playback(probe, Duration::ZERO, Duration::from_secs(60)).unwrap();
        assert_eq!(calls.get(), 5);
    }

    #[test]
    fn wait_times_out_when_command_is_never_applied() {
        let err = wait_for_playback(
            || (0, 0),
            Duration::from_millis(1),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(err.to_string().contains("output device"), "{err}");
    }

    #[test]
    fn wait_has_no_timeout_once_playing() {
        // A long sound keeps playing past the start timeout.
        let calls = Cell::new(0u32);
        let probe = || {
            calls.set(calls.get() + 1);
            if calls.get() < 30 { (1, 1) } else { (1, 0) }
        };
        wait_for_playback(probe, Duration::from_millis(1), Duration::ZERO).unwrap();
        assert_eq!(calls.get(), 30);
    }
}
