# Codec implementation notes

How the plugin actually implements each codec. Companion to
[`codecs.md`](codecs.md), which lists what each *platform* uses — this
file lists what *we're* doing about it, with a focus on the non-obvious
gotchas you can't find by reading just the source.

| Codec               | Library                                       | Used by                                                           |
| ------------------- | --------------------------------------------- | ----------------------------------------------------------------- |
| Opus                | `opus` (libopus FFI)                          | YouTube Music web, YouTube video web, Amazon Music SD             |
| Vorbis              | `aotuv_lancer_vorbis_sys` (raw libvorbis FFI) | Spotify Normal / High / Very High                                 |
| MP3                 | `mp3lame-encoder` + `minimp3-sys`             | Deezer (all tiers), SoundCloud legacy                             |
| AAC-LC              | `fdk-aac` (feature-gated)                     | Spotify web, YouTube Music mobile, Tidal, SoundCloud, YouTube video, TikTok, Instagram, Bluetooth AAC |
| HE-AAC v1           | `fdk-aac` (feature-gated)                     | Apple Music High Efficiency                                       |
| HE-AAC v2           | `fdk-aac` (feature-gated)                     | Spotify Low                                                       |
| SBC (Bluetooth)     | `libsbc-sys` (BlueZ libsbc FFI, vendored)     | Bluetooth A2DP universal baseline                                 |
| LC3 (Bluetooth)     | `lc3-codec` (pure Rust)                       | Bluetooth LE Audio                                                |
| FM Radio            | in-house DSP (no external codec)              | FM broadcast-chain simulation                                     |
| Bypass              | n/a                                           | Every "Lossless" tier; FLAC; ALAC; pass-through utility           |

All codecs share the same `ResampledPipeline` scaffolding in
[`src/processor/pipeline.rs`](../src/processor/pipeline.rs): host-rate
input ring → resample to codec rate → encode → decode → resample back
→ host-rate output ring. Latency is reported per-codec via
`worst_case_latency_at(host_rate, channels)` and aligned to the
plugin-wide max in `lib.rs::initialize` so codec switches don't shift
PDC.

---

## Vorbis

Direct FFI to libvorbis (with **aoTuV** quality patches), **without**
the Ogg container. We work packet-by-packet:

```text
input → vorbis_analysis_buffer / vorbis_analysis_wrote
      → vorbis_analysis_blockout / vorbis_analysis
      → vorbis_bitrate_addblock / vorbis_bitrate_flushpacket  (raw packet, no Ogg)
      → vorbis_synthesis / vorbis_synthesis_blockin
      → vorbis_synthesis_pcmout / vorbis_synthesis_read
      → output
```

The audible result is bit-identical to "real" Spotify Vorbis at the same
bitrate — Ogg is purely a packaging layer, it doesn't transform audio.

### `-q` mapping (Spotify-verified)

We run libvorbis in **VBR quality mode** (`vorbis_encode_init_vbr`),
*not* bitrate-managed mode. This is what Spotify itself does — verified
by [SoundExpert's 2010 reverse-engineering of the stream][se].

| Spotify tier | Nominal bitrate | libvorbis quality |
| ------------ | --------------- | ----------------- |
| Normal       | ~96 kbps        | `-q2` (0.2)       |
| High         | ~160 kbps       | `-q5` (0.5)       |
| Very High    | ~320 kbps       | `-q9` (0.9)       |

Quality mode is also what aoTuV's psychoacoustic tunings are calibrated
for — bitrate-managed mode would short-circuit the very tunings that
make aoTuV worth using.

[se]: https://soundexpert.org/articles/-/blogs/11910

### Warm-up / pre-roll — must handle

Vorbis is MDCT-based with 50% overlap. The first ~1024 samples out of
the decoder are **garbage** (overlap-add filter warm-up). Without Ogg
we don't get granule positions to tell us this; we track it manually
(see `samples_to_discard` in `vorbis.rs`). Forgetting this gives a
click/pop the first time the user enables a Vorbis tier.

### Why not `vorbis_rs`

`vorbis_rs` is Ogg-stream-only and its decoder takes a `std::io::Read`.
The contract for `Read::read` says `Ok(0)` means EOF — there's no
"WouldBlock"-style status. In a real-time loop where we're encoding
and decoding tiny DAW blocks, the decoder reads, sees 0 bytes,
concludes the stream is done, and returns `Ok(None)` permanently.
Recovery requires recreating the decoder. Raw FFI sidesteps the whole
issue.

---

## Opus

Standard libopus FFI. Frames in, frames out. We use
`OPUS_APPLICATION_AUDIO` (high-quality music mode) and run libopus at
its native 48 kHz internal rate — at 44.1 kHz hosts, libopus
auto-resamples internally, adding a few samples of latency.

Used as the first codec the plugin ever wired because it has the
lowest-friction API in the catalog: validates the whole audio-thread
architecture before tackling Vorbis's Ogg / FFI complications.

---

## MP3

LAME 3.100 (`mp3lame-encoder`) for encode; raw FFI to `minimp3-sys` for
decode. No `-D__LP64__=1` cross-compile workaround like FDK-AAC needs;
the SSP stubs from `cross/ssp_stubs.c` are the only mingw-w64 tweak.

### Why `minimp3-sys` directly, not the high-level `minimp3` wrapper

MP3 has a **bit reservoir**: the encoder can borrow bits from previous
frames into the current one, so the decoder needs persistent state
across frame decodes. The high-level `minimp3` crate exposes the
decoder behind a `Read`-based API that consumes the reader on
construction; pushing bytes into a long-lived decoder from a real-time
`process()` callback would require a custom `Read` adapter over a
queue, and that's strictly more code than calling `mp3dec_decode_frame`
ourselves.

### LAME's auto-downsample

LAME consumes audio at 44.1 kHz on the input side, but the **bitstream
output rate is decided by `lame_init_params` from the (channels,
bitrate) combo**:

| Tier                | Channels | Bitrate  | LAME's chosen output rate | MPEG version    | Frame size   |
| ------------------- | -------- | -------- | ------------------------- | --------------- | ------------ |
| Deezer Basic        | 2        | 64 kbps  | **24 kHz**                | MPEG-2 Layer 3  | 576 samples  |
| Deezer Standard     | 2        | 128 kbps | 44.1 kHz                  | MPEG-1 Layer 3  | 1152 samples |
| Deezer High Quality | 2        | 320 kbps | 44.1 kHz                  | MPEG-1 Layer 3  | 1152 samples |

LAME picks the lower rate at 64 kbps stereo because there isn't enough
bit budget to encode the full 22 kHz audio band cleanly. **Real-world
encoders behave identically** — this matches what Deezer Basic actually
streams.

The processor builds a throwaway encoder + minimp3 instance during
`Mp3Codec::new` to read back `info.hz` from the first decoded frame.
That's the rate LAME just committed to; the processor stores it as
`decoded_hz` and rebuilds the `internal→host` resampler accordingly.

### minimp3's bit-reservoir wipe

`mp3dec_decode_frame` resets internal state — including the bit
reservoir saved from the previous frame — whenever the input buffer is
too small to validate the *next* frame's sync (`frame_size + HDR_SIZE
> mp3_bytes`). Frames after the first reference the reservoir via
`main_data_begin > 0`, so wiping it makes every subsequent frame fail
to decode (`samples == 0` even though the header is recognised).
Symptom: pure silence after frame 1.

We avoid the trap by only calling `mp3dec_decode_frame` when
`dec_input` has at least `2 * frame_bytes + HDR_SIZE` bytes — sized
dynamically from the active bitrate. See the `min_decode_bytes`
comment in `mp3.rs`.

### Encoder settings

| Setting             | Value                | Why                                                                                                                              |
| ------------------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Quality             | `Good` (LAME `-q5`)  | LAME's default; the most-cited "production" preset for streaming-scale pipelines.                                                |
| Mode                | `JointStereo`        | Universally documented for streaming MP3 across every bitrate.                                                                   |
| VBR                 | `Off` (CBR)          | Deezer's tiers are nominal CBR.                                                                                                  |
| Output sample rate  | (unset)              | Lets LAME auto-downsample at low bitrates exactly like real encoders.                                                            |
| VBR tag             | disabled             | Streaming, not file output. Saves a frame of warm-up silence at the head of the stream.                                          |

---

## AAC-LC / HE-AAC v1 / HE-AAC v2

All three live in `aac.rs`, `heaac.rs`, `heaacv2.rs` respectively, and
all three are gated behind the `fdk-aac` cargo feature. Standard
FDK-AAC encode → decode round-trip via [`fdk-aac`](https://crates.io/crates/fdk-aac).

| Variant     | Encoder profile             | Notes                                                                                          |
| ----------- | --------------------------- | ---------------------------------------------------------------------------------------------- |
| AAC-LC      | `Mpeg4LowComplexity`        | 1024-sample frames, ~60 ms total latency. Also has a mono mode for TikTok / Instagram fallbacks (sums L+R → encode mono → duplicate to L=R after decode). |
| HE-AAC v1   | `Mpeg4HeAac`                | AAC-LC core + Spectral Band Replication (SBR), no Parametric Stereo. Apple Music High Efficiency. |
| HE-AAC v2   | `Mpeg4HeAacV2`              | AAC-LC core + SBR + PS. Spotify Low's signature artifact spectrum (hollow flickering high-end, fake-wide stereo). ~120 ms total latency. |

CBR bitrate management on all three. The `AFTERBURNER` flag (very
likely on in production server-side pipelines) isn't exposed by the
`fdk-aac` crate — we run with library defaults. Inaudible difference
at the bitrates we model.

### License — the FDK situation

FDK-AAC ships under a permissive-but-patent-clausey license that the
FSF considers GPL-incompatible. The cargo feature `fdk-aac` (default
**off**) gates the dependency; without it, AAC tiers fall through to a
transparent bypass. **Official release builds enable the feature**;
the audio open-source community has shipped GPL+FDK binaries for ~15
years without legal incident. See [`licensing.md`](licensing.md) for
the full discussion.

---

## SBC (Bluetooth A2DP)

FFI binding to BlueZ libsbc via [`libsbc-sys`](https://crates.io/crates/libsbc-sys),
`source-build` feature on by default — vendors the C source so no
system libsbc install is needed.

### Configuration

- 8 subbands, 16 blocks per frame (max-quality A2DP standard preset)
- Joint stereo (or mono when `channels == 1`)
- SNR allocation
- 44.1 kHz internal rate
- Bitpool **19** (Low preset, ~127 kbps) or **53** (High preset, ~328 kbps)
- Frame size: 128 samples per channel

### `sbc_init_a2dp` segfault gotcha

Do **not** call `sbc_init_a2dp` with a null config blob — it fails
internally (`sbc_set_a2dp` returns -EINVAL on null `conf`) and calls
`sbc_finish` on the partially-initialised state, freeing `priv`.
Subsequent `sbc_encode` / `sbc_decode` calls then deref freed memory
and segfault. Use plain `sbc_init` and overwrite the parameter fields
directly.

### Cross-compile note

The cross-compile script exports `-include stdint.h` in `CFLAGS`
because BlueZ's libsbc relies on `<sys/types.h>` to transitively pull
in `int32_t` / `int16_t`, which works on Linux glibc but not on
mingw-w64.

---

## LC3 (Bluetooth LE Audio)

Pure-Rust [`lc3-codec`](https://crates.io/crates/lc3-codec). No FFI, no
system deps.

### Configuration

- 48 kHz internal rate (LC3 standard)
- 10 ms frame duration (more common than 7.5 ms)
- **Low** preset: mono, 64 kbps, 80 bytes/frame. Sums L+R / 2 to mono
  before encode, encodes once, duplicates back to L = R after decode —
  realistic for LE Audio low-power profiles.
- **High** preset: stereo, 80 kbps × 2 channels = 160 kbps, 100
  bytes/channel/frame.

### Lifetime gymnastics

`Lc3Encoder<'a>` and `Lc3Decoder<'a>` borrow their working buffers by
reference (the crate targets embedded MCUs and avoids heap allocation).
Naively storing them next to the buffers in the same struct creates a
self-referential trap. We work around it with a controlled `unsafe`
lifetime extension to `'static`, anchored on `Box<[T]>` heap
allocations whose addresses are stable across struct moves. See the
soundness argument and field-ordering invariants in
[`src/processor/bluetooth/lc3.rs`](../src/processor/bluetooth/lc3.rs).

---

## Bluetooth cascade

The Bluetooth layer is structured differently from every other codec:
it doesn't replace the platform codec, it *cascades on top* of it. The
user can run any platform codec preset *and* separately toggle a
Bluetooth roundtrip.

```text
input → platform codec → BluetoothProcessor (when on) → output
        ^^^^^^^^^^^^^^   ^^^^^^^^^^^^^^^^^^
        Spotify Vorbis,  SBC / AAC / LC3 on top
        Apple HE-AAC,
        ...
```

Bluetooth comes *after* the platform codec, never before — that mirrors
how real listeners hear two lossy stages: streaming compresses, device
decodes, Bluetooth re-encodes for transmission.

The cascade is implemented as a separate processor in
[`src/processor/bluetooth.rs`](../src/processor/bluetooth.rs), wrapping
three independent codec backends (`SbcCodec`, `Lc3Codec`, `AacBtCodec`).
Each backend is lazy-init'd on first dispatch — switching between
presets keeps previously-touched slots warm but never builds a backend
the user hasn't asked for.

### AAC-BT vs platform AAC

Bluetooth AAC uses a **separate** `AacProcessor` instance, not the
platform one. The two AAC paths may run with different bitrates
simultaneously (e.g. Apple Music HE-AAC platform + AAC 128 kbps BT)
and shouldn't fight over the same FDK-AAC encoder state.

### Latency budget

Each backend has its own resampler + codec roundtrip delay (~30-50 ms
at non-native host rates). `BluetoothProcessor::worst_case_latency_at`
returns the worst case across all three, folded into the plugin-wide
`target_latency_samples` via `.max(...)`.

Strictly the BT layer cascades *on top of* the platform codec, so the
truly correct global target would be `max_platform + bluetooth_latency`.
We use `.max(...)` instead — under-reporting by ~30-50 ms when both
are active — because over-reporting wastes that much latency on every
session that doesn't use BT, which is the common case. Toggling BT on
may shift PDC alignment by tens of ms; users sensitive to that can
compensate manually in the DAW.

---

## FM Radio (full broadcast-chain simulation)

Unlike every other "platform", FM isn't a codec — it's a transmission
pipeline. We simulate the audibly-relevant stages of a real airchain
end-to-end.

DSP chain across three modules:

- [`src/processor/fm_radio.rs`](../src/processor/fm_radio.rs) — orchestrator + AGC + EQ + pre/de-emphasis + clipper + auto-makeup + delay
- [`src/processor/fm_multiband/`](../src/processor/fm_multiband/) — Linkwitz-Riley 4-band crossover (`crossover.rs`) + per-band compressor + per-band limiter (`dynamics.rs`) + gain-share link bus
- [`src/processor/fm_mpx/`](../src/processor/fm_mpx/) — host↔192 kHz oversampling + MPX encoder + imperfect channel + MPX decoder

```text
input
  ▼
1. Input AGC                          3:1 @ -10 dBFS, 100 ms attack / 1500 ms release
  ▼
2. Broadcast EQ                       Low shelf +3 dB @ 80 Hz, high shelf +2 dB @ 3 kHz
  ▼
3+4. Multiband comp + per-band lim    LR4 crossovers @ 100 / 800 / 4000 Hz, 70/30 gain-share link, -1 dBFS per-band ceiling
  ▼
6. Pre-emphasis                       FIR; 75 µs (US) or 50 µs (Europe)
  ▼
7. 2× oversampled hard clipper        Linear-interp upsample → clip @ -0.5 dBFS → 2-tap boxcar decimate
  ▼
─── composite-rate (192 kHz internally) ───
  ▼
8. MPX stereo encoder                 0.45·(L+R) + 0.45·(L-R)·cos(2π·38k·t) + 0.09·sin(2π·19k·t)
  ▼
9. Imperfect channel                  Pristine: pass-through.
                                      Urban: -6 dB on 22 kHz+ band + pink noise.
                                      Fringe: -18 dB on 22 kHz+ band + more noise + 0.3 Hz multipath LFO.
  ▼
10. MPX decoder                       LP @ 14 kHz → L+R sum.
                                      BPF 23-53 kHz + product-detect with coherent 38 kHz → L-R diff.
                                      Matrix back to L, R.
  ▼
─── back to host rate ───
  ▼
11. De-emphasis                       IIR; exact mathematical inverse of pre-emph on linear material
  ▼
12. Auto-makeup gain                  ~1 s envelope follower on input vs output; clamped to ±12 dB
  ▼
output
```

### Per-band compressor parameters

Tuned to approximate an Orban Optimod / Omnia.9 "Pop Rock" multiband
preset. Mid + high bands hit harder and faster than sub + low — that's
what gives commercial FM its dense, present upper midrange.

| Band | Range       | Ratio | Threshold | Attack | Release |
| ---- | ----------- | ----- | --------- | ------ | ------- |
| Sub  | 0-100 Hz    | 4:1   | -16 dB    | 5 ms   | 200 ms  |
| Low  | 100-800 Hz  | 4:1   | -14 dB    | 8 ms   | 150 ms  |
| Mid  | 800-4 kHz   | 6:1   | -12 dB    | 3 ms   | 80 ms   |
| High | 4 kHz +     | 6:1   | -10 dB    | 2 ms   | 60 ms   |

### Internal sample rate — 192 kHz

The composite spectrum spans 0-53 kHz (we omit RDS at 57 kHz). To
represent it without aliasing the 38 kHz subcarrier, the MPX path runs
at 192 kHz internally. A rubato `FftFixedIn` pair handles host ↔ 192
kHz; at 192 kHz host the resamplers drop out. Latency cost: ~5-10 ms
total.

### MPX decoder filter design

- 4-pole Butterworth LP @ 14 kHz on composite → L+R sum (cutoff lowered
  from 15 kHz to give the 19 kHz pilot 5 kHz of headroom for rejection).
- 4-pole Butterworth BPF @ 23-53 kHz, **implemented as HP @ 23 kHz
  cascaded with LP @ 53 kHz**. A single "constant skirt gain" cookbook
  BPF would have peak gain = Q at center; 4 cascaded would hit ~Q⁴ peak
  gain, throwing the L-R reconstruction off. Unity-gain pass band is
  critical for the encode/decode math to balance.

### Auto-makeup gain

The AGC + multiband + clipper combo pushes FM output 4-8 dB louder
than input on a typical mix, which would make A/B switching a
loudness comparison rather than an artifact comparison. The auto-makeup
stage is a long-term envelope follower (~1 s τ) that applies the inverse
ratio as makeup, keeping output within ~1 dB of input on programme
material across all 6 tiers. Mirrors the receive-side AGC every real
broadcast processor runs as the last stage.

### What we deliberately don't simulate

- **Composite clipper** — real airchains often clip the post-MPX
  composite signal directly. We clip the audio domain only.
- **RDS** — 57 kHz data subcarrier, filtered out by the decoder's
  14 kHz LP. Audibly inert; ~80 LOC for zero perceptible audio change.
- **Frequency drift** between encoder and decoder — both ends of our
  chain share a phase counter (no PLL needed). Audibly minimal except
  in extreme weak-signal cases.
- **Adjacent-channel interference** — receiver-side artifact, not
  airchain.
- **Station-specific multiband presets** — we pick one reasonable
  "Pop Rock" tuning rather than expose tunable parameters.

---

## Bypass / FLAC / ALAC

`CodecSpec::Bypass` is `process()`'s no-op arm. Used for every
"Lossless" tier (FLAC, ALAC) — both are bit-identical to source so
there's nothing to encode and nothing to decode in the
audible-difference sense. Routed through Opus's passthrough path so
reported latency stays consistent and toggling bypass doesn't shift
PDC.
