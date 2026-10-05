//! Reusable pieces views compose from [`PaintCx`] primitives.

use ghostrealm_core::Rect;
use glyphon::Family;

use super::text_edit::LineEdit;
use super::{theme, PaintCx};

/// A single-line text field in `r`: background, border, the text with its
/// selection (or a dim placeholder when empty), and, when `focused`, a caret at
/// the cursor. Text is monospace; when it doesn't fit, the field shows its tail
/// (the most specific part of a path) unless the cursor is further left.
pub fn text_field(cx: &mut PaintCx, r: Rect, edit: &LineEdit, placeholder: &str, focused: bool) {
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

    let caret_col = if edit.is_empty() {
        let text = shorten_start(placeholder, cols);
        let shaped = cx.shape(&text, Family::Monospace, inner_w);
        cx.place(shaped, left, top, r, theme::DIM);
        0
    } else {
        let chars: Vec<char> = edit.text().chars().collect();
        let first = visible_start(chars.len(), edit.cursor(), cols);
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
