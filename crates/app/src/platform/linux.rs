//! Linux glue.

use std::io::ErrorKind;
use std::process::Command;

use anyhow::{Context, bail};

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

/// The mic sink's device when the config names none: the null sink the app
/// creates through `pactl`.
pub fn default_virtual_device() -> &'static str {
    super::PACTL_SINK_NAME
}

/// The inputs the capture must never open: the pattern (which also matches
/// the null sink's monitor) and the remapped source that call apps record.
pub fn virtual_input_exclusions(pattern: &str) -> Vec<String> {
    vec![pattern.to_string(), "sound_babound_mic".to_string()]
}

/// What the user does to get the virtual microphone.
pub fn virtual_mic_hint() -> &'static str {
    "choose \"Sound-Babound Mic\" as the microphone in the call app"
}

/// The tray status when no output matches the virtual device pattern: the
/// app creates the device itself, so creating it failed (the log says why).
pub fn virtual_mic_not_found() -> &'static str {
    "could not create it, see log"
}

/// What `mic-check` suggests when no output matches the virtual device
/// pattern.
pub fn virtual_device_missing_hint() -> &'static str {
    "the tray creates it while the mic is on; mic-check does not, so start the tray with the mic on (or set [mic] virtual_device to an existing output)"
}

/// Runs `pactl` with `args` and returns its stdout. A missing `pactl` and a
/// non-zero exit are errors; the latter carries pactl's stderr.
fn run_pactl(args: &[String]) -> anyhow::Result<String> {
    let output = match Command::new("pactl").args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            bail!("pactl not found: install pulseaudio-utils (or pipewire-pulse's pactl)")
        }
        Err(err) => return Err(err).context("could not run pactl"),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "pactl {} failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The real `pactl` runner a [`VirtualMic`] holds.
type PactlRunner = fn(&[String]) -> anyhow::Result<String>;

/// The virtual microphone "Sound-Babound Mic": a null sink the mic sink
/// plays into and a source remapped from its monitor, created through
/// `pactl` (so it works on PulseAudio and pipewire-pulse alike).
///
/// Dropping it unloads both modules, the source first.
pub struct VirtualMic {
    _device: super::PactlDevice<PactlRunner>,
}

impl VirtualMic {
    /// Unloads modules a crashed run left behind, then loads the null sink and
    /// the remap source.
    pub fn create() -> anyhow::Result<Self> {
        let run: PactlRunner = run_pactl;
        super::PactlDevice::create(run)
            .map(|device| Self { _device: device })
            .context("could not create the virtual mic through pactl")
    }
}

/// Creates the virtual microphone. Always `Some` on success: Linux is the
/// one OS where the app makes the device itself.
pub fn create_virtual_mic() -> anyhow::Result<Option<VirtualMic>> {
    VirtualMic::create().map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_defaults_target_the_pactl_device() {
        assert_eq!(default_virtual_device(), "sound_babound_sink");
        assert_eq!(
            virtual_input_exclusions("sound_babound_sink"),
            ["sound_babound_sink", "sound_babound_mic"]
        );
        assert!(virtual_mic_hint().contains("Sound-Babound Mic"));
        assert!(virtual_mic_not_found().contains("see log"));
    }
}
