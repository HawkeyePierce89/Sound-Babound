//! Sample-format conversion between the f32 mix and the device formats:
//! [`OutputSample`] for the output streams, [`InputSample`] for the capture.
//!
//! These are pure, allocation-free functions that run inside the audio
//! callback. Conversion goes through cpal's `FromSample`, so every integer
//! format gets the same scaling; input is clamped to [-1, 1] first (and NaN
//! becomes silence) so a stray value can never wrap around.
//!
//! The 24-bit formats are the exception: dasp maps +1.0 to `1 << 23` (one
//! past the I24 maximum), which wraps to full negative once packed into 24
//! bits. They convert through `i32`, which saturates, instead.

use cpal::{I24, Sample, SizedSample, U24};

/// A device sample type the f32 mix can be written to.
pub trait OutputSample: SizedSample {
    /// Convert a sample already clamped to [-1, 1], saturating at full scale.
    fn from_mix(sample: f32) -> Self;
}

macro_rules! direct_output_sample {
    ($($t:ty),*) => {$(
        impl OutputSample for $t {
            #[inline]
            fn from_mix(sample: f32) -> Self {
                <$t>::from_sample(sample)
            }
        }
    )*};
}

direct_output_sample!(f32, f64, i8, i16, i32, i64, u8, u16, u32, u64);

impl OutputSample for I24 {
    #[inline]
    fn from_mix(sample: f32) -> Self {
        I24::from_sample(i32::from_sample(sample))
    }
}

impl OutputSample for U24 {
    #[inline]
    fn from_mix(sample: f32) -> Self {
        U24::from_sample(i32::from_sample(sample))
    }
}

/// A device sample type the capture can read from.
pub trait InputSample: SizedSample {
    /// Convert to f32 in [-1, 1]. NaN becomes silence.
    fn to_mix(self) -> f32;
}

macro_rules! input_sample {
    ($($t:ty),*) => {$(
        impl InputSample for $t {
            #[inline]
            fn to_mix(self) -> f32 {
                clamp(f32::from_sample(self))
            }
        }
    )*};
}

// dasp scales I24 and U24 by 2^23 without overflow on the way in, so unlike
// the output side they need no detour through i32.
input_sample!(f32, f64, i8, i16, I24, i32, i64, u8, u16, U24, u32, u64);

/// Clamp a mixed sample to the valid range. NaN maps to silence.
#[inline]
fn clamp(sample: f32) -> f32 {
    if sample.is_nan() {
        0.0
    } else {
        sample.clamp(-1.0, 1.0)
    }
}

/// Convert `src` into `dst`, sample by sample.
///
/// Only `min(src.len(), dst.len())` samples are written; the rest of `dst`
/// is left untouched. Works for every cpal PCM sample type.
#[inline]
pub fn write_samples<T: OutputSample>(src: &[f32], dst: &mut [T]) {
    for (out, &sample) in dst.iter_mut().zip(src) {
        *out = T::from_mix(clamp(sample));
    }
}

/// Fill `dst` with the format's silence value (0 for signed and float
/// formats, the midpoint for unsigned ones).
#[inline]
pub fn fill_silence<T: Sample>(dst: &mut [T]) {
    dst.fill(T::EQUILIBRIUM);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert<T: OutputSample>(src: &[f32]) -> Vec<T> {
        let mut out = vec![T::EQUILIBRIUM; src.len()];
        write_samples(src, &mut out);
        out
    }

    const BOUNDARIES: [f32; 8] = [-1.0, 0.0, 1.0, 0.5, -0.5, 1.5, -2.0, f32::NAN];

    #[test]
    fn i16_boundaries_and_clipping() {
        let out: Vec<i16> = convert(&BOUNDARIES);
        assert_eq!(
            out,
            [
                i16::MIN,
                0,
                i16::MAX,
                16_384,
                -16_384,
                i16::MAX,
                i16::MIN,
                0
            ]
        );
    }

    #[test]
    fn u16_boundaries_and_clipping() {
        let out: Vec<u16> = convert(&BOUNDARIES);
        assert_eq!(
            out,
            [0, 32_768, u16::MAX, 49_152, 16_384, u16::MAX, 0, 32_768]
        );
    }

    #[test]
    fn f32_passes_through_clamped() {
        let out: Vec<f32> = convert(&BOUNDARIES);
        assert_eq!(out, [-1.0, 0.0, 1.0, 0.5, -0.5, 1.0, -1.0, 0.0]);
    }

    #[test]
    fn i32_and_u8_boundaries() {
        let out: Vec<i32> = convert(&[-1.0, 0.0, 1.0, 3.0]);
        assert_eq!(out, [i32::MIN, 0, i32::MAX, i32::MAX]);

        let out: Vec<u8> = convert(&[-1.0, 0.0, 1.0, -3.0]);
        assert_eq!(out, [0, 128, u8::MAX, 0]);
    }

    #[test]
    fn twenty_four_and_sixty_four_bit_boundaries() {
        let out: Vec<I24> = convert(&[-1.0, 0.0, 1.0, 3.0, f32::NAN]);
        let raw: Vec<i32> = out.iter().map(|s| s.inner()).collect();
        assert_eq!(raw, [-(1 << 23), 0, (1 << 23) - 1, (1 << 23) - 1, 0]);

        let out: Vec<U24> = convert(&[-1.0, 0.0, 1.0, -3.0]);
        let raw: Vec<i32> = out.iter().map(|s| s.inner()).collect();
        assert_eq!(raw, [0, 1 << 23, (1 << 24) - 1, 0]);

        let out: Vec<i64> = convert(&[-1.0, 0.0, 2.0]);
        assert_eq!(out[0], i64::MIN);
        assert_eq!(out[1], 0);
        assert!(out[2] > i64::MAX / 2);

        let out: Vec<u64> = convert(&[-1.0, 0.0]);
        assert_eq!(out, [0, 1 << 63]);

        let mut i = [I24::new_unchecked(5); 2];
        fill_silence(&mut i);
        assert_eq!(i.map(|s| s.inner()), [0; 2]);
        let mut u = [U24::new_unchecked(5); 2];
        fill_silence(&mut u);
        assert_eq!(u.map(|s| s.inner()), [1 << 23; 2]);
    }

    #[test]
    fn input_conversion_at_full_scale() {
        assert_eq!(i16::MIN.to_mix(), -1.0);
        assert_eq!(0i16.to_mix(), 0.0);
        assert_eq!(i16::MAX.to_mix(), 32_767.0 / 32_768.0);
        assert_eq!(i8::MIN.to_mix(), -1.0);
        assert_eq!(i32::MIN.to_mix(), -1.0);
        assert!(i32::MAX.to_mix() > 0.999_999);
        assert_eq!(i64::MIN.to_mix(), -1.0);
        assert!(i64::MAX.to_mix() > 0.999_999);

        assert_eq!(0u8.to_mix(), -1.0);
        assert_eq!(128u8.to_mix(), 0.0);
        assert_eq!(u8::MAX.to_mix(), 127.0 / 128.0);
        assert_eq!(0u16.to_mix(), -1.0);
        assert_eq!(32_768u16.to_mix(), 0.0);
        assert_eq!(0u32.to_mix(), -1.0);
        assert_eq!(0u64.to_mix(), -1.0);
        assert_eq!((1u64 << 63).to_mix(), 0.0);

        assert_eq!(I24::new_unchecked(-(1 << 23)).to_mix(), -1.0);
        assert_eq!(I24::new_unchecked(0).to_mix(), 0.0);
        let max = I24::new_unchecked((1 << 23) - 1).to_mix();
        assert!(max > 0.999_999 && max < 1.0, "{max}");
        assert_eq!(U24::new_unchecked(0).to_mix(), -1.0);
        assert_eq!(U24::new_unchecked(1 << 23).to_mix(), 0.0);
        let max = U24::new_unchecked((1 << 24) - 1).to_mix();
        assert!(max > 0.999_999 && max < 1.0, "{max}");

        assert_eq!(0.5f32.to_mix(), 0.5);
        assert_eq!(1.5f32.to_mix(), 1.0);
        assert_eq!((-3.0f64).to_mix(), -1.0);
        assert_eq!(f32::NAN.to_mix(), 0.0);
        assert_eq!(f64::NAN.to_mix(), 0.0);
    }

    #[test]
    fn writes_only_the_overlap() {
        let mut out = [7i16; 4];
        write_samples(&[0.0, 0.0], &mut out);
        assert_eq!(out, [0, 0, 7, 7]);

        let mut out = [7i16; 2];
        write_samples(&[0.0, 0.0, 0.0, 0.0], &mut out);
        assert_eq!(out, [0, 0]);
    }

    #[test]
    fn silence_is_the_format_midpoint() {
        let mut u = [1u16; 3];
        fill_silence(&mut u);
        assert_eq!(u, [32_768; 3]);

        let mut i = [5i16; 3];
        fill_silence(&mut i);
        assert_eq!(i, [0; 3]);

        let mut b = [5u8; 2];
        fill_silence(&mut b);
        assert_eq!(b, [128; 2]);

        let mut f = [0.3f32; 2];
        fill_silence(&mut f);
        assert_eq!(f, [0.0; 2]);
    }
}
