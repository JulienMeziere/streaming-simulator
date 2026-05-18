//! Bluetooth codec simulation — runs *after* the platform codec to model
//! the real listening chain (streaming → device → BT → headphones).
//!
//! ```text
//!   master → platform codec → BluetoothProcessor → out
//! ```
//!
//! Three FOSS codecs cover the audible BT landscape:
//! - **SBC** — universal A2DP baseline (cheap earbuds default).
//! - **AAC over A2DP** — iPhone / AirPods default; uses our FDK-AAC.
//! - **LC3** — Bluetooth LE Audio (5.2+) standard.
//!
//! aptX family (Qualcomm-proprietary), LDAC (closed-source decoder),
//! LHDC / LLAC / Samsung Scalable, and LC3plus (Fraunhofer-patented) are
//! deliberately skipped — see docs/codecs.md.

use nih_plug::prelude::*;

/// 6 presets covering the audibly meaningful range (cheap earbuds →
/// LE Audio high quality). `Lc3_64` / `Lc3_160` use trailing underscores
/// because Rust identifiers can't start with a digit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Enum)]
pub enum BluetoothProtocol {
    /// SBC bitpool 19 (~127 kbps) — cheap earbuds, audible HF loss.
    #[id = "sbc-low"]
    #[name = "SBC · Low"]
    SbcLow,
    /// SBC bitpool 53 (~328 kbps) — default for most modern BT headphones.
    #[id = "sbc-high"]
    #[name = "SBC · High"]
    SbcHigh,
    /// AAC-LC 128 kbps — the "Android AAC bug" tier (worse than SBC at the
    /// same bitrate on old encoders).
    #[id = "aac-128"]
    #[name = "AAC · 128 kbps"]
    Aac128,
    /// AAC-LC 256 kbps — iPhone + AirPods default.
    #[id = "aac-256"]
    #[name = "AAC · 256 kbps"]
    Aac256,
    /// LC3 mono 64 kbps — LE Audio low-power preset.
    #[id = "lc3-64"]
    #[name = "LC3 · 64 kbps"]
    Lc3_64,
    /// LC3 stereo 160 kbps — LE Audio high-quality preset.
    #[id = "lc3-160"]
    #[name = "LC3 · 160 kbps"]
    Lc3_160,
}

impl BluetoothProtocol {
    /// Display label for the gear popup. Centralised so the editor doesn't
    /// have to go through `Enum`-trait methods or duplicate the strings.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::SbcLow => "SBC · Low",
            Self::SbcHigh => "SBC · High",
            Self::Aac128 => "AAC · 128 kbps",
            Self::Aac256 => "AAC · 256 kbps",
            Self::Lc3_64 => "LC3 · 64 kbps",
            Self::Lc3_160 => "LC3 · 160 kbps",
        }
    }
}

// ─── SBC codec ──────────────────────────────────────────────────────
mod sbc;
pub use sbc::SbcCodec;

// ─── LC3 codec ──────────────────────────────────────────────────────
mod lc3;
pub use lc3::Lc3Codec;

// ─── AAC-BT codec ───────────────────────────────────────────────────
#[cfg(feature = "fdk-aac")]
mod aac_bt;
#[cfg(feature = "fdk-aac")]
pub use aac_bt::AacBtCodec;

// ─── Orchestrator ───────────────────────────────────────────────────

/// Cascade processor — one lazy slot per backend, dispatched per block
/// based on the active [`BluetoothProtocol`].
pub struct BluetoothProcessor {
    sbc_low: Option<SbcCodec>,
    sbc_high: Option<SbcCodec>,
    lc3_64: Option<Lc3Codec>,
    lc3_160: Option<Lc3Codec>,
    #[cfg(feature = "fdk-aac")]
    aac_128: Option<AacBtCodec>,
    #[cfg(feature = "fdk-aac")]
    aac_256: Option<AacBtCodec>,

    sample_rate: u32,
    channels: usize,
    max_block_size: usize,
    ready: bool,
}

impl BluetoothProcessor {
    pub fn new() -> Self {
        Self {
            sbc_low: None,
            sbc_high: None,
            lc3_64: None,
            lc3_160: None,
            #[cfg(feature = "fdk-aac")]
            aac_128: None,
            #[cfg(feature = "fdk-aac")]
            aac_256: None,
            sample_rate: 44_100,
            channels: 2,
            max_block_size: 0,
            ready: false,
        }
    }

    pub fn initialize(&mut self, sample_rate: u32, channels: usize, max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.max_block_size = max_block_size;
        // Drop any state sized for a previous host config.
        self.sbc_low = None;
        self.sbc_high = None;
        self.lc3_64 = None;
        self.lc3_160 = None;
        #[cfg(feature = "fdk-aac")]
        {
            self.aac_128 = None;
            self.aac_256 = None;
        }
        self.ready = matches!(channels, 1 | 2);
    }

    pub fn reset(&mut self) {
        if !self.ready {
            return;
        }
        if let Some(c) = self.sbc_low.as_mut() {
            c.reset();
        }
        if let Some(c) = self.sbc_high.as_mut() {
            c.reset();
        }
        if let Some(c) = self.lc3_64.as_mut() {
            c.reset();
        }
        if let Some(c) = self.lc3_160.as_mut() {
            c.reset();
        }
        #[cfg(feature = "fdk-aac")]
        {
            if let Some(c) = self.aac_128.as_mut() {
                c.reset();
            }
            if let Some(c) = self.aac_256.as_mut() {
                c.reset();
            }
        }
    }

    /// In-place encode → decode through the selected protocol. `buffer`
    /// arrives as the platform-codec output and leaves as the cascaded
    /// output.
    pub fn process(&mut self, buffer: &mut nih_plug::buffer::Buffer, protocol: BluetoothProtocol) {
        if !self.ready {
            return;
        }
        match protocol {
            BluetoothProtocol::SbcLow => {
                let codec = self.sbc_low.get_or_insert_with(|| {
                    SbcCodec::new(
                        self.sample_rate,
                        self.channels,
                        self.max_block_size,
                        sbc::SbcQuality::Low,
                    )
                });
                codec.process(buffer);
            }
            BluetoothProtocol::SbcHigh => {
                let codec = self.sbc_high.get_or_insert_with(|| {
                    SbcCodec::new(
                        self.sample_rate,
                        self.channels,
                        self.max_block_size,
                        sbc::SbcQuality::High,
                    )
                });
                codec.process(buffer);
            }
            BluetoothProtocol::Lc3_64 => {
                let codec = self.lc3_64.get_or_insert_with(|| {
                    Lc3Codec::new(
                        self.sample_rate,
                        self.channels,
                        self.max_block_size,
                        lc3::Lc3Quality::Low64,
                    )
                });
                codec.process(buffer);
            }
            BluetoothProtocol::Lc3_160 => {
                let codec = self.lc3_160.get_or_insert_with(|| {
                    Lc3Codec::new(
                        self.sample_rate,
                        self.channels,
                        self.max_block_size,
                        lc3::Lc3Quality::High160,
                    )
                });
                codec.process(buffer);
            }
            BluetoothProtocol::Aac128 => {
                #[cfg(feature = "fdk-aac")]
                {
                    let codec = self.aac_128.get_or_insert_with(|| {
                        AacBtCodec::new(
                            self.sample_rate,
                            self.channels,
                            self.max_block_size,
                            128,
                        )
                    });
                    codec.process(buffer);
                }
                // Without FDK-AAC the AAC presets are silent pass-through.
                // The UI shouldn't expose them in that build, but be defensive.
            }
            BluetoothProtocol::Aac256 => {
                #[cfg(feature = "fdk-aac")]
                {
                    let codec = self.aac_256.get_or_insert_with(|| {
                        AacBtCodec::new(
                            self.sample_rate,
                            self.channels,
                            self.max_block_size,
                            256,
                        )
                    });
                    codec.process(buffer);
                }
            }
        }
    }

    /// Worst-case host-rate latency across all 6 presets so PDC covers
    /// any preset switch without re-ticking.
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        let sbc_l = SbcCodec::worst_case_latency_at(host_rate, channels);
        let lc3_l = Lc3Codec::worst_case_latency_at(host_rate, channels);
        let aac_l = {
            #[cfg(feature = "fdk-aac")]
            {
                AacBtCodec::worst_case_latency_at(host_rate, channels)
            }
            #[cfg(not(feature = "fdk-aac"))]
            {
                0u32
            }
        };
        sbc_l.max(lc3_l).max(aac_l)
    }
}

impl Default for BluetoothProcessor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test for lazy-init dispatch — every preset must emit audio at
    /// every common host rate.
    #[test]
    fn orchestrator_emits_audio_for_every_preset() {
        let host_rates: &[u32] = &[44_100, 48_000, 96_000];
        // AAC variants need `fdk-aac`; skip them when the feature is off.
        let presets: &[BluetoothProtocol] = &[
            BluetoothProtocol::SbcLow,
            BluetoothProtocol::SbcHigh,
            #[cfg(feature = "fdk-aac")]
            BluetoothProtocol::Aac128,
            #[cfg(feature = "fdk-aac")]
            BluetoothProtocol::Aac256,
            BluetoothProtocol::Lc3_64,
            BluetoothProtocol::Lc3_160,
        ];
        for &host_rate in host_rates {
            for &preset in presets {
                let mut bt = BluetoothProcessor::new();
                bt.initialize(host_rate, 2, 256);
                let peak = crate::test_helpers::drive_with_sine_and_measure_buffer(
                    host_rate,
                    256,
                    2.0,
                    0.5,
                    1_000.0,
                    0.3,
                    |buf| bt.process(buf, preset),
                );
                assert!(
                    peak > 0.05,
                    "BT orchestrator at {host_rate} Hz / {preset:?} produced \
                     near-silent output ({peak:.3})"
                );
            }
        }
    }

    /// `initialize` alone must not allocate any codec backend — that
    /// only happens on the first matching `process` call.
    #[test]
    fn orchestrator_is_lazy_until_first_dispatch() {
        let mut bt = BluetoothProcessor::new();
        bt.initialize(44_100, 2, 256);
        assert!(bt.sbc_low.is_none());
        assert!(bt.sbc_high.is_none());
        assert!(bt.lc3_64.is_none());
        assert!(bt.lc3_160.is_none());
        #[cfg(feature = "fdk-aac")]
        {
            assert!(bt.aac_128.is_none());
            assert!(bt.aac_256.is_none());
        }
    }

    /// Touched codec slots stay warm so back-and-forth A/B is glitch-free.
    #[test]
    fn orchestrator_caches_warmed_codecs() {
        let mut bt = BluetoothProcessor::new();
        bt.initialize(44_100, 2, 256);
        let block_size = 128;
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; block_size]; 2];
        for preset in [
            BluetoothProtocol::SbcLow,
            BluetoothProtocol::SbcHigh,
            BluetoothProtocol::Lc3_64,
        ] {
            crate::test_helpers::with_buffer(&mut planar, block_size, |buf| {
                bt.process(buf, preset);
            });
        }
        assert!(bt.sbc_low.is_some());
        assert!(bt.sbc_high.is_some());
        assert!(bt.lc3_64.is_some());
        assert!(bt.lc3_160.is_none(), "untouched slot must stay None");
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 96_000] {
            let l = BluetoothProcessor::worst_case_latency_at(rate, 2);
            assert!(l > 0, "BT worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn default_constructor_matches_new() {
        let from_new = BluetoothProcessor::new();
        let from_default = BluetoothProcessor::default();
        assert_eq!(from_new.sample_rate, from_default.sample_rate);
        assert_eq!(from_new.channels, from_default.channels);
        assert_eq!(from_new.ready, from_default.ready);
    }

    #[test]
    fn reset_before_initialize_is_noop() {
        let mut bt = BluetoothProcessor::new();
        bt.reset();
        assert!(!bt.ready);
    }

    /// `reset` clears codec state but keeps slots allocated so the next
    /// dispatch doesn't have to rebuild.
    #[test]
    fn reset_after_dispatch_clears_codec_state() {
        let mut bt = BluetoothProcessor::new();
        bt.initialize(48_000, 2, 256);
        let mut planar: Vec<Vec<f32>> = vec![vec![0.0; 256]; 2];
        crate::test_helpers::with_buffer(&mut planar, 256, |buf| {
            bt.process(buf, BluetoothProtocol::SbcLow);
        });
        assert!(bt.sbc_low.is_some());
        bt.reset();
        assert!(bt.sbc_low.is_some());
    }

    #[test]
    fn initialize_with_unsupported_channel_count_marks_not_ready() {
        let mut bt = BluetoothProcessor::new();
        bt.initialize(48_000, 7, 256);
        assert!(!bt.ready);
    }
}
