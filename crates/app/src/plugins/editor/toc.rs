//! The sticky table of contents: a document's outline drawn at its top-left,
//! where scrolling never moves it. Collapsed, a column of dashes in a rail
//! left of the text, one per heading, as wide as its level is shallow, the
//! current section's in the accent colour. Resting the pointer on the column
//! expands it into a panel (above the content) listing the headings by
//! title, indented by level; a click on a row or a dash jumps there.
//!
//! The view owns the outline and the jump; this keeps the hover state and the
//! geometry of the last paint.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use ghostrealm_core::outline::OutlineItem;
use ghostrealm_core::Rect;

use crate::plugin::paint::intersect;
use crate::plugin::{rect_contains, theme, Font, PaintCx, UiMetrics};

/// A document with fewer headings shows no table of contents: one heading
/// has nothing to move between.
pub const MIN_ITEMS: usize = 2;
/// A document area narrower than this many cells shows none: the rail would
/// take too much of the text's width.
const MIN_COLS: f32 = 30.0;
/// The rail's width, in cells: the widest dash with room either side.
const RAIL_CELLS: f32 = 2.5;
/// Each level's dash width in cells, from level 1; deeper levels take the
/// last.
const DASH_CELLS: [f32; 4] = [1.5, 1.2, 0.9, 0.6];
/// At most this many dashes show. A longer outline shows a window of them
/// around the current section, a dot at an end with more past it, so the
/// column stays short and the current section always has its dash.
const MAX_DASHES: usize = 24;
/// How many of the longest titles are shaped to size the panel.
const MEASURED: usize = 8;
/// The panel's widest, in cells.
const PANEL_MAX_CELLS: f32 = 48.0;
/// The pointer rests this long on the dashes before the panel opens, so
/// passing over them doesn't flash it.
pub const HOVER_INTENT: Duration = Duration::from_millis(200);
/// The panel stays this long after the pointer leaves it, so a pointer
/// slipping off its edge doesn't close it.
pub const LEAVE_GRACE: Duration = Duration::from_millis(300);

/// A table of contents' state.
#[derive(Default)]
pub struct Toc {
    open: bool,
    /// When the pointer came to rest on the dashes (the panel opens
    /// [`HOVER_INTENT`] later).
    intent: Option<Instant>,
    /// When the pointer left the open panel (it closes [`LEAVE_GRACE`]
    /// later).
    leaving: Option<Instant>,
    /// It closed under the pointer (a jump, Escape): it opens again only
    /// once the pointer has left it.
    rearm: bool,
    /// How far the panel's rows are scrolled (px).
    scroll: f32,
    /// Scroll the current row into view at the next paint (the panel just
    /// opened).
    reveal: bool,
    /// As last painted: the dashes' column, the panel, and each item's line.
    column: Option<Rect>,
    panel: Option<Rect>,
    lines: Vec<usize>,
    /// The widest row's content (indent and title, px) of the outline with
    /// this hash, measured when the panel opens.
    measured: Option<(u64, f32)>,
}

/// The rail's width left of the text in `area` for a document whose
/// language has headings or not: none in a narrow area. Kept whatever the
/// headings, so the text doesn't move as the second comes or goes.
pub fn rail_w(ui: UiMetrics, area: Rect, has_headings: bool) -> f32 {
    if !has_headings || !wide(ui, area) {
        return 0.0;
    }
    (RAIL_CELLS * ui.cell_w).round()
}

fn wide(ui: UiMetrics, area: Rect) -> bool {
    area.w >= MIN_COLS * ui.cell_w
}

fn dash_w(ui: UiMetrics, level: u8) -> f32 {
    let i = (level.max(1) as usize - 1).min(DASH_CELLS.len() - 1);
    (DASH_CELLS[i] * ui.cell_w).round()
}

/// The dashes' vertical pitch and thickness.
fn dash_pitch(ui: UiMetrics) -> (f32, f32) {
    ((ui.cell_h * 0.5).round(), (2.0 * ui.scale).round().max(2.0))
}

/// The section `top` is in: the last of `items` heading at or above it.
pub fn current(items: &[OutlineItem], top: usize) -> Option<usize> {
    items.partition_point(|i| i.line <= top).checked_sub(1)
}

impl Toc {
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether `(x, y)` is on the table of contents as last painted: its
    /// dashes, or its open panel. Text there isn't the pointer's.
    pub fn contains(&self, (x, y): (f32, f32)) -> bool {
        let on = |r: Option<Rect>| r.is_some_and(|r| rect_contains(r, x, y));
        on(self.column) || (self.open && on(self.panel))
    }

    /// The line of the item with button id `base + i`, as last painted.
    pub fn target(&self, i: u32) -> Option<usize> {
        self.lines.get(i as usize).copied()
    }

    pub fn close(&mut self) {
        self.open = false;
        self.intent = None;
        self.leaving = None;
        self.scroll = 0.0;
    }

    /// Close it under the pointer (a jump, Escape): it opens again only once
    /// the pointer has left it and come back.
    pub fn dismiss(&mut self) {
        self.close();
        self.rearm = true;
    }

    /// The pointer moved to `pos` at `now`: start or stop the wait to open
    /// or close.
    pub fn pointer(&mut self, pos: (f32, f32), now: Instant) {
        let on = self.contains(pos);
        if !on {
            self.rearm = false;
        }
        if self.open {
            match (on, self.leaving) {
                (true, Some(_)) => self.leaving = None,
                (false, None) => self.leaving = Some(now),
                _ => {}
            }
        } else {
            match (on && !self.rearm, self.intent) {
                (true, None) => self.intent = Some(now),
                (false, Some(_)) => self.intent = None,
                _ => {}
            }
        }
    }

    /// The pointer left the view, or the view lost the keys or went off
    /// screen: close, and stop waiting to open.
    pub fn leave(&mut self) -> bool {
        let was = self.open;
        self.close();
        self.rearm = false;
        was
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self.open {
            true => self.leaving.map(|t| t + LEAVE_GRACE),
            false => self.intent.map(|t| t + HOVER_INTENT),
        }
    }

    /// Open or close as the waits run out. Returns whether it did.
    pub fn tick(&mut self, now: Instant) -> bool {
        if !self.open && self.intent.is_some_and(|t| now >= t + HOVER_INTENT) {
            self.intent = None;
            self.open = true;
            self.reveal = true;
            return true;
        }
        if self.open && self.leaving.is_some_and(|t| now >= t + LEAVE_GRACE) {
            self.close();
            return true;
        }
        false
    }

    /// The wheel over the open panel scrolls its rows (`dy` positive shows
    /// rows above). Returns whether it took the wheel.
    pub fn scroll(&mut self, (x, y): (f32, f32), dy: f32) -> bool {
        if !self.open || !self.panel.is_some_and(|p| rect_contains(p, x, y)) {
            return false;
        }
        if dy.is_finite() {
            self.scroll = (self.scroll - dy).max(0.0);
        }
        true
    }

    /// Draw the table of contents of `items` in the document area `area`
    /// (the section `current` highlighted): the dashes in the area's rail
    /// ([`rail_w`]), or, open, the panel above the content. Rows and dashes
    /// are buttons `base + i` for item `i`. Paint the panel before the
    /// content's own buttons, so its rows take the clicks where they cover
    /// one; the dashes after the content's background.
    pub fn paint(
        &mut self,
        cx: &mut PaintCx,
        area: Rect,
        items: &[OutlineItem],
        current: Option<usize>,
        base: u32,
    ) {
        self.column = None;
        self.panel = None;
        self.lines.clear();
        if cx.modal || items.len() < MIN_ITEMS || !wide(cx.ui, area) {
            self.close();
            return;
        }
        self.lines.extend(items.iter().map(|i| i.line));
        match self.open {
            true => self.paint_panel(cx, area, items, current, base),
            false => self.paint_dashes(cx, area, items, current, base),
        }
    }

    /// The collapsed column: a window of [`MAX_DASHES`] around the current
    /// section, ending in a dot where more lie past it.
    fn paint_dashes(
        &mut self,
        cx: &mut PaintCx,
        area: Rect,
        items: &[OutlineItem],
        current: Option<usize>,
        base: u32,
    ) {
        let ui = cx.ui;
        let (pitch, thick) = dash_pitch(ui);
        let top = area.y + (ui.cell_h * 0.75).round();
        let fit = ((area.y + area.h - top - ui.cell_h * 0.5) / pitch).floor().max(0.0) as usize;
        let slots = items.len().min(MAX_DASHES).min(fit);
        if slots == 0 {
            return;
        }
        let at = current.unwrap_or(0);
        let first = at.saturating_sub(slots / 2).min(items.len() - slots);
        let x = area.x + ((RAIL_CELLS * ui.cell_w - dash_w(ui, 1)) * 0.5).round();
        let rail = (RAIL_CELLS * ui.cell_w).round();
        let column = Rect { x: area.x, y: top - ((pitch - thick) * 0.5).round(), w: rail, h: slots as f32 * pitch };
        self.column = Some(column);
        cx.with_clip(area, |cx| {
            for slot in 0..slots {
                let i = first + slot;
                let y = top + slot as f32 * pitch;
                // A dot stands for its slot's heading too: it jumps there.
                let hit = Rect { x: area.x, y: column.y + slot as f32 * pitch, w: rail, h: pitch };
                let hov = cx.button(hit, base + i as u32);
                let more = (slot == 0 && first > 0) || (slot + 1 == slots && first + slots < items.len());
                if more {
                    let d = thick + 1.0;
                    let dot = Rect { x, y: y + ((thick - d) * 0.5).round(), w: d, h: d };
                    cx.round_rect(dot, d * 0.5, if hov { theme::GLYPH_HOVER } else { theme::DIM }, 1.0, None);
                    continue;
                }
                let color = match (Some(i) == current, hov) {
                    (true, _) => cx.chrome.accent,
                    (false, true) => theme::GLYPH_HOVER,
                    (false, false) => theme::DIM,
                };
                let dash = Rect { x, y, w: dash_w(ui, items[i].level), h: thick };
                cx.round_rect(dash, thick * 0.5, color, 1.0, None);
            }
        });
    }

    /// The open panel, over the content and clipped to `area`: a row per
    /// heading (its dash, then its title indented by level), the current
    /// one bright, a pill under the hovered one; scrolled by the wheel.
    fn paint_panel(
        &mut self,
        cx: &mut PaintCx,
        area: Rect,
        items: &[OutlineItem],
        current: Option<usize>,
        base: u32,
    ) {
        let ui = cx.ui;
        let (cw, ch) = (ui.cell_w, ui.cell_h);
        let (_, thick) = dash_pitch(ui);
        let (pad_x, pad_y) = ((cw * 0.5).round(), (ch * 0.4).round());
        let row_h = (ch * 1.5).round();
        let indent = (cw * 1.5).round();
        let min_level = items.iter().map(|i| i.level).min().unwrap_or(1);
        let indent_of = |level: u8| (level - min_level) as f32 * indent;
        // From the row's left: the dash, then the title.
        let dash_x = (cw * 0.5).round();
        let title_x = dash_x + dash_w(ui, 1) + (cw * 0.75).round();
        let key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for i in items {
                (i.level, &i.text).hash(&mut h);
            }
            h.finish()
        };
        let content_w = match self.measured {
            Some((k, w)) if k == key => w,
            _ => {
                // Shaping every title would take a scratch buffer each: the
                // widest is among the longest.
                let mut longest: Vec<&OutlineItem> = items.iter().collect();
                let chars = |i: &OutlineItem| i.text.chars().count() + (i.level - min_level) as usize * 2;
                longest.sort_by_key(|i| std::cmp::Reverse(chars(i)));
                longest.truncate(MEASURED);
                let widest = longest.iter().map(|i| indent_of(i.level) + cx.shape(&i.text, Font::SANS, area.w).width);
                let w = widest.fold(0.0, f32::max);
                self.measured = Some((key, w));
                w
            }
        };
        // From the rail's left edge, so it covers the rail.
        let x = area.x;
        let y = area.y + (ch * 0.25).round();
        let max_w = (area.x + area.w - x - (cw * 0.25).round()).min(PANEL_MAX_CELLS * cw);
        let w = (title_x + content_w + 2.0 * pad_x).min(max_w).max((16.0 * cw).min(max_w)).round();
        let rows_h = items.len() as f32 * row_h;
        let h = (rows_h + 2.0 * pad_y).min(area.y + area.h - y - (ch * 0.25).round()).max(row_h).round();
        let panel = Rect { x, y, w, h };
        self.panel = Some(panel);
        let inner = Rect { x, y: y + pad_y, w, h: (h - 2.0 * pad_y).max(1.0) };
        let max_scroll = (rows_h - inner.h).max(0.0);
        if std::mem::take(&mut self.reveal) {
            let at = current.unwrap_or(0) as f32 * row_h;
            self.scroll = at - (inner.h - row_h) * 0.5;
        }
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let accent = cx.chrome.accent;
        let scroll = self.scroll;
        cx.above(|cx| {
            cx.with_clip(area, |cx| {
                let border = Some((theme::hairline(ui), theme::INPUT_BORDER, 1.0));
                cx.round_rect(panel, (ch * 0.5).round(), theme::TOOLTIP_BG, 1.0, border);
            });
            let Some(clip) = intersect(inner, area) else { return };
            let first = (scroll / row_h).floor() as usize;
            let last = (((scroll + inner.h) / row_h).ceil() as usize).min(items.len());
            cx.with_clip(clip, |cx| {
                for (i, item) in items.iter().enumerate().take(last).skip(first) {
                    let top = (inner.y + i as f32 * row_h - scroll).round();
                    let row = Rect { x: x + (pad_x * 0.5).round(), y: top, w: w - pad_x, h: row_h };
                    let hov = cx.button(row, base + i as u32);
                    if hov {
                        cx.round_rect(row, theme::pill_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
                    }
                    let is_current = Some(i) == current;
                    let dash = Rect {
                        x: row.x + dash_x,
                        y: top + ((row_h - thick) * 0.5).round(),
                        w: dash_w(ui, item.level),
                        h: thick,
                    };
                    cx.round_rect(dash, thick * 0.5, if is_current { accent } else { theme::DIM }, 1.0, None);
                    if item.text.is_empty() {
                        continue;
                    }
                    let color = if is_current || hov { theme::TEXT } else { theme::LABEL };
                    let left = row.x + title_x + indent_of(item.level);
                    let text_clip = Rect { x: left, w: (row.x + row.w - pad_x - left).max(0.0), ..clip };
                    let text_top = top + ((row_h - ch) * 0.5).round();
                    cx.row_font(&[(item.text.clone(), color)], Font::SANS, left, text_top, area.w, text_clip, color);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{EditorBuffer, EditorView, ViewMode, BTN_LINK, BTN_TOC};
    use super::*;
    use crate::plugin::paint::round_quad;
    use crate::plugin::testing::{press, tmpdir, Harness, UI};
    use crate::plugin::{MouseButton, MouseEvent, View};
    use ghostrealm_terminal::Key;

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 200.0 };

    fn doc(tag: &str, name: &str, text: &str) -> (crate::plugin::testing::TmpDir, EditorView) {
        let dir = tmpdir(tag);
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        (dir, EditorView::new(EditorBuffer::open(path).unwrap()))
    }

    /// Headings of levels 1 to 5, each followed by `gap` lines of text.
    fn sections(gap: usize) -> String {
        let mut text = String::new();
        for (level, title) in ["One", "Two", "Three", "Four", "Five"].iter().enumerate() {
            text += &format!("{} {title}\n", "#".repeat(level + 1));
            text += &"text\n".repeat(gap);
        }
        text
    }

    /// The table of contents' buttons: item index and hit rect.
    fn buttons(h: &Harness) -> Vec<(u32, Rect)> {
        let ids = h.frame.view_buttons.iter().filter(|b| (BTN_TOC..BTN_LINK).contains(&b.2));
        ids.map(|b| (b.2 - BTN_TOC, b.0)).collect()
    }

    /// The collapsed dashes (rounded quads in the rail): width and colour.
    fn dashes(h: &Harness) -> Vec<(f32, [f32; 4])> {
        let (sw, _) = h.frame.screen;
        let rail = RAIL_CELLS * UI.cell_w;
        let in_rail = |q: &&crate::plugin::paint::QuadInstance| q.radius > 0.0 && (q.pos[0] + 1.0) / 2.0 * sw < rail;
        h.frame.bg.iter().filter(in_rail).map(|q| (q.px[0], q.color)).collect()
    }

    /// A dot's width: the dashes' thickness and a pixel.
    fn theme_dot(_h: &Harness) -> f32 {
        dash_pitch(UI).1 + 1.0
    }

    fn color(c: [u8; 3]) -> [f32; 4] {
        round_quad(Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 }, 1.0, 1.0, 0.0, c, 1.0, None).color
    }

    fn centre(r: Rect) -> (f32, f32) {
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    }

    /// Rest the pointer on the dashes until the panel opens, and paint it.
    fn open(h: &mut Harness, v: &mut EditorView) {
        h.paint(v, RECT);
        let (_, dash) = buttons(h)[0];
        // From off it: one dismissed under the pointer opens again only so.
        h.mouse(v, MouseEvent::Move { pos: (390.0, 190.0) });
        h.cursor = centre(dash);
        h.mouse(v, MouseEvent::Move { pos: h.cursor });
        h.tick(v, Instant::now() + HOVER_INTENT);
        assert!(v.toc.is_open());
        h.paint(v, RECT);
    }

    #[test]
    fn fewer_than_two_headings_show_no_dashes_but_keep_the_rail() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-one", "doc.md", "# Only\ntext\n```\n## code\n```\n");
        h.paint(&mut v, RECT);
        assert!(buttons(&h).is_empty());
        assert!(dashes(&h).is_empty());
        let rail = (RAIL_CELLS * UI.cell_w).round();
        assert_eq!(text_x(&h), rail + 4.0 * UI.cell_w, "the rail stays for a language with headings");
        let (_v_dir, mut v) = doc("toc-plain", "doc.txt", "# a\n# b\n");
        h.paint(&mut v, RECT);
        assert!(buttons(&h).is_empty(), "a file with no highlighter has no outline");
        assert_eq!(text_x(&h), 4.0 * UI.cell_w, "nor a rail");
        let (_v_dir, mut v) = doc("toc-narrow", "doc.md", &sections(2));
        h.paint(&mut v, Rect { w: MIN_COLS * UI.cell_w - 1.0, ..RECT });
        assert!(buttons(&h).is_empty(), "too narrow");
        assert_eq!(text_x(&h), 4.0 * UI.cell_w);
    }

    #[test]
    fn a_second_heading_brings_the_dashes_without_moving_the_text() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-second", "doc.md", "# One\ntext\n");
        h.paint(&mut v, RECT);
        let before = text_x(&h);
        let b = v.buffer_mut();
        b.cursor = (1, 4);
        b.insert_str("\n## Two");
        v.note_edits();
        h.paint(&mut v, RECT);
        assert_eq!(dashes(&h).len(), 2);
        assert_eq!(text_x(&h), before);
    }

    /// The text's left edge: line numbers sit left of it.
    fn text_x(h: &Harness) -> f32 {
        h.frame.text.iter().map(|t| t.left).fold(f32::MIN, f32::max)
    }

    #[test]
    fn collapsed_dashes_narrow_by_level_and_the_current_section_follows_the_scroll() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-dashes", "doc.md", &sections(20));
        h.paint(&mut v, RECT);
        let widths: Vec<f32> = dashes(&h).iter().map(|d| d.0).collect();
        assert_eq!(widths, [12.0, 10.0, 7.0, 5.0, 5.0], "1.5, 1.2, 0.9, then 0.6 cells, rounded");
        let lit = |h: &Harness| dashes(h).iter().position(|d| d.1 == color(h.chrome.accent));
        assert_eq!(lit(&h), Some(0), "the top is in the first section");
        assert!(dashes(&h)[1..].iter().all(|d| d.1 == color(theme::DIM)));
        // The headings are on lines 0, 21, 42, 63 and 84.
        let tops = [(20, Some(0)), (21, Some(1)), (41, Some(1)), (42, Some(2)), (63, Some(3)), (104, Some(4))];
        for (scroll, want) in tops {
            v.buffer_mut().scroll = scroll;
            h.paint(&mut v, RECT);
            assert_eq!(lit(&h), want, "scrolled to {scroll}");
        }
        // The dashes stay put as the text scrolls.
        let at = |h: &Harness| buttons(h).iter().map(|b| b.1).collect::<Vec<_>>();
        let scrolled = at(&h);
        v.buffer_mut().scroll = 0;
        h.paint(&mut v, RECT);
        assert_eq!(at(&h), scrolled);
        // Above the first heading no section is current.
        let (_v_dir, mut v) = doc("toc-preamble", "doc.md", &format!("intro\n{}", sections(2)));
        h.paint(&mut v, RECT);
        assert_eq!(lit(&h), None);
    }

    #[test]
    fn a_long_outline_shows_a_window_of_dashes_around_the_current_section() {
        let mut h = Harness::new();
        let text: String = (0..60).map(|i| format!("## h{i}\ntext\n")).collect();
        let (_v_dir, mut v) = doc("toc-many", "doc.md", &text);
        let rect = Rect { h: 600.0, ..RECT };
        h.paint(&mut v, rect);
        let ids: Vec<u32> = buttons(&h).iter().map(|b| b.0).collect();
        assert_eq!(ids, (0..MAX_DASHES as u32).collect::<Vec<_>>(), "a dot takes presses for its slot's heading");
        assert_eq!(dashes(&h).last().map(|d| d.0), Some(theme_dot(&h)), "the last slot is a dot: more below");
        v.buffer_mut().scroll = 60;
        h.paint(&mut v, rect);
        let ids: Vec<u32> = buttons(&h).iter().map(|b| b.0).collect();
        assert_eq!(ids.len(), MAX_DASHES);
        assert!(ids.contains(&30), "the current section keeps its dash: {ids:?}");
        let widths: Vec<f32> = dashes(&h).iter().map(|d| d.0).collect();
        assert_eq!((widths[0], widths[MAX_DASHES - 1]), (theme_dot(&h), theme_dot(&h)), "dots at both ends");
    }

    #[test]
    fn resting_on_the_dashes_opens_the_panel_after_a_moment_listing_the_headings() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-open", "doc.md", &sections(20));
        h.paint(&mut v, RECT);
        let (_, dash) = buttons(&h)[2];
        let t0 = Instant::now();
        h.mouse(&mut v, MouseEvent::Move { pos: centre(dash) });
        assert!(v.deadline(&h.cfg).is_some());
        h.tick(&mut v, t0);
        assert!(!v.toc.is_open(), "passing over doesn't open it");
        // Moving off before the delay cancels it.
        h.mouse(&mut v, MouseEvent::Move { pos: (200.0, 100.0) });
        h.tick(&mut v, t0 + HOVER_INTENT * 2);
        assert!(!v.toc.is_open());
        open(&mut h, &mut v);
        let texts = h.texts();
        for title in ["One", "Two", "Three", "Four", "Five"] {
            assert!(texts.iter().any(|t| t == title), "{title} in {texts:?}");
        }
        assert!(dashes(&h).is_empty(), "the panel replaces the dashes");
        // Titles indent by level.
        let left = |title: &str| {
            let text = |i: &&crate::plugin::TextItem| {
                h.text.buffer(i.src).and_then(|b| b.layout_runs().next().map(|r| r.text))
            };
            h.frame.overlay_text.iter().find(|i| text(i) == Some(title)).map(|i| i.left)
        };
        let (one, two, five) = (left("One").unwrap(), left("Two").unwrap(), left("Five").unwrap());
        assert!(one < two && two < five, "{one} {two} {five}");
        // A row under the pointer shows the hover pill.
        let (_, row) = buttons(&h)[1];
        h.cursor = centre(row);
        h.paint(&mut v, RECT);
        let pill = color(theme::HOVER_BG);
        assert!(h.frame.top.iter().any(|q| q.color[..3] == pill[..3] && q.color[3] == theme::HOVER_ALPHA));
    }

    #[test]
    fn escape_leaving_the_view_or_the_panel_closes_it() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-close", "doc.md", &sections(20));
        open(&mut h, &mut v);
        let (used, _) = h.key(&mut v, press(Key::Escape));
        assert!(used && !v.toc.is_open());
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: h.cursor });
        h.tick(&mut v, Instant::now() + HOVER_INTENT);
        assert!(!v.toc.is_open(), "not again under the pointer that dismissed it");
        open(&mut h, &mut v);
        h.mouse(&mut v, MouseEvent::Leave { pos: (900.0, 0.0) });
        assert!(!v.toc.is_open());
        open(&mut h, &mut v);
        let t0 = Instant::now();
        h.mouse(&mut v, MouseEvent::Move { pos: (390.0, 190.0) });
        h.tick(&mut v, t0);
        assert!(v.toc.is_open(), "a grace before it closes");
        // Coming back keeps it.
        h.mouse(&mut v, MouseEvent::Move { pos: h.cursor });
        h.tick(&mut v, t0 + LEAVE_GRACE * 2);
        assert!(v.toc.is_open());
        h.mouse(&mut v, MouseEvent::Move { pos: (390.0, 190.0) });
        h.tick(&mut v, Instant::now() + LEAVE_GRACE);
        assert!(!v.toc.is_open());
        // Escape with it closed is the editor's.
        assert!(!h.key(&mut v, press(Key::Escape)).0);
    }

    #[test]
    fn a_click_jumps_to_the_heading_in_integrated_and_source() {
        for mode in [ViewMode::Integrated, ViewMode::Source] {
            let mut h = Harness::new();
            let (_v_dir, mut v) = doc("toc-jump", "doc.md", &sections(20));
            v.set_view_mode(mode);
            open(&mut h, &mut v);
            let (i, _) = buttons(&h)[2];
            h.button(&mut v, BTN_TOC + i);
            assert!(!v.toc.is_open(), "{mode:?}");
            assert_eq!(v.buffer().cursor, (42, 0), "{mode:?}: the caret at the heading");
            h.paint(&mut v, RECT);
            assert_eq!(v.buffer().scroll, 42, "{mode:?}: the heading at the top");
            // The pointer still on it doesn't open it again until it leaves.
            h.mouse(&mut v, MouseEvent::Move { pos: h.cursor });
            h.tick(&mut v, Instant::now() + HOVER_INTENT);
            assert!(!v.toc.is_open(), "{mode:?}");
            // A collapsed dash jumps too.
            let (i, _) = buttons(&h)[4];
            h.button(&mut v, BTN_TOC + i);
            assert_eq!(v.buffer().cursor, (84, 0), "{mode:?}");
        }
    }

    #[test]
    fn a_click_scrolls_the_preview_to_the_heading() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-preview", "doc.md", &sections(20));
        v.set_view_mode(ViewMode::Preview);
        h.paint(&mut v, RECT);
        let lit = dashes(&h).iter().position(|d| d.1 == color(h.chrome.accent));
        assert_eq!(dashes(&h).len(), 5, "the parsed headings");
        assert_eq!(lit, Some(0));
        open(&mut h, &mut v);
        assert!(h.texts().iter().any(|t| t == "Three"));
        let (i, _) = buttons(&h)[2];
        h.button(&mut v, BTN_TOC + i);
        h.paint(&mut v, RECT);
        assert_eq!(v.preview.top_line(), Some(42));
        let lit = dashes(&h).iter().position(|d| d.1 == color(h.chrome.accent));
        assert_eq!(lit, Some(2), "its section is current");
    }

    #[test]
    fn a_press_beside_the_dashes_places_the_caret_and_one_on_them_does_not() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-click", "doc.md", &sections(20));
        h.paint(&mut v, RECT);
        let (_, dash) = buttons(&h)[3];
        let y = centre(dash).1;
        let down = |pos| MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left };
        h.mouse(&mut v, down(centre(dash)));
        assert_eq!(v.buffer().cursor, (0, 0), "the dash's button takes the press");
        // Row 0 is the H1 (29 px tall), then a base row per line.
        let line = 1 + ((y - 29.0) / UI.cell_h) as usize;
        // Right of the rail and the gutter.
        let text_x = text_x(&h);
        assert_eq!(text_x, 20.0 + 5.0 * UI.cell_w, "the rail, then a gutter for three digits");
        h.mouse(&mut v, down((text_x + 2.0 * UI.cell_w, y)));
        assert_eq!(v.buffer().cursor, (line, 2));
    }

    #[test]
    fn the_wheel_over_the_open_panel_scrolls_its_rows_not_the_text() {
        let mut h = Harness::new();
        let text: String = (0..60).map(|i| format!("## h{i}\ntext\n")).collect();
        let (_v_dir, mut v) = doc("toc-wheel", "doc.md", &text);
        open(&mut h, &mut v);
        let (_, row) = buttons(&h)[0];
        h.scroll(&mut v, centre(row), -100.0);
        assert_eq!(v.buffer().scroll, 0);
        h.paint(&mut v, RECT);
        assert!(buttons(&h)[0].0 > 0, "rows scrolled");
        h.scroll(&mut v, (390.0, 190.0), -100.0);
        assert!(v.buffer().scroll > 0, "outside it the text scrolls");
    }

    #[test]
    fn a_long_documents_outline_follows_edits_once_they_pause() {
        let mut h = Harness::new();
        let text = format!("# A\n{}# B\n", "text\n".repeat(ghostrealm_core::outline::DEBOUNCE_LINES));
        let (_v_dir, mut v) = doc("toc-debounce", "doc.md", &text);
        h.paint(&mut v, RECT);
        assert_eq!(buttons(&h).len(), 2);
        let b = v.buffer_mut();
        b.cursor = (1, 0);
        b.insert_str("# New\n");
        v.note_edits();
        h.paint(&mut v, RECT);
        assert_eq!(buttons(&h).len(), 2, "waits for the edits to pause");
        let due = v.deadline(&h.cfg).expect("a deadline for the outline");
        h.tick(&mut v, due);
        h.paint(&mut v, RECT);
        assert_eq!(buttons(&h).len(), 3);
    }

    #[test]
    fn losing_the_highlighter_drops_a_waiting_outline() {
        let mut h = Harness::new();
        let text = format!("# A\n{}# B\n", "text\n".repeat(ghostrealm_core::outline::DEBOUNCE_LINES));
        let (_v_dir, mut v) = doc("toc-lost", "doc.md", &text);
        h.paint(&mut v, RECT);
        v.buffer_mut().insert_str("# New\n");
        v.note_edits();
        h.paint(&mut v, RECT);
        assert!(v.deadline(&h.cfg).is_some(), "the outline waits");
        let tmp = tmpdir("toc-lost");
        let plain = tmp.join("doc.txt");
        v.save_as(plain).unwrap();
        h.paint(&mut v, RECT);
        assert_eq!(v.deadline(&h.cfg), None, "nothing left to wait for, so no tick repaints");
        assert!(buttons(&h).is_empty());
    }

    #[test]
    fn a_jump_while_the_outline_waits_lands_on_the_moved_heading() {
        let mut h = Harness::new();
        let text = format!("# A\n{}# B\n", "text\n".repeat(ghostrealm_core::outline::DEBOUNCE_LINES));
        let (_v_dir, mut v) = doc("toc-moved", "doc.md", &text);
        h.paint(&mut v, RECT);
        let b = ghostrealm_core::outline::DEBOUNCE_LINES + 1;
        let buf = v.buffer_mut();
        buf.cursor = (1, 0);
        buf.insert_str("one\ntwo\nthree\n");
        v.note_edits();
        h.paint(&mut v, RECT);
        assert!(v.deadline(&h.cfg).is_some(), "not outlined again yet");
        let (i, _) = buttons(&h)[1];
        h.button(&mut v, BTN_TOC + i);
        assert_eq!(v.buffer().cursor, (b + 3, 0));
        assert_eq!(v.buffer().lines[b + 3], "# B");
    }

    #[test]
    fn a_drag_from_the_panel_leaves_the_caret_be() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-drag", "doc.md", &sections(20));
        open(&mut h, &mut v);
        // Its padding: no row's button takes the press, so the view gets it.
        let panel = v.toc.panel.unwrap();
        let pad = (panel.x + 2.0, panel.y + 1.0);
        assert!(buttons(&h).iter().all(|(_, r)| !rect_contains(*r, pad.0, pad.1)));
        let left = MouseButton::Left;
        h.mouse(&mut v, MouseEvent::Down { pos: pad, clicks: 1, button: left });
        h.mouse(&mut v, MouseEvent::Drag { pos: (200.0, 100.0), button: left });
        h.mouse(&mut v, MouseEvent::Up { pos: (200.0, 100.0), dragged: true, button: left });
        assert_eq!((v.buffer().cursor, v.buffer().anchor), ((0, 0), None));
        // The next press is the text's again.
        h.mouse(&mut v, MouseEvent::Leave { pos: (900.0, 0.0) });
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: (200.0, 100.0), clicks: 1, button: left });
        assert_ne!(v.buffer().cursor, (0, 0));
    }

    #[test]
    fn losing_the_keys_or_the_screen_closes_it_and_stops_the_wait() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-hidden", "doc.md", &sections(20));
        open(&mut h, &mut v);
        h.focus(&mut v, false);
        assert!(!v.toc.is_open());
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: h.cursor });
        assert!(v.deadline(&h.cfg).is_some(), "resting on the dashes again");
        v.shown(false);
        assert_eq!(v.deadline(&h.cfg), None, "off screen, it doesn't open");
        open(&mut h, &mut v);
        v.shown(false);
        assert!(!v.toc.is_open());
    }

    #[test]
    fn the_panel_covers_the_rail() {
        let mut h = Harness::new();
        let (_v_dir, mut v) = doc("toc-cover", "doc.md", &sections(20));
        open(&mut h, &mut v);
        let panel = v.toc.panel.unwrap();
        assert_eq!(panel.x, RECT.x);
        assert!(panel.x + panel.w > RAIL_CELLS * UI.cell_w);
    }
}
