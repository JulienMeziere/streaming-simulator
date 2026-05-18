//! SBC (Sub-Band Coding) — universal A2DP baseline. FFI to BlueZ libsbc
//! via `libsbc-sys` (with `source-build` so no system libsbc is needed,
//! including cross-compile). Internal rate 44.1 kHz; max-quality preset
//! (8 subbands, 16 blocks, joint stereo, SNR allocation), bitpool 19 or
//! 53 for Low / High.

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use libsbc_sys as sbc_sys;
use nih_plug::buffer::Buffer;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::os::raw::c_void;

const SBC_RATE: u32 = 44_100;

/// 8 subbands × 16 blocks = 128 samples / channel / frame.
const SBC_FRAME_SAMPLES: usize = 128;

/// Generous upper bound for one frame at bitpool 53 stereo.
const SBC_MAX_FRAME_BYTES: usize = 1024;

const RESAMPLER_SUB_CHUNKS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SbcQuality {
    /// Bitpool 19, ~127 kbps stereo — cheap earbuds.
    Low,
    /// Bitpool 53, ~328 kbps stereo — modern BT default.
    High,
}

impl SbcQuality {
    fn bitpool(self) -> u8 {
        match self {
            SbcQuality::Low => 19,
            SbcQuality::High => 53,
        }
    }
}

/// `Drop` calls `sbc_finish` on both enc/dec — see impl below.
pub struct SbcCodec {
    enc: sbc_sys::sbc_t,
    dec: sbc_sys::sbc_t,

    sample_rate: u32,
    channels: usize,

    /// `None` when host rate already matches `SBC_RATE`.
    h2c: Option<Fft<f32>>,
    c2h: Option<Fft<f32>>,
    h2c_chunk: usize,
    c2h_chunk: usize,
    h2c_in_buf: Vec<Vec<f32>>,
    h2c_out_buf: Vec<Vec<f32>>,
    c2h_in_buf: Vec<Vec<f32>>,
    c2h_out_buf: Vec<Vec<f32>>,

    host_in: Vec<VecDeque<f32>>,
    sbc_in: Vec<VecDeque<f32>>,
    sbc_out: Vec<VecDeque<f32>>,
    host_out: Vec<VecDeque<f32>>,

    /// Scratch — encoder input + decoder output share it (no time overlap).
    pcm_s16: Vec<i16>,
    encoded: Vec<u8>,
}

// SAFETY: `sbc_t` interior pointers are stable; we only touch the codec
// from a single audio thread.
unsafe impl Send for SbcCodec {}

impl SbcCodec {
    pub fn new(
        host_sample_rate: u32,
        channels: usize,
        max_block_size: usize,
        quality: SbcQuality,
    ) -> Self {
        let mut enc: sbc_sys::sbc_t = unsafe { std::mem::zeroed() };
        let mut dec: sbc_sys::sbc_t = unsafe { std::mem::zeroed() };

        // Use plain `sbc_init` (not `sbc_init_a2dp`). The A2DP variant
        // requires an `a2dp_sbc` config blob; passing null fails with
        // -EINVAL *and* calls `sbc_finish` on the half-initialised sbc_t,
        // freeing `priv` — subsequent encode/decode calls segfault on
        // the freed pointer. Plain init allocates `priv` cleanly; we
        // then write the parameter fields directly.
        unsafe {
            sbc_sys::sbc_init(&mut enc as *mut _, 0);
        }
        enc.frequency = sbc_sys::SBC_FREQ_44100 as u8;
        enc.subbands = sbc_sys::SBC_SB_8 as u8;
        enc.blocks = sbc_sys::SBC_BLK_16 as u8;
        enc.allocation = sbc_sys::SBC_AM_SNR as u8;
        enc.mode = if channels >= 2 {
            sbc_sys::SBC_MODE_JOINT_STEREO as u8
        } else {
            sbc_sys::SBC_MODE_MONO as u8
        };
        enc.endian = sbc_sys::SBC_LE as u8;
        enc.bitpool = quality.bitpool();

        unsafe {
            sbc_sys::sbc_init(&mut dec as *mut _, 0);
        }
        dec.frequency = sbc_sys::SBC_FREQ_44100 as u8;
        dec.subbands = sbc_sys::SBC_SB_8 as u8;
        dec.blocks = sbc_sys::SBC_BLK_16 as u8;
        dec.allocation = sbc_sys::SBC_AM_SNR as u8;
        dec.mode = enc.mode;
        dec.endian = sbc_sys::SBC_LE as u8;
        dec.bitpool = quality.bitpool();

        let mut codec = Self {
            enc,
            dec,
            sample_rate: host_sample_rate,
            channels,
            h2c: None,
            c2h: None,
            h2c_chunk: 0,
            c2h_chunk: 0,
            h2c_in_buf: Vec::new(),
            h2c_out_buf: Vec::new(),
            c2h_in_buf: Vec::new(),
            c2h_out_buf: Vec::new(),
            host_in: Vec::new(),
            sbc_in: Vec::new(),
            sbc_out: Vec::new(),
            host_out: Vec::new(),
            pcm_s16: vec![0i16; SBC_FRAME_SAMPLES * channels.max(2)],
            encoded: vec![0u8; SBC_MAX_FRAME_BYTES],
        };
        codec.setup_resamplers(max_block_size);
        codec
    }

    fn setup_resamplers(&mut self, max_block_size: usize) {
        let ring_cap = max_block_size + (self.sample_rate as usize / 10) + 1;
        let sbc_ring_cap = max_block_size * 2 + SBC_FRAME_SAMPLES * 4;

        if self.sample_rate != SBC_RATE {
            // ~10 ms chunks — clean rubato cadence at every host rate.
            let h2c_chunk = (self.sample_rate as usize / 100).max(1);
            let c2h_chunk = (SBC_RATE as usize / 100).max(1);
            let h2c = Fft::<f32>::new(
                self.sample_rate as usize,
                SBC_RATE as usize,
                h2c_chunk,
                RESAMPLER_SUB_CHUNKS,
                self.channels,
                FixedSync::Input,
            )
            .expect("SBC h2c resampler init");
            let c2h = Fft::<f32>::new(
                SBC_RATE as usize,
                self.sample_rate as usize,
                c2h_chunk,
                RESAMPLER_SUB_CHUNKS,
                self.channels,
                FixedSync::Input,
            )
            .expect("SBC c2h resampler init");
            let h2c_out_max = h2c.output_frames_max();
            let c2h_out_max = c2h.output_frames_max();
            self.h2c_chunk = h2c_chunk;
            self.c2h_chunk = c2h_chunk;
            self.h2c_in_buf = vec![vec![0.0; h2c_chunk]; self.channels];
            self.h2c_out_buf = vec![vec![0.0; h2c_out_max]; self.channels];
            self.c2h_in_buf = vec![vec![0.0; c2h_chunk]; self.channels];
            self.c2h_out_buf = vec![vec![0.0; c2h_out_max]; self.channels];
            self.h2c = Some(h2c);
            self.c2h = Some(c2h);
        }

        self.host_in = (0..self.channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();
        self.sbc_in = (0..self.channels)
            .map(|_| VecDeque::with_capacity(sbc_ring_cap))
            .collect();
        self.sbc_out = (0..self.channels)
            .map(|_| VecDeque::with_capacity(sbc_ring_cap))
            .collect();
        self.host_out = (0..self.channels)
            .map(|_| VecDeque::with_capacity(ring_cap))
            .collect();

        // Pre-fill `host_out` to cover encoder warm-up.
        let prefill = Self::worst_case_latency_at(self.sample_rate, self.channels) as usize;
        for ch in 0..self.channels {
            for _ in 0..prefill {
                self.host_out[ch].push_back(0.0);
            }
        }
    }

    pub fn reset(&mut self) {
        if let Some(r) = &mut self.h2c {
            r.reset();
        }
        if let Some(r) = &mut self.c2h {
            r.reset();
        }
        for ch in 0..self.channels {
            self.host_in[ch].clear();
            self.sbc_in[ch].clear();
            self.sbc_out[ch].clear();
            self.host_out[ch].clear();
        }
        let prefill = Self::worst_case_latency_at(self.sample_rate, self.channels) as usize;
        for ch in 0..self.channels {
            for _ in 0..prefill {
                self.host_out[ch].push_back(0.0);
            }
        }
    }

    /// In-place encode → decode roundtrip on `buffer`.
    pub fn process(&mut self, buffer: &mut Buffer) {
        let n = buffer.samples();
        let block = buffer.as_slice();
        let channels = self.channels.min(block.len());

        for ch in 0..channels {
            self.host_in[ch].reserve(n);
            self.host_in[ch].extend(block[ch][..n].iter().copied());
        }

        self.pump_host_to_sbc();
        self.pump_codec();
        self.pump_sbc_to_host();

        // Drain via `make_contiguous` + `copy_from_slice` — one ring drain
        // per channel rather than `n` pop_fronts.
        for ch in 0..channels {
            let take = self.host_out[ch].len().min(n);
            if take > 0 {
                let head = self.host_out[ch].make_contiguous();
                block[ch][..take].copy_from_slice(&head[..take]);
                self.host_out[ch].drain(..take);
            }
            if take < n {
                block[ch][take..n].fill(0.0);
            }
        }
    }

    fn pump_host_to_sbc(&mut self) {
        match &mut self.h2c {
            None => {
                for ch in 0..self.channels {
                    let n = self.host_in[ch].len();
                    if n > 0 {
                        self.sbc_in[ch].reserve(n);
                        let head = self.host_in[ch].make_contiguous();
                        self.sbc_in[ch].extend(head[..n].iter().copied());
                        self.host_in[ch].clear();
                    }
                }
            }
            Some(r) => {
                let chunk = self.h2c_chunk;
                let out_max = self.h2c_out_buf[0].len();
                while self.host_in[0].len() >= chunk {
                    for ch in 0..self.channels {
                        let head = self.host_in[ch].make_contiguous();
                        self.h2c_in_buf[ch][..chunk].copy_from_slice(&head[..chunk]);
                        self.host_in[ch].drain(..chunk);
                    }
                    let in_adapter =
                        SequentialSliceOfVecs::new(&self.h2c_in_buf, self.channels, chunk)
                            .expect("SBC h2c in-buffer dimensions match");
                    let mut out_adapter =
                        SequentialSliceOfVecs::new_mut(&mut self.h2c_out_buf, self.channels, out_max)
                            .expect("SBC h2c out-buffer dimensions match");
                    let produced = r
                        .process_into_buffer(&in_adapter, &mut out_adapter, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.channels {
                        self.sbc_in[ch].reserve(produced);
                        self.sbc_in[ch]
                            .extend(self.h2c_out_buf[ch][..produced].iter().copied());
                    }
                }
            }
        }
    }

    fn pump_sbc_to_host(&mut self) {
        match &mut self.c2h {
            None => {
                for ch in 0..self.channels {
                    let n = self.sbc_out[ch].len();
                    if n > 0 {
                        self.host_out[ch].reserve(n);
                        let head = self.sbc_out[ch].make_contiguous();
                        self.host_out[ch].extend(head[..n].iter().copied());
                        self.sbc_out[ch].clear();
                    }
                }
            }
            Some(r) => {
                let chunk = self.c2h_chunk;
                let out_max = self.c2h_out_buf[0].len();
                while self.sbc_out[0].len() >= chunk {
                    for ch in 0..self.channels {
                        let head = self.sbc_out[ch].make_contiguous();
                        self.c2h_in_buf[ch][..chunk].copy_from_slice(&head[..chunk]);
                        self.sbc_out[ch].drain(..chunk);
                    }
                    let in_adapter =
                        SequentialSliceOfVecs::new(&self.c2h_in_buf, self.channels, chunk)
                            .expect("SBC c2h in-buffer dimensions match");
                    let mut out_adapter =
                        SequentialSliceOfVecs::new_mut(&mut self.c2h_out_buf, self.channels, out_max)
                            .expect("SBC c2h out-buffer dimensions match");
                    let produced = r
                        .process_into_buffer(&in_adapter, &mut out_adapter, None)
                        .map(|(_, out)| out)
                        .unwrap_or(0);
                    for ch in 0..self.channels {
                        self.host_out[ch].reserve(produced);
                        self.host_out[ch]
                            .extend(self.c2h_out_buf[ch][..produced].iter().copied());
                    }
                }
            }
        }
    }

    /// Drain `sbc_in` one 128-sample frame at a time through enc → dec.
    fn pump_codec(&mut self) {
        while self.sbc_in[0].len() >= SBC_FRAME_SAMPLES {
            crate::processor::pipeline::drain_to_i16_interleaved(
                &mut self.sbc_in,
                SBC_FRAME_SAMPLES,
                &mut self.pcm_s16[..SBC_FRAME_SAMPLES * self.channels],
            );

            let input_bytes = SBC_FRAME_SAMPLES * self.channels * 2;
            let mut written: isize = 0;
            let encoded_len = unsafe {
                sbc_sys::sbc_encode(
                    &mut self.enc as *mut _,
                    self.pcm_s16.as_ptr() as *const c_void,
                    input_bytes,
                    self.encoded.as_mut_ptr() as *mut c_void,
                    self.encoded.len(),
                    &mut written as *mut isize,
                )
            };
            if encoded_len <= 0 || written <= 0 {
                // Emit silence and keep going on encode failure.
                for ch in 0..self.channels {
                    self.sbc_out[ch].reserve(SBC_FRAME_SAMPLES);
                    self.sbc_out[ch]
                        .extend(std::iter::repeat_n(0.0, SBC_FRAME_SAMPLES));
                }
                continue;
            }

            // libsbc's API is asymmetric: encode's `written` is `ssize_t*`,
            // decode's is `size_t*`. We mirror that here.
            let mut decoded: usize = 0;
            let consumed = unsafe {
                sbc_sys::sbc_decode(
                    &mut self.dec as *mut _,
                    self.encoded.as_ptr() as *const c_void,
                    written as usize,
                    self.pcm_s16.as_mut_ptr() as *mut c_void,
                    SBC_FRAME_SAMPLES * self.channels * 2,
                    &mut decoded as *mut usize,
                )
            };
            if consumed <= 0 || decoded == 0 {
                for ch in 0..self.channels {
                    self.sbc_out[ch].reserve(SBC_FRAME_SAMPLES);
                    self.sbc_out[ch]
                        .extend(std::iter::repeat_n(0.0, SBC_FRAME_SAMPLES));
                }
                continue;
            }

            let decoded_samples = decoded / 2 / self.channels.max(1);
            crate::processor::pipeline::push_i16_interleaved(
                &self.pcm_s16[..decoded_samples * self.channels],
                decoded_samples,
                self.channels,
                &mut self.sbc_out,
            );
        }
    }

    /// Conservative latency budget: ~25 ms of resampler delay + 4 SBC
    /// frames (128 × 4 / 44.1 ≈ 11.6 ms) of codec warm-up.
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        let _ = channels;
        let resampler_delay = if host_rate != SBC_RATE {
            (host_rate as f32 * 0.025) as u32
        } else {
            0
        };
        let codec_delay = (host_rate as u64 * 4 * SBC_FRAME_SAMPLES as u64
            / SBC_RATE as u64) as u32;
        resampler_delay + codec_delay
    }
}

impl Drop for SbcCodec {
    fn drop(&mut self) {
        unsafe {
            sbc_sys::sbc_finish(&mut self.enc as *mut _);
            sbc_sys::sbc_finish(&mut self.dec as *mut _);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only planar f32 driver.
    impl SbcCodec {
        pub fn process_planar(&mut self, input: &[Vec<f32>], output: &mut [Vec<f32>]) {
            let n = input[0].len();
            for ch in 0..self.channels {
                for s in 0..n {
                    self.host_in[ch].push_back(input[ch][s]);
                }
            }
            self.pump_host_to_sbc();
            self.pump_codec();
            self.pump_sbc_to_host();
            for ch in 0..self.channels {
                for s in 0..n {
                    output[ch][s] = self.host_out[ch].pop_front().unwrap_or(0.0);
                }
            }
        }
    }

    #[test]
    fn sbc_roundtrip_emits_audio_at_every_host_rate() {
        for &host_rate in &[44_100u32, 48_000, 96_000] {
            for &quality in &[SbcQuality::Low, SbcQuality::High] {
                let mut codec = SbcCodec::new(host_rate, 2, 256, quality);
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
                    "SBC at {} Hz / {:?} produced near-silent output ({:.3})",
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
            let l = SbcCodec::worst_case_latency_at(rate, 2);
            assert!(l > 0, "SBC worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut codec = SbcCodec::new(48_000, 2, 256, SbcQuality::High);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            codec.process_planar(&inp, &mut out);
        }
        codec.reset();
    }
}
