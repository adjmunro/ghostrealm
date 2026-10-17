//! The Files surface's file tabs: a pill per document in its top row (file
//! icon, name, and a close slot), the active one filled. Too many to fit
//! shrink, the widest first, down to a minimum; past that the row scrolls
//! sideways (wheel or trackpad over it), keeping the active tab in view.

use std::path::{Component, Path, PathBuf};

use ghostrealm_core::Rect;

use crate::plugin::icons::{FileKind, Icon};
use crate::plugin::paint::intersect;
use crate::plugin::{theme, DocId, Font, PaintCx};

/// The tabs' button ids, past the surface's own and below the sidebar's
/// (see [`button_id`]).
pub(super) const TAB_IDS: u32 = 1 << 16;

/// A tab no wider than this, and none shrunk narrower (logical px).
const TAB_MAX_W: f32 = 240.0;
const TAB_MIN_W: f32 = 90.0;
/// Space between tabs, and inside a tab around its contents (logical px).
const TAB_GAP: f32 = 2.0;
const TAB_PAD: f32 = 8.0;
/// Space between a tab's icon, name, and close slot (logical px).
const TAB_SPACE: f32 = 6.0;

/// What a tab shows of its document.
pub(super) struct TabInfo {
    pub id: DocId,
    pub title: String,
    /// Dim text after the title telling it apart from another tab's of the
    /// same name (the file's folder).
    pub suffix: Option<String>,
    /// The tooltip: the file's full path.
    pub path: Option<String>,
    pub kind: FileKind,
    /// The preview tab: the next single click in the sidebar replaces it.
    pub preview: bool,
    pub modified: bool,
    /// The file changed on disk under unsaved edits.
    pub changed: bool,
}

/// The button id of document `id`'s close slot.
pub(super) fn button_id(id: DocId) -> u32 {
    TAB_IDS + id.0
}

/// The document whose close slot a button id names: ids from [`TAB_IDS`]
/// below `end`.
pub(super) fn from_button(id: u32, end: u32) -> Option<DocId> {
    (TAB_IDS..end).contains(&id).then(|| DocId(id - TAB_IDS))
}

/// Disambiguating suffixes for `titles` of files in `dirs`: for each title
/// another one shares, the shortest end of its folder's path that no other
/// file of that title has (`a/src` beside `b/src`), `/` for the root.
pub(super) fn suffixes(titles: &[String], dirs: &[Option<PathBuf>]) -> Vec<Option<String>> {
    (0..titles.len())
        .map(|i| {
            let dir = dirs[i].as_deref()?;
            let others: Vec<&Path> = (0..titles.len())
                .filter(|j| *j != i && titles[*j] == titles[i])
                .filter_map(|j| dirs[j].as_deref())
                .collect();
            if others.is_empty() {
                return None;
            }
            let n = parts(dir).len();
            let k = (1..=n).find(|k| others.iter().all(|o| tail(o, *k) != tail(dir, *k))).unwrap_or(n);
            Some(tail(dir, k))
        })
        .collect()
}

fn parts(dir: &Path) -> Vec<String> {
    dir.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The last `k` names of `dir`'s path, `/`-joined; all of them from the root
/// with a leading `/`, which alone is the root.
fn tail(dir: &Path, k: usize) -> String {
    let parts = parts(dir);
    let k = k.min(parts.len());
    let end = parts[parts.len() - k..].join("/");
    match k == parts.len() && dir.has_root() {
        true => format!("/{end}"),
        false => end,
    }
}

/// A tab's title spans fitting `avail` (physical px): whole if they fit;
/// else the title whole and the dim suffix cut; else, when even the title
/// alone doesn't fit, the title cut and no suffix.
fn fitting_spans(cx: &mut PaintCx, tab: &TabInfo, font: Font, avail: f32) -> Vec<(String, [u8; 3])> {
    let color = if tab.changed { theme::WARN } else { theme::TEXT };
    let title_w = cx.shape(&tab.title, font, avail.max(1.0)).width;
    let suffix = tab.suffix.as_ref().map(|d| format!(" \u{00b7} {d}"));
    match suffix {
        Some(suffix) if title_w <= avail => {
            let suffix = cx.cut_to_fit(&suffix, font, avail - title_w);
            vec![(tab.title.clone(), color), (suffix, theme::TEXT_DIM)]
        }
        _ => vec![(cx.cut_to_fit(&tab.title, font, avail), color)],
    }
}

/// Widths for tabs wanting `natural` (each at most its own) whose sum fits
/// `avail` where it can: the widest shrink first, none below `min` (or its
/// own width, if less). Whole pixels, so pills don't seam.
pub(super) fn fit(natural: &[f32], min: f32, avail: f32) -> Vec<f32> {
    let total: f32 = natural.iter().sum();
    if total <= avail {
        return natural.iter().map(|w| w.floor()).collect();
    }
    // The cap `c` with Σ min(w, c) = avail: past the k narrowest, the rest
    // share what is left equally.
    let mut sorted = natural.to_vec();
    sorted.sort_by(f32::total_cmp);
    let mut below = 0.0;
    let mut cap = min;
    for (k, w) in sorted.iter().enumerate() {
        let share = (avail - below) / (sorted.len() - k) as f32;
        if share <= *w {
            cap = share;
            break;
        }
        below += w;
    }
    let cap = cap.max(min);
    natural.iter().map(|w| w.min(cap).floor()).collect()
}

/// A `side` square centred in `r`, on whole pixels.
fn square_in(r: Rect, side: f32) -> Rect {
    Rect { x: r.x + ((r.w - side) * 0.5).round(), y: r.y + ((r.h - side) * 0.5).round(), w: side, h: side }
}

/// The active tab and the layout it was scrolled into view in.
#[derive(Clone, Copy, PartialEq)]
struct Reveal {
    id: DocId,
    area_w: f32,
    tabs: usize,
    /// The active tab's offset from the first and its width.
    at: f32,
    w: f32,
}

/// The tab row's state between paints.
#[derive(Default)]
pub(super) struct TabRow {
    /// How far the tabs are scrolled left (physical px).
    scroll: f32,
    /// The most it may scroll, as last painted.
    max_scroll: f32,
    /// What the row last scrolled the active tab into view for: any of it
    /// changing scrolls it into view again.
    revealed: Option<Reveal>,
    /// The tabs' area as last painted.
    area: Option<Rect>,
    /// Each tab's pill as last painted (unclipped), in tab order.
    pills: Vec<(DocId, Rect)>,
}

impl TabRow {
    /// The tab whose pill is at `pos`, as last painted.
    pub fn tab_at(&self, pos: (f32, f32)) -> Option<DocId> {
        let inside = |r: Rect| pos.0 >= r.x && pos.0 < r.x + r.w && pos.1 >= r.y && pos.1 < r.y + r.h;
        if !self.area.is_some_and(inside) {
            return None;
        }
        self.pills.iter().find(|(_, r)| inside(*r)).map(|(id, _)| *id)
    }

    /// Whether `pos` is over the tabs' area.
    pub fn contains(&self, pos: (f32, f32)) -> bool {
        self.area.is_some_and(|r| pos.0 >= r.x && pos.0 < r.x + r.w && pos.1 >= r.y && pos.1 < r.y + r.h)
    }

    /// Scroll by a wheel or trackpad `delta` (physical px; positive reveals
    /// the tabs to the left). Returns whether it moved.
    pub fn scroll_by(&mut self, delta: f32) -> bool {
        let to = (self.scroll - delta).clamp(0.0, self.max_scroll);
        let moved = to != self.scroll;
        self.scroll = to;
        moved
    }

    /// The pill of tab `id`, as last painted.
    #[cfg(test)]
    pub fn pill(&self, id: DocId) -> Option<Rect> {
        self.pills.iter().find(|(d, _)| *d == id).map(|(_, r)| *r)
    }

    #[cfg(test)]
    pub fn area(&self) -> Option<Rect> {
        self.area
    }

    /// Paint `tabs` into `area` (the row right of the sidebar button), `active`
    /// filled. Registers each close slot as a button; a press on a tab
    /// itself reaches the view (it tells a double click apart).
    pub fn paint(&mut self, cx: &mut PaintCx, area: Rect, tabs: &[TabInfo], active: Option<DocId>) {
        let ui = cx.ui;
        let s = ui.scale;
        let (pad, space, gap) = ((TAB_PAD * s).round(), (TAB_SPACE * s).round(), (TAB_GAP * s).round());
        let icon = (ui.cell_h * 0.9).round();
        let close = ui.cell_h.round();
        let inset = (3.0 * s).round();
        let natural: Vec<f32> = tabs
            .iter()
            .map(|t| {
                let font = Font::SANS.italic(t.preview);
                let suffix = t.suffix.as_ref().map(|d| format!(" \u{00b7} {d}")).unwrap_or_default();
                let name = cx.shape(&format!("{}{suffix}", t.title), font, TAB_MAX_W * s).width;
                (pad * 2.0 + icon + space + name + space + close).min(TAB_MAX_W * s).ceil()
            })
            .collect();
        let gaps = gap * tabs.len().saturating_sub(1) as f32;
        let widths = fit(&natural, (TAB_MIN_W * s).round(), (area.w - gaps).max(0.0));
        let total = widths.iter().sum::<f32>() + gaps;
        self.max_scroll = (total - area.w).max(0.0);

        let mut offsets = Vec::with_capacity(widths.len());
        let mut x = 0.0;
        for w in &widths {
            offsets.push(x);
            x += w + gap;
        }
        let index = active.and_then(|a| tabs.iter().position(|t| t.id == a));
        if let (Some(id), Some(i)) = (active, index) {
            let now = Reveal { id, area_w: area.w, tabs: tabs.len(), at: offsets[i], w: widths[i] };
            if self.revealed != Some(now) {
                let (lo, hi) = (offsets[i], offsets[i] + widths[i]);
                if lo < self.scroll {
                    self.scroll = lo;
                } else if hi > self.scroll + area.w {
                    self.scroll = hi - area.w;
                }
                self.revealed = Some(now);
            }
        }
        self.scroll = self.scroll.clamp(0.0, self.max_scroll).round();
        self.area = Some(area);
        self.pills.clear();

        let radius = theme::pill_radius(ui);
        cx.with_clip(area, |cx| {
            for (tab, (off, w)) in tabs.iter().zip(offsets.iter().zip(&widths)) {
                let pill = Rect { x: area.x + off - self.scroll, y: area.y + inset, w: *w, h: area.h - inset * 2.0 };
                self.pills.push((tab.id, pill));
                if pill.x + pill.w <= area.x || pill.x >= area.x + area.w {
                    continue;
                }
                let slot = square_in(Rect { x: pill.x + pill.w - pad * 0.5 - close, w: close, ..pill }, close);
                // Registered before anything else here: the slot wins its clicks.
                let slot_hover = cx.button(slot, button_id(tab.id));
                let hover = cx.hovered(pill) && cx.hovered(area);
                // Only the part in view: a pill scrolled past the row's edge
                // mustn't take the sidebar button's tooltip.
                if let (Some(path), Some(seen)) = (&tab.path, intersect(pill, area)) {
                    cx.tooltip(seen, path.clone());
                }
                if Some(tab.id) == active {
                    cx.round_rect(pill, radius, theme::TAB_ACTIVE_BG, theme::TAB_ACTIVE_ALPHA, None);
                } else if hover {
                    cx.round_rect(pill, radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
                }
                let icon_r = square_in(Rect { x: pill.x + pad, w: icon, ..pill }, icon);
                tab.kind.paint(cx, icon_r);
                let name_x = icon_r.x + icon + space;
                let name_clip = Rect { x: name_x, y: pill.y, w: (slot.x - space * 0.5 - name_x).max(0.0), h: pill.h };
                if let Some(clip) = intersect(name_clip, area) {
                    let top = pill.y + ((pill.h - ui.cell_h) * 0.5).round();
                    let font = Font::SANS.italic(tab.preview);
                    let spans = fitting_spans(cx, tab, font, name_clip.w);
                    let name = cx.shape_spans(&spans, font, name_clip.w.max(1.0));
                    cx.place(name, name_x, top, clip, theme::TEXT);
                }
                // Unsaved: a dot, until the pointer is over the tab and the
                // slot offers to close it.
                if tab.modified && !hover {
                    let d = (ui.cell_h * 0.45).round();
                    cx.round_rect(square_in(slot, d), d * 0.5, theme::TEXT, 1.0, None);
                    continue;
                }
                if slot_hover {
                    cx.round_rect(slot, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
                }
                let glyph = square_in(slot, (close * 0.7).round());
                Icon::Close.paint(cx, glyph, if slot_hover { theme::GLYPH_HOVER } else { theme::GLYPH });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_that_fit_keep_their_widths() {
        assert_eq!(fit(&[100.0, 150.5], 90.0, 400.0), vec![100.0, 150.0]);
    }

    #[test]
    fn the_widest_shrink_first_and_none_below_the_minimum() {
        // 100 + 200 + 300 into 450: the two widest share 350.
        assert_eq!(fit(&[100.0, 200.0, 300.0], 90.0, 450.0), vec![100.0, 175.0, 175.0]);
        // Into 240: everything at the minimum, past what fits.
        assert_eq!(fit(&[100.0, 200.0, 300.0], 90.0, 240.0), vec![90.0, 90.0, 90.0]);
        // A tab narrower than the minimum keeps its own width.
        assert_eq!(fit(&[60.0, 200.0, 300.0], 90.0, 200.0), vec![60.0, 90.0, 90.0]);
    }

    #[test]
    fn button_ids_name_the_tab() {
        let end = 1 << 24;
        assert_eq!(from_button(button_id(DocId(7)), end), Some(DocId(7)));
        assert_eq!(from_button(TAB_IDS - 1, end), None);
        assert_eq!(from_button(end, end), None);
    }

    #[test]
    fn only_shared_titles_get_the_shortest_path_that_tells_them_apart() {
        let titles = ["mod.rs", "a.md", "mod.rs", "x.rs", "x.rs", "r.txt", "r.txt"].map(String::from);
        let dirs = ["/p/files", "/p/docs", "/p/editor", "/p/a/src", "/p/b/src", "/", "/p"]
            .map(|d| Some(PathBuf::from(d)));
        let want = [Some("files"), None, Some("editor"), Some("a/src"), Some("b/src"), Some("/"), Some("/p")];
        assert_eq!(suffixes(&titles, &dirs), want.map(|w| w.map(String::from)));
    }
}
