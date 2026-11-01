//! Linux: ALSA for the monitor, PulseAudio for the mic paths.
//!
//! With cpal's `pulseaudio` feature enabled, `cpal::default_host()` prefers
//! PulseAudio. The monitor stays on ALSA's `default` PCM, as before the mic
//! path existed; that PCM follows the PipeWire or PulseAudio default sink.
//! The mic paths need PulseAudio, because only it lists the app's null sink
//! and remapped source by name. PipeWire answers it through pipewire-pulse.

use std::sync::Arc;

use cpal::HostId;
use cpal::traits::HostTrait;

use super::shared_host::SharedHost;

/// The PulseAudio host every mic path shares. A dropped PulseAudio host
/// leaks its reactor thread and connection, so there is only this one.
static MIC_HOST: SharedHost<cpal::Host> = SharedHost::new();

/// The monitor's host: ALSA, or the default host if ALSA cannot open.
pub fn monitor_host() -> cpal::Host {
    cpal::host_from_id(HostId::Alsa).unwrap_or_else(|_| cpal::default_host())
}

/// The mic paths' host: the process-wide PulseAudio host, reconnected only
/// if its connection no longer answers (a query on a dead connection fails
/// at once), or ALSA if no PulseAudio server answers. The fallback is not
/// kept, so the next call tries PulseAudio again.
///
/// The fallback is ALSA by id, never cpal's default host: that tries
/// PulseAudio first, and a PulseAudio host outside [`MIC_HOST`] would leak
/// its reactor thread and connection when dropped. Only if ALSA cannot open
/// either is the default host used, and a PulseAudio host it returns is
/// kept in [`MIC_HOST`] like any other.
pub fn mic_host() -> Arc<cpal::Host> {
    let alive = |host: &cpal::Host| host.devices().is_ok();
    if let Some(host) = MIC_HOST.get(alive, || cpal::host_from_id(HostId::PulseAudio).ok()) {
        return host;
    }
    match cpal::host_from_id(HostId::Alsa) {
        Ok(host) => Arc::new(host),
        Err(_) => {
            let host = cpal::default_host();
            if host.id() == HostId::PulseAudio {
                MIC_HOST.adopt(host)
            } else {
                Arc::new(host)
            }
        }
    }
}

/// The mic paths' stream period: 10 ms. With the default buffer size cpal
/// lets the PulseAudio server choose, and PulseAudio and pipewire-pulse then
/// use about 2 s per fragment: the capture would arrive in 2 s blocks (far
/// past the passthrough's high mark) and the mic sink would ask for 2 s at a
/// time (an underrun every block).
pub const MIC_PERIOD_MS: Option<u32> = Some(10);

/// The virtual device the real-device smoke test looks for: the null sink
/// the app creates.
#[cfg(test)]
pub const SMOKE_MIC_PATTERN: &str = "sound_babound_sink";
