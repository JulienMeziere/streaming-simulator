//! Real-time MP3 encode → decode via LAME + minimp3.
//!
//! Audio is resampled to 44.1 kHz internally (MPEG-1 Layer 3 only supports
//! 32 / 44.1 / 48 kHz). Encoder and decoder are wired with raw MP3 frame
//! bytes — no container.
//!
//! See `docs/codec-implementation.md` for the rationale behind using
//! `minimp3-sys` directly (bit-reservoir state across frames) and how we
//! follow LAME's auto-downsample at low bitrates (24 kHz @ 64 kbps stereo).

use crate::processor::pipeline::ResampledPipeline;
use mp3lame_encoder::{Bitrate, Builder, DualPcm, Encoder, Mode, MonoPcm, Quality, VbrMode};
use nih_plug::prelude::*;

const MP3_RATE: u32 = 44_100;
/// MPEG-1 Layer 3 frame size — 1152 samples per channel, every bitrate.
const MP3_FRAME_SIZE: usize = 1152;
/// First decoded frame is MDCT warm-up garbage; discard it.
const PREROLL_SAMPLES: usize = MP3_FRAME_SIZE;
/// Decoder input ring cap — well above 3 frames at 320 kbps stereo. Just a
/// safety net; never hit at steady state.
const DEC_INPUT_CAP: usize = 32 * 1024;
/// `mp3lame_encoder::max_required_buffer_size` for 1152 samples at 320 kbps.
const MAX_OUTPUT_PER_FRAME: usize = 7_200 + 1_152 * 5 / 4;

#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub enum Mp3Mode {
    Passthrough,
    Mp3 { bitrate_kbps: u32 },
}

pub struct Mp3Processor {
    pipeline: ResampledPipeline,

    current_bitrate_kbps: i32,
    /// Minimum `dec_input` bytes before calling `mp3dec_decode_frame`.
    /// minimp3 wipes its bit-reservoir state when it can't validate the
    /// *next* frame's sync, which makes every subsequent frame fail to
    /// decode (they reference the reservoir via `main_data_begin`). We
    /// avoid that by buffering ~2 frames + 4 bytes header slack; computed
    /// from the bitrate at codec-build time.
    min_decode_bytes: usize,
    codec: Option<Mp3Codec>,

    /// Planar i16 PCM, one frame per channel. LAME's `DualPcm` takes two
    /// `&[i16]`, so planar fits better than interleaved.
    pcm_planar: Vec<Vec<i16>>,
    /// Encoder output scratch, reused to keep `process()` alloc-free.
    enc_out_buf: Vec<u8>,
    /// Encoder-emission-order bytes pending decode.
    dec_input: Vec<u8>,
    /// Decoder PCM scratch — interleaved i16, MINIMP3_MAX_SAMPLES_PER_FRAME.
    pcm_dec: Vec<i16>,

    samples_to_discard: usize,

    /// One-shot diagnostic flags — logged at most once each.
    logged_first_encode: bool,
    logged_first_decode: bool,
    logged_first_host_push: bool,
}

/// Encoder + decoder pair, rebuilt on bitrate changes (LAME requires the
/// bitrate at construction time).
struct Mp3Codec {
    encoder: Encoder,
    /// minimp3 decoder state. POD with no interior pointers, so unlike
    /// `VorbisCodec` we don't have to box it.
    decoder: minimp3_sys::mp3dec_t,
    /// Rate minimp3 emits at — may be lower than `MP3_RATE` because LAME
    /// auto-downsamples the bitstream at low bitrates (e.g. 24 kHz at
    /// 64 kbps stereo). Matches what Deezer streams for those tiers.
    output_hz: u32,
}

impl Mp3Codec {
    fn new(channels: usize, bitrate_kbps: u32) -> Option<Self> {
        let bitrate = bitrate_to_lame(bitrate_kbps)?;
        // mp3lame-encoder doesn't expose `out_samplerate` as a getter, so
        // we probe with a throwaway encoder + decoder.
        let output_hz = detect_output_rate(channels, bitrate)?;

        let encoder = build_encoder(channels, bitrate)?;
        let mut decoder: minimp3_sys::mp3dec_t = unsafe { std::mem::zeroed() };
        // SAFETY: `mp3dec_init` is a pure zero-init.
        unsafe { minimp3_sys::mp3dec_init(&mut decoder as *mut _) };

        Some(Self {
            encoder,
            decoder,
            output_hz,
        })
    }
}

/// LAME encoder with the streaming-grade settings (see docs/codecs.md).
/// Shared by the production encoder and the throwaway probe in
/// `detect_output_rate` so they can't drift.
fn build_encoder(channels: usize, bitrate: Bitrate) -> Option<Encoder> {
    let mut builder = Builder::new()?;
    builder.set_num_channels(channels as u8).ok()?;
    builder.set_sample_rate(MP3_RATE).ok()?;
    builder.set_brate(bitrate).ok()?;
    builder.set_quality(Quality::Good).ok()?;
    builder.set_mode(Mode::JointStereo).ok()?;
    builder.set_vbr_mode(VbrMode::Off).ok()?;
    // No Xing/Info VBR header — streaming, not file output. Saves one frame
    // of warm-up silence at the start.
    builder.set_to_write_vbr_tag(false).ok()?;
    builder.build().ok()
}

/// Encode silence through a throwaway LAME and read back the first
/// successfully-decoded frame's `info.hz`.
fn detect_output_rate(channels: usize, bitrate: Bitrate) -> Option<u32> {
    let mut encoder = build_encoder(channels, bitrate)?;
    let silence_l = vec![0i16; MP3_FRAME_SIZE];
    let silence_r = vec![0i16; MP3_FRAME_SIZE];
    let mut enc_buf: Vec<u8> = Vec::with_capacity(MAX_OUTPUT_PER_FRAME);
    let mut dec_input: Vec<u8> = Vec::with_capacity(32 * 1024);
    let mut dec: minimp3_sys::mp3dec_t = unsafe { std::mem::zeroed() };
    unsafe { minimp3_sys::mp3dec_init(&mut dec) };
    let mut pcm = vec![0i16; minimp3_sys::MINIMP3_MAX_SAMPLES_PER_FRAME as usize];
    // 2 frames at 320 kbps + 4-byte header slack — under this minimp3 will
    // wipe its state and the probe never converges.
    const PROBE_THRESHOLD: usize = 2 * 1100 + 4;

    for _ in 0..64 {
        enc_buf.clear();
        let out = enc_buf.spare_capacity_mut();
        let n = if channels == 2 {
            encoder
                .encode(
                    DualPcm {
                        left: &silence_l,
                        right: &silence_r,
                    },
                    out,
                )
                .unwrap_or(0)
        } else {
            encoder.encode(MonoPcm(&silence_l), out).unwrap_or(0)
        };
        // SAFETY: encoder wrote exactly `n` bytes into the spare prefix.
        unsafe { enc_buf.set_len(n) };
        if n > 0 {
            dec_input.extend_from_slice(&enc_buf);
        }
        if dec_input.len() >= PROBE_THRESHOLD {
            let mut info: minimp3_sys::mp3dec_frame_info_t = unsafe { std::mem::zeroed() };
            let _ = unsafe {
                minimp3_sys::mp3dec_decode_frame(
                    &mut dec,
                    dec_input.as_ptr(),
                    dec_input.len() as i32,
                    pcm.as_mut_ptr(),
                    &mut info,
                )
            };
            if info.hz > 0 {
                return Some(info.hz as u32);
            }
            let consumed = info.frame_bytes as usize;
            if consumed > 0 && consumed < dec_input.len() {
                dec_input.drain(..consumed);
            }
        }
    }
    // 64 silent frames ≈ 1.7 s — if minimp3 hasn't reported a rate by now
    // the codec is broken. Fall back to the default rate.
    #[cfg(debug_assertions)]
    nih_log!(
        "Mp3Processor: detect_output_rate timed out — falling back to {} Hz",
        MP3_RATE
    );
    Some(MP3_RATE)
}

/// Snap a free-form kbps value to LAME's CBR enum (closest value ≥ input).
fn bitrate_to_lame(kbps: u32) -> Option<Bitrate> {
    Some(match kbps {
        0..=8 => Bitrate::Kbps8,
        9..=16 => Bitrate::Kbps16,
        17..=24 => Bitrate::Kbps24,
        25..=32 => Bitrate::Kbps32,
        33..=40 => Bitrate::Kbps40,
        41..=48 => Bitrate::Kbps48,
        49..=64 => Bitrate::Kbps64,
        65..=80 => Bitrate::Kbps80,
        81..=96 => Bitrate::Kbps96,
        97..=112 => Bitrate::Kbps112,
        113..=128 => Bitrate::Kbps128,
        129..=160 => Bitrate::Kbps160,
        161..=192 => Bitrate::Kbps192,
        193..=224 => Bitrate::Kbps224,
        225..=256 => Bitrate::Kbps256,
        _ => Bitrate::Kbps320,
    })
}

impl Mp3Processor {
    pub fn new() -> Self {
        Self {
            pipeline: ResampledPipeline::new(),
            current_bitrate_kbps: -1,
            min_decode_bytes: 0,
            codec: None,
            pcm_planar: Vec::new(),
            enc_out_buf: Vec::new(),
            dec_input: Vec::new(),
            pcm_dec: Vec::new(),
            samples_to_discard: 0,
            logged_first_encode: false,
            logged_first_decode: false,
            logged_first_host_push: false,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        nih_log!(
            "Mp3Processor::initialize host_rate={} channels={} max_block={}",
            sample_rate,
            channels,
            max_block_size
        );
        if !matches!(channels, 1 | 2) {
            nih_log!(
                "Mp3Processor: only mono and stereo are supported, got {} channels.",
                channels
            );
            return;
        }
        self.current_bitrate_kbps = -1;
        self.codec = None;

        // Initial decode_rate = MP3_RATE; ensure_codec rebuilds the i2h
        // resampler if LAME picks a lower rate at the chosen bitrate.
        if !self
            .pipeline
            .setup(sample_rate, channels, max_block_size, MP3_RATE, MP3_RATE)
        {
            nih_log!("Mp3Processor: pipeline resampler setup failed.");
            return;
        }

        self.pcm_planar = (0..channels).map(|_| vec![0i16; MP3_FRAME_SIZE]).collect();
        // `len = 0` with reserved capacity — every encode writes into the
        // spare-capacity `[MaybeUninit<u8>]` then bumps `len`. See `pump_codec`.
        self.enc_out_buf = Vec::with_capacity(MAX_OUTPUT_PER_FRAME);
        self.dec_input = Vec::with_capacity(DEC_INPUT_CAP);
        self.pcm_dec = vec![0i16; minimp3_sys::MINIMP3_MAX_SAMPLES_PER_FRAME as usize];

        // Pad to the *worst-case* decode_rate (22.05 kHz for low-bitrate
        // stereo) so subsequent codec rebuilds never need to grow the ring
        // mid-stream and risk an audible underrun.
        let latency = self.worst_case_natural_latency();
        self.pipeline.set_latency(latency);

        nih_log!(
            "Mp3Processor: initialized at host_rate={} Hz, channels={}, latency={} host samples ({:.1} ms), h2i_chunk={}, i2h_chunk={}",
            sample_rate,
            channels,
            self.pipeline.latency_host_samples,
            self.pipeline.latency_host_samples as f64 * 1000.0 / sample_rate as f64,
            self.pipeline.h2i_chunk,
            self.pipeline.i2h_chunk,
        );
    }

    /// Pipeline delay for the active `decode_rate`. Budget 6 frames at the
    /// LAME *input* rate (44.1 kHz) — covers LAME's 576-sample lookahead +
    /// ~528-sample filter delay + the decoder's MDCT warm-up. Wall-clock
    /// time, independent of output rate.
    fn compute_natural_latency(&self) -> u32 {
        let host_rate = self.pipeline.host_rate as u64;
        let lame_in_rate = MP3_RATE as u64;
        const MP3_ROUNDTRIP_INTERNAL: u64 = 6 * (MP3_FRAME_SIZE as u64);

        let (h2i_chunk_host, h2i_delay_internal) = self.pipeline.h2i_latency_pair();
        let h2i_delay_at_host = h2i_delay_internal as u64 * host_rate / lame_in_rate;
        let lame_at_host = MP3_ROUNDTRIP_INTERNAL * host_rate / lame_in_rate;
        let (i2h_chunk_at_host, i2h_delay_at_host) = self.pipeline.i2h_latency_pair();

        (h2i_chunk_host as u64
            + h2i_delay_at_host
            + lame_at_host
            + i2h_chunk_at_host as u64
            + i2h_delay_at_host as u64) as u32
    }

    /// Max latency across every `decode_rate` LAME might pick for our tiers.
    /// Reported latency stays fixed at this value regardless of the active
    /// bitrate so bitrate switches don't re-tick host PDC.
    fn worst_case_natural_latency(&mut self) -> u32 {
        let mut max = 0u32;
        let original_decode_rate = self.pipeline.decode_rate;
        for &candidate_hz in &[22_050u32, 24_000, 32_000, MP3_RATE] {
            if !self.pipeline.setup_i2h(candidate_hz) {
                continue;
            }
            max = max.max(self.compute_natural_latency());
        }
        // Restore the i2h resampler to the rate the caller expects.
        let _ = self.pipeline.setup_i2h(original_decode_rate);
        max
    }

    pub fn pad_output_to(&mut self, target: u32) {
        self.pipeline.pad_output_to(target);
    }

    /// Static estimate for the lazy-init path. Probes every possible LAME
    /// output rate and returns the max.
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        const MP3_ROUNDTRIP_INTERNAL: u32 = 6 * (MP3_FRAME_SIZE as u32);
        let mut max_latency = 0u32;
        for &decode_hz in &[22_050u32, 24_000, 32_000, MP3_RATE] {
            let l = ResampledPipeline::estimate_latency(
                host_rate,
                channels,
                MP3_RATE,
                decode_hz,
                MP3_ROUNDTRIP_INTERNAL,
            );
            max_latency = max_latency.max(l);
        }
        max_latency
    }

    pub fn reset(&mut self) {
        if !self.pipeline.ready {
            return;
        }
        self.codec = None;
        self.current_bitrate_kbps = -1;
        self.dec_input.clear();
        self.pipeline.reset();
    }

    pub fn latency_samples(&self) -> u32 {
        if self.pipeline.ready {
            self.pipeline.latency_host_samples
        } else {
            0
        }
    }

    pub fn process(&mut self, buffer: &mut Buffer, mode: Mp3Mode) {
        if !self.pipeline.ready {
            return;
        }
        let n_samples = buffer.samples();
        let block = buffer.as_slice();
        self.pipeline.push_host_block(block, n_samples);
        self.run_pipeline(mode);
        self.pipeline.drain_host_block(block, n_samples);
    }

    /// Drive the host_input → host_output side. Factored out of `process()`
    /// so tests can drive planar f32 buffers without a nih-plug `Buffer`.
    fn run_pipeline(&mut self, mode: Mp3Mode) {
        self.pipeline.pump_host_to_internal();

        match mode {
            Mp3Mode::Passthrough => {
                // Bulk slice copy: `make_contiguous` once per channel + one
                // `clear` to drop the prefix.
                let channels = self.pipeline.channels;
                for ch in 0..channels {
                    let n = self.pipeline.internal_input[ch].len();
                    if n == 0 {
                        continue;
                    }
                    self.pipeline.internal_output[ch].reserve(n);
                    let head = self.pipeline.internal_input[ch].make_contiguous();
                    self.pipeline.internal_output[ch].extend(head[..n].iter().copied());
                    self.pipeline.internal_input[ch].clear();
                }
            }
            Mp3Mode::Mp3 { bitrate_kbps } => {
                self.ensure_codec(bitrate_kbps);
                if self.codec.is_some() {
                    self.pump_codec();
                } else {
                    // Encoder build failed — drain input to silence so we
                    // don't stall.
                    let channels = self.pipeline.channels;
                    for ch in 0..channels {
                        let n = self.pipeline.internal_input[ch].len();
                        if n > 0 {
                            self.pipeline.internal_output[ch].reserve(n);
                            self.pipeline.internal_output[ch]
                                .extend(std::iter::repeat_n(0.0, n));
                            self.pipeline.internal_input[ch].clear();
                        }
                    }
                }
            }
        }

        self.pipeline.pump_internal_to_host();
    }

    /// Test-only `process()` without the nih-plug `Buffer` wrapping.
    #[cfg(test)]
    pub fn process_planar(
        &mut self,
        input: &[Vec<f32>],
        output: &mut [Vec<f32>],
        mode: Mp3Mode,
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
        self.run_pipeline(mode);
        for ch in 0..channels {
            for s in 0..n {
                output[ch][s] = self.pipeline.host_output[ch].pop_front().unwrap_or(0.0);
            }
        }
    }

    fn ensure_codec(&mut self, bitrate_kbps: u32) {
        let target = bitrate_kbps as i32;
        if self.current_bitrate_kbps == target && self.codec.is_some() {
            return;
        }
        self.codec = None;
        let channels = self.pipeline.channels;
        let new_codec = Mp3Codec::new(channels, bitrate_kbps);
        let Some(codec) = new_codec else {
            self.current_bitrate_kbps = -1;
            self.min_decode_bytes = 0;
            #[cfg(debug_assertions)]
            nih_log!(
                "Mp3Processor: FAILED to build encoder/decoder for {} kbps at {} Hz, {} channels.",
                bitrate_kbps,
                MP3_RATE,
                channels,
            );
            return;
        };

        let new_decoded_hz = codec.output_hz;
        let decoded_hz_changed = new_decoded_hz != self.pipeline.decode_rate;
        self.codec = Some(codec);
        self.current_bitrate_kbps = target;

        // Rate-change path (e.g. Deezer Basic ↔ Standard): rebuild the i2h
        // resampler and flush the in-flight `internal_output` samples,
        // which were emitted at the *old* rate. Causes a brief silence
        // glitch — encoder re-priming would do that anyway.
        if decoded_hz_changed {
            for ring in self.pipeline.internal_output.iter_mut() {
                ring.clear();
            }
            let _ = self.pipeline.setup_i2h(new_decoded_hz);
        }

        // MPEG-2 / 2.5 rates emit 576 samples/frame, MPEG-1 emits 1152.
        // Compute the threshold from the actual decode rate so it's right
        // for every (bitrate × auto-downsample) combo.
        let frame_samples = if self.pipeline.decode_rate <= 24_000 { 576 } else { 1152 };
        let nominal_bytes = frame_samples
            * (bitrate_kbps as usize)
            * 1000
            / 8
            / self.pipeline.decode_rate as usize;
        // 2 full frames + 4-byte HDR_SIZE slack. See `min_decode_bytes` doc.
        self.min_decode_bytes = 2 * (nominal_bytes + 1) + 4;
        self.samples_to_discard = PREROLL_SAMPLES;
        self.dec_input.clear();
        self.logged_first_encode = false;
        self.logged_first_decode = false;
        self.logged_first_host_push = false;
        #[cfg(debug_assertions)]
        nih_log!(
            "Mp3Processor: built encoder/decoder for {} kbps -- LAME picked {} Hz output, {} channels (frame~{}B, decode threshold {}B).",
            bitrate_kbps,
            self.pipeline.decode_rate,
            channels,
            nominal_bytes,
            self.min_decode_bytes,
        );
    }

    /// Drain `internal_input` through encoder → decoder → `internal_output`,
    /// one frame-aligned chunk at a time. The decoder may emit zero, one,
    /// or several frames per call depending on bit-reservoir state.
    fn pump_codec(&mut self) {
        let codec = match self.codec.as_mut() {
            Some(c) => c,
            None => return,
        };
        let channels = self.pipeline.channels;

        while self.pipeline.internal_input[0].len() >= MP3_FRAME_SIZE {
            // Bulk drain + f32→i16 quantise per channel: `make_contiguous`
            // gives the autovec a real slice + one `drain` per frame.
            for ch in 0..channels {
                let head = self.pipeline.internal_input[ch].make_contiguous();
                let frame = &head[..MP3_FRAME_SIZE];
                let dst = &mut self.pcm_planar[ch][..MP3_FRAME_SIZE];
                for (i, &f) in frame.iter().enumerate() {
                    dst[i] = (f.clamp(-1.0, 1.0) * 32767.0) as i16;
                }
                self.pipeline.internal_input[ch].drain(..MP3_FRAME_SIZE);
            }

            // SAFETY: `Encoder::encode` initialises exactly `written` bytes
            // of the spare-capacity prefix; `set_len` exposes only those.
            self.enc_out_buf.clear();
            let out_slice = self.enc_out_buf.spare_capacity_mut();
            let written = if channels == 2 {
                let input = DualPcm {
                    left: &self.pcm_planar[0],
                    right: &self.pcm_planar[1],
                };
                codec.encoder.encode(input, out_slice).unwrap_or(0)
            } else {
                let input = MonoPcm(&self.pcm_planar[0]);
                codec.encoder.encode(input, out_slice).unwrap_or(0)
            };
            unsafe { self.enc_out_buf.set_len(written); }

            if written > 0 {
                if !self.logged_first_encode {
                    self.logged_first_encode = true;
                    #[cfg(debug_assertions)]
                    nih_log!(
                        "Mp3Processor: first encoder output -- {} bytes",
                        written
                    );
                }
                // Safety net for runaway buildup; never triggers at steady state.
                if self.dec_input.len() + written > DEC_INPUT_CAP {
                    let excess = self.dec_input.len() + written - DEC_INPUT_CAP;
                    self.dec_input.drain(..excess.min(self.dec_input.len()));
                }
                self.dec_input.extend_from_slice(&self.enc_out_buf);
            }

            // Below `min_decode_bytes` minimp3 wipes its bit reservoir —
            // see the field's doc comment. Wait for more encoder output.
            if self.dec_input.len() < self.min_decode_bytes {
                continue;
            }

            // `mp3dec_decode_frame` contract:
            //   - samples > 0, frame_bytes > 0: frame decoded; advance by
            //     frame_bytes.
            //   - samples == 0, frame_bytes < pre_len: skip `frame_bytes`
            //     of junk before the next sync, then retry.
            //   - samples == 0, frame_bytes >= pre_len: incomplete frame
            //     at end of buffer — *do not drain*; wait for more bytes.
            loop {
                let pre_len = self.dec_input.len();
                if pre_len < self.min_decode_bytes {
                    break;
                }
                let mut info: minimp3_sys::mp3dec_frame_info_t =
                    unsafe { std::mem::zeroed() };
                // SAFETY: `dec_input` is contiguous; `pcm_dec` is at least
                // `MINIMP3_MAX_SAMPLES_PER_FRAME` long.
                let samples = unsafe {
                    minimp3_sys::mp3dec_decode_frame(
                        &mut codec.decoder as *mut _,
                        self.dec_input.as_ptr(),
                        self.dec_input.len() as i32,
                        self.pcm_dec.as_mut_ptr(),
                        &mut info as *mut _,
                    )
                };

                let consumed = info.frame_bytes as usize;
                if consumed == 0 {
                    break;
                }
                if samples == 0 && consumed >= pre_len {
                    break;
                }
                self.dec_input.drain(..consumed.min(self.dec_input.len()));

                if samples > 0 {
                    if !self.logged_first_decode {
                        self.logged_first_decode = true;
                        #[cfg(debug_assertions)]
                        nih_log!(
                            "Mp3Processor: first decoder output — {} samples, {} ch, {} Hz",
                            samples,
                            info.channels,
                            info.hz
                        );
                    }
                    let n = samples as usize;
                    let dec_channels = info.channels as usize;
                    let skip = self.samples_to_discard.min(n);
                    self.samples_to_discard -= skip;
                    if skip < n && !self.logged_first_host_push {
                        self.logged_first_host_push = true;
                        #[cfg(debug_assertions)]
                        nih_log!(
                            "Mp3Processor: first audible sample reached internal_output ring",
                        );
                    }
                    // Per-channel bulk push: one `reserve` + strided iter
                    // over interleaved `pcm_dec` doing i16→f32. Mono input
                    // duplicates channel 0 to fill extra host channels.
                    let kept = n - skip;
                    let used_dec_channels = channels.min(dec_channels);
                    for ch in 0..used_dec_channels {
                        let dst = &mut self.pipeline.internal_output[ch];
                        dst.reserve(kept);
                        dst.extend(
                            self.pcm_dec[skip * dec_channels..n * dec_channels]
                                .chunks_exact(dec_channels)
                                .map(|frame| frame[ch] as f32 / 32768.0),
                        );
                    }
                    if dec_channels < channels {
                        for ch in dec_channels..channels {
                            let dst = &mut self.pipeline.internal_output[ch];
                            dst.reserve(kept);
                            dst.extend(
                                self.pcm_dec[skip * dec_channels..n * dec_channels]
                                    .chunks_exact(dec_channels)
                                    .map(|frame| frame[0] as f32 / 32768.0),
                            );
                        }
                    }
                }
            }
        }
    }
}

impl Default for Mp3Processor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LAME's auto-downsample decision must match real-world encoders
    /// (Deezer Basic 64 kbps stereo → 24 kHz). This test pins it.
    #[test]
    fn detect_output_rate_matches_lame_decision() {
        let cases = [
            (64u32, 2usize, 24_000u32),
            (128, 2, 44_100),
            (320, 2, 44_100),
            (128, 1, 44_100),
            (320, 1, 44_100),
        ];
        for &(bitrate_kbps, channels, expected_rate) in &cases {
            let codec = Mp3Codec::new(channels, bitrate_kbps)
                .expect("Mp3Codec::new should succeed for valid configs");
            assert_eq!(
                codec.output_hz, expected_rate,
                "{} kbps x {} ch — expected {} Hz, got {} Hz",
                bitrate_kbps, channels, expected_rate, codec.output_hz,
            );
        }
    }

    /// Reproduces the "silent at 96 kHz" symptom locally so we don't have
    /// to depend on DAW logs to catch this class of bug.
    #[test]
    fn emits_audio_at_every_host_rate_and_bitrate() {
        for &host_rate in &[44_100u32, 48_000, 88_200, 96_000, 192_000] {
            for &bitrate_kbps in &[64u32, 128, 320] {
                let mut proc = Mp3Processor::new();
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
                        proc.process_planar(inp, out, Mp3Mode::Mp3 { bitrate_kbps });
                    },
                );
                assert!(
                    peak > 0.05,
                    "MP3 at {host_rate} Hz / {bitrate_kbps} kbps: \
                     peak {peak:.3} below threshold (latency={} samples)",
                    proc.latency_samples()
                );
            }
        }
    }

    #[test]
    fn passthrough_mode_emits_audio() {
        let mut proc = Mp3Processor::new();
        proc.initialize(48_000, 2, 256);
        let peak = crate::test_helpers::drive_with_sine_io_and_measure_planar(
            48_000,
            256,
            1.0,
            0.25,
            440.0,
            0.5,
            |inp, out| proc.process_planar(inp, out, Mp3Mode::Passthrough),
        );
        assert!(peak > 0.05, "MP3 passthrough silent: {peak:.3}");
    }

    #[test]
    fn reset_clears_state_safely() {
        let mut proc = Mp3Processor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for _ in 0..4 {
            proc.process_planar(&inp, &mut out, Mp3Mode::Mp3 { bitrate_kbps: 128 });
        }
        proc.reset();
        assert!(proc.pipeline.ready);
    }

    #[test]
    fn pad_output_to_grows_latency() {
        let mut proc = Mp3Processor::new();
        proc.initialize(48_000, 2, 256);
        let before = proc.latency_samples();
        proc.pad_output_to(before + 1_000);
        assert_eq!(proc.latency_samples(), before + 1_000);
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 88_200, 96_000, 192_000] {
            let l = Mp3Processor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_yields_unready_processor() {
        let proc = Mp3Processor::default();
        assert!(!proc.pipeline.ready);
    }

    #[test]
    fn bitrate_change_rebuilds_codec_without_panic() {
        let mut proc = Mp3Processor::new();
        proc.initialize(48_000, 2, 256);
        let inp: Vec<Vec<f32>> = vec![vec![0.3; 256]; 2];
        let mut out: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        for kbps in [64u32, 128, 320, 64] {
            proc.process_planar(&inp, &mut out, Mp3Mode::Mp3 { bitrate_kbps: kbps });
        }
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut proc = Mp3Processor::new();
        proc.initialize(48_000, 7, 256);
        assert!(!proc.pipeline.ready);
    }
}
