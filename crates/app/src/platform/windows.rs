//! Windows glue.

use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

/// Attaches to the console of the parent process, if it has one.
///
/// The binary is built for the GUI subsystem so tray mode opens no console
/// window. In CLI mode this makes stdout, stderr and log output visible in the
/// terminal the app was started from. Failure (no parent console, or already
/// attached) is harmless and ignored.
pub fn attach_parent_console() {
    // SAFETY: AttachConsole takes a process id by value and has no memory
    // safety preconditions.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// The mic sink's device when the config names none: the playback side of
/// VB-CABLE.
pub fn default_virtual_device() -> &'static str {
    "CABLE Input"
}

/// No preference: the default pattern names one device.
pub fn default_virtual_device_preference() -> Option<&'static str> {
    None
}

/// The inputs the capture must never open, since they carry what the mic
/// sink plays: the pattern, VB-CABLE's recording side, and the recording
/// side of the cable the pattern names (see [`recording_side`]).
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    let mut exclusions = vec![pattern.to_string(), "CABLE Output".to_string()];
    if let Some(recording) = recording_side(pattern).filter(|r| !exclusions.contains(r)) {
        exclusions.push(recording);
    }
    exclusions
}

/// The recording side of the virtual cable whose playback side `pattern`
/// names. Windows cables name the two sides "<cable> Input" and "<cable>
/// Output" ("CABLE-A Input", "VoiceMeeter Input"), so the last "input"
/// (in any case) becomes "Output". `None` when the pattern has no "input".
fn recording_side(pattern: &str) -> Option<String> {
    const INPUT: &str = "input";
    // ASCII lowercasing keeps every byte offset.
    let at = pattern.to_ascii_lowercase().rfind(INPUT)?;
    Some(format!(
        "{}Output{}",
        &pattern[..at],
        &pattern[at + INPUT.len()..]
    ))
}

/// What the user does to get the virtual microphone.
pub fn virtual_mic_hint() -> &'static str {
    "install VB-CABLE, then choose \"CABLE Output\" as the microphone in the call app"
}

/// The tray status when no output matches the virtual device pattern.
pub fn virtual_mic_not_found() -> &'static str {
    "not found, install VB-CABLE"
}

/// What `mic-check` suggests when no output matches the virtual device
/// pattern.
pub fn virtual_device_missing_hint() -> &'static str {
    "install VB-CABLE (or set [mic] virtual_device to the output of the virtual cable you use)"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_defaults_target_vb_cable() {
        assert_eq!(default_virtual_device(), "CABLE Input");
        assert_eq!(default_virtual_device_preference(), None);
        assert_eq!(
            virtual_input_exclusions("CABLE Input"),
            ["CABLE Input", "CABLE Output"]
        );
        assert!(virtual_mic_hint().contains("CABLE Output"));
        assert!(virtual_mic_not_found().contains("install VB-CABLE"));
    }

    #[test]
    fn a_custom_cable_excludes_its_recording_side() {
        assert_eq!(
            virtual_input_exclusions("CABLE-A Input"),
            ["CABLE-A Input", "CABLE Output", "CABLE-A Output"]
        );
        assert_eq!(
            virtual_input_exclusions("voicemeeter input (VB-Audio)"),
            [
                "voicemeeter input (VB-Audio)",
                "CABLE Output",
                "voicemeeter Output (VB-Audio)"
            ]
        );
        assert_eq!(
            virtual_input_exclusions("Speakers"),
            ["Speakers", "CABLE Output"]
        );
    }
}
