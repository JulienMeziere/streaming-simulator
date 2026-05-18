# Licensing notes

The plugin itself is licensed under **GPL-3.0-or-later** (see `LICENSE`).
This is forced on us by [`nih-plug`](https://github.com/robbert-vdh/nih-plug),
which is also GPL-3.0 — anything that links it must be too.

The plugin links several codec libraries; most are GPL-compatible. The one
license that needs explicit discussion is Fraunhofer FDK-AAC.

## Fraunhofer FDK-AAC

[FDK-AAC](https://github.com/mstorsjo/fdk-aac) is the high-quality
open-source AAC encoder/decoder we use for AAC-LC, HE-AAC v1, and HE-AAC
v2 (Spotify, YouTube Music mobile, Apple Music, Tidal, SoundCloud,
YouTube video, TikTok, Instagram, Bluetooth AAC). It's distributed under
a permissive BSD-style license — *but* with explicit patent-grant
clauses that the **Free Software Foundation considers incompatible with
GPL**:

> If you alter, or otherwise modify the source code, you may not
> redistribute the modified version under any license other than the
> original FDK License.

The argument is that this clause imposes additional restrictions vs. plain
GPL, conflicting with GPL's "no further restrictions" rule when the two are
linked into the same binary.

This interpretation is **principled and contested** — it has never been
tested in court, and the broader open-source community has been distributing
GPL-with-FDK-bundled binaries for ~15 years (FFmpeg fdk builds, OBS Studio,
Audacity, countless audio plugins) without legal incident. The Fraunhofer
patents underlying AAC-LC mostly expired in 2017; some HE-AAC v2 / SBR
patents may still be active in some jurisdictions but Fraunhofer's
enforcement focus has always been commercial, not free FOSS.

## What this project does

We **ship official builds with FDK-AAC bundled**. Every codec button works
out of the box.

This is a deliberate decision, accepting:

- The FSF's principled position that GPL+FDK is non-conformant.
- The community's empirical evidence that no lawsuits have been brought
  over this combination at the FOSS level in two decades.
- The practical reality that most users want one binary that "just works",
  not a build instruction.

If you'd rather build a strictly GPL-clean binary:

```sh
cargo build --release  # no `--features fdk-aac`
```

That produces a build where the AAC-LC, HE-AAC v1, HE-AAC v2, and
Bluetooth AAC tiers fall through to a transparent bypass — the buttons
stay clickable but the audio isn't degraded. All other codecs (Vorbis,
Opus, MP3, SBC, LC3, FM Radio, FLAC bypass) work as normal.

## What this means if you redistribute *this* plugin

If you take our binary and redistribute it: you carry whatever risk we
carry, which based on community precedent is essentially zero for FOSS use
but isn't legally pristine.

If you redistribute a *modified* version: read the FDK License carefully.
The clause quoted above is binding on modifications.

If you redistribute *commercially* (selling it): you should obtain a
commercial AAC license from Fraunhofer to be safe. The FOSS-tolerance
the community has built up over 20 years is around free distribution, not
paid products.

## Other licenses bundled

| Component                                 | License                                        | Purpose                                     |
| ----------------------------------------- | ---------------------------------------------- | ------------------------------------------- |
| nih-plug + nih_plug_egui                  | GPL-3.0-or-later                               | Plugin framework (forces our GPL license)   |
| egui                                      | MIT OR Apache-2.0                              | Immediate-mode UI                           |
| rubato                                    | MIT OR Apache-2.0                              | Sample-rate conversion                      |
| png                                       | MIT OR Apache-2.0                              | PNG icon decoding                           |
| fast_image_resize                         | MIT OR Apache-2.0                              | Lanczos3 icon resampling                    |
| libopus (via `opus`)                      | BSD-3-Clause                                   | Opus codec                                  |
| libvorbis (via `aotuv_lancer_vorbis_sys`) | BSD-3-Clause                                   | Vorbis codec (with aoTuV / Lancer patches)  |
| libogg (via `ogg_next_sys`)               | BSD-3-Clause                                   | Required by libvorbis API                   |
| LAME (via `mp3lame-encoder`)              | LGPL-2.1-or-later                              | MP3 encoder                                 |
| minimp3 (via `minimp3-sys`)               | CC0-1.0                                        | MP3 decoder                                 |
| BlueZ libsbc (via `libsbc-sys`)           | LGPL-2.1-or-later                              | Bluetooth SBC codec                         |
| lc3-codec                                 | Apache-2.0 OR MIT                              | Bluetooth LE Audio LC3 codec (pure Rust)    |
| FDK-AAC (via `fdk-aac`, optional)         | Fraunhofer FDK AAC Codec License (see above)   | AAC-LC / HE-AAC v1 / HE-AAC v2              |

The release archives include a [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md)
summarizing the above for end users.
