//! macOS: CoreAudio is the only host, for every role.

/// The monitor's host: CoreAudio.
pub fn monitor_host() -> cpal::Host {
    cpal::default_host()
}

/// The mic paths' host: CoreAudio, which lists virtual cables such as
/// BlackHole as ordinary devices.
pub fn mic_host() -> cpal::Host {
    cpal::default_host()
}

/// The virtual device the real-device smoke test looks for.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "BlackHole";
