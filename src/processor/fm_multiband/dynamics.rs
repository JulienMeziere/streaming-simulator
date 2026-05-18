//! Per-band dynamics for `MultibandProcessor`.
//!
//! Both share a max-of-L-R detector with attack/release smoothing and
//! a stereo-linked gain output. They differ in the gain computer:
//! compressor uses the standard ratio formula above threshold; limiter
//! uses `ceiling / detect` (effectively infinite ratio).

/// Stereo-linked single-band feedback compressor.
#[derive(Clone, Copy, Debug)]
pub struct BandCompressor {
    threshold_lin: f32,
    /// `(1/ratio) - 1` — pre-computed so the hot path is one `powf`.
    ratio_inv_minus_one: f32,
    attack_coef: f32,
    release_coef: f32,
    env_gain: f32,
}

impl BandCompressor {
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        sample_rate: f32,
    ) -> Self {
        Self {
            threshold_lin: 10f32.powf(threshold_db / 20.0),
            ratio_inv_minus_one: (1.0 / ratio) - 1.0,
            attack_coef: (-1.0 / (attack_ms * 1e-3 * sample_rate)).exp(),
            release_coef: (-1.0 / (release_ms * 1e-3 * sample_rate)).exp(),
            env_gain: 1.0,
        }
    }

    /// Detector + envelope for one stereo sample. Returns the linear
    /// gain reduction (1.0 = no GR, < 1.0 = duck).
    #[inline]
    pub fn process_detection(&mut self, l: f32, r: f32) -> f32 {
        let detect = l.abs().max(r.abs());
        let target_gain = if detect > self.threshold_lin {
            (detect / self.threshold_lin).powf(self.ratio_inv_minus_one)
        } else {
            1.0
        };
        let coef = if target_gain < self.env_gain {
            self.attack_coef
        } else {
            self.release_coef
        };
        self.env_gain = target_gain + (self.env_gain - target_gain) * coef;
        self.env_gain
    }

    pub fn current_gain(&self) -> f32 {
        self.env_gain
    }

    pub fn reset(&mut self) {
        self.env_gain = 1.0;
    }
}

/// Hard peak limiter — catches whatever the band compressor missed.
#[derive(Clone, Copy, Debug)]
pub struct BandLimiter {
    ceiling_lin: f32,
    attack_coef: f32,
    release_coef: f32,
    env_gain: f32,
}

impl BandLimiter {
    pub fn new(ceiling_db: f32, attack_ms: f32, release_ms: f32, sample_rate: f32) -> Self {
        Self {
            ceiling_lin: 10f32.powf(ceiling_db / 20.0),
            attack_coef: (-1.0 / (attack_ms * 1e-3 * sample_rate)).exp(),
            release_coef: (-1.0 / (release_ms * 1e-3 * sample_rate)).exp(),
            env_gain: 1.0,
        }
    }

    #[inline]
    pub fn process_detection(&mut self, l: f32, r: f32) -> f32 {
        let detect = l.abs().max(r.abs());
        let target_gain = if detect > self.ceiling_lin {
            self.ceiling_lin / detect
        } else {
            1.0
        };
        let coef = if target_gain < self.env_gain {
            self.attack_coef
        } else {
            self.release_coef
        };
        self.env_gain = target_gain + (self.env_gain - target_gain) * coef;
        self.env_gain
    }

    pub fn reset(&mut self) {
        self.env_gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reduce gain on hot input, pass quiet input untouched.
    #[test]
    fn band_compressor_reduces_gain_on_hot_input() {
        let fs = 48_000.0;
        let mut comp = BandCompressor::new(-12.0, 4.0, 5.0, 100.0, fs);

        // Settle on a hot signal (~+6 dB above threshold).
        let n_settle = (fs * 0.5) as usize;
        for _ in 0..n_settle {
            comp.process_detection(0.5, 0.5);
        }
        assert!(
            comp.current_gain() < 0.85,
            "compressor failed to reduce gain on hot input: env_gain = {:.3}",
            comp.current_gain()
        );

        // Long quiet tail — envelope must recover.
        let n_recover = (fs * 1.0) as usize;
        for _ in 0..n_recover {
            comp.process_detection(0.01, 0.01);
        }
        assert!(
            comp.current_gain() > 0.95,
            "compressor failed to recover on quiet input: env_gain = {:.3}",
            comp.current_gain()
        );
    }
}
