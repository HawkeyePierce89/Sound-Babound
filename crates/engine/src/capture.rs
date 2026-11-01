//! The cpal capture: the microphone stream that feeds the passthrough.
//!
//! [`CpalCaptureFactory`] follows the system default input of
//! [`platform::mic_host`], re-queried on every build and every identity
//! query. It never captures the virtual mic device itself: when the default
//! input matches one of the exclusions (the virtual device or its monitor),
//! [`choose_input`] falls back to the most recent physical default input if
//! it is still present, and otherwise refuses the build with
//! [`BuildError::FeedbackExcluded`], so the supervisor retries with its
//! failure-log throttle until the default changes.
//!
//! Each build calls [`PassthroughShared::begin_capture`] with the stream's
//! rate before the stream starts. The data callback only downmixes into the
//! ring ([`PassthroughShared::write`]), which follows the realtime rules.

use std::sync::Arc;
use std::sync::mpsc::Sender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{I24, SampleFormat, StreamConfig, U24};
use tracing::{debug, warn};

use crate::convert::InputSample;
use crate::cpal_sink::{
    CpalStream, OPEN_TIMEOUT, build_error, device_labels, forward_errors, identity_of,
    name_matches, stream_info,
};
use crate::error::BuildError;
use crate::passthrough::PassthroughShared;
use crate::platform;
use crate::sink::{BuiltStream, DeviceIdentity, StreamFactory, StreamInfo, StreamRole};
use crate::supervisor::SupervisorMsg;

/// An input device as [`choose_input`] sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputCandidate {
    /// The device identity (backend id, or name).
    pub identity: DeviceIdentity,
    /// The display name.
    pub name: String,
    /// The backend id string, when there is one.
    pub id: Option<String>,
}

impl InputCandidate {
    fn of(device: &cpal::Device) -> InputCandidate {
        let (name, id) = device_labels(device);
        InputCandidate {
            identity: identity_of(device),
            name,
            id,
        }
    }

    /// Whether any of `exclusions` names this device.
    fn is_excluded(&self, exclusions: &[String]) -> bool {
        exclusions
            .iter()
            .any(|pattern| name_matches(pattern, &self.name, self.id.as_deref()))
    }
}

/// Which input a capture build uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputChoice {
    /// The default input, which is a physical device.
    Default,
    /// The default input is excluded; use `available[index]`, the most
    /// recent physical default input, which is still present.
    Fallback(usize),
    /// The default input is excluded and no physical input to fall back to
    /// is present.
    Excluded,
    /// There is no default input.
    NoDevice,
}

/// Pick the input a capture build uses. Pure.
///
/// - A default input that no exclusion names is used as is.
/// - A default input an exclusion names (the virtual device or its monitor)
///   falls back to `last_physical` if `available` still lists it and no
///   exclusion names it either.
/// - Otherwise the choice is [`InputChoice::Excluded`].
pub fn choose_input(
    default: Option<&InputCandidate>,
    available: &[InputCandidate],
    last_physical: Option<&DeviceIdentity>,
    exclusions: &[String],
) -> InputChoice {
    let Some(default) = default else {
        return InputChoice::NoDevice;
    };
    if !default.is_excluded(exclusions) {
        return InputChoice::Default;
    }
    last_physical
        .and_then(|last| {
            available
                .iter()
                .position(|c| &c.identity == last && !c.is_excluded(exclusions))
        })
        .map_or(InputChoice::Excluded, InputChoice::Fallback)
}

/// A capture decision worth a warning: the default input was replaced or
/// refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputNotice {
    /// The excluded default `excluded` was replaced by `using`.
    Fallback {
        excluded: DeviceIdentity,
        using: DeviceIdentity,
    },
    /// The excluded default `excluded` was refused, with nothing to fall
    /// back to.
    Excluded { excluded: DeviceIdentity },
}

/// Decides when a capture decision is logged at warn: once per change.
///
/// Builds repeat (every retry, every rebuild), and the same fallback or
/// refusal must not warn on each of them. A build that uses the default
/// input again resets it, so the next fallback or refusal warns again.
#[derive(Debug, Default)]
pub struct NoticeLog {
    last: Option<InputNotice>,
}

impl NoticeLog {
    /// Record the decision of one build (`None` when the default input was
    /// used). Returns true if it should be logged at warn.
    pub fn observe(&mut self, notice: Option<InputNotice>) -> bool {
        let warn = notice.is_some() && notice != self.last;
        self.last = notice;
        warn
    }
}

/// What [`CpalCaptureFactory::select`] found.
enum Selection {
    /// Capture from this device. Boxed: a cpal device is large on Linux.
    Device(Box<cpal::Device>),
    /// The default input is excluded and there is no fallback.
    Excluded(InputCandidate),
}

/// Builds cpal capture streams on the default input, minus the exclusions.
#[derive(Debug)]
pub struct CpalCaptureFactory {
    exclusions: Vec<String>,
    /// The most recent default input no exclusion named.
    last_physical: Option<DeviceIdentity>,
    notices: NoticeLog,
}

impl CpalCaptureFactory {
    /// A capture that never opens a device any of `exclusions` names (a
    /// case-insensitive substring of the display name or id).
    pub fn new(exclusions: Vec<String>) -> CpalCaptureFactory {
        CpalCaptureFactory {
            exclusions,
            last_physical: None,
            notices: NoticeLog::default(),
        }
    }

    /// Resolve the device and its default config without building a stream,
    /// for `mic-check`. It logs nothing.
    pub fn probe(&mut self) -> Result<StreamInfo, BuildError> {
        match self.select()?.0 {
            Selection::Device(device) => {
                let supported = device.default_input_config().map_err(build_error)?;
                Ok(stream_info(&device, &supported))
            }
            Selection::Excluded(default) => Err(BuildError::FeedbackExcluded(default.name)),
        }
    }

    /// Re-query the default input and the input list, and apply
    /// [`choose_input`]. Remembers the default input as the last physical
    /// one when no exclusion names it. Returns the selection and the notice
    /// it deserves.
    fn select(&mut self) -> Result<(Selection, Option<InputNotice>), BuildError> {
        let host = platform::mic_host();
        let default_device = host.default_input_device();
        let default = default_device.as_ref().map(InputCandidate::of);
        // Listing is only needed to find a fallback.
        let mut devices = Vec::new();
        if default
            .as_ref()
            .is_some_and(|d| d.is_excluded(&self.exclusions))
        {
            match host.input_devices() {
                Ok(list) => devices.extend(list),
                Err(err) => debug!(error = %err, "cannot list input devices"),
            }
        }
        let available: Vec<InputCandidate> = devices.iter().map(InputCandidate::of).collect();

        let choice = choose_input(
            default.as_ref(),
            &available,
            self.last_physical.as_ref(),
            &self.exclusions,
        );
        match (choice, default_device, default) {
            (InputChoice::Default, Some(device), Some(default)) => {
                self.last_physical = Some(default.identity);
                Ok((Selection::Device(Box::new(device)), None))
            }
            (InputChoice::Fallback(index), _, Some(default)) => {
                let notice = InputNotice::Fallback {
                    excluded: default.identity,
                    using: available[index].identity.clone(),
                };
                let device = devices.swap_remove(index);
                Ok((Selection::Device(Box::new(device)), Some(notice)))
            }
            (InputChoice::Excluded, _, Some(default)) => {
                let notice = InputNotice::Excluded {
                    excluded: default.identity.clone(),
                };
                Ok((Selection::Excluded(default), Some(notice)))
            }
            _ => Err(BuildError::NoDevice),
        }
    }

    /// Log a fallback or refusal at warn, once per change.
    fn log_notice(&mut self, notice: Option<InputNotice>) {
        if !self.notices.observe(notice.clone()) {
            return;
        }
        match notice {
            Some(InputNotice::Fallback { excluded, using }) => warn!(
                excluded = %excluded,
                using = %using,
                "default input is the virtual mic device; capturing the last physical input"
            ),
            Some(InputNotice::Excluded { excluded }) => warn!(
                excluded = %excluded,
                "default input is the virtual mic device and no physical input is known; \
                 capture refused"
            ),
            None => {}
        }
    }
}

impl StreamFactory for CpalCaptureFactory {
    type Target = Arc<PassthroughShared>;

    fn role(&self) -> StreamRole {
        StreamRole::Capture
    }

    fn build(
        &mut self,
        passthrough: Arc<PassthroughShared>,
        on_error: Sender<SupervisorMsg>,
    ) -> Result<BuiltStream, BuildError> {
        let (selection, notice) = self.select()?;
        self.log_notice(notice);
        let device = match selection {
            Selection::Device(device) => device,
            Selection::Excluded(default) => {
                return Err(BuildError::FeedbackExcluded(default.name));
            }
        };

        let supported = device.default_input_config().map_err(build_error)?;
        let info = stream_info(&device, &supported);
        let sample_format = supported.sample_format();
        let config = supported.config();

        // No stream exists yet, so nothing writes the ring while it resets.
        passthrough.begin_capture(config.sample_rate);

        let p = passthrough;
        let e = on_error;
        let stream = match sample_format {
            SampleFormat::F32 => build_typed::<f32>(&device, &config, p, e),
            SampleFormat::F64 => build_typed::<f64>(&device, &config, p, e),
            SampleFormat::I8 => build_typed::<i8>(&device, &config, p, e),
            SampleFormat::I16 => build_typed::<i16>(&device, &config, p, e),
            SampleFormat::I24 => build_typed::<I24>(&device, &config, p, e),
            SampleFormat::I32 => build_typed::<i32>(&device, &config, p, e),
            SampleFormat::I64 => build_typed::<i64>(&device, &config, p, e),
            SampleFormat::U8 => build_typed::<u8>(&device, &config, p, e),
            SampleFormat::U16 => build_typed::<u16>(&device, &config, p, e),
            SampleFormat::U24 => build_typed::<U24>(&device, &config, p, e),
            SampleFormat::U32 => build_typed::<u32>(&device, &config, p, e),
            SampleFormat::U64 => build_typed::<u64>(&device, &config, p, e),
            // DSD bitstreams are not PCM and cannot be read as samples.
            other => return Err(BuildError::UnsupportedFormat(other.to_string())),
        }?;
        stream.play().map_err(build_error)?;
        Ok((Box::new(CpalStream { _stream: stream }), info))
    }

    fn current_identity(&mut self) -> Option<DeviceIdentity> {
        match self.select().ok()?.0 {
            Selection::Device(device) => Some(identity_of(&device)),
            Selection::Excluded(_) => None,
        }
    }
}

fn build_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    passthrough: Arc<PassthroughShared>,
    on_error: Sender<SupervisorMsg>,
) -> Result<cpal::Stream, BuildError>
where
    T: InputSample + Send + 'static,
{
    let channels = usize::from(config.channels);
    device
        .build_input_stream::<T, _, _>(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| passthrough.write(data, channels),
            forward_errors(on_error),
            Some(OPEN_TIMEOUT),
        )
        .map_err(build_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> InputCandidate {
        InputCandidate {
            identity: DeviceIdentity::new(format!("id:{name}")),
            name: name.to_string(),
            id: Some(format!("id:{name}")),
        }
    }

    fn exclusions() -> Vec<String> {
        vec![
            "Sound-Babound Mic".into(),
            "sound_babound_sink.monitor".into(),
        ]
    }

    #[test]
    fn a_physical_default_is_used() {
        let mic = input("MacBook Pro Microphone");
        let choice = choose_input(Some(&mic), &[], None, &exclusions());
        assert_eq!(choice, InputChoice::Default);
    }

    #[test]
    fn the_virtual_default_falls_back_to_the_known_physical_input() {
        let virtual_mic = input("Sound-Babound Mic");
        let available = [virtual_mic.clone(), input("USB Headset"), input("Webcam")];
        let last = DeviceIdentity::new("id:Webcam");
        let choice = choose_input(Some(&virtual_mic), &available, Some(&last), &exclusions());
        assert_eq!(choice, InputChoice::Fallback(2));
    }

    #[test]
    fn the_virtual_monitor_with_no_known_physical_input_is_excluded() {
        let monitor = InputCandidate {
            identity: DeviceIdentity::new("pulseaudio:sound_babound_sink.monitor"),
            name: "Monitor of Sound-Babound".into(),
            id: Some("pulseaudio:sound_babound_sink.monitor".into()),
        };
        let available = [monitor.clone(), input("USB Headset")];
        let choice = choose_input(Some(&monitor), &available, None, &exclusions());
        assert_eq!(choice, InputChoice::Excluded);
    }

    #[test]
    fn a_vanished_physical_input_is_not_a_fallback() {
        let virtual_mic = input("Sound-Babound Mic");
        let available = [virtual_mic.clone(), input("USB Headset")];
        let last = DeviceIdentity::new("id:Webcam");
        let choice = choose_input(Some(&virtual_mic), &available, Some(&last), &exclusions());
        assert_eq!(choice, InputChoice::Excluded);
    }

    #[test]
    fn an_excluded_last_input_is_not_a_fallback() {
        let virtual_mic = input("Sound-Babound Mic");
        let available = [virtual_mic.clone()];
        let last = virtual_mic.identity.clone();
        let choice = choose_input(Some(&virtual_mic), &available, Some(&last), &exclusions());
        assert_eq!(choice, InputChoice::Excluded);
    }

    #[test]
    fn no_default_input_is_no_device() {
        let available = [input("USB Headset")];
        let last = DeviceIdentity::new("id:USB Headset");
        let choice = choose_input(None, &available, Some(&last), &exclusions());
        assert_eq!(choice, InputChoice::NoDevice);
    }

    #[test]
    fn exclusions_match_case_insensitively_by_name_or_id() {
        let by_name = input("sound-babound MIC");
        assert!(by_name.is_excluded(&exclusions()));
        let by_id = InputCandidate {
            identity: DeviceIdentity::new("x"),
            name: "Monitor of Null Output".into(),
            id: Some("pulseaudio:SOUND_BABOUND_SINK.monitor".into()),
        };
        assert!(by_id.is_excluded(&exclusions()));
        assert!(!input("USB Headset").is_excluded(&exclusions()));
        // Blank patterns exclude nothing.
        assert!(!input("USB Headset").is_excluded(&[String::new(), "  ".into()]));
    }

    #[test]
    fn notices_warn_once_per_change() {
        let fallback = |using: &str| InputNotice::Fallback {
            excluded: DeviceIdentity::new("virtual"),
            using: DeviceIdentity::new(using),
        };
        let excluded = InputNotice::Excluded {
            excluded: DeviceIdentity::new("virtual"),
        };
        let mut log = NoticeLog::default();
        assert!(!log.observe(None), "the default input is never a warning");
        assert!(log.observe(Some(fallback("webcam"))));
        assert!(!log.observe(Some(fallback("webcam"))), "a repeat is quiet");
        assert!(
            log.observe(Some(fallback("headset"))),
            "a new fallback warns"
        );
        assert!(log.observe(Some(excluded.clone())));
        assert!(!log.observe(Some(excluded.clone())), "retries are quiet");
        assert!(!log.observe(None));
        assert!(
            log.observe(Some(excluded)),
            "after the default was usable, the refusal warns again"
        );
    }
}
