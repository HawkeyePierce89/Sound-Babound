//! The virtual mic status report: what `mic-check` prints and the settings
//! window shows. It reads the `[mic]` table without creating the config
//! file, resolves the devices without opening a stream and formats the
//! result as text.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use babound_engine::{BuildError, MicProbe, MicSettings, StreamInfo, probe_mic};

use crate::actions::settings_for;
use crate::config::MicConfig;
use crate::hotkeys::resolve_mic;
use crate::platform::{self, MicrophoneAccess};

/// The status of the virtual mic with the config file at `path`, for the
/// settings window's background refresher: the `[mic]` table (read without
/// creating the file), the microphone permission and the devices the mic
/// path would open. It opens no stream, and it probes the devices even with
/// the mic off, so the setup can be checked before turning it on.
pub fn status_check(path: &Path) -> MicCheck {
    let (mic, notes) = mic_config_at(path);
    check_with(
        Some(path.to_path_buf()),
        mic,
        notes,
        platform::microphone_access(),
        probe_mic,
    )
}

/// Builds the check of `mic` (from the config file at `config`, with
/// `notes`) under `access`: `probe` resolves the mic settings `mic` would
/// use, whether or not it is enabled, with the capture only when `access`
/// allows it.
pub fn check_with(
    config: Option<PathBuf>,
    mic: MicConfig,
    notes: Vec<String>,
    access: MicrophoneAccess,
    probe: impl FnOnce(&MicSettings) -> MicProbe,
) -> MicCheck {
    let settings = settings_for(&mic, access.allows_capture());
    MicCheck {
        config,
        notes,
        probe: probe(&settings),
        mic,
        access,
    }
}

/// Reads the `[mic]` table of the config file at `path` without creating
/// it. A missing, unreadable or malformed file means the defaults (mic off),
/// with a note saying so; resolve warnings (clamped gains, a blank device)
/// are notes too.
pub fn mic_config_at(path: &Path) -> (MicConfig, Vec<String>) {
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

/// Everything `mic-check` and the settings window report on.
#[derive(Debug)]
pub struct MicCheck {
    /// The config file, when its path could be determined.
    pub config: Option<PathBuf>,
    /// Why the config fell back or was adjusted.
    pub notes: Vec<String>,
    /// The resolved `[mic]` table.
    pub mic: MicConfig,
    /// The OS microphone permission.
    pub access: MicrophoneAccess,
    /// What the mic path would open.
    pub probe: MicProbe,
}

/// The `mic-check` report, also shown by the settings window.
#[derive(Debug, PartialEq, Eq)]
pub struct MicReport {
    /// The report, one item per line.
    pub text: String,
    /// Whether the virtual mic can work: the virtual device is found and the
    /// microphone can be captured.
    pub available: bool,
}

/// Formats the `mic-check` report. Pure. `missing_hint` is the platform's
/// advice for a virtual device that is not found.
pub fn format_mic_check(check: &MicCheck, missing_hint: &str) -> MicReport {
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
            format!(
                "Microphone input: none (the default input {default:?} is the virtual mic; \
                 the tray's capture stays on the last physical input it used, if there was one)"
            )
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
pub fn describe(info: &StreamInfo) -> String {
    let mut text = info.device_name.clone();
    let _ = write!(
        text,
        " ({} Hz, {} ch, {})",
        info.sample_rate, info.channels, info.sample_format
    );
    text
}

/// The microphone permission, for the report.
pub fn access_text(access: MicrophoneAccess) -> &'static str {
    match access {
        MicrophoneAccess::Granted => "granted",
        MicrophoneAccess::Denied => "denied",
        MicrophoneAccess::Undetermined => "not asked yet (the first capture asks)",
        MicrophoneAccess::Unknown => "unknown (readable from macOS 14); the capture is tried",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::test_support::TempDir;

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
        assert!(
            text.contains(
                "the virtual mic; the tray's capture stays on the last physical input it used, \
                 if there was one)\n"
            ),
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
    fn report_of_another_capture_error() {
        let probe = MicProbe {
            capture: Some(Err(BuildError::Backend("input busy".into()))),
            ..found()
        };
        let report = format_mic_check(&check(probe, MicrophoneAccess::Granted), HINT);
        assert!(!report.available);
        let text = &report.text;
        assert!(
            text.contains("Microphone input: error (") && text.contains("input busy"),
            "{text}"
        );
        assert!(text.contains("  - cannot open the microphone: "), "{text}");
        assert!(text.contains("Virtual mic: sounds only"), "{text}");
    }

    #[test]
    fn report_names_every_permission() {
        for (access, shown, available) in [
            (MicrophoneAccess::Granted, "granted", true),
            (MicrophoneAccess::Denied, "denied", false),
            (
                MicrophoneAccess::Undetermined,
                "not asked yet (the first capture asks)",
                true,
            ),
            (
                MicrophoneAccess::Unknown,
                "unknown (readable from macOS 14); the capture is tried",
                true,
            ),
        ] {
            let report = format_mic_check(&check(found(), access), HINT);
            assert!(
                report
                    .text
                    .contains(&format!("Microphone permission: {shown}\n")),
                "{access:?}: {}",
                report.text
            );
            assert_eq!(report.available, available, "{access:?}: {}", report.text);
        }
    }

    #[test]
    fn a_check_probes_the_settings_of_the_mic_table() {
        let mic = MicConfig {
            enabled: false,
            virtual_device: "My Cable".to_string(),
            device_prefer: Some("2ch".to_string()),
            ..MicConfig::default()
        };
        let seen = RefCell::new(None);
        let check = check_with(
            Some(PathBuf::from("/cfg/config.toml")),
            mic.clone(),
            vec!["a note".to_string()],
            MicrophoneAccess::Granted,
            |settings| {
                *seen.borrow_mut() = Some(settings.clone());
                found()
            },
        );
        // A disabled mic is probed too, with the configured pattern.
        let settings = seen.into_inner().unwrap();
        assert_eq!(settings, settings_for(&mic, true));
        assert!(settings.capture);
        assert_eq!(check.config, Some(PathBuf::from("/cfg/config.toml")));
        assert_eq!(check.notes, vec!["a note".to_string()]);
        assert_eq!(check.mic, mic);
        assert_eq!(check.access, MicrophoneAccess::Granted);
        assert!(check.probe.sink.is_ok());
    }

    #[test]
    fn a_check_under_a_denied_permission_probes_no_capture() {
        let seen = RefCell::new(None);
        let check = check_with(
            None,
            MicConfig::default(),
            Vec::new(),
            MicrophoneAccess::Denied,
            |settings| {
                *seen.borrow_mut() = Some(settings.capture);
                MicProbe {
                    capture: None,
                    ..found()
                }
            },
        );
        assert_eq!(seen.into_inner(), Some(false));
        let report = format_mic_check(&check, HINT);
        assert!(
            report.text.contains("Config file: unknown"),
            "{}",
            report.text
        );
        assert!(
            report
                .text
                .contains("Microphone input: not opened (microphone access denied)"),
            "{}",
            report.text
        );
    }

    #[test]
    fn a_check_of_a_file_reads_its_mic_table() {
        let dir = TempDir::new("mic-report-check");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[mic]\nenabled = true\nvirtual_device = \"Cable\"\n").unwrap();
        let (mic, notes) = mic_config_at(&path);
        let check = check_with(
            Some(path.clone()),
            mic,
            notes,
            MicrophoneAccess::Granted,
            |_| found(),
        );
        let report = format_mic_check(&check, HINT);
        assert!(report.available, "{}", report.text);
        assert!(report.text.contains("Mic to call: on"), "{}", report.text);
        assert!(
            report.text.contains("Virtual device pattern: \"Cable\""),
            "{}",
            report.text
        );
    }
}
