//! The microphone passthrough: a lock-free mono ring from the capture stream
//! into the mic sink's mixer, with drift compensation.
//!
//! The capture and the mic sink run on two different device clocks, so
//! their nominal rates can differ (44.1 kHz in, 48 kHz out) and their real
//! rates drift apart by a few hundred ppm at most. The ring absorbs the
//! cadence mismatch, and [`PassthroughReader`] resamples with a linear
//! interpolator whose ratio a PI controller nudges by at most
//! [`MAX_CORRECTION`] to hold the fill at [`TARGET_FILL`].
//!
//! Both ends follow the realtime rules: the ring is a preallocated
//! [`ArrayQueue`], and neither side allocates, frees, locks or logs.
//!
//! - The writer side ([`PassthroughShared::write`]) runs in the capture
//!   callback. It downmixes to mono, pushes, and trims the oldest audio if
//!   the fill passes the high mark (target plus [`HIGH_MARK_MARGIN`]).
//! - The reader side ([`PassthroughReader::read`]) runs in the mic sink
//!   callback. It outputs silence until the ring holds the target fill
//!   (priming), and re-primes after an underrun or when a new capture starts.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

use crate::convert::InputSample;

/// The highest capture rate the ring is sized for.
pub const MAX_CAPTURE_RATE: u32 = 192_000;
/// Ring capacity in mono samples: 0.5 s at [`MAX_CAPTURE_RATE`].
pub const RING_CAPACITY: usize = MAX_CAPTURE_RATE as usize / 2;
/// The fill the reader primes to and the controller holds.
pub const TARGET_FILL: Duration = Duration::from_millis(30);
/// How far above the target the fill may grow before the writer trims it.
pub const HIGH_MARK_MARGIN: Duration = Duration::from_millis(50);
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

/// The fill above which the writer trims, in mono samples.
fn high_mark_samples(rate: u32) -> usize {
    samples_for(rate, TARGET_FILL + HIGH_MARK_MARGIN)
}

/// State shared by the capture callback (writer), the mic sink callback
/// (reader) and the control threads.
#[derive(Debug)]
pub struct PassthroughShared {
    ring: ArrayQueue<f32>,
    capture_rate: AtomicU32,
    generation: AtomicU64,
    heartbeat: AtomicU64,
    active: AtomicBool,
    underruns: AtomicU64,
    overruns: AtomicU64,
}

impl PassthroughShared {
    /// Allocate the ring once, with room for 0.5 s at 192 kHz.
    pub fn new() -> PassthroughShared {
        PassthroughShared {
            ring: ArrayQueue::new(RING_CAPACITY),
            capture_rate: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            heartbeat: AtomicU64::new(0),
            active: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
        }
    }

    /// A reader for the mic mixer.
    pub fn reader(self: &Arc<Self>) -> PassthroughReader {
        PassthroughReader::new(Arc::clone(self))
    }

    /// Start a new capture at `rate`. Called from a capture build before its
    /// stream starts: empties the ring, stores the rate and bumps the
    /// generation, so the reader resets and re-primes.
    pub fn begin_capture(&self, rate: u32) {
        while self.ring.pop().is_some() {}
        self.capture_rate.store(rate, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Write one block of interleaved capture input. Realtime-safe.
    ///
    /// Each frame is downmixed to mono by averaging its channels; a trailing
    /// partial frame is ignored. One overrun is counted per block that
    /// displaced the oldest samples from a full ring or pushed the fill past
    /// the high mark; in the latter case the oldest samples are popped until
    /// the fill is back at the target.
    pub fn write<T: InputSample>(&self, data: &[T], channels: usize) {
        if channels > 0 {
            let scale = 1.0 / channels as f32;
            let mut overrun = false;
            for frame in data.chunks_exact(channels) {
                let sum: f32 = frame.iter().map(|&s| s.to_mix()).sum();
                overrun |= self.ring.force_push(sum * scale).is_some();
            }
            let rate = self.capture_rate.load(Ordering::Acquire);
            if rate > 0 && self.ring.len() > high_mark_samples(rate) {
                let target = target_samples(rate);
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

    /// Set the mic sink's rate. Called from `Mixer::configure`; a change
    /// resets the resampler and the controller.
    pub fn set_sink_rate(&mut self, rate: u32) {
        if rate != self.sink_rate {
            self.sink_rate = rate;
            self.reset();
        }
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
    /// Outputs silence while priming, with no capture or sink rate, and for
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
        if capture_rate == 0 || self.sink_rate == 0 || !self.prime(capture_rate) {
            out.fill(0.0);
            return;
        }

        self.update_correction(out.len(), capture_rate);
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

    /// Prime once the ring holds the target fill. Returns whether the
    /// reader is primed.
    fn prime(&mut self, capture_rate: u32) -> bool {
        if self.primed {
            return true;
        }
        if self.shared.ring.len() < target_samples(capture_rate).max(2) {
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

    /// One PI controller step per block, on the EMA-smoothed fill error.
    fn update_correction(&mut self, frames: usize, capture_rate: u32) {
        let fill = self.shared.ring.len() as f64;
        let target = target_samples(capture_rate) as f64;
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

    fn write_mono(shared: &PassthroughShared, samples: usize, value: f32) {
        let block = vec![value; samples];
        shared.write(&block, 1);
    }

    #[test]
    fn sizes_follow_the_rate() {
        assert_eq!(target_samples(48_000), 1_440);
        assert_eq!(high_mark_samples(48_000), 3_840);
        assert_eq!(target_samples(44_100), 1_323);
        assert_eq!(RING_CAPACITY, 96_000);
        assert!(high_mark_samples(MAX_CAPTURE_RATE) < RING_CAPACITY);
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
    fn overrun_past_the_high_mark_drops_the_oldest_audio() {
        let shared = shared_at(48_000);
        let high = high_mark_samples(48_000);
        let target = target_samples(48_000);
        // A ramp in [0, 1), so every sample is distinct and none clamps.
        let ramp: Vec<f32> = (0..=high).map(|i| i as f32 * 1e-5).collect();
        shared.write(&ramp, 1);
        assert_eq!(shared.overruns(), 1);
        assert_eq!(shared.fill(), target);
        let oldest_kept = shared.ring.pop().unwrap_or(f32::NAN);
        assert_eq!(
            oldest_kept,
            ramp[high + 1 - target],
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

        let mut out = vec![1.0f32; target + 100];
        reader.read(&mut out);
        assert_eq!(shared.underruns(), 1);
        assert!(!reader.is_primed());
        assert!(out[..target - 10].iter().all(|&s| (s - 0.5).abs() < 1e-6));
        assert!(
            out[target + 10..].iter().all(|&s| s == 0.0),
            "rest of the block is silent"
        );

        // Not enough to re-prime: silence, and no further underrun.
        write_mono(&shared, target / 2, 0.5);
        let mut out = [1.0f32; 64];
        reader.read(&mut out);
        assert_eq!(out, [0.0; 64]);
        assert_eq!(shared.underruns(), 1);

        write_mono(&shared, target, 0.25);
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

        // A new rate re-primes at the new target.
        shared.begin_capture(24_000);
        write_mono(&shared, target_samples(24_000), 0.5);
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
        shared.write(&ramp, 1);
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
        shared.write(&ramp, 1);
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
        audible: bool,
    }

    const CAPTURE_BLOCK: usize = 480;
    const SINK_BLOCK: usize = 512;
    const SIMULATED: f64 = 600.0;
    const SETTLE: f64 = 5.0;

    fn simulate(clocks: &Clocks) -> DriftResult {
        let shared = shared_at(clocks.capture_rate);
        let mut reader = shared.reader();
        reader.set_sink_rate(clocks.sink_rate);
        let target = target_samples(clocks.capture_rate) as f64;

        let mut input = vec![0.0f32; CAPTURE_BLOCK];
        let mut output = vec![0.0f32; SINK_BLOCK];
        let mut written = 0usize;
        let (mut capture_blocks, mut sink_blocks) = (1u64, 1u64);
        let capture_period = CAPTURE_BLOCK as f64 / clocks.capture_clock;
        let sink_period = SINK_BLOCK as f64 / clocks.sink_clock;

        let mut max_deviation = 0.0f64;
        let (mut late_sum, mut late_count) = (0.0f64, 0u64);
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
                    let deviation =
                        (shared.fill() as f64 - target) / f64::from(clocks.capture_rate);
                    max_deviation = max_deviation.max(deviation.abs());
                }
                reader.read(&mut output);
                audible |= output.iter().any(|&s| s.abs() > 0.1);
                if sink_at > SIMULATED - 60.0 {
                    late_sum += reader.correction();
                    late_count += 1;
                }
                sink_blocks += 1;
            }
        }
        DriftResult {
            max_deviation,
            underruns: shared.underruns(),
            overruns: shared.overruns(),
            late_correction: late_sum / late_count.max(1) as f64,
            audible,
        }
    }

    fn assert_holds_the_target(clocks: Clocks) {
        let result = simulate(&clocks);
        let label = format!(
            "{} Hz at {} -> {} Hz at {}",
            clocks.capture_rate, clocks.capture_clock, clocks.sink_rate, clocks.sink_clock
        );
        assert_eq!(result.underruns, 0, "{label}: underruns");
        assert_eq!(result.overruns, 0, "{label}: overruns");
        assert!(
            result.max_deviation <= 0.020,
            "{label}: fill deviated {:.1} ms",
            result.max_deviation * 1000.0
        );
        assert!(result.audible, "{label}: the passthrough carried audio");
        // The correction settles on the real clock ratio.
        let expected = (clocks.capture_clock / f64::from(clocks.capture_rate))
            / (clocks.sink_clock / f64::from(clocks.sink_rate))
            - 1.0;
        assert!(
            (result.late_correction - expected).abs() < 20e-6,
            "{label}: correction {:.1} ppm, expected {:.1} ppm",
            result.late_correction * 1e6,
            expected * 1e6
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
}
