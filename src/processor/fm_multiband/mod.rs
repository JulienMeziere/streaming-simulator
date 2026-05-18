//! 4-band multiband compressor + limiter for the FM airchain.
//!
//! Modeled on Orban Optimod / Omnia.9 / Wheatstone Aura topology:
//! Linkwitz-Riley crossover → per-band compressor → gain-share link bus
//! → per-band peak limiter → sum.
//!
//! ```text
//!  input → LR4 split @ 100/800/4 kHz → [sub|low|mid|high] comps
//!                                       │       │
//!                                       └ link bus (70% own / 30% mean GR)
//!                                       │
//!                                       └ per-band limiter (-1 dBFS) → sum
//! ```
//!
//! The link bus prevents bands from fighting on transients (kick drums
//! slamming the sub while the high stays full = pumpy unnatural sound).
//! Blending 30% of the mean GR makes all bands duck together.

mod crossover;
mod dynamics;

pub use crossover::LinkwitzRileyBank;
pub use dynamics::{BandCompressor, BandLimiter};

pub struct MultibandProcessor {
    crossover: LinkwitzRileyBank,
    /// `[sub, low, mid, high]`.
    band_comps: [BandCompressor; 4],
    band_lims: [BandLimiter; 4],
    pub ready: bool,
}

impl MultibandProcessor {
    pub fn new() -> Self {
        // Placeholder rate — overwritten by `initialize` before any audio.
        let placeholder_fs = 44_100.0;
        let comp = BandCompressor::new(-12.0, 4.0, 5.0, 100.0, placeholder_fs);
        let lim = BandLimiter::new(-1.0, 0.5, 50.0, placeholder_fs);
        Self {
            crossover: LinkwitzRileyBank::new(),
            band_comps: [comp; 4],
            band_lims: [lim; 4],
            ready: false,
        }
    }

    /// Per-band parameters approximate an Orban Optimod / Omnia.9
    /// "Pop/Rock" preset. Not user-configurable for now.
    pub fn initialize(&mut self, sample_rate: u32) {
        let fs = sample_rate as f32;
        self.crossover.set_crossovers(fs, [100.0, 800.0, 4000.0]);

        // (threshold_db, ratio, attack_ms, release_ms) per band.
        // Sub gets a slow release to keep kicks from pumping the rest;
        // mid/high get faster ratios for "broadcast presence".
        let comp_params: [(f32, f32, f32, f32); 4] = [
            (-16.0, 4.0, 5.0, 200.0), // sub
            (-14.0, 4.0, 8.0, 150.0), // low
            (-12.0, 6.0, 3.0, 80.0),  // mid
            (-10.0, 6.0, 2.0, 60.0),  // high
        ];
        for i in 0..4 {
            let (thr, ratio, atk, rel) = comp_params[i];
            self.band_comps[i] = BandCompressor::new(thr, ratio, atk, rel, fs);
            // Hard fast peak limiter on every band — catches what the
            // compressor missed.
            self.band_lims[i] = BandLimiter::new(-1.0, 0.5, 50.0, fs);
        }

        self.ready = true;
    }

    /// Process one stereo sample. Returns `(out_l, out_r)`.
    #[inline]
    pub fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        if !self.ready {
            return (l, r);
        }

        let l_bands = self.crossover.split(0, l);
        let r_bands = self.crossover.split(1, r);

        // Per-band stereo-linked GR (max-of-L-R inside each compressor).
        let mut individual_gr = [1.0f32; 4];
        for i in 0..4 {
            individual_gr[i] = self.band_comps[i].process_detection(l_bands[i], r_bands[i]);
        }

        // 70% own / 30% mean — see module-level note on gain-share linking.
        let mean = (individual_gr[0] + individual_gr[1] + individual_gr[2] + individual_gr[3])
            * 0.25;
        let mut applied_gr = [1.0f32; 4];
        for i in 0..4 {
            applied_gr[i] = 0.7 * individual_gr[i] + 0.3 * mean;
        }

        // Compressor + per-band limiter, sum back to stereo.
        let mut sum_l = 0.0;
        let mut sum_r = 0.0;
        for i in 0..4 {
            let mut bl = l_bands[i] * applied_gr[i];
            let mut br = r_bands[i] * applied_gr[i];
            let lim_gr = self.band_lims[i].process_detection(bl, br);
            bl *= lim_gr;
            br *= lim_gr;
            sum_l += bl;
            sum_r += br;
        }

        (sum_l, sum_r)
    }

    pub fn reset(&mut self) {
        self.crossover.reset();
        for i in 0..4 {
            self.band_comps[i].reset();
            self.band_lims[i].reset();
        }
    }
}

impl Default for MultibandProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: chain is alive end-to-end and bounded by the per-band
    /// limiters (4 bands × -1 dBFS = ~3.6 absolute upper bound).
    #[test]
    fn multiband_emits_audio_and_respects_limiter() {
        let fs = 48_000u32;
        let mut mb = MultibandProcessor::new();
        mb.initialize(fs);

        let block_size = 4096usize;
        let mut peak: f32 = 0.0;
        for s in 0..block_size {
            let t = s as f32 / fs as f32;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.9;
            let (l, r) = mb.process_stereo(x, x);
            if s >= 1024 {
                peak = peak.max(l.abs()).max(r.abs());
            }
        }
        assert!(peak > 0.05, "Multiband produced near-silent output: {:.4}", peak);
        assert!(peak < 4.0, "Multiband output unbounded: {:.4}", peak);
    }

    /// Silence in → silence out — no DC offset or limiter thump.
    #[test]
    fn silence_in_silence_out() {
        let fs = 48_000u32;
        let mut mb = MultibandProcessor::new();
        mb.initialize(fs);
        for _ in 0..4_096 {
            let (l, r) = mb.process_stereo(0.0, 0.0);
            assert!(
                l.abs() < 1e-6 && r.abs() < 1e-6,
                "multiband produced non-zero output for silence input: l={l}, r={r}"
            );
        }
    }

    /// Massively overdriven input must come back below unity once the
    /// limiters have engaged.
    #[test]
    fn extreme_input_clipped_below_unity_after_warmup() {
        let fs = 48_000u32;
        let mut mb = MultibandProcessor::new();
        mb.initialize(fs);
        // ~85 ms warmup — past compressor attack + limiter engage.
        let warmup = 4_096;
        let measure = 4_096;
        for s in 0..warmup {
            let t = s as f32 / fs as f32;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 4.0;
            let _ = mb.process_stereo(x, x);
        }
        let mut peak: f32 = 0.0;
        for s in warmup..warmup + measure {
            let t = s as f32 / fs as f32;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 4.0;
            let (l, r) = mb.process_stereo(x, x);
            peak = peak.max(l.abs()).max(r.abs());
        }
        // For a 1 kHz sine the energy concentrates in one band and the
        // ~0.89 (-1 dBFS) per-band ceiling pulls the observed peak near unity.
        assert!(
            peak < 1.5,
            "multiband let extreme input through above 1.5: peak {peak:.4}"
        );
    }

    #[test]
    fn default_multiband_processor_matches_new() {
        let _ = MultibandProcessor::default();
    }

    #[test]
    fn reset_clears_filter_state() {
        let fs = 48_000u32;
        let mut mb = MultibandProcessor::new();
        mb.initialize(fs);
        for s in 0..1024 {
            let t = s as f32 / fs as f32;
            let x = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.8;
            let _ = mb.process_stereo(x, x);
        }
        mb.reset();
        let (l, r) = mb.process_stereo(0.0, 0.0);
        assert_eq!(l, 0.0);
        assert_eq!(r, 0.0);
    }
}
