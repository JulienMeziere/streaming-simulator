//! AAC over Bluetooth A2DP — plain AAC-LC at 128 or 256 kbps.
//!
//! Wraps a *separate* [`AacProcessor`] instance (not shared with the
//! platform AAC) so the platform and BT paths can run different bitrates
//! without sharing FDK-AAC encoder state.

use crate::processor::aac::{AacMode, AacProcessor};
use nih_plug::buffer::Buffer;

/// AAC-LC at fixed BT operating points.
pub struct AacBtCodec {
    inner: AacProcessor,
    bitrate_kbps: u32,
}

impl AacBtCodec {
    /// `bitrate_kbps` is 128 or 256 — the two BT presets.
    pub fn new(
        sample_rate: u32,
        channels: usize,
        max_block_size: usize,
        bitrate_kbps: u32,
    ) -> Self {
        let mut inner = AacProcessor::new();
        inner.initialize(sample_rate, channels, max_block_size);
        Self {
            inner,
            bitrate_kbps,
        }
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    pub fn process(&mut self, buffer: &mut Buffer) {
        self.inner.process(
            buffer,
            AacMode::AacLc {
                bitrate_kbps: self.bitrate_kbps,
                mono: false,
            },
        );
    }

    /// AAC-LC latency is bitrate-independent (same frame size + warm-up).
    pub fn worst_case_latency_at(host_rate: u32, channels: usize) -> u32 {
        AacProcessor::worst_case_latency_at(host_rate, channels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sine through, peak nonzero — we don't assert closeness because AAC
    /// at 128 kbps mangles enough to make strict comparisons flaky.
    #[test]
    fn aac_bt_roundtrip_emits_audio_at_every_host_rate() {
        for &host_rate in &[44_100u32, 48_000, 96_000] {
            for &bitrate in &[128u32, 256] {
                let mut codec = AacBtCodec::new(host_rate, 2, 256, bitrate);
                let peak = crate::test_helpers::drive_with_sine_and_measure_buffer(
                    host_rate,
                    256,
                    2.0,
                    0.5,
                    1_000.0,
                    0.3,
                    |buf| codec.process(buf),
                );
                assert!(
                    peak > 0.05,
                    "AAC-BT at {host_rate} Hz / {bitrate} kbps produced \
                     near-silent output ({peak:.3})"
                );
            }
        }
    }

    #[test]
    fn worst_case_latency_at_is_positive_for_every_supported_rate() {
        for &rate in &[44_100u32, 48_000, 96_000] {
            let l = AacBtCodec::worst_case_latency_at(rate, 2);
            assert!(l > 0, "worst_case_latency_at({rate}) returned 0");
        }
    }

    #[test]
    fn reset_does_not_panic() {
        let mut codec = AacBtCodec::new(48_000, 2, 256, 256);
        codec.reset();
    }
}
