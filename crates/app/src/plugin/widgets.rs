//! Reusable pieces views compose from [`PaintCx`] primitives.

use ghostrealm_core::Rect;
use glyphon::Family;
use winit::window::CursorIcon;

use super::text_edit::LineEdit;
use super::{theme, MouseEvent, PaintCx};

/// Where a [`text_field`] drew its text, for mapping the mouse to columns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldGeom {
    /// The whole field.
    pub rect: Rect,
    /// The x of the first shown character's left edge.
    pub left: f32,
    pub cell_w: f32,
    /// The first shown column.
    pub first: usize,
    /// The text's length in columns.
    pub len: usize,
}

impl FieldGeom {
    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        x >= self.rect.x && x < self.rect.x + self.rect.w && y >= self.rect.y && y < self.rect.y + self.rect.h
    }

    /// The caret column nearest `x`: the start for a point left of the field,
    /// the end for one right of it.
    pub fn caret_at(&self, x: f32) -> usize {
        if x < self.rect.x {
            return 0;
        }
        if x >= self.rect.x + self.rect.w {
            return self.len;
        }
        let col = ((x - self.left) / self.cell_w).round() as isize + self.first as isize;
        col.clamp(0, self.len as isize) as usize
    }

    /// The column of the character under `x`.
    pub fn char_at(&self, x: f32) -> usize {
        let col = ((x - self.left) / self.cell_w).floor() as isize + self.first as isize;
        col.clamp(0, self.len.saturating_sub(1) as isize) as usize
    }
}

/// Mouse handling for one [`text_field`]: remembers the field's geometry from
/// its last paint and which press began inside it.
#[derive(Default)]
pub struct FieldMouse {
    geom: std::cell::Cell<Option<FieldGeom>>,
    pressed: bool,
    /// Where a press from [`hold`](Self::hold) began, until its drag starts.
    origin: Option<(f32, f32)>,
}

impl FieldMouse {
    /// Record what [`text_field`] returned for this frame, and show the text
    /// cursor over it: only a field that takes the mouse says it painted.
    pub fn painted(&self, cx: &mut PaintCx, geom: FieldGeom) {
        cx.cursor(geom.rect, CursorIcon::Text);
        self.geom.set(Some(geom));
    }

    /// Where the field was last drawn, if it is live.
    pub fn geom(&self) -> Option<FieldGeom> {
        self.geom.get()
    }

    /// The field is not drawn, or not editable, this frame.
    pub fn clear(&self) {
        self.geom.set(None);
    }

    /// A press at `pos` began outside editing and made the field editable (its
    /// text is selected whole). A drag in the same press selects from `pos`.
    pub fn hold(&mut self, pos: (f32, f32)) {
        self.pressed = true;
        self.origin = Some(pos);
    }

    /// Apply `event` to `edit`; whether the field took it. A press inside the
    /// field starts a selection; the drag and release that follow are the
    /// field's wherever the pointer goes. `extend` is Shift.
    pub fn event(&mut self, edit: &mut LineEdit, event: &MouseEvent, extend: bool) -> bool {
        let Some(g) = self.geom.get() else { return false };
        match *event {
            MouseEvent::Down { pos, clicks } if g.contains(pos) => {
                self.pressed = true;
                self.origin = None;
                edit.press(g.caret_at(pos.0), g.char_at(pos.0), clicks, extend);
                true
            }
            MouseEvent::Down { .. } => {
                self.pressed = false;
                self.origin = None;
                false
            }
            MouseEvent::Drag { pos } if self.pressed => {
                if let Some(from) = self.origin.take() {
                    edit.press(g.caret_at(from.0), g.char_at(from.0), 1, false);
                }
                edit.drag_to(g.caret_at(pos.0));
                true
            }
            MouseEvent::Up { .. } if self.pressed => {
                self.pressed = false;
                self.origin = None;
                edit.release();
                true
            }
            _ => false,
        }
    }
}

/// A single-line text field in `r`: background, border, the text with its
/// selection (or a dim placeholder when empty), and, when `focused`, a caret at
/// the cursor. Text is monospace; when it doesn't fit, the field shows its tail
/// (the most specific part of a path) unless the cursor is further left.
/// Returns where the text was drawn, for [`FieldMouse`], which shows the text
/// cursor (a field that is inert shows none).
pub fn text_field(cx: &mut PaintCx, r: Rect, edit: &LineEdit, placeholder: &str, focused: bool) -> FieldGeom {
    let scale = cx.ui.scale;
    let pad = 8.0 * scale;
    let cw = cx.ui.cell_w;
    cx.fill(r, cx.chrome.background);
    cx.border(r, theme::INPUT_BORDER);
    let inner_w = (r.w - pad).max(1.0);
    let cols = ((inner_w / cw) as usize).max(1);
    let left = r.x + pad * 0.5;
    let top = r.y + (r.h - cx.ui.cell_h) * 0.5;
    let band = |x: f32, w: f32| Rect { x, y: r.y + 2.0 * scale, w, h: (r.h - 4.0 * scale).max(1.0) };

    let mut first_col = 0;
    let caret_col = if edit.is_empty() {
        let text = shorten_start(placeholder, cols);
        let shaped = cx.shape(&text, Family::Monospace, inner_w);
        cx.place(shaped, left, top, r, theme::DIM);
        0
    } else {
        let chars: Vec<char> = edit.text().chars().collect();
        let first = visible_start(chars.len(), edit.cursor(), cols);
        first_col = first;
        let last = (first + cols).min(chars.len());
        if let Some((s, e)) = edit.selection() {
            let (s, e) = (s.clamp(first, last), e.clamp(first, last));
            if e > s {
                let x = left + (s - first) as f32 * cw;
                cx.fill_alpha(band(x, (e - s) as f32 * cw), cx.chrome.accent, 0.35);
            }
        }
        let shown: String = chars[first..last].iter().collect();
        let shaped = cx.shape(&shown, Family::Monospace, inner_w);
        cx.place(shaped, left, top, r, theme::INPUT_TEXT);
        edit.cursor() - first
    };
    let caret_x = left + caret_col as f32 * cw;
    if focused && caret_x < r.x + r.w {
        cx.fill_top(band(caret_x, 2.0 * scale), cx.chrome.accent, 0.9);
    }
    FieldGeom { rect: r, left, cell_w: cw, first: first_col, len: edit.text().chars().count() }
}

/// The first column a field `cols` wide shows of `len` chars so the cursor is
/// visible: the tail (with room for the caret after it), else from the cursor.
fn visible_start(len: usize, cursor: usize, cols: usize) -> usize {
    if len < cols {
        return 0;
    }
    let tail = len + 1 - cols;
    cursor.min(tail)
}

/// Truncate `s` to at most `max` characters, keeping the tail (prefixed with an
/// ellipsis) so the most specific part of a path stays visible.
pub fn shorten_start(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if max == 0 || count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("\u{2026}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(first: usize, len: usize) -> FieldGeom {
        FieldGeom { rect: Rect { x: 10.0, y: 0.0, w: 100.0, h: 20.0 }, left: 14.0, cell_w: 8.0, first, len }
    }

    #[test]
    fn the_pointer_maps_to_the_nearest_caret_and_the_character_under_it() {
        let g = geom(0, 5);
        assert_eq!(g.caret_at(14.0), 0);
        assert_eq!(g.caret_at(17.0), 0, "left half of the first cell");
        assert_eq!(g.caret_at(19.0), 1, "right half of it");
        assert_eq!(g.char_at(19.0), 0, "but the character under it is the first");
        assert_eq!(g.caret_at(60.0), 5, "past the text: the end");
        assert_eq!(g.char_at(60.0), 4, "the last character");
        assert_eq!(g.caret_at(2.0), 0, "left of the field");
        assert_eq!(g.caret_at(500.0), 5, "right of the field");
    }

    #[test]
    fn a_scrolled_field_offsets_columns_by_its_first_shown_one() {
        let g = geom(30, 60);
        assert_eq!(g.caret_at(14.0), 30);
        assert_eq!(g.char_at(14.0 + 8.0 * 3.0 + 1.0), 33);
    }

    #[test]
    fn a_long_field_shows_the_tail_unless_the_cursor_is_left_of_it() {
        assert_eq!(super::visible_start(5, 5, 10), 0, "fits");
        assert_eq!(super::visible_start(20, 20, 10), 11, "tail, caret in the last column");
        assert_eq!(super::visible_start(20, 15, 10), 11);
        assert_eq!(super::visible_start(20, 3, 10), 3, "scrolled to the cursor");
    }

    #[test]
    fn shorten_start_keeps_the_tail() {
        assert_eq!(super::shorten_start("/a/b/c", 10), "/a/b/c");
        assert_eq!(super::shorten_start("/very/long/path", 6), "\u{2026}/path");
    }
}
