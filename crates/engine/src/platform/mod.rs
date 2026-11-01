//! The cpal host each stream role opens its devices on.
//!
//! Each OS has its own submodule, picked by `cfg(target_os)` here; shared code
//! only calls the functions re-exported from this module.
//!
//! - [`monitor_host`]: the host the monitor output has always used.
//! - [`mic_host`]: the host for the mic sink and the capture, which must see
//!   the virtual device by name.
//! - [`MIC_PERIOD_MS`]: the period the mic paths' streams ask for, if the
//!   host's default does not suit the passthrough.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::{MIC_PERIOD_MS, mic_host, monitor_host};
#[cfg(target_os = "macos")]
pub use macos::{MIC_PERIOD_MS, mic_host, monitor_host};
#[cfg(target_os = "windows")]
pub use windows::{MIC_PERIOD_MS, mic_host, monitor_host};

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

/// Every other OS: the host's default period.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub const MIC_PERIOD_MS: Option<u32> = None;

/// Every other OS has no known virtual device.
#[cfg(all(
    test,
    not(any(target_os = "linux", target_os = "macos", target_os = "windows"))
))]
pub const SMOKE_MIC_PATTERN: &str = "";
