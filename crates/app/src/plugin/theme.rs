//! Shared UI colour tokens, so views and the app's chrome read as one UI. The
//! configurable colours (background, sidebar, accent) come from
//! [`PaintCx::chrome`](super::PaintCx::chrome); these are the fixed ones.

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
/// Body text on chrome surfaces (tab titles, ribbons).
pub const TEXT: [u8; 3] = [220, 220, 230];
/// Metadata beside body text (sizes, counts, paths).
pub const TEXT_DIM: [u8; 3] = [140, 140, 155];
/// Icon glyphs (close '×', toggles), and the same under the cursor.
pub const GLYPH: [u8; 3] = [140, 140, 155];
pub const GLYPH_HOVER: [u8; 3] = [235, 235, 245];
/// A text field's border and typed text.
pub const INPUT_BORDER: [u8; 3] = [80, 80, 95];
pub const INPUT_TEXT: [u8; 3] = [225, 225, 235];
