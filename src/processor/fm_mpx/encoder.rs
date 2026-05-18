//! Stereo MPX encoder. See parent module for the composite layout.

use super::{MOD_DIFF, MOD_PILOT, MOD_SUM, PILOT_HZ};

pub(super) struct MpxEncoder {
    /// Pilot phase ∈ [0, 2π). The 38 kHz subcarrier = `cos(2·phase_19k)`,
    /// coherent by construction.
    phase_19k: f32,
    phase_inc: f32,
}

impl MpxEncoder {
    pub(super) fn new(mpx_rate: f32) -> Self {
        Self {
            phase_19k: 0.0,
            phase_inc: 2.0 * std::f32::consts::PI * PILOT_HZ / mpx_rate,
        }
    }

    /// `composite = MOD_SUM·(L+R) + MOD_DIFF·(L−R)·cos(2π·38k·t)
    ///            + MOD_PILOT·sin(2π·19k·t)`.
    #[inline]
    pub(super) fn encode(&mut self, l: f32, r: f32) -> f32 {
        let sum = l + r;
        let diff = l - r;
        let pilot = self.phase_19k.sin();
        let carrier_38k = (2.0 * self.phase_19k).cos();

        self.phase_19k += self.phase_inc;
        if self.phase_19k > 2.0 * std::f32::consts::PI {
            self.phase_19k -= 2.0 * std::f32::consts::PI;
        }

        MOD_SUM * sum + MOD_DIFF * diff * carrier_38k + MOD_PILOT * pilot
    }

    pub(super) fn reset(&mut self) {
        self.phase_19k = 0.0;
    }
}
