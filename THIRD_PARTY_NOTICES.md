# Third-party notices

The Streaming Simulator binary you are using contains the following
third-party components. The plugin itself is licensed under
GPL-3.0-or-later (see `LICENSE`).

## Codec libraries

### libopus

Used for the Opus codec (YouTube Music web, YouTube video web, Amazon
Music SD).
License: BSD-3-Clause.
<https://opus-codec.org/>

### libvorbis (with aoTuV / Lancer patches)

Used for the Ogg Vorbis codec (Spotify Normal / High / Very High).
License: BSD-3-Clause.
<https://xiph.org/vorbis/>

### libogg

Required by libvorbis's C API (we use libvorbis in raw-packet mode without
the Ogg container).
License: BSD-3-Clause.
<https://xiph.org/ogg/>

### LAME (libmp3lame)

Used for the MP3 encoder (Deezer Basic / Standard / High Quality,
SoundCloud legacy MP3).
License: LGPL-2.1-or-later, with the standard LGPL linking permission —
fine to bundle in our GPL-3 binary.
<https://lame.sourceforge.io/>

### minimp3

Used for the MP3 decoder (paired with LAME on the encode side).
License: CC0 / public domain.
<https://github.com/lieff/minimp3>

### Fraunhofer FDK-AAC

Used for AAC-LC (Spotify web, YouTube Music mobile, Apple Music High
Quality, Tidal Low, SoundCloud, YouTube video AAC, TikTok, Instagram,
Bluetooth AAC), HE-AAC v1 (Apple Music High Efficiency), and HE-AAC v2
(Spotify Low).
License: "Software License for The Fraunhofer FDK AAC Codec Library for
Android", a permissive BSD-style license with patent-grant clauses. See
the full text at <https://github.com/mstorsjo/fdk-aac/blob/master/NOTICE>.

The Free Software Foundation considers this license incompatible with GPL.
Practically, the broader audio open-source community has shipped
GPL+FDK-bundled binaries for ~15 years without legal incident. See
[`docs/licensing.md`](docs/licensing.md) in the source repository for a
fuller discussion of the situation.

### BlueZ libsbc

Used for the SBC codec (Bluetooth A2DP universal baseline).
License: LGPL-2.1-or-later.
<https://git.kernel.org/pub/scm/bluetooth/bluez.git/tree/sbc>

### lc3-codec

Used for the LC3 codec (Bluetooth LE Audio). Pure-Rust implementation.
License: Apache-2.0 OR MIT.
<https://crates.io/crates/lc3-codec>

## DSP and infrastructure libraries

| Library              | License             | Purpose                                |
| -------------------- | ------------------- | -------------------------------------- |
| nih-plug             | GPL-3.0-or-later    | Plugin framework (forces our GPL license) |
| nih_plug_egui        | GPL-3.0-or-later    | UI integration                         |
| egui                 | MIT OR Apache-2.0   | Immediate-mode UI                      |
| baseview             | MIT OR Apache-2.0   | Window hosting                         |
| rubato               | MIT OR Apache-2.0   | Sample-rate conversion                 |
| png                  | MIT OR Apache-2.0   | PNG icon decoding                      |
| fast_image_resize    | MIT OR Apache-2.0   | Lanczos3 icon resampling               |

A complete dependency tree is available via `cargo tree` against the
project source.
