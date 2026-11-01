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
