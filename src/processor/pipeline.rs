//! Shared scaffolding for every codec processor: ring buffers, rubato
//! resamplers, host I/O glue, latency padding.
//!
//! ```text
//!  host_input (host_rate)                host_output (host_rate, pre-filled)
//!      │                                              ▲
//!      │ pump_host_to_internal                        │ pump_internal_to_host
//!      ▼                                              │
//!  internal_input (encode_rate)   internal_output (decode_rate)
//!      │                                              ▲
//!      └──── codec's encode → decode (lives in codec module) ────┘
//! ```
//!
//! Each codec module owns one [`ResampledPipeline`], fills `host_input`,
//! calls `pump_host_to_internal`, runs its own encode/decode against
//! `internal_input` / `internal_output`, calls `pump_internal_to_host`,
//! then drains `host_output` to the DAW. `encode_rate` is fixed per
//! codec; `decode_rate` may differ (MP3 auto-downsample) and is updated
//! via [`setup_i2h`].

use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;

/// rubato's default; quality/latency sweet spot for music.
const RESAMPLER_SUB_CHUNKS: usize = 2;

pub struct ResampledPipeline {
    /// `setup` succeeded. Every pump / push / drain is a no-op while false.
    pub ready: bool,
    pub host_rate: u32,
    pub channels: usize,
    /// Encoder input rate. Fixed for the lifetime of this pipeline.
    pub encode_rate: u32,
    /// Decoder output rate. May differ from `encode_rate` (MP3 auto-
    /// downsample); updated via [`setup_i2h`].
    pub decode_rate: u32,

    h2i: Option<FftFixedIn<f32>>,
    i2h: Option<FftFixedIn<f32>>,
    pub h2i_chunk: usize,
    pub i2h_chunk: usize,
    h2i_in_buf: Vec<Vec<f32>>,
    h2i_out_buf: Vec<Vec<f32>>,
    i2h_in_buf: Vec<Vec<f32>>,
    i2h_out_buf: Vec<Vec<f32>>,

    drain_scratch: Vec<f32>,

    pub host_input: Vec<VecDeque<f32>>,
    pub internal_input: Vec<VecDeque<f32>>,
    pub internal_output: Vec<VecDeque<f32>>,
    pub host_output: Vec<VecDeque<f32>>,

    pub latency_host_samples: u32,
}

impl ResampledPipeline {
    pub fn new() -> Self {
        Self {
            ready: false,
            host_rate: 44_100,
            channels: 2,
            encode_rate: 44_100,
            decode_rate: 44_100,
            h2i: None,
            i2h: None,
            h2i_chunk: 0,
            i2h_chunk: 0,
            h2i_in_buf: Vec::new(),
            h2i_out_buf: Vec::new(),
            i2h_in_buf: Vec::new(),
            i2h_out_buf: Vec::new(),
            drain_scratch: Vec::new(),
            host_input: Vec::new(),
            internal_input: Vec::new(),
            internal_output: Vec::new(),
            host_output: Vec::new(),
            latency_host_samples: 0,
        }
    }

    /// (Re)initialise the pipeline. Codecs that change `decode_rate`
    /// dynamically (MP3) pass a sensible initial `decode_rate_hint` here
    /// and call [`setup_i2h`] later. Returns `false` on resampler-build
    /// failure; caller should bail.
    pub fn setup(
        &mut self,
        host_rate: u32,
        channels: usize,
        max_block_size: usize,
        encode_rate: u32,
        decode_rate_hint: u32,
    ) -> bool {
        self.ready = false;
        self.host_rate = host_rate;
        self.channels = channels;
        self.encode_rate = encode_rate;
        self.decode_rate = decode_rate_hint;

        if !self.setup_h2i() {
            return false;
        }
        if !self.setup_i2h(decode_rate_hint) {
            return false;
        }

        // host buffer + ~100 ms of push/drain cadence skew. VecDeque grows
        // beyond this if needed; the hint just avoids the first few allocs.
        let ring_cap = max_block_size + (host_rate as usize / 10) + 1;
        self.host_input = (0..channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();
        self.internal_input = (0..channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();
        self.internal_output = (0..channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();
        self.host_output = (0..channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();

        self.latency_host_samples = 0;
        self.ready = true;
        true
    }

    /// Build the host→encode-rate resampler, or skip it (direct path)
    /// when the rates already match.
    fn setup_h2i(&mut self) -> bool {
        self.h2i = None;
        self.h2i_chunk = 0;
        self.h2i_in_buf.clear();
        self.h2i_out_buf.clear();
        if self.host_rate == self.encode_rate {
            self.update_drain_scratch();
            return true;
        }
        // ~20 ms chunk: clean rubato ratio across all common host rates.
        let chunk = (self.host_rate as usize) / 50;
        let r = match FftFixedIn::<f32>::new(
            self.host_rate as usize,
            self.encode_rate as usize,
            chunk,
            RESAMPLER_SUB_CHUNKS,
            self.channels,
        ) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let out_max = r.output_frames_max();
        self.h2i_chunk = chunk;
        self.h2i_in_buf = vec![vec![0.0; chunk]; self.channels];
        self.h2i_out_buf = vec![vec![0.0; out_max]; self.channels];
        self.h2i = Some(r);
        self.update_drain_scratch();
        true
    }

    /// Rebuild the decode-rate→host resampler. Called by codecs whose
    /// decoder rate changes at runtime (MP3 auto-downsample). Idempotent.
    pub fn setup_i2h(&mut self, decode_rate: u32) -> bool {
        self.i2h = None;
        self.i2h_chunk = 0;
        self.i2h_in_buf.clear();
        self.i2h_out_buf.clear();
        self.decode_rate = decode_rate;
        if decode_rate == self.host_rate {
            self.update_drain_scratch();
            return true;
        }
        let chunk = (decode_rate as usize) / 50;
        let r = match FftFixedIn::<f32>::new(
            decode_rate as usize,
            self.host_rate as usize,
            chunk,
            RESAMPLER_SUB_CHUNKS,
            self.channels,
        ) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let out_max = r.output_frames_max();
        self.i2h_chunk = chunk;
        self.i2h_in_buf = vec![vec![0.0; chunk]; self.channels];
        self.i2h_out_buf = vec![vec![0.0; out_max]; self.channels];
        self.i2h = Some(r);
        self.update_drain_scratch();
        true
    }

    fn update_drain_scratch(&mut self) {
        let needed = self.h2i_chunk.max(self.i2h_chunk);
        if self.drain_scratch.len() < needed {
            self.drain_scratch.resize(needed, 0.0);
        }
    }

    /// Pre-fill `host_output` with N silence so the audio thread can read
    /// before the codec produces its first real sample.
    pub fn set_latency(&mut self, latency_host_samples: u32) {
        if !self.ready {
            return;
        }
        self.latency_host_samples = latency_host_samples;
        let n = latency_host_samples as usize;
        for ch in 0..self.channels {
            // Bulk reserve + extend with a sized iterator so VecDeque
            // pre-grows once and memsets in place.
            self.host_output[ch].clear();
            self.host_output[ch].reserve(n);
            self.host_output[ch].extend(std::iter::repeat_n(0.0, n));
        }
    }

    /// Grow the pre-fill to `target` if currently shorter. Used to align
    /// every codec to the plugin-wide latency so switches don't re-tick PDC.
    pub fn pad_output_to(&mut self, target: u32) {
        if !self.ready || target <= self.latency_host_samples {
            return;
        }
        let extra = (target - self.latency_host_samples) as usize;
        for ch in 0..self.channels {
            self.host_output[ch].reserve(extra);
            self.host_output[ch].extend(std::iter::repeat_n(0.0, extra));
        }
        self.latency_host_samples = target;
    }

    /// Push one block of host samples (per-channel) into `host_input`.
    pub fn push_host_block(&mut self, block: &[&mut [f32]], n_samples: usize) {
        if !self.ready {
            return;
        }
        for ch in 0..self.channels {
            self.host_input[ch].extend(block[ch][..n_samples].iter().copied());
        }
    }

    /// Drain `n_samples` per channel into the host buffer via
    /// `make_contiguous` + `copy_from_slice` (one bulk copy, autovec
    /// friendly — `pop_front` per sample inhibits vectorisation). Pads
    /// with silence on underflow, which shouldn't happen at steady state.
    pub fn drain_host_block(&mut self, block: &mut [&mut [f32]], n_samples: usize) {
        if !self.ready {
            for ch in 0..self.channels {
                block[ch][..n_samples].fill(0.0);
            }
            return;
        }
        for ch in 0..self.channels {
            let take = self.host_output[ch].len().min(n_samples);
            if take > 0 {
                let head = self.host_output[ch].make_contiguous();
                block[ch][..take].copy_from_slice(&head[..take]);
                self.host_output[ch].drain(..take);
            }
            if take < n_samples {
                block[ch][take..n_samples].fill(0.0);
            }
        }
    }

    /// Pump `host_input` → `internal_input` (resample or direct copy).
    pub fn pump_host_to_internal(&mut self) {
        if !self.ready {
            return;
        }
        match &mut self.h2i {
            None => {
                // host_rate == encode_rate. Bulk memcpy via
                // `make_contiguous`; no FFT work to hide churn here so a
                // pop_front/push_back loop would be measurably slower.
                for ch in 0..self.channels {
                    let n = self.host_input[ch].len();
                    if n > 0 {
                        self.internal_input[ch].reserve(n);
                        let src = self.host_input[ch].make_contiguous();
                        self.internal_input[ch].extend(src[..n].iter().copied());
                        self.host_input[ch].clear();
                    }
                }
            }
            Some(r) => {
                let chunk = self.h2i_chunk;
                while self.host_input[0].len() >= chunk {
                    for ch in 0..self.channels {
                        bulk_drain(&mut self.host_input[ch], &mut self.h2i_in_buf[ch], chunk);
                    }
                    let produced = r
                        .process_into_buffer(&self.h2i_in_buf, &mut self.h2i_out_buf, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.channels {
                        self.internal_input[ch]
                            .extend(self.h2i_out_buf[ch][..produced].iter().copied());
                    }
                }
            }
        }
    }

    /// Pump `internal_output` → `host_output` (resample or direct copy).
    pub fn pump_internal_to_host(&mut self) {
        if !self.ready {
            return;
        }
        match &mut self.i2h {
            None => {
                // Direct path — same memcpy rationale as `pump_host_to_internal`.
                for ch in 0..self.channels {
                    let n = self.internal_output[ch].len();
                    if n > 0 {
                        self.host_output[ch].reserve(n);
                        let src = self.internal_output[ch].make_contiguous();
                        self.host_output[ch].extend(src[..n].iter().copied());
                        self.internal_output[ch].clear();
                    }
                }
            }
            Some(r) => {
                let chunk = self.i2h_chunk;
                while self.internal_output[0].len() >= chunk {
                    for ch in 0..self.channels {
                        bulk_drain(
                            &mut self.internal_output[ch],
                            &mut self.i2h_in_buf[ch],
                            chunk,
                        );
                    }
                    let produced = r
                        .process_into_buffer(&self.i2h_in_buf, &mut self.i2h_out_buf, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.channels {
                        self.host_output[ch]
                            .extend(self.i2h_out_buf[ch][..produced].iter().copied());
                    }
                }
            }
        }
    }

    /// Output-side latency: `(chunk_at_host, delay_at_host)`.
    pub fn i2h_latency_pair(&self) -> (u32, u32) {
        match &self.i2h {
            None => (0, 0),
            Some(r) => {
                let chunk_at_host = if self.decode_rate == 0 {
                    0
                } else {
                    (self.i2h_chunk as u64 * self.host_rate as u64 / self.decode_rate as u64) as u32
                };
                (chunk_at_host, r.output_delay() as u32)
            }
        }
    }

    /// Input-side latency: `(chunk_at_host, delay_at_encode_rate)`.
    pub fn h2i_latency_pair(&self) -> (u32, u32) {
        match &self.h2i {
            None => (0, 0),
            Some(r) => (self.h2i_chunk as u32, r.output_delay() as u32),
        }
    }

    /// Compute worst-case host-rate latency without instantiating the
    /// codec. Called from `Plugin::initialize` — builds a throwaway
    /// pipeline to read rubato's `output_delay()`, then drops it.
    /// `codec_roundtrip_internal` is the encoder + decoder warm-up at
    /// encode rate (e.g. `4 * 1024` for AAC-LC: 4 frames × 1024 samples).
    pub fn estimate_latency(
        host_rate: u32,
        channels: usize,
        encode_rate: u32,
        decode_rate: u32,
        codec_roundtrip_internal: u32,
    ) -> u32 {
        let mut probe = ResampledPipeline::new();
        // `max_block_size` is a ring-capacity hint — irrelevant for latency.
        if !probe.setup(host_rate, channels, 1, encode_rate, decode_rate) {
            return 0;
        }
        let host = host_rate as u64;
        let internal = encode_rate as u64;
        let to_host = |internal_samples: u32| {
            (internal_samples as u64 * host / internal) as u32
        };
        let (h2i_chunk_host, h2i_delay_internal) = probe.h2i_latency_pair();
        let (i2h_chunk_at_host, i2h_delay_at_host) = probe.i2h_latency_pair();
        h2i_chunk_host
            + to_host(h2i_delay_internal + codec_roundtrip_internal)
            + i2h_chunk_at_host
            + i2h_delay_at_host
    }

    /// Clear all rings + rubato state, then re-prime `host_output` with
    /// the preroll silence. Called from `Plugin::reset`.
    pub fn reset(&mut self) {
        if !self.ready {
            return;
        }
        if let Some(r) = &mut self.h2i {
            r.reset();
        }
        if let Some(r) = &mut self.i2h {
            r.reset();
        }
        let n = self.latency_host_samples as usize;
        for ch in 0..self.channels {
            self.host_input[ch].clear();
            self.internal_input[ch].clear();
            self.internal_output[ch].clear();
            self.host_output[ch].clear();
            self.host_output[ch].reserve(n);
            self.host_output[ch].extend(std::iter::repeat_n(0.0, n));
        }
    }
}

impl Default for ResampledPipeline {
    fn default() -> Self {
        Self::new()
    }
}

/// Bulk-drain via `make_contiguous` + `copy_from_slice` so the per-element
/// bounds checks fold into one sliced copy (autovec friendly). Pads with
/// zeros on underflow to match `pop_front().unwrap_or(0.0)` semantics.
fn bulk_drain(ring: &mut VecDeque<f32>, dst: &mut [f32], n: usize) {
    debug_assert!(dst.len() >= n);
    let take = ring.len().min(n);
    if take > 0 {
        let head = ring.make_contiguous();
        dst[..take].copy_from_slice(&head[..take]);
        ring.drain(..take);
    }
    for s in &mut dst[take..n] {
        *s = 0.0;
    }
}

// ── Bulk codec-glue helpers ─────────────────────────────────────────
//
// Each codec used to inline its own per-sample VecDeque ↔ interleaved-i16
// conversion. Centralised here so the `clamp + as i16` quantisation can
// be vectorised once (single `make_contiguous` + sliced loop), and any
// future `std::simd` upgrade lands in one place.

/// Drain `n` samples per ring and write them packed-interleaved as i16
/// with `clamp(-1, 1) * 32767.0` quantisation. Pads with zero on underflow.
pub fn drain_to_i16_interleaved(rings: &mut [VecDeque<f32>], n: usize, out: &mut [i16]) {
    let channels = rings.len();
    debug_assert!(out.len() >= n * channels);
    for ch in 0..channels {
        let take = rings[ch].len().min(n);
        if take > 0 {
            let head = rings[ch].make_contiguous();
            for s in 0..take {
                let f = head[s].clamp(-1.0, 1.0);
                out[s * channels + ch] = (f * 32767.0) as i16;
            }
            rings[ch].drain(..take);
        }
        for s in take..n {
            out[s * channels + ch] = 0;
        }
    }
}

/// Inverse of [`drain_to_i16_interleaved`]: push interleaved i16 into the
/// rings as f32 (`i16 as f32 / 32768.0`).
pub fn push_i16_interleaved(src: &[i16], n: usize, channels: usize, rings: &mut [VecDeque<f32>]) {
    debug_assert!(rings.len() >= channels);
    debug_assert!(src.len() >= n * channels);
    for ch in 0..channels {
        rings[ch].reserve(n);
        for s in 0..n {
            let f = src[s * channels + ch] as f32 / 32768.0;
            rings[ch].push_back(f);
        }
    }
}

/// Drain into planar f32 buffers (no quantisation). Used by libvorbis,
/// which takes/produces f32 directly.
pub fn drain_to_planar_f32(rings: &mut [VecDeque<f32>], n: usize, dest: &mut [&mut [f32]]) {
    let channels = rings.len();
    debug_assert!(dest.len() >= channels);
    for ch in 0..channels {
        let take = rings[ch].len().min(n);
        if take > 0 {
            let head = rings[ch].make_contiguous();
            dest[ch][..take].copy_from_slice(&head[..take]);
            rings[ch].drain(..take);
        }
        if take < n {
            dest[ch][take..n].fill(0.0);
        }
    }
}

/// Push planar f32 slices into the rings via bulk `extend`.
pub fn push_planar_f32(src: &[&[f32]], rings: &mut [VecDeque<f32>]) {
    let channels = src.len().min(rings.len());
    for ch in 0..channels {
        rings[ch].reserve(src[ch].len());
        rings[ch].extend(src[ch].iter().copied());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub codec that copies `internal_input` → `internal_output` 1:1.
    /// If this passes, any silence in a real codec is the codec's fault.
    #[test]
    fn pipeline_passthrough_emits_audio_at_each_host_rate() {
        let block_size = 256usize;
        let channels = 2usize;
        // (host_rate, encode_rate) pairs covering direct path + each
        // resample direction the real codecs actually take.
        let cases = [
            (44_100u32, 44_100u32),
            (44_100, 48_000),
            (48_000, 44_100),
            (96_000, 44_100),
            (192_000, 44_100),
            (32_000, 44_100),
        ];

        for &(host_rate, encode_rate) in &cases {
            let mut pipeline = ResampledPipeline::new();
            assert!(
                pipeline.setup(host_rate, channels, block_size, encode_rate, encode_rate),
                "pipeline.setup failed at host={} encode={}",
                host_rate,
                encode_rate
            );
            pipeline.set_latency(host_rate / 4);

            let total_samples = (host_rate as usize) * 3;

            let mut produced_nonzero = 0usize;
            let mut produced_total = 0usize;
            let mut cursor = 0usize;
            let mut block_in: Vec<Vec<f32>> = vec![vec![0.0; block_size]; channels];
            let mut block_out: Vec<Vec<f32>> = vec![vec![0.0; block_size]; channels];

            while cursor + block_size <= total_samples {
                for s in 0..block_size {
                    let t = (cursor + s) as f32 / host_rate as f32;
                    let v = (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5;
                    for ch in 0..channels {
                        block_in[ch][s] = v;
                    }
                }

                for ch in 0..channels {
                    for s in 0..block_size {
                        pipeline.host_input[ch].push_back(block_in[ch][s]);
                    }
                }
                pipeline.pump_host_to_internal();
                // Stub codec.
                for ch in 0..channels {
                    while let Some(s) = pipeline.internal_input[ch].pop_front() {
                        pipeline.internal_output[ch].push_back(s);
                    }
                }
                pipeline.pump_internal_to_host();
                for ch in 0..channels {
                    for s in 0..block_size {
                        block_out[ch][s] =
                            pipeline.host_output[ch].pop_front().unwrap_or(0.0);
                    }
                }
                for ch in 0..channels {
                    for s in 0..block_size {
                        produced_total += 1;
                        if block_out[ch][s].abs() > 1e-4 {
                            produced_nonzero += 1;
                        }
                    }
                }
                cursor += block_size;
            }

            let nonzero_pct =
                100.0 * produced_nonzero as f64 / produced_total.max(1) as f64;
            eprintln!(
                "host={} Hz, encode={} Hz, nonzero {}/{} ({:.1}%)",
                host_rate,
                encode_rate,
                produced_nonzero,
                produced_total,
                nonzero_pct
            );
            assert!(
                produced_nonzero > 0,
                "pipeline at host={} encode={} produced 0 nonzero samples",
                host_rate,
                encode_rate
            );
            // 3 s of sine vs ~250 ms preroll → well over half nonzero.
            assert!(
                nonzero_pct > 50.0,
                "pipeline at host={} encode={} produced only {:.1}% nonzero output",
                host_rate,
                encode_rate,
                nonzero_pct
            );
        }
    }

    // ── Bulk-helper unit tests ────────────────────────────────────

    fn fill_rings(samples: &[Vec<f32>]) -> Vec<VecDeque<f32>> {
        samples
            .iter()
            .map(|ch| ch.iter().copied().collect::<VecDeque<_>>())
            .collect()
    }

    #[test]
    fn drain_to_i16_interleaved_quantises_correctly() {
        // Boundary values -1, 0, +1 → -32767, 0, +32767, interleaved
        // [s0 ch0, s0 ch1, s1 ch0, …].
        let mut rings = fill_rings(&[vec![-1.0, 0.0, 1.0], vec![-1.0, 0.0, 1.0]]);
        let mut out = vec![0i16; 3 * 2];
        drain_to_i16_interleaved(&mut rings, 3, &mut out);
        assert_eq!(out[0], -32767);
        assert_eq!(out[1], -32767);
        assert_eq!(out[2], 0);
        assert_eq!(out[3], 0);
        assert_eq!(out[4], 32767);
        assert_eq!(out[5], 32767);
        assert!(rings.iter().all(|r| r.is_empty()));
    }

    #[test]
    fn drain_to_i16_interleaved_clamps_out_of_range() {
        let mut rings = fill_rings(&[vec![-2.5, 5.0], vec![-2.5, 5.0]]);
        let mut out = vec![0i16; 4];
        drain_to_i16_interleaved(&mut rings, 2, &mut out);
        assert_eq!(out[0], -32767);
        assert_eq!(out[1], -32767);
        assert_eq!(out[2], 32767);
        assert_eq!(out[3], 32767);
    }

    #[test]
    fn drain_to_i16_interleaved_underflow_pads_zeros() {
        let mut rings = fill_rings(&[vec![1.0, -1.0], vec![1.0, -1.0]]);
        let mut out = vec![123i16; 4 * 2];
        drain_to_i16_interleaved(&mut rings, 4, &mut out);
        assert_eq!(out[0], 32767);
        assert_eq!(out[1], 32767);
        assert_eq!(out[2], -32767);
        assert_eq!(out[3], -32767);
        assert_eq!(out[4], 0);
        assert_eq!(out[5], 0);
        assert_eq!(out[6], 0);
        assert_eq!(out[7], 0);
    }

    #[test]
    fn push_i16_interleaved_dequantises_correctly() {
        let src: Vec<i16> = vec![-32768, -32768, 0, 0, 32767, 32767];
        let mut rings: Vec<VecDeque<f32>> =
            (0..2).map(|_| VecDeque::new()).collect();
        push_i16_interleaved(&src, 3, 2, &mut rings);
        assert!((rings[0][0] - -1.0).abs() < 1e-6);
        assert!((rings[1][0] - -1.0).abs() < 1e-6);
        assert!(rings[0][1].abs() < 1e-6);
        assert!(rings[1][1].abs() < 1e-6);
        assert!(rings[0][2] > 0.99 && rings[0][2] < 1.0);
    }

    /// f32 → i16 → f32 should be lossless within ~1 LSB.
    #[test]
    fn i16_roundtrip_preserves_samples_modulo_quantisation() {
        let original: Vec<Vec<f32>> = vec![
            vec![0.0, 0.5, -0.5, 0.25, -0.25],
            vec![0.0, -0.5, 0.5, -0.25, 0.25],
        ];
        let mut rings = fill_rings(&original);
        let n = original[0].len();
        let mut interleaved = vec![0i16; n * 2];
        drain_to_i16_interleaved(&mut rings, n, &mut interleaved);
        let mut roundtripped: Vec<VecDeque<f32>> =
            (0..2).map(|_| VecDeque::new()).collect();
        push_i16_interleaved(&interleaved, n, 2, &mut roundtripped);
        for ch in 0..2 {
            for s in 0..n {
                assert!(
                    (original[ch][s] - roundtripped[ch][s]).abs() < 1.0 / 32767.0 + 1e-6,
                    "ch{ch}[{s}] {} -> {}",
                    original[ch][s],
                    roundtripped[ch][s]
                );
            }
        }
    }

    #[test]
    fn drain_to_planar_f32_copies_then_drains() {
        let mut rings = fill_rings(&[vec![1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0]]);
        let mut buf_l: Vec<f32> = vec![0.0; 3];
        let mut buf_r: Vec<f32> = vec![0.0; 3];
        let mut dest: Vec<&mut [f32]> = vec![&mut buf_l, &mut buf_r];
        drain_to_planar_f32(&mut rings, 3, &mut dest);
        assert_eq!(buf_l, vec![1.0, 2.0, 3.0]);
        assert_eq!(buf_r, vec![4.0, 5.0, 6.0]);
        assert!(rings.iter().all(|r| r.is_empty()));
    }

    #[test]
    fn drain_to_planar_f32_underflow_pads_zeros() {
        let mut rings = fill_rings(&[vec![1.0, 2.0], vec![4.0, 5.0]]);
        let mut buf_l: Vec<f32> = vec![99.0; 4];
        let mut buf_r: Vec<f32> = vec![99.0; 4];
        let mut dest: Vec<&mut [f32]> = vec![&mut buf_l, &mut buf_r];
        drain_to_planar_f32(&mut rings, 4, &mut dest);
        assert_eq!(buf_l, vec![1.0, 2.0, 0.0, 0.0]);
        assert_eq!(buf_r, vec![4.0, 5.0, 0.0, 0.0]);
    }

    #[test]
    fn push_planar_f32_appends_in_order() {
        let mut rings: Vec<VecDeque<f32>> =
            (0..2).map(|_| VecDeque::new()).collect();
        let l = [1.0_f32, 2.0, 3.0];
        let r = [4.0_f32, 5.0, 6.0];
        push_planar_f32(&[&l, &r], &mut rings);
        assert_eq!(rings[0].iter().copied().collect::<Vec<_>>(), vec![1.0, 2.0, 3.0]);
        assert_eq!(rings[1].iter().copied().collect::<Vec<_>>(), vec![4.0, 5.0, 6.0]);
    }

    // ── set_latency / pad_output_to / reset ───────────────────────

    #[test]
    fn set_latency_pre_fills_host_output_with_silence() {
        let mut p = ResampledPipeline::new();
        assert!(p.setup(48_000, 2, 256, 48_000, 48_000));
        p.set_latency(1_024);
        assert_eq!(p.latency_host_samples, 1_024);
        for ch in 0..2 {
            assert_eq!(p.host_output[ch].len(), 1_024);
            assert!(p.host_output[ch].iter().all(|&s| s == 0.0));
        }
    }

    #[test]
    fn pad_output_to_grows_only_when_target_is_larger() {
        let mut p = ResampledPipeline::new();
        assert!(p.setup(48_000, 2, 256, 48_000, 48_000));
        p.set_latency(512);
        assert_eq!(p.host_output[0].len(), 512);
        p.pad_output_to(256);
        assert_eq!(p.host_output[0].len(), 512);
        assert_eq!(p.latency_host_samples, 512);
        p.pad_output_to(512);
        assert_eq!(p.host_output[0].len(), 512);
        p.pad_output_to(2_048);
        assert_eq!(p.host_output[0].len(), 2_048);
        assert_eq!(p.latency_host_samples, 2_048);
    }

    #[test]
    fn reset_restores_prefill_and_clears_other_rings() {
        let mut p = ResampledPipeline::new();
        assert!(p.setup(48_000, 2, 256, 48_000, 48_000));
        p.set_latency(256);
        for ch in 0..2 {
            for _ in 0..32 {
                p.host_input[ch].push_back(0.5);
                p.internal_input[ch].push_back(0.5);
                p.internal_output[ch].push_back(0.5);
                p.host_output[ch].push_back(0.5);
            }
        }
        p.reset();
        for ch in 0..2 {
            assert!(p.host_input[ch].is_empty());
            assert!(p.internal_input[ch].is_empty());
            assert!(p.internal_output[ch].is_empty());
            assert_eq!(p.host_output[ch].len(), 256);
            assert!(p.host_output[ch].iter().all(|&s| s == 0.0));
        }
    }

    // ── Latency math ──────────────────────────────────────────────

    /// Rates pulled from the real codec set so a regression in
    /// `setup_h2i` / `setup_i2h` would surface here.
    #[test]
    fn estimate_latency_is_positive_for_real_codec_combinations() {
        for &(host, encode, decode, codec_budget) in &[
            (44_100u32, 48_000u32, 48_000u32, 960u32 * 4),
            (48_000, 44_100, 44_100, 1024 * 4),
            (96_000, 44_100, 22_050, 1152 * 4),
            (44_100, 44_100, 44_100, 1024 * 4),
        ] {
            let lat = ResampledPipeline::estimate_latency(host, 2, encode, decode, codec_budget);
            assert!(
                lat > 0,
                "estimate_latency host={host} encode={encode} decode={decode} returned 0"
            );
        }
    }

    #[test]
    fn h2i_and_i2h_latency_pairs_are_zero_when_rates_match() {
        let mut p = ResampledPipeline::new();
        assert!(p.setup(48_000, 2, 256, 48_000, 48_000));
        assert_eq!(p.h2i_latency_pair(), (0, 0));
        assert_eq!(p.i2h_latency_pair(), (0, 0));
    }

    #[test]
    fn h2i_and_i2h_latency_pairs_are_nonzero_when_rates_differ() {
        let mut p = ResampledPipeline::new();
        assert!(p.setup(48_000, 2, 256, 44_100, 44_100));
        let (chunk, delay) = p.h2i_latency_pair();
        assert!(chunk > 0 && delay > 0, "h2i: chunk={chunk} delay={delay}");
        let (chunk, delay) = p.i2h_latency_pair();
        assert!(chunk > 0 && delay > 0, "i2h: chunk={chunk} delay={delay}");
    }

    // ── pump_* before / after setup ───────────────────────────────

    #[test]
    fn pumps_are_no_ops_when_not_ready() {
        let mut p = ResampledPipeline::new();
        p.pump_host_to_internal();
        p.pump_internal_to_host();
        assert!(p.host_input.is_empty());
        assert!(p.internal_input.is_empty());
        assert!(p.internal_output.is_empty());
        assert!(p.host_output.is_empty());
    }

    #[test]
    fn drain_host_block_writes_silence_when_not_ready() {
        let mut p = ResampledPipeline::new();
        let mut buf_l: Vec<f32> = vec![0.5; 4];
        let mut buf_r: Vec<f32> = vec![0.5; 4];
        let mut block: Vec<&mut [f32]> = vec![&mut buf_l, &mut buf_r];
        p.drain_host_block(&mut block, 4);
        assert_eq!(buf_l, vec![0.0; 4]);
        assert_eq!(buf_r, vec![0.0; 4]);
    }

    /// MP3 auto-downsample case: encode 44.1 kHz, decode 24 kHz.
    #[test]
    fn pipeline_handles_mismatched_decode_rate() {
        let mut pipeline = ResampledPipeline::new();
        assert!(pipeline.setup(96_000, 2, 256, 44_100, 24_000));
        pipeline.set_latency(48_000);
        for _ in 0..100 {
            for ch in 0..2 {
                for _ in 0..256 {
                    pipeline.host_input[ch].push_back(0.5);
                }
            }
            pipeline.pump_host_to_internal();
            for ch in 0..2 {
                while let Some(s) = pipeline.internal_input[ch].pop_front() {
                    pipeline.internal_output[ch].push_back(s);
                }
            }
            pipeline.pump_internal_to_host();
        }
        let total: usize = pipeline.host_output.iter().map(|r| r.len()).sum();
        assert!(
            total > pipeline.latency_host_samples as usize,
            "pipeline didn't accumulate output beyond preroll: total={}, prefill={}",
            total,
            pipeline.latency_host_samples
        );
    }
}
