//! Real-time Opus encode/decode pipeline.
//!
//! All work happens at 48 kHz internally — libopus only accepts
//! {8, 12, 16, 24, 48} kHz. The shared [`ResampledPipeline`] handles
//! host↔48 kHz resampling and ring buffers.

use crate::processor::pipeline::ResampledPipeline;
use nih_plug::prelude::*;
use opus::{Application, Channels, Decoder, Encoder};

const OPUS_RATE: u32 = 48_000;
/// Opus frame size at `OPUS_RATE` (20 ms).
const OPUS_FRAME_SIZE: usize = 960;

/// Per-block processing mode. Derived from `CodecSpec` in `dispatch_buffer`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OpusMode {
    /// Pass-through, still routed through the ring so latency matches the
    /// real Opus path.
    Passthrough,
    Opus { bitrate_kbps: u32 },
}

pub struct OpusProcessor {
    pipeline: ResampledPipeline,

    encoder: Option<Encoder>,
    decoder: Option<Decoder>,
    current_bitrate_bps: i32,

    /// Interleaved 48 kHz buffer; encoder input and decoder output share it
    /// (the two never overlap in time).
    interleaved: Vec<f32>,
    packet: Vec<u8>,
}

impl OpusProcessor {
    pub fn new() -> Self {
        Self {
            pipeline: ResampledPipeline::new(),
            encoder: None,
            decoder: None,
            current_bitrate_bps: -1,
            interleaved: Vec::new(),
            packet: Vec::new(),
        }
    }

    pub fn initialize(
        &mut self,
        host_sample_rate: u32,
        channels: usize,
        max_block_size: usize,
    ) {
        if !matches!(channels, 1 | 2) {
            nih_log!(
                "OpusProcessor: only mono and stereo are supported, got {} channels.",
                channels
            );
            return;
        }
        let opus_channels = if channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        };

        match (
            Encoder::new(OPUS_RATE, opus_channels, Application::Audio),
            Decoder::new(OPUS_RATE, opus_channels),
        ) {
            (Ok(enc), Ok(dec)) => {
                self.encoder = Some(enc);
                self.decoder = Some(dec);
            }
            _ => {
                nih_log!("OpusProcessor: libopus encoder/decoder creation failed.");
                return;
            }
        }

        if !self
            .pipeline
            .setup(host_sample_rate, channels, max_block_size, OPUS_RATE, OPUS_RATE)
        {
            nih_log!("OpusProcessor: pipeline resampler setup failed.");
            return;
        }

        self.interleaved = vec![0.0; OPUS_FRAME_SIZE * channels];
        self.packet = vec![0; 4000];
        self.current_bitrate_bps = -1;

        let latency = self.compute_natural_latency();
        self.pipeline.set_latency(latency);
    }

    /// Pipeline delay in host samples = h2i resampler chunk + h2i delay +
    /// encoder accumulation + libopus algorithmic delay + i2h chunk + i2h
    /// delay. Same shape every codec uses, driven off the pipeline's
    /// reported `latency_pair`s.
    fn compute_natural_latency(&self) -> u32 {
        let host_rate = self.pipeline.host_rate as u64;
        let internal_rate = OPUS_RATE as u64;
        let to_host = |internal: u32| (internal as u64 * host_rate / internal_rate) as u32;

        let (h2i_chunk_host, h2i_delay_internal) = self.pipeline.h2i_latency_pair();
        let (_, i2h_delay_host) = self.pipeline.i2h_latency_pair();

        // With no resampler `internal_input` must accumulate a whole frame
        // before encode fires; with one, h2i delivers a full frame per call.
        let opus_accum_internal = if self.pipeline.host_rate == OPUS_RATE {
            OPUS_FRAME_SIZE as u32
        } else {
            0
        };
        // libopus's reported algorithmic delay at 48 kHz.
        let opus_algo_internal = 312;
        let i2h_chunk_internal = if self.pipeline.host_rate == OPUS_RATE {
            0
        } else {
            OPUS_FRAME_SIZE as u32
        };

        let internal_total =
            h2i_delay_internal + opus_accum_internal + opus_algo_internal + i2h_chunk_internal;
        h2i_chunk_host + to_host(internal_total) + i2h_delay_host
    }

    pub fn pad_output_to(&mut self, target: u32) {
        self.pipeline.pad_output_to(target);
    }

    /// Static estimate for the lazy-init path — see
    /// [`ResampledPipeline::estimate_latency`]. Conservative budget of
    /// 1 frame + 312-sample algorithmic delay.
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        const OPUS_ROUNDTRIP: u32 = OPUS_FRAME_SIZE as u32 + 312;
        ResampledPipeline::estimate_latency(
            host_rate,
            channels,
            OPUS_RATE,
            OPUS_RATE,
            OPUS_ROUNDTRIP,
        )
    }

    pub fn reset(&mut self) {
        if !self.pipeline.ready {
            return;
        }
        if let (Some(enc), Some(dec)) = (&mut self.encoder, &mut self.decoder) {
            let _ = enc.reset_state();
            let _ = dec.reset_state();
        }
        self.pipeline.reset();
    }

    pub fn latency_samples(&self) -> u32 {
        if self.pipeline.ready {
            self.pipeline.latency_host_samples
        } else {
            0
        }
    }

    pub fn process(&mut self, buffer: &mut Buffer, mode: OpusMode) {
        if !self.pipeline.ready {
            return;
        }

        if let OpusMode::Opus { bitrate_kbps } = mode {
            self.maybe_update_bitrate(bitrate_kbps);
        }

        let n_samples = buffer.samples();
        let block = buffer.as_slice();

        self.pipeline.push_host_block(block, n_samples);
        self.pipeline.pump_host_to_internal();
        self.pump_codec(mode);
        self.pipeline.pump_internal_to_host();
        self.pipeline.drain_host_block(block, n_samples);
    }

    fn pump_codec(&mut self, mode: OpusMode) {
        let channels = self.pipeline.channels;
        while self.pipeline.internal_input[0].len() >= OPUS_FRAME_SIZE {
            // `make_contiguous` per channel so the interleave reads a real
            // slice instead of paying per-sample `pop_front`.
            for ch in 0..channels {
                let head = self.pipeline.internal_input[ch].make_contiguous();
                let frame = &head[..OPUS_FRAME_SIZE];
                for s in 0..OPUS_FRAME_SIZE {
                    self.interleaved[s * channels + ch] = frame[s];
                }
                self.pipeline.internal_input[ch].drain(..OPUS_FRAME_SIZE);
            }

            match mode {
                OpusMode::Passthrough => {}
                OpusMode::Opus { .. } => {
                    let enc = self.encoder.as_mut().expect("encoder set when ready");
                    let dec = self.decoder.as_mut().expect("decoder set when ready");
                    let packet_len =
                        match enc.encode_float(&self.interleaved, &mut self.packet) {
                            Ok(n) => n,
                            Err(_) => {
                                // Emit silence on transient encode error.
                                self.interleaved.fill(0.0);
                                0
                            }
                        };
                    if packet_len > 0 {
                        let _ = dec.decode_float(
                            &self.packet[..packet_len],
                            &mut self.interleaved,
                            false,
                        );
                    }
                }
            }

            // Deinterleave back into internal_output via bulk `extend` so
            // the ring grows once.
            for ch in 0..channels {
                self.pipeline.internal_output[ch].reserve(OPUS_FRAME_SIZE);
                self.pipeline.internal_output[ch].extend(
                    (0..OPUS_FRAME_SIZE).map(|s| self.interleaved[s * channels + ch]),
                );
            }
        }
    }

    fn maybe_update_bitrate(&mut self, bitrate_kbps: u32) {
        let target_bps = (bitrate_kbps as i32) * 1000;
        if target_bps == self.current_bitrate_bps {
            return;
        }
        if let Some(enc) = &mut self.encoder {
            if enc.set_bitrate(opus::Bitrate::Bits(target_bps)).is_ok() {
                self.current_bitrate_bps = target_bps;
            }
        }
    }
}

impl OpusProcessor {
    /// Test-only `process()` without the nih-plug `Buffer` wrapping.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: OpusMode,
    ) {
        if !self.pipeline.ready {
            return;
        }
        let channels = self.pipeline.channels;
        let n = input[0].len();
        for ch in 0..channels {
            for s in 0..n {
                self.pipeline.host_input[ch].push_back(input[ch][s]);
            }
        }
        if let OpusMode::Opus { bitrate_kbps } = mode {
            self.maybe_update_bitrate(bitrate_kbps);
        }
        self.pipeline.pump_host_to_internal();
        self.pump_codec(mode);
        self.pipeline.pump_internal_to_host();
        for ch in 0..channels {
            for s in 0..n {
                output[ch][s] = self.pipeline.host_output[ch].pop_front().unwrap_or(0.0);
            }
        }
    }
}

impl Default for OpusProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sweep the user-relevant (host_rate × bitrate) matrix and confirm
    /// audible output past the warm-up window.
    #[test]
    fn emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000] {
            for &bitrate_kbps in &[48u32, 128, 256] {
                let mut proc = OpusProcessor::new();
                let block_size = 256;
                proc.initialize(host_rate, 2, block_size);
                assert!(
                    proc.pipeline.ready,
                    "initialize failed for host_rate={host_rate} Hz"
                );

                let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
                    host_rate,
                    block_size,
                    3.0,
                    0.5,
                    440.0,
                    0.5,
                    |inp, out| {
                        proc.process_planar(inp, out, OpusMode::Opus { bitrate_kbps });
                    },
                );
                assert!(
                    peak > 0.05,
                    "Opus at {host_rate} Hz / {bitrate_kbps} kbps: \
                     peak {peak:.3} below threshold (latency={} samples)",
                    proc.latency_samples()
                );
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = OpusProcessor::new();
        proc.initialize(48_000, 2, 256);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, OpusMode::Passthrough),
        );
        assert!(peak > 0.05, "Opus passthrough silent: {peak:.3}");
    }

    #[test]
    fn reset_does_not_break_subsequent_processing() {
        let mut proc = OpusProcessor::new();
        proc.initialize(48_000, 2, 256);
        let mut inp: Vec<Vec<f32>> = vec![vec![0.5; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(&inp, &mut out, OpusMode::Opus { bitrate_kbps: 128 });
        }
        proc.reset();
        assert!(proc.pipeline.ready);
        assert!(proc.latency_samples() > 0);
        for s in 0..256 {
            inp[0][s] = (s as f32 * 0.01).sin() * 0.3;
            inp[1][s] = inp[0][s];
        }
        proc.process_planar(&inp, &mut out, OpusMode::Opus { bitrate_kbps: 128 });
    }

    #[test]
    fn pad_output_to_grows_latency() {
        let mut proc = OpusProcessor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.latency_samples();
        proc.pad_output_to(before + 1_000);
        assert_eq!(proc.latency_samples(), before + 1_000);
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 88_200, 96_000] {
            let l = OpusProcessor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = OpusProcessor::default();
        assert!(!proc.pipeline.ready);
        assert_eq!(proc.latency_samples(), 0);
    }

    /// Exercises the encoder rebuild path on bitrate changes.
    #[test]
    fn bitrate_change_rebuilds_codec_without_panic() {
        let mut proc = OpusProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for kbps in [64u32, 128, 256, 64] {
            proc.process_planar(&inp, &mut out, OpusMode::Opus { bitrate_kbps: kbps });
        }
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = OpusProcessor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.pipeline.ready);
    }
}
