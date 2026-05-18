# Streaming platform codecs

A reference of which codecs and bitrates each streaming service uses for each
quality tier. This is the source-of-truth document for the simulation targets
the plugin is trying to model — when adding or updating a tier in
`src/platforms.rs`, double-check it against the row here.

Bitrates are quoted as the platform itself does ("equivalent to ~N kbps"); the
real streams are VBR and may drift around the nominal value.

## Encoder settings — verified vs assumed

The libraries below are what each platform demonstrably ships (deducible
from byte patterns in the streams). The specific tunings are listed with
their confidence level and source.

### Spotify (Vorbis)

- **Library**: libvorbis with the **aoTuV** quality patches.
- **Mode**: VBR quality mode (`vorbis_encode_init_vbr`), *not* the
  bitrate-managed mode.
- **Per-tier quality**: verified from a 2010 reverse-engineering effort by
  [SoundExpert][se-spotify] that has remained accurate since:

  | Tier      | Bitrate    | Vorbis setting | Source                                       |
  | --------- | ---------- | -------------- | -------------------------------------------- |
  | Normal    | ~96 kbps   | `-q2`          | Inferred from the standard q→bitrate table   |
  | High      | ~160 kbps  | `-q5`          | **SoundExpert 2010 — Spotify-verified**      |
  | Very High | ~320 kbps  | `-q9`          | **SoundExpert 2010 — Spotify-verified**      |

[se-spotify]: https://soundexpert.org/articles/-/blogs/11910

### Spotify Low / Spotify Web / YouTube Music Mobile (FDK-AAC family)

These three platforms appear to use Fraunhofer **FDK-AAC** server-side
(the de-facto reference encoder for streaming AAC). The libraries used
aren't *directly* reverse-engineered the way Spotify's Vorbis settings
are, but it's the universal community assumption based on stream byte
patterns. The plugin uses FDK-AAC for all three.

- **Profile**: AAC-LC for the high-bitrate tiers, **HE-AAC v2** (AAC-LC
  core + SBR + Parametric Stereo) for Spotify Low at 24 kbps.
- **`AFTERBURNER` flag**: very likely *on* in production (universal for any
  non-real-time AAC pipeline) but the `fdk-aac` Rust crate doesn't expose
  the toggle, so we run with library defaults. Inaudible difference at
  the bitrates we model.
- **Bitrate management**: CBR.

### Apple Music (CoreAudio AAC family)

Apple Music does **not** use FDK-AAC. They use Apple's own **CoreAudio
AAC encoder** — the one packaged inside macOS / iOS as
`AudioToolbox.framework`, exposed at the command line via `afconvert`
and (third-party) `qaac`. It's a different psychoacoustic model than
FDK-AAC; produces subtly different artifacts at the same bitrate
(slightly less aggressive HF rolloff, slightly different transient
behaviour). The encoder is closed-source and macOS-only, so the plugin
**uses FDK-AAC as the closest available substitute** — same shape of
caveat as Deezer's MP3 (real Deezer's encoder ≠ public LAME, but
public LAME is the closest open-source equivalent).

| Tier | Real Apple settings (verified) | Our FDK-AAC approximation |
| ---- | ------------------------------ | ------------------------- |
| **High Quality** (256 kbps) | `afconvert -d aac -f m4af -b 256000 -q 127 -s 2` — **CVBR** (Constrained VBR), 256 kbps target, max quality (127/127), Sound Check enabled. **Verified** by the [Hydrogenaudio reverse-engineering thread][ha-itunes-plus] (= "QuickTime CVBR @ 256k, max quality"). Same settings as iTunes Plus and the Apple Digital Masters spec ([PDF][adm-pdf]). | `BitRate::Cbr(256000)` — strict CBR, FDK-AAC defaults for everything else. Average bitrate matches; per-frame bit allocation differs slightly (real Apple gives complex passages a few extra kbps). Audible difference at 256 kbps is minimal. |
| **High Efficiency** (~64 kbps) | **HE-AAC v1** (AAC-LC core + SBR, *no* Parametric Stereo). Apple's tech note recommends 32–80 kbps for HE-AAC v1 stereo; cellular streaming docs cite ~64 kbps. Likely also CVBR at max quality, but Apple has not published the exact `afconvert` invocation for this tier. | `AudioObjectType::Mpeg4HeAac` (= HE-AAC v1, **distinct from the v2 variant we use for Spotify Low**) at `BitRate::Cbr(64000)`. |
| **Lossless** / **Hi-Res Lossless** | **ALAC** at 16-bit/44.1 → 24-bit/48 (Lossless) or up to 24-bit/192 (Hi-Res Lossless). Bit-identical to source. | `Bypass` — same as every other lossless tier we expose. |

Plus a separate **Dolby Atmos** spatial tier (Dolby Digital Plus / E-AC-3
multi-channel, downmixed to stereo on non-Atmos playback). Out of scope
for a stereo-only plugin — we don't model it.

Loudness normalization for Apple Music is handled by **Sound Check**
metadata (RMS-based, written into the m4a `iTunSMPB`/`iTunNORM` atoms
during the master encode) and applied at playback against a roughly
**−16 LUFS** target. This is metadata + playback-side gain, not a
transformation of the encoded audio bytes — so it doesn't affect what
our codec round trip produces.

[ha-itunes-plus]: https://hydrogenaudio.org/index.php/topic,70405.0.html
[adm-pdf]: https://www.apple.com/apple-music/apple-digital-masters/docs/apple-digital-masters.pdf

### YouTube Music Web / YouTube (video) / Amazon Music SD (Opus family)

- **Library**: libopus (Google's reference; same crate everyone uses).
  This is one library shared across three different platforms.
- **Application mode**: `OPUS_APPLICATION_AUDIO` — the documented universal
  choice for music streaming (not `LOWDELAY` or `RESTRICTED_LOWDELAY`).
- **Bitrate mode**: VBR (libopus default for `Application::Audio`).
- **Complexity**: not publicly documented per platform; we use the libopus
  default.

Per-platform Opus bitrates (each backed by a verifiable source):

| Platform               | Tier           | Bitrate    | Source                                                                                              |
| ---------------------- | -------------- | ---------- | --------------------------------------------------------------------------------------------------- |
| YouTube Music Web      | Low            | 48 kbps    | YouTube Music app docs (mirrors mobile AAC tier bitrate)                                            |
| YouTube Music Web      | Normal         | 128 kbps   | "                                                                                                    |
| YouTube Music Web      | High           | 256 kbps   | "                                                                                                    |
| YouTube (video)        | Opus low (250) | ~70 kbps   | [yt-dlp itag table][yt-dlp-itags]                                                                  |
| YouTube (video)        | Opus high (251)| ~160 kbps  | "                                                                                                    |
| **Amazon Music SD**    | Low            | **48 kbps**  | **[Amazon Music developer docs][amazon-dev-docs] — directly verified**                            |
| **Amazon Music SD**    | Standard       | **192 kbps** | "                                                                                                  |
| **Amazon Music SD**    | High           | **320 kbps** | "                                                                                                  |

[yt-dlp-itags]: https://github.com/yt-dlp/yt-dlp/issues/12878
[amazon-dev-docs]: https://developer.amazon.com/docs/music/audio-formats.html

### Amazon Music HD / Ultra HD (FLAC)

- **Codec**: FLAC. Bit-identical to source — bypass in this plugin.
- **HD**: 16-bit / 44.1 kHz, ~800 kbps avg.
- **UHD**: 24-bit / 44.1–192 kHz, ~1600 / 2800 / 5000 kbps avg variants.
- Verified directly from Amazon Music's developer docs (link above).

### SoundCloud (AAC + MP3 legacy)

- **Library** for AAC: assumed **FDK-AAC** server-side (community
  assumption, not directly reverse-engineered).
- **Library** for MP3 legacy: LAME (industry standard).
- **Codec migration in progress** (2025–2026): SoundCloud is replacing
  the legacy MP3 128 kbps + Opus 64 kbps streams with AAC-only HLS
  delivery, **directly confirmed** by their staff engineer in the
  [public deprecation notice][sc-deprecation]:

  | Tier              | Codec       | Bitrate    | Status                                          |
  | ----------------- | ----------- | ---------- | ----------------------------------------------- |
  | Free / Low        | AAC-LC      | 96 kbps    | Planned (low-bandwidth fallback, not yet rolled out as of Apr 2026) |
  | Free / Standard   | AAC-LC      | 160 kbps   | New default (rolling out, replacing MP3 128)    |
  | Go+ / High        | AAC-LC      | 256 kbps   | Go+ subscribers, partner API only               |
  | Legacy            | MP3         | 128 kbps   | Still served for tracks not yet retranscoded    |
  | (removed)         | Opus        | 64 kbps    | Already removed                                 |

- **Bitrate management**: CBR for AAC tiers. MP3 128 legacy is also CBR.
- **Per the SoundCloud staff engineer's own justification**: "Opus was the
  right choice when SoundCloud started… networks improved over the last
  two decades. SoundCloud is simply moving with the industry here."
  Translation: AAC-LC won the streaming codec war, even at low bitrates.

[sc-deprecation]: https://github.com/soundcloud/api/issues/441

### TikTok (AAC-LC, video container)

- **Library**: assumed FDK-AAC server-side (industry standard for video
  AAC pipelines; not directly verified). The plugin uses FDK-AAC.
- **Profile**: AAC-LC. **No HE-AAC, no xHE-AAC.** TikTok's published
  audio guidelines and reverse-engineered reels both consistently show
  plain AAC-LC.
- **Sample rate**: 44.1 or 48 kHz, depending on the source. Mobile
  recording inside the TikTok app captures at 48 kHz.
- **Bitrate management**: CBR. TikTok's transcoding ladder serves the
  high tier at ~128 kbps stereo and a low/watermarked-download tier at
  ~64 kbps mono. Adaptive selection is on the network side, not exposed
  as separate user tiers.
- **Loudness**: TikTok does *not* use traditional LUFS-based
  normalization. They apply **dynamic-range compression + a brick-wall
  limiter + peak normalization** during transcode. This produces a
  perceptibly louder, less-dynamic playback than the source — distinct
  from how Spotify / YouTube / Apple normalize. We don't simulate the
  compression stage; the codec pipeline reproduces the AAC artifacts
  but not the level squashing.

### Instagram / Reels / Stories (Meta — actually xHE-AAC, we approximate with AAC-LC)

- **Library on the real platform**: Meta migrated to **xHE-AAC**
  (Extended HE-AAC, MPEG-D USAC profile) for Reels and Stories,
  [confirmed publicly][meta-xheaac] in their 2023 engineering post.
  xHE-AAC has noticeably better quality at low bitrates than HE-AAC v2
  and includes built-in MPEG-D loudness management.
- **Library in the plugin**: **FDK-AAC's xHE-AAC profile is *not*
  exposed by the `fdk-aac` Rust crate** (0.8.0 latest only exposes
  AAC-LC, HE-AAC v1/v2, AAC-LD, AAC-ELD — no `Mpeg4Usac` variant). So
  we approximate Instagram with **AAC-LC at the same bitrate**. This
  is the largest fidelity gap of any tier we model — the artifact
  spectrum is in the right family but not identical. xHE-AAC at the
  bitrates Instagram uses (96–128 kbps stereo) sounds slightly
  cleaner; the approximation gap is most audible on transients and
  high-frequency content.
- **Sample rate**: 44.1 or 48 kHz preserved from source.
- **Bitrate management**: adaptive (xHE-AAC's ABR feature). The plugin
  exposes a representative ~128 kbps stereo CBR through AAC-LC.
- **Loudness**: xHE-AAC has integrated loudness metadata + DRC
  (MPEG-D), targeting **~−14 LUFS** average across Reels. We don't
  simulate the loudness/DRC stage; only the codec round-trip.

[meta-xheaac]: https://engineering.fb.com/2023/04/11/video-engineering/high-quality-audio-xhe-aac-codec-meta

### YouTube (video) (AAC + Opus)

- Two parallel codec arms, picked by client:
  - **AAC-LC (M4A)**: served to Safari (no Opus support in WebM) and
    most mobile clients.
  - **Opus (WebM)**: served to Chromium-based browsers + Firefox.
- Sample rate: **48 kHz** for both. YouTube's upload guideline is 48 kHz
  and they don't resample server-side. Note that this differs from
  YouTube Music *Mobile*'s AAC tiers, which are 44.1 kHz like every
  other AAC streaming service — but YouTube *video* AAC is consistently
  48 kHz, matching the Opus delivery rate.
- The plugin's `AacProcessor` runs at 44.1 kHz internally, so YouTube
  video AAC tiers go through one extra 48→44.1→48 resample pass
  compared to real YouTube. Same order of magnitude of fidelity gap as
  the FDK-AAC-vs-CoreAudio-AAC mismatch for Apple Music. Codec artifacts
  dominate either way.

### Tidal (AAC + FLAC)

- **Library** for the lossy tier: assumed **FDK-AAC** — same caveat as
  Spotify Web / YouTube Music Mobile (universal community assumption,
  not directly reverse-engineered). The plugin uses FDK-AAC.
- **Profile**: **AAC-LC** for the "Low" tier at 320 kbps. HE-AAC isn't
  used at this bitrate (it's not designed for stereo above ~128 kbps).
- **Bitrate management**: CBR up to 320 kbps. Tidal's documentation says
  "up to 320 kbps" — adaptive bitrate is on the delivery / network side,
  not exposed as separate user tiers.
- **API tier vs UI tier**: Tidal's underlying API (per the public
  reverse-engineered [hmelder/TIDAL wiki][tidal-api]) still defines
  five quality levels — `LOW` (96 kbps AAC), `HIGH` (320 kbps AAC),
  `LOSSLESS` (16/44.1 FLAC), `HI_RES` (24/96 MQA, deprecated), and
  `HI_RES_LOSSLESS` (24/192 FLAC). The consumer app maps its three
  user-visible tiers ("Low" / "High" / "Max") onto API `HIGH` /
  `LOSSLESS` / `HI_RES_LOSSLESS` for the standard happy path. The
  vestigial 96 kbps `LOW` API tier may still be used as a fallback on
  poor connections but isn't selectable in the modern UI; the plugin
  models the canonical 320 kbps AAC-LC delivery.
- **Anecdotal "320 isn't really 320"**: a few users have reported
  spectral cutoffs around 17 kHz on Tidal's 320 kbps AAC streams
  (suggesting ~160-quality encoder settings rather than transparent
  320 kbps), via the [tidal-ui issue][tidal-spec-issue]. Not officially
  acknowledged by Tidal; we encode at our usual FDK-AAC default which
  goes higher than 17 kHz at 320 kbps. If real Tidal does limit
  bandwidth at 320, the plugin will sound *cleaner* than the real
  service for that tier.
- **MQA**: **gone** as of July 24, 2024. Tidal switched the entire Max
  tier to HiRes FLAC and is migrating remaining MQA-only tracks to
  FLAC. The plugin doesn't model MQA — it was niche even when it
  existed and the algorithm is proprietary.

[tidal-api]: https://github.com/hmelder/TIDAL/wiki/track-playbackinfopostpaywall
[tidal-spec-issue]: https://github.com/binimum/tidal-ui/issues/134

### Deezer (MP3)

- **Library**: LAME — the de-facto MP3 encoder; what every published Deezer
  technical writeup references.
- **Quality preset**: LAME's own default, **`Quality::Good`** (= LAME `-q5`).
  This is the most-cited "production" preset for streaming pipelines that
  encode large libraries — quality 0–2 is too slow for the scale, quality
  7–9 introduces audible artifacts beyond what the bitrate alone explains.
- **Mode**: `JointStereo` (universally documented for streaming MP3 across
  every bitrate).
- **Output sample rate**: *not* pinned — at 64 kbps stereo LAME auto-
  downsamples the bitstream to **MPEG-2 Layer 3 at 24 kHz**, which is
  exactly what real low-bitrate streams sound like. Verified by encoding
  silence and reading minimp3's reported `info.hz` (see
  [`docs/codec-implementation.md`](codec-implementation.md)).
- **Bitrate management**: CBR.

## Spotify

Spotify uses different codec stacks depending on the client. The plugin
exposes both on the Spotify tab: row 1 is the desktop / mobile experience,
row 2 is the web player.

### Desktop / mobile apps (row 1)

| Tier        | Codec       | Bitrate                  | Notes                                                                                                                                                                                                          |
| ----------- | ----------- | ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Low         | HE-AAC v2   | ~24 kbps                 | Free + Premium. Uses Spectral Band Replication (SBR) and Parametric Stereo (PS) — the high-frequency rolloff and stereo collapse artifacts that motivate this plugin come from exactly here.                  |
| Normal      | Ogg Vorbis  | ~96 kbps                 | Free + Premium. Audible artifacts on most material.                                                                                                                                                            |
| High        | Ogg Vorbis  | ~160 kbps                | Free + Premium. Default "Automatic" caps here.                                                                                                                                                                 |
| Very High   | Ogg Vorbis  | ~320 kbps                | Premium-only. Mostly transparent for casual listening but still lossy.                                                                                                                                         |
| Lossless    | FLAC        | up to 24-bit / 44.1 kHz  | Premium-only, rolled out 2025. Bit-identical to source — nothing to simulate.                                                                                                                                  |

### Web player (row 2)

The browser player uses **AAC-LC** instead of Vorbis, a legacy of EME / DRM
constraints (Widevine supported AAC but not Vorbis when the player shipped).
Only two tiers exist, tied to subscription level rather than user choice:

| Tier    | Codec  | Bitrate  | Notes                                                                  |
| ------- | ------ | -------- | ---------------------------------------------------------------------- |
| Free    | AAC-LC | 128 kbps | What you get listening on `open.spotify.com` without a subscription.   |
| Premium | AAC-LC | 256 kbps | Premium subscribers on the web player. Different codec than the apps.  |

## YouTube Music

Two codecs, picked by client: AAC-LC on the iOS/Android apps, Opus on the
web player. Same bitrate per tier on both. Free users are capped at the
"Normal" tier (128 kbps). No lossless or hi-res audio at any subscription
level.

| Tier    | Codec (mobile / web) | Bitrate  | Notes                                                                                       |
| ------- | -------------------- | -------- | ------------------------------------------------------------------------------------------- |
| Low     | AAC-LC / Opus        | 48 kbps  | Data-saving mode. Available to Free and Premium. Audibly rough; this is the "tin-can" tier. |
| Normal  | AAC-LC / Opus        | 128 kbps | Default tier. Hard cap for Free users.                                                      |
| High    | AAC-LC / Opus        | 256 kbps | Premium-only. Mostly transparent for casual listening.                                      |

Quality is configured separately for Wi-Fi and cellular on the mobile apps.

YouTube Music also exposes an **"Always High"** option in the app settings.
It's not its own tier — it's a network-behavior flag that pins the stream to
256 kbps and disables the auto-downgrade on a poor connection. Same codec,
same bitrate, **bit-identical audio to "High"**. Because of that, we don't
expose it as a separate button: the plugin simulates encoders, not network
behavior, so the two would be indistinguishable and confuse users.

## Deezer

Single **codec** stack across every client — unlike YouTube Music, there's
no codec split between web and mobile. There *is* a small **tier
availability** difference (mobile-only Basic, different label for 320 kbps
on desktop), but the encoder is identical, so the plugin doesn't expose
per-client buttons. We surface the mobile lineup as the superset.

| Tier         | Codec | Bitrate    | Mobile           | Desktop / Web        | Notes                                                                              |
| ------------ | ----- | ---------- | ---------------- | -------------------- | ---------------------------------------------------------------------------------- |
| Basic        | MP3   | 64 kbps    | yes (Free + Premium) | not exposed       | Below MP3's "comfortable" range — sounds rough on most material.                   |
| Standard     | MP3   | 128 kbps   | yes              | yes                  | Default Deezer setting. Free users are capped here.                                |
| High Quality | MP3   | 320 kbps   | yes              | yes (labelled "Better") | Paid only. Same encode as mobile; just a different name in the desktop UI.      |
| HiFi         | FLAC  | ~1411 kbps | yes (Paid)       | yes (Paid)           | 16-bit / 44.1 kHz lossless. Bit-identical to source — nothing to simulate.         |

Deezer used to ship a 24-bit Hi-Res tier ("Sonic Activity" / "Hi-Res
Audio"), but that's been folded into HiFi at 16/44.1 in current plans.
No Atmos / spatial audio.

## Apple Music

Apple Music splits its delivery between a lossy AAC pipeline (using
Apple's own CoreAudio AAC encoder) and a lossless ALAC pipeline. Free
users don't exist — the lowest tier (High Efficiency) is a cellular
data-saving option for paid subscribers.

There **is** a small mobile-vs-web tier-availability difference, but the
codec/encoder for shared tiers is identical, so the plugin doesn't expose
per-client buttons. We expose the apps' superset (= 4 tiers, of which we
collapse the two ALAC ones).

| Tier              | Codec      | Bitrate / format          | iOS / Android / macOS apps | Web (music.apple.com) | Notes                                                                                                                                       |
| ----------------- | ---------- | ------------------------- | -------------------------- | --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| High Efficiency   | HE-AAC v1  | ~64 kbps                  | yes (cellular)             | not exposed           | Cellular streaming default in the iOS Music app's "Cellular Streaming → High Efficiency" setting. SBR but no Parametric Stereo (= real stereo encoded, unlike Spotify Low). |
| High Quality      | AAC-LC     | 256 kbps                  | yes                        | yes                   | Apple Music's "default" tier — what the iTunes Plus / Apple Digital Masters spec produces. Considered transparent to source for most listeners.                              |
| Lossless          | ALAC       | up to 24-bit / 48 kHz     | yes (Paid)                 | not exposed           | Lossless tier. Bit-identical to source — nothing to simulate.                                                                              |
| Hi-Res Lossless   | ALAC       | up to 24-bit / 192 kHz    | yes (Paid)                 | not exposed           | Highest tier. Also bit-identical to source.                                                                                                |

The plugin shows **one button** for both ALAC tiers — they'd both be
pure pass-through in our simulation, so two buttons doing exactly the
same thing would just confuse users. (Same exclusion logic that kept
YouTube Music's "Always High" off the UI.)

A separate **Dolby Atmos** tier (Dolby Digital Plus / E-AC-3, multichannel)
exists but is out of scope for this plugin — we only model stereo
encoding.

Loudness normalization is **Sound Check** (~ −16 LUFS playback target),
written into the m4a as metadata and applied client-side. We don't
simulate it; see the encoder-settings section above for why.

## Tidal

Tidal restructured significantly in 2024:
- **April 2024**: merged the old HiFi ($9.99/mo, FLAC) and HiFi Plus
  ($19.99/mo, MQA + Atmos) into a single $10.99/mo plan that includes
  everything.
- **July 2024**: dropped MQA entirely. The Max tier now serves **HiRes
  FLAC** instead of MQA, with Tidal migrating remaining MQA-only tracks
  to FLAC. Sony 360 Reality Audio was also removed.

Current tiers, exposed in the user-facing settings as a dropdown rather
than a per-bitrate ladder:

| Tier  | Codec        | Bitrate / format       | Notes                                                                                                                               |
| ----- | ------------ | ---------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Low   | AAC-LC       | up to 320 kbps         | Mobile / cellular default. Single bitrate from the user's perspective; Tidal handles adaptive delivery internally.                  |
| High  | FLAC         | 16-bit / 44.1 kHz      | CD-quality lossless. Bit-identical to source.                                                                                       |
| Max   | HiRes FLAC   | up to 24-bit / 192 kHz | Highest tier. Also bit-identical to source. Replaces MQA (removed July 2024).                                                       |

The plugin shows **2 buttons** for Tidal: Low (AAC-LC 320 kbps) and a
single "Lossless" bypass button covering both High and Max — same
exclusion logic as Apple Music (two tiers that produce bit-identical
output collapse into one). No Atmos: stereo plugin, out of scope.

No mobile-vs-web codec split. The web player at listen.tidal.com supports
the same Low / High / Max tiers as the apps (no missing tiers, unlike
Apple Music's web limitations).

Loudness normalization: Tidal applies **−14 LUFS album normalization**
by default on iOS / Android (turned on automatically since 2020 — see
[Production Advice][tidal-loudness] and [audioXpress][tidal-audiox]).
Like YouTube Music it's "down only" — louder masters get reduced, but
quieter masters aren't boosted. Also unlike the others it's **album-
based**: the loudest track in the album is normalised to −14 LUFS and
the rest of the album is offset to keep relative dynamics. Users can
override the target via a Pre-amp slider (−18 to −6 LUFS).

[tidal-loudness]: https://productionadvice.co.uk/tidal-loudness/
[tidal-audiox]: https://audioxpress.com/news/tidal-implements-album-loudness-normalization-and-activates-it-by-default-for-mobile-players

## Amazon Music

Amazon Music's tier matrix is **directly verified** from Amazon's own
developer documentation (link in the encoder-settings section above) —
the highest confidence we have for any platform's encoder choice. They
use a different codec stack than every other platform: **Opus** for
the lossy SD tier (most platforms here use AAC), **FLAC** for the
lossless HD/UHD tiers.

| Tier           | Codec             | Format / bitrate                         | Subscription                       | Notes                                                                                                                                  |
| -------------- | ----------------- | ---------------------------------------- | ---------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| SD Low         | Opus              | 48 kbps · 16-bit / 48 kHz                | Free, Prime, Unlimited             | Data-saver tier. Audibly rough — Opus is good at this bitrate but it's still 48 kbps stereo.                                          |
| SD Standard    | Opus              | 192 kbps · 16-bit / 48 kHz               | Free, Prime, Unlimited             | Adaptive default for most listeners not on a strong connection.                                                                       |
| SD High        | Opus              | 320 kbps · 16-bit / 48 kHz               | Free, Prime, Unlimited             | Top of the lossy tier. Mostly transparent.                                                                                            |
| HD             | FLAC              | ~800 kbps avg · 16-bit / 44.1 kHz        | Unlimited (paid)                   | CD-quality lossless. Bit-identical to source.                                                                                          |
| UHD            | FLAC              | ~1.6–5 Mbps avg · 24-bit / 44.1–192 kHz  | Unlimited (paid)                   | Hi-Res lossless. Also bit-identical.                                                                                                   |

The plugin shows **4 buttons** for Amazon Music — the three SD bitrates
plus a single "Lossless (bypass)" button covering both HD and UHD.

A separate **Atmos** tier (Dolby Digital Plus / E-AC-3 multichannel) and
**Sony 360 Reality Audio** tier (MPEG-H 3D) exist but are out of scope
for a stereo plugin.

Loudness normalization: **−14 LUFS, track-based, down-only**, on by
default. Same target as Spotify/YouTube/Apple/Tidal.

## SoundCloud

SoundCloud is in the middle of a **codec migration** as of 2025–2026
(see encoder-settings section for the source). The legacy MP3 128 +
Opus 64 stack is being replaced with AAC HLS at multiple bitrates.

| Tier              | Codec  | Bitrate    | Subscription / availability                                                  |
| ----------------- | ------ | ---------- | ---------------------------------------------------------------------------- |
| Low               | AAC-LC | 96 kbps    | Future low-bandwidth fallback (not yet rolled out as of April 2026).         |
| Standard          | AAC-LC | 160 kbps   | New default tier for all users — replacing MP3 128.                          |
| High Quality      | AAC-LC | 256 kbps   | SoundCloud Go+ subscribers, partner API only.                                |
| Legacy MP3        | MP3    | 128 kbps   | Still served for tracks not yet retranscoded; gradually disappearing.        |

The plugin shows **4 buttons** for SoundCloud — three AAC tiers + one
MP3 legacy tier. We expose all four because the artifact spectra are
audibly distinct (96/160/256 kbps AAC each sound different, and MP3 128
sounds different from any of them).

No mobile-vs-web codec split. Same files served to every client.

Loudness normalization: **−14 LUFS** is reported by multiple mastering
guides, but SoundCloud's behaviour here is poorly documented compared
to other platforms — some sources claim it normalizes both up and
down (unusual), some claim it doesn't normalize at all. Don't rely on
this when A/B-ing.

## YouTube (video)

Distinct from YouTube Music: this is the audio of regular YouTube
videos. Two codec arms run in parallel; which one your listener gets
depends on their client:

- **AAC-LC (M4A)** — served to Safari and most mobile clients.
- **Opus (WebM)** — served to Chromium-based browsers and Firefox.

| Codec  | itag | Bitrate    | When you'd hear this                                                                       |
| ------ | ---- | ---------- | ------------------------------------------------------------------------------------------ |
| AAC-LC | 139  | 48 kbps    | Low-bandwidth fallback on Safari / mobile.                                                 |
| AAC-LC | 140  | 128 kbps   | Default on Safari and most mobile clients.                                                 |
| AAC-LC | 141  | 256 kbps   | Rare — partner-only / YouTube Music sometimes.                                             |
| Opus   | 250  | ~70 kbps   | Low-bandwidth fallback on Chrome/Firefox/Edge.                                             |
| Opus   | 251  | ~160 kbps  | Default on Chrome/Firefox/Edge.                                                            |

The plugin shows **4 buttons** in two rows (mirroring the YouTube Music
tab structure):

- **Row 1 — Safari / mobile (AAC-LC)**: 48 kbps (low), 128 kbps (normal).
- **Row 2 — Chrome / Firefox (Opus)**: 70 kbps (low), 160 kbps (normal).

We omit itag 141 (AAC 256) because it's rarely served for general video
content. We omit itag 249 (Opus 50 kbps) for the same reason.

Sample rate is **48 kHz** for both codec arms (YouTube's universal native
rate). For the closest A/B match, run your project at 48 kHz.

Loudness normalization: **−14 LUFS, down-only** (same as YouTube Music).

## TikTok

TikTok isn't a music streaming service strictly speaking — it's a
short-form video platform — but lots of music gets first-listened-to
there, so it's a useful "where will my track end up" target for
producers. The audio is embedded in MP4 video files transcoded by
ByteDance's own pipeline.

| Tier                 | Codec  | Bitrate / format               | Notes                                                                                                              |
| -------------------- | ------ | ------------------------------ | ------------------------------------------------------------------------------------------------------------------ |
| Standard playback    | AAC-LC | ~128 kbps stereo · 44.1/48 kHz | What 99% of viewers hear in-app on iOS / Android / web.                                                            |
| Watermarked download | AAC-LC | ~64 kbps **mono** · 48 kHz     | Saved-to-device files (often reposted to other platforms). The plugin sums L+R before encoding and duplicates back to L=R after decode, so the artifact character matches real mono-AAC at 64 kbps — distinctly different from stereo at the same bitrate. |

The plugin shows **2 buttons** for TikTok — Standard and Watermarked
download. The 96 kbps stereo low-bandwidth tier isn't exposed (audibly
too close to Standard at 128 to add a third button).

**TikTok's loudness pipeline is unusual** — it applies aggressive DRC
+ a peak-normalising limiter rather than the LUFS-based gain offset
every other platform here uses. The plugin doesn't simulate the DRC
stage; you only hear the codec artifacts.

## Instagram (Reels / Stories)

Same shape as TikTok: video platform, audio embedded in MP4. Meta
migrated to **xHE-AAC** in 2023 (the next-gen AAC profile from
Fraunhofer); see encoder-settings notes above for what we model and
the fidelity gap that creates.

| Tier              | Codec (real) | Bitrate / format               | Plugin approximation                                                                                |
| ----------------- | ------------ | ------------------------------ | --------------------------------------------------------------------------------------------------- |
| Standard Reel     | xHE-AAC      | ~128 kbps stereo · 44.1/48 kHz | AAC-LC 128 kbps stereo (closest available)                                                          |
| Low bandwidth     | xHE-AAC      | ~96 kbps stereo · 44.1/48 kHz  | (not exposed — too close to Standard at 128 to add a third button)                                  |
| Poor connection   | xHE-AAC      | ~64 kbps **mono**              | AAC-LC 64 kbps **mono** (sum L+R → encode mono → duplicate L=R)                                     |

The plugin shows **2 buttons** for Instagram — Standard Reel and Poor
connection. Both are modelled with AAC-LC since the `fdk-aac` Rust
crate doesn't expose xHE-AAC; the artifact characters are in the right
family but not identical. For the exact "what xHE-AAC sounds like"
you'd need the Fraunhofer encoder SDK, which is commercial-only and
not part of `fdk-aac`'s Rust bindings as of 0.8.0.

## FM Radio

The only "platform" in the plugin that isn't a codec — it's a full
broadcast-chain simulation. We model the pipeline a real FM station
runs a master through (input AGC, broadcast EQ, multiband compressor,
hard clipper, pre-emphasis), the over-the-air transmission (MPX
stereo encoder, channel imperfection, MPX decoder), and the listener's
side (de-emphasis). See
[`docs/codec-implementation.md`](codec-implementation.md) for the
detailed DSP chain and per-stage filter coefficients.

FM is **not standardised end-to-end**. Some parts of the chain are
spec-defined; the parts that give commercial FM its loud, squashed
sound are vendor-tuned (Orban Optimod / Omnia.9 / Wheatstone Aura
presets) and vary station-to-station.

| Element                  | Standard / source                                                                | What we simulate                                                          |
| ------------------------ | -------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Audio bandwidth          | Universal hard 15 kHz cutoff (the 19 kHz stereo pilot has to live above)         | MPX decoder's 14 kHz LPF on sum recovery (decoder-side) + 4-kHz crossover band's natural HF rolloff |
| Pre-emphasis time const. | [ITU-R BS.450](https://www.itu.int/rec/R-REC-BS.450/) — **75 µs** Americas/Korea, **50 µs** Europe/Africa/Asia/Australia | Matched FIR pre-emphasis + IIR de-emphasis pair, exact mathematical inverses on linear material |
| Modulation peak limit    | FCC Part 73 (US, ≤100% modulation), [ITU-R BS.412](https://www.itu.int/rec/R-REC-BS.412/) (Europe, MPX power) | 2× oversampled hard clipper at -0.5 dBFS                                |
| Stereo MPX subcarrier    | 38 kHz DSBSC modulator coherent with 19 kHz pilot tone (FCC + ITU spec)         | Full MPX encoder + decoder running at 192 kHz internal, 9% pilot modulation depth |
| Multiband compression    | **Not standardised** — Orban / Omnia / Wheatstone presets vary station-to-station | 4-band Linkwitz-Riley crossover at 100 / 800 / 4000 Hz, per-band ratio/threshold/attack/release tuned to a "Pop Rock" preset, gain-share linked |
| Channel imperfection     | RF transmission + reception artifacts                                          | 3 reception-quality presets (Pristine / Urban / Fringe) with stereo separation collapse, HF noise, multipath swirl |

Topology: AGC → EQ → multiband (with per-band limiter) → pre-emphasis
→ hard clipper → MPX encoder → channel → MPX decoder → de-emphasis →
auto-makeup → output. Pre-emphasis sits *after* multiband (matches
real broadcast); de-emphasis perfectly inverts it on linear material,
so the HF distortion that defines the FM sound comes from the
non-linear processing in between.

### Tiers

Six buttons in the catalog: regional pre-emphasis crossed with
reception quality.

| Tier               | Region              | Reception | Audible character                                                                |
| ------------------ | ------------------- | --------- | -------------------------------------------------------------------------------- |
| FM · US Pristine   | 75 µs (FCC)         | Pristine  | Best-case North American FM. All audio-domain processing, perfect channel.       |
| FM · US Urban      | 75 µs               | Urban     | -6 dB stereo separation collapse, mild HF hiss in the 5-15 kHz band              |
| FM · US Fringe     | 75 µs               | Fringe    | Almost mono (-18 dB on L-R), audible HF hiss, slow multipath stereo swirl        |
| FM · EU Pristine   | 50 µs (ITU-R BS.450)| Pristine  | Best-case European FM. Slightly different HF tilt vs US Pristine (50 µs has less HF boost above 2 kHz) |
| FM · EU Urban      | 50 µs               | Urban     | European city reception                                                          |
| FM · EU Fringe     | 50 µs               | Fringe    | European fringe reception                                                        |

The 75 µs vs 50 µs difference shows up as a few dB of HF shelf shift
around 2-3 kHz; subtle but real on bright source material. The
reception-quality difference is much more audible — Fringe is
unmistakably "weak signal" with the stereo image collapsing toward
mono.

### Auto-makeup gain

The AGC + multiband + clipper combination pushes FM output 4-8 dB
louder than the input on a typical mastering-grade mix, which would
make A/B switching against the codec processors a loudness comparison
rather than an artifact comparison. The FM processor runs an
**auto-makeup gain stage** at the output: a long-term envelope
follower (~1 s time constant) on input vs output amplitude applies
the inverse ratio as makeup. Output level stays within ~1 dB of input
on programme material across all 6 tiers.

This mirrors the receive-side AGC every real broadcast processor
(Orban / Omnia / Wheatstone) runs as the last stage before the
listener.

### Internal sample rate

The MPX path needs to represent the 38 kHz subcarrier without
aliasing, so it runs at **192 kHz** internally. A rubato `FftFixedIn`
resampler pair handles host ↔ 192 kHz conversion; at 192 kHz host the
resamplers drop out. Latency cost: ~5-10 ms total round-trip.

Reported latency feeds into the plugin-wide `target_latency_samples`
mechanism — codec ↔ FM switches don't re-tick host PDC.

### What we don't model

- **Composite clipper (stage 9)**: real airchains often clip the
  post-MPX composite signal directly. We clip the audio domain only.
- **RDS data subcarrier (stage 10)**: 57 kHz, audibly inert (filtered
  by the 14 kHz decoder LP). Skipped.
- **Adjacent-channel interference**: receiver-side artifact, not
  airchain.
- **Station-specific multiband presets**: we pick one reasonable
  "Pop Rock" tuning rather than expose tunable parameters.

## Bluetooth listening

A separate global toggle (bottom-left of the editor) cascades a
Bluetooth codec roundtrip *on top of* whichever platform codec is
selected. Real listeners hear two lossy stages, not one — the
streaming service compresses to Vorbis / AAC / Opus / etc., the
device decodes it, then Bluetooth re-encodes for transmission to the
headphones — and this layer simulates the second stage so producers
can A/B "what does my mix sound like through cheap Bluetooth earbuds
on Spotify".

The cascade order is fixed: platform codec → Bluetooth codec. The
BT layer always sees post-platform-codec audio.

### Presets

Six presets cover the audibly meaningful range. The gear button next
to the BT toggle opens a popup with all six in this order (worst →
best):

| Preset             | Codec     | Config              | Audible character                                                                | Listener profile                       |
| ------------------ | --------- | ------------------- | -------------------------------------------------------------------------------- | -------------------------------------- |
| **SBC · Low**      | SBC       | bitpool 19          | Audible HF rolloff above ~14 kHz, transient smearing, occasional swoosh artifact | Cheap no-name earbuds, BT 4.x speakers |
| **SBC · High**     | SBC       | bitpool 53          | Mostly transparent, occasional swoosh on cymbals                                 | Default fallback for most BT headphones |
| **AAC · 128 kbps** | AAC-LC    | 128 kbps stereo     | "Android AAC bug" — old Android encoder produced audibly worse output than SBC at the same rate | Older Android over BT |
| **AAC · 256 kbps** | AAC-LC    | 256 kbps stereo     | Mostly transparent                                                               | iPhone + AirPods default               |
| **LC3 · 64 kbps**  | LC3       | mono, 64 kbps       | Cleaner than SBC at the same bitrate but mono                                    | LE Audio low-power mode                |
| **LC3 · 160 kbps** | LC3       | stereo, 80 kbps × 2 | Transparent on programme material                                                | LE Audio high-quality mode             |

### Codecs we deliberately skip

| Codec                                      | Why skipped                                                                                                                 |
| ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------- |
| **aptX** (Classic / HD / Adaptive / Lossless) | Qualcomm-proprietary. License-restricted. No FOSS encoder exists                                                         |
| **LDAC**                                   | Sony released the encoder under LGPL but the decoder is closed-source. We can't roundtrip                                   |
| **LHDC, LLAC, Samsung Scalable Codec**     | Vendor-proprietary, no FOSS bindings                                                                                        |
| **LC3plus**                                | Fraunhofer-patented superset of LC3. LGPL implementations exist but the patent situation is unclear for binary distribution |

### Latency

The BT layer adds ~30-50 ms of additional codec roundtrip + resampler
delay on top of the platform codec when enabled. This folds into the
plugin-wide reported latency via `BluetoothProcessor::worst_case_latency_at`.
PDC alignment is approximate when both layers are active; see
`src/lib.rs` for the latency-budget trade-off rationale.

