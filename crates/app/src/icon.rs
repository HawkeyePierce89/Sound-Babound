//! The tray icon, drawn in code: a speaker with two sound waves.
//!
//! The icon is a single color on transparent with anti-aliased edges. On
//! macOS it is drawn black and used as a template icon (the system recolors
//! it for light and dark menu bars); other platforms pick a color that reads
//! on both light and dark taskbars. No image asset or decoder is involved.

use anyhow::{Context, Result};
use tray_icon::Icon;

/// Width and height in pixels.
pub const SIZE: u32 = 32;

/// Sub-samples per pixel side used for anti-aliasing.
const SUPERSAMPLE: u32 = 4;

/// Whether the point `(x, y)` (in pixel units, origin top-left) is inside
/// the speaker shape.
fn inside(x: f32, y: f32) -> bool {
    let dy = (y - 16.0).abs();
    // Speaker body.
    if (5.0..=10.0).contains(&x) && dy <= 4.0 {
        return true;
    }
    // Cone, widening from the body to the mouth.
    if (10.0..=17.0).contains(&x) && dy <= 4.0 + (x - 10.0) * (6.0 / 7.0) {
        return true;
    }
    // Two arcs centred on the mouth, limited to a ±50° wedge.
    let (cx, cy) = (17.0, 16.0);
    let (rx, ry) = (x - cx, y - cy);
    if rx <= 2.0 {
        return false;
    }
    let angle = ry.atan2(rx).abs();
    if angle > 50f32.to_radians() {
        return false;
    }
    let r = (rx * rx + ry * ry).sqrt();
    (5.0..=7.0).contains(&r) || (9.5..=11.5).contains(&r)
}

/// The icon in `color` as RGBA bytes, `SIZE * SIZE * 4` long, row-major.
pub fn rgba(color: [u8; 3]) -> Vec<u8> {
    let samples = SUPERSAMPLE * SUPERSAMPLE;
    let mut buffer = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for py in 0..SIZE {
        for px in 0..SIZE {
            let mut covered = 0;
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = px as f32 + (sx as f32 + 0.5) / SUPERSAMPLE as f32;
                    let y = py as f32 + (sy as f32 + 0.5) / SUPERSAMPLE as f32;
                    if inside(x, y) {
                        covered += 1;
                    }
                }
            }
            let alpha = (covered * 255 + samples / 2) / samples;
            buffer.extend_from_slice(&[color[0], color[1], color[2], alpha as u8]);
        }
    }
    buffer
}

/// The tray icon in `color`.
pub fn tray_icon(color: [u8; 3]) -> Result<Icon> {
    Icon::from_rgba(rgba(color), SIZE, SIZE).context("failed to create the tray icon")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: [u8; 3] = [0, 0, 0];

    fn alpha_at(buffer: &[u8], x: u32, y: u32) -> u8 {
        buffer[((y * SIZE + x) * 4 + 3) as usize]
    }

    #[test]
    fn buffer_has_one_rgba_pixel_per_position() {
        assert_eq!(rgba(BLACK).len(), (SIZE * SIZE * 4) as usize);
    }

    #[test]
    fn icon_is_black_with_varying_alpha() {
        let buffer = rgba(BLACK);
        assert!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .all(|px| px[..3] == [0, 0, 0])
        );
        let alphas: Vec<u8> = buffer.as_chunks::<4>().0.iter().map(|px| px[3]).collect();
        let opaque = alphas.iter().filter(|&&a| a == 255).count();
        let clear = alphas.iter().filter(|&&a| a == 0).count();
        let partial = alphas.len() - opaque - clear;
        assert!(opaque > 50, "too few opaque pixels: {opaque}");
        assert!(clear > alphas.len() / 2, "too few clear pixels: {clear}");
        assert!(partial > 0, "edges are not anti-aliased");
    }

    #[test]
    fn shape_lands_where_expected() {
        let buffer = rgba(BLACK);
        // Corners are clear, the body and the cone are solid.
        assert_eq!(alpha_at(&buffer, 0, 0), 0);
        assert_eq!(alpha_at(&buffer, SIZE - 1, SIZE - 1), 0);
        assert_eq!(alpha_at(&buffer, 7, 16), 255);
        assert_eq!(alpha_at(&buffer, 14, 16), 255);
        // Gap between the cone and the first wave, then the first wave.
        assert_eq!(alpha_at(&buffer, 19, 16), 0);
        assert_eq!(alpha_at(&buffer, 23, 16), 255);
    }

    #[test]
    fn tray_icon_accepts_the_buffer() {
        tray_icon(BLACK).unwrap();
    }

    #[test]
    fn drawing_is_deterministic() {
        assert_eq!(rgba(BLACK), rgba(BLACK));
    }

    #[test]
    fn color_only_changes_rgb() {
        let black = rgba(BLACK);
        let colored = rgba([10, 20, 30]);
        for (b, c) in black
            .as_chunks::<4>()
            .0
            .iter()
            .zip(colored.as_chunks::<4>().0)
        {
            assert_eq!(&c[..3], &[10, 20, 30]);
            assert_eq!(b[3], c[3]);
        }
    }
}
