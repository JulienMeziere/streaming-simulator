//! Icon decoding + caching for the editor.
//!
//! Embedded PNGs are decoded once on first frame and cached in `UiState`.
//! Reloads happen on cold start and after the editor window is closed and
//! reopened (which tears down + rebuilds the GL context, invalidating
//! our `TextureHandle`s).

use nih_plug_egui::egui;

/// Pre-resize to ~2× displayed size so the GPU does negligible minification
/// and mipmaps handle the rest cleanly across HiDPI scales.
const ICON_TEXTURE_PX: u32 = 128;

/// True if the cache is empty or our handles point at textures the current
/// `Context` no longer knows about (post window-reopen).
pub(super) fn icons_need_reload(ctx: &egui::Context, icons: &[egui::TextureHandle]) -> bool {
    if icons.is_empty() {
        return true;
    }
    let mgr = ctx.tex_manager();
    let mgr = mgr.read();
    icons.iter().any(|h| mgr.meta(h.id()).is_none())
}

/// Decode `png_bytes`, aspect-fit onto a transparent square canvas, and
/// upload as a named egui texture. Only runs when [`icons_need_reload`]
/// reports a miss.
pub(super) fn load_icon(
    ctx: &egui::Context,
    name: &str,
    png_bytes: &[u8],
) -> egui::TextureHandle {
    use fast_image_resize::{
        images::Image as FirImage, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
    };

    // Decode to 8-bit RGBA. Transformations normalise any valid PNG
    // (palette / grayscale / 16-bit) to plain RGBA8 for the resize step.
    let mut decoder = png::Decoder::new(png_bytes);
    decoder.set_transformations(
        png::Transformations::ALPHA | png::Transformations::EXPAND | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().expect("failed to read PNG header");
    let info = reader.info();
    let (sw, sh) = (info.width, info.height);
    let mut src_buf = vec![0u8; reader.output_buffer_size()];
    reader
        .next_frame(&mut src_buf)
        .expect("failed to decode PNG");

    // Aspect-preserving fit: scale until the image touches one edge of
    // the ICON_TEXTURE_PX bounding box.
    let scale =
        (ICON_TEXTURE_PX as f32 / sw as f32).min(ICON_TEXTURE_PX as f32 / sh as f32);
    let rw = ((sw as f32 * scale).round() as u32).max(1);
    let rh = ((sh as f32 * scale).round() as u32).max(1);

    // Lanczos3 over **non-premultiplied** RGBA on purpose. Alpha-correct
    // premultiply (the technically-right choice on a transparent canvas)
    // loses 8-bit precision through the multiply→resize→divide round trip
    // and fades coloured edges toward the canvas. The "wrong" path keeps
    // the saturated edge colours the icons were authored against.
    let src_img = FirImage::from_vec_u8(sw, sh, src_buf, PixelType::U8x4)
        .expect("source PNG dimensions invalid for fir");
    let mut resized_img = FirImage::new(rw, rh, PixelType::U8x4);
    let mut resizer = Resizer::new();
    resizer
        .resize(
            &src_img,
            &mut resized_img,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .expect("Lanczos3 resize failed");
    let resized_buf = resized_img.into_vec();

    // Centre on a transparent square so non-square source PNGs don't get
    // stretched back to a square button at draw time.
    let canvas_side = ICON_TEXTURE_PX as usize;
    let mut square = vec![0u8; canvas_side * canvas_side * 4];
    let dx = ((ICON_TEXTURE_PX - rw) / 2) as usize;
    let dy = ((ICON_TEXTURE_PX - rh) / 2) as usize;
    let row_bytes = rw as usize * 4;
    for row in 0..rh as usize {
        let src_off = row * row_bytes;
        let dst_off = ((dy + row) * canvas_side + dx) * 4;
        square[dst_off..dst_off + row_bytes]
            .copy_from_slice(&resized_buf[src_off..src_off + row_bytes]);
    }

    let pixels = egui::ColorImage::from_rgba_unmultiplied([canvas_side, canvas_side], &square);
    ctx.load_texture(
        name,
        pixels,
        egui::TextureOptions {
            magnification: egui::TextureFilter::Linear,
            minification: egui::TextureFilter::Linear,
            // Trilinear-style minification for crisp HiDPI rendering.
            mipmap_mode: Some(egui::TextureFilter::Linear),
            ..Default::default()
        },
    )
}
