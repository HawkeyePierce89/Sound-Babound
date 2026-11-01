//! macOS glue.

use objc2_avf_audio::{AVAudioApplication, AVAudioApplicationRecordPermission};
use tao::event_loop::EventLoop;
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::TrayIconBuilder;

use super::{HotkeyKeys, MicrophoneAccess};

/// No-op: a macOS binary always keeps the console it was launched from.
pub fn attach_parent_console() {}

/// Runs the app as an accessory: a menu bar item with no Dock icon and no
/// app menu.
pub fn prepare_event_loop<T: 'static>(event_loop: &mut EventLoop<T>) {
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
}

/// Sets a black tray icon as a template image, so the menu bar recolors it
/// for light and dark appearance.
pub fn with_tray_icon(builder: TrayIconBuilder) -> anyhow::Result<TrayIconBuilder> {
    Ok(builder.with_icon_templated(crate::icon::tray_icon([0, 0, 0])?))
}

/// The mic sink's device when the config names none: BlackHole, matched by
/// name, so "BlackHole 2ch" and "BlackHole 16ch" both qualify. When several
/// are installed, [`default_virtual_device_preference`] picks the 2ch one.
pub fn default_virtual_device() -> &'static str {
    "BlackHole"
}

/// Which of the devices [`default_virtual_device`] names the mic sink
/// prefers: "BlackHole 2ch", the one the setup hint tells the user to
/// install and pick in the call app, over "BlackHole 16ch" or "64ch".
pub fn default_virtual_device_preference() -> Option<&'static str> {
    Some("2ch")
}

/// The inputs the capture must never open. BlackHole's input carries the
/// same name as its output, so the pattern alone covers it.
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string()]
}

/// What the user does to get the virtual microphone.
pub fn virtual_mic_hint() -> &'static str {
    "install BlackHole 2ch, then choose \"BlackHole 2ch\" as the microphone in the call app"
}

/// The tray status when no output matches the virtual device pattern.
pub fn virtual_mic_not_found() -> &'static str {
    "not found, install BlackHole"
}

/// What `mic-check` suggests when no output matches the virtual device
/// pattern.
pub fn virtual_device_missing_hint() -> &'static str {
    "install BlackHole 2ch (or set [mic] virtual_device to the output of the virtual cable you use)"
}

/// The app's microphone permission. `AVAudioApplication` exists from
/// macOS 14; before that the permission cannot be read and is `Unknown`.
/// Reading it never prompts; opening the capture does.
pub fn microphone_access() -> MicrophoneAccess {
    if !objc2::available!(macos = 14.0) {
        return MicrophoneAccess::Unknown;
    }
    // SAFETY: the class exists on macOS 14 and later (checked above), and
    // `sharedInstance` and `recordPermission` have no preconditions.
    let permission = unsafe { AVAudioApplication::sharedInstance().recordPermission() };
    access_of(permission)
}

/// Maps AVFAudio's record permission onto [`MicrophoneAccess`].
fn access_of(permission: AVAudioApplicationRecordPermission) -> MicrophoneAccess {
    match permission {
        AVAudioApplicationRecordPermission::Granted => MicrophoneAccess::Granted,
        AVAudioApplicationRecordPermission::Denied => MicrophoneAccess::Denied,
        AVAudioApplicationRecordPermission::Undetermined => MicrophoneAccess::Undetermined,
        _ => MicrophoneAccess::Unknown,
    }
}

/// Which key a hotkey names: global-hotkey registers a macOS hotkey by virtual key code, which is a
/// key position (`KeyA` is the key where a US layout has A).
pub fn hotkey_keys() -> HotkeyKeys {
    HotkeyKeys::Position
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_defaults_target_blackhole() {
        assert_eq!(default_virtual_device(), "BlackHole");
        assert_eq!(default_virtual_device_preference(), Some("2ch"));
        assert_eq!(virtual_input_exclusions("BlackHole"), ["BlackHole"]);
        assert!(virtual_mic_hint().contains("BlackHole"));
        assert!(virtual_mic_not_found().contains("install BlackHole"));
    }

    #[test]
    fn record_permission_maps_to_access() {
        use AVAudioApplicationRecordPermission as P;
        assert_eq!(access_of(P::Granted), MicrophoneAccess::Granted);
        assert_eq!(access_of(P::Denied), MicrophoneAccess::Denied);
        assert_eq!(access_of(P::Undetermined), MicrophoneAccess::Undetermined);
        assert_eq!(access_of(P(0)), MicrophoneAccess::Unknown);
    }

    #[test]
    fn microphone_access_reads_without_prompting() {
        // Only reads the recorded permission: no prompt, no audio. Any
        // answer is fine, as long as the lookup itself works.
        let _ = microphone_access();
    }
}
