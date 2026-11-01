//! Generates the embedded demo chime, `crates/app/assets/demo.wav`.
//!
//! The chime is deterministic: two sine partials (A5, then E6 120 ms later)
//! with exponential decay, a short attack ramp and a final fade so nothing
//! clicks. The output is 44.1 kHz mono 16-bit PCM on purpose: it exercises
//! resampling and mono-to-stereo mapping in the mixer.
//!
//! Usage: `cargo run -p babound-engine --example gen_demo_wav [OUT_PATH]`

use std::f64::consts::TAU;
use std::path::PathBuf;

const RATE: u32 = 44_100;
const DURATION_S: f64 = 1.5;
const PEAK: f64 = 0.6;

/// (frequency Hz, start s, amplitude, decay time constant s)
const PARTIALS: [(f64, f64, f64, f64); 2] = [(880.0, 0.0, 1.0, 0.35), (1318.51, 0.12, 0.8, 0.30)];

const ATTACK_S: f64 = 0.005;
const FADE_S: f64 = 0.05;

fn synthesize() -> Vec<i16> {
    let frames = (f64::from(RATE) * DURATION_S).round() as usize;
    let mut buf: Vec<f64> = (0..frames)
        .map(|i| {
            let t = i as f64 / f64::from(RATE);
            let mut v = 0.0;
            for (freq, start, amp, tau) in PARTIALS {
                if t < start {
                    continue;
                }
                let local = t - start;
                let attack = (local / ATTACK_S).min(1.0);
                v += amp * attack * (-local / tau).exp() * (TAU * freq * local).sin();
            }
            let fade = ((DURATION_S - t) / FADE_S).clamp(0.0, 1.0);
            v * fade
        })
        .collect();

    let max = buf.iter().fold(0.0f64, |m, s| m.max(s.abs()));
    if max > 0.0 {
        for s in &mut buf {
            *s *= PEAK / max;
        }
    }
    buf.iter()
        .map(|s| (s * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect()
}

fn wav_bytes(samples: &[i16]) -> Vec<u8> {
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    let block_align = CHANNELS * BITS / 8;
    let data_len = (samples.len() * usize::from(block_align)) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn main() -> std::io::Result<()> {
    let out = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../app/assets/demo.wav")
        });
    let bytes = wav_bytes(&synthesize());
    std::fs::write(&out, &bytes)?;
    println!("wrote {} ({} bytes)", out.display(), bytes.len());
    Ok(())
}
