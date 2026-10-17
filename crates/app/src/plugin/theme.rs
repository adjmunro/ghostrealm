//! Shared UI colour tokens, so views and the app's chrome read as one UI. The
//! configurable colours (background, sidebar, accent) come from
//! [`PaintCx::chrome`](super::PaintCx::chrome); these are the fixed ones.
//! Sizes are functions of the [`UiMetrics`] (physical px), so they scale with
//! the font and the display.

use super::UiMetrics;

/// Hover / keyboard-selection highlight: this colour at [`HOVER_ALPHA`].
pub const HOVER_BG: [u8; 3] = [255, 255, 255];
pub const HOVER_ALPHA: f32 = 0.14;
/// A button label, and the same label under the cursor or when "on".
pub const LABEL: [u8; 3] = [150, 150, 165];
pub const LABEL_HOVER: [u8; 3] = [210, 210, 220];
/// Secondary text: placeholders, hints, "off" toggles.
pub const DIM: [u8; 3] = [120, 120, 135];
/// Headings and titles.
pub const TITLE: [u8; 3] = [170, 170, 185];
/// Body text on chrome surfaces (tab titles, document headers).
pub const TEXT: [u8; 3] = [220, 220, 230];
/// Metadata beside body text (sizes, counts, paths).
pub const TEXT_DIM: [u8; 3] = [140, 140, 155];
/// Icon glyphs (close '×', toggles), and the same under the cursor.
pub const GLYPH: [u8; 3] = [140, 140, 155];
pub const GLYPH_HOVER: [u8; 3] = [235, 235, 245];
/// A toggle that is on, and a successful result.
pub const ON: [u8; 3] = [80, 180, 90];
/// A file changed on disk under unsaved edits.
pub const WARN: [u8; 3] = [225, 170, 70];
/// Lines added and deleted (diff counts).
pub const ADDED: [u8; 3] = [80, 180, 90];
pub const DELETED: [u8; 3] = [210, 90, 90];
/// A tooltip's panel.
pub const TOOLTIP_BG: [u8; 3] = [44, 44, 54];
/// A text field's border and typed text.
pub const INPUT_BORDER: [u8; 3] = [80, 80, 95];
pub const INPUT_TEXT: [u8; 3] = [225, 225, 235];
/// The active tab's pill: this colour at [`TAB_ACTIVE_ALPHA`], stronger than
/// hover so the open tab reads apart from the one under the cursor.
pub const TAB_ACTIVE_BG: [u8; 3] = [255, 255, 255];
pub const TAB_ACTIVE_ALPHA: f32 = 0.28;
/// A selected list row's pill: this colour at [`ROW_SELECTED_ALPHA`], stronger
/// than hover for the same reason.
pub const ROW_SELECTED_BG: [u8; 3] = [255, 255, 255];
pub const ROW_SELECTED_ALPHA: f32 = 0.24;
/// Indent guides in a tree: this colour at [`GUIDE_ALPHA`], so they read on
/// any background. Translucent, so where two guides overlap they look
/// denser: draw them without overlap.
pub const GUIDE: [u8; 3] = [255, 255, 255];
pub const GUIDE_ALPHA: f32 = 0.10;
/// A code box (inline code, a fenced block): fill at [`CODE_BOX_ALPHA`] over
/// the background, outlined in [`CODE_BOX_BORDER`] at
/// [`CODE_BOX_BORDER_ALPHA`]. Neutral and low in contrast, so the outline
/// defines the box rather than the fill: over the editor's background the
/// fill lifts it by about 12 levels and the border by about 27.
pub const CODE_BOX_BG: [u8; 3] = [255, 255, 255];
pub const CODE_BOX_ALPHA: f32 = 0.055;
pub const CODE_BOX_BORDER: [u8; 3] = [255, 255, 255];
pub const CODE_BOX_BORDER_ALPHA: f32 = 0.12;
/// A text field's border while it has keyboard focus.
pub const FIELD_FOCUS_BORDER: [u8; 3] = [110, 140, 210];

/// Corner radius of a pill: a tab, a list row's hover or selection.
pub fn pill_radius(ui: UiMetrics) -> f32 {
    (ui.cell_h * 0.3).round()
}

/// Corner radius of a box: a code box, a text field.
pub fn box_radius(ui: UiMetrics) -> f32 {
    (ui.cell_h * 0.2).round()
}

/// One logical pixel, at least one physical: box borders, indent guides.
/// Draw a straight guide as a [`fill`](super::PaintCx::fill) on whole pixels
/// (a stroke is centred on its coordinate, so it blurs across two), and
/// without overlapping another: translucent fills look denser where they do.
pub fn hairline(ui: UiMetrics) -> f32 {
    ui.scale.round().max(1.0)
}

/// A focused text field's border width.
pub fn focus_ring_w(ui: UiMetrics) -> f32 {
    (2.0 * ui.scale).round().max(1.0)
}
