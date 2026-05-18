//! 4-way Linkwitz-Riley crossover for the multiband processor. Tree of
//! 3 LR4 LP/HP pairs splits stereo input into sub / low / mid / high.
//!
//! LR4 = two cascaded matched-Q (≈ 0.7071) Butterworth biquads. LR4 LP +
//! LR4 HP at the same fc sums to flat magnitude (with a 360° phase
//! rotation), so the four bands recombine cleanly.

use crate::processor::biquad::{Biquad, BiquadCoeffs};

const LR4_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// One LR4 stage = two cascaded matched-Q biquads.
#[derive(Clone, Copy, Debug)]
struct Lr4Pair {
    stages: [Biquad; 2],
}

impl Lr4Pair {
    fn new() -> Self {
        Self {
            stages: [Biquad::new(); 2],
        }
    }

    fn set_lowpass(&mut self, cutoff_hz: f32, sample_rate: f32) {
        let coeffs = BiquadCoeffs::lowpass(cutoff_hz, sample_rate, LR4_Q);
        self.stages[0].set_coeffs(coeffs);
        self.stages[1].set_coeffs(coeffs);
    }

    fn set_highpass(&mut self, cutoff_hz: f32, sample_rate: f32) {
        let coeffs = BiquadCoeffs::highpass(cutoff_hz, sample_rate, LR4_Q);
        self.stages[0].set_coeffs(coeffs);
        self.stages[1].set_coeffs(coeffs);
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let s0 = self.stages[0].process(x);
        self.stages[1].process(s0)
    }

    fn reset(&mut self) {
        self.stages[0].reset();
        self.stages[1].reset();
    }
}

/// 4-way bank using 3 LR4 splits in a tree:
/// ```text
///   x ─┬─ LP(f1) ─────────────────── sub
///      └─ HP(f1) ─┬─ LP(f2) ──────── low
///                 └─ HP(f2) ─┬─ LP(f3) ─ mid
///                            └─ HP(f3) ─ high
/// ```
pub struct LinkwitzRileyBank {
    sub_lp: [Lr4Pair; 2],
    after_hp1: [Lr4Pair; 2],
    low_lp: [Lr4Pair; 2],
    after_hp2: [Lr4Pair; 2],
    mid_lp: [Lr4Pair; 2],
    high_hp: [Lr4Pair; 2],
}

impl LinkwitzRileyBank {
    pub fn new() -> Self {
        Self {
            sub_lp: [Lr4Pair::new(); 2],
            after_hp1: [Lr4Pair::new(); 2],
            low_lp: [Lr4Pair::new(); 2],
            after_hp2: [Lr4Pair::new(); 2],
            mid_lp: [Lr4Pair::new(); 2],
            high_hp: [Lr4Pair::new(); 2],
        }
    }

    /// Configure crossover frequencies. `freqs` = [f1, f2, f3] where
    /// f1 < f2 < f3.
    pub fn set_crossovers(&mut self, sample_rate: f32, freqs: [f32; 3]) {
        let [f1, f2, f3] = freqs;
        for ch in 0..2 {
            self.sub_lp[ch].set_lowpass(f1, sample_rate);
            self.after_hp1[ch].set_highpass(f1, sample_rate);
            self.low_lp[ch].set_lowpass(f2, sample_rate);
            self.after_hp2[ch].set_highpass(f2, sample_rate);
            self.mid_lp[ch].set_lowpass(f3, sample_rate);
            self.high_hp[ch].set_highpass(f3, sample_rate);
        }
    }

    /// Split one input sample (for channel `ch`) into four band
    /// outputs: [sub, low, mid, high].
    #[inline]
    pub fn split(&mut self, ch: usize, x: f32) -> [f32; 4] {
        let sub = self.sub_lp[ch].process(x);
        let hp1 = self.after_hp1[ch].process(x);
        let low = self.low_lp[ch].process(hp1);
        let hp2 = self.after_hp2[ch].process(hp1);
        let mid = self.mid_lp[ch].process(hp2);
        let high = self.high_hp[ch].process(hp2);
        [sub, low, mid, high]
    }

    pub fn reset(&mut self) {
        for ch in 0..2 {
            self.sub_lp[ch].reset();
            self.after_hp1[ch].reset();
            self.low_lp[ch].reset();
            self.after_hp2[ch].reset();
            self.mid_lp[ch].reset();
            self.high_hp[ch].reset();
        }
    }
}

impl Default for LinkwitzRileyBank {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3-stage LR4 tree reconstructs to ~±1.5 dB across the audio band
    /// (non-adjacent bands aren't perfectly phase-aligned). RMS rather
    /// than peak — peak is unreliable at HF due to sparse-sample aliasing.
    #[test]
    fn lr4_split_recombines_to_flat_input() {
        let fs = 48_000.0;
        let mut bank = LinkwitzRileyBank::new();
        bank.set_crossovers(fs, [100.0, 800.0, 4000.0]);

        for &test_hz in &[50.0_f32, 500.0, 2000.0, 6000.0, 10_000.0] {
            bank.reset();
            let block_size = 8192usize;
            let mut sum_in_sq: f64 = 0.0;
            let mut sum_out_sq: f64 = 0.0;
            let mut count: usize = 0;
            for s in 0..block_size {
                let t = s as f32 / fs;
                let x = (2.0 * std::f32::consts::PI * test_hz * t).sin() * 0.5;
                let bands = bank.split(0, x);
                let sum = bands[0] + bands[1] + bands[2] + bands[3];
                if s >= 2048 {
                    sum_in_sq += (x as f64) * (x as f64);
                    sum_out_sq += (sum as f64) * (sum as f64);
                    count += 1;
                }
            }
            let rms_in = (sum_in_sq / count as f64).sqrt();
            let rms_sum = (sum_out_sq / count as f64).sqrt();
            let ratio_db = 20.0 * (rms_sum / rms_in).log10();
            assert!(
                ratio_db.abs() < 1.5,
                "LR4 reconstruction at {} Hz off by {:.2} dB (RMS)",
                test_hz,
                ratio_db
            );
        }
    }

    /// Each band must pass its own range and reject the others.
    #[test]
    fn lr4_bands_isolate_their_ranges() {
        let fs = 48_000.0;
        let mut bank = LinkwitzRileyBank::new();
        bank.set_crossovers(fs, [100.0, 800.0, 4000.0]);

        let cases = [
            (50.0_f32, 0usize),
            (400.0, 1),
            (2000.0, 2),
            (8000.0, 3),
        ];
        for (test_hz, expected_band) in cases {
            bank.reset();
            let block_size = 4096usize;
            let mut peaks = [0.0f32; 4];
            for s in 0..block_size {
                let t = s as f32 / fs;
                let x = (2.0 * std::f32::consts::PI * test_hz * t).sin() * 0.5;
                let bands = bank.split(0, x);
                if s >= 1024 {
                    for i in 0..4 {
                        peaks[i] = peaks[i].max(bands[i].abs());
                    }
                }
            }
            let max_idx = peaks
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0;
            assert_eq!(
                max_idx, expected_band,
                "{} Hz should pick band {}, got band {}",
                test_hz, expected_band, max_idx
            );
        }
    }

    #[test]
    fn lr4_bank_reset_clears_state() {
        let fs = 48_000.0_f32;
        let mut bank = LinkwitzRileyBank::new();
        bank.set_crossovers(fs, [100.0, 800.0, 4000.0]);
        for s in 0..512 {
            let t = s as f32 / fs;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.5;
            let _ = bank.split(0, x);
        }
        bank.reset();
        let bands = bank.split(0, 0.0);
        assert_eq!(bands, [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn lr4_bank_default_matches_new() {
        let _ = LinkwitzRileyBank::default();
    }
}
