//! egui editor entry point and state machine.
//!
//! Split into three files:
//! - this module — `UiState`, the per-frame closure, and pure state-
//!   transformation helpers (`compute_codec_rows`, `platform_index_of`, …).
//! - [`widgets`] — every drawing primitive + the constants they need.
//! - [`icons`] — PNG decode + Lanczos3 resize + texture caching.

mod icons;
mod widgets;

use crate::{PlatformDef, StreamingSimulatorParams, PLATFORMS};
use icons::{icons_need_reload, load_icon};
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, EguiState};
use std::sync::Arc;
use widgets::{
    bt_toggle_button_at, bypass_button_at, draw_bt_protocol_popup, draw_codec_row,
    draw_info_popup, draw_platform_row, info_button_at, BT_BUTTON_SIZE, BT_PILL_RADIUS,
    BT_ROW_PAD, BYPASS_BUTTON_H, BYPASS_BUTTON_W, BYPASS_ROW_PAD, INFO_BUTTON_SIZE, INFO_ROW_PAD,
};

// Sized for Spotify's 5-button row + a 2-codec-row platform like YouTube
// Music. Single-row platforms get extra empty space below — kept constant
// across tabs so the window doesn't resize on switch.
const WINDOW_WIDTH: u32 = 980;
const WINDOW_HEIGHT: u32 = 310;

pub fn default_state() -> Arc<EguiState> {
    EguiState::from_size(WINDOW_WIDTH, WINDOW_HEIGHT)
}

/// Per-instance UI state retained across frames and editor open/close.
/// Fields are `pub(super)` so [`widgets`] can mutate them while drawing.
#[derive(Default)]
pub(super) struct UiState {
    /// Platform icons, parallel to `PLATFORMS`.
    pub(super) icons: Vec<egui::TextureHandle>,
    pub(super) bt_icon: Option<egui::TextureHandle>,
    pub(super) gear_icon: Option<egui::TextureHandle>,
    /// Index into `PLATFORMS` for the currently visible tab. Initialised
    /// from whichever platform owns the persisted codec parameter.
    pub(super) selected_platform_idx: Option<usize>,
    pub(super) bt_popup_open: bool,
    pub(super) info_popup_open: bool,
    /// Cached codec-index grouping per row for the active tab. Without
    /// caching this `filter().collect()` runs every frame on every input
    /// event. Rebuilt only when `codec_rows_for` falls out of sync.
    pub(super) codec_rows: Vec<Vec<usize>>,
    pub(super) codec_rows_for: Option<usize>,
}

pub fn create(
    params: Arc<StreamingSimulatorParams>,
    egui_state: Arc<EguiState>,
) -> Option<Box<dyn Editor>> {
    create_egui_editor(
        egui_state,
        UiState::default(),
        |egui_ctx, _state| {
            // Default tooltip delay is ~0.5 s, which feels sluggish for short
            // labels like "Choose Bluetooth protocol". Set once at build time.
            egui_ctx.style_mut(|style| {
                style.interaction.tooltip_delay = 0.0;
            });
        },
        move |egui_ctx, setter, state| {
            // Reload icons when the cache is empty or stale. Stale happens
            // when the user closes + reopens the editor: nih_plug_egui keeps
            // `UiState` alive but the GL context (and its textures) doesn't.
            if icons_need_reload(egui_ctx, &state.icons) {
                state.icons = PLATFORMS
                    .iter()
                    .map(|p| load_icon(egui_ctx, p.id, p.icon_png))
                    .collect();
                state.bt_icon = Some(load_icon(
                    egui_ctx,
                    "bluetooth",
                    include_bytes!("../../resources/bluetooth.png"),
                ));
                state.gear_icon = Some(load_icon(
                    egui_ctx,
                    "settings",
                    include_bytes!("../../resources/settings.png"),
                ));
            }
            if state.selected_platform_idx.is_none() {
                state.selected_platform_idx = Some(platform_index_of(params.codec.value()));
            }

            egui::CentralPanel::default().show(egui_ctx, |ui| {
                let panel_rect = ui.max_rect();
                let bypassed = params.bypass.value();

                // `add_enabled_ui` disables clicks while bypassed. Visual
                // dimming is handled by the explicit overlay below, not
                // egui's default disabled tint.
                ui.add_space(20.0);
                ui.add_enabled_ui(!bypassed, |ui| {
                    draw_platform_row(ui, state);

                    ui.add_space(22.0);
                    let active_idx = state.selected_platform_idx.unwrap_or(0);
                    draw_codec_row(ui, &params, setter, active_idx, state);
                });

                // Bottom-left Bluetooth pill: BT toggle on the left half,
                // gear on the right, drawn as one rounded rectangle split
                // down the middle. Painted *before* the bypass overlay so
                // it dims along with everything else.
                let pill_left = panel_rect.left() + BT_ROW_PAD;
                let pill_top = panel_rect.bottom() - BT_BUTTON_SIZE - BT_ROW_PAD;
                let bt_rect = egui::Rect::from_min_size(
                    egui::pos2(pill_left, pill_top),
                    egui::vec2(BT_BUTTON_SIZE, BT_BUTTON_SIZE),
                );
                let gear_rect = egui::Rect::from_min_size(
                    egui::pos2(pill_left + BT_BUTTON_SIZE, pill_top),
                    egui::vec2(BT_BUTTON_SIZE, BT_BUTTON_SIZE),
                );
                // Outer corners rounded, inner edges sharp so the two
                // halves read as one pill.
                let bt_corner = egui::CornerRadius {
                    nw: BT_PILL_RADIUS,
                    sw: BT_PILL_RADIUS,
                    ne: 0,
                    se: 0,
                };
                let gear_corner = egui::CornerRadius {
                    nw: 0,
                    sw: 0,
                    ne: BT_PILL_RADIUS,
                    se: BT_PILL_RADIUS,
                };
                let bt_enabled = params.bluetooth_enabled.value();
                let mut gear_clicked_this_frame = false;
                if let (Some(bt_icon), Some(gear_icon)) = (&state.bt_icon, &state.gear_icon) {
                    // `_at_pointer` (vs plain `on_hover_text`) because the
                    // pill is at the very bottom of the window — the
                    // default tooltip would render below it, off-screen.
                    let bt_resp = bt_toggle_button_at(
                        ui,
                        bt_rect,
                        bt_enabled,
                        bt_icon,
                        bt_corner,
                        !bypassed,
                    )
                    .on_hover_text_at_pointer(
                        "Bluetooth: simulates how transmission to wireless \
                         speakers or headphones affects the sound. \
                         Cascades on top of the selected platform codec.",
                    );
                    if !bypassed && bt_resp.clicked() {
                        setter.begin_set_parameter(&params.bluetooth_enabled);
                        setter.set_parameter(&params.bluetooth_enabled, !bt_enabled);
                        setter.end_set_parameter(&params.bluetooth_enabled);
                    }
                    let gear_resp = bt_toggle_button_at(
                        ui,
                        gear_rect,
                        false,
                        gear_icon,
                        gear_corner,
                        !bypassed,
                    )
                    .on_hover_text_at_pointer("Choose Bluetooth protocol");
                    if !bypassed && gear_resp.clicked() {
                        state.bt_popup_open = !state.bt_popup_open;
                        gear_clicked_this_frame = true;
                    }
                }
                // Popup represents an action that's invalid while bypassed.
                maybe_close_popup_on_bypass(state, bypassed);

                // Overlay covers the full window (not `ui.max_rect()`, which
                // is inset by the panel margin and would leave uncovered
                // strips). Drawn after the BT pill so it dims, before the
                // bypass button so that stays full-colour.
                if bypassed {
                    ui.painter().rect_filled(
                        egui_ctx.screen_rect(),
                        0.0,
                        egui::Color32::from_rgba_premultiplied(20, 20, 20, 140),
                    );
                }

                // Bottom-right cluster: "?" info button on the far right,
                // bypass to its left. Both centred on the same Y axis (info
                // is slightly smaller, so we offset its top down to align
                // visual centres).
                const INFO_BYPASS_GAP: f32 = 8.0;
                let info_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        panel_rect.right() - INFO_BUTTON_SIZE - INFO_ROW_PAD,
                        panel_rect.bottom()
                            - INFO_ROW_PAD
                            - INFO_BUTTON_SIZE
                            - (BYPASS_BUTTON_H - INFO_BUTTON_SIZE) * 0.5,
                    ),
                    egui::vec2(INFO_BUTTON_SIZE, INFO_BUTTON_SIZE),
                );
                let bypass_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        info_rect.left() - INFO_BYPASS_GAP - BYPASS_BUTTON_W,
                        panel_rect.bottom() - BYPASS_BUTTON_H - BYPASS_ROW_PAD,
                    ),
                    egui::vec2(BYPASS_BUTTON_W, BYPASS_BUTTON_H),
                );
                let bypass_resp = bypass_button_at(ui, bypass_rect, bypassed);
                if bypass_resp.clicked() {
                    setter.begin_set_parameter(&params.bypass);
                    setter.set_parameter(&params.bypass, !bypassed);
                    setter.end_set_parameter(&params.bypass);
                }

                // Info button is always interactive (the popup is purely
                // metadata; no audio action to gate on bypass).
                let info_resp = info_button_at(ui, info_rect)
                    .on_hover_text_at_pointer("About this plugin");
                let mut info_clicked_this_frame = false;
                if info_resp.clicked() {
                    state.info_popup_open = !state.info_popup_open;
                    info_clicked_this_frame = true;
                }

                // Painted after the overlay so the popups are never dimmed.
                // `clicked_elsewhere` closes the popup when the user clicks
                // outside its rect — but we ignore the click that just
                // toggled the matching trigger button this same frame, so
                // it doesn't immediately re-close.
                if state.bt_popup_open {
                    let popup_anchor =
                        egui::pos2(gear_rect.left(), gear_rect.top() - 6.0);
                    let popup_resp = draw_bt_protocol_popup(
                        egui_ctx,
                        &params,
                        setter,
                        popup_anchor,
                        &mut state.bt_popup_open,
                    );
                    if popup_resp.clicked_elsewhere() && !gear_clicked_this_frame {
                        state.bt_popup_open = false;
                    }
                }
                if state.info_popup_open {
                    // Grow upward from above the info button — the button
                    // sits at the bottom of the window, so a downward popup
                    // would render off-screen.
                    let popup_anchor =
                        egui::pos2(info_rect.right(), info_rect.top() - 6.0);
                    let popup_resp = draw_info_popup(egui_ctx, popup_anchor);
                    if popup_resp.clicked_elsewhere() && !info_clicked_this_frame {
                        state.info_popup_open = false;
                    }
                }
            });
        },
    )
}

// ── Pure state-transformation helpers ──────────────────────────────

fn platform_index_of(codec: crate::Codec) -> usize {
    let target_id = codec.platform().id;
    PLATFORMS
        .iter()
        .position(|p| p.id == target_id)
        .unwrap_or(0)
}

/// Group codec indices by `row`, dropping empty rows. Returns indices into
/// `platform.codecs`. Pure function so it's testable without an egui ctx.
fn compute_codec_rows(platform: &PlatformDef) -> Vec<Vec<usize>> {
    let max_row = platform.codecs.iter().map(|c| c.row).max().unwrap_or(1);
    let mut rows = Vec::new();
    for row_num in 1..=max_row {
        let mut row = Vec::new();
        for (i, c) in platform.codecs.iter().enumerate() {
            if c.row == row_num {
                row.push(i);
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }
    rows
}

/// Rebuild `state.codec_rows` only when the cached platform changed.
/// Called every frame from [`widgets::draw_codec_row`].
pub(super) fn rebuild_codec_rows_if_stale(
    state: &mut UiState,
    platform_idx: usize,
    platform: &PlatformDef,
) {
    if state.codec_rows_for != Some(platform_idx) {
        state.codec_rows = compute_codec_rows(platform);
        state.codec_rows_for = Some(platform_idx);
    }
}

/// Close the BT-protocol popup when bypass engages. Free function so tests
/// can drive it with hand-built `UiState`s.
fn maybe_close_popup_on_bypass(state: &mut UiState, bypassed: bool) {
    if bypassed {
        state.bt_popup_open = false;
    }
}

#[cfg(test)]
mod tests {
    //! Smoke tests for the pure state helpers. The widget layer needs an
    //! egui context and is not unit-tested here.
    use super::*;

    fn find_platform(id: &str) -> &'static PlatformDef {
        PLATFORMS
            .iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("platform `{id}` not in catalog"))
    }

    // ── compute_codec_rows ────────────────────────────────────────

    #[test]
    fn codec_rows_handles_single_row_platform() {
        // Tidal collapses everything onto row 1.
        let tidal = find_platform("tidal");
        let rows = compute_codec_rows(tidal);
        assert_eq!(rows.len(), 1, "Tidal should have exactly one row");
        assert_eq!(rows[0].len(), tidal.codecs.len());
        for &i in &rows[0] {
            assert!(i < tidal.codecs.len());
        }
    }

    #[test]
    fn codec_rows_partitions_youtube_music_into_two_rows() {
        let yt = find_platform("youtube-music");
        let rows = compute_codec_rows(yt);
        assert_eq!(
            rows.len(),
            2,
            "YouTube Music has parallel mobile + web codec paths"
        );
        for &i in &rows[0] {
            assert_eq!(yt.codecs[i].row, 1);
        }
        for &i in &rows[1] {
            assert_eq!(yt.codecs[i].row, 2);
        }
    }

    /// Every codec must appear in exactly one row.
    #[test]
    fn codec_rows_total_equals_codec_count() {
        for platform in PLATFORMS.iter() {
            let rows = compute_codec_rows(platform);
            let total: usize = rows.iter().map(|r| r.len()).sum();
            assert_eq!(
                total,
                platform.codecs.len(),
                "platform `{}` has {} codecs but row-partition contains {}",
                platform.id,
                platform.codecs.len(),
                total
            );
        }
    }

    // ── rebuild_codec_rows_if_stale ───────────────────────────────

    #[test]
    fn cache_rebuild_skipped_when_platform_unchanged() {
        let mut state = UiState::default();
        let spotify_idx = PLATFORMS.iter().position(|p| p.id == "spotify").unwrap();
        let spotify = &PLATFORMS[spotify_idx];

        rebuild_codec_rows_if_stale(&mut state, spotify_idx, spotify);
        assert_eq!(state.codec_rows_for, Some(spotify_idx));
        let first = state.codec_rows.clone();

        // Tag the cache with a sentinel and confirm a same-platform call
        // doesn't rebuild it away.
        state.codec_rows.push(vec![999]);
        rebuild_codec_rows_if_stale(&mut state, spotify_idx, spotify);
        assert_eq!(state.codec_rows.last(), Some(&vec![999]));

        // Switching platforms wipes the sentinel via a real rebuild.
        let deezer_idx = PLATFORMS.iter().position(|p| p.id == "deezer").unwrap();
        let deezer = &PLATFORMS[deezer_idx];
        rebuild_codec_rows_if_stale(&mut state, deezer_idx, deezer);
        assert_eq!(state.codec_rows_for, Some(deezer_idx));
        assert!(!state.codec_rows.iter().any(|r| r.contains(&999)));

        rebuild_codec_rows_if_stale(&mut state, spotify_idx, spotify);
        assert_eq!(state.codec_rows, first);
    }

    // ── maybe_close_popup_on_bypass ───────────────────────────────

    #[test]
    fn popup_closes_when_bypass_toggled_on() {
        let mut state = UiState::default();
        state.bt_popup_open = true;
        maybe_close_popup_on_bypass(&mut state, true);
        assert!(!state.bt_popup_open);
    }

    #[test]
    fn popup_stays_open_when_not_bypassed() {
        let mut state = UiState::default();
        state.bt_popup_open = true;
        maybe_close_popup_on_bypass(&mut state, false);
        assert!(state.bt_popup_open);
    }

    #[test]
    fn popup_already_closed_stays_closed() {
        let mut state = UiState::default();
        state.bt_popup_open = false;
        maybe_close_popup_on_bypass(&mut state, true);
        assert!(!state.bt_popup_open);
        maybe_close_popup_on_bypass(&mut state, false);
        assert!(!state.bt_popup_open);
    }

    // ── platform_index_of ─────────────────────────────────────────

    #[test]
    fn platform_index_of_round_trips_every_codec() {
        let n = <crate::Codec as nih_plug::prelude::Enum>::variants().len();
        for i in 0..n {
            let codec = crate::Codec::from_index(i);
            let idx = platform_index_of(codec);
            let platform = &PLATFORMS[idx];
            assert!(
                platform.codecs.iter().any(|c| c.codec == codec),
                "platform_index_of({codec:?}) returned platform `{}` \
                 which doesn't list this codec",
                platform.id
            );
        }
    }

    // ── UiState ───────────────────────────────────────────────────

    #[test]
    fn default_ui_state_is_blank() {
        let state = UiState::default();
        assert!(state.icons.is_empty());
        assert!(state.bt_icon.is_none());
        assert!(state.gear_icon.is_none());
        assert!(state.selected_platform_idx.is_none());
        assert!(!state.bt_popup_open);
        assert!(!state.info_popup_open);
        assert!(state.codec_rows.is_empty());
        assert!(state.codec_rows_for.is_none());
    }

    #[test]
    fn default_state_returns_egui_state_with_window_dimensions() {
        let state = default_state();
        let (w, h) = state.size();
        assert_eq!(w, WINDOW_WIDTH);
        assert_eq!(h, WINDOW_HEIGHT);
    }
}
