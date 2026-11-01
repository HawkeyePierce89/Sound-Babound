//! The microphone passthrough: a lock-free mono ring from the capture stream
//! into the mic sink's mixer, with drift compensation.
//!
//! The capture and the mic sink run on two different device clocks, so
//! their nominal rates can differ (44.1 kHz in, 48 kHz out) and their real
//! rates drift apart by a few hundred ppm at most. The ring absorbs the
//! cadence mismatch, and [`PassthroughReader`] resamples with a linear
//! interpolator whose ratio a PI controller nudges by at most
//! [`MAX_CORRECTION`] to hold the fill at the effective target.
//!
//! The effective target is [`TARGET_FILL`] (30 ms) unless the device blocks
//! need more: `max(TARGET_FILL, sink block + capture block + BLOCK_MARGIN)`,
//! in capture-rate samples. Long device buffers (a DAW sets 1024 to 4096
//! frames) deliver and consume audio in big steps, and when the two clocks
//! slip past each other the fill seen before a read drops by a whole capture
//! block, so the target has to cover one block of each side. The writer
//! records its largest block ([`PassthroughShared::write`]); the reader
//! records the largest sink block, announced once per callback through
//! [`PassthroughReader::note_block`] before the mixer reads in chunks, and
//! publishes the target it uses so the writer's high mark follows it.
//!
//! Both ends follow the realtime rules: the ring is a preallocated
//! [`ArrayQueue`], and neither side allocates, frees, locks or logs.
//!
//! - The writer side ([`PassthroughShared::write`]) runs in the capture
//!   callback. It downmixes to mono, pushes, and trims the oldest audio if
//!   the fill passes the high mark: the published target plus
//!   `max(HIGH_MARK_MARGIN, capture block + BLOCK_MARGIN)`.
//! - The reader side ([`PassthroughReader::read`]) runs in the mic sink
//!   callback. It outputs silence until the ring holds the effective target
//!   (priming), and re-primes after an underrun or when a new capture starts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

use crate::convert::InputSample;

/// The highest capture rate the ring is sized for.
pub const MAX_CAPTURE_RATE: u32 = 192_000;
/// Ring capacity in mono samples: 0.5 s at [`MAX_CAPTURE_RATE`].
pub const RING_CAPACITY: usize = MAX_CAPTURE_RATE as usize / 2;
/// The smallest fill the reader primes to and the controller holds; the
/// effective target grows past it for long device blocks.
pub const TARGET_FILL: Duration = Duration::from_millis(30);
/// How far above the target the fill may grow before the writer trims it,
/// at least (a long capture block raises it).
pub const HIGH_MARK_MARGIN: Duration = Duration::from_millis(50);
/// Slack on top of one sink block plus one capture block, in the effective
/// target and in the high mark. Small enough that the target stays at
/// [`TARGET_FILL`] for 10 ms blocks (480 capture / 512 sink at 44.1–48 kHz).
pub const BLOCK_MARGIN: Duration = Duration::from_millis(5);
/// The largest effective target, in mono samples: half the ring, so priming
/// stays reachable whatever the blocks.
const MAX_TARGET: usize = RING_CAPACITY / 2;
/// The largest relative ratio correction, ±0.2 %.
pub const MAX_CORRECTION: f64 = 0.002;

/// Proportional gain: correction per second of fill error.
const KP: f64 = 0.5;
/// Integral gain: correction per second of accumulated fill error.
const KI: f64 = 0.05;
/// Time constant of the EMA that smooths the measured fill error, in
/// seconds. It averages out the block cadence of both streams.
const EMA_TAU: f64 = 0.5;

/// `duration` worth of mono samples at `rate`.
fn samples_for(rate: u32, duration: Duration) -> usize {
    let samples = u128::from(rate) * duration.as_nanos() / 1_000_000_000;
    usize::try_from(samples).unwrap_or(usize::MAX)
}

/// The target fill at `rate`, in mono samples.
fn target_samples(rate: u32) -> usize {
    samples_for(rate, TARGET_FILL)
}

/// The fill above which the writer trims, in mono samples, for a `target`
/// and the largest capture block (in frames) at `rate`.
fn high_mark_samples(rate: u32, target: usize, capture_block: usize) -> usize {
    let margin = samples_for(rate, HIGH_MARK_MARGIN)
        .max(capture_block.saturating_add(samples_for(rate, BLOCK_MARGIN)));
    target.saturating_add(margin)
}

/// The effective target at `capture_rate`, in mono samples: [`TARGET_FILL`],
/// or one sink block (converted from `sink_rate`) plus one capture block plus
/// [`BLOCK_MARGIN`] if that is more, at most [`MAX_TARGET`]. Integer math
/// only, realtime-safe.
fn effective_target(
    capture_rate: u32,
    sink_rate: u32,
    sink_block: usize,
    capture_block: usize,
) -> usize {
    let sink = u64::try_from(sink_block)
        .unwrap_or(u64::MAX)
        .saturating_mul(u64::from(capture_rate))
        .div_ceil(u64::from(sink_rate.max(1)));
    let sink = usize::try_from(sink).unwrap_or(usize::MAX);
    let blocks = sink
        .saturating_add(capture_block)
        .saturating_add(samples_for(capture_rate, BLOCK_MARGIN));
    target_samples(capture_rate).max(blocks).min(MAX_TARGET)
}

/// State shared by the capture callback (writer), the mic sink callback
/// (reader) and the control threads.
#[derive(Debug)]
pub struct PassthroughShared {
    ring: ArrayQueue<f32>,
    capture_rate: AtomicU32,
    /// The largest capture block written since `begin_capture`, in frames.
    capture_block: AtomicUsize,
    /// The effective target the reader last used, in mono samples (0 before
    /// the reader published one for this capture).
    target: AtomicUsize,
    generation: AtomicU64,
    heartbeat: AtomicU64,
    active: AtomicBool,
    /// Set once the mic path is stopped: writes are dropped from then on.
    closed: AtomicBool,
    underruns: AtomicU64,
    overruns: AtomicU64,
}

impl PassthroughShared {
    /// Allocate the ring once, with room for 0.5 s at 192 kHz.
    pub fn new() -> PassthroughShared {
        PassthroughShared {
            ring: ArrayQueue::new(RING_CAPACITY),
            capture_rate: AtomicU32::new(0),
            capture_block: AtomicUsize::new(0),
            target: AtomicUsize::new(0),
            generation: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            active: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
        }
    }

    /// A reader for the mic mixer.
    pub fn reader(self: &Arc<Self>) -> PassthroughReader {
        PassthroughReader::new(Arc::clone(self))
    }

    /// Start a new capture at `rate`. Called from a capture build before its
    /// stream starts: empties the ring, forgets the recorded capture block and
    /// the published target, stores the rate and bumps the generation, so the
    /// reader resets and re-primes.
    pub fn begin_capture(&self, rate: u32) {
        while self.ring.pop().is_some() {}
        self.capture_block.store(0, Ordering::Release);
        self.target.store(0, Ordering::Release);
        self.capture_rate.store(rate, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Close the passthrough for good: later writes are dropped and the ring
    /// is emptied. Set when the mic path is stopped, before its supervisors
    /// are joined, so a capture that outlives the stop (its supervisor
    /// blocked in a device query) passes nothing on.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        while self.ring.pop().is_some() {}
    }

    /// Whether [`close`](PassthroughShared::close) was called.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Write one block of interleaved capture input. Realtime-safe.
    ///
    /// Each frame is downmixed to mono by averaging its channels; a trailing
    /// partial frame is ignored. The block's frame count raises the recorded
    /// capture block. One overrun is counted per block that displaced the
    /// oldest samples from a full ring or pushed the fill past the high mark;
    /// in the latter case the oldest samples are popped until the fill is
    /// back at the target (the published one, or [`TARGET_FILL`] before the
    /// reader published any). Once the passthrough is closed, the block is
    /// dropped (the heartbeat still moves).
    pub fn write<T: InputSample>(&self, data: &[T], channels: usize) {
        if channels > 0 && !self.is_closed() {
            let scale = 1.0 / channels as f32;
            let mut overrun = false;
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|&s| s.to_mix()).sum();
                overrun |= self.ring.force_push(sum * scale).is_some();
            }
            let capture_block = self
                .capture_block
                .fetch_max(data.len() / channels, Ordering::AcqRel)
                .max(data.len() / channels);
            let rate = self.capture_rate.load(Ordering::Acquire);
            let target = match self.target.load(Ordering::Acquire) {
                0 => target_samples(rate),
                published => published,
            };
            if rate > 0 && self.ring.len() > high_mark_samples(rate, target, capture_block) {
                while self.ring.len() > target && self.ring.pop().is_some() {}
                overrun = true;
            }
            if overrun {
                self.overruns.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.heartbeat.fetch_add(1, Ordering::AcqRel);
    }

    /// The rate of the current capture (0 before the first one).
    pub fn capture_rate(&self) -> u32 {
        self.capture_rate.load(Ordering::Acquire)
    }

    /// The largest capture block written since the last
    /// [`begin_capture`](Self::begin_capture), in frames.
    fn capture_block(&self) -> usize {
        self.capture_block.load(Ordering::Acquire)
    }

    #[cfg(test)]
    /// The effective target the reader last published, in mono samples (0
    /// before it published one for this capture).
    pub fn published_target(&self) -> usize {
        self.target.load(Ordering::Acquire)
    }

    /// Bumped by every [`begin_capture`](Self::begin_capture).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Incremented on every capture block; the supervisor watches it for
    /// stalls.
    pub fn heartbeat(&self) -> u64 {
        self.heartbeat.load(Ordering::Acquire)
    }

    #[cfg(test)]
    /// Whether a capture stream is running.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Set by the capture supervisor around every build.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    #[cfg(test)]
    /// Blocks in which the reader ran the ring empty.
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Acquire)
    }

    #[cfg(test)]
    /// Blocks in which the writer dropped the oldest audio.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Acquire)
    }

    #[cfg(test)]
    /// Mono samples currently buffered.
    pub fn fill(&self) -> usize {
        self.ring.len()
    }
}

impl Default for PassthroughShared {
    fn default() -> Self {
        PassthroughShared::new()
    }
}

/// The mic sink side of the passthrough, owned by the mic mixer.
///
/// [`read`](Self::read) runs in the mic sink callback and is realtime-safe.
#[derive(Debug)]
pub struct PassthroughReader {
    shared: Arc<PassthroughShared>,
    sink_rate: u32,
    /// The largest sink block seen since the mixer was configured, in frames.
    sink_block: usize,
    /// The announced callback's length, and how much of it is still to be
    /// read (0 outside one), in frames.
    callback_frames: usize,
    callback_left: usize,
    /// Set by [`note_block`](Self::note_block): the controller has not
    /// stepped yet in the announced callback.
    callback_unmeasured: bool,
    /// The generation and capture rate the state below belongs to.
    generation: u64,
    capture_rate: u32,
    primed: bool,
    /// The two capture samples the output position lies between.
    prev: f32,
    next: f32,
    /// The position between `prev` and `next`, in [0, 1).
    frac: f64,
    /// Set on priming, so the first measurement seeds the EMA.
    fresh: bool,
    /// EMA of the fill error, in seconds.
    fill_error: f64,
    /// Accumulated fill error, in seconds × seconds.
    integral: f64,
    correction: f64,
}

impl PassthroughReader {
    fn new(shared: Arc<PassthroughShared>) -> PassthroughReader {
        PassthroughReader {
            generation: shared.generation(),
            capture_rate: shared.capture_rate(),
            shared,
            sink_rate: 0,
            sink_block: 0,
            callback_frames: 0,
            callback_left: 0,
            callback_unmeasured: false,
            primed: false,
            prev: 0.0,
            next: 0.0,
            frac: 0.0,
            fresh: true,
            fill_error: 0.0,
            integral: 0.0,
            correction: 0.0,
        }
    }

    /// Set the mic sink's rate. Called from `Mixer::configure`, before a new
    /// stream starts: it forgets the recorded sink block (the new stream may
    /// have another buffer size), and a rate change resets the resampler and
    /// the controller.
    pub fn set_sink_rate(&mut self, rate: u32) {
        self.sink_block = 0;
        self.callback_left = 0;
        if rate != self.sink_rate {
            self.sink_rate = rate;
            self.reset();
        }
    }

    /// Announce a sink callback of `frames` frames, before the mixer reads it
    /// in chunks: raises the recorded sink block, and makes the controller
    /// step once for the whole callback (on its first read) instead of once
    /// per chunk, so the chunks after the first, which see a fill lowered by
    /// the earlier ones, do not bias it. Realtime-safe.
    pub fn note_block(&mut self, frames: usize) {
        self.sink_block = self.sink_block.max(frames);
        self.callback_frames = frames;
        self.callback_left = frames;
        self.callback_unmeasured = true;
    }

    #[cfg(test)]
    /// The largest sink block recorded, in frames.
    pub fn sink_block(&self) -> usize {
        self.sink_block
    }

    #[cfg(test)]
    /// Whether the reader is primed and producing audio.
    pub fn is_primed(&self) -> bool {
        self.primed
    }

    #[cfg(test)]
    /// The current relative ratio correction.
    pub fn correction(&self) -> f64 {
        self.correction
    }

    /// Forget everything: re-prime and restart the controller.
    fn reset(&mut self) {
        self.primed = false;
        self.frac = 0.0;
        self.fresh = true;
        self.fill_error = 0.0;
        self.integral = 0.0;
        self.correction = 0.0;
    }

    /// Fill `out` with mono samples at the sink rate. Realtime-safe.
    ///
    /// `out.len()` raises the recorded sink block. The effective target is
    /// computed and published before the priming check, so the first read of
    /// a long block already primes to it. Outputs silence while priming, with no capture or sink rate, and for
    /// the rest of a block in which the ring ran empty (an underrun, which
    /// is counted and re-primes).
    pub fn read(&mut self, out: &mut [f32]) {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let capture_rate = self.shared.capture_rate.load(Ordering::Acquire);
        if generation != self.generation || capture_rate != self.capture_rate {
            self.generation = generation;
            self.capture_rate = capture_rate;
            self.reset();
        }
        if out.is_empty() {
            return;
        }
        self.sink_block = self.sink_block.max(out.len());
        // The controller steps once per announced callback, or once per read
        // outside one.
        let step_frames = if self.callback_left == 0 {
            Some(out.len())
        } else {
            self.callback_left = self.callback_left.saturating_sub(out.len());
            let frames = self.callback_frames;
            std::mem::take(&mut self.callback_unmeasured).then_some(frames)
        };
        if capture_rate == 0 || self.sink_rate == 0 {
            out.fill(0.0);
            return;
        }
        let target = effective_target(
            capture_rate,
            self.sink_rate,
            self.sink_block,
            self.shared.capture_block(),
        );
        self.shared.target.store(target, Ordering::Release);
        if !self.prime(target) {
            out.fill(0.0);
            return;
        }

        if let Some(frames) = step_frames {
            self.update_correction(frames, capture_rate, target);
        }
        let step = f64::from(capture_rate) / f64::from(self.sink_rate) * (1.0 + self.correction);
        for i in 0..out.len() {
            out[i] = self.prev + (self.next - self.prev) * self.frac as f32;
            self.frac += step;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                match self.shared.ring.pop() {
                    Some(sample) => {
                        self.prev = self.next;
                        self.next = sample;
                    }
                    None => {
                        self.shared.underruns.fetch_add(1, Ordering::AcqRel);
                        self.primed = false;
                        self.fresh = true;
                        out[i + 1..].fill(0.0);
                        return;
                    }
                }
            }
        }
    }

    /// Prime once the ring holds `target` samples. Returns whether the
    /// reader is primed.
    fn prime(&mut self, target: usize) -> bool {
        if self.primed {
            return true;
        }
        if self.shared.ring.len() < target.max(2) {
            return false;
        }
        match (self.shared.ring.pop(), self.shared.ring.pop()) {
            (Some(prev), Some(next)) => {
                self.prev = prev;
                self.next = next;
                self.frac = 0.0;
                self.fresh = true;
                self.primed = true;
                true
            }
            _ => false,
        }
    }

    /// One PI controller step per block, on the EMA-smoothed error of the
    /// fill against the effective `target`.
    fn update_correction(&mut self, frames: usize, capture_rate: u32, target: usize) {
        let fill = self.shared.ring.len() as f64;
        let target = target as f64;
        let error = (fill - target) / f64::from(capture_rate);
        let dt = frames as f64 / f64::from(self.sink_rate);
        if self.fresh {
            self.fill_error = error;
            self.fresh = false;
        } else {
            self.fill_error += dt / (EMA_TAU + dt) * (error - self.fill_error);
        }
        let limit = MAX_CORRECTION / KI;
        self.integral = (self.integral + self.fill_error * dt).clamp(-limit, limit);
        self.correction =
            (KP * self.fill_error + KI * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_at(rate: u32) -> Arc<PassthroughShared> {
        let shared = Arc::new(PassthroughShared::new());
        shared.begin_capture(rate);
        shared
    }

    /// Write `samples` copies of `value` in 10 ms blocks at 48 kHz, as a
    /// capture would (one huge block would raise the target).
    fn write_mono(shared: &PassthroughShared, samples: usize, value: f32) {
        write_blocks(shared, &vec![value; samples]);
    }

    /// Write `samples` in blocks of [`TEST_BLOCK`] frames.
    fn write_blocks(shared: &PassthroughShared, samples: &[f32]) {
        for block in samples.chunks(TEST_BLOCK) {
            shared.write(block, 1);
        }
    }

    /// The capture block the unit tests write: 10 ms at 48 kHz.
    const TEST_BLOCK: usize = 480;

    #[test]
    fn sizes_follow_the_rate() {
        assert_eq!(target_samples(48_000), 1_440);
        assert_eq!(high_mark_samples(48_000, 1_440, 480), 3_840);
        assert_eq!(target_samples(44_100), 1_323);
        assert_eq!(RING_CAPACITY, 96_000);
        let max = target_samples(MAX_CAPTURE_RATE);
        assert!(high_mark_samples(MAX_CAPTURE_RATE, max, 1_920) < RING_CAPACITY);
        assert!(high_mark_samples(MAX_CAPTURE_RATE, MAX_TARGET, 8_192) < RING_CAPACITY);
    }

    #[test]
    fn the_target_stays_30_ms_for_short_blocks_and_grows_for_long_ones() {
        // 10 ms-ish blocks, as the old drift tests use.
        for (capture, sink) in [(48_000, 48_000), (44_100, 48_000), (48_000, 44_100)] {
            assert_eq!(
                effective_target(capture, sink, 512, 480),
                target_samples(capture),
                "{capture} -> {sink}"
            );
        }
        // 2048-frame blocks: one sink block plus one capture block plus 5 ms.
        assert_eq!(effective_target(48_000, 48_000, 2_048, 2_048), 4_336);
        assert_eq!(effective_target(48_000, 48_000, 2_048, 480), 2_768);
        assert_eq!(effective_target(48_000, 48_000, 480, 2_048), 2_768);
        // The sink block is converted to capture-rate samples, rounding up.
        assert_eq!(
            effective_target(44_100, 48_000, 2_048, 480),
            1_882 + 480 + 220
        );
        // Nothing recorded yet: the floor.
        assert_eq!(effective_target(48_000, 48_000, 0, 0), 1_440);
        // Absurd blocks are capped so priming stays reachable.
        assert_eq!(
            effective_target(48_000, 48_000, 1 << 20, 1 << 20),
            MAX_TARGET
        );
    }

    #[test]
    fn the_reader_publishes_the_target_and_primes_to_it() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        assert_eq!(shared.published_target(), 0);
        shared.write(&[0.5f32; 2_048], 1);
        assert_eq!(shared.capture_block(), 2_048);

        // A 2048-frame callback read in two 1024-frame chunks, announced
        // first: the very first read already uses the larger target.
        reader.note_block(2_048);
        let mut out = vec![1.0f32; 1_024];
        reader.read(&mut out);
        assert_eq!(shared.published_target(), 4_336);
        assert!(!reader.is_primed(), "2048 samples are below the target");
        assert!(out.iter().all(|&s| s == 0.0));

        shared.write(&[0.5f32; 2_048], 1);
        reader.read(&mut out);
        assert!(!reader.is_primed(), "4096 samples are still below it");
        shared.write(&[0.5f32; 2_048], 1);
        reader.read(&mut out);
        assert!(reader.is_primed());
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
        reader.read(&mut out);
        assert!(reader.is_primed(), "the whole callback was covered");
        assert_eq!(shared.underruns(), 0);
        assert_eq!(shared.overruns(), 0);
    }

    #[test]
    fn begin_capture_and_set_sink_rate_reset_the_block_sizes() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        shared.write(&[0.5f32; 2_048], 1);
        reader.note_block(2_048);
        let mut out = vec![0.0f32; 64];
        reader.read(&mut out);
        assert_eq!(reader.sink_block(), 2_048, "read never lowers it");
        assert_eq!(shared.capture_block(), 2_048);
        assert_eq!(shared.published_target(), 4_336);

        shared.begin_capture(48_000);
        assert_eq!(shared.capture_block(), 0);
        assert_eq!(shared.published_target(), 0, "the target is republished");
        write_mono(&shared, 480, 0.5);
        assert_eq!(shared.capture_block(), 480);

        // A configure (same rate) forgets the sink block, without a reset.
        reader.set_sink_rate(48_000);
        assert_eq!(reader.sink_block(), 0);
        reader.read(&mut out);
        assert_eq!(reader.sink_block(), 64, "a read raises it to its length");
        assert_eq!(shared.published_target(), 1_440);
    }

    #[test]
    fn the_high_mark_follows_the_published_target() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        // Before any target is published: 30 ms + max(50 ms, block + 5 ms).
        assert_eq!(high_mark_samples(48_000, 1_440, 2_048), 3_840);
        assert_eq!(high_mark_samples(48_000, 1_440, 4_096), 1_440 + 4_336);

        shared.write(&[0.0f32; 2_048], 1);
        reader.note_block(2_048);
        let mut out = vec![0.0f32; 16];
        reader.read(&mut out);
        let target = shared.published_target();
        assert_eq!(target, 4_336);
        // The new high mark is 4336 + 2400 = 6736: 6144 samples are past the
        // old one (3840) but not past it.
        shared.write(&[0.0f32; 2_048], 1);
        shared.write(&[0.0f32; 2_048], 1);
        assert_eq!(shared.fill(), 6_144);
        assert_eq!(shared.overruns(), 0, "the old high mark would have trimmed");
        shared.write(&[0.0f32; 2_048], 1);
        assert_eq!(shared.overruns(), 1, "past target + 50 ms");
        assert_eq!(shared.fill(), target, "trimmed back to the target");
    }

    #[test]
    fn downmixes_by_averaging() {
        let shared = shared_at(48_000);
        shared.write(&[1.0f32, 0.0, 0.5, 0.5, -1.0, -0.5, 0.9], 2);
        assert_eq!(shared.fill(), 3, "the trailing partial frame is ignored");
        assert_eq!(shared.ring.pop(), Some(0.5));
        assert_eq!(shared.ring.pop(), Some(0.5));
        assert_eq!(shared.ring.pop(), Some(-0.75));

        shared.write(&[i16::MAX, i16::MIN, 0, 0, i16::MIN, i16::MIN], 3);
        assert_eq!(shared.fill(), 2);
        let first = shared.ring.pop().unwrap_or(f32::NAN);
        assert!(first.abs() < 1e-4, "{first}");
        let second = shared.ring.pop().unwrap_or(f32::NAN);
        assert!((second + 2.0 / 3.0).abs() < 1e-6, "{second}");

        let before = shared.heartbeat();
        shared.write::<f32>(&[0.5, 0.5], 0);
        assert_eq!(shared.fill(), 0, "zero channels writes nothing");
        assert_eq!(shared.heartbeat(), before + 1, "but still beats");
    }

    #[test]
    fn writes_bump_the_heartbeat_and_active_is_a_flag() {
        let shared = shared_at(48_000);
        assert_eq!(shared.heartbeat(), 0);
        write_mono(&shared, 10, 0.0);
        write_mono(&shared, 10, 0.0);
        assert_eq!(shared.heartbeat(), 2);
        assert!(!shared.is_active());
        shared.set_active(true);
        assert!(shared.is_active());
    }

    #[test]
    fn a_closed_passthrough_drops_its_input() {
        let shared = shared_at(48_000);
        write_mono(&shared, 100, 0.5);
        assert_eq!(shared.fill(), 100);
        shared.close();
        assert!(shared.is_closed());
        assert_eq!(shared.fill(), 0, "closing empties the ring");
        write_mono(&shared, 100, 0.5);
        assert_eq!(shared.fill(), 0);
        assert_eq!(shared.heartbeat(), 2, "but still beats");
    }

    #[test]
    fn overrun_past_the_high_mark_drops_the_oldest_audio() {
        let shared = shared_at(48_000);
        let high = high_mark_samples(48_000, target_samples(48_000), 480);
        let target = target_samples(48_000);
        // A ramp in [0, 1), so every sample is distinct and none clamps,
        // written in 480-frame blocks until the fill passes the high mark.
        let blocks = high / 480 + 1;
        let ramp: Vec<f32> = (0..blocks * 480).map(|i| i as f32 * 1e-5).collect();
        for (i, block) in ramp.chunks(480).enumerate() {
            shared.write(block, 1);
            let expected = u64::from(i + 1 == blocks);
            assert_eq!(shared.overruns(), expected, "block {i}");
        }
        assert_eq!(shared.fill(), target);
        let oldest_kept = shared.ring.pop().unwrap_or(f32::NAN);
        assert_eq!(
            oldest_kept,
            ramp[ramp.len() - target],
            "the newest audio stays"
        );

        write_mono(&shared, 10, 0.0);
        assert_eq!(
            shared.overruns(),
            1,
            "below the high mark nothing is dropped"
        );
    }

    #[test]
    fn a_full_ring_displaces_the_oldest_sample() {
        // No capture rate yet, so no high mark: only displacement applies.
        let shared = PassthroughShared::new();
        let ramp: Vec<f32> = (0..RING_CAPACITY + 5).map(|i| i as f32 * 1e-6).collect();
        shared.write(&ramp, 1);
        assert_eq!(shared.overruns(), 1, "one overrun per block");
        assert_eq!(shared.fill(), RING_CAPACITY);
        assert_eq!(shared.ring.pop(), Some(ramp[5]));
    }

    #[test]
    fn primes_to_the_target_before_producing_audio() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        let mut out = [1.0f32; 64];

        write_mono(&shared, target_samples(48_000) - 1, 0.5);
        reader.read(&mut out);
        assert!(!reader.is_primed());
        assert_eq!(out, [0.0; 64], "silence while priming");

        write_mono(&shared, 1, 0.5);
        reader.read(&mut out);
        assert!(reader.is_primed());
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6), "{out:?}");
        assert_eq!(shared.underruns(), 0);
    }

    #[test]
    fn silence_without_a_capture_or_sink_rate() {
        let shared = Arc::new(PassthroughShared::new());
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        write_mono(&shared, 4_000, 0.5);
        let mut out = [1.0f32; 32];
        reader.read(&mut out);
        assert_eq!(out, [0.0; 32], "no capture rate yet");

        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        write_mono(&shared, 4_000, 0.5);
        let mut out = [1.0f32; 32];
        reader.read(&mut out);
        assert_eq!(out, [0.0; 32], "no sink rate yet");
        assert!(!reader.is_primed());
    }

    #[test]
    fn underrun_gives_silence_and_re_primes() {
        let shared = shared_at(48_000);
        let target = target_samples(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        write_mono(&shared, target, 0.5);
        let mut first = [0.0f32; 64];
        reader.read(&mut first);
        assert!(reader.is_primed());

        // A read longer than what is left runs the ring empty.
        let left = shared.fill();
        let mut out = vec![1.0f32; left + 100];
        reader.read(&mut out);
        assert_eq!(shared.underruns(), 1);
        assert!(!reader.is_primed());
        assert!(out[..left - 10].iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert!(
            out[left + 10..].iter().all(|&s| s == 0.0),
            "rest of the block is silent"
        );

        // Not enough to re-prime: silence, and no further underrun.
        write_mono(&shared, target / 2, 0.5);
        let mut out = [1.0f32; 64];
        reader.read(&mut out);
        assert_eq!(out, [0.0; 64]);
        assert_eq!(shared.underruns(), 1);

        // The long read raised the target past 30 ms, so it takes more.
        write_mono(&shared, 2 * target, 0.25);
        reader.read(&mut out);
        assert!(reader.is_primed());
        assert!(out.iter().all(|&s| s > 0.0), "audio again after re-priming");
        assert_eq!(shared.underruns(), 1);
    }

    #[test]
    fn a_generation_change_resets() {
        let shared = shared_at(48_000);
        let target = target_samples(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        write_mono(&shared, 2 * target, 0.5);
        let mut out = [0.0f32; 256];
        reader.read(&mut out);
        reader.read(&mut out);
        assert!(reader.is_primed());
        assert!(reader.correction() != 0.0, "the controller is running");

        let generation = shared.generation();
        shared.begin_capture(48_000);
        assert_eq!(shared.generation(), generation + 1);
        assert_eq!(shared.fill(), 0, "begin_capture empties the ring");

        let mut out = [1.0f32; 256];
        reader.read(&mut out);
        assert!(!reader.is_primed(), "a new generation re-primes");
        assert_eq!(reader.correction(), 0.0, "and restarts the controller");
        assert_eq!(out, [0.0; 256]);
        assert_eq!(shared.underruns(), 0, "a reset is not an underrun");

        // A new rate re-primes at the new target (10 ms blocks at 24 kHz).
        shared.begin_capture(24_000);
        for block in vec![0.5f32; target_samples(24_000)].chunks(240) {
            shared.write(block, 1);
        }
        reader.read(&mut out);
        assert!(reader.is_primed());
    }

    #[test]
    fn a_sink_rate_change_resets() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        write_mono(&shared, 2 * target_samples(48_000), 0.5);
        let mut out = [0.0f32; 64];
        reader.read(&mut out);
        assert!(reader.is_primed());
        reader.set_sink_rate(48_000);
        assert!(reader.is_primed(), "the same rate keeps the state");
        reader.set_sink_rate(44_100);
        assert!(!reader.is_primed());
    }

    #[test]
    fn upsampling_interpolates_linearly() {
        let shared = shared_at(24_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(48_000);
        let target = target_samples(24_000);
        let ramp: Vec<f32> = (0..target).map(|i| i as f32 * 1e-4).collect();
        write_blocks(&shared, &ramp);
        let mut out = [0.0f32; 200];
        reader.read(&mut out);
        // Two output samples per input sample: the steps are half the ramp's.
        for pair in out.windows(2) {
            let step = pair[1] - pair[0];
            assert!((step - 0.5e-4).abs() < 1e-6, "{step}");
        }
    }

    #[test]
    fn downsampling_interpolates_linearly() {
        let shared = shared_at(48_000);
        let mut reader = shared.reader();
        reader.set_sink_rate(24_000);
        let target = target_samples(48_000);
        let ramp: Vec<f32> = (0..target * 2).map(|i| i as f32 * 1e-4).collect();
        write_blocks(&shared, &ramp);
        let mut out = [0.0f32; 200];
        reader.read(&mut out);
        // One output sample per two input samples: the steps are twice the
        // ramp's.
        for pair in out.windows(2) {
            let step = pair[1] - pair[0];
            assert!((step - 2e-4).abs() < 1e-6, "{step}");
        }
    }

    /// Two simulated device clocks, no threads and no devices.
    struct Clocks {
        capture_rate: u32,
        capture_clock: f64,
        sink_rate: u32,
        sink_clock: f64,
    }

    struct DriftResult {
        /// The largest fill deviation from the target after settling, in
        /// seconds.
        max_deviation: f64,
        underruns: u64,
        overruns: u64,
        /// Mean correction over the last minute.
        late_correction: f64,
        /// Mean correction from [`CONVERGED`] to the end of the run.
        settled_correction: f64,
        audible: bool,
    }

    /// Device block sizes, in frames.
    #[derive(Clone, Copy)]
    struct Blocks {
        capture: usize,
        sink: usize,
    }

    /// The blocks of the original drift tests: about 10 ms each.
    const SHORT_BLOCKS: Blocks = Blocks {
        capture: 480,
        sink: 512,
    };
    const SIMULATED: f64 = 600.0;
    const SETTLE: f64 = 5.0;
    /// Where [`CorrectionCheck::Settled`] starts: the controller is past its
    /// priming transient by then.
    const CONVERGED: f64 = 60.0;

    fn simulate(clocks: &Clocks, blocks: Blocks) -> DriftResult {
        let shared = shared_at(clocks.capture_rate);
        let mut reader = shared.reader();
        reader.set_sink_rate(clocks.sink_rate);

        let mut input = vec![0.0f32; blocks.capture];
        let mut output = vec![0.0f32; blocks.sink];
        let mut written = 0usize;
        let (mut capture_blocks, mut sink_blocks) = (1u64, 1u64);
        let capture_period = blocks.capture as f64 / clocks.capture_clock;
        let sink_period = blocks.sink as f64 / clocks.sink_clock;

        let mut max_deviation = 0.0f64;
        let (mut late_sum, mut late_count) = (0.0f64, 0u64);
        let (mut settled_sum, mut settled_count) = (0.0f64, 0u64);
        let mut audible = false;
        loop {
            let capture_at = capture_blocks as f64 * capture_period;
            let sink_at = sink_blocks as f64 * sink_period;
            if capture_at.min(sink_at) > SIMULATED {
                break;
            }
            if capture_at <= sink_at {
                for sample in &mut input {
                    let t = written as f32 / clocks.capture_rate as f32;
                    *sample = 0.5 * (std::f32::consts::TAU * 440.0 * t).sin();
                    written += 1;
                }
                shared.write(&input, 1);
                capture_blocks += 1;
            } else {
                if sink_at > SETTLE {
                    let target = shared.published_target() as f64;
                    let deviation =
                        (shared.fill() as f64 - target) / f64::from(clocks.capture_rate);
                    max_deviation = max_deviation.max(deviation.abs());
                }
                // As in production: the callback is announced, then read in
                // scratch-sized chunks.
                reader.note_block(blocks.sink);
                for chunk in output.chunks_mut(crate::mixer::SCRATCH_FRAMES) {
                    reader.read(chunk);
                }
                audible |= output.iter().any(|&s| s.abs() > 0.1);
                if sink_at > SIMULATED - 60.0 {
                    late_sum += reader.correction();
                    late_count += 1;
                }
                if sink_at > CONVERGED {
                    settled_sum += reader.correction();
                    settled_count += 1;
                }
                sink_blocks += 1;
            }
        }
        DriftResult {
            max_deviation,
            underruns: shared.underruns(),
            overruns: shared.overruns(),
            late_correction: late_sum / late_count.max(1) as f64,
            settled_correction: settled_sum / settled_count.max(1) as f64,
            audible,
        }
    }

    /// How the drift tests check that the correction found the clock ratio.
    #[derive(Clone, Copy)]
    enum CorrectionCheck {
        /// The mean over the last minute is within 20 ppm of it.
        Late,
        /// The mean from [`CONVERGED`] to the end is within 20 ppm plus one
        /// capture block over that span of it. For equal block periods: the
        /// fill seen before a read is then a staircase that steps by a whole
        /// capture block each time the two callbacks' order slips (every
        /// 3.5 minutes at 200 ppm), so the controller corrects in a burst at
        /// each slip and a one-minute mean says nothing. No controller can
        /// spread it out from the order of the callbacks alone.
        Settled,
    }

    fn assert_holds_the_target(clocks: Clocks) {
        assert_holds(clocks, SHORT_BLOCKS, 0.020, CorrectionCheck::Late);
    }

    /// The long-block variant: the fill may stray one capture block further,
    /// since the order of the two callbacks slips now and then.
    fn assert_holds_with_long_blocks(clocks: Clocks, blocks: Blocks, check: CorrectionCheck) {
        let slack = 0.020 + blocks.capture as f64 / f64::from(clocks.capture_rate);
        assert_holds(clocks, blocks, slack, check);
    }

    fn assert_holds(clocks: Clocks, blocks: Blocks, max_deviation: f64, check: CorrectionCheck) {
        let result = simulate(&clocks, blocks);
        let label = format!(
            "{} Hz at {} -> {} Hz at {}, {} / {} frames",
            clocks.capture_rate,
            clocks.capture_clock,
            clocks.sink_rate,
            clocks.sink_clock,
            blocks.capture,
            blocks.sink
        );
        assert_eq!(result.underruns, 0, "{label}: underruns");
        assert_eq!(result.overruns, 0, "{label}: overruns");
        assert!(
            result.max_deviation <= max_deviation,
            "{label}: fill deviated {:.1} ms",
            result.max_deviation * 1000.0
        );
        assert!(result.audible, "{label}: the passthrough carried audio");
        // The correction settles on the real clock ratio.
        let expected = (clocks.capture_clock / f64::from(clocks.capture_rate))
            / (clocks.sink_clock / f64::from(clocks.sink_rate))
            - 1.0;
        let (correction, tolerance) = match check {
            CorrectionCheck::Late => (result.late_correction, 20e-6),
            CorrectionCheck::Settled => {
                let settled = (SIMULATED - CONVERGED) * f64::from(clocks.capture_rate);
                (
                    result.settled_correction,
                    20e-6 + blocks.capture as f64 / settled,
                )
            }
        };
        assert!(
            (correction - expected).abs() < tolerance,
            "{label}: correction {:.1} ppm, expected {:.1} ± {:.1} ppm",
            correction * 1e6,
            expected * 1e6,
            tolerance * 1e6
        );
    }

    /// Sink clocks to try for a nominal 48 kHz sink: half a hertz fast, and
    /// 200 ppm off in both directions.
    const SINK_CLOCKS: [f64; 3] = [
        48_000.5,
        48_000.0 * (1.0 + 200e-6),
        48_000.0 * (1.0 - 200e-6),
    ];

    #[test]
    fn drift_48k_to_48k_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_the_target(Clocks {
                capture_rate: 48_000,
                capture_clock: 48_000.0,
                sink_rate: 48_000,
                sink_clock,
            });
        }
    }

    #[test]
    fn drift_44k1_to_48k_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_the_target(Clocks {
                capture_rate: 44_100,
                capture_clock: 44_100.0,
                sink_rate: 48_000,
                sink_clock,
            });
        }
    }

    #[test]
    fn drift_48k_to_44k1_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_the_target(Clocks {
                capture_rate: 48_000,
                capture_clock: 48_000.0,
                sink_rate: 44_100,
                sink_clock: sink_clock * 44_100.0 / 48_000.0,
            });
        }
    }

    #[test]
    fn capture_clock_drift_holds_the_target() {
        for drift in [200e-6, -200e-6] {
            assert_holds_the_target(Clocks {
                capture_rate: 48_000,
                capture_clock: 48_000.0 * (1.0 + drift),
                sink_rate: 48_000,
                sink_clock: 48_000.0,
            });
        }
    }

    #[test]
    fn drift_with_2048_frame_blocks_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_with_long_blocks(
                Clocks {
                    capture_rate: 48_000,
                    capture_clock: 48_000.0,
                    sink_rate: 48_000,
                    sink_clock,
                },
                Blocks {
                    capture: 2_048,
                    sink: 2_048,
                },
                CorrectionCheck::Settled,
            );
        }
    }

    #[test]
    fn drift_with_a_2048_frame_sink_block_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_with_long_blocks(
                Clocks {
                    capture_rate: 48_000,
                    capture_clock: 48_000.0,
                    sink_rate: 48_000,
                    sink_clock,
                },
                Blocks {
                    capture: 480,
                    sink: 2_048,
                },
                CorrectionCheck::Late,
            );
        }
    }

    #[test]
    fn drift_with_a_2048_frame_capture_block_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_with_long_blocks(
                Clocks {
                    capture_rate: 48_000,
                    capture_clock: 48_000.0,
                    sink_rate: 48_000,
                    sink_clock,
                },
                Blocks {
                    capture: 2_048,
                    sink: 480,
                },
                CorrectionCheck::Late,
            );
        }
    }

    #[test]
    fn drift_44k1_to_48k_with_a_2048_frame_sink_block_holds_the_target() {
        for sink_clock in SINK_CLOCKS {
            assert_holds_with_long_blocks(
                Clocks {
                    capture_rate: 44_100,
                    capture_clock: 44_100.0,
                    sink_rate: 48_000,
                    sink_clock,
                },
                Blocks {
                    capture: 480,
                    sink: 2_048,
                },
                CorrectionCheck::Late,
            );
        }
    }
}
