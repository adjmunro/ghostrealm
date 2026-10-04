//! Reusable pieces views compose from [`PaintCx`] primitives.

use ghostrealm_core::Rect;
use glyphon::Family;

use super::{theme, PaintCx};

/// A single-line text field in `r`: background, border, the value (or a dim
/// placeholder when the value is empty), and, when `focused`, a caret after the
/// value. Text is monospace and keeps its tail when it doesn't fit (the most
/// specific part of a path stays visible).
pub fn text_field(cx: &mut PaintCx, r: Rect, value: &str, placeholder: &str, focused: bool) {
    let scale = cx.ui.scale;
    let pad = 8.0 * scale;
    cx.fill(r, cx.chrome.background);
    cx.border(r, theme::INPUT_BORDER);
    let inner_w = (r.w - pad).max(1.0);
    let cols = (inner_w / cx.ui.cell_w) as usize;
    let (text, color, is_placeholder) = if value.is_empty() {
        (shorten_start(placeholder, cols), theme::DIM, true)
    } else {
        (shorten_start(value, cols), theme::INPUT_TEXT, false)
    };
    let left = r.x + pad * 0.5;
    let top = r.y + (r.h - cx.ui.cell_h) * 0.5;
    let shaped = cx.shape(&text, Family::Monospace, inner_w);
    cx.place(shaped, left, top, r, color);
    let caret_x = left + if is_placeholder { 0.0 } else { shaped.width } + scale;
    if focused && caret_x < r.x + r.w {
        let caret = Rect {
            x: caret_x,
            y: r.y + 2.0 * scale,
            w: 2.0 * scale,
            h: (r.h - 4.0 * scale).max(1.0),
        };
        cx.fill_top(caret, cx.chrome.accent, 0.9);
    }
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
    fn shorten_start_keeps_the_tail() {
        assert_eq!(super::shorten_start("/a/b/c", 10), "/a/b/c");
        assert_eq!(super::shorten_start("/very/long/path", 6), "\u{2026}/path");
    }
}
