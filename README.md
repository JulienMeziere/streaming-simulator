# streaming-simulator

An audio plugin for the master channel that emulates how streaming
platforms (and Bluetooth, and FM radio) compress and degrade audio, so
you can hear what your mix will sound like before you ship it.


<img width="1460" height="451" alt="image" src="https://github.com/user-attachments/assets/496456a6-641a-439a-8c36-744b2bde1760" />


Built in Rust with [nih-plug](https://github.com/robbert-vdh/nih-plug).
Compiles to **CLAP**, **VST3**, and a **standalone** binary.

## Features

- **10 streaming platforms**, each with its real codec stack at the
  bitrates the service actually uses: Spotify (Vorbis, HE-AAC v2,
  AAC-LC), Deezer (MP3 + auto-downsample at 64 kbps), Apple Music
  (HE-AAC v1, AAC-LC), YouTube Music (mobile AAC + web Opus),
  SoundCloud (AAC-LC + legacy MP3), Tidal (AAC-LC), Amazon Music
  (Opus), YouTube video (AAC + Opus), TikTok (AAC-LC stereo + mono),
  Instagram (AAC-LC approximating xHE-AAC).
- **FM Radio**: full broadcast-chain simulation (input AGC, broadcast
  EQ, 4-band multiband compressor, 2× oversampled hard clipper, MPX
  stereo encoder + imperfect channel + decoder, pre/de-emphasis,
  auto-makeup gain). 6 tiers covering US (75 µs) and Europe (50 µs)
  pre-emphasis × Pristine / Urban / Fringe reception.
- **Bluetooth cascade**: optional second lossy stage on top of the
  selected platform codec. 6 presets covering SBC (Low / High), AAC
  (128 / 256 kbps), and LC3 (LE Audio 64 / 160 kbps). Models the real
  "streaming → device → Bluetooth → headphones" listening chain.
- **Master bypass**, latency-aligned across every codec so toggling
  doesn't shift PDC.

See [`docs/codecs.md`](docs/codecs.md) for the per-platform codec /
bitrate matrix with citations and
[`docs/codec-implementation.md`](docs/codec-implementation.md) for the
implementation notes.

## Requirements

- Rust (stable, 1.75+ recommended) — install via [rustup](https://rustup.rs).
- A C/C++ toolchain:
  - Linux: `build-essential`, `pkg-config`, plus `libx11-dev libxcb1-dev libxcb-icccm4-dev libxcursor-dev libxkbcommon-dev libxcb-shape0-dev libxcb-xfixes0-dev` for the GUI.
  - macOS: Xcode Command Line Tools.
  - Windows: MSVC build tools.

## Build

Build proper plugin bundles (`.clap`, `.vst3` with the right folder layout):

```sh
cargo xtask bundle streaming-simulator --release --features fdk-aac
```

The `fdk-aac` feature is needed for every AAC-based tier (most platforms
ship AAC at some quality). Without it those tiers fall through to
bypass — see [`docs/licensing.md`](docs/licensing.md) for the GPL/FDK
situation. For a quick incremental dev build (no bundling):

```sh
cargo build --features fdk-aac
```

Output ends up in `target/bundled/`:

```
target/bundled/
├── Streaming Simulator.clap
└── Streaming Simulator.vst3/
    └── Contents/<arch>/Streaming Simulator.{so,dll,dylib}
```

### Cross-compile for Windows

Linux and macOS contributors can produce Windows bundles:

```sh
rustup target add x86_64-pc-windows-gnu
# plus a MinGW toolchain (e.g. `apt install mingw-w64`, `brew install mingw-w64`)

./scripts/crosscompile-windows.sh                       # default (no AAC)
./scripts/crosscompile-windows.sh --features fdk-aac    # full feature parity
./scripts/crosscompile-windows.sh --help                # all options
```

The bundles end up in `target/bundled/` (nih-plug's bundler always writes
there regardless of target). Copy them onto a Windows machine into a
folder your DAW scans (see the [Install](#install) table below).

### Standalone binary (optional)

Gated behind the `standalone` feature so the default build doesn't pull
in OS audio dependencies:

```sh
cargo xtask bundle streaming-simulator --release --features "standalone fdk-aac"
./target/bundled/streaming-simulator --help
```

> Linux requires `libasound2-dev` and `libjack-jackd2-dev` for the
> ALSA / JACK backends. macOS and Windows have what they need out of
> the box.

## Install

Pre-built bundles for Linux x86_64, Windows x86_64, and macOS (universal
— Apple Silicon + Intel) are attached to each [GitHub Release](../../releases).
Download the zip for your platform, then copy the bundles into your
DAW's plugin path:

| OS      | CLAP                            | VST3                            |
| ------- | ------------------------------- | ------------------------------- |
| Linux   | `~/.clap` or `/usr/lib/clap`    | `~/.vst3` or `/usr/lib/vst3`    |
| macOS   | `~/Library/Audio/Plug-Ins/CLAP` | `~/Library/Audio/Plug-Ins/VST3` |
| Windows | `%COMMONPROGRAMFILES%\CLAP`     | `%COMMONPROGRAMFILES%\VST3`     |

### macOS extra step

The macOS bundles aren't code-signed (signing requires a paid Apple
Developer account). After unzipping but **before** moving the bundles
into your plugin folder, clear the download-quarantine flag in
Terminal:

```sh
xattr -dr com.apple.quarantine "Streaming Simulator.clap"
xattr -dr com.apple.quarantine "Streaming Simulator.vst3"
```

Otherwise the DAW will refuse to load the plugin with a "damaged or
from an unidentified developer" error. One-time step per download.

## Tips for an accurate A/B against the real platforms

For the closest possible match to what each service actually streams,
**run your project at the codec's native sample rate**. The plugin pins
each codec to a fixed internal pipeline; if your host rate matches it,
no resampling happens at all before or after the codec — you hear
bit-identical pre/post-encode audio relative to the real platforms,
plus the codec's own artifacts.

| Host rate  | Best for                                                                                                                                              |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **44.1 kHz** | Spotify (every tier), YouTube Music mobile, Deezer, Apple Music lossy tiers, Tidal Low, SoundCloud, Bluetooth SBC                                    |
| **48 kHz**   | YouTube Music web, YouTube video, Amazon Music SD, TikTok, Instagram, Bluetooth LC3                                                                  |
| 88.2 / 96 / 192 kHz | Plugin still works — each block makes two extra trips through a high-quality FFT resampler (`rubato`). Codec artifacts dominate either way.   |

## Testing

```sh
cargo test --features fdk-aac --lib
```

Coverage (uses [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)):

```sh
cargo install cargo-llvm-cov            # one-time install
rustup component add llvm-tools-preview # one-time install

./scripts/coverage.sh                   # HTML report at target/llvm-cov/html/
./scripts/coverage.sh --summary-only    # numeric summary only
```

Current coverage — **~85% lines / ~89% functions** across 168 tests,
with every codec processor at 90%+ line coverage. Editor rendering code
is deliberately not unit-tested (would need headless GL infrastructure
for marginal value); only its state machine is. Full per-module
breakdown via `./scripts/coverage.sh`.

## Project layout

```
.
├── Cargo.toml              # workspace root
├── bundler.toml            # nih_plug_xtask bundling config
├── src/
│   ├── lib.rs              # plugin entry, dispatch, params
│   ├── platforms.rs        # platform / codec / tier catalog
│   ├── editor/             # egui UI (mod / widgets / icons)
│   ├── processor/          # codec + DSP processors
│   │   ├── pipeline.rs     # shared resampler + ring scaffolding
│   │   ├── biquad.rs       # cookbook biquad primitives
│   │   ├── opus.rs / vorbis.rs / mp3.rs / aac.rs / heaac.rs / heaacv2.rs
│   │   ├── bluetooth/      # BT cascade (sbc / lc3 / aac_bt + orchestrator)
│   │   ├── fm_radio.rs     # FM broadcast-chain orchestrator
│   │   ├── fm_multiband/   # 4-band multiband compressor + limiter
│   │   └── fm_mpx/         # MPX encoder / channel / decoder
│   ├── test_helpers.rs     # shared sine / peak / RMS / Buffer helpers
│   └── main.rs             # standalone binary entry
├── xtask/                  # `cargo xtask bundle ...` helper
├── docs/                   # codecs.md, codec-implementation.md, licensing.md
├── examples/               # recolor_icons.rs (icon dev tool)
├── resources/              # platform / BT / settings PNG icons
├── scripts/                # coverage, cross-compile, install
└── cross/                  # vendored C glue for cross-compile
```

## Contributing

Contributions are welcome — this is exactly the kind of project that
benefits from many ears.

- **Found a bug, or a codec that doesn't sound right against the real
  platform?** [Open an issue](../../issues/new) with a reproduction
  recipe (host, host sample rate, platform tier, source material if
  shareable). Audio comparisons are easier when there's a concrete
  reference to A/B against.
- **Want a platform or Bluetooth codec that isn't here yet?** Open an
  issue with the *Feature request* label. Include any sources you have
  for the platform's encoder choice — the bar for adding a new tier is
  "we know what library + bitrate the real service uses". See
  [`docs/codecs.md`](docs/codecs.md) for the level of source-citation
  the existing tiers were built with.
- **Want to send a patch?** Fork → branch → PR. Run
  `cargo test --features fdk-aac --lib` before pushing; the test suite
  is the safety net against accidentally breaking codec round-trips.
  New codec tiers should ship with their own round-trip test in the
  matching processor module. Run `cargo fmt` before committing.

If you're not sure whether something fits, opening an issue first is
always fine.

## License

The plugin is **GPL-3.0-or-later** (nih-plug is GPL-3.0, so anything
built with it must be too).

Official builds bundle [Fraunhofer FDK-AAC](https://github.com/mstorsjo/fdk-aac)
for the AAC-based codec tiers. FDK's permissive-but-patent-clausey
license is considered GPL-incompatible by the FSF; the audio
open-source community has shipped GPL+FDK binaries for ~15 years
without legal incident. See [`docs/licensing.md`](docs/licensing.md)
for the full discussion. If you want a strictly GPL-clean binary, omit
the `fdk-aac` cargo feature — AAC tiers fall through to bypass and
every other codec works as normal.

Release archives include [`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md)
summarizing the bundled components.
