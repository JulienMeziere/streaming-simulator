//! Drawing primitives: row layouts, button widgets, and the BT popup.
//! Paint-only — decision-bearing state lives in [`super`](crate::editor).

use crate::editor::{rebuild_codec_rows_if_stale, UiState};
use crate::processor::bluetooth::BluetoothProtocol;
use crate::{StreamingSimulatorParams, PLATFORMS};
use nih_plug::prelude::*;
use nih_plug_egui::egui;
use std::sync::Arc;

// ── Widget sizing ──────────────────────────────────────────────────

pub(super) const PLATFORM_BUTTON_SIZE: f32 = 56.0;
pub(super) const PLATFORM_BUTTON_PADDING: f32 = 10.0;
pub(super) const PLATFORM_BUTTON_FOOTPRINT: f32 =
    PLATFORM_BUTTON_SIZE + 2.0 * PLATFORM_BUTTON_PADDING;

pub(super) const CODEC_BUTTON_W: f32 = 180.0;
pub(super) const CODEC_BUTTON_H: f32 = 60.0;
pub(super) const CODEC_BUTTON_SPACING: f32 = 8.0;
pub(super) const CODEC_ROW_GAP: f32 = 8.0;

pub(super) const BYPASS_BUTTON_W: f32 = 96.0;
pub(super) const BYPASS_BUTTON_H: f32 = 26.0;
pub(super) const BYPASS_ROW_PAD: f32 = 12.0;

/// Bottom-left BT pill: two square halves with only the outer corners
/// rounded, sized to balance the bypass button on the right.
pub(super) const BT_BUTTON_SIZE: f32 = 28.0;
pub(super) const BT_PILL_RADIUS: u8 = 4;
pub(super) const BT_ROW_PAD: f32 = 12.0;

/// Top-right "?" info button.
pub(super) const INFO_BUTTON_SIZE: f32 = 24.0;
pub(super) const INFO_ROW_PAD: f32 = 12.0;

// ── Row drawers ────────────────────────────────────────────────────

pub(super) fn draw_platform_row(ui: &mut egui::Ui, state: &mut UiState) {
    let n = PLATFORMS.len();
    let row_w =
        n as f32 * PLATFORM_BUTTON_FOOTPRINT + n.saturating_sub(1) as f32 * CODEC_BUTTON_SPACING;

    centered_row(ui, row_w, |ui| {
        for (i, platform) in PLATFORMS.iter().enumerate() {
            let icon = match state.icons.get(i) {
                Some(t) => t,
                None => continue,
            };
            let selected = state.selected_platform_idx == Some(i);
            let resp = platform_button(ui, icon, platform.display_name, selected);
            if resp.clicked() {
                state.selected_platform_idx = Some(i);
            }
        }
    });
}

pub(super) fn draw_codec_row(
    ui: &mut egui::Ui,
    params: &Arc<StreamingSimulatorParams>,
    setter: &ParamSetter<'_>,
    platform_idx: usize,
    state: &mut UiState,
) {
    let Some(platform) = PLATFORMS.get(platform_idx) else {
        return;
    };
    let current = params.codec.value();

    // Catalog is static, so the row partition only changes on tab switch.
    // Caching avoids a `filter().collect()` per frame at egui's redraw rate.
    rebuild_codec_rows_if_stale(state, platform_idx, platform);

    let total_rows = state.codec_rows.len();
    for (row_idx, row_indices) in state.codec_rows.iter().enumerate() {
        let row_w = row_indices.len() as f32 * CODEC_BUTTON_W
            + row_indices.len().saturating_sub(1) as f32 * CODEC_BUTTON_SPACING;

        centered_row(ui, row_w, |ui| {
            ui.spacing_mut().item_spacing.x = CODEC_BUTTON_SPACING;
            for &codec_idx in row_indices {
                let codec_def = &platform.codecs[codec_idx];
                let resp = codec_button(
                    ui,
                    codec_def.short_label,
                    codec_def.format_label,
                    current == codec_def.codec,
                );
                if resp.clicked() && current != codec_def.codec {
                    setter.begin_set_parameter(&params.codec);
                    setter.set_parameter(&params.codec, codec_def.codec);
                    setter.end_set_parameter(&params.codec);
                }
            }
        });

        if row_idx + 1 < total_rows {
            ui.add_space(CODEC_ROW_GAP);
        }
    }
}

// ── Button widgets ─────────────────────────────────────────────────

/// Platform tab: framed icon on top, label centred below. Click sense
/// covers the whole footprint so the label is a hit target too.
fn platform_button(
    ui: &mut egui::Ui,
    icon: &egui::TextureHandle,
    label: &str,
    selected: bool,
) -> egui::Response {
    const LABEL_FONT_PX: f32 = 11.0;
    const LABEL_GAP: f32 = 4.0;
    const LABEL_HEIGHT: f32 = LABEL_FONT_PX + 4.0;
    const ICON_BOX: f32 = PLATFORM_BUTTON_FOOTPRINT;

    let total_size = egui::vec2(ICON_BOX, ICON_BOX + LABEL_GAP + LABEL_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(total_size, egui::Sense::click());
    let icon_rect = egui::Rect::from_min_size(rect.min, egui::vec2(ICON_BOX, ICON_BOX));

    // `interact_selectable` returns hover/pressed/selected tints sourced
    // from the theme so we don't hard-code colours.
    let visuals = ui.style().interact_selectable(&response, selected);
    let painter = ui.painter();
    let corner = egui::CornerRadius::same(4);
    painter.rect(
        icon_rect,
        corner,
        visuals.bg_fill,
        visuals.bg_stroke,
        egui::StrokeKind::Inside,
    );

    let image_rect = icon_rect.shrink(PLATFORM_BUTTON_PADDING);
    egui::Image::new((icon.id(), image_rect.size())).paint_at(ui, image_rect);

    let label_rect = egui::Rect::from_min_size(
        egui::pos2(rect.min.x, icon_rect.max.y + LABEL_GAP),
        egui::vec2(ICON_BOX, LABEL_HEIGHT),
    );
    let text_color = if selected {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().text_color()
    };
    ui.painter().text(
        label_rect.center(),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(LABEL_FONT_PX),
        text_color,
    );

    response
}

/// "BYPASS" button at an absolute rect (skips the natural layout flow).
/// On state uses a muted warning red to clearly read as "audio untouched".
pub(super) fn bypass_button_at(ui: &mut egui::Ui, rect: egui::Rect, is_on: bool) -> egui::Response {
    let response = ui.interact(rect, ui.id().with("bypass-toggle"), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, is_on);
        let painter = ui.painter();

        let bg = if is_on {
            // Muted warning red — a useful tool, not an error.
            egui::Color32::from_rgb(190, 70, 60)
        } else {
            visuals.bg_fill
        };
        painter.rect_filled(rect, 4.0, bg);
        painter.rect_stroke(rect, 4.0, visuals.bg_stroke, egui::StrokeKind::Inside);

        let text_color = if is_on {
            egui::Color32::WHITE
        } else {
            visuals.text_color()
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "BYPASS",
            egui::FontId::proportional(12.5),
            text_color,
        );
    }

    response
}

/// Square icon button for the BT pill halves. `rounding` controls
/// per-corner radius so two of these can sit flush as a single pill.
/// `enabled = false` keeps tooltips working but ignores clicks.
pub(super) fn bt_toggle_button_at(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    is_active: bool,
    icon: &egui::TextureHandle,
    rounding: egui::CornerRadius,
    enabled: bool,
) -> egui::Response {
    let id_seed = (rect.min.x as i32, rect.min.y as i32);
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let response = ui.interact(rect, ui.id().with(("bt-btn", id_seed)), sense);

    if ui.is_rect_visible(rect) {
        // `interact_selectable` produces the same selected-tint the platform
        // tabs use, keeping the BT pill visually consistent with the rest
        // of the UI. No stroke on purpose — under the bypass overlay an
        // outline reads as a noisy ring around an otherwise-grayed pill.
        let visuals = ui.style().interact_selectable(&response, is_active);
        let painter = ui.painter();
        painter.rect_filled(rect, rounding, visuals.bg_fill);
        let icon_rect = rect.shrink(5.0);
        egui::Image::new((icon.id(), icon_rect.size())).paint_at(ui, icon_rect);
    }

    response
}

/// BT-protocol picker. Free-floating `egui::Area` anchored above the gear
/// button so it grows up into empty editor space. Each row pairs the codec
/// label with a plain-language device hint (e.g. "iPhone / AirPods").
///
/// Returns the popup's interaction response so the caller can detect clicks
/// outside the popup (and close it).
pub(super) fn draw_bt_protocol_popup(
    ctx: &egui::Context,
    params: &Arc<StreamingSimulatorParams>,
    setter: &ParamSetter<'_>,
    anchor: egui::Pos2,
    popup_open: &mut bool,
) -> egui::Response {
    // Worst → best so the popup reads top-down from "cheap earbuds" to
    // "high-end LE Audio".
    const PRESETS: &[(BluetoothProtocol, &str, &str)] = &[
        (
            BluetoothProtocol::SbcLow,
            "SBC · Low",
            "Cheap earbuds / older speakers",
        ),
        (
            BluetoothProtocol::SbcHigh,
            "SBC · High",
            "Default fallback for most BT headphones",
        ),
        (
            BluetoothProtocol::Aac128,
            "AAC · 128 kbps",
            "Older Android phones",
        ),
        (
            BluetoothProtocol::Aac256,
            "AAC · 256 kbps",
            "iPhone / AirPods",
        ),
        (
            BluetoothProtocol::Lc3_64,
            "LC3 · 64 kbps",
            "LE Audio low-power (mono)",
        ),
        (
            BluetoothProtocol::Lc3_160,
            "LC3 · 160 kbps",
            "LE Audio high-quality",
        ),
    ];
    let current = params.bluetooth_protocol.value();
    let mut should_close = false;

    let area_resp = egui::Area::new(egui::Id::new("bt-protocol-popup"))
        .fixed_pos(anchor)
        // Anchor the popup's bottom-left so it grows *upward* from above
        // the gear button (default would drop it down into the codec rows).
        .pivot(egui::Align2::LEFT_BOTTOM)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.label(egui::RichText::new("Bluetooth protocol").strong());
                ui.separator();
                for (preset, primary, secondary) in PRESETS {
                    let selected = *preset == current;
                    if bt_preset_button(ui, primary, secondary, selected).clicked() {
                        if !selected {
                            setter.begin_set_parameter(&params.bluetooth_protocol);
                            setter.set_parameter(&params.bluetooth_protocol, *preset);
                            setter.end_set_parameter(&params.bluetooth_protocol);
                        }
                        should_close = true;
                    }
                }
            });
        });

    if should_close {
        *popup_open = false;
    }

    area_resp.response
}

/// Two-line popup row: codec label above a dim device hint. Same idea as
/// `codec_button` but left-aligned and more compact.
fn bt_preset_button(
    ui: &mut egui::Ui,
    primary: &str,
    secondary: &str,
    selected: bool,
) -> egui::Response {
    let size = egui::vec2(ui.available_width().max(200.0), 38.0);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        let painter = ui.painter();
        let bg = if selected {
            visuals.weak_bg_fill
        } else if response.hovered() {
            visuals.bg_fill
        } else {
            egui::Color32::TRANSPARENT
        };
        painter.rect_filled(rect, 4.0, bg);

        let title_color = visuals.text_color();
        let subtitle_color = dim(title_color, 0.65);
        let pad_x = 8.0;
        painter.text(
            egui::pos2(rect.left() + pad_x, rect.top() + 7.0),
            egui::Align2::LEFT_TOP,
            primary,
            egui::FontId::proportional(13.0),
            title_color,
        );
        painter.text(
            egui::pos2(rect.left() + pad_x, rect.top() + 22.0),
            egui::Align2::LEFT_TOP,
            secondary,
            egui::FontId::proportional(10.5),
            subtitle_color,
        );
    }

    response
}

/// Two-line button: bold title above a dim subtitle. One per codec tier.
fn codec_button(ui: &mut egui::Ui, title: &str, subtitle: &str, selected: bool) -> egui::Response {
    let size = egui::vec2(CODEC_BUTTON_W, CODEC_BUTTON_H);
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&response, selected);
        let painter = ui.painter();

        let bg = if selected {
            visuals.weak_bg_fill
        } else {
            visuals.bg_fill
        };
        painter.rect_filled(rect, 5.0, bg);
        painter.rect_stroke(rect, 5.0, visuals.bg_stroke, egui::StrokeKind::Inside);

        let title_color = visuals.text_color();
        let subtitle_color = dim(title_color, 0.6);

        let title_pos = rect.center() - egui::vec2(0.0, 9.0);
        let subtitle_pos = rect.center() + egui::vec2(0.0, 11.0);

        painter.text(
            title_pos,
            egui::Align2::CENTER_CENTER,
            title,
            egui::FontId::proportional(13.5),
            title_color,
        );
        painter.text(
            subtitle_pos,
            egui::Align2::CENTER_CENTER,
            subtitle,
            egui::FontId::proportional(10.5),
            subtitle_color,
        );
    }

    response
}

// ── Info button + popup ────────────────────────────────────────────

/// Top-right "?" button. Clicking toggles the info popup.
pub(super) fn info_button_at(ui: &mut egui::Ui, rect: egui::Rect) -> egui::Response {
    let id_seed = (rect.min.x as i32, rect.min.y as i32);
    let response = ui.interact(rect, ui.id().with(("info-btn", id_seed)), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, visuals.bg_fill);
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "?",
            egui::FontId::proportional(14.0),
            visuals.text_color(),
        );
    }

    response
}

/// "About this plugin" popup: title, author, version, GitHub link, license.
/// Anchored top-right at `anchor` (so it grows down-and-left from the
/// info button). Returns the area's response so callers can detect
/// clicked-elsewhere and close.
pub(super) fn draw_info_popup(ctx: &egui::Context, anchor: egui::Pos2) -> egui::Response {
    const VERSION: &str = env!("CARGO_PKG_VERSION");
    const GITHUB_URL: &str = "https://github.com/JulienMeziere/streaming-simulator";

    let area_resp = egui::Area::new(egui::Id::new("info-popup"))
        .fixed_pos(anchor)
        // Anchor the popup's bottom-right corner so it grows up-and-left
        // from above the info button (which sits at the bottom of the
        // editor — a default top-left pivot would render off-screen).
        .pivot(egui::Align2::RIGHT_BOTTOM)
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(280.0);
                ui.set_max_width(320.0);
                ui.label(egui::RichText::new("Streaming Simulator").strong().size(15.0));
                ui.label(
                    egui::RichText::new(format!("v{VERSION}"))
                        .size(11.0)
                        .color(dim(ui.visuals().text_color(), 0.7)),
                );
                ui.add_space(6.0);
                ui.label(
                    "Audio plugin for the master channel that emulates how \
                     streaming platforms, Bluetooth, and FM radio compress \
                     and degrade audio.",
                );
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Julien MEZIERE").strong());
                ui.hyperlink_to("GitHub repository", GITHUB_URL);
                ui.label(
                    egui::RichText::new("Licensed under GPL-3.0-or-later")
                        .size(11.0)
                        .color(dim(ui.visuals().text_color(), 0.65)),
                );
            });
        });

    area_resp.response
}

// ── Layout / colour helpers ────────────────────────────────────────

/// egui's `vertical_centered` doesn't actually center horizontal rows
/// (the inner horizontal claims `available_width()` and items end up
/// left-aligned). We pad explicitly with the precomputed content width.
fn centered_row<R>(
    ui: &mut egui::Ui,
    content_width: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        let avail = ui.available_width();
        let pad = ((avail - content_width) * 0.5).max(0.0);
        ui.add_space(pad);
        add(ui)
    })
    .inner
}

fn dim(c: egui::Color32, factor: f32) -> egui::Color32 {
    egui::Color32::from_rgba_premultiplied(
        (c.r() as f32 * factor) as u8,
        (c.g() as f32 * factor) as u8,
        (c.b() as f32 * factor) as u8,
        c.a(),
    )
}
