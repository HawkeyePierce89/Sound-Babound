//! Sample-format conversion from the f32 mix to the device format.
//!
//! These are pure, allocation-free functions that run inside the audio
//! callback. Conversion goes through cpal's [`FromSample`], so every integer
//! format gets the same scaling; input is clamped to [-1, 1] first (and NaN
//! becomes silence) so a stray value can never wrap around.

use cpal::{FromSample, Sample};

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
/// is left untouched. Works for `f32`, `i16`, `u16`, `i32`, `u8` and every
/// other cpal sample type.
#[inline]
pub fn write_samples<T>(src: &[f32], dst: &mut [T])
where
    T: Sample + FromSample<f32>,
{
    for (out, &sample) in dst.iter_mut().zip(src) {
        *out = T::from_sample(clamp(sample));
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

    fn convert<T: Sample + FromSample<f32>>(src: &[f32]) -> Vec<T> {
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
