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

/// The inputs the capture must never open: the pattern and VB-CABLE's
/// recording side, which carries what the mic sink plays.
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string(), "CABLE Output".to_string()]
}

/// What the user does to get the virtual microphone.
#[allow(dead_code)] // For the tray mic status.
pub fn virtual_mic_hint() -> &'static str {
    "install VB-CABLE, then choose \"CABLE Output\" as the microphone in the call app"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_defaults_target_vb_cable() {
        assert_eq!(default_virtual_device(), "CABLE Input");
        assert_eq!(
            virtual_input_exclusions("CABLE Input"),
            ["CABLE Input", "CABLE Output"]
        );
        assert!(virtual_mic_hint().contains("CABLE Output"));
    }
}
