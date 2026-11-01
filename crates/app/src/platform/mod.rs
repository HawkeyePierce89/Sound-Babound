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
pub use linux::attach_parent_console;
#[cfg(target_os = "macos")]
pub use macos::attach_parent_console;
#[cfg(target_os = "windows")]
pub use windows::attach_parent_console;

/// Fallback for every other OS: nothing to attach.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn attach_parent_console() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_parent_console_is_safe_to_call_twice() {
        attach_parent_console();
        attach_parent_console();
    }
}
