//! Direct Form II Transposed biquad with Robert Bristow-Johnson cookbook
//! coefficient generators. DF2T minimises float rounding error vs DF1 and
//! uses two state variables per channel regardless of filter order.

/// Pre-computed coefficients. Pure data; runtime state lives in [`Biquad`].
#[derive(Clone, Copy, Debug)]
pub struct BiquadCoeffs {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl BiquadCoeffs {
    /// Pass-through. Default state before [`Biquad::set_coeffs`] runs.
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    /// 2nd-order lowpass. `q = 0.7071` ≈ Butterworth single-biquad. For
    /// 4th-order Butterworth, cascade with q1 ≈ 0.5412 and q2 ≈ 1.3066.
    pub fn lowpass(cutoff_hz: f32, sample_rate: f32, q: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * cutoff_hz / sample_rate;
        let cos_omega = omega.cos();
        let sin_omega = omega.sin();
        let alpha = sin_omega / (2.0 * q);

        let b0 = (1.0 - cos_omega) * 0.5;
        let b1 = 1.0 - cos_omega;
        let b2 = (1.0 - cos_omega) * 0.5;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha;

        // Normalise to a0 = 1.
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// 2nd-order highpass. Pair matching-Q LP + HP for Linkwitz-Riley
    /// crossovers (LR4 = two cascaded matched-Q LP + two cascaded HP).
    pub fn highpass(cutoff_hz: f32, sample_rate: f32, q: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * cutoff_hz / sample_rate;
        let cos_omega = omega.cos();
        let sin_omega = omega.sin();
        let alpha = sin_omega / (2.0 * q);

        let b0 = (1.0 + cos_omega) * 0.5;
        let b1 = -(1.0 + cos_omega);
        let b2 = (1.0 + cos_omega) * 0.5;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Low-shelf with `gain_db` boost (+) or cut (−). `q = 0.7071` is the
    /// resonance-free Butterworth-shaped transition.
    pub fn low_shelf(freq_hz: f32, sample_rate: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
        let cos_omega = omega.cos();
        let sin_omega = omega.sin();
        let alpha = sin_omega / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) - (a - 1.0) * cos_omega + two_sqrt_a_alpha);
        let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_omega);
        let b2 = a * ((a + 1.0) - (a - 1.0) * cos_omega - two_sqrt_a_alpha);
        let a0 = (a + 1.0) + (a - 1.0) * cos_omega + two_sqrt_a_alpha;
        let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_omega);
        let a2 = (a + 1.0) + (a - 1.0) * cos_omega - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// High-shelf — same conventions as [`Self::low_shelf`].
    pub fn high_shelf(freq_hz: f32, sample_rate: f32, gain_db: f32, q: f32) -> Self {
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * std::f32::consts::PI * freq_hz / sample_rate;
        let cos_omega = omega.cos();
        let sin_omega = omega.sin();
        let alpha = sin_omega / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let b0 = a * ((a + 1.0) + (a - 1.0) * cos_omega + two_sqrt_a_alpha);
        let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_omega);
        let b2 = a * ((a + 1.0) + (a - 1.0) * cos_omega - two_sqrt_a_alpha);
        let a0 = (a + 1.0) - (a - 1.0) * cos_omega + two_sqrt_a_alpha;
        let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_omega);
        let a2 = (a + 1.0) - (a - 1.0) * cos_omega - two_sqrt_a_alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// 2nd-order bandpass, constant skirt gain (peak gain = q). Used in
    /// the MPX decoder for the L-R subcarrier.
    pub fn bandpass(center_hz: f32, sample_rate: f32, q: f32) -> Self {
        let omega = 2.0 * std::f32::consts::PI * center_hz / sample_rate;
        let cos_omega = omega.cos();
        let sin_omega = omega.sin();
        let alpha = sin_omega / (2.0 * q);

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_omega;
        let a2 = 1.0 - alpha;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

/// Runtime DF2T biquad — one instance per channel per filter stage.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    coeffs: BiquadCoeffs,
    z1: f32,
    z2: f32,
}

impl Biquad {
    pub fn new() -> Self {
        Self {
            coeffs: BiquadCoeffs::identity(),
            z1: 0.0,
            z2: 0.0,
        }
    }

    pub fn set_coeffs(&mut self, coeffs: BiquadCoeffs) {
        self.coeffs = coeffs;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process one sample using DF2T:
    /// ```text
    ///   y[n] = b0·x[n] + z1
    ///   z1   = b1·x[n] - a1·y[n] + z2
    ///   z2   = b2·x[n] - a2·y[n]
    /// ```
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.coeffs.b0 * x + self.z1;
        self.z1 = self.coeffs.b1 * x - self.coeffs.a1 * y + self.z2;
        self.z2 = self.coeffs.b2 * x - self.coeffs.a2 * y;
        y
    }
}

impl Default for Biquad {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    //! Magnitude tests use sine-driven peak measurement rather than
    //! re-deriving the cookbook formulas (which would be circular).
    use super::*;
    use std::f32::consts::PI;

    /// Steady-state magnitude at `freq_hz`: drive with a unit sine for 1 s,
    /// measure peak over the last 0.5 s.
    fn magnitude_at(biquad: &mut Biquad, freq_hz: f32, sample_rate: f32) -> f32 {
        let total = (sample_rate * 1.0) as usize;
        let measure_start = (sample_rate * 0.5) as usize;
        let mut peak = 0.0f32;
        for n in 0..total {
            let t = n as f32 / sample_rate;
            let x = (2.0 * PI * freq_hz * t).sin();
            let y = biquad.process(x).abs();
            if n >= measure_start && y > peak {
                peak = y;
            }
        }
        peak
    }

    fn make_biquad(coeffs: BiquadCoeffs) -> Biquad {
        let mut b = Biquad::new();
        b.set_coeffs(coeffs);
        b
    }

    // ── BiquadCoeffs::identity ────────────────────────────────────

    #[test]
    fn identity_passes_signal_through_unchanged() {
        let mut b = make_biquad(BiquadCoeffs::identity());
        let inputs: [f32; 5] = [1.0, -0.5, 0.25, -0.75, 0.1];
        for &x in &inputs {
            assert!((b.process(x) - x).abs() < 1e-6);
        }
    }

    // ── lowpass ────────────────────────────────────────────────────

    #[test]
    fn lowpass_passes_dc_attenuates_nyquist() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 0.7071));
        let pass = magnitude_at(&mut b, 100.0, sr);
        b.reset();
        let stop = magnitude_at(&mut b, 18_000.0, sr);
        assert!(pass > 0.9, "LP DC magnitude {pass:.3} too low");
        assert!(stop < 0.05, "LP HF magnitude {stop:.3} too high");
    }

    /// High Q → resonant peak at the cutoff.
    #[test]
    fn lowpass_q_changes_resonance_at_cutoff() {
        let sr = 48_000.0;
        let mut tame = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 0.7071));
        let mut peaky = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 4.0));
        let tame_at_corner = magnitude_at(&mut tame, 2_000.0, sr);
        let peaky_at_corner = magnitude_at(&mut peaky, 2_000.0, sr);
        assert!(
            peaky_at_corner > tame_at_corner,
            "high-Q LP {peaky_at_corner:.3} should exceed Butterworth-Q LP {tame_at_corner:.3}"
        );
    }

    // ── highpass ───────────────────────────────────────────────────

    #[test]
    fn highpass_passes_treble_attenuates_dc() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::highpass(2_000.0, sr, 0.7071));
        let stop = magnitude_at(&mut b, 100.0, sr);
        b.reset();
        let pass = magnitude_at(&mut b, 18_000.0, sr);
        assert!(stop < 0.05, "HP LF magnitude {stop:.3} too high");
        assert!(pass > 0.9, "HP HF magnitude {pass:.3} too low");
    }

    // ── bandpass ───────────────────────────────────────────────────

    #[test]
    fn bandpass_attenuates_dc_and_nyquist_passes_centre() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::bandpass(2_000.0, sr, 1.0));
        let centre = magnitude_at(&mut b, 2_000.0, sr);
        b.reset();
        let lf = magnitude_at(&mut b, 50.0, sr);
        b.reset();
        let hf = magnitude_at(&mut b, 22_000.0, sr);
        assert!(centre > 0.9, "BP centre magnitude {centre:.3} too low");
        assert!(lf < 0.1, "BP LF magnitude {lf:.3} too high");
        assert!(hf < 0.1, "BP HF magnitude {hf:.3} too high");
    }

    // ── low_shelf ──────────────────────────────────────────────────

    #[test]
    fn low_shelf_boost_lifts_low_band() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::low_shelf(200.0, sr, 6.0, 0.7071));
        let low = magnitude_at(&mut b, 50.0, sr);
        b.reset();
        let high = magnitude_at(&mut b, 8_000.0, sr);
        // +6 dB ≈ ratio 2.0; RBJ shelf has ~±2 dB ripple far from corner.
        assert!(low > 1.5, "low-shelf LF {low:.3} should be > 1.5 (+6 dB target)");
        assert!(
            (high - 1.0).abs() < 0.2,
            "low-shelf HF {high:.3} should be approximately unity"
        );
    }

    #[test]
    fn low_shelf_cut_drops_low_band() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::low_shelf(200.0, sr, -6.0, 0.7071));
        let low = magnitude_at(&mut b, 50.0, sr);
        assert!(low < 0.7, "low-shelf cut LF {low:.3} should be < 0.7 (-6 dB target)");
    }

    // ── high_shelf ─────────────────────────────────────────────────

    #[test]
    fn high_shelf_boost_lifts_high_band() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::high_shelf(5_000.0, sr, 6.0, 0.7071));
        let low = magnitude_at(&mut b, 100.0, sr);
        b.reset();
        let high = magnitude_at(&mut b, 12_000.0, sr);
        assert!(high > 1.5, "high-shelf HF {high:.3} should be > 1.5 (+6 dB target)");
        assert!(
            (low - 1.0).abs() < 0.2,
            "high-shelf LF {low:.3} should be approximately unity"
        );
    }

    #[test]
    fn high_shelf_cut_drops_high_band() {
        let sr = 48_000.0;
        let mut b = make_biquad(BiquadCoeffs::high_shelf(5_000.0, sr, -6.0, 0.7071));
        let high = magnitude_at(&mut b, 12_000.0, sr);
        assert!(high < 0.7, "high-shelf cut HF {high:.3} should be < 0.7 (-6 dB target)");
    }

    // ── runtime ────────────────────────────────────────────────────

    /// First output of a zero-state LP biquad is exactly `b0 * x`.
    #[test]
    fn process_first_sample_matches_df2t_formula() {
        let coeffs = BiquadCoeffs::lowpass(1_000.0, 48_000.0, 0.7071);
        let mut b = make_biquad(coeffs);
        let x = 0.5f32;
        let y = b.process(x);
        assert!((y - coeffs.b0 * x).abs() < 1e-6);
    }

    #[test]
    fn reset_clears_filter_state_to_zero() {
        let mut b = make_biquad(BiquadCoeffs::lowpass(1_000.0, 48_000.0, 0.7071));
        for n in 0..100 {
            let _ = b.process((n as f32 * 0.1).sin());
        }
        b.reset();
        assert_eq!(b.process(0.0), 0.0);
        assert_eq!(b.process(0.0), 0.0);
    }

    #[test]
    fn cascade_two_lowpass_steeper_than_one() {
        let sr = 48_000.0;
        let mut single = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 0.7071));
        let mut first = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 0.7071));
        let mut second = make_biquad(BiquadCoeffs::lowpass(2_000.0, sr, 0.7071));
        // 8 kHz, 2 octaves above cutoff: cascade must attenuate more.
        let total = (sr * 1.0) as usize;
        let measure_start = (sr * 0.5) as usize;
        let mut peak_single = 0.0f32;
        let mut peak_cascade = 0.0f32;
        for n in 0..total {
            let t = n as f32 / sr;
            let x = (2.0 * PI * 8_000.0 * t).sin();
            let y_single = single.process(x).abs();
            let y_cascade = second.process(first.process(x)).abs();
            if n >= measure_start {
                if y_single > peak_single {
                    peak_single = y_single;
                }
                if y_cascade > peak_cascade {
                    peak_cascade = y_cascade;
                }
            }
        }
        assert!(
            peak_cascade < peak_single,
            "cascade {peak_cascade:.4} should attenuate more than single {peak_single:.4}"
        );
    }

    #[test]
    fn default_biquad_is_identity() {
        let mut b = Biquad::default();
        assert!((b.process(0.7) - 0.7).abs() < 1e-6);
        assert!((b.process(-0.3) - -0.3).abs() < 1e-6);
    }

    #[test]
    fn coeffs_are_copy_and_clone() {
        let a = BiquadCoeffs::lowpass(1_000.0, 48_000.0, 0.7071);
        let b = a;
        let c = a.clone();
        assert_eq!(a.b0, b.b0);
        assert_eq!(a.b0, c.b0);
    }
}
