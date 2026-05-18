//! Catalog of streaming platforms and the codec tiers each one offers.
//!
//! Every fact about every codec lives in the `catalog!` invocation below. The
//! macro expands to the `Codec` enum (with `nih_plug::Enum` attrs for DAW
//! persistence) and the `PLATFORMS` static iterated by the editor and DSP
//! layers. Adding a codec = one line; adding a platform = one block.

use nih_plug::prelude::Enum;
use std::sync::OnceLock;

pub use crate::processor::fm_mpx::FmReception;
pub use crate::processor::fm_radio::{FmRadioVariant, FmRegion};

pub struct PlatformDef {
    /// Stable identifier — persisted in DAW state. Never change post-release.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Raw PNG bytes for the tab icon, embedded at compile time.
    pub icon_png: &'static [u8],
    /// Tiers ordered left-to-right as shown in the codec row.
    pub codecs: &'static [CodecDef],
}

pub struct CodecDef {
    pub codec: Codec,
    /// Big label on the codec button (e.g. `"Normal"`).
    pub short_label: &'static str,
    /// Small subtitle (e.g. `"Vorbis · 96 kbps"`).
    pub format_label: &'static str,
    /// 1-based UI row. Most platforms only use row 1; platforms with parallel
    /// codec paths (e.g. YouTube Music's mobile AAC + web Opus) split across
    /// rows 1 and 2.
    pub row: u8,
    pub spec: CodecSpec,
}

/// DSP recipe for a codec. Plain config data; actual processing lives in
/// `processor::*` and is dispatched in `StreamingSimulator::dispatch_buffer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecSpec {
    /// Output equals input. Used for every Lossless / FLAC / ALAC tier.
    Bypass,
    Opus { bitrate_kbps: u32 },
    /// libvorbis through raw packets (no Ogg container).
    Vorbis { bitrate_kbps: u32 },
    /// FDK-AAC AAC-LC. Gated behind the `fdk-aac` cargo feature; falls
    /// through to bypass when absent. See `docs/licensing.md`.
    AacLc { bitrate_kbps: u32 },
    /// AAC-LC encoded as mono: input is summed `(L+R)/2` before encode, then
    /// duplicated to L=R after decode. Mono at the same bitrate has a
    /// distinctly different artifact spectrum (full bit budget per channel,
    /// no stereo-coupling decisions). Used for TikTok watermarked downloads
    /// and Instagram poor-connection fallback.
    AacLcMono { bitrate_kbps: u32 },
    /// FDK-AAC HE-AAC v1 (AAC-LC core + SBR, no PS). Distinct from `HeAacV2`.
    HeAacV1 { bitrate_kbps: u32 },
    /// FDK-AAC HE-AAC v2 (AAC-LC core + SBR + PS).
    HeAacV2 { bitrate_kbps: u32 },
    /// LAME (encode) + minimp3 (decode). MPEG-1/2 Layer 3.
    Mp3 { bitrate_kbps: u32 },
    /// Full FM broadcast-chain simulation — not a codec, but a multi-stage
    /// DSP path. See [`crate::processor::fm_radio`] for the chain.
    FmRadio { variant: FmRadioVariant },
}

/// Generates the `Codec` enum and the `PLATFORMS` static from a declarative
/// block. Per-codec syntax:
///
/// ```text
/// VariantIdent "stable-id" "DAW display name" "Short label" "Format · bitrate" row N => CodecSpec::…
/// ```
///
/// `stable-id` is persisted in DAW projects — never change it after release.
macro_rules! catalog {
    {
        $(
            platform $platform_id:literal $platform_name:literal $icon:literal {
                $(
                    $variant:ident $codec_id:literal $daw_name:literal $short:literal $format:literal row $row:literal => $spec:expr
                ),* $(,)?
            }
        )*
    } => {
        #[derive(Enum, PartialEq, Eq, Clone, Copy, Debug)]
        pub enum Codec {
            $(
                $(
                    #[id = $codec_id]
                    #[name = $daw_name]
                    $variant,
                )*
            )*
        }

        pub static PLATFORMS: &[PlatformDef] = &[
            $(
                PlatformDef {
                    id: $platform_id,
                    display_name: $platform_name,
                    icon_png: include_bytes!($icon),
                    codecs: &[
                        $(
                            CodecDef {
                                codec: Codec::$variant,
                                short_label: $short,
                                format_label: $format,
                                row: $row,
                                spec: $spec,
                            },
                        )*
                    ],
                },
            )*
        ];
    };
}

catalog! {
    platform "spotify" "Spotify" "../resources/spotify.png" {
        // Row 1 = desktop/mobile apps (Vorbis + HE-AAC v2 + FLAC bypass).
        // Row 2 = web player (AAC-LC, separate Free/Premium bitrates).
        SpotifyLow         "spotify-low"          "Spotify · Low"               "Low"               "HE-AAC v2 · 24 kbps"     row 1 => CodecSpec::HeAacV2 { bitrate_kbps: 24 },
        SpotifyNormal      "spotify-normal"       "Spotify · Normal"            "Normal"            "Vorbis · 96 kbps"        row 1 => CodecSpec::Vorbis { bitrate_kbps: 96 },
        SpotifyHigh        "spotify-high"         "Spotify · High"              "High"              "Vorbis · 160 kbps"       row 1 => CodecSpec::Vorbis { bitrate_kbps: 160 },
        SpotifyVeryHigh    "spotify-very-high"    "Spotify · Very High"         "Very High"         "Vorbis · 320 kbps"       row 1 => CodecSpec::Vorbis { bitrate_kbps: 320 },
        SpotifyLossless    "spotify-lossless"     "Spotify · Lossless (bypass)" "Lossless (bypass)" "FLAC · pass-through"     row 1 => CodecSpec::Bypass,
        SpotifyWebFree     "spotify-web-free"     "Spotify · Free (web)"        "Free"              "AAC-LC · 128 kbps (web)" row 2 => CodecSpec::AacLc { bitrate_kbps: 128 },
        SpotifyWebPremium  "spotify-web-premium"  "Spotify · Premium (web)"     "Premium"           "AAC-LC · 256 kbps (web)" row 2 => CodecSpec::AacLc { bitrate_kbps: 256 },
    }

    platform "deezer" "Deezer" "../resources/deezer.png" {
        // Same codec stack on every client — only tier availability differs
        // between mobile / desktop. We expose the mobile superset.
        DeezerBasic       "deezer-basic"        "Deezer · Basic"          "Basic"        "MP3 · 64 kbps"         row 1 => CodecSpec::Mp3 { bitrate_kbps: 64 },
        DeezerStandard    "deezer-standard"     "Deezer · Standard"       "Standard"     "MP3 · 128 kbps"        row 1 => CodecSpec::Mp3 { bitrate_kbps: 128 },
        DeezerHighQuality "deezer-high-quality" "Deezer · High Quality"   "High Quality" "MP3 · 320 kbps"        row 1 => CodecSpec::Mp3 { bitrate_kbps: 320 },
        DeezerHiFi        "deezer-hifi"         "Deezer · HiFi (bypass)"  "HiFi (bypass)" "FLAC · pass-through"  row 1 => CodecSpec::Bypass,
    }

    platform "apple-music" "Apple Music" "../resources/apple-music.png" {
        // Real Apple uses CoreAudio AAC (closed-source, macOS-only) — we
        // approximate with FDK-AAC AAC-LC at the same bitrate. High
        // Efficiency is HE-AAC v1 (no PS, distinct from Spotify Low's v2).
        // Lossless + Hi-Res Lossless collapse into one bypass tier — both
        // are ALAC, both bit-identical to source. See docs/codecs.md.
        AppleMusicHighEfficiency "apple-music-high-efficiency" "Apple Music · High Efficiency" "High Efficiency"  "HE-AAC v1 · 64 kbps"   row 1 => CodecSpec::HeAacV1 { bitrate_kbps: 64 },
        AppleMusicHighQuality    "apple-music-high-quality"    "Apple Music · High Quality"    "High Quality"     "AAC-LC · 256 kbps"     row 1 => CodecSpec::AacLc { bitrate_kbps: 256 },
        AppleMusicLossless       "apple-music-lossless"        "Apple Music · Lossless (bypass)" "Lossless (bypass)" "ALAC · pass-through" row 1 => CodecSpec::Bypass,
    }

    platform "youtube-music" "YouTube Music" "../resources/youtube-music.png" {
        // Row 1 = mobile (AAC-LC), Row 2 = web (Opus). "Always High" is a
        // network-behavior flag (bit-identical to "High") so it's not exposed.
        YtMusicLowMobile    "yt-music-low-mobile"    "YT Music · Low (mobile)"    "Low"    "AAC-LC · 48 kbps (mobile)"   row 1 => CodecSpec::AacLc { bitrate_kbps: 48 },
        YtMusicNormalMobile "yt-music-normal-mobile" "YT Music · Normal (mobile)" "Normal" "AAC-LC · 128 kbps (mobile)"  row 1 => CodecSpec::AacLc { bitrate_kbps: 128 },
        YtMusicHighMobile   "yt-music-high-mobile"   "YT Music · High (mobile)"   "High"   "AAC-LC · 256 kbps (mobile)"  row 1 => CodecSpec::AacLc { bitrate_kbps: 256 },
        YtMusicLowWeb       "yt-music-low-web"       "YT Music · Low (web)"       "Low"    "Opus · 48 kbps (web)"        row 2 => CodecSpec::Opus { bitrate_kbps: 48 },
        YtMusicNormalWeb    "yt-music-normal-web"    "YT Music · Normal (web)"    "Normal" "Opus · 128 kbps (web)"       row 2 => CodecSpec::Opus { bitrate_kbps: 128 },
        YtMusicHighWeb      "yt-music-high-web"      "YT Music · High (web)"      "High"   "Opus · 256 kbps (web)"       row 2 => CodecSpec::Opus { bitrate_kbps: 256 },
    }

    platform "soundcloud" "SoundCloud" "../resources/soundcloud.png" {
        // Mid-migration from MP3+Opus to AAC HLS as of 2025-2026 — we expose
        // the new AAC tiers plus the legacy MP3 128 still served on
        // un-retranscoded older tracks. See docs/codecs.md for the source.
        SoundCloudLow      "soundcloud-low"      "SoundCloud · Low"          "Low"          "AAC-LC · 96 kbps"    row 1 => CodecSpec::AacLc { bitrate_kbps: 96 },
        SoundCloudStandard "soundcloud-standard" "SoundCloud · Standard"     "Standard"     "AAC-LC · 160 kbps"   row 1 => CodecSpec::AacLc { bitrate_kbps: 160 },
        SoundCloudGoPlus   "soundcloud-go-plus"  "SoundCloud · Go+ HQ"       "Go+ HQ"       "AAC-LC · 256 kbps"   row 1 => CodecSpec::AacLc { bitrate_kbps: 256 },
        SoundCloudLegacy   "soundcloud-legacy"   "SoundCloud · Legacy MP3"   "Legacy"       "MP3 · 128 kbps"      row 1 => CodecSpec::Mp3 { bitrate_kbps: 128 },
    }

    platform "tidal" "Tidal" "../resources/tidal.png" {
        // Post-2024 Tidal: 3 user-visible tiers (Low / High / Max). High +
        // Max collapse to one bypass button (both are FLAC, bit-identical).
        TidalLow      "tidal-low"      "Tidal · Low"               "Low"               "AAC-LC · 320 kbps"   row 1 => CodecSpec::AacLc { bitrate_kbps: 320 },
        TidalLossless "tidal-lossless" "Tidal · Lossless (bypass)" "Lossless (bypass)" "FLAC · pass-through" row 1 => CodecSpec::Bypass,
    }

    platform "amazon-music" "Amazon Music" "../resources/amazon-music.png" {
        // Only major streaming platform using **Opus** for its lossy tier —
        // verified directly from Amazon's developer docs. HD + UHD both
        // collapse to one bypass (FLAC, bit-identical).
        AmazonMusicLow      "amazon-music-low"      "Amazon Music · Low"               "Low"               "Opus · 48 kbps"      row 1 => CodecSpec::Opus { bitrate_kbps: 48 },
        AmazonMusicStandard "amazon-music-standard" "Amazon Music · Standard"          "Standard"          "Opus · 192 kbps"     row 1 => CodecSpec::Opus { bitrate_kbps: 192 },
        AmazonMusicHigh     "amazon-music-high"     "Amazon Music · High"              "High"              "Opus · 320 kbps"     row 1 => CodecSpec::Opus { bitrate_kbps: 320 },
        AmazonMusicLossless "amazon-music-lossless" "Amazon Music · Lossless (bypass)" "Lossless (bypass)" "FLAC · pass-through" row 1 => CodecSpec::Bypass,
    }

    platform "youtube" "YouTube" "../resources/youtube.png" {
        // Video (distinct from YouTube Music). Row 1 = AAC (Safari / mobile),
        // Row 2 = Opus (Chrome / Firefox / Edge). Bitrates from yt-dlp itag
        // tables. YouTube serves AAC at 48 kHz; our AacProcessor is 44.1 kHz
        // internally, so these tiers add one resample pass — documented gap.
        YouTubeAacLow    "youtube-aac-low"    "YouTube · AAC 48 kbps (Safari)"   "Low"    "AAC-LC · 48 kbps (Safari)"  row 1 => CodecSpec::AacLc { bitrate_kbps: 48 },
        YouTubeAacNormal "youtube-aac-normal" "YouTube · AAC 128 kbps (Safari)"  "Normal" "AAC-LC · 128 kbps (Safari)" row 1 => CodecSpec::AacLc { bitrate_kbps: 128 },
        YouTubeOpusLow   "youtube-opus-low"   "YouTube · Opus 70 kbps (web)"     "Low"    "Opus · 70 kbps (web)"       row 2 => CodecSpec::Opus { bitrate_kbps: 70 },
        YouTubeOpusHigh  "youtube-opus-high"  "YouTube · Opus 160 kbps (web)"    "Normal" "Opus · 160 kbps (web)"      row 2 => CodecSpec::Opus { bitrate_kbps: 160 },
    }

    platform "tiktok" "TikTok" "../resources/tiktok.png" {
        // AAC-LC in MP4. Standard = 128 kbps stereo in-app; Watermarked = the
        // 64 kbps mono baked into saved-to-device files. We model the codec
        // only, not TikTok's aggressive DRC + peak-limiter loudness stage.
        TikTokStandard   "tiktok-standard"   "TikTok · Standard"             "Standard"             "AAC-LC · 128 kbps"          row 1 => CodecSpec::AacLc { bitrate_kbps: 128 },
        TikTokWatermark  "tiktok-watermark"  "TikTok · Watermarked download" "Watermarked download" "AAC-LC · 64 kbps mono"      row 1 => CodecSpec::AacLcMono { bitrate_kbps: 64 },
    }

    platform "instagram" "Instagram" "../resources/instagram.png" {
        // Real Instagram uses xHE-AAC, but the `fdk-aac` crate doesn't expose
        // that profile — we approximate with AAC-LC at the same bitrate.
        // Largest fidelity gap of any tier we model. See docs/codecs.md.
        InstagramStandard  "instagram-standard"  "Instagram · Standard Reel"     "Standard"        "AAC-LC · 128 kbps (≈xHE-AAC)" row 1 => CodecSpec::AacLc { bitrate_kbps: 128 },
        InstagramPoorConn  "instagram-poor"      "Instagram · Poor connection"   "Poor connection" "AAC-LC · 64 kbps mono"        row 1 => CodecSpec::AacLcMono { bitrate_kbps: 64 },
    }

    platform "fm-radio" "FM Radio" "../resources/fm-radio.png" {
        // Full broadcast-airchain simulation, not a codec — see
        // `processor/fm_radio.rs` and docs/codec-implementation.md for the
        // chain. 6 tiers = 2 regional time constants (US 75 µs / EU 50 µs)
        // × 3 reception qualities (Pristine / Urban / Fringe).
        FmRadioUsPristine "fm-radio-us-pristine" "FM Radio · US Pristine" "US"      "75 µs · pristine reception" row 1 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Us75us, reception: FmReception::Pristine } },
        FmRadioUsUrban    "fm-radio-us-urban"    "FM Radio · US Urban"    "US"      "75 µs · urban reception"    row 1 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Us75us, reception: FmReception::Urban } },
        FmRadioUsFringe   "fm-radio-us-fringe"   "FM Radio · US Fringe"   "US"      "75 µs · fringe reception"   row 1 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Us75us, reception: FmReception::Fringe } },
        FmRadioEuPristine "fm-radio-eu-pristine" "FM Radio · EU Pristine" "Europe"  "50 µs · pristine reception" row 2 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Eu50us, reception: FmReception::Pristine } },
        FmRadioEuUrban    "fm-radio-eu-urban"    "FM Radio · EU Urban"    "Europe"  "50 µs · urban reception"    row 2 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Eu50us, reception: FmReception::Urban } },
        FmRadioEuFringe   "fm-radio-eu-fringe"   "FM Radio · EU Fringe"   "Europe"  "50 µs · fringe reception"   row 2 => CodecSpec::FmRadio { variant: FmRadioVariant { region: FmRegion::Eu50us, reception: FmReception::Fringe } },
    }
}

/// Per-`Codec` metadata indexed by `Codec::to_index()`, built once via
/// `OnceLock` so `Codec::def()` (called every audio block) is an O(1) array
/// index instead of a linear scan over ~50 catalog entries.
struct CodecLookup {
    defs: Box<[&'static CodecDef]>,
    platforms_for_codec: Box<[&'static PlatformDef]>,
}

fn codec_lookup() -> &'static CodecLookup {
    static CACHE: OnceLock<CodecLookup> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut pairs: Vec<(&'static PlatformDef, &'static CodecDef)> = PLATFORMS
            .iter()
            .flat_map(|p| p.codecs.iter().map(move |c| (p, c)))
            .collect();
        pairs.sort_by_key(|(_, c)| c.codec.to_index());
        let defs: Box<[_]> = pairs.iter().map(|&(_, c)| c).collect();
        let platforms_for_codec: Box<[_]> = pairs.iter().map(|&(p, _)| p).collect();
        CodecLookup {
            defs,
            platforms_for_codec,
        }
    })
}

impl Codec {
    /// O(1) lookup of this codec's catalog entry.
    pub fn def(self) -> &'static CodecDef {
        codec_lookup().defs[self.to_index()]
    }

    /// O(1) lookup of the platform owning this codec.
    pub fn platform(self) -> &'static PlatformDef {
        codec_lookup().platforms_for_codec[self.to_index()]
    }
}

#[cfg(test)]
mod tests {
    //! Coverage for catalog well-formedness and the `OnceLock` lookups. Most
    //! tests iterate `Codec::variants()` so they pick up new tiers for free.
    use super::*;
    use crate::processor::bluetooth::BluetoothProtocol;

    /// nih-plug's `Enum::variants()` returns variant *names*, not values, so
    /// we materialise the actual `Codec`s here for index iteration.
    fn all_codec_variants() -> Vec<Codec> {
        let n = <Codec as Enum>::variants().len();
        (0..n).map(Codec::from_index).collect()
    }

    #[test]
    fn every_codec_resolves_to_a_def_and_platform() {
        for codec in all_codec_variants() {
            let def = codec.def();
            assert_eq!(
                def.codec, codec,
                "Codec::def() for {codec:?} returned a def for {:?}",
                def.codec
            );
            let platform = codec.platform();
            assert!(
                platform.codecs.iter().any(|c| c.codec == codec),
                "Codec::platform() for {codec:?} returned platform {} \
                 whose codecs slice does not contain it",
                platform.id
            );
        }
    }

    #[test]
    fn codec_lookup_is_idempotent() {
        let first = codec_lookup();
        let second = codec_lookup();
        assert_eq!(first.defs.len(), second.defs.len());
        assert_eq!(first as *const _, second as *const _);
    }

    #[test]
    fn catalog_has_expected_platforms() {
        let ids: Vec<&str> = PLATFORMS.iter().map(|p| p.id).collect();
        for expected in &[
            "spotify",
            "deezer",
            "apple-music",
            "youtube-music",
            "soundcloud",
            "tidal",
            "amazon-music",
            "youtube",
            "tiktok",
            "instagram",
            "fm-radio",
        ] {
            assert!(
                ids.contains(expected),
                "PLATFORMS missing expected id `{expected}`; got {ids:?}"
            );
        }
    }

    #[test]
    fn catalog_well_formed() {
        for platform in PLATFORMS.iter() {
            assert!(
                !platform.id.is_empty(),
                "platform with empty id (display_name = {})",
                platform.display_name
            );
            assert!(
                !platform.display_name.is_empty(),
                "platform `{}` has empty display_name",
                platform.id
            );
            assert!(
                !platform.icon_png.is_empty(),
                "platform `{}` has empty icon_png",
                platform.id
            );
            assert!(
                !platform.codecs.is_empty(),
                "platform `{}` has zero codecs",
                platform.id
            );
            for codec_def in platform.codecs.iter() {
                assert!(
                    !codec_def.short_label.is_empty(),
                    "platform `{}` codec {:?} has empty short_label",
                    platform.id,
                    codec_def.codec
                );
                assert!(
                    !codec_def.format_label.is_empty(),
                    "platform `{}` codec {:?} has empty format_label",
                    platform.id,
                    codec_def.codec
                );
                assert!(
                    matches!(codec_def.row, 1 | 2),
                    "platform `{}` codec {:?} has unexpected row {} (only 1 or 2 are used)",
                    platform.id,
                    codec_def.codec,
                    codec_def.row
                );
                match codec_def.spec {
                    CodecSpec::Bypass | CodecSpec::FmRadio { .. } => {}
                    CodecSpec::Opus { bitrate_kbps }
                    | CodecSpec::Vorbis { bitrate_kbps }
                    | CodecSpec::AacLc { bitrate_kbps }
                    | CodecSpec::AacLcMono { bitrate_kbps }
                    | CodecSpec::HeAacV1 { bitrate_kbps }
                    | CodecSpec::HeAacV2 { bitrate_kbps }
                    | CodecSpec::Mp3 { bitrate_kbps } => {
                        assert!(
                            bitrate_kbps > 0,
                            "platform `{}` codec {:?} has bitrate 0",
                            platform.id,
                            codec_def.codec
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn no_duplicate_platform_ids() {
        let mut ids: Vec<&str> = PLATFORMS.iter().map(|p| p.id).collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate platform id detected");
    }

    #[test]
    fn every_codec_belongs_to_exactly_one_platform() {
        for codec in all_codec_variants() {
            let owners: Vec<&str> = PLATFORMS
                .iter()
                .filter(|p| p.codecs.iter().any(|c| c.codec == codec))
                .map(|p| p.id)
                .collect();
            assert_eq!(
                owners.len(),
                1,
                "{codec:?} belongs to {} platforms (expected 1): {owners:?}",
                owners.len()
            );
        }
    }

    #[test]
    fn bluetooth_protocol_short_labels_match_expected_strings() {
        assert_eq!(BluetoothProtocol::SbcLow.short_label(), "SBC · Low");
        assert_eq!(BluetoothProtocol::SbcHigh.short_label(), "SBC · High");
        assert_eq!(BluetoothProtocol::Aac128.short_label(), "AAC · 128 kbps");
        assert_eq!(BluetoothProtocol::Aac256.short_label(), "AAC · 256 kbps");
        assert_eq!(BluetoothProtocol::Lc3_64.short_label(), "LC3 · 64 kbps");
        assert_eq!(BluetoothProtocol::Lc3_160.short_label(), "LC3 · 160 kbps");
    }

    #[test]
    fn fm_radio_platform_has_six_variants() {
        let fm = PLATFORMS
            .iter()
            .find(|p| p.id == "fm-radio")
            .expect("fm-radio platform present");
        assert_eq!(fm.codecs.len(), 6, "2 regions × 3 receptions = 6 tiers");
        for c in fm.codecs.iter() {
            assert!(
                matches!(c.spec, CodecSpec::FmRadio { .. }),
                "fm-radio codec {:?} should have FmRadio spec",
                c.codec
            );
        }
    }

    /// Regression guard: every `CodecSpec` variant must be used by at least
    /// one tier — no orphan specs introduced and forgotten.
    #[test]
    fn codec_spec_variants_cover_real_codec_set() {
        let mut seen_bypass = false;
        let mut seen_opus = false;
        let mut seen_vorbis = false;
        let mut seen_aac_lc = false;
        let mut seen_aac_mono = false;
        let mut seen_he_aac_v1 = false;
        let mut seen_he_aac_v2 = false;
        let mut seen_mp3 = false;
        let mut seen_fm_radio = false;
        for platform in PLATFORMS.iter() {
            for c in platform.codecs.iter() {
                match c.spec {
                    CodecSpec::Bypass => seen_bypass = true,
                    CodecSpec::Opus { .. } => seen_opus = true,
                    CodecSpec::Vorbis { .. } => seen_vorbis = true,
                    CodecSpec::AacLc { .. } => seen_aac_lc = true,
                    CodecSpec::AacLcMono { .. } => seen_aac_mono = true,
                    CodecSpec::HeAacV1 { .. } => seen_he_aac_v1 = true,
                    CodecSpec::HeAacV2 { .. } => seen_he_aac_v2 = true,
                    CodecSpec::Mp3 { .. } => seen_mp3 = true,
                    CodecSpec::FmRadio { .. } => seen_fm_radio = true,
                }
            }
        }
        assert!(seen_bypass, "no Bypass tier in catalog");
        assert!(seen_opus, "no Opus tier in catalog");
        assert!(seen_vorbis, "no Vorbis tier in catalog");
        assert!(seen_aac_lc, "no AacLc tier in catalog");
        assert!(seen_aac_mono, "no AacLcMono tier in catalog");
        assert!(seen_he_aac_v1, "no HeAacV1 tier in catalog");
        assert!(seen_he_aac_v2, "no HeAacV2 tier in catalog");
        assert!(seen_mp3, "no Mp3 tier in catalog");
        assert!(seen_fm_radio, "no FmRadio tier in catalog");
    }
}
