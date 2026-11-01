//! Platform-specific glue for the app.
//!
//! Each OS has its own submodule, picked by `cfg(target_os)` here; shared code
//! only calls the functions re-exported from this module.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{
    attach_parent_console, default_virtual_device, global_hotkeys_supported,
    virtual_input_exclusions,
};
#[cfg(target_os = "macos")]
pub use macos::{
    attach_parent_console, default_virtual_device, prepare_event_loop, virtual_input_exclusions,
    with_tray_icon,
};
#[cfg(target_os = "windows")]
pub use windows::{attach_parent_console, default_virtual_device, virtual_input_exclusions};

/// Fallback for every other OS: nothing to attach.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn attach_parent_console() {}

/// Fallback for every other OS: no known virtual cable, so the user names
/// the device in the config. A blank pattern matches no device.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn default_virtual_device() -> &'static str {
    ""
}

/// Fallback for every other OS: only the configured pattern is excluded.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string()]
}

/// Fallback for every other OS.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[allow(dead_code)] // For the tray mic status.
pub fn virtual_mic_hint() -> &'static str {
    "set [mic] virtual_device to a virtual audio cable's playback device"
}

/// Everywhere but macOS the tray app needs no event loop tweaks.
#[cfg(not(target_os = "macos"))]
pub fn prepare_event_loop<T: 'static>(_event_loop: &mut tao::event_loop::EventLoop<T>) {}

/// Template icons are a macOS concept. Elsewhere the icon is drawn in a
/// neutral gray that stays visible on light and dark taskbars.
#[cfg(not(target_os = "macos"))]
pub fn with_tray_icon(
    builder: tray_icon::TrayIconBuilder,
) -> anyhow::Result<tray_icon::TrayIconBuilder> {
    Ok(builder.with_icon(crate::icon::tray_icon([0x9e, 0x9e, 0x9e])?))
}

/// Global hotkeys work everywhere except Linux Wayland sessions, which only
/// the Linux module can detect.
#[cfg(not(target_os = "linux"))]
pub fn global_hotkeys_supported() -> bool {
    true
}

/// Whether the session described by `var` (an environment lookup) is a
/// Wayland session.
///
/// `XDG_SESSION_TYPE` decides when it is set to a known type; otherwise a
/// non-empty `WAYLAND_DISPLAY` means Wayland. Kept here, free of OS calls, so
/// the detection is testable on every OS.
#[cfg(any(target_os = "linux", test))]
fn is_wayland_env(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    let session = var("XDG_SESSION_TYPE").map(|v| v.to_string_lossy().trim().to_ascii_lowercase());
    match session.as_deref() {
        Some("wayland") => true,
        Some("x11") | Some("tty") => false,
        _ => var("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn attach_parent_console_is_safe_to_call_twice() {
        attach_parent_console();
        attach_parent_console();
    }

    #[test]
    fn virtual_input_exclusions_start_with_the_pattern() {
        let exclusions = virtual_input_exclusions("My Cable");
        assert_eq!(exclusions.first().map(String::as_str), Some("My Cable"));
        let defaults = virtual_input_exclusions(default_virtual_device());
        assert_eq!(defaults[0], default_virtual_device());
    }

    #[test]
    fn wayland_session_type_means_wayland() {
        assert!(is_wayland_env(env(&[("XDG_SESSION_TYPE", "wayland")])));
        assert!(is_wayland_env(env(&[("XDG_SESSION_TYPE", " Wayland ")])));
        assert!(is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "wayland"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ])));
    }

    #[test]
    fn x11_session_type_wins_over_wayland_display() {
        assert!(!is_wayland_env(env(&[("XDG_SESSION_TYPE", "x11")])));
        assert!(!is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "x11"),
            ("WAYLAND_DISPLAY", "wayland-0"),
        ])));
    }

    #[test]
    fn wayland_display_decides_without_a_known_session_type() {
        assert!(is_wayland_env(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        assert!(is_wayland_env(env(&[
            ("XDG_SESSION_TYPE", "unspecified"),
            ("WAYLAND_DISPLAY", "wayland-1"),
        ])));
        assert!(!is_wayland_env(env(&[("WAYLAND_DISPLAY", "")])));
        assert!(!is_wayland_env(env(&[])));
    }
}
