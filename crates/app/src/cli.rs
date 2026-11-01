//! Command-line interface.
//!
//! With no subcommand the app runs in tray mode. `play [FILE]` plays a single
//! sound headlessly and exits; without `FILE` it plays the embedded demo.
//! `mic-check` reports whether the virtual microphone can work, without
//! opening a stream.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use babound_engine::{BuildError, Engine, MicProbe, MicSettings, Sound, StreamInfo, probe_mic};
use clap::{Parser, Subcommand};

use crate::actions::{mic_settings, settings_for};
use crate::config::{MicConfig, SoundSource};
use crate::hotkeys::resolve_mic;
use crate::platform::{self, MicrophoneAccess};

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(name = "sound-babound", version, about)]
pub struct Cli {
    /// Subcommand to run. Omit it to start the tray soundboard.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
pub enum Command {
    /// Play a sound through the default output device and exit. With
    /// `[mic] enabled = true` in the config it also plays on the virtual mic.
    Play {
        /// Audio file to play (WAV, FLAC, OGG Vorbis, MP3). Omit it to play
        /// the embedded demo chime.
        file: Option<PathBuf>,
    },
    /// Check the virtual mic setup: the virtual device, the microphone input
    /// and the microphone permission. Opens no stream and plays nothing;
    /// exits non-zero when the virtual mic cannot work.
    MicCheck,
}

/// How often `play` checks whether playback has finished.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long `play` waits for the mixer to pick up the play command. The
/// engine keeps retrying when there is no output device, so without a limit a
/// machine with no audio output would wait forever.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `play` waits for the mic sink after the monitor is up. Past it,
/// `play` warns and plays on the monitor only.
const MIC_START_TIMEOUT: Duration = Duration::from_secs(3);

/// Slack added to the reported output latency before `play` stops the
/// stream. It covers the poll interval; the reported latency already
/// includes the duration of the last callback's buffer.
const OUTPUT_DRAIN_MARGIN: Duration = Duration::from_millis(200);

/// The most output latency `play` waits out, so a bogus backend timestamp
/// cannot hold the process.
const MAX_OUTPUT_LATENCY: Duration = Duration::from_secs(5);

/// How long past the sound's own duration `play` waits for it to finish.
/// If the output device disappears mid-playback the voice never finishes,
/// so without a limit `play` would wait forever.
const FINISH_MARGIN: Duration = Duration::from_secs(10);

/// Plays `file` (or the embedded demo) through the default output device and
/// returns once it has finished. With the mic enabled in the config, it also
/// plays on the virtual mic.
///
/// The sound is decoded before any audio starts, so a bad file fails fast
/// with an error that names it. Ctrl+C terminates the process the default
/// way.
pub fn run_play(file: Option<PathBuf>) -> Result<()> {
    let sound = load_play_sound(file.as_deref())?;
    let (_, mic, notes) = cli_mic_config();
    for note in &notes {
        tracing::info!("{note}");
    }
    tracing::info!(
        sound = sound.label(),
        frames = sound.frames(),
        sample_rate = sound.sample_rate(),
        channels = sound.channels(),
        mic = mic.enabled,
        "playing"
    );

    let mut engine = Engine::start().context("failed to start the audio engine")?;
    let outcome = play_to_end(&mut engine, &sound, &mic);
    engine.shutdown();
    outcome
}

/// Starts the mic path if `mic` is enabled, waits for the output streams,
/// plays `sound` and waits until it has finished and the outputs have had
/// time to drain.
///
/// The mic path runs without the capture: a headless play only puts the
/// sound on the virtual mic, so it never opens the microphone (or asks for
/// its permission). It does not create the Linux virtual device either; that
/// is the tray's job.
fn play_to_end(engine: &mut Engine, sound: &Sound, mic: &MicConfig) -> Result<()> {
    if let Some(settings) = headless_mic_settings(mic)
        && let Err(err) = engine.set_mic(Some(settings))
    {
        tracing::warn!(error = %err, "cannot start the virtual mic; playing on the monitor only");
    }
    // A play queued while no stream runs is dropped, so wait for one first.
    wait_for_output(|| engine.has_output(), POLL_INTERVAL, START_TIMEOUT)?;
    if mic.enabled && !poll_until(|| engine.has_mic_output(), POLL_INTERVAL, MIC_START_TIMEOUT) {
        tracing::warn!(
            status = ?engine.mic_status(),
            "the virtual mic did not come up within {}s; playing on the monitor only",
            MIC_START_TIMEOUT.as_secs()
        );
        engine
            .set_mic(None)
            .context("failed to stop the virtual mic")?;
    }

    // Each running path applies the play once.
    let expected = 1 + u64::from(engine.has_mic_output());
    let baseline = engine.commands_applied();
    if !engine.play(sound, 1.0) {
        bail!("the engine rejected the play command: the output stream went away");
    }
    wait_for_playback(
        || {
            (
                engine.commands_applied().saturating_sub(baseline),
                engine.active_voices(),
            )
        },
        expected,
        POLL_INTERVAL,
        START_TIMEOUT,
        sound_duration(sound) + FINISH_MARGIN,
    )?;
    // The mixers report no voices once they have rendered the last samples,
    // but those have not reached the outputs yet; stopping now would cut them
    // off. The engine reports the larger latency of the two paths.
    thread::sleep(output_drain(engine.output_latency()));
    Ok(())
}

/// The mic settings of a headless play: `None` with the mic off, and never
/// a capture, so `play` does not open the microphone.
fn headless_mic_settings(mic: &MicConfig) -> Option<MicSettings> {
    mic_settings(mic, false)
}

/// Reports whether the virtual mic can work with the `[mic]` config and
/// returns the process exit code: non-zero when it cannot.
///
/// It reads the config file without creating it, resolves the devices
/// through [`probe_mic`] and prints the report. It opens no stream and plays
/// nothing; on Linux it does not create the virtual device.
pub fn run_mic_check() -> Result<ExitCode> {
    let (config, mic, notes) = cli_mic_config();
    let access = platform::microphone_access();
    // Probe the devices even with the mic off, so the setup can be checked
    // before turning it on.
    let settings = settings_for(&mic, access.allows_capture());
    let check = MicCheck {
        config,
        notes,
        mic,
        access,
        probe: probe_mic(&settings),
    };
    let report = format_mic_check(&check, platform::virtual_device_missing_hint());
    print!("{}", report.text);
    Ok(if report.available {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The `[mic]` table the CLI uses, from the config file at its usual path,
/// with notes on anything that made it fall back. The file is never created.
fn cli_mic_config() -> (Option<PathBuf>, MicConfig, Vec<String>) {
    match crate::paths::config_file() {
        Ok(path) => {
            let (mic, notes) = mic_config_at(&path);
            (Some(path), mic, notes)
        }
        Err(err) => (
            None,
            MicConfig::default(),
            vec![format!(
                "cannot locate the config file ({err:#}); using the defaults (mic off)"
            )],
        ),
    }
}

/// Reads the `[mic]` table of the config file at `path` without creating
/// it. A missing, unreadable or malformed file means the defaults (mic off),
/// with a note saying so; resolve warnings (clamped gains, a blank device)
/// are notes too.
fn mic_config_at(path: &Path) -> (MicConfig, Vec<String>) {
    let shown = path.display();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let note = format!("no config file at {shown}; using the defaults (mic off)");
            return (MicConfig::default(), vec![note]);
        }
        Err(err) => {
            let note = format!("cannot read {shown} ({err}); using the defaults (mic off)");
            return (MicConfig::default(), vec![note]);
        }
    };
    match crate::config::parse(&text) {
        Ok(raw) => {
            let mut notes = Vec::new();
            let mic = resolve_mic(&raw.mic, &mut notes);
            (mic, notes)
        }
        Err(err) => {
            let note = format!("{shown} is malformed ({err:#}); using the defaults (mic off)");
            (MicConfig::default(), vec![note])
        }
    }
}

/// Everything `mic-check` reports on.
#[derive(Debug)]
struct MicCheck {
    /// The config file, when its path could be determined.
    config: Option<PathBuf>,
    /// Why the config fell back or was adjusted.
    notes: Vec<String>,
    /// The resolved `[mic]` table.
    mic: MicConfig,
    /// The OS microphone permission.
    access: MicrophoneAccess,
    /// What the mic path would open.
    probe: MicProbe,
}

/// The `mic-check` output.
#[derive(Debug, PartialEq, Eq)]
struct MicReport {
    /// The report, one item per line.
    text: String,
    /// Whether the virtual mic can work: the virtual device is found and the
    /// microphone can be captured.
    available: bool,
}

/// Formats the `mic-check` report. Pure. `missing_hint` is the platform's
/// advice for a virtual device that is not found.
fn format_mic_check(check: &MicCheck, missing_hint: &str) -> MicReport {
    let mut text = String::new();
    let mut problems = Vec::new();
    let mut line = |s: String| {
        text.push_str(&s);
        text.push('\n');
    };

    line(match &check.config {
        Some(path) => format!("Config file: {}", path.display()),
        None => "Config file: unknown".to_string(),
    });
    for note in &check.notes {
        line(format!("  note: {note}"));
    }
    line(if check.mic.enabled {
        "Mic to call: on".to_string()
    } else {
        "Mic to call: off (turn it on from the tray, or set [mic] enabled = true)".to_string()
    });

    let pattern = &check.mic.virtual_device;
    line(match &check.mic.device_prefer {
        Some(prefer) => {
            format!("Virtual device pattern: {pattern:?} (prefers a {prefer:?} device)")
        }
        None => format!("Virtual device pattern: {pattern:?}"),
    });
    line(match &check.probe.sink {
        Ok(info) => format!("Virtual device: {}", describe(info)),
        Err(BuildError::DeviceNotFound(_)) => {
            problems.push(format!(
                "no output device matches {pattern:?}: {missing_hint}"
            ));
            "Virtual device: not found".to_string()
        }
        Err(err) => {
            problems.push(format!("cannot open the virtual device: {err}"));
            format!("Virtual device: error ({err})")
        }
    });

    line(match &check.probe.capture {
        None => "Microphone input: not opened (microphone access denied)".to_string(),
        Some(Ok(info)) => match &check.probe.excluded_default {
            Some(default) => format!(
                "Microphone input: {}, instead of the default input {default:?}, \
                 which is the virtual mic",
                describe(info)
            ),
            None => format!("Microphone input: {}", describe(info)),
        },
        Some(Err(BuildError::FeedbackExcluded(default))) => {
            problems.push(format!(
                "the default input {default:?} is the virtual mic and no physical input is \
                 known to fall back to: choose your microphone as the default input"
            ));
            format!("Microphone input: none (the default input {default:?} is the virtual mic)")
        }
        Some(Err(BuildError::NoDevice)) => {
            problems.push("there is no microphone input device".to_string());
            "Microphone input: none".to_string()
        }
        Some(Err(err)) => {
            problems.push(format!("cannot open the microphone: {err}"));
            format!("Microphone input: error ({err})")
        }
    });

    line(format!(
        "Microphone permission: {}",
        access_text(check.access)
    ));
    if check.access == MicrophoneAccess::Denied {
        problems.push(
            "microphone access is denied: allow Sound-Babound under System Settings > \
             Privacy & Security > Microphone; until then the call hears the sounds only"
                .to_string(),
        );
    }

    let available = problems.is_empty();
    line(if available {
        "Virtual mic: available".to_string()
    } else if check.probe.sink.is_ok() {
        "Virtual mic: sounds only, the microphone cannot be passed through".to_string()
    } else {
        "Virtual mic: unavailable".to_string()
    });
    for problem in problems {
        line(format!("  - {problem}"));
    }
    MicReport { text, available }
}

/// A device and its default format, for the report.
fn describe(info: &StreamInfo) -> String {
    let mut text = info.device_name.clone();
    let _ = write!(
        text,
        " ({} Hz, {} ch, {})",
        info.sample_rate, info.channels, info.sample_format
    );
    text
}

/// The microphone permission, for the report.
fn access_text(access: MicrophoneAccess) -> &'static str {
    match access {
        MicrophoneAccess::Granted => "granted",
        MicrophoneAccess::Denied => "denied",
        MicrophoneAccess::Undetermined => "not asked yet (the first capture asks)",
        MicrophoneAccess::Unknown => "unknown (readable from macOS 14); the capture is tried",
    }
}

/// How long to keep the stream up after the mixer has finished: the output
/// latency the engine measured until the last written frame is played
/// (backend buffering, device latency and the callback buffer itself, so
/// large buffers and wireless outputs are covered), capped, plus a margin.
fn output_drain(latency: Duration) -> Duration {
    latency.min(MAX_OUTPUT_LATENCY) + OUTPUT_DRAIN_MARGIN
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

/// How long `sound` plays at its own sample rate.
fn sound_duration(sound: &Sound) -> Duration {
    match sound.sample_rate() {
        0 => Duration::ZERO,
        rate => Duration::from_secs_f64(sound.frames() as f64 / f64::from(rate)),
    }
}

/// Polls `ready` every `poll` until it holds. Fails after `timeout`, which
/// means no output stream came up.
fn wait_for_output(ready: impl Fn() -> bool, poll: Duration, timeout: Duration) -> Result<()> {
    if !poll_until(ready, poll, timeout) {
        bail!(
            "no output stream within {}s: no usable output device",
            timeout.as_secs()
        );
    }
    Ok(())
}

/// Polls `ready` every `poll` until it holds. Returns false after `timeout`.
fn poll_until(ready: impl Fn() -> bool, poll: Duration, timeout: Duration) -> bool {
    let started = Instant::now();
    while !ready() {
        if started.elapsed() >= timeout {
            return false;
        }
        thread::sleep(poll);
    }
    true
}

/// Polls `probe` (returning `(commands_applied, active_voices)`, the command
/// count since the play was queued) every `poll` until every path has
/// applied the play (`expected` commands: one per running path) and no voice
/// is left on any path.
///
/// Fails if no path has applied the command within `start_timeout`, which
/// means no output stream came up, or if the paths are not done
/// `finish_timeout` after the first one applied it, which means an output
/// stopped mid-playback.
fn wait_for_playback(
    probe: impl Fn() -> (u64, usize),
    expected: u64,
    poll: Duration,
    start_timeout: Duration,
    finish_timeout: Duration,
) -> Result<()> {
    let started = Instant::now();
    let mut applied_at = None;
    loop {
        let (applied, voices) = probe();
        if applied >= 1 {
            if applied >= expected && voices == 0 {
                return Ok(());
            }
            let since = *applied_at.get_or_insert_with(Instant::now);
            if since.elapsed() >= finish_timeout {
                bail!(
                    "playback did not finish within {}s: the output device stopped",
                    finish_timeout.as_secs()
                );
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

    /// A finish timeout no test reaches.
    const LONG: Duration = Duration::from_secs(60);

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
    fn output_drain_follows_the_reported_latency() {
        assert_eq!(output_drain(Duration::ZERO), OUTPUT_DRAIN_MARGIN);
        // A wireless output with two seconds of latency is waited out.
        assert_eq!(
            output_drain(Duration::from_secs(2)),
            Duration::from_secs(2) + OUTPUT_DRAIN_MARGIN
        );
        assert_eq!(
            output_drain(Duration::from_secs(600)),
            MAX_OUTPUT_LATENCY + OUTPUT_DRAIN_MARGIN
        );
    }

    #[test]
    fn wait_for_output_returns_once_ready() {
        let calls = Cell::new(0);
        let ready = || {
            calls.set(calls.get() + 1);
            calls.get() >= 4
        };
        wait_for_output(ready, Duration::ZERO, LONG).unwrap();
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn wait_for_output_times_out_without_a_device() {
        let err = wait_for_output(
            || false,
            Duration::from_millis(1),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(err.to_string().contains("output device"), "{err}");
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
        wait_for_playback(probe, 1, Duration::ZERO, Duration::from_secs(60), LONG).unwrap();
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
        wait_for_playback(probe, 1, Duration::ZERO, Duration::from_secs(60), LONG).unwrap();
        assert_eq!(calls.get(), 5);
    }

    #[test]
    fn wait_times_out_when_command_is_never_applied() {
        let err = wait_for_playback(
            || (0, 0),
            1,
            Duration::from_millis(1),
            Duration::from_millis(20),
            LONG,
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
        wait_for_playback(probe, 1, Duration::from_millis(1), Duration::ZERO, LONG).unwrap();
        assert_eq!(calls.get(), 30);
    }

    #[test]
    fn wait_times_out_when_a_voice_never_finishes() {
        // The output went away mid-playback: the voice count never drops.
        let err = wait_for_playback(
            || (1, 1),
            1,
            Duration::from_millis(1),
            Duration::ZERO,
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
    }

    #[test]
    fn finish_timeout_counts_from_when_the_command_is_applied() {
        // Waiting for the stream to come up does not eat into the finish
        // timeout: the voice finishes 10 ms after the command is applied,
        // well inside a 200 ms finish timeout, after a 300 ms start wait.
        let started = Instant::now();
        let probe = || {
            let t = started.elapsed();
            if t < Duration::from_millis(300) {
                (0, 0)
            } else if t < Duration::from_millis(310) {
                (1, 1)
            } else {
                (1, 0)
            }
        };
        wait_for_playback(
            probe,
            1,
            Duration::from_millis(1),
            Duration::from_secs(60),
            Duration::from_millis(200),
        )
        .unwrap();
    }

    #[test]
    fn mic_check_subcommand() {
        let cli = Cli::try_parse_from(["sound-babound", "mic-check"]).unwrap();
        assert_eq!(cli.command, Some(Command::MicCheck));
    }

    #[test]
    fn mic_check_takes_no_arguments() {
        assert!(Cli::try_parse_from(["sound-babound", "mic-check", "extra"]).is_err());
    }

    #[test]
    fn mic_config_of_a_missing_file_is_the_default_with_a_note() {
        let dir = TempDir::new("mic-config-missing");
        let path = dir.path().join("config.toml");
        let (mic, notes) = mic_config_at(&path);
        assert_eq!(mic, MicConfig::default());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("no config file"), "{notes:?}");
        // Reading it does not create it.
        assert!(!path.exists());
    }

    #[test]
    fn mic_config_of_a_malformed_file_is_the_default_with_a_note() {
        let dir = TempDir::new("mic-config-malformed");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[mic\nenabled = true\n").unwrap();
        let (mic, notes) = mic_config_at(&path);
        assert_eq!(mic, MicConfig::default());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("malformed"), "{notes:?}");
    }

    #[test]
    fn mic_config_reads_the_mic_table() {
        let dir = TempDir::new("mic-config-valid");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mic]\nenabled = true\nvirtual_device = \"My Cable\"\nmic_gain = 0.5\n",
        )
        .unwrap();
        let (mic, notes) = mic_config_at(&path);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(mic.enabled);
        assert_eq!(mic.virtual_device, "My Cable");
        assert_eq!((mic.sound_gain, mic.mic_gain), (1.0, 0.5));
    }

    #[test]
    fn mic_config_resolve_warnings_are_notes() {
        let dir = TempDir::new("mic-config-warn");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[mic]\nvirtual_device = \"  \"\n").unwrap();
        let (mic, notes) = mic_config_at(&path);
        assert_eq!(mic.virtual_device, platform::default_virtual_device());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("virtual_device"), "{notes:?}");
    }

    const HINT: &str = "install the cable";

    fn info(name: &str, rate: u32, channels: u16) -> StreamInfo {
        StreamInfo {
            identity: babound_engine::DeviceIdentity::new(name),
            device_name: name.to_string(),
            sample_rate: rate,
            channels,
            sample_format: "f32".to_string(),
        }
    }

    fn check(probe: MicProbe, access: MicrophoneAccess) -> MicCheck {
        MicCheck {
            config: Some(PathBuf::from("/cfg/config.toml")),
            notes: Vec::new(),
            mic: MicConfig {
                enabled: true,
                virtual_device: "BlackHole".to_string(),
                device_prefer: None,
                ..MicConfig::default()
            },
            access,
            probe,
        }
    }

    fn found() -> MicProbe {
        MicProbe {
            sink: Ok(info("BlackHole 2ch", 48_000, 2)),
            capture: Some(Ok(info("Built-in Microphone", 44_100, 1))),
            excluded_default: None,
        }
    }

    #[test]
    fn report_of_a_working_setup() {
        let report = format_mic_check(&check(found(), MicrophoneAccess::Granted), HINT);
        assert!(report.available, "{}", report.text);
        let text = &report.text;
        assert!(text.contains("Config file: /cfg/config.toml"), "{text}");
        assert!(text.contains("Mic to call: on"), "{text}");
        assert!(text.contains("pattern: \"BlackHole\""), "{text}");
        assert!(
            text.contains("Virtual device: BlackHole 2ch (48000 Hz, 2 ch, f32)"),
            "{text}"
        );
        assert!(
            text.contains("Microphone input: Built-in Microphone (44100 Hz, 1 ch, f32)\n"),
            "{text}"
        );
        assert!(text.contains("Microphone permission: granted"), "{text}");
        assert!(text.contains("Virtual mic: available"), "{text}");
        assert!(!text.contains("  - "), "{text}");
    }

    #[test]
    fn the_pattern_line_names_the_preference() {
        let mut preferring = check(found(), MicrophoneAccess::Granted);
        preferring.mic.device_prefer = Some("2ch".into());
        let text = format_mic_check(&preferring, HINT).text;
        assert!(
            text.contains("Virtual device pattern: \"BlackHole\" (prefers a \"2ch\" device)\n"),
            "{text}"
        );

        let text = format_mic_check(&check(found(), MicrophoneAccess::Granted), HINT).text;
        assert!(
            text.contains("Virtual device pattern: \"BlackHole\"\n"),
            "{text}"
        );
    }

    #[test]
    fn report_of_a_missing_virtual_device() {
        let probe = MicProbe {
            sink: Err(BuildError::DeviceNotFound("BlackHole".into())),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(!report.available);
        let text = &report.text;
        assert!(text.contains("Virtual device: not found"), "{text}");
        assert!(text.contains("Virtual mic: unavailable"), "{text}");
        assert!(
            text.contains("  - no output device matches \"BlackHole\": install the cable"),
            "{text}"
        );
    }

    #[test]
    fn report_of_another_virtual_device_error() {
        let probe = MicProbe {
            sink: Err(BuildError::Backend("device busy".into())),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(!report.available);
        assert!(report.text.contains("device busy"), "{}", report.text);
        assert!(report.text.contains("Virtual mic: unavailable"));
    }

    #[test]
    fn report_of_a_fallback_from_the_excluded_default_input() {
        let probe = MicProbe {
            excluded_default: Some("Sound-Babound Mic".into()),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(report.available, "{}", report.text);
        assert!(
            report.text.contains(
                "Microphone input: Built-in Microphone (44100 Hz, 1 ch, f32), instead of \
                 the default input \"Sound-Babound Mic\", which is the virtual mic"
            ),
            "{}",
            report.text
        );
    }

    #[test]
    fn report_of_an_excluded_default_input_with_no_fallback() {
        let probe = MicProbe {
            capture: Some(Err(BuildError::FeedbackExcluded("BlackHole 2ch".into()))),
            excluded_default: Some("BlackHole 2ch".into()),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(!report.available);
        let text = &report.text;
        assert!(
            text.contains("Microphone input: none (the default input \"BlackHole 2ch\""),
            "{text}"
        );
        assert!(text.contains("choose your microphone"), "{text}");
        // The virtual device itself works: the call still hears the sounds.
        assert!(text.contains("Virtual mic: sounds only"), "{text}");
    }

    #[test]
    fn report_of_no_microphone() {
        let probe = MicProbe {
            capture: Some(Err(BuildError::NoDevice)),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(!report.available);
        assert!(report.text.contains("no microphone input device"));
    }

    #[test]
    fn report_of_a_denied_permission() {
        let probe = MicProbe {
            capture: None,
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Denied), HINT);
        assert!(!report.available);
        let text = &report.text;
        assert!(text.contains("Microphone permission: denied"), "{text}");
        assert!(
            text.contains("Microphone input: not opened (microphone access denied)"),
            "{text}"
        );
        assert!(text.contains("Privacy & Security > Microphone"), "{text}");
        assert!(text.contains("Virtual mic: sounds only"), "{text}");
    }

    #[test]
    fn report_of_an_undetermined_permission_is_available() {
        let report = format_mic_check(&check(found(), MicrophoneAccess::Undetermined), HINT);
        assert!(report.available);
        assert!(report.text.contains("not asked yet"), "{}", report.text);
    }

    #[test]
    fn report_of_a_disabled_mic_still_checks_the_devices() {
        let mut check = check(found(), MicrophoneAccess::Granted);
        check.mic.enabled = false;
        check.config = None;
        check.notes = vec!["no config file at /x; using the defaults (mic off)".into()];
        let report = format_mic_check(&check, HINT);
        assert!(report.available);
        let text = &report.text;
        assert!(text.contains("Config file: unknown"), "{text}");
        assert!(text.contains("  note: no config file at /x"), "{text}");
        assert!(text.contains("Mic to call: off"), "{text}");
        assert!(text.contains("Virtual device: BlackHole 2ch"), "{text}");
    }

    #[test]
    fn a_headless_play_never_captures_the_microphone() {
        assert_eq!(headless_mic_settings(&MicConfig::default()), None);
        let mic = MicConfig {
            enabled: true,
            ..MicConfig::default()
        };
        let settings = headless_mic_settings(&mic).unwrap();
        assert!(!settings.capture);
        assert_eq!(settings.device_match, mic.virtual_device);
        assert_eq!(settings.device_prefer, mic.device_prefer);
    }

    #[test]
    fn poll_until_returns_true_once_ready() {
        let calls = Cell::new(0);
        let ready = || {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        };
        assert!(poll_until(ready, Duration::ZERO, LONG));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn poll_until_gives_up_after_the_timeout() {
        let started = Instant::now();
        assert!(!poll_until(
            || false,
            Duration::from_millis(1),
            Duration::from_millis(20)
        ));
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn wait_for_playback_waits_for_every_path() {
        // Two paths: the monitor applies the play and finishes before the
        // mic sink has applied it; that is not the end.
        let states = [(1, 1), (1, 0), (1, 0), (2, 1), (2, 0)];
        let calls = Cell::new(0);
        let probe = || {
            let i = calls.get();
            calls.set(i + 1);
            states[i.min(states.len() - 1)]
        };
        wait_for_playback(probe, 2, Duration::ZERO, Duration::from_secs(60), LONG).unwrap();
        assert_eq!(calls.get(), states.len());
    }

    #[test]
    fn wait_for_playback_times_out_when_a_path_never_applies() {
        // The mic sink went away: only the monitor applied the play.
        let err = wait_for_playback(
            || (1, 0),
            2,
            Duration::from_millis(1),
            Duration::ZERO,
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not finish"), "{err}");
    }

    #[test]
    fn sound_duration_is_frames_over_rate() {
        let sound = load_play_sound(None).unwrap();
        let expected = sound.frames() as f64 / f64::from(sound.sample_rate());
        assert!((sound_duration(&sound).as_secs_f64() - expected).abs() < 1e-9);
        assert!(sound_duration(&sound) > Duration::ZERO);
    }
}
