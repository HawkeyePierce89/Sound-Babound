//! Decoded, in-memory sounds.
//!
//! A [`Sound`] is decoded once, up front, into interleaved `f32` samples at the
//! file's native sample rate and channel count. Resampling and channel mapping
//! happen in the mixer, so the same `Sound` plays correctly after the output
//! device (and its rate) changes.

use std::fmt;
use std::fs::File;
use std::io::{Cursor, ErrorKind};
use std::path::Path;
use std::sync::Arc;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

use crate::error::LoadError;

/// The decoded audio behind a [`Sound`].
#[derive(Debug)]
pub struct SoundData {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: u16,
    frames: usize,
    label: String,
}

impl SoundData {
    /// Interleaved samples, `frames * channels` long.
    pub fn samples(&self) -> &[f32] {
        &self.samples
    }

    /// Native sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Native channel count (at least 1).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Number of frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Display label (file path or the label passed to `load_bytes`).
    pub fn label(&self) -> &str {
        &self.label
    }
}

/// A decoded sound. Cloning is cheap: it only bumps a reference count.
#[derive(Clone)]
pub struct Sound(Arc<SoundData>);

impl Sound {
    /// Decode a whole file (WAV, FLAC, OGG/Vorbis or MP3).
    pub fn load_file(path: impl AsRef<Path>) -> Result<Sound, LoadError> {
        let path = path.as_ref();
        let label = path.display().to_string();
        let file = File::open(path).map_err(|source| LoadError::Io {
            label: label.clone(),
            source,
        })?;
        decode(Box::new(file), hint_for(path), label)
    }

    /// Decode a whole in-memory file. `label` is used in errors and the UI; if
    /// it ends in a file extension, that extension is used as a format hint.
    pub fn load_bytes(
        bytes: impl Into<Vec<u8>>,
        label: impl Into<String>,
    ) -> Result<Sound, LoadError> {
        let label = label.into();
        let hint = hint_for(Path::new(&label));
        decode(Box::new(Cursor::new(bytes.into())), hint, label)
    }

    /// Build a sound from already-decoded interleaved samples.
    ///
    /// Trailing samples that do not fill a whole frame are discarded.
    pub fn from_interleaved(
        samples: Vec<f32>,
        sample_rate: u32,
        channels: u16,
        label: impl Into<String>,
    ) -> Result<Sound, LoadError> {
        let label = label.into();
        if channels == 0 || sample_rate == 0 {
            return Err(LoadError::Unsupported {
                label,
                reason: format!("invalid stream spec: {sample_rate} Hz, {channels} channels"),
            });
        }
        let mut samples = samples;
        let frames = samples.len() / usize::from(channels);
        samples.truncate(frames * usize::from(channels));
        if frames == 0 {
            return Err(LoadError::Empty { label });
        }
        Ok(Sound(Arc::new(SoundData {
            samples,
            sample_rate,
            channels,
            frames,
            label,
        })))
    }

    /// Whether two handles point at the same decoded data.
    pub fn ptr_eq(a: &Sound, b: &Sound) -> bool {
        Arc::ptr_eq(&a.0, &b.0)
    }

    /// Number of live handles to this sound's data.
    pub fn handle_count(&self) -> usize {
        Arc::strong_count(&self.0)
    }
}

impl std::ops::Deref for Sound {
    type Target = SoundData;

    fn deref(&self) -> &SoundData {
        &self.0
    }
}

impl fmt::Debug for Sound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sound")
            .field("label", &self.label)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("frames", &self.frames)
            .finish()
    }
}

fn hint_for(path: &Path) -> Hint {
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    hint
}

fn is_eof(err: &SymphoniaError) -> bool {
    matches!(err, SymphoniaError::IoError(e) if e.kind() == ErrorKind::UnexpectedEof)
}

/// Why decoding stopped before the end of the stream, if it did.
enum Stop {
    /// The underlying source failed to read.
    Io(std::io::Error),
    /// The stream could not be demuxed or decoded further.
    Other(String),
}

fn decode(source: Box<dyn MediaSource>, hint: Hint, label: String) -> Result<Sound, LoadError> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut format = match symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) {
        Ok(format) => format,
        Err(SymphoniaError::IoError(source)) if source.kind() != ErrorKind::UnexpectedEof => {
            return Err(LoadError::Io { label, source });
        }
        Err(err) => {
            return Err(LoadError::Unsupported {
                label,
                reason: err.to_string(),
            });
        }
    };

    let Some(track) = format.default_track(TrackType::Audio) else {
        return Err(LoadError::NoAudioTrack { label });
    };
    let track_id = track.id;
    let Some(params) = track.codec_params.as_ref().and_then(|p| p.audio()) else {
        return Err(LoadError::NoAudioTrack { label });
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|err| LoadError::Unsupported {
            label: label.clone(),
            reason: err.to_string(),
        })?;

    let mut samples: Vec<f32> = Vec::new();
    let mut packet_samples: Vec<f32> = Vec::new();
    // (rate, channels) of the first decoded buffer; later buffers must match.
    let mut spec: Option<(u32, usize)> = None;
    let mut stop: Option<Stop> = None;
    let mut skipped = 0usize;
    let mut last_skip_reason = String::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(err) if is_eof(&err) => break,
            Err(SymphoniaError::IoError(e)) => {
                stop = Some(Stop::Io(e));
                break;
            }
            Err(err) => {
                stop = Some(Stop::Other(err.to_string()));
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                if buf.frames() == 0 {
                    continue;
                }
                let this_spec = (buf.spec().rate(), buf.spec().channels().count());
                match spec {
                    None => spec = Some(this_spec),
                    Some(s) if s != this_spec => {
                        skipped += 1;
                        last_skip_reason = "stream spec changed mid-stream".into();
                        continue;
                    }
                    Some(_) => {}
                }
                buf.copy_to_vec_interleaved(&mut packet_samples);
                samples.extend_from_slice(&packet_samples);
            }
            // Recoverable: a single corrupt packet. Skip it and keep going.
            Err(SymphoniaError::DecodeError(reason)) => {
                skipped += 1;
                last_skip_reason = reason.to_string();
            }
            Err(err) if is_eof(&err) => break,
            Err(SymphoniaError::IoError(e)) => {
                stop = Some(Stop::Io(e));
                break;
            }
            Err(err) => {
                stop = Some(Stop::Other(err.to_string()));
                break;
            }
        }
    }

    if skipped > 0 {
        tracing::debug!(%label, skipped, reason = %last_skip_reason, "skipped undecodable packets");
    }

    let Some((rate, channels)) = spec.filter(|_| !samples.is_empty()) else {
        return Err(match stop {
            Some(Stop::Io(source)) => LoadError::Io { label, source },
            Some(Stop::Other(reason)) => LoadError::Decode { label, reason },
            None if skipped > 0 => LoadError::Decode {
                label,
                reason: last_skip_reason,
            },
            None => LoadError::Empty { label },
        });
    };

    if let Some(stop) = stop {
        let reason = match stop {
            Stop::Io(e) => e.to_string(),
            Stop::Other(r) => r,
        };
        tracing::debug!(%label, %reason, "decoding stopped early; keeping decoded audio");
    }

    let channels = u16::try_from(channels).map_err(|_| LoadError::Unsupported {
        label: label.clone(),
        reason: format!("{channels} channels"),
    })?;
    Sound::from_interleaved(samples, rate, channels, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal 16-bit PCM WAV file.
    fn wav_bytes(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let block_align = channels * 2;
        let mut out = Vec::with_capacity(44 + samples.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn ramp(len: usize) -> Vec<i16> {
        (0..len)
            .map(|i| ((i as i32 * 37) % 65_536 - 32_768) as i16)
            .collect()
    }

    #[test]
    fn mono_44k_decodes() {
        let input = ramp(4410);
        let sound = Sound::load_bytes(wav_bytes(44_100, 1, &input), "mono.wav").unwrap();
        assert_eq!(sound.sample_rate(), 44_100);
        assert_eq!(sound.channels(), 1);
        assert_eq!(sound.frames(), 4410);
        assert_eq!(sound.samples().len(), 4410);
        assert_eq!(sound.label(), "mono.wav");
    }

    #[test]
    fn stereo_48k_decodes() {
        let input = ramp(2 * 4800);
        let sound = Sound::load_bytes(wav_bytes(48_000, 2, &input), "stereo.wav").unwrap();
        assert_eq!(sound.sample_rate(), 48_000);
        assert_eq!(sound.channels(), 2);
        assert_eq!(sound.frames(), 4800);
        assert_eq!(sound.samples().len(), 9600);
    }

    #[test]
    fn sample_values_round_trip() {
        let input: Vec<i16> = vec![0, 16_384, -16_384, 32_767, -32_768, 1000, -1000, 0];
        let sound = Sound::load_bytes(wav_bytes(44_100, 2, &input), "values.wav").unwrap();
        assert_eq!(sound.frames(), 4);
        for (got, want) in sound.samples().iter().zip(&input) {
            let want = f32::from(*want) / 32_768.0;
            assert!((got - want).abs() < 1e-3, "got {got}, want {want}");
        }
        // Interleaving is preserved: frame 0 is (L=0, R=0.5), frame 1 starts with L=-0.5.
        assert!(sound.samples()[0].abs() < 1e-3);
        assert!((sound.samples()[1] - 0.5).abs() < 1e-3);
        assert!((sound.samples()[2] + 0.5).abs() < 1e-3);
    }

    #[test]
    fn decodes_without_extension_hint() {
        let sound = Sound::load_bytes(wav_bytes(22_050, 1, &ramp(100)), "no hint").unwrap();
        assert_eq!(sound.frames(), 100);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        let garbage: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        assert!(Sound::load_bytes(garbage.clone(), "garbage.wav").is_err());
        assert!(Sound::load_bytes(garbage, "garbage").is_err());
        assert!(Sound::load_bytes(Vec::new(), "empty-file.wav").is_err());
        assert!(Sound::load_bytes(b"RIFF".to_vec(), "truncated.wav").is_err());
    }

    #[test]
    fn empty_data_chunk_is_empty() {
        let err = Sound::load_bytes(wav_bytes(44_100, 1, &[]), "silent.wav").unwrap_err();
        assert!(matches!(err, LoadError::Empty { .. }), "got {err:?}");
        assert_eq!(err.label(), "silent.wav");
    }

    #[test]
    fn missing_file_is_io() {
        let path = std::env::temp_dir().join("babound-definitely-missing-file.wav");
        let err = Sound::load_file(&path).unwrap_err();
        assert!(matches!(err, LoadError::Io { .. }), "got {err:?}");
        assert!(
            err.to_string()
                .contains("babound-definitely-missing-file.wav")
        );
    }

    #[test]
    fn load_file_decodes() {
        let path =
            std::env::temp_dir().join(format!("babound-sound-test-{}.wav", std::process::id()));
        std::fs::write(&path, wav_bytes(48_000, 2, &ramp(960))).unwrap();
        let sound = Sound::load_file(&path);
        std::fs::remove_file(&path).unwrap();
        let sound = sound.unwrap();
        assert_eq!(sound.frames(), 480);
        assert_eq!(sound.label(), path.display().to_string());
    }

    #[test]
    fn committed_demo_decodes() {
        let bytes = include_bytes!("../../app/assets/demo.wav");
        let sound = Sound::load_bytes(&bytes[..], "builtin:demo").unwrap();
        assert_eq!(sound.sample_rate(), 44_100);
        assert_eq!(sound.channels(), 1);
        // ~1.5 s.
        assert!(
            (60_000..=70_000).contains(&sound.frames()),
            "{}",
            sound.frames()
        );
        let peak = sound.samples().iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "peak {peak}");
    }

    #[test]
    fn clone_is_shared() {
        let a = Sound::from_interleaved(vec![0.0; 8], 48_000, 2, "a").unwrap();
        let b = a.clone();
        assert!(Sound::ptr_eq(&a, &b));
        assert_eq!(a.handle_count(), 2);
        drop(b);
        assert_eq!(a.handle_count(), 1);
    }

    /// Serves `bytes` until absolute position `fail_at`, then every read fails
    /// with a non-EOF I/O error.
    struct FailingSource {
        inner: Cursor<Vec<u8>>,
        fail_at: u64,
    }

    impl std::io::Read for FailingSource {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let pos = self.inner.position();
            if pos >= self.fail_at {
                return Err(std::io::Error::other("device went away"));
            }
            let room = usize::try_from(self.fail_at - pos).unwrap_or(usize::MAX);
            let len = buf.len().min(room);
            self.inner.read(&mut buf[..len])
        }
    }

    impl std::io::Seek for FailingSource {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    impl MediaSource for FailingSource {
        fn is_seekable(&self) -> bool {
            true
        }

        fn byte_len(&self) -> Option<u64> {
            Some(self.inner.get_ref().len() as u64)
        }
    }

    fn decode_failing(bytes: Vec<u8>, fail_at: u64) -> Result<Sound, LoadError> {
        let source = FailingSource {
            inner: Cursor::new(bytes),
            fail_at,
        };
        decode(
            Box::new(source),
            hint_for(Path::new("x.wav")),
            "x.wav".into(),
        )
    }

    #[test]
    fn io_error_before_any_audio_is_io() {
        // A failure while probing the very first bytes reads as "no format";
        // once the RIFF header is recognized, the I/O error is reported.
        for fail_at in [12, 44] {
            let err = decode_failing(wav_bytes(44_100, 1, &ramp(44_100)), fail_at).unwrap_err();
            assert!(
                matches!(err, LoadError::Io { .. }),
                "{fail_at}: got {err:?}"
            );
            assert_eq!(err.label(), "x.wav");
        }
    }

    #[test]
    fn io_error_mid_stream_keeps_decoded_audio() {
        // 4 s of mono audio; the source dies about halfway through.
        let total = 4 * 44_100;
        let bytes = wav_bytes(44_100, 1, &ramp(total));
        let fail_at = 44 + total as u64;
        let sound = decode_failing(bytes, fail_at).unwrap();
        assert!(sound.frames() > 0, "nothing was kept");
        assert!(sound.frames() < total, "{} frames", sound.frames());
    }

    #[test]
    fn truncated_data_chunk_keeps_what_is_there() {
        let mut bytes = wav_bytes(44_100, 1, &ramp(4410));
        // Claim twice as much data as the file holds (RIFF size kept consistent).
        let claimed = 2 * 4410u32 * 2;
        bytes[4..8].copy_from_slice(&(36 + claimed).to_le_bytes());
        bytes[40..44].copy_from_slice(&claimed.to_le_bytes());
        let sound = Sound::load_bytes(bytes, "short.wav").unwrap();
        assert!(
            sound.frames() > 0 && sound.frames() <= 4410,
            "{}",
            sound.frames()
        );
    }

    #[test]
    fn codec_without_decoder_is_unsupported() {
        // A WAV header for IMA ADPCM, a codec this build does not enable.
        let block_align: u16 = 256;
        let data = vec![0u8; usize::from(block_align) * 4];
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(4 + 8 + 20 + 8 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&20u32.to_le_bytes());
        bytes.extend_from_slice(&0x0011u16.to_le_bytes()); // IMA ADPCM
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&22_050u32.to_le_bytes());
        bytes.extend_from_slice(&11_100u32.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes()); // cbSize
        bytes.extend_from_slice(&505u16.to_le_bytes()); // samples per block
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&data);
        let err = Sound::load_bytes(bytes, "adpcm.wav").unwrap_err();
        assert!(matches!(err, LoadError::Unsupported { .. }), "got {err:?}");
        assert_eq!(err.label(), "adpcm.wav");
    }

    #[test]
    fn debug_shows_the_stream_spec_not_the_samples() {
        let s = Sound::from_interleaved(vec![0.25; 6], 48_000, 2, "dbg").unwrap();
        let text = format!("{s:?}");
        for part in ["\"dbg\"", "48000", "channels: 2", "frames: 3"] {
            assert!(text.contains(part), "{text}");
        }
        assert!(!text.contains("0.25"), "{text}");
    }

    #[test]
    fn from_interleaved_validates() {
        let s = Sound::from_interleaved(vec![0.1; 5], 44_100, 2, "odd").unwrap();
        assert_eq!(s.frames(), 2);
        assert_eq!(s.samples().len(), 4);
        assert!(matches!(
            Sound::from_interleaved(vec![], 44_100, 1, "e"),
            Err(LoadError::Empty { .. })
        ));
        assert!(matches!(
            Sound::from_interleaved(vec![0.0; 4], 44_100, 0, "c"),
            Err(LoadError::Unsupported { .. })
        ));
        assert!(matches!(
            Sound::from_interleaved(vec![0.0; 4], 0, 1, "r"),
            Err(LoadError::Unsupported { .. })
        ));
    }
}
