// DSP code in this crate walks parallel arrays (channels × samples ×
// frames) where indexed loops read more clearly than `iter().enumerate()`
// chains. Allowed crate-wide instead of sprinkling `#[allow]` everywhere.
#![allow(clippy::needless_range_loop)]

use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use std::collections::VecDeque;
use std::sync::Arc;

mod editor;
mod platforms;
// Public so integration tests under `tests/` can drive processors directly.
pub mod processor;
#[cfg(test)]
pub(crate) mod test_helpers;

pub use platforms::{Codec, CodecDef, CodecSpec, FmRadioVariant, PlatformDef, PLATFORMS};

use processor::bluetooth::{BluetoothProcessor, BluetoothProtocol};
use processor::fm_radio::{FmRadioMode, FmRadioProcessor};
use processor::mp3::{Mp3Mode, Mp3Processor};
use processor::opus::{OpusMode, OpusProcessor};
use processor::vorbis::{VorbisMode, VorbisProcessor};

#[cfg(feature = "fdk-aac")]
use processor::aac::{AacMode, AacProcessor};
#[cfg(feature = "fdk-aac")]
use processor::heaac::{HeAacV1Mode, HeAacV1Processor};
#[cfg(feature = "fdk-aac")]
use processor::heaacv2::{HeAacV2Mode, HeAacV2Processor};

pub struct StreamingSimulator {
    params: Arc<StreamingSimulatorParams>,

    /// Codec processors are lazy. Each one carries ~50-500 KB of pre-fill
    /// silence, ring buffers, and rubato FFT plans, and a typical session
    /// only touches one or two. They're built on first dispatch; up-front
    /// latency comes from each processor's static `worst_case_latency_at`.
    opus: Option<OpusProcessor>,
    vorbis: Option<VorbisProcessor>,
    mp3: Option<Mp3Processor>,
    fm_radio: Option<FmRadioProcessor>,
    #[cfg(feature = "fdk-aac")]
    aac: Option<AacProcessor>,
    #[cfg(feature = "fdk-aac")]
    heaac_v1: Option<HeAacV1Processor>,
    #[cfg(feature = "fdk-aac")]
    heaac: Option<HeAacV2Processor>,

    /// Bluetooth cascade processor — applies on top of the platform codec.
    /// Lazy like the rest; the inner SBC / LC3 / AAC-BT backends are
    /// themselves lazy so flipping presets doesn't pre-allocate unused codecs.
    bluetooth: Option<BluetoothProcessor>,

    /// Host config cached at `Plugin::initialize` so lazy dispatch can build
    /// each processor with the right settings on first use.
    host_sample_rate: u32,
    host_channels: usize,
    host_max_block_size: usize,
    /// Plugin-wide reported latency = max across every codec. Every freshly-
    /// built processor is `pad_output_to`'d to this so codec switches don't
    /// shift PDC.
    target_latency_samples: u32,

    /// Per-channel dry delay matched to `target_latency_samples`. Used as
    /// the bypass / FLAC-tier output. Always pushed to (every `process`
    /// call) and always drained — that keeps it continuous so toggling
    /// bypass doesn't shift the playhead.
    bypass_delay: Vec<VecDeque<f32>>,

    /// Last codec we dispatched on, used to detect tab changes and reset
    /// the new codec's pipeline so stale audio in its output ring doesn't
    /// leak when the user comes back to it.
    last_dispatched_codec: Option<Codec>,
}

#[derive(Params)]
pub struct StreamingSimulatorParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    #[id = "codec"]
    pub codec: EnumParam<Codec>,

    /// Plugin-wide bypass. Routed through the same passthrough pipeline as
    /// a Lossless tier so reported latency stays constant and toggling
    /// doesn't trigger a host PDC re-tick.
    #[id = "bypass"]
    pub bypass: BoolParam,

    /// Bluetooth cascade enable. When on, audio runs through the selected
    /// `bluetooth_protocol` *after* the platform codec.
    #[id = "bluetooth_enabled"]
    pub bluetooth_enabled: BoolParam,

    #[id = "bluetooth_protocol"]
    pub bluetooth_protocol: EnumParam<BluetoothProtocol>,
}

impl Default for StreamingSimulator {
    fn default() -> Self {
        Self {
            params: Arc::new(StreamingSimulatorParams::default()),
            opus: None,
            vorbis: None,
            mp3: None,
            fm_radio: None,
            #[cfg(feature = "fdk-aac")]
            aac: None,
            #[cfg(feature = "fdk-aac")]
            heaac_v1: None,
            #[cfg(feature = "fdk-aac")]
            heaac: None,
            bluetooth: None,
            host_sample_rate: 44_100,
            host_channels: 2,
            host_max_block_size: 0,
            target_latency_samples: 0,
            bypass_delay: Vec::new(),
            last_dispatched_codec: None,
        }
    }
}

impl StreamingSimulator {
    /// Construct + initialise a processor on demand. Each `dispatch_buffer`
    /// match arm calls one of these so the slot fills on first use and the
    /// instance is reused thereafter.
    fn ensure_opus(&mut self) -> &mut OpusProcessor {
        self.opus.get_or_insert_with(|| {
            let mut p = OpusProcessor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    fn ensure_vorbis(&mut self) -> &mut VorbisProcessor {
        self.vorbis.get_or_insert_with(|| {
            let mut p = VorbisProcessor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    fn ensure_mp3(&mut self) -> &mut Mp3Processor {
        self.mp3.get_or_insert_with(|| {
            let mut p = Mp3Processor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    fn ensure_fm_radio(&mut self) -> &mut FmRadioProcessor {
        self.fm_radio.get_or_insert_with(|| {
            let mut p = FmRadioProcessor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            // FmRadio has zero inherent latency; pad to the global target
            // so codec→FM switches don't move PDC.
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    fn ensure_bluetooth(&mut self) -> &mut BluetoothProcessor {
        self.bluetooth.get_or_insert_with(|| {
            let mut p = BluetoothProcessor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p
        })
    }

    #[cfg(feature = "fdk-aac")]
    fn ensure_aac(&mut self) -> &mut AacProcessor {
        self.aac.get_or_insert_with(|| {
            let mut p = AacProcessor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    #[cfg(feature = "fdk-aac")]
    fn ensure_heaac_v1(&mut self) -> &mut HeAacV1Processor {
        self.heaac_v1.get_or_insert_with(|| {
            let mut p = HeAacV1Processor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }

    #[cfg(feature = "fdk-aac")]
    fn ensure_heaac_v2(&mut self) -> &mut HeAacV2Processor {
        self.heaac.get_or_insert_with(|| {
            let mut p = HeAacV2Processor::new();
            p.initialize(
                self.host_sample_rate,
                self.host_channels,
                self.host_max_block_size,
            );
            p.pad_output_to(self.target_latency_samples);
            p
        })
    }
}

impl Default for StreamingSimulatorParams {
    fn default() -> Self {
        Self {
            editor_state: editor::default_state(),
            codec: EnumParam::new("Codec", Codec::SpotifyHigh),
            bypass: BoolParam::new("Bypass", false),
            bluetooth_enabled: BoolParam::new("Bluetooth", false),
            bluetooth_protocol: EnumParam::new("BT Protocol", BluetoothProtocol::SbcHigh),
        }
    }
}

impl Plugin for StreamingSimulator {
    const NAME: &'static str = "Streaming Simulator";
    const VENDOR: &'static str = "Julien Meziere";
    const URL: &'static str = env!("CARGO_PKG_REPOSITORY");
    const EMAIL: &'static str = "julien@meziere.org";

    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;

    const SAMPLE_ACCURATE_AUTOMATION: bool = true;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.params.editor_state.clone())
    }

    fn initialize(
        &mut self,
        audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        context: &mut impl InitContext<Self>,
    ) -> bool {
        let sample_rate = buffer_config.sample_rate as u32;
        let max_block_size = buffer_config.max_buffer_size as usize;
        let channels = audio_io_layout
            .main_output_channels
            .map(|c| c.get() as usize)
            .unwrap_or(2);

        // Drop any processors held over from a previous host config; the
        // host_* fields below capture the new one for lazy re-init.
        self.opus = None;
        self.vorbis = None;
        self.mp3 = None;
        self.fm_radio = None;
        self.bluetooth = None;
        #[cfg(feature = "fdk-aac")]
        {
            self.aac = None;
            self.heaac_v1 = None;
            self.heaac = None;
        }

        self.host_sample_rate = sample_rate;
        self.host_channels = channels;
        self.host_max_block_size = max_block_size;

        // Worst-case latency across every codec family, computed without
        // instantiating any processors — `worst_case_latency_at` builds and
        // drops throwaway rubato probes to read `output_delay()`.
        #[cfg_attr(not(feature = "fdk-aac"), allow(unused_mut))]
        let mut target_latency = OpusProcessor::worst_case_latency_at(sample_rate, channels)
            .max(VorbisProcessor::worst_case_latency_at(sample_rate, channels))
            .max(Mp3Processor::worst_case_latency_at(sample_rate, channels))
            .max(FmRadioProcessor::worst_case_latency_at(sample_rate, channels));
        #[cfg(feature = "fdk-aac")]
        {
            target_latency = target_latency
                .max(AacProcessor::worst_case_latency_at(sample_rate, channels))
                .max(HeAacV1Processor::worst_case_latency_at(sample_rate, channels))
                .max(HeAacV2Processor::worst_case_latency_at(sample_rate, channels));
        }
        // BT cascades on top of the platform codec, so the strictly correct
        // budget would be `max_platform + bluetooth`. We use `.max()` instead
        // to avoid over-reporting on every BT-off session (the common case);
        // enabling BT may shift PDC by ~30-50 ms.
        target_latency = target_latency
            .max(BluetoothProcessor::worst_case_latency_at(sample_rate, channels));
        self.target_latency_samples = target_latency;

        // Pre-fill the dry-delay line so bypass output is `target_latency`
        // samples behind the input (matches every codec's PDC). Capacity
        // overshoots by one block to absorb push-then-drain churn.
        let target = target_latency as usize;
        self.bypass_delay = (0..channels)
            .map(|_| {
                let mut d = VecDeque::with_capacity(target + max_block_size + 1);
                d.extend(std::iter::repeat_n(0.0_f32, target));
                d
            })
            .collect();
        self.last_dispatched_codec = None;

        context.set_latency_samples(target_latency);
        true
    }

    fn reset(&mut self) {
        if let Some(p) = self.opus.as_mut() {
            p.reset();
        }
        if let Some(p) = self.vorbis.as_mut() {
            p.reset();
        }
        if let Some(p) = self.mp3.as_mut() {
            p.reset();
        }
        if let Some(p) = self.fm_radio.as_mut() {
            p.reset();
        }
        if let Some(p) = self.bluetooth.as_mut() {
            p.reset();
        }
        #[cfg(feature = "fdk-aac")]
        {
            if let Some(p) = self.aac.as_mut() {
                p.reset();
            }
            if let Some(p) = self.heaac_v1.as_mut() {
                p.reset();
            }
            if let Some(p) = self.heaac.as_mut() {
                p.reset();
            }
        }
        // Re-prime the dry delay line with silence so it stays the right
        // length after a host-initiated reset (transport stop, project load).
        let target = self.target_latency_samples as usize;
        for ch in 0..self.host_channels {
            if let Some(d) = self.bypass_delay.get_mut(ch) {
                d.clear();
                d.extend(std::iter::repeat_n(0.0_f32, target));
            }
        }
        self.last_dispatched_codec = None;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Dispatch lives in `dispatch_buffer` with explicit args so unit
        // tests can drive every codec path without faking `ProcessContext`
        // or going through nih-plug's `pub(crate)` param setters.
        let bypass = self.params.bypass.value();
        let codec = self.params.codec.value();
        let bt_enabled = self.params.bluetooth_enabled.value();
        let bt_protocol = self.params.bluetooth_protocol.value();
        self.dispatch_buffer(buffer, bypass, codec, bt_enabled, bt_protocol);
        ProcessStatus::Normal
    }
}

impl StreamingSimulator {
    /// Routes `buffer` through the right processor based on the explicit
    /// args. Called from `Plugin::process` (with values pulled from params)
    /// and from tests (with hand-picked values).
    ///
    /// Output flow per state:
    /// - `bypass = true`: dry delay only; no codec output, no BT cascade.
    /// - `bypass = false`, codec spec is `Bypass` (FLAC / ALAC tier): dry
    ///   delay → BT cascade (if enabled). Lets the user A/B "lossless +
    ///   Bluetooth" — the explicit point of those tiers when paired with BT.
    /// - `bypass = false`, codec spec runs: codec wet → BT cascade.
    ///
    /// The active codec runs *every* call regardless of bypass state, so its
    /// pipeline stays warm and toggling bypass off doesn't drop into a stale
    /// ring buffer. A codec change resets the new codec's pipeline so the
    /// user gets a clean start instead of leftover audio from last time.
    pub(crate) fn dispatch_buffer(
        &mut self,
        buffer: &mut Buffer,
        bypass: bool,
        codec: Codec,
        bt_enabled: bool,
        bt_protocol: BluetoothProtocol,
    ) {
        let n = buffer.samples();
        let channels = self.host_channels;
        let codec_spec = codec.def().spec;
        let codec_runs = !matches!(codec_spec, CodecSpec::Bypass);
        // Buffer should hold the dry-delay signal (rather than whatever the
        // codec wrote) when either the user toggled bypass or the codec
        // itself is a no-op tier.
        let use_dry_signal = bypass || !codec_runs;

        // Codec change → wipe the new codec's rings so stale audio from a
        // previous tab doesn't bleed in. Bypass tiers have no codec to reset.
        if self.last_dispatched_codec != Some(codec) {
            if codec_runs {
                self.reset_codec_for(codec_spec);
            }
            self.last_dispatched_codec = Some(codec);
        }

        // Snapshot input into the dry delay (always — keeps it continuous
        // for the moment bypass turns on).
        {
            let block = buffer.as_slice();
            for ch in 0..channels {
                self.bypass_delay[ch].reserve(n);
                self.bypass_delay[ch].extend(block[ch][..n].iter().copied());
            }
        }

        // Run the active codec even when bypassed so its pipeline stays warm.
        // Skipped only for `CodecSpec::Bypass`, which has no codec to run.
        if codec_runs {
            self.run_codec(buffer, codec_spec);
        }

        // Replace the buffer with the dry-delay output *before* the BT
        // cascade. That way Bluetooth sees the dry signal on Lossless tiers
        // (matches the "FLAC into Bluetooth headphones" mental model) and
        // the dry signal on plugin-wide bypass (where BT is suppressed
        // below anyway).
        let drained_for_dry = if use_dry_signal {
            let block = buffer.as_slice();
            for ch in 0..channels {
                let take = self.bypass_delay[ch].len().min(n);
                if take > 0 {
                    let head = self.bypass_delay[ch].make_contiguous();
                    block[ch][..take].copy_from_slice(&head[..take]);
                    self.bypass_delay[ch].drain(..take);
                }
                if take < n {
                    block[ch][take..n].fill(0.0);
                }
            }
            true
        } else {
            false
        };

        // BT cascade applies to whatever is in the buffer — codec wet on
        // normal tiers, dry signal on Lossless. Suppressed only by the
        // plugin-wide bypass switch (which means "no processing").
        if !bypass && bt_enabled {
            self.ensure_bluetooth().process(buffer, bt_protocol);
        }

        // Drain the delay line if we didn't already use it as buffer source,
        // so its length stays bounded at `target_latency_samples`.
        if !drained_for_dry {
            for ch in 0..channels {
                let drain = self.bypass_delay[ch].len().min(n);
                self.bypass_delay[ch].drain(..drain);
            }
        }
    }

    fn run_codec(&mut self, buffer: &mut Buffer, spec: CodecSpec) {
        match spec {
            CodecSpec::Bypass => {} // handled by dispatch_buffer
            CodecSpec::Opus { bitrate_kbps } => {
                self.ensure_opus()
                    .process(buffer, OpusMode::Opus { bitrate_kbps });
            }
            CodecSpec::Vorbis { bitrate_kbps } => {
                self.ensure_vorbis()
                    .process(buffer, VorbisMode::Vorbis { bitrate_kbps });
            }
            CodecSpec::AacLc { bitrate_kbps } => {
                #[cfg(feature = "fdk-aac")]
                self.ensure_aac().process(
                    buffer,
                    AacMode::AacLc {
                        bitrate_kbps,
                        mono: false,
                    },
                );
                // FDK absent → buffer is unchanged; the dry-delay path
                // above will deliver the input, so the user still hears audio.
                #[cfg(not(feature = "fdk-aac"))]
                let _ = bitrate_kbps;
            }
            CodecSpec::AacLcMono { bitrate_kbps } => {
                #[cfg(feature = "fdk-aac")]
                self.ensure_aac().process(
                    buffer,
                    AacMode::AacLc {
                        bitrate_kbps,
                        mono: true,
                    },
                );
                #[cfg(not(feature = "fdk-aac"))]
                let _ = bitrate_kbps;
            }
            CodecSpec::HeAacV1 { bitrate_kbps } => {
                #[cfg(feature = "fdk-aac")]
                self.ensure_heaac_v1()
                    .process(buffer, HeAacV1Mode::HeAacV1 { bitrate_kbps });
                #[cfg(not(feature = "fdk-aac"))]
                let _ = bitrate_kbps;
            }
            CodecSpec::HeAacV2 { bitrate_kbps } => {
                #[cfg(feature = "fdk-aac")]
                self.ensure_heaac_v2()
                    .process(buffer, HeAacV2Mode::HeAacV2 { bitrate_kbps });
                #[cfg(not(feature = "fdk-aac"))]
                let _ = bitrate_kbps;
            }
            CodecSpec::Mp3 { bitrate_kbps } => {
                self.ensure_mp3()
                    .process(buffer, Mp3Mode::Mp3 { bitrate_kbps });
            }
            CodecSpec::FmRadio { variant } => {
                self.ensure_fm_radio()
                    .process(buffer, FmRadioMode::FmRadio { variant });
            }
        }
    }

    fn reset_codec_for(&mut self, spec: CodecSpec) {
        match spec {
            CodecSpec::Bypass => {}
            CodecSpec::Opus { .. } => {
                if let Some(p) = self.opus.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::Vorbis { .. } => {
                if let Some(p) = self.vorbis.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::Mp3 { .. } => {
                if let Some(p) = self.mp3.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::FmRadio { .. } => {
                if let Some(p) = self.fm_radio.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::AacLc { .. } | CodecSpec::AacLcMono { .. } => {
                #[cfg(feature = "fdk-aac")]
                if let Some(p) = self.aac.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::HeAacV1 { .. } => {
                #[cfg(feature = "fdk-aac")]
                if let Some(p) = self.heaac_v1.as_mut() {
                    p.reset();
                }
            }
            CodecSpec::HeAacV2 { .. } => {
                #[cfg(feature = "fdk-aac")]
                if let Some(p) = self.heaac.as_mut() {
                    p.reset();
                }
            }
        }
    }
}

impl ClapPlugin for StreamingSimulator {
    // Permanent identity for hosts loading saved sessions — never change
    // post-release or DAWs will fail to recognise existing project files.
    const CLAP_ID: &'static str = "io.github.JulienMeziere.streaming-simulator";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Emulates streaming platform encoding artifacts");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;

    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Mastering,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for StreamingSimulator {
    // Random UUIDv4 (8ed66cd1-8535-4a73-92a4-a5dec1db58d5) — the plugin's
    // permanent identity. Never change across versions or hosts will treat
    // it as a brand-new plugin and drop saved sessions.
    const VST3_CLASS_ID: [u8; 16] = [
        0x8e, 0xd6, 0x6c, 0xd1, 0x85, 0x35, 0x4a, 0x73, 0x92, 0xa4, 0xa5, 0xde, 0xc1, 0xdb, 0x58,
        0xd5,
    ];

    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Mastering,
        Vst3SubCategory::Stereo,
    ];
}

nih_export_clap!(StreamingSimulator);
nih_export_vst3!(StreamingSimulator);

#[cfg(test)]
mod tests {
    //! Coverage for lazy-init plumbing, `dispatch_buffer` routing, and the
    //! latency / reset / param glue that doesn't fit inside per-codec tests.
    use super::*;
    use crate::test_helpers::{drive_with_sine_and_measure_buffer, peak, with_buffer};

    fn fresh_sim(host_rate: u32, channels: usize, max_block: usize) -> StreamingSimulator {
        let mut sim = StreamingSimulator::default();
        let layout = AudioIOLayout::default();
        let buffer_config = BufferConfig {
            sample_rate: host_rate as f32,
            min_buffer_size: None,
            max_buffer_size: max_block as u32,
            process_mode: ProcessMode::Realtime,
        };
        let mut init_ctx = test_init_context::TestInitContext;
        let _ = sim.initialize(&layout, &buffer_config, &mut init_ctx);
        // `initialize` reads channels from `layout`, not directly. Override
        // here so codec dispatch matches the buffer the test actually uses.
        sim.host_channels = channels;
        sim
    }

    mod test_init_context {
        use super::*;
        pub struct TestInitContext;
        impl InitContext<StreamingSimulator> for TestInitContext {
            fn plugin_api(&self) -> PluginApi {
                PluginApi::Clap
            }
            fn execute(&self, _task: <StreamingSimulator as Plugin>::BackgroundTask) {}
            fn set_latency_samples(&self, _samples: u32) {}
            fn set_current_voice_capacity(&self, _capacity: u32) {}
        }
    }

    // ── default + lazy-init invariants ─────────────────────────────

    #[test]
    fn default_state_is_lazy() {
        let sim = StreamingSimulator::default();
        assert!(sim.opus.is_none());
        assert!(sim.vorbis.is_none());
        assert!(sim.mp3.is_none());
        assert!(sim.fm_radio.is_none());
        assert!(sim.bluetooth.is_none());
        #[cfg(feature = "fdk-aac")]
        {
            assert!(sim.aac.is_none());
            assert!(sim.heaac_v1.is_none());
            assert!(sim.heaac.is_none());
        }
    }

    #[test]
    fn initialize_caches_host_config() {
        let sim = fresh_sim(48_000, 2, 256);
        assert_eq!(sim.host_sample_rate, 48_000);
        assert_eq!(sim.host_channels, 2);
        assert_eq!(sim.host_max_block_size, 256);
        assert!(
            sim.target_latency_samples > 0,
            "target_latency_samples should be non-zero after initialize"
        );
    }

    #[test]
    fn target_latency_at_least_each_codec_worst_case() {
        let sim = fresh_sim(48_000, 2, 256);
        assert!(
            sim.target_latency_samples
                >= OpusProcessor::worst_case_latency_at(48_000, 2)
        );
        assert!(
            sim.target_latency_samples
                >= VorbisProcessor::worst_case_latency_at(48_000, 2)
        );
        assert!(
            sim.target_latency_samples
                >= Mp3Processor::worst_case_latency_at(48_000, 2)
        );
        assert!(
            sim.target_latency_samples
                >= FmRadioProcessor::worst_case_latency_at(48_000, 2)
        );
        assert!(
            sim.target_latency_samples
                >= BluetoothProcessor::worst_case_latency_at(48_000, 2)
        );
    }

    #[test]
    fn reset_on_uninitialized_is_noop() {
        let mut sim = StreamingSimulator::default();
        sim.reset();
    }

    // ── ensure_* idempotency ───────────────────────────────────────

    #[test]
    fn ensure_opus_is_idempotent() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let p1 = sim.ensure_opus() as *mut OpusProcessor;
        let p2 = sim.ensure_opus() as *mut OpusProcessor;
        assert_eq!(
            p1, p2,
            "ensure_opus must hand back the same processor instance"
        );
    }

    #[test]
    fn ensure_helpers_are_lazy() {
        let mut sim = fresh_sim(48_000, 2, 256);
        assert!(sim.opus.is_none());
        sim.ensure_opus();
        assert!(sim.opus.is_some());
        assert!(sim.vorbis.is_none());
        assert!(sim.mp3.is_none());
        assert!(sim.fm_radio.is_none());
    }

    // ── dispatch_buffer routing per CodecSpec ─────────────────────

    /// Run a sine through `dispatch_buffer` with the given codec and return
    /// the post-warmup peak.
    fn dispatch_peak(codec: Codec, host_rate: u32) -> f32 {
        let mut sim = fresh_sim(host_rate, 2, 256);
        drive_with_sine_and_measure_buffer(
            host_rate,
            256,
            3.0,
            0.5,
            440.0,
            0.5,
            |buf| {
                sim.dispatch_buffer(
                    buf,
                    false,
                    codec,
                    false,
                    BluetoothProtocol::SbcHigh,
                );
            },
        )
    }

    #[test]
    fn dispatch_bypass_codec_passes_audio() {
        let p = dispatch_peak(Codec::SpotifyLossless, 48_000);
        assert!(p > 0.05, "Bypass dispatch produced near-silent output ({p:.3})");
    }

    #[test]
    fn dispatch_opus_codec_passes_audio() {
        let p = dispatch_peak(Codec::YtMusicLowWeb, 48_000);
        assert!(p > 0.05, "Opus dispatch produced near-silent output ({p:.3})");
    }

    #[test]
    fn dispatch_vorbis_codec_passes_audio() {
        let p = dispatch_peak(Codec::SpotifyHigh, 48_000);
        assert!(p > 0.05, "Vorbis dispatch produced near-silent output ({p:.3})");
    }

    #[test]
    fn dispatch_mp3_codec_passes_audio() {
        let p = dispatch_peak(Codec::DeezerStandard, 48_000);
        assert!(p > 0.05, "MP3 dispatch produced near-silent output ({p:.3})");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn dispatch_aac_codec_passes_audio() {
        let p = dispatch_peak(Codec::TidalLow, 48_000);
        assert!(p > 0.05, "AAC dispatch produced near-silent output ({p:.3})");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn dispatch_aac_mono_codec_passes_audio() {
        let mut found = None;
        for c_idx in 0..<Codec as Enum>::variants().len() {
            let c = Codec::from_index(c_idx);
            if matches!(c.def().spec, CodecSpec::AacLcMono { .. }) {
                found = Some(c);
                break;
            }
        }
        let codec = found.expect("at least one AacLcMono tier in the catalog");
        let p = dispatch_peak(codec, 48_000);
        assert!(p > 0.05, "AacLcMono dispatch produced near-silent output ({p:.3})");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn dispatch_he_aac_v1_codec_passes_audio() {
        let p = dispatch_peak(Codec::AppleMusicHighEfficiency, 48_000);
        assert!(p > 0.05, "HE-AAC v1 dispatch produced near-silent output ({p:.3})");
    }

    #[cfg(feature = "fdk-aac")]
    #[test]
    fn dispatch_he_aac_v2_codec_passes_audio() {
        let p = dispatch_peak(Codec::SpotifyLow, 48_000);
        assert!(p > 0.05, "HE-AAC v2 dispatch produced near-silent output ({p:.3})");
    }

    #[test]
    fn dispatch_fm_radio_codec_passes_audio() {
        let mut found = None;
        for c_idx in 0..<Codec as Enum>::variants().len() {
            let c = Codec::from_index(c_idx);
            if matches!(c.def().spec, CodecSpec::FmRadio { .. }) {
                found = Some(c);
                break;
            }
        }
        let codec = found.expect("at least one FmRadio tier in the catalog");
        let p = dispatch_peak(codec, 48_000);
        assert!(p > 0.05, "FmRadio dispatch produced near-silent output ({p:.3})");
    }

    // ── Bypass param + Bluetooth cascade gating ───────────────────

    /// Lossless tier + BT enabled must build the BT processor (i.e. the
    /// cascade actually runs). Regression for the bug where `CodecSpec::Bypass`
    /// suppressed the BT layer the same way plugin-wide bypass does.
    #[test]
    fn bluetooth_runs_on_top_of_lossless_tier() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        with_buffer(&mut planar, 256, |buf| {
            sim.dispatch_buffer(
                buf,
                false,
                Codec::SpotifyLossless, // CodecSpec::Bypass
                true,
                BluetoothProtocol::SbcHigh,
            );
        });
        assert!(
            sim.bluetooth.is_some(),
            "BT processor must be built when Lossless + BT-enabled — \
             user expects FLAC-into-Bluetooth simulation to actually engage"
        );
    }

    /// Plugin-wide bypass *does* suppress BT (bypass means "no processing").
    #[test]
    fn plugin_bypass_suppresses_bluetooth() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        with_buffer(&mut planar, 256, |buf| {
            sim.dispatch_buffer(
                buf,
                true, // plugin-wide bypass
                Codec::SpotifyHigh,
                true, // BT toggled on, but bypass overrides
                BluetoothProtocol::SbcHigh,
            );
        });
        assert!(
            sim.bluetooth.is_none(),
            "BT processor must not run when plugin-wide bypass is active"
        );
    }

    #[test]
    fn bypass_routes_audio_through_passthrough() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let p = drive_with_sine_and_measure_buffer(
            48_000,
            256,
            3.0,
            0.5,
            440.0,
            0.5,
            |buf| {
                sim.dispatch_buffer(
                    buf,
                    true,
                    Codec::SpotifyHigh,
                    false,
                    BluetoothProtocol::SbcHigh,
                );
            },
        );
        assert!(p > 0.05, "bypass route produced near-silent output ({p:.3})");
    }

    #[test]
    fn bluetooth_cascade_runs_when_enabled() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let bt_off_peak = drive_with_sine_and_measure_buffer(
            48_000,
            256,
            2.0,
            0.5,
            440.0,
            0.5,
            |buf| {
                sim.dispatch_buffer(
                    buf,
                    false,
                    Codec::SpotifyHigh,
                    false,
                    BluetoothProtocol::SbcLow,
                );
            },
        );
        assert!(
            sim.bluetooth.is_none(),
            "BT processor must not be built when bt_enabled=false"
        );

        let mut sim = fresh_sim(48_000, 2, 256);
        let bt_on_peak = drive_with_sine_and_measure_buffer(
            48_000,
            256,
            2.0,
            0.5,
            440.0,
            0.5,
            |buf| {
                sim.dispatch_buffer(
                    buf,
                    false,
                    Codec::SpotifyHigh,
                    true,
                    BluetoothProtocol::SbcLow,
                );
            },
        );
        assert!(
            sim.bluetooth.is_some(),
            "BT processor must be built when bt_enabled=true"
        );
        assert!(bt_off_peak > 0.05, "BT-off peak {bt_off_peak:.3} too low");
        assert!(bt_on_peak > 0.05, "BT-on peak {bt_on_peak:.3} too low");
    }

    #[test]
    fn every_bluetooth_protocol_dispatches_without_panic() {
        let mut sim = fresh_sim(48_000, 2, 256);
        for &proto in &[
            BluetoothProtocol::SbcLow,
            BluetoothProtocol::SbcHigh,
            BluetoothProtocol::Aac128,
            BluetoothProtocol::Aac256,
            BluetoothProtocol::Lc3_64,
            BluetoothProtocol::Lc3_160,
        ] {
            let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
            with_buffer(&mut planar, 256, |buf| {
                sim.dispatch_buffer(
                    buf,
                    false,
                    Codec::SpotifyHigh,
                    true,
                    proto,
                );
            });
            // First block is allowed to be silent (warm-up); the test only
            // asserts that dispatch didn't panic.
            let _ = peak(&planar);
        }
    }

    // ── reset clears live processor state ─────────────────────────

    #[test]
    fn reset_after_dispatch_does_not_panic() {
        let mut sim = fresh_sim(48_000, 2, 256);
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        with_buffer(&mut planar, 256, |buf| {
            sim.dispatch_buffer(
                buf,
                false,
                Codec::SpotifyHigh,
                false,
                BluetoothProtocol::SbcHigh,
            );
        });
        assert!(sim.vorbis.is_some(), "vorbis should be built after dispatch");
        sim.reset();
    }
}
