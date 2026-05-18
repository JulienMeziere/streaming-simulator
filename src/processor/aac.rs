//! Real-time AAC-LC encode → decode via Fraunhofer FDK-AAC. Gated on the
//! `fdk-aac` feature; see `docs/licensing.md`.
//!
//! Always runs at 44.1 kHz internally (matches Spotify / YouTube Music
//! mobile / Apple Music). Encoder ↔ decoder talk in ADTS-framed packets
//! (the `fdk-aac` decoder's only available transport). i16 interleaved
//! PCM internally. Bitrate changes rebuild the pair.

use crate::processor::pipeline::{self, ResampledPipeline};
use fdk_aac::dec::{Decoder, DecoderError, Transport as DecTransport};
use fdk_aac::enc::{
    AudioObjectType, BitRate, ChannelMode, Encoder, EncoderParams, Transport as EncTransport,
};
use nih_plug::prelude::*;

const AAC_RATE: u32 = 44_100;
const AAC_FRAME_SIZE: usize = 1024;
/// First decoded frame is MDCT edge-windowed garbage; discard it.
const PREROLL_SAMPLES: usize = AAC_FRAME_SIZE;
/// Generous upper bound for one ADTS packet at 320 kbps stereo.
const MAX_PACKET_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq)]
// Passthrough reserved for future "follow the previous codec on Bypass".
#[allow(dead_code)]
pub enum AacMode {
    Passthrough,
    /// `mono = true` forces one-channel encode regardless of host channels
    /// — used for TikTok / Instagram mono-fallback tiers.
    AacLc { bitrate_kbps: u32, mono: bool },
}

pub struct AacProcessor {
    pipeline: ResampledPipeline,

    /// `ensure_codec` keys on (bitrate, mono); switching either rebuilds.
    current_bitrate_kbps: i32,
    current_mono: bool,
    codec: Option<AacCodec>,

    pcm_i16: Vec<i16>,
    packet: Vec<u8>,

    samples_to_discard: usize,
}

struct AacCodec {
    encoder: Encoder,
    decoder: Decoder,
}

impl AacCodec {
    fn new(channels: usize, bitrate_bps: u32) -> Option<Self> {
        let params = EncoderParams {
            bit_rate: BitRate::Cbr(bitrate_bps),
            sample_rate: AAC_RATE,
            transport: EncTransport::Adts,
            channels: if channels == 1 {
                ChannelMode::Mono
            } else {
                ChannelMode::Stereo
            },
            audio_object_type: AudioObjectType::Mpeg4LowComplexity,
        };
        let encoder = Encoder::new(params).ok()?;
        let decoder = Decoder::new(DecTransport::Adts);
        Some(Self { encoder, decoder })
    }
}

impl AacProcessor {
    pub fn new() -> Self {
        Self {
            pipeline: ResampledPipeline::new(),
            current_bitrate_kbps: -1,
            current_mono: false,
            codec: None,
            pcm_i16: Vec::new(),
            packet: Vec::new(),
            samples_to_discard: 0,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        if !matches!(channels, 1 | 2) {
            nih_log!(
                "AacProcessor: only mono and stereo are supported, got {} channels.",
                channels
            );
            return;
        }
        self.current_bitrate_kbps = -1;
        self.current_mono = false;
        self.codec = None;

        if !self
            .pipeline
            .setup(sample_rate, channels, max_block_size, AAC_RATE, AAC_RATE)
        {
            nih_log!("AacProcessor: pipeline resampler setup failed.");
            return;
        }

        self.pcm_i16 = vec![0i16; AAC_FRAME_SIZE * channels];
        self.packet = vec![0u8; MAX_PACKET_BYTES];

        let latency = self.compute_natural_latency();
        self.pipeline.set_latency(latency);
    }

    /// AAC-LC: encoder needs ~2 frames in for packet #1 (lookahead), decoder
    /// needs 2 packets to start, first frame is MDCT warm-up garbage —
    /// budget 4 × frame size from input to first usable output.
    fn compute_natural_latency(&self) -> u32 {
        let host_rate = self.pipeline.host_rate as u64;
        let internal_rate = AAC_RATE as u64;
        let to_host = |internal: u32| (internal as u64 * host_rate / internal_rate) as u32;

        let (h2i_chunk_host, h2i_delay_internal) = self.pipeline.h2i_latency_pair();
        let (i2h_chunk_at_host, i2h_delay_at_host) = self.pipeline.i2h_latency_pair();
        const AAC_ROUNDTRIP_INTERNAL: u32 = 4 * (AAC_FRAME_SIZE as u32);

        let internal_total = h2i_delay_internal + AAC_ROUNDTRIP_INTERNAL;
        h2i_chunk_host + to_host(internal_total) + i2h_chunk_at_host + i2h_delay_at_host
    }

    pub fn pad_output_to(&mut self, target: u32) {
        self.pipeline.pad_output_to(target);
    }

    /// Static estimate for the lazy-init path. See
    /// [`ResampledPipeline::estimate_latency`].
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        const AAC_ROUNDTRIP_INTERNAL: u32 = 4 * (AAC_FRAME_SIZE as u32);
        ResampledPipeline::estimate_latency(
            host_rate,
            channels,
            AAC_RATE,
            AAC_RATE,
            AAC_ROUNDTRIP_INTERNAL,
        )
    }

    pub fn reset(&mut self) {
        if !self.pipeline.ready {
            return;
        }
        self.codec = None;
        self.current_bitrate_kbps = -1;
        self.current_mono = false;
        self.pipeline.reset();
    }

    pub fn latency_samples(&self) -> u32 {
        if self.pipeline.ready {
            self.pipeline.latency_host_samples
        } else {
            0
        }
    }

    pub fn process(&mut self, buffer: &mut Buffer, mode: AacMode) {
        if !self.pipeline.ready {
            return;
        }
        let n_samples = buffer.samples();
        let block = buffer.as_slice();
        self.pipeline.push_host_block(block, n_samples);
        self.pipeline.pump_host_to_internal();

        match mode {
            AacMode::Passthrough => {
                let channels = self.pipeline.channels;
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            AacMode::AacLc { bitrate_kbps, mono } => {
                self.ensure_codec(bitrate_kbps, mono);
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

    fn ensure_codec(&mut self, bitrate_kbps: u32, mono: bool) {
        let target_kbps = bitrate_kbps as i32;
        if self.current_bitrate_kbps == target_kbps
            && self.current_mono == mono
            && self.codec.is_some()
        {
            return;
        }
        self.codec = None;
        // pump_codec sums L+R → mono before encoding and duplicates back
        // to L = R after decoding.
        let codec_channels = if mono { 1 } else { self.pipeline.channels };
        self.codec = AacCodec::new(codec_channels, bitrate_kbps * 1000);
        if self.codec.is_some() {
            self.current_bitrate_kbps = target_kbps;
            self.current_mono = mono;
            self.samples_to_discard = PREROLL_SAMPLES;
        } else {
            self.current_bitrate_kbps = -1;
            #[cfg(debug_assertions)]
            nih_log!(
                "AacProcessor: failed to build encoder/decoder for {} kbps at {} Hz, {} channel(s).",
                bitrate_kbps,
                AAC_RATE,
                codec_channels,
            );
        }
    }

    /// Drain `internal_input` through the codec one frame at a time.
    /// Mono mode sums L+R before encoding and duplicates the mono decode
    /// back to L = R, reproducing the real artifact spectrum of mono AAC
    /// (full bit budget per channel) instead of `stereo @ N kbps where L=R`.
    fn pump_codec(&mut self) {
        let codec = match self.codec.as_mut() {
            Some(c) => c,
            None => return,
        };
        let host_channels = self.pipeline.channels;
        let codec_channels = if self.current_mono { 1 } else { host_channels };
        let frame_samples = AAC_FRAME_SIZE * codec_channels;

        while self.pipeline.internal_input[0].len() >= AAC_FRAME_SIZE {
            // Drain one frame into pcm_i16. `make_contiguous` per channel +
            // one `drain(..frame)` instead of `frame_size` `pop_front`s.
            if self.current_mono {
                // Stack scratch — fixed-size const so no heap.
                let mut sum = [0.0f32; AAC_FRAME_SIZE];
                for ch in 0..host_channels {
                    let head = self.pipeline.internal_input[ch].make_contiguous();
                    let frame = &head[..AAC_FRAME_SIZE];
                    if ch == 0 {
                        sum.copy_from_slice(frame);
                    } else {
                        for (acc, &f) in sum.iter_mut().zip(frame.iter()) {
                            *acc += f;
                        }
                    }
                    self.pipeline.internal_input[ch].drain(..AAC_FRAME_SIZE);
                }
                let inv_ch = 1.0 / host_channels as f32;
                for s in 0..AAC_FRAME_SIZE {
                    let mono = (sum[s] * inv_ch).clamp(-1.0, 1.0);
                    self.pcm_i16[s] = (mono * 32767.0) as i16;
                }
            } else {
                pipeline::drain_to_i16_interleaved(
                    &mut self.pipeline.internal_input,
                    AAC_FRAME_SIZE,
                    &mut self.pcm_i16[..AAC_FRAME_SIZE * host_channels],
                );
            }

            // fdk-aac is streaming — may need several `encode` calls to
            // consume one frame.
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
                            // Bulk push per channel: one `reserve` + strided
                            // iter doing i16→f32. Mono→stereo duplicates ch 0.
                            let kept = n_decoded - skip;
                            if kept > 0 {
                                if dec_channels == 1 && host_channels > 1 {
                                    let mono_slice = &self.pcm_i16[skip..n_decoded];
                                    for ch in 0..host_channels {
                                        let dst = &mut self.pipeline.internal_output[ch];
                                        dst.reserve(kept);
                                        dst.extend(
                                            mono_slice.iter().map(|&v| v as f32 / 32768.0),
                                        );
                                    }
                                } else {
                                    let used = host_channels.min(dec_channels);
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
                        }
                        Err(e) if e == DecoderError::NOT_ENOUGH_BITS => break,
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

impl AacProcessor {
    /// Test-only `process()` without the nih-plug `Buffer` wrapping.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: AacMode,
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
            AacMode::Passthrough => {
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            AacMode::AacLc { bitrate_kbps, mono } => {
                self.ensure_codec(bitrate_kbps, mono);
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

impl Default for AacProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000] {
            for &bitrate_kbps in &[48u32, 128, 256, 320] {
                run_aac_smoke_test(host_rate, bitrate_kbps, false);
            }
        }
    }

    /// Mono path (TikTok / Instagram poor-conn): stereo → mono sum →
    /// 1-channel encode → duplicate back to L=R.
    #[test]
    fn mono_emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000] {
            for &bitrate_kbps in &[48u32, 64, 128] {
                run_aac_smoke_test(host_rate, bitrate_kbps, true);
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 2, 256);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, AacMode::Passthrough),
        );
        assert!(peak > 0.05, "AAC passthrough silent: {peak:.3}");
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(
                &inp,
                &mut out,
                AacMode::AacLc { bitrate_kbps: 128, mono: false },
            );
        }
        proc.reset();
        assert!(proc.pipeline.ready);
    }

    #[test]
    fn pad_output_to_grows_latency() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.latency_samples();
        proc.pad_output_to(before + 1_000);
        assert_eq!(proc.latency_samples(), before + 1_000);
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 88_200, 96_000] {
            let l = AacProcessor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = AacProcessor::default();
        assert!(!proc.pipeline.ready);
    }

    #[test]
    fn bitrate_change_rebuilds_codec_without_panic() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for kbps in [128u32, 256, 64, 256] {
            proc.process_planar(
                &inp,
                &mut out,
                AacMode::AacLc { bitrate_kbps: kbps, mono: false },
            );
        }
    }

    /// Stereo → mono → stereo transitions force `ensure_codec` to rebuild.
    #[test]
    fn mono_to_stereo_transition_rebuilds_codec() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        proc.process_planar(
            &inp,
            &mut out,
            AacMode::AacLc { bitrate_kbps: 128, mono: false },
        );
        proc.process_planar(
            &inp,
            &mut out,
            AacMode::AacLc { bitrate_kbps: 128, mono: true },
        );
        proc.process_planar(
            &inp,
            &mut out,
            AacMode::AacLc { bitrate_kbps: 128, mono: false },
        );
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = AacProcessor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.pipeline.ready);
    }

    fn run_aac_smoke_test(host_rate: u32, bitrate_kbps: u32, mono: bool) {
        let mut proc = AacProcessor::new();
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
                proc.process_planar(inp, out, AacMode::AacLc { bitrate_kbps, mono });
            },
        );
        assert!(
            peak > 0.05,
            "AAC at {host_rate} Hz / {bitrate_kbps} kbps / mono={mono}: \
             peak {peak:.3} below threshold (latency={} samples)",
            proc.latency_samples()
        );
    }
}
