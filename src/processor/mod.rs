//! Real-time codec processors.
//!
//! Each codec family has its own processor with its own ring-buffer
//! pipeline. `Plugin::process` dispatches to the right one based on the
//! active `CodecSpec`. Switching codec families (e.g., Spotify Vorbis →
//! YouTube Opus) swaps audio pipelines and is **not** glitch-free; switching
//! within a family (e.g., between Opus bitrates) stays on the same pipeline.
//!
//! ### Latency strategy
//!
//! Each processor exposes a static `natural_latency(host_rate)` that returns
//! the worst-case pipeline delay (in host samples) for the given sample
//! rate. `Plugin::initialize` queries both, takes the max, and configures
//! both processors to pre-fill their output ring with that many silent
//! samples — so both pipelines have the same effective delay, codec
//! switches don't trigger PDC re-ticks, and we never report more latency
//! than the slowest path actually needs at the current sample rate.
//!
//! At 44.1 kHz (the most common host rate for music) this is ~50 ms;
//! at higher rates where Vorbis has to resample, it grows to ~100 ms.

pub mod biquad;
pub mod bluetooth;
pub mod fm_mpx;
pub mod fm_multiband;
pub mod fm_radio;
pub mod mp3;
pub mod opus;
pub mod pipeline;
pub mod vorbis;

#[cfg(feature = "fdk-aac")]
pub mod aac;
#[cfg(feature = "fdk-aac")]
pub mod heaac;
#[cfg(feature = "fdk-aac")]
pub mod heaacv2;
