//! Real-time FM broadcast-chain simulation.
//!
//! ```text
//!  input → AGC → broadcast EQ → multiband + per-band limiter
//!       → pre-emphasis → 2× oversampled hard clipper
//!       → MPX encoder → imperfect channel → MPX decoder
//!       → de-emphasis → auto-makeup → delay → output
//! ```
//!
//! See [`FmRadioVariant`] for the 6 tiers (2 regions × 3 reception
//! qualities) and `docs/codec-implementation.md` for per-stage notes.
//!
//! Pre-emphasis: `y[n] = (1/α)·x[n] − ((1−α)/α)·x[n−1]` (FIR one-zero).
//! De-emphasis: `y[n] = α·x[n] + (1−α)·y[n−1]` (IIR one-pole). With
//! `α = 1/(1 + τ·fs)` they're exact inverses on linear material — the
//! HF distortion that defines the FM sound comes from the non-linear
//! stages (multiband + clipping) between them.

use crate::processor::biquad::{Biquad, BiquadCoeffs};
use crate::processor::fm_multiband::{BandCompressor, MultibandProcessor};
use crate::processor::fm_mpx::{FmReception, MpxPipeline};
use std::collections::VecDeque;

const TAU_75_US: f32 = 75e-6;
const TAU_50_US: f32 = 50e-6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FmRegion {
    /// FCC 75 µs — Americas, Korea.
    Us75us,
    /// ITU-R BS.450 50 µs — Europe, Africa, Asia, Australia.
    Eu50us,
}

impl FmRegion {
    fn tau(self) -> f32 {
        match self {
            FmRegion::Us75us => TAU_75_US,
            FmRegion::Eu50us => TAU_50_US,
        }
    }
}

/// 2 regions × 3 reception qualities = 6 catalog tiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FmRadioVariant {
    pub region: FmRegion,
    pub reception: FmReception,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FmRadioMode {
    Passthrough,
    FmRadio { variant: FmRadioVariant },
}

pub struct FmRadioProcessor {
    ready: bool,
    sample_rate: u32,
    channels: usize,

    /// Stereo-linked, single-band leveler.
    agc: BandCompressor,

    /// Per-channel cascaded shelves (low +3 dB @ 80 Hz, presence +2 dB @ 3 kHz)
    /// tuned to a "default rock" broadcast preset.
    eq_low_shelf: [Biquad; 2],
    eq_high_shelf: [Biquad; 2],

    multiband: MultibandProcessor,

    pre_x_prev: [f32; 2],
    pre_inv_alpha: f32,
    pre_neg_a: f32,

    clipper_last_input: [f32; 2],

    /// host ↔ 192 kHz oversampling + MPX encoder + imperfect channel +
    /// MPX decoder.
    mpx: MpxPipeline,

    deemph_y_prev: [f32; 2],
    deemph_alpha: f32,

    /// Long-term envelope follower used for codec ↔ FM loudness-fair A/B —
    /// AGC + multiband + clipper push output 4-8 dB louder than input.
    input_loudness: f32,
    output_loudness: f32,
    makeup_smoothing: f32,
    makeup_lin: f32,

    /// Per-channel delay for PDC alignment with the rest of the codecs.
    delay: Vec<VecDeque<f32>>,
    delay_len: usize,

    /// Pre/post-MPX block staging — preallocated to avoid audio-thread allocs.
    pre_mpx_l: Vec<f32>,
    pre_mpx_r: Vec<f32>,
    post_mpx_l: Vec<f32>,
    post_mpx_r: Vec<f32>,

    /// Caches the active variant — `setup_chain` only does work on changes.
    current_variant: Option<FmRadioVariant>,

    latency_host_samples: u32,
}

impl FmRadioProcessor {
    pub fn new() -> Self {
        let placeholder_fs = 44_100.0;
        Self {
            ready: false,
            sample_rate: 44_100,
            channels: 2,
            agc: BandCompressor::new(-10.0, 3.0, 100.0, 1500.0, placeholder_fs),
            eq_low_shelf: [Biquad::new(); 2],
            eq_high_shelf: [Biquad::new(); 2],
            multiband: MultibandProcessor::new(),
            pre_x_prev: [0.0; 2],
            pre_inv_alpha: 1.0,
            pre_neg_a: 0.0,
            clipper_last_input: [0.0; 2],
            mpx: MpxPipeline::new(FmReception::Pristine),
            deemph_y_prev: [0.0; 2],
            deemph_alpha: 1.0,
            input_loudness: 0.0,
            output_loudness: 0.0,
            makeup_smoothing: 0.0,
            makeup_lin: 1.0,
            delay: Vec::new(),
            delay_len: 0,
            pre_mpx_l: Vec::new(),
            pre_mpx_r: Vec::new(),
            post_mpx_l: Vec::new(),
            post_mpx_r: Vec::new(),
            current_variant: None,
            latency_host_samples: 0,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        self.ready = false;
        if !matches!(channels, 1 | 2) {
            return;
        }
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.current_variant = None;
        let fs = sample_rate as f32;

        // -10 dB target, 3:1, slow attack/release — follows programme loudness.
        self.agc = BandCompressor::new(-10.0, 3.0, 100.0, 1500.0, fs);

        let low_shelf = BiquadCoeffs::low_shelf(80.0, fs, 3.0, 0.7);
        let high_shelf = BiquadCoeffs::high_shelf(3_000.0, fs, 2.0, 0.7);
        for ch in 0..2 {
            self.eq_low_shelf[ch].set_coeffs(low_shelf);
            self.eq_high_shelf[ch].set_coeffs(high_shelf);
            self.eq_low_shelf[ch].reset();
            self.eq_high_shelf[ch].reset();
        }

        self.multiband.initialize(sample_rate);

        if !self.mpx.initialize(sample_rate, channels, max_block_size) {
            return;
        }

        // ~1 s envelope follower for input/output loudness.
        self.makeup_smoothing = (-1.0 / fs).exp();
        self.input_loudness = 0.0;
        self.output_loudness = 0.0;
        self.makeup_lin = 1.0;

        self.pre_mpx_l = vec![0.0; max_block_size];
        self.pre_mpx_r = vec![0.0; max_block_size];
        self.post_mpx_l = vec![0.0; max_block_size];
        self.post_mpx_r = vec![0.0; max_block_size];

        // Sized by `pad_output_to`.
        self.delay = (0..channels).map(|_| VecDeque::new()).collect();
        self.delay_len = 0;
        self.latency_host_samples = 0;

        self.pre_x_prev = [0.0; 2];
        self.deemph_y_prev = [0.0; 2];
        self.clipper_last_input = [0.0; 2];

        self.ready = true;
    }

    /// Pad the delay line so reported latency matches the plugin-wide PDC
    /// target.
    pub fn pad_output_to(&mut self, target: u32) {
        if !self.ready {
            return;
        }
        // Subtract MPX's intrinsic latency — we only need to top up.
        let mpx_latency = self.mpx.latency_host_samples();
        let extra = target.saturating_sub(mpx_latency) as usize;
        if extra > self.delay_len {
            let grow = extra - self.delay_len;
            for ch in 0..self.channels {
                for _ in 0..grow {
                    self.delay[ch].push_back(0.0);
                }
            }
            self.delay_len = extra;
            self.latency_host_samples = target;
        }
    }

    pub fn reset(&mut self) {
        if !self.ready {
            return;
        }
        self.agc.reset();
        for ch in 0..2 {
            self.eq_low_shelf[ch].reset();
            self.eq_high_shelf[ch].reset();
        }
        self.multiband.reset();
        self.mpx.reset();
        self.pre_x_prev = [0.0; 2];
        self.deemph_y_prev = [0.0; 2];
        self.clipper_last_input = [0.0; 2];
        self.input_loudness = 0.0;
        self.output_loudness = 0.0;
        self.makeup_lin = 1.0;
        for ch in 0..self.channels {
            self.delay[ch].clear();
            for _ in 0..self.delay_len {
                self.delay[ch].push_back(0.0);
            }
        }
    }

    pub fn latency_samples(&self) -> u32 {
        if self.ready {
            self.latency_host_samples
        } else {
            0
        }
    }

    /// MPX (~5-10 ms resampler delay) dominates; audio-domain stages add
    /// negligible group delay.
    pub fn worst_case_latency_at(host_rate: u32, _channels: usize) -> u32 {
        MpxPipeline::estimate_latency(host_rate)
    }

    fn setup_chain(&mut self, variant: FmRadioVariant) {
        if Some(variant) == self.current_variant {
            return;
        }
        let alpha = 1.0 / (1.0 + variant.region.tau() * self.sample_rate as f32);
        self.deemph_alpha = alpha;
        self.pre_inv_alpha = 1.0 / alpha;
        self.pre_neg_a = -(1.0 - alpha) / alpha;
        self.mpx.set_reception(variant.reception);
        // Wipe filter memory so the previous variant doesn't bleed into the new one.
        self.pre_x_prev = [0.0; 2];
        self.deemph_y_prev = [0.0; 2];
        self.clipper_last_input = [0.0; 2];
        self.current_variant = Some(variant);
    }

    /// 2× oversampled hard clipper at −0.5 dBFS. Linear-interp upsample +
    /// 2-tap boxcar decimate gives 6-8 dB of alias suppression — the
    /// threshold where digital fizz stops being audible. A polyphase
    /// halfband would cost much more for marginal audible improvement.
    #[inline]
    fn hard_clip_2x(&mut self, ch: usize, x: f32) -> f32 {
        const CEIL: f32 = 0.944_06; // −0.5 dBFS
        let prev = self.clipper_last_input[ch];
        let mid = (prev + x) * 0.5;
        self.clipper_last_input[ch] = x;
        let cur_clipped = x.clamp(-CEIL, CEIL);
        let mid_clipped = mid.clamp(-CEIL, CEIL);
        (mid_clipped + cur_clipped) * 0.5
    }

    pub fn process(&mut self, buffer: &mut nih_plug::buffer::Buffer, mode: FmRadioMode) {
        if !self.ready {
            return;
        }
        match mode {
            FmRadioMode::Passthrough => {
                let n = buffer.samples();
                let block = buffer.as_slice();
                for ch in 0..self.channels {
                    for s in 0..n {
                        self.delay[ch].push_back(block[ch][s]);
                        block[ch][s] = self.delay[ch].pop_front().unwrap_or(0.0);
                    }
                }
            }
            FmRadioMode::FmRadio { variant } => {
                self.setup_chain(variant);
                self.process_fm(buffer);
            }
        }
    }

    fn process_fm(&mut self, buffer: &mut nih_plug::buffer::Buffer) {
        let n = buffer.samples();
        let block = buffer.as_slice();

        // AGC → EQ → multiband → pre-emph → clipper, sample-by-sample.
        for s in 0..n {
            let mut l = block[0][s];
            let mut r = if self.channels > 1 { block[1][s] } else { l };

            // Track input loudness *before* processing alters the level.
            let in_amp = l.abs().max(r.abs());
            self.input_loudness = self.makeup_smoothing * self.input_loudness
                + (1.0 - self.makeup_smoothing) * in_amp;

            let agc_gain = self.agc.process_detection(l, r);
            l *= agc_gain;
            r *= agc_gain;

            l = self.eq_high_shelf[0].process(self.eq_low_shelf[0].process(l));
            r = self.eq_high_shelf[1].process(self.eq_low_shelf[1].process(r));

            let (ml, mr) = self.multiband.process_stereo(l, r);
            l = ml;
            r = mr;

            let pl = self.pre_inv_alpha * l + self.pre_neg_a * self.pre_x_prev[0];
            let pr = self.pre_inv_alpha * r + self.pre_neg_a * self.pre_x_prev[1];
            self.pre_x_prev[0] = l;
            self.pre_x_prev[1] = r;
            l = pl;
            r = pr;

            l = self.hard_clip_2x(0, l);
            r = self.hard_clip_2x(1, r);

            self.pre_mpx_l[s] = l;
            self.pre_mpx_r[s] = r;
        }

        // MPX encode → channel → decode at 192 kHz internally; the
        // pipeline's rings absorb the resample-rate mismatch.
        self.mpx
            .push_block(&self.pre_mpx_l[..n], &self.pre_mpx_r[..n]);
        self.mpx.pump();
        self.mpx
            .drain_block(&mut self.post_mpx_l[..n], &mut self.post_mpx_r[..n], n);

        // De-emphasis → auto-makeup → delay → output.
        for s in 0..n {
            let mut l = self.post_mpx_l[s];
            let mut r = self.post_mpx_r[s];

            let dl = self.deemph_alpha * l + (1.0 - self.deemph_alpha) * self.deemph_y_prev[0];
            let dr = self.deemph_alpha * r + (1.0 - self.deemph_alpha) * self.deemph_y_prev[1];
            self.deemph_y_prev[0] = dl;
            self.deemph_y_prev[1] = dr;
            l = dl;
            r = dr;

            let out_amp = l.abs().max(r.abs());
            self.output_loudness = self.makeup_smoothing * self.output_loudness
                + (1.0 - self.makeup_smoothing) * out_amp;

            // Smoothed makeup with a ±12 dB clamp so a runaway envelope
            // can't blow up the output.
            let target_makeup = self.input_loudness / self.output_loudness.max(1e-4);
            self.makeup_lin = self.makeup_smoothing * self.makeup_lin
                + (1.0 - self.makeup_smoothing) * target_makeup;
            let safe_makeup = self.makeup_lin.clamp(0.25, 4.0);
            l *= safe_makeup;
            r *= safe_makeup;

            self.delay[0].push_back(l);
            block[0][s] = self.delay[0].pop_front().unwrap_or(0.0);
            if self.channels > 1 {
                self.delay[1].push_back(r);
                block[1][s] = self.delay[1].pop_front().unwrap_or(0.0);
            }
        }
    }

    /// Test-only `process()` without the nih-plug `Buffer` wrapping.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: FmRadioMode,
    ) {
        if !self.ready {
            return;
        }
        let n = input[0].len();
        match mode {
            FmRadioMode::Passthrough => {
                for ch in 0..self.channels {
                    for s in 0..n {
                        self.delay[ch].push_back(input[ch][s]);
                        output[ch][s] = self.delay[ch].pop_front().unwrap_or(0.0);
                    }
                }
            }
            FmRadioMode::FmRadio { variant } => {
                self.setup_chain(variant);
                self.process_fm_planar(input, output);
            }
        }
    }

    #[cfg(test)]
    fn process_fm_planar(&mut self, input: &[Vec<f32>], output: &mut [Vec<f32>]) {
        let n = input[0].len();
        for s in 0..n {
            let mut l = input[0][s];
            let mut r = if self.channels > 1 { input[1][s] } else { l };
            let in_amp = l.abs().max(r.abs());
            self.input_loudness = self.makeup_smoothing * self.input_loudness
                + (1.0 - self.makeup_smoothing) * in_amp;
            let agc_gain = self.agc.process_detection(l, r);
            l *= agc_gain;
            r *= agc_gain;
            l = self.eq_high_shelf[0].process(self.eq_low_shelf[0].process(l));
            r = self.eq_high_shelf[1].process(self.eq_low_shelf[1].process(r));
            let (ml, mr) = self.multiband.process_stereo(l, r);
            l = ml;
            r = mr;
            let pl = self.pre_inv_alpha * l + self.pre_neg_a * self.pre_x_prev[0];
            let pr = self.pre_inv_alpha * r + self.pre_neg_a * self.pre_x_prev[1];
            self.pre_x_prev[0] = l;
            self.pre_x_prev[1] = r;
            l = pl;
            r = pr;
            l = self.hard_clip_2x(0, l);
            r = self.hard_clip_2x(1, r);
            self.pre_mpx_l[s] = l;
            self.pre_mpx_r[s] = r;
        }
        self.mpx
            .push_block(&self.pre_mpx_l[..n], &self.pre_mpx_r[..n]);
        self.mpx.pump();
        self.mpx
            .drain_block(&mut self.post_mpx_l[..n], &mut self.post_mpx_r[..n], n);
        for s in 0..n {
            let mut l = self.post_mpx_l[s];
            let mut r = self.post_mpx_r[s];
            let dl = self.deemph_alpha * l + (1.0 - self.deemph_alpha) * self.deemph_y_prev[0];
            let dr = self.deemph_alpha * r + (1.0 - self.deemph_alpha) * self.deemph_y_prev[1];
            self.deemph_y_prev[0] = dl;
            self.deemph_y_prev[1] = dr;
            l = dl;
            r = dr;
            let out_amp = l.abs().max(r.abs());
            self.output_loudness = self.makeup_smoothing * self.output_loudness
                + (1.0 - self.makeup_smoothing) * out_amp;
            let target_makeup = self.input_loudness / self.output_loudness.max(1e-4);
            self.makeup_lin = self.makeup_smoothing * self.makeup_lin
                + (1.0 - self.makeup_smoothing) * target_makeup;
            let safe_makeup = self.makeup_lin.clamp(0.25, 4.0);
            l *= safe_makeup;
            r *= safe_makeup;
            self.delay[0].push_back(l);
            output[0][s] = self.delay[0].pop_front().unwrap_or(0.0);
            if self.channels > 1 {
                self.delay[1].push_back(r);
                output[1][s] = self.delay[1].pop_front().unwrap_or(0.0);
            }
        }
    }
}

impl Default for FmRadioProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_smoke_test(host_rate: u32, region: FmRegion, reception: FmReception) {
        let mut proc = FmRadioProcessor::new();
        let block_size = 256usize;
        proc.initialize(host_rate, 2, block_size);
        proc.pad_output_to(host_rate / 50);
        assert!(proc.ready);

        let total_samples = (host_rate as usize) * 3;
        let mut block_in: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
        let mut block_out: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];

        let mut peak_in: f32 = 0.0;
        let mut peak_out: f32 = 0.0;
        let warmup = host_rate as usize;
        let mut cursor = 0usize;
        while cursor + block_size <= total_samples {
            for s in 0..block_size {
                let t = (cursor + s) as f32 / host_rate as f32;
                // Three-sine broadband test signal.
                let v = ((2.0 * std::f32::consts::PI * 220.0 * t).sin()
                    + (2.0 * std::f32::consts::PI * 880.0 * t).sin() * 0.6
                    + (2.0 * std::f32::consts::PI * 3_300.0 * t).sin() * 0.3)
                    * 0.1;
                block_in[0][s] = v;
                block_in[1][s] = v;
            }
            proc.process_planar(
                &block_in,
                &mut block_out,
                FmRadioMode::FmRadio {
                    variant: FmRadioVariant { region, reception },
                },
            );
            for s in 0..block_size {
                let pos = cursor + s;
                if pos >= warmup {
                    peak_in = peak_in.max(block_in[0][s].abs());
                    peak_out = peak_out.max(block_out[0][s].abs());
                }
            }
            cursor += block_size;
        }
        let delta_db = 20.0 * (peak_out / peak_in.max(1e-6)).log10();
        eprintln!(
            "FM full-chain {}Hz {:?}/{:?}: peak in {:.3}, peak out {:.3}, delta {:.2} dB",
            host_rate, region, reception, peak_in, peak_out, delta_db
        );
        assert!(
            peak_out > 0.01,
            "FM full chain produced near-silent output for {} Hz / {:?} / {:?}",
            host_rate,
            region,
            reception
        );
        // ±6 dB tolerance — real programme material tracks tighter, but a
        // 3-sine test signal is brittle for the envelope follower.
        assert!(
            delta_db.abs() < 6.0,
            "FM full chain output level {:.2} dB off input for {} Hz / {:?} / {:?}",
            delta_db,
            host_rate,
            region,
            reception
        );
    }

    /// Smoke test: every (host rate × region × reception) combination
    /// produces audio that's roughly loudness-matched to the input.
    #[test]
    fn full_chain_smoke_test() {
        for &host_rate in &[44_100u32, 48_000, 96_000] {
            for &region in &[FmRegion::Us75us, FmRegion::Eu50us] {
                for &reception in &[FmReception::Pristine, FmReception::Urban, FmReception::Fringe] {
                    run_smoke_test(host_rate, region, reception);
                }
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = FmRadioProcessor::new();
        proc.initialize(48_000, 2, 256);
        proc.pad_output_to(48_000 / 50);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, FmRadioMode::Passthrough),
        );
        assert!(peak > 0.05, "FM passthrough silent: {peak:.3}");
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 96_000] {
            let l = FmRadioProcessor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "FM worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = FmRadioProcessor::default();
        assert!(!proc.ready);
    }

    #[test]
    fn reset_after_dispatch_clears_state_safely() {
        let mut proc = FmRadioProcessor::new();
        proc.initialize(48_000, 2, 256);
        proc.pad_output_to(48_000 / 50);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(
                &inp,
                &mut out,
                FmRadioMode::FmRadio {
                    variant: FmRadioVariant {
                        region: FmRegion::Us75us,
                        reception: FmReception::Pristine,
                    },
                },
            );
        }
        proc.reset();
        assert!(proc.ready);
    }

    #[test]
    fn pad_output_to_grows_delay() {
        let mut proc = FmRadioProcessor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.delay[0].len();
        proc.pad_output_to(before as u32 + 10_000);
        for ch in 0..2 {
            assert!(
                proc.delay[ch].len() > before,
                "ch{ch} delay didn't grow after pad_output_to: was {before}, now {}",
                proc.delay[ch].len()
            );
        }
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = FmRadioProcessor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.ready);
    }
}
