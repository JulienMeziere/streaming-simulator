//! Imperfect-channel model — applies reception-dependent degradation
//! between encoder and decoder.
//!
//! To attenuate stereo separation specifically (without ducking the
//! mono-compatible L+R or the pilot), we split the composite into
//! LF (≤22 kHz, audio + pilot) and HF (>22 kHz, L−R subcarrier),
//! scale the HF, then sum back. HF is computed as `composite − LF`
//! so the split stays phase-coherent without an extra HP stage.

use super::FmReception;
use crate::processor::biquad::{Biquad, BiquadCoeffs};

pub(super) struct FmChannel {
    pub(super) quality: FmReception,
    /// 4-pole Butterworth LP @ 22 kHz for the composite band split.
    band_lp: [Biquad; 2],
    /// XorShift PRNG → noise shaped to pink, then bandpassed to the
    /// 5-15 kHz band where it's most audible after de-emphasis.
    rng_state: u32,
    /// `pub(super)` so parent tests can observe `reset` clearing it.
    pub(super) noise_lp: f32,
    noise_bp: Biquad,
    /// 0.3 Hz LFO — slow audible multipath swirl on Fringe.
    /// `pub(super)` for the reset-observation test.
    pub(super) multipath_phase: f32,
    multipath_inc: f32,
}

impl FmChannel {
    pub(super) fn new(quality: FmReception, mpx_rate: f32) -> Self {
        let mut noise_bp = Biquad::new();
        noise_bp.set_coeffs(BiquadCoeffs::bandpass(8_000.0, mpx_rate, 0.7));
        // 4th-order Butterworth = two cascaded biquads with these Qs.
        let butter_q = [0.5412_f32, 1.3066];
        let mut band_lp = [Biquad::new(); 2];
        band_lp[0].set_coeffs(BiquadCoeffs::lowpass(22_000.0, mpx_rate, butter_q[0]));
        band_lp[1].set_coeffs(BiquadCoeffs::lowpass(22_000.0, mpx_rate, butter_q[1]));
        Self {
            quality,
            band_lp,
            rng_state: 0xACE1u32,
            noise_lp: 0.0,
            noise_bp,
            multipath_phase: 0.0,
            multipath_inc: 2.0 * std::f32::consts::PI * 0.3 / mpx_rate,
        }
    }

    /// XorShift, mapped to f32 in [−1.0, 1.0).
    #[inline]
    fn rand(&mut self) -> f32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x;
        (x as i32 as f32) / (i32::MAX as f32)
    }

    /// Test-only convenience — production calls `process_with_noise`
    /// directly with the match hoisted out of the per-sample loop.
    #[cfg(test)]
    pub(super) fn process(&mut self, composite: f32) -> f32 {
        match self.quality {
            FmReception::Pristine => composite,
            FmReception::Urban => self.process_with_noise(composite, 0.0032, 0.5),
            FmReception::Fringe => self.process_with_noise(composite, 0.0316, 0.126),
        }
    }

    /// Apply HF noise + L−R subcarrier attenuation. `subcarrier_gain`:
    /// 0.5 = −6 dB (Urban), 0.126 = −18 dB (Fringe). LF (audio +
    /// pilot) stays full-strength → simulates a receiver whose L−R
    /// demod noise-floors up under weak signal: stereo collapses
    /// without the mono channel quieting.
    pub(super) fn process_with_noise(
        &mut self,
        composite: f32,
        noise_amp_lin: f32,
        subcarrier_gain: f32,
    ) -> f32 {
        // White → pink (single-pole tilt) → bandpass.
        let raw = self.rand();
        self.noise_lp = 0.99 * self.noise_lp + 0.01 * raw;
        let pinky = raw - self.noise_lp;
        let shaped_noise = self.noise_bp.process(pinky) * noise_amp_lin;

        // Fringe-only ±20% LFO on subcarrier gain.
        let multipath = if matches!(self.quality, FmReception::Fringe) {
            self.multipath_phase += self.multipath_inc;
            if self.multipath_phase > 2.0 * std::f32::consts::PI {
                self.multipath_phase -= 2.0 * std::f32::consts::PI;
            }
            1.0 + 0.2 * self.multipath_phase.sin()
        } else {
            1.0
        };
        let effective_sub_gain = subcarrier_gain * multipath;

        let lf0 = self.band_lp[0].process(composite);
        let lf = self.band_lp[1].process(lf0);
        let hf = composite - lf;

        lf + hf * effective_sub_gain + shaped_noise
    }

    pub(super) fn reset(&mut self) {
        self.noise_lp = 0.0;
        self.noise_bp.reset();
        for stage in &mut self.band_lp {
            stage.reset();
        }
        self.multipath_phase = 0.0;
    }
}
