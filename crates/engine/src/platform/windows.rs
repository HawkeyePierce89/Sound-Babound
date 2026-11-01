//! Windows: WASAPI is the default host, for every role.

/// The monitor's host: WASAPI.
pub fn monitor_host() -> cpal::Host {
    cpal::default_host()
}

/// The mic paths' host: WASAPI, which lists virtual cables such as VB-CABLE
/// as ordinary endpoints.
pub fn mic_host() -> cpal::Host {
    cpal::default_host()
}

/// The virtual device the real-device smoke test looks for.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "CABLE Input";
