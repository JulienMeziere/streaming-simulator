//! Shared scaffolding for the per-module `#[cfg(test)] mod tests` blocks.
//!
//! ```ignore
//! let mut codec = OpusProcessor::new();
//! codec.initialize(host_rate, 2, BLOCK_SIZE);
//! let peak = drive_with_sine_and_measure_planar(
//!     host_rate, BLOCK_SIZE, 2.0, 0.25, 1_000.0, 0.3,
//!     |block, n| codec.process_planar(block, OpusMode::Opus { bitrate_kbps: 64 }, n),
//! );
//! assert!(peak > 0.05);
//! ```

#![cfg(test)]

use std::f32::consts::PI;

/// Stereo sine block at `cursor` host-rate samples since test start.
/// Both channels carry the same waveform — our codec tests don't care
/// about stereo separation, only "did audio come through".
pub fn sine_stereo(rate: u32, freq: f32, amp: f32, n: usize, cursor: usize) -> Vec<Vec<f32>> {
    let mut out = vec![vec![0.0f32; n]; 2];
    for s in 0..n {
        let t = (cursor + s) as f32 / rate as f32;
        let v = (2.0 * PI * freq * t).sin() * amp;
        out[0][s] = v;
        out[1][s] = v;
    }
    out
}

/// Max absolute sample across every channel — proxy for "produced audio".
pub fn peak(planar: &[Vec<f32>]) -> f32 {
    let mut p = 0.0f32;
    for ch in planar {
        for &s in ch {
            p = p.max(s.abs());
        }
    }
    p
}

/// Root-mean-square across every sample of every channel. Less spike-
/// sensitive than `peak`; used to check FM auto-makeup loudness matching.
pub fn rms(planar: &[Vec<f32>]) -> f32 {
    let mut sum_sq = 0.0f64;
    let mut count = 0usize;
    for ch in planar {
        for &s in ch {
            sum_sq += (s as f64) * (s as f64);
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        ((sum_sq / count as f64) as f32).sqrt()
    }
}

/// Wrap a planar `Vec<Vec<f32>>` as a nih-plug `Buffer` for the closure.
/// Centralises the one `unsafe` block needed by `Buffer::set_slices`.
pub fn with_buffer<R>(
    planar: &mut [Vec<f32>],
    n: usize,
    f: impl FnOnce(&mut nih_plug::buffer::Buffer) -> R,
) -> R {
    let mut buffer = nih_plug::buffer::Buffer::default();
    // SAFETY: `planar` outlives the call to `f`; the raw slices alias the
    // same backing storage, and `buffer` is dropped before we return.
    unsafe {
        buffer.set_slices(n, |slices| {
            slices.clear();
            for ch in planar.iter_mut() {
                slices.push(std::slice::from_raw_parts_mut(ch.as_mut_ptr(), n));
            }
        });
    }
    f(&mut buffer)
}

/// Drive an in-place planar closure with a sine for `total_seconds`,
/// return the peak observed after `warmup_seconds`. Closure signature is
/// `(block, n_samples)`, matching the codec processors' `process_planar`.
pub fn drive_with_sine_and_measure_planar(
    rate: u32,
    block_size: usize,
    total_seconds: f32,
    warmup_seconds: f32,
    freq: f32,
    amp: f32,
    mut process_block: impl FnMut(&mut [Vec<f32>], usize),
) -> f32 {
    let total = (rate as f32 * total_seconds) as usize;
    let warmup = (rate as f32 * warmup_seconds) as usize;
    let mut block: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
    let mut peak_observed = 0.0f32;
    let mut cursor = 0usize;
    while cursor + block_size <= total {
        for s in 0..block_size {
            let t = (cursor + s) as f32 / rate as f32;
            let v = (2.0 * PI * freq * t).sin() * amp;
            block[0][s] = v;
            block[1][s] = v;
        }
        process_block(&mut block, block_size);
        if cursor >= warmup {
            let p = peak(&block);
            if p > peak_observed {
                peak_observed = p;
            }
        }
        cursor += block_size;
    }
    peak_observed
}

/// Like [`drive_with_sine_and_measure_planar`] but the closure has separate
/// input + output buffers (`process_planar(&input, &mut output)` flavour).
pub fn drive_with_sine_io_and_measure_planar(
    rate: u32,
    block_size: usize,
    total_seconds: f32,
    warmup_seconds: f32,
    freq: f32,
    amp: f32,
    mut process_block: impl FnMut(&[Vec<f32>], &mut [Vec<f32>]),
) -> f32 {
    let total = (rate as f32 * total_seconds) as usize;
    let warmup = (rate as f32 * warmup_seconds) as usize;
    let mut block_in: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
    let mut block_out: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
    let mut peak_observed = 0.0f32;
    let mut cursor = 0usize;
    while cursor + block_size <= total {
        for s in 0..block_size {
            let t = (cursor + s) as f32 / rate as f32;
            let v = (2.0 * PI * freq * t).sin() * amp;
            block_in[0][s] = v;
            block_in[1][s] = v;
        }
        process_block(&block_in, &mut block_out);
        if cursor >= warmup {
            let p = peak(&block_out);
            if p > peak_observed {
                peak_observed = p;
            }
        }
        cursor += block_size;
    }
    peak_observed
}

/// Like [`drive_with_sine_and_measure_planar`] but with a nih-plug
/// `Buffer` instead of a planar `Vec`. Used by the BT orchestrator tests.
pub fn drive_with_sine_and_measure_buffer(
    rate: u32,
    block_size: usize,
    total_seconds: f32,
    warmup_seconds: f32,
    freq: f32,
    amp: f32,
    mut process_block: impl FnMut(&mut nih_plug::buffer::Buffer),
) -> f32 {
    let total = (rate as f32 * total_seconds) as usize;
    let warmup = (rate as f32 * warmup_seconds) as usize;
    let mut planar: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
    let mut peak_observed = 0.0f32;
    let mut cursor = 0usize;
    while cursor + block_size <= total {
        for s in 0..block_size {
            let t = (cursor + s) as f32 / rate as f32;
            let v = (2.0 * PI * freq * t).sin() * amp;
            planar[0][s] = v;
            planar[1][s] = v;
        }
        with_buffer(&mut planar, block_size, &mut process_block);
        if cursor >= warmup {
            let p = peak(&planar);
            if p > peak_observed {
                peak_observed = p;
            }
        }
        cursor += block_size;
    }
    peak_observed
}

#[cfg(test)]
mod tests {
    //! Self-tests so a regression here doesn't silently distort the suite.
    use super::*;

    #[test]
    fn sine_stereo_amplitude_matches_amp() {
        // 4800 samples at 48 kHz × 1 kHz = 100 cycles → guaranteed peak.
        let block = sine_stereo(48_000, 1_000.0, 0.5, 4_800, 0);
        assert!((peak(&block) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn sine_stereo_silence_when_amp_zero() {
        let block = sine_stereo(44_100, 440.0, 0.0, 1_024, 0);
        assert_eq!(peak(&block), 0.0);
    }

    /// RMS of sin(t) is √(½) ≈ 0.707.
    #[test]
    fn rms_of_unit_sine_is_about_root_two_over_two() {
        let block = sine_stereo(48_000, 1_000.0, 1.0, 48_000, 0);
        let r = rms(&block);
        assert!((r - 0.7071).abs() < 0.01, "rms = {r}");
    }

    #[test]
    fn rms_of_silence_is_zero() {
        let block = vec![vec![0.0f32; 256]; 2];
        assert_eq!(rms(&block), 0.0);
    }

    #[test]
    fn rms_of_empty_block_is_zero() {
        let empty: Vec<Vec<f32>> = vec![];
        assert_eq!(rms(&empty), 0.0);
    }

    #[test]
    fn peak_handles_negative_samples() {
        let block = vec![vec![-0.9, 0.1, -0.3, 0.5], vec![0.0, 0.0, 0.0, 0.0]];
        assert!((peak(&block) - 0.9).abs() < 1e-6);
    }

    #[test]
    fn drive_with_sine_passthrough_returns_amp() {
        let measured = drive_with_sine_and_measure_planar(
            48_000,
            256,
            0.5,
            0.05,
            1_000.0,
            0.4,
            |_block, _n| {},
        );
        assert!((measured - 0.4).abs() < 1e-3);
    }

    #[test]
    fn drive_with_sine_buffer_runs_closure_each_block() {
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 4]; 2];
        with_buffer(&mut planar, 4, |buf| {
            assert_eq!(buf.samples(), 4);
            for ch in buf.as_slice() {
                for s in ch.iter_mut() {
                    *s = 0.5;
                }
            }
        });
        assert_eq!(planar[0], vec![0.5; 4]);
        assert_eq!(planar[1], vec![0.5; 4]);
    }
}
