//! Stereo MPX encoder + imperfect channel + decoder for the FM
//! broadcast simulation.
//!
//! Real FM stereo composite baseband:
//! - 0-15 kHz: L+R sum (mono-compatible)
//! - 19 kHz: pilot tone (~9% modulation, FCC standard)
//! - 23-53 kHz: L−R DSBSC-modulated on a 38 kHz cosine, generated
//!   coherently from the pilot
//! - (57 kHz RDS — not modelled, audibly inert under the 15 kHz LPF)
//!
//! Internal rate is 192 kHz — fits the 0-53 kHz composite without
//! aliasing the 38 kHz subcarrier and gives clean rubato ratios from
//! every common host rate. ~5-10 ms total resampler latency, folded
//! into the plugin-wide PDC budget.
//!
//! RF stages 11-13 (modulator / RF amp / demodulator) have no audio-rate
//! analog; their audible effects (weak-signal noise, stereo collapse,
//! multipath swirl) live in [`channel::FmChannel`].

mod channel;
mod decoder;
mod encoder;

use channel::FmChannel;
use decoder::MpxDecoder;
use encoder::MpxEncoder;

use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;

pub const MPX_RATE: u32 = 192_000;

// Per FCC + ITU-R BS.450: 9% pilot / 45% sum / 45% diff·subcarrier.
// Shared with the decoder so encode/decode math stays balanced.
pub(super) const MOD_SUM: f32 = 0.45;
pub(super) const MOD_DIFF: f32 = 0.45;
pub(super) const MOD_PILOT: f32 = 0.09;

/// 38 kHz L−R subcarrier is exactly twice this, generated coherently.
pub(super) const PILOT_HZ: f32 = 19_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FmReception {
    /// Encoder + channel + decoder roundtrip is a no-op (modulo numerical
    /// precision and the resampler's group delay).
    Pristine,
    /// City-grade reception: −6 dB stereo separation, mild HF noise.
    Urban,
    /// Fringe coverage: −18 dB separation, louder HF noise, slow multipath
    /// LFO on the L−R subcarrier.
    Fringe,
}

pub struct MpxPipeline {
    pub ready: bool,
    host_rate: u32,
    h2m_chunk: usize,
    m2h_chunk: usize,

    /// `None` when `host_rate == MPX_RATE` (no resample needed).
    h2m: Option<FftFixedIn<f32>>,
    m2h: Option<FftFixedIn<f32>>,

    h2m_in_buf: Vec<Vec<f32>>,
    h2m_out_buf: Vec<Vec<f32>>,
    m2h_in_buf: Vec<Vec<f32>>,
    m2h_out_buf: Vec<Vec<f32>>,

    pub host_input: Vec<VecDeque<f32>>,
    /// 192 kHz L/R after upsample.
    mpx_l: VecDeque<f32>,
    mpx_r: VecDeque<f32>,
    /// 192 kHz L/R after decode, before downsample.
    decoded_l: VecDeque<f32>,
    decoded_r: VecDeque<f32>,
    pub host_output: Vec<VecDeque<f32>>,

    encoder: MpxEncoder,
    channel: FmChannel,
    decoder: MpxDecoder,
}

impl MpxPipeline {
    pub fn new(reception: FmReception) -> Self {
        let mpx_rate_f = MPX_RATE as f32;
        Self {
            ready: false,
            host_rate: 44_100,
            h2m_chunk: 0,
            m2h_chunk: 0,
            h2m: None,
            m2h: None,
            h2m_in_buf: Vec::new(),
            h2m_out_buf: Vec::new(),
            m2h_in_buf: Vec::new(),
            m2h_out_buf: Vec::new(),
            host_input: Vec::new(),
            mpx_l: VecDeque::new(),
            mpx_r: VecDeque::new(),
            decoded_l: VecDeque::new(),
            decoded_r: VecDeque::new(),
            host_output: Vec::new(),
            encoder: MpxEncoder::new(mpx_rate_f),
            channel: FmChannel::new(reception, mpx_rate_f),
            decoder: MpxDecoder::new(mpx_rate_f),
        }
    }

    pub fn set_reception(&mut self, reception: FmReception) {
        self.channel = FmChannel::new(reception, MPX_RATE as f32);
    }

    /// Set up resamplers + ring buffers for the given host config.
    pub fn initialize(&mut self, host_rate: u32, channels: usize, max_block_size: usize) -> bool {
        self.ready = false;
        self.host_rate = host_rate;
        let _ = channels; // we always run stereo through this pipeline
        self.encoder.reset();
        self.decoder.reset();
        self.channel.reset();

        // No resampling needed at exactly MPX_RATE.
        if host_rate == MPX_RATE {
            self.h2m = None;
            self.m2h = None;
            self.h2m_chunk = 0;
            self.m2h_chunk = 0;
        } else {
            // ~10 ms chunk on each side. Use a host_rate / 100 size
            // for the upsampler input; rubato will figure out the
            // matching output chunk size.
            let h2m_chunk = (host_rate as usize / 100).max(1);
            let m2h_chunk = (MPX_RATE as usize / 100).max(1);
            let h2m = match FftFixedIn::<f32>::new(
                host_rate as usize,
                MPX_RATE as usize,
                h2m_chunk,
                2,
                2,
            ) {
                Ok(r) => r,
                Err(_) => return false,
            };
            let m2h = match FftFixedIn::<f32>::new(
                MPX_RATE as usize,
                host_rate as usize,
                m2h_chunk,
                2,
                2,
            ) {
                Ok(r) => r,
                Err(_) => return false,
            };
            let h2m_out_max = h2m.output_frames_max();
            let m2h_out_max = m2h.output_frames_max();
            self.h2m_chunk = h2m_chunk;
            self.m2h_chunk = m2h_chunk;
            self.h2m_in_buf = vec![vec![0.0; h2m_chunk]; 2];
            self.h2m_out_buf = vec![vec![0.0; h2m_out_max]; 2];
            self.m2h_in_buf = vec![vec![0.0; m2h_chunk]; 2];
            self.m2h_out_buf = vec![vec![0.0; m2h_out_max]; 2];
            self.h2m = Some(h2m);
            self.m2h = Some(m2h);
        }

        let ring_cap = max_block_size + (host_rate as usize / 10) + 1;
        let mpx_ring_cap = max_block_size * 5 + (MPX_RATE as usize / 10) + 1;
        self.host_input = vec![VecDeque::with_capacity(ring_cap); 2];
        self.mpx_l = VecDeque::with_capacity(mpx_ring_cap);
        self.mpx_r = VecDeque::with_capacity(mpx_ring_cap);
        self.decoded_l = VecDeque::with_capacity(mpx_ring_cap);
        self.decoded_r = VecDeque::with_capacity(mpx_ring_cap);
        self.host_output = vec![VecDeque::with_capacity(ring_cap); 2];

        self.ready = true;
        true
    }

    pub fn reset(&mut self) {
        if !self.ready {
            return;
        }
        if let Some(r) = &mut self.h2m {
            r.reset();
        }
        if let Some(r) = &mut self.m2h {
            r.reset();
        }
        self.encoder.reset();
        self.decoder.reset();
        self.channel.reset();
        for ring in &mut self.host_input {
            ring.clear();
        }
        self.mpx_l.clear();
        self.mpx_r.clear();
        self.decoded_l.clear();
        self.decoded_r.clear();
        for ring in &mut self.host_output {
            ring.clear();
        }
    }

    /// Push one block of host-rate stereo samples into the pipeline.
    pub fn push_block(&mut self, l: &[f32], r: &[f32]) {
        if !self.ready {
            return;
        }
        for &s in l {
            self.host_input[0].push_back(s);
        }
        for &s in r {
            self.host_input[1].push_back(s);
        }
    }

    /// Run as many resampler / codec chunks as the input rings allow.
    /// Idempotent — call repeatedly without re-pushing.
    pub fn pump(&mut self) {
        if !self.ready {
            return;
        }
        // Stage 1: host_input → mpx_l/r (resample or direct copy).
        match &mut self.h2m {
            None => {
                let n = self.host_input[0].len().min(self.host_input[1].len());
                if n > 0 {
                    self.mpx_l.reserve(n);
                    self.mpx_r.reserve(n);
                    let head_l = self.host_input[0].make_contiguous();
                    self.mpx_l.extend(head_l[..n].iter().copied());
                    self.host_input[0].drain(..n);
                    let head_r = self.host_input[1].make_contiguous();
                    self.mpx_r.extend(head_r[..n].iter().copied());
                    self.host_input[1].drain(..n);
                }
            }
            Some(r) => {
                let chunk = self.h2m_chunk;
                while self.host_input[0].len() >= chunk {
                    for ch in 0..2 {
                        let head = self.host_input[ch].make_contiguous();
                        self.h2m_in_buf[ch][..chunk].copy_from_slice(&head[..chunk]);
                        self.host_input[ch].drain(..chunk);
                    }
                    let produced = r
                        .process_into_buffer(&self.h2m_in_buf, &mut self.h2m_out_buf, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    self.mpx_l.reserve(produced);
                    self.mpx_l.extend(self.h2m_out_buf[0][..produced].iter().copied());
                    self.mpx_r.reserve(produced);
                    self.mpx_r.extend(self.h2m_out_buf[1][..produced].iter().copied());
                }
            }
        }

        // Stage 2: encode → channel → decode at MPX rate. Hoisting the
        // `match quality` out of the per-sample loop gives the autovec
        // a tight monomorphic body for each branch.
        let n = self.mpx_l.len().min(self.mpx_r.len());
        if n > 0 {
            self.decoded_l.reserve(n);
            self.decoded_r.reserve(n);
            let quality = self.channel.quality;
            // SAFETY: `l_take` / `r_take` alias `self.mpx_l` /
            // `self.mpx_r`, which we don't touch until after the loop.
            // The raw-pointer dance only exists to convince the borrow
            // checker that mpx_*, encoder, channel, decoder, and
            // decoded_* are all disjoint.
            let l_slice = self.mpx_l.make_contiguous();
            let l_take = &l_slice[..n] as *const [f32];
            let r_slice = self.mpx_r.make_contiguous();
            let r_take = &r_slice[..n] as *const [f32];
            let l_take = unsafe { &*l_take };
            let r_take = unsafe { &*r_take };

            match quality {
                FmReception::Pristine => {
                    // Pristine: channel is a pass-through.
                    for i in 0..n {
                        let composite = self.encoder.encode(l_take[i], r_take[i]);
                        let (dl, dr) = self.decoder.decode(composite);
                        self.decoded_l.push_back(dl);
                        self.decoded_r.push_back(dr);
                    }
                }
                FmReception::Urban => {
                    for i in 0..n {
                        let composite = self.encoder.encode(l_take[i], r_take[i]);
                        let degraded =
                            self.channel.process_with_noise(composite, 0.0032, 0.5);
                        let (dl, dr) = self.decoder.decode(degraded);
                        self.decoded_l.push_back(dl);
                        self.decoded_r.push_back(dr);
                    }
                }
                FmReception::Fringe => {
                    for i in 0..n {
                        let composite = self.encoder.encode(l_take[i], r_take[i]);
                        let degraded =
                            self.channel.process_with_noise(composite, 0.0316, 0.126);
                        let (dl, dr) = self.decoder.decode(degraded);
                        self.decoded_l.push_back(dl);
                        self.decoded_r.push_back(dr);
                    }
                }
            }
            self.mpx_l.drain(..n);
            self.mpx_r.drain(..n);
        }

        // Stage 3: decoded_l/r → host_output (resample or direct copy).
        match &mut self.m2h {
            None => {
                let n = self.decoded_l.len().min(self.decoded_r.len());
                if n > 0 {
                    self.host_output[0].reserve(n);
                    self.host_output[1].reserve(n);
                    let head_l = self.decoded_l.make_contiguous();
                    self.host_output[0].extend(head_l[..n].iter().copied());
                    self.decoded_l.drain(..n);
                    let head_r = self.decoded_r.make_contiguous();
                    self.host_output[1].extend(head_r[..n].iter().copied());
                    self.decoded_r.drain(..n);
                }
            }
            Some(r) => {
                let chunk = self.m2h_chunk;
                while self.decoded_l.len() >= chunk {
                    let head_l = self.decoded_l.make_contiguous();
                    self.m2h_in_buf[0][..chunk].copy_from_slice(&head_l[..chunk]);
                    self.decoded_l.drain(..chunk);
                    let head_r = self.decoded_r.make_contiguous();
                    self.m2h_in_buf[1][..chunk].copy_from_slice(&head_r[..chunk]);
                    self.decoded_r.drain(..chunk);
                    let produced = r
                        .process_into_buffer(&self.m2h_in_buf, &mut self.m2h_out_buf, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    self.host_output[0].reserve(produced);
                    self.host_output[0]
                        .extend(self.m2h_out_buf[0][..produced].iter().copied());
                    self.host_output[1].reserve(produced);
                    self.host_output[1]
                        .extend(self.m2h_out_buf[1][..produced].iter().copied());
                }
            }
        }
    }

    /// Drain `n` host-rate stereo samples into the provided slices.
    /// Falls back to silence if the pipeline hasn't accumulated
    /// enough output yet.
    pub fn drain_block(&mut self, l: &mut [f32], r: &mut [f32], n: usize) {
        if !self.ready {
            for s in 0..n {
                l[s] = 0.0;
                r[s] = 0.0;
            }
            return;
        }
        for s in 0..n {
            l[s] = self.host_output[0].pop_front().unwrap_or(0.0);
            r[s] = self.host_output[1].pop_front().unwrap_or(0.0);
        }
    }

    /// Combined resampler-pair latency in host-rate samples.
    pub fn latency_host_samples(&self) -> u32 {
        let h2m_delay = self
            .h2m
            .as_ref()
            .map(|r| r.output_delay() as u32)
            .unwrap_or(0);
        let m2h_delay = self
            .m2h
            .as_ref()
            .map(|r| r.output_delay() as u32)
            .unwrap_or(0);
        // h2m delay is at MPX rate, convert to host rate.
        let host = self.host_rate as u64;
        let mpx = MPX_RATE as u64;
        let h2m_at_host = (h2m_delay as u64 * host / mpx) as u32;
        // m2h delay is already at host rate. Plus chunk-fill latencies.
        let h2m_chunk_at_host = self.h2m_chunk as u32;
        let m2h_chunk_at_host = (self.m2h_chunk as u64 * host / mpx) as u32;
        h2m_chunk_at_host + h2m_at_host + m2h_chunk_at_host + m2h_delay
    }

    /// Static estimate via a throwaway pipeline. Used by the lazy-init
    /// path in `FmRadioProcessor`.
    pub fn estimate_latency(host_rate: u32) -> u32 {
        let mut probe = Self::new(FmReception::Pristine);
        if !probe.initialize(host_rate, 2, 1) {
            return 0;
        }
        probe.latency_host_samples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode → decode at MPX_RATE with no channel and no resampling
    /// must roundtrip stereo within ~2.5 dB (filter group delay only).
    #[test]
    fn encoder_decoder_roundtrip_pristine() {
        let mpx_rate = MPX_RATE as f32;
        let mut enc = MpxEncoder::new(mpx_rate);
        let mut dec = MpxDecoder::new(mpx_rate);

        let n = 32_768usize;
        let mut peak_in_l: f32 = 0.0;
        let mut peak_in_r: f32 = 0.0;
        let mut peak_out_l: f32 = 0.0;
        let mut peak_out_r: f32 = 0.0;
        let warmup = (mpx_rate * 0.05) as usize;
        for s in 0..n {
            let t = s as f32 / mpx_rate;
            let l_in = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.4;
            let r_in = (2.0 * std::f32::consts::PI * 2_000.0 * t).sin() * 0.4;
            let comp = enc.encode(l_in, r_in);
            let (l_out, r_out) = dec.decode(comp);
            if s >= warmup {
                peak_in_l = peak_in_l.max(l_in.abs());
                peak_in_r = peak_in_r.max(r_in.abs());
                peak_out_l = peak_out_l.max(l_out.abs());
                peak_out_r = peak_out_r.max(r_out.abs());
            }
        }
        let l_db = 20.0 * (peak_out_l / peak_in_l).log10();
        let r_db = 20.0 * (peak_out_r / peak_in_r).log10();
        assert!(
            l_db.abs() < 2.5 && r_db.abs() < 2.5,
            "MPX roundtrip lost more than 2.5 dB on either channel"
        );
    }

    /// Fringe reception must collapse stereo separation vs Pristine.
    #[test]
    fn fringe_collapses_stereo_separation() {
        let mpx_rate = MPX_RATE as f32;
        let mut enc = MpxEncoder::new(mpx_rate);
        let mut dec = MpxDecoder::new(mpx_rate);
        let mut chan_pristine = FmChannel::new(FmReception::Pristine, mpx_rate);
        let mut chan_fringe = FmChannel::new(FmReception::Fringe, mpx_rate);

        let n = 16_384usize;
        let mut pristine_r_peak: f32 = 0.0;
        let mut fringe_r_peak: f32 = 0.0;
        let mut dec2 = MpxDecoder::new(mpx_rate);
        let mut enc2 = MpxEncoder::new(mpx_rate);

        for s in 0..n {
            let t = s as f32 / mpx_rate;
            let l = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.5;
            let r = 0.0;
            let comp_p = enc.encode(l, r);
            let comp_p_chan = chan_pristine.process(comp_p);
            let (_lp, rp) = dec.decode(comp_p_chan);
            let comp_f = enc2.encode(l, r);
            let comp_f_chan = chan_fringe.process(comp_f);
            let (_lf, rf) = dec2.decode(comp_f_chan);
            if s >= (mpx_rate * 0.005) as usize {
                pristine_r_peak = pristine_r_peak.max(rp.abs());
                fringe_r_peak = fringe_r_peak.max(rf.abs());
            }
        }
        assert!(
            fringe_r_peak > pristine_r_peak * 2.0,
            "Fringe didn't collapse stereo separation: pristine {:.3} vs fringe {:.3}",
            pristine_r_peak,
            fringe_r_peak
        );
    }

    /// Full pipeline must emit audio at every common host rate.
    #[test]
    fn pipeline_emits_audio_at_every_host_rate() {
        for &host_rate in &[44_100u32, 48_000, 96_000, MPX_RATE] {
            for &reception in &[FmReception::Pristine, FmReception::Urban, FmReception::Fringe] {
                let mut pipe = MpxPipeline::new(reception);
                assert!(pipe.initialize(host_rate, 2, 256));

                let block_size = 256;
                let total = (host_rate as usize) * 2;
                let mut block_in_l = vec![0.0f32; block_size];
                let mut block_in_r = vec![0.0f32; block_size];
                let mut block_out_l = vec![0.0f32; block_size];
                let mut block_out_r = vec![0.0f32; block_size];
                let mut peak: f32 = 0.0;
                let mut cursor = 0usize;
                while cursor + block_size <= total {
                    for s in 0..block_size {
                        let t = (cursor + s) as f32 / host_rate as f32;
                        let v = (2.0 * std::f32::consts::PI * 1_000.0 * t).sin() * 0.3;
                        block_in_l[s] = v;
                        block_in_r[s] = v;
                    }
                    pipe.push_block(&block_in_l, &block_in_r);
                    pipe.pump();
                    pipe.drain_block(&mut block_out_l, &mut block_out_r, block_size);
                    if cursor >= host_rate as usize / 20 {
                        for s in 0..block_size {
                            peak = peak.max(block_out_l[s].abs()).max(block_out_r[s].abs());
                        }
                    }
                    cursor += block_size;
                }
                assert!(
                    peak > 0.05,
                    "MpxPipeline at {} Hz / {:?} produced near-silent output ({:.3})",
                    host_rate,
                    reception,
                    peak
                );
            }
        }
    }

    /// Every queued sample passes through encode → channel → decode.
    #[test]
    fn pump_drains_mpx_input_rings() {
        let mut pipe = MpxPipeline::new(FmReception::Pristine);
        // MPX rate → stage 1 takes the direct-copy path.
        assert!(pipe.initialize(MPX_RATE, 2, 1024));
        let block_in_l = vec![0.1f32; 1024];
        let block_in_r = vec![-0.1f32; 1024];
        pipe.push_block(&block_in_l, &block_in_r);
        pipe.pump();
        assert!(
            pipe.mpx_l.is_empty(),
            "mpx_l still has {} samples after pump",
            pipe.mpx_l.len()
        );
        assert!(
            pipe.mpx_r.is_empty(),
            "mpx_r still has {} samples after pump",
            pipe.mpx_r.len()
        );
    }

    /// Switching reception must wipe the previous channel's state so
    /// noise/multipath history doesn't leak.
    #[test]
    fn set_reception_resets_channel_state() {
        let mut pipe = MpxPipeline::new(FmReception::Fringe);
        assert!(pipe.initialize(48_000, 2, 256));
        let block_size = 256;
        let total = 48_000 * 2;
        let l = vec![0.5f32; block_size];
        let r = vec![-0.5f32; block_size];
        let mut cursor = 0usize;
        while cursor + block_size <= total {
            pipe.push_block(&l, &r);
            pipe.pump();
            let mut out_l = vec![0.0f32; block_size];
            let mut out_r = vec![0.0f32; block_size];
            pipe.drain_block(&mut out_l, &mut out_r, block_size);
            cursor += block_size;
        }
        pipe.set_reception(FmReception::Pristine);
        assert_eq!(pipe.channel.quality, FmReception::Pristine);
        assert_eq!(pipe.channel.noise_lp, 0.0);
        assert_eq!(pipe.channel.multipath_phase, 0.0);
    }
}
