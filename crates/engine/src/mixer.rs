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
//!
//! # No output
//!
//! Only a running stream drains the command queue. `Engine::play` accepts
//! plays only while [`MixerShared::accepts_plays`] holds: while a stream runs,
//! or inside the short play grace window the supervisor opens when it tears
//! down a live stream for a rebuild. So presses made with no device do not
//! all start at once when one comes back, but a press made during the gap of
//! a device switch is kept and played by the replacement stream. While no
//! stream exists and the grace window is closed, the supervisor calls
//! [`Mixer::apply_offline`], which drops any play that passed that check
//! (just before the stream went away, or inside a window whose rebuild
//! failed) and still applies `StopAll`, `SetMasterGain` and `SetPathGains`.
//!
//! # Mic passthrough
//!
//! The mic sink's mixer is built with [`Mixer::with_passthrough`] and owns a
//! [`PassthroughReader`]. Every render adds `input gain × passthrough` to the
//! mix before the clamp, with the mono-sound channel mapping. The monitor's
//! mixer is built with [`Mixer::new`] and has no reader, so it can never
//! carry the microphone. [`Command::SetPathGains`] sets the per-path sound and
//! input gains; both default to 1.0, so a mixer that never receives it mixes
//! exactly as in stage 1.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_queue::ArrayQueue;

use crate::passthrough::PassthroughReader;
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
    /// Set this path's gains: `sound` multiplies every voice (on top of the
    /// voice and master gains), `input` scales the mic passthrough.
    SetPathGains { sound: f32, input: f32 },
}

/// State shared between the mixer and the control threads.
pub struct MixerShared {
    commands: ArrayQueue<Command>,
    garbage: ArrayQueue<Sound>,
    active_voices: AtomicUsize,
    commands_applied: AtomicU64,
    callback_heartbeat: AtomicU64,
    output_active: AtomicBool,
    /// Set once the path is stopped: renders output silence from then on.
    silenced: AtomicBool,
    output_latency_nanos: AtomicU64,
    /// The reference point for `grace_until_nanos`.
    epoch: Instant,
    /// End of the play grace window in nanoseconds since `epoch`; 0 means no
    /// window.
    grace_until_nanos: AtomicU64,
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
            output_active: AtomicBool::new(false),
            silenced: AtomicBool::new(false),
            output_latency_nanos: AtomicU64::new(0),
            epoch: Instant::now(),
            grace_until_nanos: AtomicU64::new(0),
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

    /// Whether an output stream is running.
    pub fn output_active(&self) -> bool {
        self.output_active.load(Ordering::Acquire)
    }

    /// Whether a play made at `now` is accepted: a stream runs, or the play
    /// grace window opened by a live teardown is still open. Plays queued
    /// with no stream and no open window are dropped by the supervisor.
    pub fn accepts_plays(&self, now: Instant) -> bool {
        self.output_active() || self.play_grace_open(now)
    }

    /// Whether the play grace window is open at `now`.
    pub fn play_grace_open(&self, now: Instant) -> bool {
        match self.grace_until_nanos.load(Ordering::Acquire) {
            0 => false,
            nanos => self
                .epoch
                .checked_add(Duration::from_nanos(nanos))
                .is_none_or(|until| now < until),
        }
    }

    /// Open the play grace window until `until`. Set by the supervisor when
    /// it tears down a live stream for a rebuild.
    pub fn open_play_grace(&self, until: Instant) {
        let since = until.saturating_duration_since(self.epoch);
        // 0 means "no window", so a deadline at the epoch itself is stored as
        // 1 ns; such a window is already closed for any later `now` anyway.
        let nanos = u64::try_from(since.as_nanos()).unwrap_or(u64::MAX).max(1);
        self.grace_until_nanos.store(nanos, Ordering::Release);
    }

    /// Close the play grace window.
    pub fn close_play_grace(&self) {
        self.grace_until_nanos.store(0, Ordering::Release);
    }

    /// Silence this path for good: every later render outputs silence (no
    /// voices, no passthrough), whatever stream renders it. Set when the
    /// path is stopped, before its supervisor is joined, so a stream that
    /// outlives the stop (its supervisor blocked in a device query) carries
    /// nothing, the live microphone included.
    pub fn silence(&self) {
        self.silenced.store(true, Ordering::Release);
    }

    /// Whether [`silence`](MixerShared::silence) was called.
    pub fn is_silenced(&self) -> bool {
        self.silenced.load(Ordering::Acquire)
    }

    /// Set by the supervisor around every build.
    pub fn set_output_active(&self, active: bool) {
        self.output_active.store(active, Ordering::Release);
    }

    /// How long until the last frame the latest callback wrote is played:
    /// the backend's reported latency for the buffer's first frame plus the
    /// buffer's own duration (zero before the first callback).
    pub fn output_latency(&self) -> Duration {
        Duration::from_nanos(self.output_latency_nanos.load(Ordering::Acquire))
    }

    /// Record the latency the backend reported for the current callback.
    /// Realtime-safe: one atomic store.
    fn set_output_latency(&self, latency: Duration) {
        let nanos = u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX);
        self.output_latency_nanos.store(nanos, Ordering::Release);
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
            .field("output_active", &self.output_active())
            .field("silenced", &self.is_silenced())
            .field(
                "grace_until_nanos",
                &self.grace_until_nanos.load(Ordering::Acquire),
            )
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
    /// This path's gain on every voice, on top of the voice and master gains.
    sound_gain: f32,
    /// This path's gain on the passthrough.
    input_gain: f32,
    sample_rate: u32,
    channels: usize,
    scratch: Vec<f32>,
    /// The mic passthrough; only the mic sink's mixer has one.
    passthrough: Option<PassthroughReader>,
    /// Mono passthrough samples for one chunk of [`SCRATCH_FRAMES`] frames,
    /// allocated by `configure` when there is a reader.
    input_scratch: Vec<f32>,
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
            sound_gain: 1.0,
            input_gain: 1.0,
            sample_rate: 0,
            channels: 0,
            scratch: Vec::new(),
            passthrough: None,
            input_scratch: Vec::new(),
        }
    }

    /// Create an unconfigured mic mixer that adds `reader`'s passthrough to
    /// its mix. See [`new`](Mixer::new).
    pub fn with_passthrough(shared: Arc<MixerShared>, reader: PassthroughReader) -> Mixer {
        Mixer {
            passthrough: Some(reader),
            ..Mixer::new(shared)
        }
    }

    /// Publish the output latency measured for the current callback, up to
    /// its last frame (see [`MixerShared::output_latency`]). Realtime-safe.
    pub fn record_output_latency(&mut self, latency: Duration) {
        self.shared.set_output_latency(latency);
    }

    /// The shared state this mixer reads commands from.
    #[cfg(test)]
    pub fn shared(&self) -> &Arc<MixerShared> {
        &self.shared
    }

    /// Set the output format. Runs outside the callback and may allocate.
    ///
    /// Voices and their source positions are kept, so playback continues
    /// where it was after a stream rebuild. The passthrough reader, if any,
    /// gets the new rate and re-primes at every call (a changed rate also
    /// resets its resampler and controller; the same rate keeps the learned
    /// correction), and forgets the sink block it recorded, since the new
    /// stream may have another buffer size.
    pub fn configure(&mut self, sample_rate: u32, channels: u16) {
        self.sample_rate = sample_rate;
        self.channels = usize::from(channels);
        self.scratch.clear();
        self.scratch.resize(SCRATCH_FRAMES * self.channels, 0.0);
        if let Some(reader) = self.passthrough.as_mut() {
            reader.set_sink_rate(sample_rate);
            self.input_scratch.clear();
            self.input_scratch.resize(SCRATCH_FRAMES, 0.0);
        }
    }

    /// Output sample rate in Hz (0 while unconfigured).
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

    /// Current path gains: `(sound, input)`.
    #[cfg(test)]
    pub fn path_gains(&self) -> (f32, f32) {
        (self.sound_gain, self.input_gain)
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

    /// Announce a device callback of `frames` frames before it is rendered in
    /// scratch-sized chunks, so the passthrough reader sizes its target to the
    /// whole device block (it cannot tell a long callback from back-to-back
    /// chunks otherwise). A no-op without a reader. Realtime-safe.
    pub fn begin_callback(&mut self, frames: usize) {
        if let Some(reader) = self.passthrough.as_mut() {
            reader.note_block(frames);
        }
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
    /// Outputs silence once the path is silenced (`MixerShared::silence`),
    /// still applying commands and releasing sounds.
    /// Realtime-safe: no allocation, no freeing, no locks, no I/O.
    pub fn render(&mut self, out: &mut [f32]) {
        self.retry_pending();
        self.release_finished();
        let applied = self.apply_commands();

        out.fill(0.0);
        if self.channels > 0 && self.sample_rate > 0 && !self.shared.is_silenced() {
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

    /// Empty the command queue while no stream exists, dropping every play.
    ///
    /// Not realtime: the supervisor calls it, with the mixer locked, only
    /// while no stream exists, so the lock never contends and sounds may be
    /// dropped here directly. Without it, every play queued while there is
    /// no output would start at once when a device comes back. `StopAll` and
    /// `SetMasterGain` still apply. Dropped plays do not count as applied.
    ///
    /// Returns the number of plays dropped.
    pub fn apply_offline(&mut self) -> usize {
        let mut dropped = 0;
        let mut applied = 0;
        while let Some(command) = self.shared.commands.pop() {
            match command {
                Command::Play { sound, .. } => {
                    drop(sound);
                    dropped += 1;
                }
                Command::StopAll => {
                    self.voices.iter_mut().for_each(|voice| drop(voice.take()));
                    applied += 1;
                }
                Command::SetMasterGain(gain) => {
                    self.master_gain = sanitize_gain(gain);
                    applied += 1;
                }
                Command::SetPathGains { sound, input } => {
                    self.set_path_gains(sound, input);
                    applied += 1;
                }
            }
        }
        if applied > 0 {
            let active = self.voices.iter().flatten().filter(|v| !v.finished).count();
            self.shared.active_voices.store(active, Ordering::Release);
            self.shared
                .commands_applied
                .fetch_add(applied, Ordering::Release);
        }
        dropped
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
            Command::SetPathGains { sound, input } => self.set_path_gains(sound, input),
        }
    }

    fn set_path_gains(&mut self, sound: f32, input: f32) {
        self.sound_gain = sanitize_gain(sound);
        self.input_gain = sanitize_gain(input);
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
        let path = self.master_gain * self.sound_gain;
        for voice in self.voices.iter_mut().flatten() {
            if voice.finished {
                continue;
            }
            let step = f64::from(voice.sound.sample_rate()) / rate;
            let gain = voice.gain * path;
            voice.finished = mix_voice(voice, out, out_channels, step, gain);
        }
        if let Some(reader) = self.passthrough.as_mut() {
            mix_passthrough(
                reader,
                &mut self.input_scratch,
                out,
                out_channels,
                self.input_gain,
            );
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

/// Add `gain × passthrough` to `out`, mapped like a mono sound: channel 0,
/// and channel 1 when there is one. Reads the passthrough in chunks of the
/// preallocated mono `scratch`, so `out` may be any length. Realtime-safe.
fn mix_passthrough(
    reader: &mut PassthroughReader,
    scratch: &mut [f32],
    out: &mut [f32],
    out_channels: usize,
    gain: f32,
) {
    let chunk_frames = scratch.len();
    if chunk_frames == 0 {
        return;
    }
    for chunk in out.chunks_mut(chunk_frames * out_channels) {
        let frames = chunk.len() / out_channels;
        let Some(input) = scratch.get_mut(..frames) else {
            return;
        };
        reader.read(input);
        for (frame, &sample) in chunk.chunks_exact_mut(out_channels).zip(input.iter()) {
            let value = sample * gain;
            frame[0] += value;
            if let Some(right) = frame.get_mut(1) {
                *right += value;
            }
        }
    }
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
    use crate::passthrough::PassthroughShared;

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
    fn a_silenced_mixer_renders_silence_but_still_applies_commands() {
        let passthrough = Arc::new(PassthroughShared::new());
        let mut m = Mixer::with_passthrough(Arc::new(MixerShared::new()), passthrough.reader());
        m.configure(48_000, 2);
        passthrough.begin_capture(48_000);
        for block in [0.25f32; 4800].chunks(480) {
            passthrough.write(block, 1);
        }
        let s = constant(0.5, 4800, 48_000, 1);
        play(&m, &s, 1.0);
        assert!(render(&mut m, 64).iter().any(|&x| x != 0.0));

        m.shared().silence();
        assert!(m.shared().is_silenced());
        play(&m, &s, 1.0);
        assert!(render(&mut m, 64).iter().all(|&x| x == 0.0));
        assert_eq!(m.shared().commands_applied(), 2);
        assert_eq!(m.shared().callback_heartbeat(), 2);
        // A stop still releases the voices.
        m.shared().push(Command::StopAll).unwrap();
        render(&mut m, 64);
        assert_eq!(m.shared().active_voices(), 0);
        assert_eq!(m.shared().drain_garbage(), 2);
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
    fn offline_drops_plays_and_applies_the_rest() {
        let mut m = mixer(48_000, 1);
        let playing = constant(0.5, 48_000, 48_000, 1);
        play(&m, &playing, 1.0);
        render(&mut m, 32);
        assert_eq!(m.shared().commands_applied(), 1);

        // The stream is gone; presses and a gain change queue up meanwhile.
        let pressed = constant(0.5, 48_000, 48_000, 1);
        play(&m, &pressed, 1.0);
        play(&m, &pressed, 1.0);
        m.shared().push(Command::SetMasterGain(0.25)).unwrap();
        assert_eq!(m.apply_offline(), 2);
        assert_eq!(pressed.handle_count(), 1);
        assert_close(m.master_gain(), 0.25);
        assert_eq!(m.shared().commands_applied(), 2);
        // The voice that was already playing is kept for the next stream.
        assert_eq!(playing.handle_count(), 2);
        assert_eq!(m.shared().active_voices(), 1);

        m.shared().push(Command::StopAll).unwrap();
        assert_eq!(m.apply_offline(), 0);
        assert_eq!(playing.handle_count(), 1);
        assert_eq!(m.shared().active_voices(), 0);
        assert_eq!(m.shared().commands_applied(), 3);

        // Once a stream runs again, plays are applied as usual.
        play(&m, &pressed, 1.0);
        render(&mut m, 32);
        assert_eq!(m.shared().active_voices(), 1);
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

    /// A capture at 48 kHz holding `samples` copies of `value`, and a mic
    /// mixer reading it at 48 kHz with `channels` channels.
    fn mic_mixer(channels: u16, samples: usize, value: f32) -> (Mixer, Arc<PassthroughShared>) {
        let passthrough = Arc::new(PassthroughShared::new());
        passthrough.begin_capture(48_000);
        // In 10 ms blocks, as a capture writes (one huge block would raise
        // the target).
        for block in vec![value; samples].chunks(480) {
            passthrough.write(block, 1);
        }
        let mut m = Mixer::with_passthrough(Arc::new(MixerShared::new()), passthrough.reader());
        m.configure(48_000, channels);
        (m, passthrough)
    }

    /// Enough passthrough to prime (30 ms at 48 kHz) and play a while,
    /// below the high mark (80 ms).
    const PRIMED: usize = 3_000;

    fn reader(m: &Mixer) -> &PassthroughReader {
        m.passthrough.as_ref().unwrap()
    }

    #[test]
    fn passthrough_is_mixed_with_the_mono_mapping() {
        let (mut m, passthrough) = mic_mixer(2, PRIMED, 0.5);
        let out = render(&mut m, 64);
        assert!(reader(&m).is_primed());
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6), "{out:?}");
        assert!(passthrough.fill() < PRIMED, "the mixer consumed the ring");

        let (mut m, _passthrough) = mic_mixer(1, PRIMED, 0.25);
        let out = render(&mut m, 8);
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6), "{out:?}");

        let (mut m, _passthrough) = mic_mixer(6, PRIMED, 0.25);
        let out = render(&mut m, 8);
        for frame in out.chunks(6) {
            assert_close(frame[0], 0.25);
            assert_close(frame[1], 0.25);
            assert!(frame[2..].iter().all(|&x| x == 0.0));
        }
    }

    #[test]
    fn passthrough_adds_to_voices_and_is_clamped() {
        let (mut m, _passthrough) = mic_mixer(2, PRIMED, 0.5);
        let s = constant(0.25, 1000, 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 16);
        assert!(out.iter().all(|&x| (x - 0.75).abs() < 1e-6), "{out:?}");

        play(&m, &s, 2.0);
        let out = render(&mut m, 16);
        assert!(out.iter().all(|&x| x == 1.0), "clamped: {out:?}");

        let (mut m, _passthrough) = mic_mixer(2, PRIMED, -0.9);
        let n = constant(-0.5, 1000, 48_000, 1);
        play(&m, &n, 1.0);
        let out = render(&mut m, 16);
        assert!(out.iter().all(|&x| x == -1.0), "clamped: {out:?}");
    }

    #[test]
    fn path_gains_multiply() {
        let (mut m, _passthrough) = mic_mixer(1, PRIMED, 0.4);
        assert_eq!(m.path_gains(), (1.0, 1.0), "defaults leave the mix alone");
        let s = constant(0.5, 1000, 48_000, 1);
        m.shared().push(Command::SetMasterGain(0.5)).unwrap();
        m.shared()
            .push(Command::SetPathGains {
                sound: 0.5,
                input: 0.25,
            })
            .unwrap();
        play(&m, &s, 0.8);
        let out = render(&mut m, 8);
        // voice 0.5 × 0.8 × master 0.5 × sound 0.5 = 0.1, plus input 0.4 × 0.25 = 0.1.
        assert!(out.iter().all(|&x| (x - 0.2).abs() < 1e-6), "{out:?}");
        assert_eq!(m.path_gains(), (0.5, 0.25));

        // The monitor (no reader) applies the sound path gain too.
        let mut monitor = mixer(48_000, 1);
        monitor
            .shared()
            .push(Command::SetPathGains {
                sound: 0.5,
                input: 3.0,
            })
            .unwrap();
        play(&monitor, &s, 1.0);
        let out = render(&mut monitor, 8);
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6), "{out:?}");
    }

    #[test]
    fn invalid_path_gains_are_sanitized_and_apply_offline() {
        let (mut m, _passthrough) = mic_mixer(1, PRIMED, 0.5);
        m.shared()
            .push(Command::SetPathGains {
                sound: f32::NAN,
                input: -1.0,
            })
            .unwrap();
        let s = constant(0.5, 1000, 48_000, 1);
        play(&m, &s, 1.0);
        let out = render(&mut m, 8);
        assert!(out.iter().all(|&x| x == 0.0), "{out:?}");
        assert_eq!(m.path_gains(), (0.0, 0.0));

        m.shared()
            .push(Command::SetPathGains {
                sound: f32::INFINITY,
                input: 0.75,
            })
            .unwrap();
        let applied = m.shared().commands_applied();
        assert_eq!(m.apply_offline(), 0);
        assert_eq!(m.path_gains(), (0.0, 0.75));
        assert_eq!(m.shared().commands_applied(), applied + 1);
    }

    #[test]
    fn configure_passes_the_sink_rate_to_the_reader() {
        let (mut m, _passthrough) = mic_mixer(2, PRIMED, 0.5);
        render(&mut m, 16);
        assert!(reader(&m).is_primed());
        m.configure(48_000, 1);
        assert!(!reader(&m).is_primed(), "the same rate re-primes");
        m.configure(44_100, 2);
        assert!(!reader(&m).is_primed(), "a new rate re-primes");

        // Unconfigured, the reader has no sink rate and the mix is silent.
        let passthrough = Arc::new(PassthroughShared::new());
        passthrough.begin_capture(48_000);
        for block in vec![0.5f32; PRIMED].chunks(480) {
            passthrough.write(block, 1);
        }
        let mut m = Mixer::with_passthrough(Arc::new(MixerShared::new()), passthrough.reader());
        let mut out = vec![1.0f32; 16];
        m.render(&mut out);
        assert!(out.iter().all(|&x| x == 0.0));
        assert_eq!(passthrough.fill(), PRIMED, "nothing was read");
    }

    #[test]
    fn mixer_without_passthrough_audio_matches_stage_one() {
        // A mic mixer whose capture has not started mixes exactly like the
        // monitor, and so does one whose input gain is zero.
        let s = sound(
            (0..3000).map(|i| (i as f32 * 0.01).sin()).collect(),
            44_100,
            1,
        );
        let mut monitor = mixer(48_000, 2);
        let mut idle = Mixer::with_passthrough(
            Arc::new(MixerShared::new()),
            Arc::new(PassthroughShared::new()).reader(),
        );
        idle.configure(48_000, 2);
        let (mut muted, _passthrough) = mic_mixer(2, PRIMED, 0.5);
        muted
            .shared()
            .push(Command::SetPathGains {
                sound: 1.0,
                input: 0.0,
            })
            .unwrap();
        for m in [&monitor, &idle, &muted] {
            play(m, &s, 0.8);
        }
        let expected = render(&mut monitor, 2000);
        assert_eq!(render(&mut idle, 2000), expected);
        assert_eq!(render(&mut muted, 2000), expected);
    }

    #[test]
    fn render_with_a_reader_keeps_its_scratch_buffers_and_matches_render_scratch() {
        fn sine_mixer() -> (Mixer, Arc<PassthroughShared>) {
            let passthrough = Arc::new(PassthroughShared::new());
            passthrough.begin_capture(48_000);
            let input: Vec<f32> = (0..3_800).map(|i| 0.5 * (i as f32 * 0.03).sin()).collect();
            for block in input.chunks(480) {
                passthrough.write(block, 1);
            }
            let mut m = Mixer::with_passthrough(Arc::new(MixerShared::new()), passthrough.reader());
            m.configure(48_000, 2);
            (m, passthrough)
        }
        let s = sound(
            (0..3000).map(|i| (i as f32 * 0.01).sin()).collect(),
            44_100,
            1,
        );
        let (mut a, _pa) = sine_mixer();
        let (mut b, _pb) = sine_mixer();
        play(&a, &s, 0.3);
        play(&b, &s, 0.3);
        let scratch = (b.scratch.as_ptr(), b.scratch.capacity());
        let input = (b.input_scratch.as_ptr(), b.input_scratch.capacity());

        // More than one scratch chunk, so the passthrough is read in chunks.
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
        assert!(reader(&b).is_primed());
        assert!(
            expected.iter().any(|&x| x.abs() > 0.1),
            "the passthrough was heard"
        );
        // The buffers were neither reallocated nor grown.
        assert_eq!((b.scratch.as_ptr(), b.scratch.capacity()), scratch);
        assert_eq!(
            (b.input_scratch.as_ptr(), b.input_scratch.capacity()),
            input
        );
        assert_eq!(b.input_scratch.len(), SCRATCH_FRAMES);
    }

    #[test]
    fn begin_callback_reaches_the_reader_and_allocates_nothing() {
        /// One 2048-frame callback, rendered in scratch-sized chunks.
        /// Returns whether the passthrough was heard.
        fn callback(m: &mut Mixer) -> bool {
            m.begin_callback(2_048);
            let mut rendered = 0;
            let mut audible = false;
            while rendered < 2_048 {
                let mix = m.render_scratch(2_048 - rendered);
                audible |= mix.iter().any(|&x| (x - 0.5).abs() < 1e-6);
                rendered += mix.len() / 2;
            }
            audible
        }
        let (mut m, passthrough) = mic_mixer(2, 0, 0.0);
        let scratch = (m.scratch.as_ptr(), m.scratch.capacity());
        let input = (m.input_scratch.as_ptr(), m.input_scratch.capacity());

        passthrough.write(&[0.5f32; 2_048], 1);
        assert!(!callback(&mut m), "priming");
        assert_eq!(reader(&m).sink_block(), 2_048);
        passthrough.write(&[0.5f32; 2_048], 1);
        passthrough.write(&[0.5f32; 2_048], 1);
        assert!(callback(&mut m));
        assert!(reader(&m).is_primed());
        assert_eq!(passthrough.published_target(), 4_336);
        assert_eq!(passthrough.underruns(), 0);
        // The buffers were neither reallocated nor grown.
        assert_eq!((m.scratch.as_ptr(), m.scratch.capacity()), scratch);
        assert_eq!(
            (m.input_scratch.as_ptr(), m.input_scratch.capacity()),
            input
        );

        // A configure forgets the recorded sink block.
        m.configure(48_000, 2);
        assert_eq!(reader(&m).sink_block(), 0);

        // Without a reader, it is a no-op.
        let mut monitor = mixer(48_000, 2);
        monitor.begin_callback(2_048);
        assert!(render(&mut monitor, 16).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn the_monitor_has_no_passthrough_buffer() {
        let m = mixer(48_000, 2);
        assert!(m.passthrough.is_none());
        assert!(m.input_scratch.is_empty());
    }

    #[test]
    fn accepts_plays_while_output_runs_or_inside_the_grace_window() {
        let shared = MixerShared::new();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        assert!(!shared.accepts_plays(t0));
        assert!(!shared.play_grace_open(t0));

        shared.set_output_active(true);
        assert!(shared.accepts_plays(t0));

        // A live teardown opens the window, then output goes away.
        shared.open_play_grace(t0 + ms(1000));
        shared.set_output_active(false);
        assert!(shared.play_grace_open(t0 + ms(999)));
        assert!(shared.accepts_plays(t0 + ms(999)));
        assert!(!shared.play_grace_open(t0 + ms(1000)));
        assert!(!shared.accepts_plays(t0 + ms(1000)));

        shared.close_play_grace();
        assert!(!shared.accepts_plays(t0 + ms(10)));

        // A deadline already in the past opens no window.
        shared.open_play_grace(t0);
        assert!(!shared.accepts_plays(t0 + ms(1)));
    }
}
