//! One-shot tool that recolors the Bluetooth + settings icon PNGs
//! to flat off-white silhouettes that match the editor's body text
//! tone. We re-run this whenever the source icons change, then
//! commit the recoloured outputs.
//!
//! The recolor rule is intentionally simple: every pixel with
//! non-zero alpha gets its RGB replaced with [`TARGET_COLOR`]. The
//! original alpha channel is preserved verbatim, so anti-aliased
//! edges and partial transparency stay intact and the silhouette
//! reads cleanly on any background colour.
//!
//! `TARGET_COLOR` is `(190, 190, 190)` — slightly dimmer than the
//! pure white we used initially. Pure white was harsh against the
//! dark UI; this matches the codec-button title text tone for
//! visual consistency.
//!
//! Run from the repo root:
//!     cargo run --example recolor_icons

use png::{BitDepth, ColorType, Decoder, Encoder};
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

const ICONS: &[&str] = &[
    "resources/bluetooth.png",
    "resources/settings.png",
];

/// Target greyscale value baked into every non-transparent pixel.
/// 190 ≈ the body-text tone in the egui dark theme — bright enough
/// to read clearly, less aggressive than pure 255.
const TARGET_GRAY: u8 = 190;

fn main() {
    for path in ICONS {
        match recolor_to_white(path) {
            Ok(()) => println!("recoloured {path}"),
            Err(e) => eprintln!("failed to recolour {path}: {e}"),
        }
    }
}

fn recolor_to_white(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(path);
    let file = File::open(path)?;
    let decoder = Decoder::new(file);
    let mut reader = decoder.read_info()?;

    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let width = info.width;
    let height = info.height;
    let color_type = info.color_type;
    let bit_depth = info.bit_depth;

    if bit_depth != BitDepth::Eight {
        return Err(format!("unsupported bit depth {:?}", bit_depth).into());
    }

    // Normalise to RGBA8 so we can write out a single uniform format
    // regardless of whether the source was indexed, grayscale, RGB,
    // or RGBA. Anything alpha-less gets a fully-opaque alpha channel.
    let rgba: Vec<u8> = match color_type {
        ColorType::Rgba => buf,
        ColorType::Rgb => buf
            .chunks(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        ColorType::GrayscaleAlpha => buf
            .chunks(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        ColorType::Grayscale => buf
            .iter()
            .flat_map(|&v| [v, v, v, 255])
            .collect(),
        ColorType::Indexed => {
            return Err(
                "indexed PNGs aren't handled — the source icons aren't this format"
                    .into(),
            );
        }
    };

    // Recolor: every pixel with non-zero alpha becomes the target
    // grey, alpha preserved. Pixels with alpha=0 stay alpha=0 so the
    // background of the source icon doesn't accidentally become
    // visible.
    let mut recoloured = Vec::with_capacity(rgba.len());
    for px in rgba.chunks(4) {
        let a = px[3];
        if a == 0 {
            recoloured.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            recoloured.extend_from_slice(&[TARGET_GRAY, TARGET_GRAY, TARGET_GRAY, a]);
        }
    }

    let out_file = File::create(path)?;
    let writer = BufWriter::new(out_file);
    let mut encoder = Encoder::new(writer, width, height);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&recoloured)?;

    Ok(())
}
