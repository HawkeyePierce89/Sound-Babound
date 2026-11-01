//! macOS: CoreAudio is the only host, for every role.

use std::sync::Arc;

/// The monitor's host: CoreAudio.
pub fn monitor_host() -> Option<Arc<cpal::Host>> {
    Some(Arc::new(cpal::default_host()))
}

/// The mic paths' host: CoreAudio, which lists virtual cables such as
/// BlackHole as ordinary devices.
pub fn mic_host() -> Option<Arc<cpal::Host>> {
    Some(Arc::new(cpal::default_host()))
}

/// The mic paths' stream period: 10 ms. CoreAudio's default is the device's
/// current buffer size, which a DAW or another app may have set to 1024–4096
/// frames (20–90 ms at 48 kHz); asking for 10 ms keeps the passthrough's
/// latency low. `mic_buffer_size` clamps the request to the device's range,
/// so a device that cannot go that low gets its smallest buffer, and the
/// passthrough's adaptive target covers whatever block size results.
pub const MIC_PERIOD_MS: Option<u32> = Some(10);

/// The virtual device the real-device smoke test looks for.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "BlackHole";
