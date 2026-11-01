//! The realtime mixer.
//!
//! [`Mixer::render`] runs inside the audio callback, so it follows the
//! realtime rules: no allocation, no freeing, no blocking locks, no I/O and no
//! logging. Control threads talk to it through [`MixerShared`], a lock-free
//! command queue plus a few atomics.
//!
//! # Releasing sounds
//!
//! A [`Sound`] is an `Arc`. Dropping the last handle would free the decoded
//! samples on the realtime thread, so the mixer never drops a `Sound` in
//! `render`. Every sound leaving a voice goes into the lock-free garbage queue,
//! which the supervisor thread empties with [`MixerShared::drain_garbage`]. If
//! the garbage queue is full, the sound is parked in a fixed pending-release
//! slot and retried at the start of the next render.
//!
//! Commands are only popped while at least [`MAX_VOICES`] pending-release
//! slots are free. One command releases at most `MAX_VOICES` sounds (a
//! `StopAll`), so every release triggered by a command always finds a slot.
//! A voice that finishes while no slot is free stays in its voice slot, marked
//! finished and silent, and is released on a later render.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;

use crate::sound::Sound;

/// Maximum number of simultaneously playing voices.
pub const MAX_VOICES: usize = 32;
/// Number of fixed pending-release slots.
pub const PENDING_RELEASE: usize = 2 * MAX_VOICES;
/// Capacity of the command queue.
pub const COMMAND_CAPACITY: usize = 64;
/// Capacity of the garbage queue.
pub const GARBAGE_CAPACITY: usize = 128;
/// Frames in the scratch buffer allocated by [`Mixer::configure`].
pub const SCRATCH_FRAMES: usize = 1024;

/// A control message for the mixer.
#[derive(Debug)]
pub enum Command {
    /// Start a new voice. Plays of the same sound overlap.
    Play { sound: Sound, gain: f32 },
    /// Stop every voice.
    StopAll,
    /// Set the gain applied on top of every voice's own gain.
    SetMasterGain(f32),
}

/// State shared between the mixer and the control threads.
pub struct MixerShared {
    commands: ArrayQueue<Command>,
    garbage: ArrayQueue<Sound>,
    active_voices: AtomicUsize,
    commands_applied: AtomicU64,
    callback_heartbeat: AtomicU64,
}

impl MixerShared {
    /// Create the shared state with empty queues and zeroed counters.
    pub fn new() -> MixerShared {
        MixerShared {
            commands: ArrayQueue::new(COMMAND_CAPACITY),
            garbage: ArrayQueue::new(GARBAGE_CAPACITY),
            active_voices: AtomicUsize::new(0),
            commands_applied: AtomicU64::new(0),
            callback_heartbeat: AtomicU64::new(0),
        }
    }

    /// Queue a command for the next render. Never blocks.
    ///
    /// If the queue is full the command is handed back in `Err`.
    pub fn push(&self, command: Command) -> Result<(), Command> {
        self.commands.push(command)
    }

    /// Drop every sound the mixer released. Call it off the realtime thread.
    ///
    /// Returns the number of sounds dropped.
    pub fn drain_garbage(&self) -> usize {
        let mut dropped = 0;
        while let Some(sound) = self.garbage.pop() {
            drop(sound);
            dropped += 1;
        }
        dropped
    }

    /// Voices that are currently audible (as of the last render).
    pub fn active_voices(&self) -> usize {
        self.active_voices.load(Ordering::Acquire)
    }

    /// Total number of commands the mixer has applied.
    pub fn commands_applied(&self) -> u64 {
        self.commands_applied.load(Ordering::Acquire)
    }

    /// Incremented on every render; the supervisor watches it for stalls.
    pub fn callback_heartbeat(&self) -> u64 {
        self.callback_heartbeat.load(Ordering::Acquire)
    }
}

impl Default for MixerShared {
    fn default() -> Self {
        MixerShared::new()
    }
}

impl std::fmt::Debug for MixerShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MixerShared")
            .field("queued_commands", &self.commands.len())
            .field("queued_garbage", &self.garbage.len())
            .field("active_voices", &self.active_voices())
            .field("commands_applied", &self.commands_applied())
            .field("callback_heartbeat", &self.callback_heartbeat())
            .finish()
    }
}

/// One playing sound.
#[derive(Debug)]
struct Voice {
    sound: Sound,
    gain: f32,
    /// Read position in source frames.
    position: f64,
    /// Start order; the lowest is evicted first.
    seq: u64,
    /// Reached the end but could not be released yet.
    finished: bool,
}

/// The realtime mixer. See the module docs for the release rule.
#[derive(Debug)]
pub struct Mixer {
    shared: Arc<MixerShared>,
    voices: [Option<Voice>; MAX_VOICES],
    pending: [Option<Sound>; PENDING_RELEASE],
    pending_used: usize,
    next_seq: u64,
    master_gain: f32,
    sample_rate: u32,
    channels: usize,
    scratch: Vec<f32>,
}

impl Mixer {
    /// Create an unconfigured mixer. It renders silence until
    /// [`configure`](Mixer::configure) is called.
    pub fn new(shared: Arc<MixerShared>) -> Mixer {
        Mixer {
            shared,
            voices: std::array::from_fn(|_| None),
            pending: std::array::from_fn(|_| None),
            pending_used: 0,
            next_seq: 0,
            master_gain: 1.0,
            sample_rate: 0,
            channels: 0,
            scratch: Vec::new(),
        }
    }

    /// The shared state this mixer reads commands from.
    #[cfg(test)]
    pub fn shared(&self) -> &Arc<MixerShared> {
        &self.shared
    }

    /// Set the output format. Runs outside the callback and may allocate.
    ///
    /// Voices and their source positions are kept, so playback continues
    /// where it was after a stream rebuild.
    pub fn configure(&mut self, sample_rate: u32, channels: u16) {
        self.sample_rate = sample_rate;
        self.channels = usize::from(channels);
        self.scratch.clear();
        self.scratch.resize(SCRATCH_FRAMES * self.channels, 0.0);
    }

    /// Output sample rate in Hz (0 while unconfigured).
    #[cfg(test)]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Output channel count (0 while unconfigured).
    pub fn channels(&self) -> u16 {
        u16::try_from(self.channels).unwrap_or(u16::MAX)
    }

    /// Current master gain.
    #[cfg(test)]
    pub fn master_gain(&self) -> f32 {
        self.master_gain
    }

    /// Frames the scratch buffer holds (0 while unconfigured).
    pub fn scratch_frames(&self) -> usize {
        self.scratch.len().checked_div(self.channels).unwrap_or(0)
    }

    /// Source positions (in source frames) of the voices that are still
    /// audible, in voice-slot order.
    #[cfg(test)]
    pub fn voice_positions(&self) -> impl Iterator<Item = f64> + '_ {
        self.voices
            .iter()
            .flatten()
            .filter(|v| !v.finished)
            .map(|v| v.position)
    }

    /// Render up to `max_frames` frames into the preallocated scratch buffer
    /// and return the interleaved mix. The result holds at most
    /// [`scratch_frames`](Mixer::scratch_frames) frames; call it repeatedly to
    /// fill a larger output buffer. Realtime-safe.
    pub fn render_scratch(&mut self, max_frames: usize) -> &[f32] {
        let len = max_frames.min(self.scratch_frames()) * self.channels;
        // Taking an allocated Vec and putting it back neither allocates nor frees.
        let mut scratch = std::mem::take(&mut self.scratch);
        if let Some(buf) = scratch.get_mut(..len) {
            self.render(buf);
        }
        self.scratch = scratch;
        self.scratch.get(..len).unwrap_or(&[])
    }

    /// Mix every voice into `out` (interleaved, `channels` per frame).
    /// Realtime-safe: no allocation, no freeing, no locks, no I/O.
    pub fn render(&mut self, out: &mut [f32]) {
        self.retry_pending();
        self.release_finished();
        let applied = self.apply_commands();

        out.fill(0.0);
        if self.channels > 0 && self.sample_rate > 0 {
            self.mix(out);
        }
        self.release_finished();

        let active = self.voices.iter().flatten().filter(|v| !v.finished).count();
        self.shared.active_voices.store(active, Ordering::Release);
        // Published after `active_voices`: a reader that sees this render's
        // command count (Acquire) also sees this render's voice count.
        if applied > 0 {
            self.shared
                .commands_applied
                .fetch_add(applied, Ordering::Release);
        }
        self.shared
            .callback_heartbeat
            .fetch_add(1, Ordering::Release);
    }

    /// Push parked sounds into the garbage queue, in slot order, while it has room.
    fn retry_pending(&mut self) {
        if self.pending_used == 0 {
            return;
        }
        for slot in self.pending.iter_mut() {
            if let Some(sound) = slot.take() {
                match self.shared.garbage.push(sound) {
                    Ok(()) => self.pending_used -= 1,
                    Err(sound) => {
                        *slot = Some(sound);
                        break;
                    }
                }
            }
        }
    }

    /// Hand a sound to the garbage queue, or park it in a free slot.
    /// Gives it back if neither has room.
    fn release(&mut self, sound: Sound) -> Result<(), Sound> {
        let sound = match self.shared.garbage.push(sound) {
            Ok(()) => return Ok(()),
            Err(sound) => sound,
        };
        match self.pending.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(sound);
                self.pending_used += 1;
                Ok(())
            }
            None => Err(sound),
        }
    }

    /// Release a sound that, by the command-drain invariant, always finds room.
    fn release_guaranteed(&mut self, sound: Sound) {
        if let Err(sound) = self.release(sound) {
            // Unreachable while the invariant holds. Leaking is the only
            // option that neither frees nor blocks on the realtime thread.
            debug_assert!(false, "release slot invariant violated");
            std::mem::forget(sound);
        }
    }

    /// Release voices that finished; keep them parked if there is no room.
    fn release_finished(&mut self) {
        for i in 0..MAX_VOICES {
            let finished = matches!(&self.voices[i], Some(v) if v.finished);
            if !finished {
                continue;
            }
            if let Some(voice) = self.voices[i].take()
                && let Err(sound) = self.release(voice.sound)
            {
                self.voices[i] = Some(Voice {
                    sound,
                    gain: 0.0,
                    position: voice.position,
                    seq: voice.seq,
                    finished: true,
                });
                break;
            }
        }
    }

    /// Apply queued commands; returns how many were applied.
    fn apply_commands(&mut self) -> u64 {
        let mut applied = 0;
        while PENDING_RELEASE - self.pending_used >= MAX_VOICES {
            let Some(command) = self.shared.commands.pop() else {
                break;
            };
            self.apply(command);
            applied += 1;
        }
        applied
    }

    fn apply(&mut self, command: Command) {
        match command {
            Command::Play { sound, gain } => {
                let slot = self.voice_slot_for_play();
                if let Some(old) = self.voices[slot].take() {
                    self.release_guaranteed(old.sound);
                }
                self.voices[slot] = Some(Voice {
                    sound,
                    gain: sanitize_gain(gain),
                    position: 0.0,
                    seq: self.next_seq,
                    finished: false,
                });
                self.next_seq += 1;
            }
            Command::StopAll => {
                for i in 0..MAX_VOICES {
                    if let Some(voice) = self.voices[i].take() {
                        self.release_guaranteed(voice.sound);
                    }
                }
            }
            Command::SetMasterGain(gain) => self.master_gain = sanitize_gain(gain),
        }
    }

    /// A free slot, else a finished one, else the oldest voice.
    fn voice_slot_for_play(&self) -> usize {
        let mut oldest = 0;
        let mut oldest_seq = u64::MAX;
        for (i, slot) in self.voices.iter().enumerate() {
            match slot {
                None => return i,
                Some(v) if v.finished => return i,
                Some(v) if v.seq < oldest_seq => {
                    oldest = i;
                    oldest_seq = v.seq;
                }
                Some(_) => {}
            }
        }
        oldest
    }

    fn mix(&mut self, out: &mut [f32]) {
        let out_channels = self.channels;
        let rate = f64::from(self.sample_rate);
        let master = self.master_gain;
        for voice in self.voices.iter_mut().flatten() {
            if voice.finished {
                continue;
            }
            let step = f64::from(voice.sound.sample_rate()) / rate;
            let gain = voice.gain * master;
            voice.finished = mix_voice(voice, out, out_channels, step, gain);
        }
        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }
    }
}

/// Mix one voice into `out`, advancing its position. Returns true once the
/// voice has played past its last frame.
fn mix_voice(
    voice: &mut Voice,
    out: &mut [f32],
    out_channels: usize,
    step: f64,
    gain: f32,
) -> bool {
    let data = &*voice.sound;
    let samples = data.samples();
    let src_channels = usize::from(data.channels());
    let frames = data.frames();
    let mut position = voice.position;

    for frame in out.chunks_exact_mut(out_channels) {
        let index = position as usize;
        if index >= frames {
            break;
        }
        let frac = (position - index as f64) as f32;
        let left = interpolate(samples, src_channels, index, 0, frac);
        if src_channels == 1 {
            let value = left * gain;
            frame[0] += value;
            if let Some(right) = frame.get_mut(1) {
                *right += value;
            }
        } else {
            let right = interpolate(samples, src_channels, index, 1, frac);
            if let [l, r, ..] = frame {
                *l += left * gain;
                *r += right * gain;
            } else {
                frame[0] += 0.5 * (left + right) * gain;
            }
        }
        position += step;
    }

    voice.position = position;
    position as usize >= frames
}

/// Linear interpolation between frame `index` and the next one on `channel`.
/// Past the last frame the signal is treated as silence.
#[inline]
fn interpolate(samples: &[f32], channels: usize, index: usize, channel: usize, frac: f32) -> f32 {
    let a = samples
        .get(index * channels + channel)
        .copied()
        .unwrap_or(0.0);
    let b = samples
        .get((index + 1) * channels + channel)
        .copied()
        .unwrap_or(0.0);
    a + (b - a) * frac
}

/// Non-finite gains become silence; negative gains clamp to 0.
fn sanitize_gain(gain: f32) -> f32 {
    if gain.is_finite() { gain.max(0.0) } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(samples: Vec<f32>, rate: u32, channels: u16) -> Sound {
        Sound::from_interleaved(samples, rate, channels, "test").unwrap()
    }

    fn constant(value: f32, frames: usize, rate: u32, channels: u16) -> Sound {
        sound(vec![value; frames * usize::from(channels)], rate, channels)
    }

    fn mixer(rate: u32, channels: u16) -> Mixer {
        let mut m = Mixer::new(Arc::new(MixerShared::new()));
        m.configure(rate, channels);
        m
    }

    fn play(m: &Mixer, sound: &Sound, gain: f32) {
        m.shared()
            .push(Command::Play {
                sound: sound.clone(),
                gain,
            })
            .unwrap();
    }

    fn render(m: &mut Mixer, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * m.channels];
        m.render(&mut out);
        out
    }

    fn assert_close(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-5, "{a} != {b}");
    }

    #[test]
    fn unconfigured_mixer_renders_silence() {
        let mut m = Mixer::new(Arc::new(MixerShared::new()));
        let s = constant(0.5, 100, 48_000, 1);
        play(&m, &s, 1.0);
        let mut out = vec![1.0; 16];
        m.render(&mut out);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().commands_applied(), 1);
        assert_eq!(m.shared().callback_heartbeat(), 1);
        assert_eq!(m.render_scratch(64).len(), 0);
    }

    #[test]
    fn thirty_third_voice_evicts_the_oldest() {
        let mut m = mixer(48_000, 2);
        let sounds: Vec<Sound> = (0..=MAX_VOICES)
            .map(|_| constant(0.01, 48_000, 48_000, 1))
            .collect();
        for s in &sounds[..MAX_VOICES] {
            play(&m, s, 1.0);
        }
        render(&mut m, 16);
        assert_eq!(m.shared().active_voices(), MAX_VOICES);

        play(&m, &sounds[MAX_VOICES], 1.0);
        render(&mut m, 16);
        assert_eq!(m.shared().active_voices(), MAX_VOICES);
        m.shared().drain_garbage();
        assert_eq!(sounds[0].handle_count(), 1, "oldest voice evicted");
        for s in &sounds[1..] {
            assert_eq!(s.handle_count(), 2, "other voices still held");
        }
        // The newcomer starts from the beginning, the survivors keep going.
        let positions: Vec<f64> = m.voice_positions().collect();
        assert_eq!(positions.iter().filter(|&&p| p == 16.0).count(), 1);
        assert_eq!(positions.iter().filter(|&&p| p == 32.0).count(), 31);
    }

    #[test]
    fn overlapping_plays_sum_without_restart() {
        let mut m = mixer(48_000, 1);
        let s = constant(0.25, 1000, 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 10);
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6));

        play(&m, &s, 1.0);
        let out = render(&mut m, 10);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));
        let mut positions: Vec<f64> = m.voice_positions().collect();
        positions.sort_by(f64::total_cmp);
        assert_eq!(positions, vec![10.0, 20.0]);
        assert_eq!(m.shared().active_voices(), 2);
    }

    #[test]
    fn stop_all_silences_and_moves_sounds_to_garbage() {
        let mut m = mixer(48_000, 2);
        let s = constant(0.5, 48_000, 48_000, 1);
        play(&m, &s, 1.0);
        play(&m, &s, 1.0);
        render(&mut m, 32);
        assert_eq!(s.handle_count(), 3);

        m.shared().push(Command::StopAll).unwrap();
        let out = render(&mut m, 32);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().active_voices(), 0);
        // Still alive: the mixer handed them to the garbage queue, not dropped them.
        assert_eq!(s.handle_count(), 3);
        assert_eq!(m.shared().drain_garbage(), 2);
        assert_eq!(s.handle_count(), 1);
    }

    #[test]
    fn finished_voice_is_released_through_garbage() {
        let mut m = mixer(48_000, 1);
        let s = constant(0.5, 10, 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 20);
        assert!(out[..10].iter().all(|&x| x > 0.0));
        assert!(out[11..].iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().active_voices(), 0);
        assert_eq!(s.handle_count(), 2);
        m.shared().drain_garbage();
        assert_eq!(s.handle_count(), 1);
    }

    #[test]
    fn resampling_44k_to_48k_consumes_at_ratio() {
        let mut m = mixer(48_000, 2);
        let s = constant(0.1, 44_100, 44_100, 1);
        play(&m, &s, 1.0);
        render(&mut m, 4800);
        let pos: Vec<f64> = m.voice_positions().collect();
        assert_eq!(pos.len(), 1);
        assert!((pos[0] / 4800.0 - 0.91875).abs() < 1e-9, "{}", pos[0]);
        assert!((pos[0] - 4410.0).abs() < 1e-6);
    }

    #[test]
    fn resampling_48k_to_44k_consumes_at_inverse_ratio() {
        let mut m = mixer(44_100, 2);
        let s = constant(0.1, 48_000, 48_000, 1);
        play(&m, &s, 1.0);
        render(&mut m, 4410);
        let pos: Vec<f64> = m.voice_positions().collect();
        assert!((pos[0] - 4800.0).abs() < 1e-6, "{}", pos[0]);
        assert!((pos[0] / 4410.0 - 48_000.0 / 44_100.0).abs() < 1e-9);
    }

    #[test]
    fn linear_interpolation_at_fractional_positions() {
        // Step 0.5: every other output sample sits halfway between two frames.
        let mut m = mixer(48_000, 1);
        let s = sound(vec![0.0, 1.0, 0.5, -1.0], 24_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 10);
        let expected = [0.0, 0.5, 1.0, 0.75, 0.5, -0.25, -1.0, -0.5, 0.0, 0.0];
        for (a, b) in out.iter().zip(expected) {
            assert_close(*a, b);
        }

        // Step 0.25 gives quarter positions.
        let mut m = mixer(48_000, 1);
        let s = sound(vec![0.0, 1.0, 1.0], 12_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 4);
        for (a, b) in out.iter().zip([0.0, 0.25, 0.5, 0.75]) {
            assert_close(*a, b);
        }
    }

    #[test]
    fn mono_to_stereo_duplicates() {
        let mut m = mixer(48_000, 2);
        let s = sound(vec![0.1, 0.2, 0.3], 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 3);
        for (frame, v) in out.chunks(2).zip([0.1, 0.2, 0.3]) {
            assert_close(frame[0], v);
            assert_close(frame[1], v);
        }
    }

    #[test]
    fn mono_on_mono_device_uses_channel_zero() {
        let mut m = mixer(48_000, 1);
        let s = sound(vec![0.1, 0.2], 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 2);
        assert_close(out[0], 0.1);
        assert_close(out[1], 0.2);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let mut m = mixer(48_000, 1);
        let s = sound(vec![0.2, 0.6, -0.4, 0.0], 48_000, 2);
        play(&m, &s, 1.0);
        let out = render(&mut m, 2);
        assert_close(out[0], 0.4);
        assert_close(out[1], -0.2);
    }

    #[test]
    fn stereo_to_stereo_maps_left_and_right() {
        let mut m = mixer(48_000, 2);
        let s = sound(vec![0.2, -0.3], 48_000, 2);
        play(&m, &s, 1.0);
        let out = render(&mut m, 1);
        assert_close(out[0], 0.2);
        assert_close(out[1], -0.3);
    }

    #[test]
    fn stereo_to_six_channels_leaves_extra_channels_silent() {
        let mut m = mixer(48_000, 6);
        let s = sound(vec![0.2, -0.3, 0.4, -0.5], 48_000, 2);
        play(&m, &s, 1.0);
        let out = render(&mut m, 2);
        for (frame, (l, r)) in out.chunks(6).zip([(0.2, -0.3), (0.4, -0.5)]) {
            assert_close(frame[0], l);
            assert_close(frame[1], r);
            assert!(frame[2..].iter().all(|&x| x == 0.0));
        }
    }

    #[test]
    fn more_than_two_source_channels_use_the_first_two() {
        let mut m = mixer(48_000, 2);
        let s = sound(vec![0.1, 0.2, 0.9, 0.9], 48_000, 4);
        play(&m, &s, 1.0);
        let out = render(&mut m, 1);
        assert_close(out[0], 0.1);
        assert_close(out[1], 0.2);
    }

    #[test]
    fn sound_gain_and_master_gain_multiply() {
        let mut m = mixer(48_000, 1);
        let s = constant(0.5, 100, 48_000, 1);
        m.shared().push(Command::SetMasterGain(0.5)).unwrap();
        play(&m, &s, 0.4);
        let out = render(&mut m, 4);
        assert!(out.iter().all(|&x| (x - 0.1).abs() < 1e-6));
        assert_close(m.master_gain(), 0.5);
    }

    #[test]
    fn output_is_clamped() {
        let mut m = mixer(48_000, 1);
        let s = constant(0.8, 100, 48_000, 1);
        play(&m, &s, 1.0);
        play(&m, &s, 1.0);
        let out = render(&mut m, 4);
        assert!(out.iter().all(|&x| x == 1.0));

        let n = constant(-0.8, 100, 48_000, 1);
        m.shared().push(Command::StopAll).unwrap();
        play(&m, &n, 2.0);
        let out = render(&mut m, 4);
        assert!(out.iter().all(|&x| x == -1.0));
    }

    #[test]
    fn invalid_gains_are_sanitized() {
        let mut m = mixer(48_000, 1);
        let s = constant(0.5, 100, 48_000, 1);
        play(&m, &s, f32::NAN);
        play(&m, &s, -1.0);
        let out = render(&mut m, 4);
        assert!(out.iter().all(|&x| x == 0.0));
        m.shared()
            .push(Command::SetMasterGain(f32::INFINITY))
            .unwrap();
        render(&mut m, 1);
        assert_eq!(m.master_gain(), 0.0);
    }

    #[test]
    fn configure_mid_playback_preserves_position() {
        let mut m = mixer(48_000, 2);
        let s = constant(0.1, 48_000, 48_000, 2);
        play(&m, &s, 1.0);
        render(&mut m, 1000);
        assert_eq!(m.voice_positions().collect::<Vec<_>>(), vec![1000.0]);

        m.configure(96_000, 1);
        assert_eq!(m.voice_positions().collect::<Vec<_>>(), vec![1000.0]);
        assert_eq!(m.scratch_frames(), SCRATCH_FRAMES);
        render(&mut m, 1000);
        assert_eq!(m.voice_positions().collect::<Vec<_>>(), vec![1500.0]);
        assert_eq!(m.shared().active_voices(), 1);
    }

    #[test]
    fn full_command_queue_returns_the_command() {
        let m = mixer(48_000, 2);
        for _ in 0..COMMAND_CAPACITY {
            m.shared().push(Command::StopAll).unwrap();
        }
        let s = constant(0.1, 10, 48_000, 1);
        match m.shared().push(Command::Play {
            sound: s.clone(),
            gain: 0.7,
        }) {
            Err(Command::Play { sound, gain }) => {
                assert!(Sound::ptr_eq(&sound, &s));
                assert_eq!(gain, 0.7);
            }
            other => panic!("expected the command back, got {other:?}"),
        }
    }

    #[test]
    fn full_garbage_queue_parks_sound_without_dropping() {
        let mut m = mixer(48_000, 1);
        let filler = constant(0.0, 1, 48_000, 1);
        for _ in 0..GARBAGE_CAPACITY {
            m.shared().garbage.push(filler.clone()).unwrap();
        }
        let s = constant(0.5, 8, 48_000, 1);
        play(&m, &s, 1.0);
        render(&mut m, 32);

        assert_eq!(m.shared().active_voices(), 0);
        assert_eq!(s.handle_count(), 2, "mixer still holds a reference");
        assert_eq!(m.pending_used, 1);
        assert!(
            m.pending
                .iter()
                .flatten()
                .any(|parked| Sound::ptr_eq(parked, &s))
        );

        assert_eq!(m.shared().drain_garbage(), GARBAGE_CAPACITY);
        assert_eq!(s.handle_count(), 2);
        render(&mut m, 32);
        assert_eq!(m.pending_used, 0);
        m.shared().drain_garbage();
        assert_eq!(s.handle_count(), 1, "released off the realtime thread");
    }

    #[test]
    fn commands_wait_while_pending_slots_are_scarce() {
        let mut m = mixer(48_000, 1);
        let filler = constant(0.0, 1, 48_000, 1);
        for _ in 0..GARBAGE_CAPACITY {
            m.shared().garbage.push(filler.clone()).unwrap();
        }
        // Leave one slot fewer than MAX_VOICES free.
        for slot in m.pending.iter_mut().take(PENDING_RELEASE - MAX_VOICES + 1) {
            *slot = Some(filler.clone());
        }
        m.pending_used = PENDING_RELEASE - MAX_VOICES + 1;

        let s = constant(0.5, 1000, 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 8);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().commands_applied(), 0);
        assert_eq!(m.shared().commands.len(), 1, "command stays queued");

        m.shared().drain_garbage();
        let out = render(&mut m, 8);
        assert_eq!(m.pending_used, 0);
        assert_eq!(m.shared().commands_applied(), 1);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));
    }

    /// `MAX_VOICES` long voices of distinct sounds, all playing, then a full
    /// garbage queue.
    fn full_board_with_full_garbage(m: &mut Mixer) -> Vec<Sound> {
        let sounds: Vec<Sound> = (0..MAX_VOICES)
            .map(|_| constant(0.01, 48_000, 48_000, 1))
            .collect();
        for s in &sounds {
            play(m, s, 1.0);
        }
        render(m, 1);
        assert_eq!(m.shared().active_voices(), MAX_VOICES);
        let filler = constant(0.0, 1, 48_000, 1);
        for _ in 0..GARBAGE_CAPACITY {
            m.shared().garbage.push(filler.clone()).unwrap();
        }
        sounds
    }

    #[test]
    fn stop_all_with_full_garbage_parks_every_voice() {
        let mut m = mixer(48_000, 1);
        let sounds = full_board_with_full_garbage(&mut m);
        let applied = m.shared().commands_applied();

        m.shared().push(Command::StopAll).unwrap();
        render(&mut m, 1);
        assert_eq!(m.shared().commands_applied(), applied + 1);
        assert_eq!(m.pending_used, MAX_VOICES);
        assert_eq!(m.shared().active_voices(), 0);
        // Nothing was freed on the render thread: each sound is held by the
        // test and by its pending slot.
        assert!(sounds.iter().all(|s| s.handle_count() == 2));

        // Exactly MAX_VOICES slots are still free, so a new play is applied.
        let next = constant(0.5, 1000, 48_000, 1);
        play(&m, &next, 1.0);
        let out = render(&mut m, 4);
        assert_eq!(m.shared().commands_applied(), applied + 2);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));

        // Once the garbage queue drains, the parked sounds move on and are
        // freed off the render thread.
        m.shared().drain_garbage();
        render(&mut m, 1);
        assert_eq!(m.pending_used, 0);
        m.shared().drain_garbage();
        assert!(sounds.iter().all(|s| s.handle_count() == 1));
    }

    #[test]
    fn eviction_with_full_garbage_parks_the_oldest_voice() {
        let mut m = mixer(48_000, 1);
        let sounds = full_board_with_full_garbage(&mut m);

        let newest = constant(0.5, 1000, 48_000, 1);
        play(&m, &newest, 1.0);
        render(&mut m, 1);
        assert_eq!(m.pending_used, 1);
        assert_eq!(m.shared().active_voices(), MAX_VOICES);
        // The oldest voice was evicted into a slot, not freed.
        assert_eq!(sounds[0].handle_count(), 2);
        assert!(sounds[1..].iter().all(|s| s.handle_count() == 2));
        assert_eq!(newest.handle_count(), 2);

        m.shared().drain_garbage();
        render(&mut m, 1);
        m.shared().drain_garbage();
        assert_eq!(sounds[0].handle_count(), 1);
    }

    #[test]
    fn finished_voice_without_slot_stays_silent_until_released() {
        let mut m = mixer(48_000, 1);
        let filler = constant(0.0, 1, 48_000, 1);
        let s = constant(0.5, 4, 48_000, 1);
        play(&m, &s, 1.0);
        render(&mut m, 1);
        // Garbage and every pending slot full.
        for _ in 0..GARBAGE_CAPACITY {
            m.shared().garbage.push(filler.clone()).unwrap();
        }
        for slot in m.pending.iter_mut() {
            *slot = Some(filler.clone());
        }
        m.pending_used = PENDING_RELEASE;

        let out = render(&mut m, 8);
        assert!(out[..3].iter().all(|&x| x > 0.0));
        assert!(out[4..].iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().active_voices(), 0);
        assert_eq!(s.handle_count(), 2, "parked in its voice slot");

        let out = render(&mut m, 8);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(s.handle_count(), 2);

        // Draining frees room; the voice is released through the queue.
        m.shared().drain_garbage();
        render(&mut m, 8);
        render(&mut m, 8);
        m.shared().drain_garbage();
        assert_eq!(s.handle_count(), 1);
    }

    #[test]
    fn render_scratch_matches_render() {
        let s = sound(
            (0..3000).map(|i| (i as f32 * 0.01).sin()).collect(),
            44_100,
            1,
        );
        let mut a = mixer(48_000, 2);
        let mut b = mixer(48_000, 2);
        play(&a, &s, 0.8);
        play(&b, &s, 0.8);

        let total = SCRATCH_FRAMES * 2 + 100;
        let expected = render(&mut a, total);
        let mut got = Vec::new();
        while got.len() < total * 2 {
            let remaining = total - got.len() / 2;
            got.extend_from_slice(b.render_scratch(remaining));
        }
        assert_eq!(expected.len(), got.len());
        for (x, y) in expected.iter().zip(&got) {
            assert_close(*x, *y);
        }
    }

    #[test]
    fn heartbeat_and_counters_advance() {
        let mut m = mixer(48_000, 2);
        let s = constant(0.1, 100, 48_000, 2);
        play(&m, &s, 1.0);
        m.shared().push(Command::SetMasterGain(0.5)).unwrap();
        render(&mut m, 4);
        render(&mut m, 4);
        assert_eq!(m.shared().callback_heartbeat(), 2);
        assert_eq!(m.shared().commands_applied(), 2);
        assert_eq!(m.shared().active_voices(), 1);
    }
}
