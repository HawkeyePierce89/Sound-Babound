//! Windows: WASAPI is the default host, for every role.

use std::sync::Arc;

/// The monitor's host: WASAPI.
pub fn monitor_host() -> Option<Arc<cpal::Host>> {
    Some(Arc::new(cpal::default_host()))
}

/// The mic paths' host: WASAPI, which lists virtual cables such as VB-CABLE
/// as ordinary endpoints.
pub fn mic_host() -> Option<Arc<cpal::Host>> {
    Some(Arc::new(cpal::default_host()))
}

/// The mic paths' stream period: WASAPI's default shared-mode period
/// (10 ms) suits the passthrough.
pub const MIC_PERIOD_MS: Option<u32> = None;

/// The virtual device the real-device smoke test looks for.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "CABLE Input";
