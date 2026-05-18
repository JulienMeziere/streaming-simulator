//! MPX decoder. 14 kHz LP recovers L+R; 23-53 kHz BPF + coherent
//! product-detection recovers L−R; matrix `L=(sum+diff)/2,
//! R=(sum-diff)/2`.

use super::{MOD_DIFF, MOD_SUM, PILOT_HZ};
use crate::processor::biquad::{Biquad, BiquadCoeffs};

pub(super) struct MpxDecoder {
    /// Coherent with the encoder — both start at 0 and advance by the
    /// same `phase_inc` at the same rate. No PLL needed since we don't
    /// simulate frequency drift.
    phase_19k: f32,
    phase_inc: f32,

    /// 4-pole Butterworth LP @ 14 kHz on the composite. Cutoff lowered
    /// from 15 kHz to give the 19 kHz pilot 5 kHz of headroom (4-pole
    /// gives ~24 dB rejection at 5 kHz away).
    sum_lp: [Biquad; 2],

    /// 23-53 kHz bandpass = HP(23 kHz) cascaded with LP(53 kHz),
    /// 4th-order Butterworth on each side. Unity-gain in the pass band
    /// is critical: a single cookbook BPF has peak gain = Q at center,
    /// and 4 cascaded would hit ~Q⁴ — would unbalance the L−R math.
    diff_bp_hp: [Biquad; 2],
    diff_bp_lp: [Biquad; 2],

    /// 4-pole LP @ 14 kHz on the product-detected diff to extract the
    /// L−R baseband from the `cos²(38k)` term.
    diff_lp: [Biquad; 2],
}

impl MpxDecoder {
    pub(super) fn new(mpx_rate: f32) -> Self {
        // 4th-order Butterworth = two cascaded biquads with these Qs.
        let sum_q = [0.5412_f32, 1.3066];

        let mut sum_lp = [Biquad::new(); 2];
        sum_lp[0].set_coeffs(BiquadCoeffs::lowpass(14_000.0, mpx_rate, sum_q[0]));
        sum_lp[1].set_coeffs(BiquadCoeffs::lowpass(14_000.0, mpx_rate, sum_q[1]));

        let mut diff_bp_hp = [Biquad::new(); 2];
        diff_bp_hp[0].set_coeffs(BiquadCoeffs::highpass(23_000.0, mpx_rate, sum_q[0]));
        diff_bp_hp[1].set_coeffs(BiquadCoeffs::highpass(23_000.0, mpx_rate, sum_q[1]));
        let mut diff_bp_lp = [Biquad::new(); 2];
        diff_bp_lp[0].set_coeffs(BiquadCoeffs::lowpass(53_000.0, mpx_rate, sum_q[0]));
        diff_bp_lp[1].set_coeffs(BiquadCoeffs::lowpass(53_000.0, mpx_rate, sum_q[1]));

        let mut diff_lp = [Biquad::new(); 2];
        diff_lp[0].set_coeffs(BiquadCoeffs::lowpass(14_000.0, mpx_rate, sum_q[0]));
        diff_lp[1].set_coeffs(BiquadCoeffs::lowpass(14_000.0, mpx_rate, sum_q[1]));

        Self {
            phase_19k: 0.0,
            phase_inc: 2.0 * std::f32::consts::PI * PILOT_HZ / mpx_rate,
            sum_lp,
            diff_bp_hp,
            diff_bp_lp,
            diff_lp,
        }
    }

    /// One composite sample → one stereo pair.
    #[inline]
    pub(super) fn decode(&mut self, composite: f32) -> (f32, f32) {
        // L+R recovery: LP(14k) → undo MOD_SUM.
        let s0 = self.sum_lp[0].process(composite);
        let sum_filtered = self.sum_lp[1].process(s0);
        let sum_lr = sum_filtered / MOD_SUM;

        // L−R recovery: BPF the subcarrier, product-detect with the
        // coherent 38 kHz reference. The product is
        // `MOD_DIFF·(L−R)·cos²(38k) = ½·MOD_DIFF·(L−R) + ½·MOD_DIFF·(L−R)·cos(76k)`;
        // the 76 kHz image is killed by `diff_lp`.
        let mut diff_subc = composite;
        for stage in &mut self.diff_bp_hp {
            diff_subc = stage.process(diff_subc);
        }
        for stage in &mut self.diff_bp_lp {
            diff_subc = stage.process(diff_subc);
        }

        let carrier_38k = (2.0 * self.phase_19k).cos();
        let diff_demod = diff_subc * carrier_38k;

        self.phase_19k += self.phase_inc;
        if self.phase_19k > 2.0 * std::f32::consts::PI {
            self.phase_19k -= 2.0 * std::f32::consts::PI;
        }

        let d0 = self.diff_lp[0].process(diff_demod);
        let diff_baseband = self.diff_lp[1].process(d0);
        // ×2/MOD_DIFF undoes the product-detect halving and the encode depth.
        let diff_lr = diff_baseband * 2.0 / MOD_DIFF;

        let l = (sum_lr + diff_lr) * 0.5;
        let r = (sum_lr - diff_lr) * 0.5;
        (l, r)
    }

    pub(super) fn reset(&mut self) {
        self.phase_19k = 0.0;
        for stage in &mut self.sum_lp {
            stage.reset();
        }
        for stage in &mut self.diff_bp_hp {
            stage.reset();
        }
        for stage in &mut self.diff_bp_lp {
            stage.reset();
        }
        for stage in &mut self.diff_lp {
            stage.reset();
        }
    }
}
