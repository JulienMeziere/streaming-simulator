//! Real-time Vorbis encode → decode pipeline.
//!
//! libvorbis through `aotuv_lancer_vorbis_sys`, raw packets (no Ogg) — see
//! `docs/codec-implementation.md` for the rationale and Vorbis's MDCT
//! warm-up handling. Always runs at 44.1 kHz internally to match what
//! Spotify actually streams.
//!
//! Bitrate changes rebuild the encoder/decoder pair, which allocates briefly
//! inside `process()`. Acceptable in release builds; debug builds with
//! `assert_process_allocs` will panic on hot-switching tiers.

use crate::processor::pipeline::ResampledPipeline;
use aotuv_lancer_vorbis_sys as vsys;
use nih_plug::prelude::*;
use ogg_next_sys as osys;
use std::ptr;

const VORBIS_RATE: u32 = 44_100;
/// One Vorbis "long" block — discarded at the start of every fresh
/// encoder/decoder pair to skip MDCT warm-up garbage.
const PREROLL_SAMPLES: usize = 1024;

/// Map Spotify's nominal bitrate to the libvorbis VBR quality Spotify
/// actually uses. `-q5` (160) and `-q9` (320) were verified from a 2010
/// SoundExpert reverse-engineering of the Spotify stream:
/// <https://soundexpert.org/articles/-/blogs/11910>. `-q2` (96) follows
/// the standard libvorbis quality→bitrate table.
fn quality_for_spotify_bitrate(kbps: u32) -> f32 {
    match kbps {
        0..=96 => 0.2,
        97..=160 => 0.5,
        _ => 0.9,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
// Bypass currently goes through OpusProcessor; this Passthrough variant is
// kept so a future dispatch can keep Bypass on whichever pipeline the user
// was last on, avoiding cross-pipeline glitches on toggle.
#[allow(dead_code)]
pub enum VorbisMode {
    Passthrough,
    Vorbis { bitrate_kbps: u32 },
}

pub struct VorbisProcessor {
    pipeline: ResampledPipeline,

    /// `-1` when no codec is built. Stored so `ensure_codec` can
    /// short-circuit when the bitrate hasn't changed.
    current_bitrate_kbps: i32,
    /// `Box`ed because libvorbis stashes pointers between its own structs
    /// during init (e.g. `vorbis_dsp_state` → `vorbis_info`); the codec
    /// mustn't move after construction.
    codec: Option<Box<VorbisCodec>>,

    /// Refilled to `PREROLL_SAMPLES` on every codec rebuild.
    samples_to_discard: usize,
}

/// One encoder + matching decoder. Drop order matters — libvorbis structs
/// own internal pointers and need their `clear` functions called in a
/// specific sequence; see [`Drop`] below.
struct VorbisCodec {
    // Encoder side
    enc_vi: vsys::vorbis_info,
    enc_vc: vsys::vorbis_comment,
    enc_vd: vsys::vorbis_dsp_state,
    enc_vb: vsys::vorbis_block,
    // Decoder side
    dec_vi: vsys::vorbis_info,
    dec_vc: vsys::vorbis_comment,
    dec_vd: vsys::vorbis_dsp_state,
    dec_vb: vsys::vorbis_block,
}

impl Drop for VorbisCodec {
    fn drop(&mut self) {
        unsafe {
            vsys::vorbis_block_clear(&mut self.enc_vb);
            vsys::vorbis_dsp_clear(&mut self.enc_vd);
            vsys::vorbis_comment_clear(&mut self.enc_vc);
            vsys::vorbis_info_clear(&mut self.enc_vi);
            vsys::vorbis_block_clear(&mut self.dec_vb);
            vsys::vorbis_dsp_clear(&mut self.dec_vd);
            vsys::vorbis_comment_clear(&mut self.dec_vc);
            vsys::vorbis_info_clear(&mut self.dec_vi);
        }
    }
}

// SAFETY: libvorbis's interior `*mut` pointers are stable for the lifetime
// of the codec and we only ever touch it from the audio thread. Sharing
// across threads would still be unsound; we never do that.
unsafe impl Send for VorbisCodec {}

impl VorbisCodec {
    /// Build a matched encoder/decoder pair in VBR-quality mode.
    /// `quality` ∈ [0.0, 1.0], same scale as the CLI `-qN` flag (0.5 ==
    /// `-q5`). The decoder is primed with the three header packets from
    /// the encoder so both sides agree on codec setup.
    unsafe fn new(sample_rate: u32, channels: usize, quality: f32) -> Option<Box<Self>> {
        let mut codec = Box::new(Self {
            enc_vi: std::mem::zeroed(),
            enc_vc: std::mem::zeroed(),
            enc_vd: std::mem::zeroed(),
            enc_vb: std::mem::zeroed(),
            dec_vi: std::mem::zeroed(),
            dec_vc: std::mem::zeroed(),
            dec_vd: std::mem::zeroed(),
            dec_vb: std::mem::zeroed(),
        });

        vsys::vorbis_info_init(&mut codec.enc_vi);
        if vsys::vorbis_encode_init_vbr(
            &mut codec.enc_vi,
            channels as std::os::raw::c_long,
            sample_rate as std::os::raw::c_long,
            quality,
        ) != 0
        {
            vsys::vorbis_info_clear(&mut codec.enc_vi);
            return None;
        }
        vsys::vorbis_comment_init(&mut codec.enc_vc);
        vsys::vorbis_analysis_init(&mut codec.enc_vd, &mut codec.enc_vi);
        vsys::vorbis_block_init(&mut codec.enc_vd, &mut codec.enc_vb);

        // Header packets point into libvorbis-owned memory that's only
        // valid until the next encoder call — feed them to the decoder
        // immediately before doing anything else with the encoder.
        let mut header: osys::ogg_packet = std::mem::zeroed();
        let mut header_comm: osys::ogg_packet = std::mem::zeroed();
        let mut header_code: osys::ogg_packet = std::mem::zeroed();
        vsys::vorbis_analysis_headerout(
            &mut codec.enc_vd,
            &mut codec.enc_vc,
            &mut header,
            &mut header_comm,
            &mut header_code,
        );

        vsys::vorbis_info_init(&mut codec.dec_vi);
        vsys::vorbis_comment_init(&mut codec.dec_vc);
        for hdr in [&mut header, &mut header_comm, &mut header_code] {
            if vsys::vorbis_synthesis_headerin(&mut codec.dec_vi, &mut codec.dec_vc, hdr) != 0 {
                return None;
            }
        }
        vsys::vorbis_synthesis_init(&mut codec.dec_vd, &mut codec.dec_vi);
        vsys::vorbis_block_init(&mut codec.dec_vd, &mut codec.dec_vb);

        Some(codec)
    }
}

impl VorbisProcessor {
    pub fn new() -> Self {
        Self {
            pipeline: ResampledPipeline::new(),
            current_bitrate_kbps: -1,
            codec: None,
            samples_to_discard: 0,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        if !matches!(channels, 1 | 2) {
            nih_log!(
                "VorbisProcessor: only mono and stereo supported, got {} channels.",
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
            VORBIS_RATE,
            VORBIS_RATE,
        ) {
            nih_log!("VorbisProcessor: pipeline resampler setup failed.");
            return;
        }

        let latency = self.compute_natural_latency();
        self.pipeline.set_latency(latency);
    }

    /// First useful decoder output arrives ~4 blocks (4 × 1024 samples) after
    /// the first input sample: encoder accumulates 1 block + 1 lookahead
    /// before packet #1; packet #2 emits the windowed first block (which we
    /// discard); packet #3 emits the first usable audio. 4096 is the
    /// budget. 3072 was empirically under-padded at 96 kHz host.
    fn compute_natural_latency(&self) -> u32 {
        let host_rate = self.pipeline.host_rate as u64;
        let internal_rate = VORBIS_RATE as u64;
        let to_host = |internal: u32| (internal as u64 * host_rate / internal_rate) as u32;

        let (h2i_chunk_host, h2i_delay_internal) = self.pipeline.h2i_latency_pair();
        let (i2h_chunk_at_host, i2h_delay_at_host) = self.pipeline.i2h_latency_pair();
        const VORBIS_ROUNDTRIP_INTERNAL: u32 = 4 * (PREROLL_SAMPLES as u32);

        let internal_total = h2i_delay_internal + VORBIS_ROUNDTRIP_INTERNAL;
        h2i_chunk_host + to_host(internal_total) + i2h_chunk_at_host + i2h_delay_at_host
    }

    pub fn pad_output_to(&mut self, target: u32) {
        self.pipeline.pad_output_to(target);
    }

    /// Static estimate for the lazy-init path. See
    /// [`ResampledPipeline::estimate_latency`].
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        const VORBIS_ROUNDTRIP_INTERNAL: u32 = 4 * (PREROLL_SAMPLES as u32);
        ResampledPipeline::estimate_latency(
            host_rate,
            channels,
            VORBIS_RATE,
            VORBIS_RATE,
            VORBIS_ROUNDTRIP_INTERNAL,
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

    pub fn process(&mut self, buffer: &mut Buffer, mode: VorbisMode) {
        if !self.pipeline.ready {
            return;
        }
        let n_samples = buffer.samples();
        let block = buffer.as_slice();
        self.pipeline.push_host_block(block, n_samples);
        self.pipeline.pump_host_to_internal();

        match mode {
            VorbisMode::Passthrough => {
                let channels = self.pipeline.channels;
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            VorbisMode::Vorbis { bitrate_kbps } => {
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

    /// Rebuild the encoder/decoder pair when the bitrate changes. Allocates
    /// inside libvorbis — see the module-level note on debug builds.
    fn ensure_codec(&mut self, bitrate_kbps: u32) {
        let target_kbps = bitrate_kbps as i32;
        if self.current_bitrate_kbps == target_kbps && self.codec.is_some() {
            return;
        }
        // Drop the old pair before building a new one — never two alive at
        // once.
        self.codec = None;
        let quality = quality_for_spotify_bitrate(bitrate_kbps);
        // The codec always runs at VORBIS_RATE; the pipeline's h2i resampler
        // has already converted host → 44.1k by this point. Configuring
        // libvorbis with the host rate instead used to fail silently
        // because libvorbis's bitrate tables reject 96k stereo at 160 kbps.
        self.codec = unsafe { VorbisCodec::new(VORBIS_RATE, self.pipeline.channels, quality) };
        if self.codec.is_some() {
            self.current_bitrate_kbps = target_kbps;
            self.samples_to_discard = PREROLL_SAMPLES;
            #[cfg(debug_assertions)]
            nih_log!(
                "VorbisProcessor: built encoder/decoder for {} kbps target -> libvorbis -q{:.1} at {} Hz, {} channels.",
                bitrate_kbps,
                quality * 10.0,
                VORBIS_RATE,
                self.pipeline.channels,
            );
        } else {
            self.current_bitrate_kbps = -1;
            #[cfg(debug_assertions)]
            nih_log!(
                "VorbisProcessor: failed to build encoder/decoder for {} kbps at {} Hz, {} channels.",
                bitrate_kbps,
                VORBIS_RATE,
                self.pipeline.channels,
            );
        }
    }

    /// Run everything in `internal_input` through the encoder + decoder.
    /// Decoded samples land in `internal_output` minus the pre-roll discard.
    fn pump_codec(&mut self) {
        let codec = match self.codec.as_mut() {
            Some(c) => c,
            None => return,
        };
        let channels = self.pipeline.channels;
        let pending = self.pipeline.internal_input[0].len();
        if pending == 0 {
            return;
        }

        unsafe {
            // libvorbis hands us a planar per-channel buffer; bulk-copy the
            // ring slice into it via `make_contiguous` + one
            // `copy_nonoverlapping` per channel.
            let buf_ptr = vsys::vorbis_analysis_buffer(&mut codec.enc_vd, pending as i32);
            for ch in 0..channels {
                let ch_buf = *buf_ptr.add(ch);
                let ring = &mut self.pipeline.internal_input[ch];
                let head = ring.make_contiguous();
                let take = head.len().min(pending);
                std::ptr::copy_nonoverlapping(head.as_ptr(), ch_buf, take);
                if take < pending {
                    std::ptr::write_bytes(ch_buf.add(take), 0, pending - take);
                }
                ring.drain(..take);
            }
            vsys::vorbis_analysis_wrote(&mut codec.enc_vd, pending as i32);

            // Analysis blocks → packets → decoder → PCM out.
            while vsys::vorbis_analysis_blockout(&mut codec.enc_vd, &mut codec.enc_vb) == 1 {
                vsys::vorbis_analysis(&mut codec.enc_vb, ptr::null_mut());
                vsys::vorbis_bitrate_addblock(&mut codec.enc_vb);

                let mut packet: osys::ogg_packet = std::mem::zeroed();
                while vsys::vorbis_bitrate_flushpacket(&mut codec.enc_vd, &mut packet) == 1 {
                    if vsys::vorbis_synthesis(&mut codec.dec_vb, &mut packet) == 0 {
                        vsys::vorbis_synthesis_blockin(&mut codec.dec_vd, &mut codec.dec_vb);
                    }

                    let mut pcm: *mut *mut f32 = ptr::null_mut();
                    loop {
                        let samples = vsys::vorbis_synthesis_pcmout(&mut codec.dec_vd, &mut pcm);
                        if samples <= 0 {
                            break;
                        }
                        let skip = self.samples_to_discard.min(samples as usize);
                        let kept_start = skip;
                        let kept_count = samples as usize - skip;
                        for ch in 0..channels {
                            let ch_ptr = *pcm.add(ch);
                            // PCM is contiguous per channel — bulk-extend the
                            // ring rather than push sample-by-sample.
                            let kept_slice =
                                std::slice::from_raw_parts(ch_ptr.add(kept_start), kept_count);
                            self.pipeline.internal_output[ch].reserve(kept_count);
                            self.pipeline.internal_output[ch]
                                .extend(kept_slice.iter().copied());
                        }
                        self.samples_to_discard -= skip;
                        vsys::vorbis_synthesis_read(&mut codec.dec_vd, samples);
                    }
                }
            }
        }
    }
}

impl VorbisProcessor {
    /// Test-only `process()` without the nih-plug `Buffer` wrapping.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: VorbisMode,
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
            VorbisMode::Passthrough => {
                for ch in 0..channels {
                    while let Some(s) = self.pipeline.internal_input[ch].pop_front() {
                        self.pipeline.internal_output[ch].push_back(s);
                    }
                }
            }
            VorbisMode::Vorbis { bitrate_kbps } => {
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

impl Default for VorbisProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000] {
            for &bitrate_kbps in &[96u32, 160, 320] {
                let mut proc = VorbisProcessor::new();
                let block_size = 256;
                proc.initialize(host_rate, 2, block_size);
                assert!(
                    proc.pipeline.ready,
                    "initialize failed for host_rate={host_rate} Hz"
                );

                // 5 s input + 1 s warmup — Vorbis has a longer warm-up than Opus.
                let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
                    host_rate,
                    block_size,
                    5.0,
                    1.0,
                    440.0,
                    0.5,
                    |inp, out| {
                        proc.process_planar(inp, out, VorbisMode::Vorbis { bitrate_kbps });
                    },
                );
                assert!(
                    peak > 0.05,
                    "Vorbis at {host_rate} Hz / {bitrate_kbps} kbps: \
                     peak {peak:.3} below threshold (latency={} samples)",
                    proc.latency_samples()
                );
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = VorbisProcessor::new();
        proc.initialize(48_000, 2, 256);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, VorbisMode::Passthrough),
        );
        assert!(peak > 0.05, "Vorbis passthrough silent: {peak:.3}");
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut proc = VorbisProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(&inp, &mut out, VorbisMode::Vorbis { bitrate_kbps: 160 });
        }
        proc.reset();
        assert!(proc.pipeline.ready);
    }

    #[test]
    fn pad_output_to_grows_latency() {
        let mut proc = VorbisProcessor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.latency_samples();
        proc.pad_output_to(before + 1_000);
        assert_eq!(proc.latency_samples(), before + 1_000);
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 88_200, 96_000] {
            let l = VorbisProcessor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = VorbisProcessor::default();
        assert!(!proc.pipeline.ready);
        assert_eq!(proc.latency_samples(), 0);
    }

    #[test]
    fn bitrate_change_rebuilds_codec_without_panic() {
        let mut proc = VorbisProcessor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for kbps in [96u32, 160, 320, 96] {
            proc.process_planar(
                &inp,
                &mut out,
                VorbisMode::Vorbis { bitrate_kbps: kbps },
            );
        }
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = VorbisProcessor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.pipeline.ready);
    }
}
