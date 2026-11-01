//! Linux: ALSA for the monitor, PulseAudio for the mic paths.
//!
//! With cpal's `pulseaudio` feature enabled, `cpal::default_host()` prefers
//! PulseAudio. The monitor stays on ALSA's `default` PCM, as before the mic
//! path existed; that PCM follows the PipeWire or PulseAudio default sink.
//! The mic paths need PulseAudio, because only it lists the app's null sink
//! and remapped source by name. PipeWire answers it through pipewire-pulse.

use cpal::HostId;

/// The monitor's host: ALSA, or the default host if ALSA cannot open.
pub fn monitor_host() -> cpal::Host {
    cpal::host_from_id(HostId::Alsa).unwrap_or_else(|_| cpal::default_host())
}

/// The mic paths' host: PulseAudio, or the default host if no PulseAudio
/// server answers.
pub fn mic_host() -> cpal::Host {
    cpal::host_from_id(HostId::PulseAudio).unwrap_or_else(|_| cpal::default_host())
}

/// The virtual device the real-device smoke test looks for: the null sink
/// the app creates.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "sound_babound_sink";
