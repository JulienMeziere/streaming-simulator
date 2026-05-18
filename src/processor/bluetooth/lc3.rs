//! LC3 — Bluetooth LE Audio (5.2+) standard. Pure-Rust `lc3-codec` crate
//! (Apache 2.0 OR MIT, no FFI). 10 ms frames at 48 kHz.
//!
//! `Lc3Encoder<'a>` / `Lc3Decoder<'a>` borrow working buffers by reference.
//! Storing them next to the buffers in one struct would be self-referential,
//! so we heap-allocate the buffers as `Box<[T]>` and construct the codecs
//! with an unsafe lifetime extension to `'static`. Soundness rests on:
//!
//! 1. `Box<[T]>`'s heap data has a stable address across struct moves —
//!    only the fat pointer moves, the pointed-to bytes stay put.
//! 2. Field declaration order = drop order: encoder + decoder are declared
//!    first and so dropped first, before the buffers they borrow from.
//!
//! Don't reorder the struct fields without re-checking soundness.
//!
//! Operating points (frame duration 10 ms, sample rate 48 kHz):
//!
//! | Preset    | Channels | kbps/channel | frame bytes/ch |
//! | --------- | -------- | ------------ | -------------- |
//! | Low64     | mono     | 64           | 80             |
//! | High160   | stereo   | 80           | 100            |

use lc3_codec::{
    common::{
        complex::Complex,
        config::{FrameDuration, SamplingFrequency},
    },
    decoder::lc3_decoder::Lc3Decoder,
    encoder::lc3_encoder::Lc3Encoder,
};
use audioadapter_buffers::direct::SequentialSliceOfVecs;
use nih_plug::buffer::Buffer;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;

const LC3_RATE: u32 = 48_000;
/// 10 ms at 48 kHz.
const LC3_FRAME_SAMPLES: usize = 480;
const RESAMPLER_SUB_CHUNKS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lc3Quality {
    /// 64 kbps mono — LE Audio low-power. Stereo input is summed before
    /// encode and duplicated back to L=R after decode.
    Low64,
    /// 80 kbps × 2 = 160 kbps stereo — LE Audio high-quality.
    High160,
}

impl Lc3Quality {
    fn channels(self) -> usize {
        match self {
            Lc3Quality::Low64 => 1,
            Lc3Quality::High160 => 2,
        }
    }

    /// `lc3-codec` infers the bitrate from the output buffer length.
    fn frame_bytes_per_channel(self) -> usize {
        match self {
            Lc3Quality::Low64 => 80,    // 64 kbps × 10 ms / 8
            Lc3Quality::High160 => 100, // 80 kbps × 10 ms / 8
        }
    }
}

pub struct Lc3Codec {
    // Encoder/decoder declared first so they drop first (Rust drops
    // fields in declaration order). They hold lifetime-extended
    // references into the `_buf` fields below — those *must* outlive
    // them. See the module-level safety note.
    encoder: Option<Lc3Encoder<'static>>,
    decoder: Option<Lc3Decoder<'static>>,

    // Load-bearing for soundness. The compiler doesn't see the
    // encoder/decoder's reads (we hand it raw `*mut [...]` at
    // construction), so it warns "unused" — silenced here. Removing
    // these dangles the codecs' references.
    #[allow(dead_code)]
    enc_integer_buf: Box<[i16]>,
    #[allow(dead_code)]
    enc_scaler_buf: Box<[f32]>,
    #[allow(dead_code)]
    enc_complex_buf: Box<[Complex]>,
    #[allow(dead_code)]
    dec_scaler_buf: Box<[f32]>,
    #[allow(dead_code)]
    dec_complex_buf: Box<[Complex]>,

    quality: Lc3Quality,
    sample_rate: u32,
    host_channels: usize,
    codec_channels: usize,

    h2c: Option<Fft<f32>>,
    c2h: Option<Fft<f32>>,
    h2c_chunk: usize,
    c2h_chunk: usize,
    h2c_in_buf: Vec<Vec<f32>>,
    h2c_out_buf: Vec<Vec<f32>>,
    c2h_in_buf: Vec<Vec<f32>>,
    c2h_out_buf: Vec<Vec<f32>>,

    host_in: Vec<VecDeque<f32>>,
    /// 48 kHz ring before encode. Length = `codec_channels` (1 for Low64,
    /// 2 for High160).
    codec_in: Vec<VecDeque<f32>>,
    codec_out: Vec<VecDeque<f32>>,
    host_out: Vec<VecDeque<f32>>,

    pcm_i16: Vec<i16>,
    encoded: Vec<u8>,
    decoded_i16: Vec<i16>,

    /// Per-block scratch for the host↔codec channel-routing stage. Sized
    /// once for the worst case + resampler tail; `clear`-ed each `process`.
    /// Replaces a pair of `Vec<Vec<f32>>` allocated per callback.
    h_to_c_intermediate: Vec<Vec<f32>>,
    c_to_h_intermediate: Vec<Vec<f32>>,
}

// SAFETY: the codecs hold lifetime-extended references into our own
// `Box<[T]>` buffers. No thread locals, no platform handles — pure DSP
// state. We never share `Lc3Codec` across threads; `Send` is required
// because `BluetoothProcessor` must stay `Send` for nih-plug.
unsafe impl Send for Lc3Codec {}

impl Lc3Codec {
    pub fn new(
        host_sample_rate: u32,
        host_channels: usize,
        max_block_size: usize,
        quality: Lc3Quality,
    ) -> Self {
        let codec_channels = quality.channels();
        let frame_duration = FrameDuration::TenMs;
        let sampling_freq = SamplingFrequency::Hz48000;

        // Box<[T]> so the data address survives `Lc3Codec` moves.
        let (enc_int_len, enc_scl_len, enc_cpx_len) =
            Lc3Encoder::calc_working_buffer_lengths(codec_channels, frame_duration, sampling_freq);
        let mut enc_integer_buf: Box<[i16]> = vec![0i16; enc_int_len].into_boxed_slice();
        let mut enc_scaler_buf: Box<[f32]> = vec![0.0f32; enc_scl_len].into_boxed_slice();
        let mut enc_complex_buf: Box<[Complex]> =
            vec![Complex::default(); enc_cpx_len].into_boxed_slice();

        let (dec_scl_len, dec_cpx_len) =
            Lc3Decoder::calc_working_buffer_lengths(codec_channels, frame_duration, sampling_freq);
        let mut dec_scaler_buf: Box<[f32]> = vec![0.0f32; dec_scl_len].into_boxed_slice();
        let mut dec_complex_buf: Box<[Complex]> =
            vec![Complex::default(); dec_cpx_len].into_boxed_slice();

        // SAFETY: lifetime extension to 'static. (1) heap data is stable
        // across struct moves; (2) raw `*mut [...]` is taken before the
        // boxes move into the struct; (3) drop order = encoder/decoder
        // first, then buffers (see field declaration order).
        let encoder = unsafe {
            let int_ptr: *mut [i16] = &mut *enc_integer_buf;
            let scl_ptr: *mut [f32] = &mut *enc_scaler_buf;
            let cpx_ptr: *mut [Complex] = &mut *enc_complex_buf;
            Lc3Encoder::new(
                codec_channels,
                frame_duration,
                sampling_freq,
                &mut *int_ptr,
                &mut *scl_ptr,
                &mut *cpx_ptr,
            )
        };
        let decoder = unsafe {
            let scl_ptr: *mut [f32] = &mut *dec_scaler_buf;
            let cpx_ptr: *mut [Complex] = &mut *dec_complex_buf;
            Lc3Decoder::new(
                codec_channels,
                frame_duration,
                sampling_freq,
                &mut *scl_ptr,
                &mut *cpx_ptr,
            )
        };

        let mut codec = Self {
            encoder: Some(encoder),
            decoder: Some(decoder),
            enc_integer_buf,
            enc_scaler_buf,
            enc_complex_buf,
            dec_scaler_buf,
            dec_complex_buf,
            quality,
            sample_rate: host_sample_rate,
            host_channels,
            codec_channels,
            h2c: None,
            c2h: None,
            h2c_chunk: 0,
            c2h_chunk: 0,
            h2c_in_buf: Vec::new(),
            h2c_out_buf: Vec::new(),
            c2h_in_buf: Vec::new(),
            c2h_out_buf: Vec::new(),
            host_in: Vec::new(),
            codec_in: Vec::new(),
            codec_out: Vec::new(),
            host_out: Vec::new(),
            pcm_i16: vec![0i16; LC3_FRAME_SAMPLES],
            encoded: vec![0u8; quality.frame_bytes_per_channel()],
            decoded_i16: vec![0i16; LC3_FRAME_SAMPLES],
            h_to_c_intermediate: Vec::new(),
            c_to_h_intermediate: Vec::new(),
        };
        codec.setup_resamplers(max_block_size);
        codec
    }

    fn setup_resamplers(&mut self, max_block_size: usize) {
        let ring_cap = max_block_size + (self.sample_rate as usize / 10) + 1;
        let codec_ring_cap = max_block_size * 2 + LC3_FRAME_SAMPLES * 4;

        if self.sample_rate != LC3_RATE {
            let h2c_chunk = (self.sample_rate as usize / 100).max(1);
            let c2h_chunk = (LC3_RATE as usize / 100).max(1);
            // Resampler runs at host_channels — mono fold-down happens
            // *after* resampling, at the encoder boundary.
            let h2c = Fft::<f32>::new(
                self.sample_rate as usize,
                LC3_RATE as usize,
                h2c_chunk,
                RESAMPLER_SUB_CHUNKS,
                self.host_channels,
                FixedSync::Input,
            )
            .expect("LC3 h2c resampler init");
            let c2h = Fft::<f32>::new(
                LC3_RATE as usize,
                self.sample_rate as usize,
                c2h_chunk,
                RESAMPLER_SUB_CHUNKS,
                self.host_channels,
                FixedSync::Input,
            )
            .expect("LC3 c2h resampler init");
            let h2c_out_max = h2c.output_frames_max();
            let c2h_out_max = c2h.output_frames_max();
            self.h2c_chunk = h2c_chunk;
            self.c2h_chunk = c2h_chunk;
            self.h2c_in_buf = vec![vec![0.0; h2c_chunk]; self.host_channels];
            self.h2c_out_buf = vec![vec![0.0; h2c_out_max]; self.host_channels];
            self.c2h_in_buf = vec![vec![0.0; c2h_chunk]; self.host_channels];
            self.c2h_out_buf = vec![vec![0.0; c2h_out_max]; self.host_channels];
            self.h2c = Some(h2c);
            self.c2h = Some(c2h);
        }

        self.host_in = (0..self.host_channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();
        self.codec_in = (0..self.codec_channels)
            .map(|_| VecDeque::with_capacity(codec_ring_cap))
            .collect();
        self.codec_out = (0..self.codec_channels)
            .map(|_| VecDeque::with_capacity(codec_ring_cap))
            .collect();
        self.host_out = (0..self.host_channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();

        // Pre-sized routing scratch — `clear`-ed each block, never
        // reallocated. Worst case = larger of host/codec ring caps.
        let intermediate_cap = ring_cap.max(codec_ring_cap);
        self.h_to_c_intermediate = (0..self.host_channels)
            .map(|_| Vec::with_capacity(intermediate_cap))
            .collect();
        self.c_to_h_intermediate = (0..self.host_channels)
            .map(|_| Vec::with_capacity(intermediate_cap))
            .collect();

        let prefill = Self::worst_case_latency_at(self.sample_rate, self.host_channels) as usize;
        for ch in 0..self.host_channels {
            self.host_out[ch].reserve(prefill);
            self.host_out[ch].extend(std::iter::repeat_n(0.0, prefill));
        }
    }

    pub fn reset(&mut self) {
        if let Some(r) = &mut self.h2c {
            r.reset();
        }
        if let Some(r) = &mut self.c2h {
            r.reset();
        }
        for ch in 0..self.host_channels {
            self.host_in[ch].clear();
            self.host_out[ch].clear();
        }
        for ch in 0..self.codec_channels {
            self.codec_in[ch].clear();
            self.codec_out[ch].clear();
        }
        let prefill = Self::worst_case_latency_at(self.sample_rate, self.host_channels) as usize;
        for ch in 0..self.host_channels {
            for _ in 0..prefill {
                self.host_out[ch].push_back(0.0);
            }
        }
    }

    pub fn process(&mut self, buffer: &mut Buffer) {
        let n = buffer.samples();
        let block = buffer.as_slice();
        let host_channels = self.host_channels.min(block.len());

        for ch in 0..host_channels {
            for s in 0..n {
                self.host_in[ch].push_back(block[ch][s]);
            }
        }

        self.pump_host_to_codec();
        self.pump_codec();
        self.pump_codec_to_host();

        for ch in 0..host_channels {
            for s in 0..n {
                block[ch][s] = self.host_out[ch].pop_front().unwrap_or(0.0);
            }
        }
    }

    fn pump_host_to_codec(&mut self) {
        // Step A — host_in (host rate, host channels) → 48 kHz scratch.
        // Direct copy when rates match. Scratch is preallocated and
        // `clear`-ed for reuse, so no per-block heap traffic.
        for ch in 0..self.host_channels {
            self.h_to_c_intermediate[ch].clear();
        }
        match &mut self.h2c {
            None => {
                for ch in 0..self.host_channels {
                    let n = self.host_in[ch].len();
                    if n > 0 {
                        let head = self.host_in[ch].make_contiguous();
                        self.h_to_c_intermediate[ch].extend_from_slice(&head[..n]);
                        self.host_in[ch].clear();
                    }
                }
            }
            Some(r) => {
                let chunk = self.h2c_chunk;
                let out_max = self.h2c_out_buf[0].len();
                while self.host_in[0].len() >= chunk {
                    for ch in 0..self.host_channels {
                        let head = self.host_in[ch].make_contiguous();
                        self.h2c_in_buf[ch][..chunk].copy_from_slice(&head[..chunk]);
                        self.host_in[ch].drain(..chunk);
                    }
                    let in_adapter =
                        SequentialSliceOfVecs::new(&self.h2c_in_buf, self.host_channels, chunk)
                            .expect("LC3 h2c in-buffer dimensions match");
                    let mut out_adapter = SequentialSliceOfVecs::new_mut(
                        &mut self.h2c_out_buf,
                        self.host_channels,
                        out_max,
                    )
                    .expect("LC3 h2c out-buffer dimensions match");
                    let produced = r
                        .process_into_buffer(&in_adapter, &mut out_adapter, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.host_channels {
                        self.h_to_c_intermediate[ch]
                            .extend_from_slice(&self.h2c_out_buf[ch][..produced]);
                    }
                }
            }
        }

        // Step B — (host_channels @ 48 kHz) → (codec_channels @ 48 kHz).
        // Mono fold-down sums + halves L+R in one pass.
        let n = self.h_to_c_intermediate.first().map(|v| v.len()).unwrap_or(0);
        if self.codec_channels == 1 && self.host_channels >= 2 {
            self.codec_in[0].reserve(n);
            let l = &self.h_to_c_intermediate[0];
            let r = &self.h_to_c_intermediate[1];
            self.codec_in[0]
                .extend((0..n).map(|s| (l[s] + r.get(s).copied().unwrap_or(l[s])) * 0.5));
        } else {
            for ch in 0..self.codec_channels {
                let src_ch = ch.min(self.h_to_c_intermediate.len().saturating_sub(1));
                let src = &self.h_to_c_intermediate[src_ch];
                self.codec_in[ch].reserve(src.len());
                self.codec_in[ch].extend(src.iter().copied());
            }
        }
    }

    fn pump_codec_to_host(&mut self) {
        // Step A: convert (codec_channels at 48 kHz) → (host_channels
        // at 48 kHz). For mono codec → stereo host, duplicate L=R.
        // Reuses the persistent intermediate; no per-block alloc.
        for ch in 0..self.host_channels {
            self.c_to_h_intermediate[ch].clear();
        }
        let n = self.codec_out.first().map(|q| q.len()).unwrap_or(0);
        if self.codec_channels == 1 && self.host_channels >= 2 {
            let head = self.codec_out[0].make_contiguous();
            for ch in 0..self.host_channels {
                self.c_to_h_intermediate[ch].extend_from_slice(&head[..n]);
            }
            self.codec_out[0].drain(..n);
        } else {
            for ch in 0..self.host_channels {
                let src_ch = ch.min(self.codec_channels - 1);
                let head = self.codec_out[src_ch].make_contiguous();
                self.c_to_h_intermediate[ch].extend_from_slice(&head[..n]);
            }
            // Drain each source ring once. Iterating `codec_channels`
            // (not host_channels) avoids double-draining when multiple
            // host channels mapped to the same source.
            for src_ch in 0..self.codec_channels {
                self.codec_out[src_ch].drain(..n);
            }
        }

        // Step B — 48 kHz scratch → host_out at host rate.
        match &mut self.c2h {
            None => {
                for ch in 0..self.host_channels {
                    self.host_out[ch].reserve(self.c_to_h_intermediate[ch].len());
                    self.host_out[ch]
                        .extend(self.c_to_h_intermediate[ch].iter().copied());
                }
            }
            Some(r) => {
                let chunk = self.c2h_chunk;
                let out_max = self.c2h_out_buf[0].len();
                let mut idx = 0usize;
                while idx + chunk <= n {
                    for ch in 0..self.host_channels {
                        self.c2h_in_buf[ch][..chunk]
                            .copy_from_slice(&self.c_to_h_intermediate[ch][idx..idx + chunk]);
                    }
                    let in_adapter =
                        SequentialSliceOfVecs::new(&self.c2h_in_buf, self.host_channels, chunk)
                            .expect("LC3 c2h in-buffer dimensions match");
                    let mut out_adapter = SequentialSliceOfVecs::new_mut(
                        &mut self.c2h_out_buf,
                        self.host_channels,
                        out_max,
                    )
                    .expect("LC3 c2h out-buffer dimensions match");
                    let produced = r
                        .process_into_buffer(&in_adapter, &mut out_adapter, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.host_channels {
                        self.host_out[ch].reserve(produced);
                        self.host_out[ch]
                            .extend(self.c2h_out_buf[ch][..produced].iter().copied());
                    }
                    idx += chunk;
                }
                // Any tail < chunk is dropped; at steady state the
                // ring drains to a multiple of `chunk` per block.
            }
        }
    }

    fn pump_codec(&mut self) {
        let encoder = match self.encoder.as_mut() {
            Some(e) => e,
            None => return,
        };
        let decoder = match self.decoder.as_mut() {
            Some(d) => d,
            None => return,
        };
        let bytes_per_channel = self.quality.frame_bytes_per_channel();

        while self.codec_in[0].len() >= LC3_FRAME_SAMPLES {
            for ch in 0..self.codec_channels {
                // f32 → i16 quantise via `make_contiguous` slice + one
                // `drain(..N)` per channel.
                {
                    let head = self.codec_in[ch].make_contiguous();
                    let frame = &head[..LC3_FRAME_SAMPLES];
                    for (i, &f) in frame.iter().enumerate() {
                        self.pcm_i16[i] = (f.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                    self.codec_in[ch].drain(..LC3_FRAME_SAMPLES);
                }
                // Length tweak only — `encoded`'s capacity is fixed for
                // the codec's lifetime, so no heap traffic at steady state.
                self.encoded.resize(bytes_per_channel, 0);
                if encoder
                    .encode_frame(ch, &self.pcm_i16, &mut self.encoded)
                    .is_err()
                {
                    self.codec_out[ch].reserve(LC3_FRAME_SAMPLES);
                    self.codec_out[ch]
                        .extend(std::iter::repeat_n(0.0, LC3_FRAME_SAMPLES));
                    continue;
                }
                // First arg is `num_bits_per_audio_sample` — `lc3-codec`
                // documents this as "should be 16".
                if decoder
                    .decode_frame(16, ch, &self.encoded, &mut self.decoded_i16)
                    .is_err()
                {
                    self.codec_out[ch].reserve(LC3_FRAME_SAMPLES);
                    self.codec_out[ch]
                        .extend(std::iter::repeat_n(0.0, LC3_FRAME_SAMPLES));
                    continue;
                }
                self.codec_out[ch].reserve(LC3_FRAME_SAMPLES);
                self.codec_out[ch].extend(
                    self.decoded_i16[..LC3_FRAME_SAMPLES]
                        .iter()
                        .map(|&v| v as f32 / 32768.0),
                );
            }
        }
    }

    /// ~25 ms resampler at non-48k hosts + 4 LC3 frames (~40 ms) warm-up.
    pub fn worst_case_latency_at(host_rate: u32, _channels: usize) -> u32 {
        let resampler_delay = if host_rate != LC3_RATE {
            (host_rate as f32 * 0.025) as u32
        } else {
            0
        };
        let codec_delay = (host_rate as u64 * 4 * LC3_FRAME_SAMPLES as u64
            / LC3_RATE as u64) as u32;
        resampler_delay + codec_delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Lc3Codec {
        pub fn process_planar(&mut self, input: &[Vec<f32>], output: &mut [Vec<f32>]) {
            let n = input[0].len();
            for ch in 0..self.host_channels {
                for s in 0..n {
                    self.host_in[ch].push_back(input[ch][s]);
                }
            }
            self.pump_host_to_codec();
            self.pump_codec();
            self.pump_codec_to_host();
            for ch in 0..self.host_channels {
                for s in 0..n {
                    output[ch][s] = self.host_out[ch].pop_front().unwrap_or(0.0);
                }
            }
        }
    }

    #[test]
    fn lc3_roundtrip_emits_audio_at_every_host_rate() {
        for &host_rate in &[44_100u32, 48_000, 96_000] {
            for &quality in &[Lc3Quality::Low64, Lc3Quality::High160] {
                let mut codec = Lc3Codec::new(host_rate, 2, 256, quality);
                let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
                    host_rate,
                    256,
                    2.0,
                    0.25,
                    1_000.0,
                    0.3,
                    |inp, out| codec.process_planar(inp, out),
                );
                assert!(
                    peak > 0.05,
                    "LC3 at {} Hz / {:?} produced near-silent output ({:.3})",
                    host_rate,
                    quality,
                    peak
                );
            }
        }
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 96_000] {
            let l = Lc3Codec::worst_case_latency_at(rate, 2);
            assert!(l > 0, "LC3 worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut codec = Lc3Codec::new(48_000, 2, 256, Lc3Quality::High160);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            codec.process_planar(&inp, &mut out);
        }
        codec.reset();
    }
}
