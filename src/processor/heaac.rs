//! Real-time HE-AAC v1 encode → decode via Fraunhofer FDK-AAC. Gated on
//! the `fdk-aac` feature.
//!
//! HE-AAC v1 = AAC-LC core + SBR (no Parametric Stereo). Stereo is
//! encoded literally inside the AAC-LC core, so the image survives much
//! better than under v2's PS angle-coupling. Used by Apple Music's
//! "High Efficiency" tier at 64 kbps stereo. Architecture mirrors
//! [`super::aac`] — only the encoder profile and frame size differ.

use crate::processor::pipeline::{self, ResampledPipeline};
use fdk_aac::dec::{Decoder, DecoderError, Transport as DecTransport};
use fdk_aac::enc::{
    AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport as EncTransport,
};
use nih_plug::prelude::*;

const HEAAC_RATE: u32 = 44_100;
/// 1024 LC core × 2 SBR upsample.
const HEAAC_FRAME_SIZE: usize = 2048;
/// One frame covers the SBR analysis-filterbank warm-up.
const PREROLL_SAMPLES: usize = HEAAC_FRAME_SIZE;
const MAX_PACKET_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum HeAacV1Mode {
    Passthrough,
    HeAacV1 { bitrate_kbps: u32 },
}

pub struct HeAacV1Processor {
    pipeline: ResampledPipeline,

    current_bitrate_kbps: i32,
    codec: Option<HeAacV1Codec>,

    pcm_i16: Vec<i16>,
    packet: Vec<u8>,

    samples_to_discard: usize,
}

struct HeAacV1Codec {
    encoder: Encoder,
    decoder: Decoder,
}

impl HeAacV1Codec {
    fn new(channels: usize, bitrate_bps: u32) -> Option<Self> {
        let params = EncoderParams {
            bit_rate: BitRate::Cbr(bitrate_bps),
            sample_rate: HEAAC_RATE,
            transport: EncTransport::Adts,
            channels: if channels == 1 {
                ChannelMode::Mono
            } else {
                ChannelMode::Stereo
            },
            audio_object_type: AudioObjectType::Mpeg4HeAac,
        };
        let encoder = Encoder::new(params).ok()?;
        let decoder = Decoder::new(DecTransport::Adts);
        Some(Self { encoder, decoder })
    }
}

impl HeAacV1Processor {
    pub fn new() -> Self {
        Self {
            pipeline: ResampledPipeline::new(),
            current_bitrate_kbps: -1,
            codec: None,
            pcm_i16: Vec::new(),
            packet: Vec::new(),
            samples_to_discard: 0,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        if !matches!(channels, 1 | 2) {
            nih_log!(
                "HeAacV1Processor: only mono and stereo are supported, got {} channels.",
                channels
            );
            return;
        }
        self.current_bitrate_kbps = -1;
        self.codec = None;

        if !self.pipeline.setup(
            sample_rate,
            channels,
            max_block_size,
            HEAAC_RATE,
            HEAAC_RATE,
        ) {
            nih_log!("HeAacV1Processor: pipeline resampler setup failed.");
            return;
        }

        self.pcm_i16 = vec![0i16; HEAAC_FRAME_SIZE * channels];
        self.packet = vec![0u8; MAX_PACKET_BYTES];

        let latency = self.compute_natural_latency();
        self.pipeline.set_latency(latency);
    }

    /// HE-AAC v1: encoder needs ~3 frames in for packet #1 (SBR adds
    /// lookahead over LC), decoder needs 2 to start, first frame is
    /// MDCT/SBR warm-up garbage — budget 5 × frame size.
    fn compute_natural_latency(&self) -> u32 {
        let host_rate = self.pipeline.host_rate as u64;
        let internal_rate = HEAAC_RATE as u64;
        let to_host = |internal: u32| (internal as u64 * host_rate / internal_rate) as u32;

        let (h2i_chunk_host, h2i_delay_internal) = self.pipeline.h2i_latency_pair();
        let (i2h_chunk_at_host, i2h_delay_at_host) = self.pipeline.i2h_latency_pair();
        const HEAAC_ROUNDTRIP_INTERNAL: u32 = 5 * (HEAAC_FRAME_SIZE as u32);

        let internal_total = h2i_delay_internal + HEAAC_ROUNDTRIP_INTERNAL;
        h2i_chunk_host + to_host(internal_total) + i2h_chunk_at_host + i2h_delay_at_host
    }

    pub fn pad_output_to(&mut self, target: u32) {
        self.pipeline.pad_output_to(target);
    }

    /// Static estimate for the lazy-init path. See
    /// [`ResampledPipeline::estimate_latency`] for details.
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        const HEAAC_ROUNDTRIP_INTERNAL: u32 = 5 * (HEAAC_FRAME_SIZE as u32);
        ResampledPipeline::estimate_latency(
            host_rate,
            channels,
            HEAAC_RATE,
            HEAAC_RATE,
            HEAAC_ROUNDTRIP_INTERNAL,
        )
    }

    pub fn reset(&mut self) {
        if !self.pipeline.ready {
            return;
        }
        self.codec = None;
        self.current_bitrate_kbps = -1;
        self.pipeline.reset();
    }

    pub fn latency_samples(&self) -> u32 {
        if self.pipeline.ready {
            self.pipeline.latency_host_samples
        } else {
            0
        }
    }

    pub fn process(&mut self, buffer: &mut Buffer, mode: HeAacV1Mode) {
        if !self.pipeline.ready {
            return;
        }
        let n_samples = buffer.samples();
        let block = buffer.as_slice();
        self.pipeline.push_host_block(block, n_samples);
        self.pipeline.pump_host_to_internal();

        match mode {
            HeAacV1Mode::Passthrough => {
                let channels = self.pipeline.channels;
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            HeAacV1Mode::HeAacV1 { bitrate_kbps } => {
                self.ensure_codec(bitrate_kbps);
                if self.codec.is_some() {
                    self.pump_codec();
                } else {
                    let channels = self.pipeline.channels;
                    for ch in 0..channels {
                        while self.pipeline.internal_input[ch].pop_front().is_some() {
                            self.pipeline.internal_output[ch].push_back(0.0);
                        }
                    }
                }
            }
        }

        self.pipeline.pump_internal_to_host();
        self.pipeline.drain_host_block(block, n_samples);
    }

    fn ensure_codec(&mut self, bitrate_kbps: u32) {
        let target_kbps = bitrate_kbps as i32;
        if self.current_bitrate_kbps == target_kbps && self.codec.is_some() {
            return;
        }
        self.codec = None;
        self.codec = HeAacV1Codec::new(self.pipeline.channels, bitrate_kbps * 1000);
        if self.codec.is_some() {
            self.current_bitrate_kbps = target_kbps;
            self.samples_to_discard = PREROLL_SAMPLES;
        } else {
            self.current_bitrate_kbps = -1;
            #[cfg(debug_assertions)]
            nih_log!(
                "HeAacV1Processor: failed to build encoder/decoder for {} kbps at {} Hz, {} channels.",
                bitrate_kbps,
                HEAAC_RATE,
                self.pipeline.channels,
            );
        }
    }

    fn pump_codec(&mut self) {
        let codec = match self.codec.as_mut() {
            Some(c) => c,
            None => return,
        };
        let channels = self.pipeline.channels;
        let frame_samples = HEAAC_FRAME_SIZE * channels;

        while self.pipeline.internal_input[0].len() >= HEAAC_FRAME_SIZE {
            pipeline::drain_to_i16_interleaved(
                &mut self.pipeline.internal_input,
                HEAAC_FRAME_SIZE,
                &mut self.pcm_i16[..HEAAC_FRAME_SIZE * channels],
            );

            let mut consumed_samples = 0usize;
            while consumed_samples < frame_samples {
                let remaining = &self.pcm_i16[consumed_samples..];
                let info = match codec.encoder.encode(remaining, &mut self.packet) {
                    Ok(i) => i,
                    Err(_) => break,
                };
                consumed_samples += info.input_consumed;
                if info.output_size == 0 {
                    if info.input_consumed == 0 {
                        break;
                    }
                    continue;
                }

                let pkt_len = info.output_size;
                let _ = codec.decoder.fill(&self.packet[..pkt_len]);
                loop {
                    match codec.decoder.decode_frame(&mut self.pcm_i16) {
                        Ok(()) => {
                            let info = codec.decoder.stream_info();
                            let n_decoded = info.frameSize as usize;
                            let dec_channels = info.numChannels as usize;
                            let skip = self.samples_to_discard.min(n_decoded);
                            self.samples_to_discard -= skip;
                            // Per-channel bulk push: `reserve` + strided
                            // i16→f32 extend, autovec friendly.
                            let kept = n_decoded - skip;
                            if kept > 0 {
                                let used = channels.min(dec_channels);
                                let frames =
                                    &self.pcm_i16[skip * dec_channels..n_decoded * dec_channels];
                                for ch in 0..used {
                                    let dst = &mut self.pipeline.internal_output[ch];
                                    dst.reserve(kept);
                                    dst.extend(
                                        frames
                                            .chunks_exact(dec_channels)
                                            .map(|frame| frame[ch] as f32 / 32768.0),
                                    );
                                }
                            }
                        }
                        Err(e) if e == DecoderError::NOT_ENOUGH_BITS => break,
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

impl HeAacV1Processor {
    /// Test-only entry point. See `OpusProcessor::process_planar`.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: HeAacV1Mode,
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
        self.pipeline.pump_host_to_internal();
        match mode {
            HeAacV1Mode::Passthrough => {
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            HeAacV1Mode::HeAacV1 { bitrate_kbps } => {
                self.ensure_codec(bitrate_kbps);
                if self.codec.is_some() {
                    self.pump_codec();
                } else {
                    for ch in 0..channels {
                        while self.pipeline.internal_input[ch].pop_front().is_some() {
                            self.pipeline.internal_output[ch].push_back(0.0);
                        }
                    }
                }
            }
        }
        self.pipeline.pump_internal_to_host();
        for ch in 0..channels {
            for s in 0..n {
                output[ch][s] = self.pipeline.host_output[ch].pop_front().unwrap_or(0.0);
            }
        }
    }
}

impl Default for HeAacV1Processor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HE-AAC v1 has the longest warm-up of the AAC family (SBR adds ~1
    /// frame on top of LC). Give it 5 seconds and a relaxed 25%
    /// nonzero threshold to allow for that pipeline depth.
    #[test]
    fn emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000] {
            for &bitrate_kbps in &[64u32] {
                let mut proc = HeAacV1Processor::new();
                let block_size = 256;
                proc.initialize(host_rate, 2, block_size);
                assert!(
                    proc.pipeline.ready,
                    "initialize failed for host_rate={host_rate} Hz"
                );

                let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
                    host_rate,
                    block_size,
                    5.0,
                    1.0,
                    440.0,
                    0.5,
                    |inp, out| {
                        proc.process_planar(
                            inp,
                            out,
                            HeAacV1Mode::HeAacV1 { bitrate_kbps },
                        );
                    },
                );
                assert!(
                    peak > 0.05,
                    "HE-AAC v1 at {host_rate} Hz / {bitrate_kbps} kbps: \
                     peak {peak:.3} below threshold (latency={} samples)",
                    proc.latency_samples()
                );
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = HeAacV1Processor::new();
        proc.initialize(48_000, 2, 256);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, HeAacV1Mode::Passthrough),
        );
        assert!(peak > 0.05, "HE-AAC v1 passthrough silent: {peak:.3}");
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut proc = HeAacV1Processor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(&inp, &mut out, HeAacV1Mode::HeAacV1 { bitrate_kbps: 64 });
        }
        proc.reset();
        assert!(proc.pipeline.ready);
    }

    #[test]
    fn pad_output_to_grows_latency() {
        let mut proc = HeAacV1Processor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.latency_samples();
        proc.pad_output_to(before + 1_000);
        assert_eq!(proc.latency_samples(), before + 1_000);
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 88_200, 96_000] {
            let l = HeAacV1Processor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = HeAacV1Processor::default();
        assert!(!proc.pipeline.ready);
    }

    #[test]
    fn bitrate_change_rebuilds_codec_without_panic() {
        let mut proc = HeAacV1Processor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for kbps in [32u32, 48, 64, 32] {
            proc.process_planar(
                &inp,
                &mut out,
                HeAacV1Mode::HeAacV1 { bitrate_kbps: kbps },
            );
        }
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = HeAacV1Processor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.pipeline.ready);
    }
}
