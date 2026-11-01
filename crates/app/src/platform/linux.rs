//! Linux glue.

/// No-op: a Linux binary always keeps the console it was launched from.
pub fn attach_parent_console() {}

/// Whether the desktop session is Wayland, judged by `XDG_SESSION_TYPE`
/// and `WAYLAND_DISPLAY`.
pub fn is_wayland_session() -> bool {
    super::is_wayland_env(|key| std::env::var_os(key))
}

/// Global hotkeys are X11 key grabs, which Wayland does not deliver to
/// other applications, so they are unsupported on Wayland.
pub fn global_hotkeys_supported() -> bool {
    !is_wayland_session()
}

/// The mic sink's device when the config names none: the null sink the app
/// creates through `pactl`.
pub fn default_virtual_device() -> &'static str {
    "sound_babound_sink"
}

/// The inputs the capture must never open: the pattern (which also matches
/// the null sink's monitor) and the remapped source that call apps record.
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string(), "sound_babound_mic".to_string()]
}

/// What the user does to get the virtual microphone.
#[allow(dead_code)] // For the tray mic status.
pub fn virtual_mic_hint() -> &'static str {
    "choose \"Sound-Babound Mic\" as the microphone in the call app"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_defaults_target_the_pactl_device() {
        assert_eq!(default_virtual_device(), "sound_babound_sink");
        assert_eq!(
            virtual_input_exclusions("sound_babound_sink"),
            ["sound_babound_sink", "sound_babound_mic"]
        );
        assert!(virtual_mic_hint().contains("Sound-Babound Mic"));
    }
}
