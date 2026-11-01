//! The cpal host each stream role opens its devices on.
//!
//! Each OS has its own submodule, picked by `cfg(target_os)` here; shared code
//! only calls the functions re-exported from this module.
//!
//! - [`monitor_host`]: the host the monitor output has always used.
//! - [`mic_host`]: the host for the mic sink and the capture, which must see
//!   the virtual device by name.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{mic_host, monitor_host};
#[cfg(target_os = "macos")]
pub use macos::{mic_host, monitor_host};
#[cfg(target_os = "windows")]
pub use windows::{mic_host, monitor_host};

#[cfg(all(test, target_os = "linux"))]
pub use linux::SMOKE_MIC_PATTERN;
#[cfg(all(test, target_os = "macos"))]
pub use macos::SMOKE_MIC_PATTERN;
#[cfg(all(test, target_os = "windows"))]
pub use windows::SMOKE_MIC_PATTERN;

/// Every other OS: cpal's default host for every role.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn monitor_host() -> cpal::Host {
    cpal::default_host()
}

/// Every other OS: cpal's default host for every role.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn mic_host() -> cpal::Host {
    cpal::default_host()
}

/// Every other OS has no known virtual device.
#[cfg(all(
    test,
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
pub const SMOKE_MIC_PATTERN: &str = "";
