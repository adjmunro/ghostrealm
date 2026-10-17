//! The sidebar's search mode: a query typed after `?` searches the files'
//! contents ([`ghostrealm_core::search`]) and the results replace the tree,
//! grouped by file. The filter box then shows the match-case, whole-word,
//! and regex toggles.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ghostrealm_core::search::{Case, SearchQuery, Searcher, WalkOptions};
use ghostrealm_core::Rect;
use ghostrealm_terminal::{Key, KeyPress};

use super::{file_name, move_highlight, FileBrowserView, Mode, RowGeom, NAME_FG};
use crate::plugin::icons::FileKind;
use crate::plugin::{theme, EventCx, Font, MouseEvent, PaintCx, Waker};

/// How long typing pauses before the search runs.
pub(super) const DEBOUNCE: Duration = Duration::from_millis(150);
/// The toggles' button ids (see [`super::View::button`]).
pub(super) const BTN_MATCH_CASE: u32 = 8;
pub(super) const BTN_WHOLE_WORD: u32 = 9;
pub(super) const BTN_REGEX: u32 = 10;
/// Chars kept before a match when a line is cut to show it.
const LEAD: usize = 6;

/// A result row: a file's header, one of its matching lines, or the note
/// that it has more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hit {
    File(usize),
    Line(usize, usize),
    More(usize),
}

/// What a search was started with: a change to any of it starts another.
type Started = (SearchQuery, PathBuf, WalkOptions);

/// Search mode's state, kept while the sidebar filters or browses so the
/// toggles stay as they were left.
pub(super) struct Search {
    searcher: Searcher,
    pub(super) match_case: bool,
    pub(super) whole_word: bool,
    pub(super) regex: bool,
    /// Why the query doesn't compile (an invalid regex).
    error: Option<String>,
    /// When typing paused long enough for the search to run.
    due: Option<Instant>,
    started: Option<Started>,
    /// The results as rows, rebuilt as they stream in (only appended to, so
    /// the highlight stays put).
    pub(super) rows: Vec<Hit>,
    /// The highlighted row, moved by keys and hover.
    pub(super) sel: Option<usize>,
    /// The row opened last: the selected look.
    chosen: Option<usize>,
    scroll: f32,
    /// The match the last open was for: its file, line, and columns, for the
    /// host to select once the file shows ([`FileBrowserView::take_match`]).
    pending: Option<(PathBuf, usize, Range<usize>)>,
}

impl Search {
    pub(super) fn new(waker: Option<Waker>) -> Self {
        Search {
            searcher: Searcher::new(waker),
            match_case: false,
            whole_word: false,
            regex: false,
            error: None,
            due: None,
            started: None,
            rows: Vec::new(),
            sel: None,
            chosen: None,
            scroll: 0.0,
            pending: None,
        }
    }

    pub(super) fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Drop the search and its results, keeping the toggles.
    fn stop(&mut self) {
        self.searcher.cancel();
        self.started = None;
        self.due = None;
        self.error = None;
        self.clear_rows();
    }

    fn clear_rows(&mut self) {
        self.rows.clear();
        self.sel = None;
        self.chosen = None;
        self.scroll = 0.0;
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        for (i, f) in self.searcher.results().files.iter().enumerate() {
            self.rows.push(Hit::File(i));
            self.rows.extend((0..f.lines.len()).map(|j| Hit::Line(i, j)));
            if f.more() {
                self.rows.push(Hit::More(i));
            }
        }
    }
}

impl FileBrowserView {
    /// Whether the sidebar is in search mode: its input starts with `?`.
    /// Pickers only filter.
    pub(super) fn searching(&self) -> bool {
        self.mode == Mode::Sidebar && self.input.text().starts_with('?')
    }

    /// The search as the input and the toggles say: the text after the `?`
    /// and any spaces after it.
    fn search_query(&self) -> SearchQuery {
        let s = &self.search;
        let text = self.input.text().strip_prefix('?').unwrap_or_default().trim_start();
        SearchQuery {
            text: text.to_string(),
            case: if s.match_case { Case::Sensitive } else { Case::Smart },
            whole_word: s.whole_word,
            regex: s.regex,
        }
    }

    fn walk_options(&self) -> WalkOptions {
        WalkOptions { show_hidden: self.tree.show_hidden(), show_gitignored: self.tree.show_gitignored() }
    }

    /// After the input changed: search (once typing pauses) or filter.
    pub(super) fn input_changed(&mut self) {
        if self.searching() {
            self.tree.set_query("");
            self.search.due = Some(Instant::now() + DEBOUNCE);
        } else {
            self.tree.set_query(self.input.text());
            self.search.stop();
        }
        self.clear_highlight();
    }

    /// Empty the input: back to the tree.
    pub(super) fn clear_input(&mut self) {
        self.input.set_text("");
        self.tree.set_query("");
        self.search.stop();
        self.clear_highlight();
    }

    /// Start the search the input and toggles describe, unless it is the one
    /// running; an empty query clears the results.
    pub(super) fn run_search(&mut self) {
        self.search.due = None;
        if !self.searching() {
            return;
        }
        let started = (self.search_query(), self.tree.root().to_path_buf(), self.walk_options());
        if self.search.started.as_ref() == Some(&started) {
            return;
        }
        let s = &mut self.search;
        s.clear_rows();
        s.error = None;
        let (query, root, walk) = &started;
        if query.text.is_empty() {
            s.searcher.cancel();
        } else if let Err(e) = s.searcher.start(root, query, *walk) {
            s.error = Some(e);
        }
        s.started = Some(started);
    }

    /// Search again once typing would have paused: the files changed.
    pub(super) fn rerun_search(&mut self) {
        if self.searching() {
            self.search.started = None;
            self.search.due = Some(Instant::now() + DEBOUNCE);
        }
    }

    /// Take in streamed results. Returns whether they changed.
    pub(super) fn poll_search(&mut self) -> bool {
        let changed = self.search.searcher.poll();
        if changed {
            self.search.rebuild_rows();
        }
        changed
    }

    pub(super) fn toggle_search_option(&mut self, id: u32) {
        let s = &mut self.search;
        match id {
            BTN_MATCH_CASE => s.match_case = !s.match_case,
            BTN_WHOLE_WORD => s.whole_word = !s.whole_word,
            BTN_REGEX => s.regex = !s.regex,
            _ => return,
        }
        self.run_search();
    }

    /// The match the last open from the results was for, if it opened a
    /// matching line: its file, line (from 0), and columns (chars). Spent.
    pub fn take_match(&mut self) -> Option<(PathBuf, usize, Range<usize>)> {
        self.search.pending.take()
    }

    /// Open result row `idx`: a matching line at its first match, a file
    /// header or "more" note at its file.
    fn open_hit(&mut self, cx: &mut EventCx, idx: usize) {
        let files = &self.search.searcher.results().files;
        let (path, at) = match self.search.rows.get(idx) {
            Some(Hit::File(i) | Hit::More(i)) => (files[*i].path.clone(), None),
            Some(Hit::Line(i, j)) => {
                let hit = &files[*i].lines[*j];
                let cols = hit.ranges.first().map_or(0..0, |r| hit.start + r.start..hit.start + r.end);
                (files[*i].path.clone(), Some((hit.line, cols)))
            }
            None => return,
        };
        self.search.chosen = Some(idx);
        self.search.pending = at.map(|(line, cols)| (path.clone(), line, cols));
        self.open_file(cx, path);
    }

    /// Search mode's keys: Up/Down move the highlight through the results,
    /// Enter opens it (none: the first match). `None` leaves the key to the
    /// input.
    pub(super) fn search_key(&mut self, cx: &mut EventCx, k: &KeyPress) -> Option<bool> {
        let m = k.mods;
        match k.key {
            Key::Up | Key::Down if !m.super_ => {
                let delta = if k.key == Key::Up { -1 } else { 1 };
                move_highlight(&mut self.search.sel, self.search.rows.len(), delta);
                self.ensure_hit_visible();
            }
            Key::Enter if !(m.super_ || m.alt || m.ctrl) => {
                let n = self.search.rows.len();
                if n == 0 {
                    return Some(false);
                }
                let idx = self.search.sel.unwrap_or(1.min(n - 1));
                self.open_hit(cx, idx);
            }
            _ => return None,
        }
        cx.redraw();
        Some(true)
    }

    /// Search mode's mouse: a press opens the row under it (a double click
    /// keeps its tab, as the host tells), and hover moves the highlight.
    /// Returns whether the event was search mode's.
    pub(super) fn search_mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) -> bool {
        match *event {
            MouseEvent::Down { pos: (x, y), .. } => {
                if let Some(idx) = self.hit_at(x, y) {
                    self.search.sel = Some(idx);
                    self.open_hit(cx, idx);
                    cx.redraw();
                }
            }
            MouseEvent::Move { pos: (x, y) } => {
                if let Some(idx) = self.hit_at(x, y).filter(|i| self.search.sel != Some(*i)) {
                    self.search.sel = Some(idx);
                    cx.redraw();
                }
            }
            _ => {}
        }
        true
    }

    pub(super) fn search_scroll(&mut self, cx: &mut EventCx, dy: f32) {
        let next = (self.search.scroll - dy).max(0.0);
        if next != self.search.scroll {
            self.search.scroll = next;
            cx.request_frame();
        }
    }

    /// The result row under a point, from the last paint.
    fn hit_at(&self, x: f32, y: f32) -> Option<usize> {
        if !crate::plugin::rect_contains(self.list, x, y) {
            return None;
        }
        let idx = ((y - self.list.y + self.search.scroll) / self.row_h).floor().max(0.0) as usize;
        (idx < self.search.rows.len()).then_some(idx)
    }

    /// The file of the result row under a point.
    pub(super) fn hit_path_at(&self, x: f32, y: f32) -> Option<PathBuf> {
        let (Hit::File(i) | Hit::Line(i, _) | Hit::More(i)) = self.search.rows[self.hit_at(x, y)?];
        Some(self.search.searcher.results().files[i].path.clone())
    }

    fn ensure_hit_visible(&mut self) {
        let Some(sel) = self.search.sel else { return };
        let (top, s) = (sel as f32 * self.row_h, &mut self.search);
        if top < s.scroll {
            s.scroll = top;
        } else if top + self.row_h > s.scroll + self.list.h {
            s.scroll = (top + self.row_h - self.list.h).max(0.0);
        }
    }

    /// The search's root, for showing where a file is.
    fn search_root(&self) -> &Path {
        self.search.started.as_ref().map_or(self.tree.root(), |(_, root, _)| root)
    }

    /// The results in `list`: each file's header (icon, name, dim folder,
    /// match count), its matching lines (number, text cut around the first
    /// match, matches lit), and a note when it has more; then a status line.
    pub(super) fn paint_search(&mut self, cx: &mut PaintCx, list: Rect) {
        let g = RowGeom::new(cx.ui, list);
        let (row_h, cw) = (g.row_h, cx.ui.cell_w);
        let status = self.search_status();
        let n = self.search.rows.len() + usize::from(status.is_some());
        let max_scroll = (n as f32 * row_h - list.h).max(0.0);
        self.search.scroll = self.search.scroll.clamp(0.0, max_scroll);
        let first = (self.search.scroll / row_h).floor() as usize;
        let last = (((self.search.scroll + list.h) / row_h).ceil() as usize).min(self.search.rows.len());
        let top = |i: usize| list.y + i as f32 * row_h - self.search.scroll;
        let root = self.search_root().to_path_buf();
        let files = &self.search.searcher.results().files;
        let text_clip = Rect { w: (list.w - g.pad).max(1.0), ..list };
        let right = list.x + list.w - g.pad;
        cx.with_clip(list, |cx| {
            for idx in first..last.max(first) {
                let ry = top(idx);
                let pill = if self.search.chosen == Some(idx) {
                    Some((theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA))
                } else if self.search.sel == Some(idx) {
                    Some((theme::HOVER_BG, theme::HOVER_ALPHA))
                } else {
                    None
                };
                if let Some((color, alpha)) = pill {
                    cx.round_rect(g.pill(ry), theme::pill_radius(cx.ui), color, alpha, None);
                }
                let ty = g.text_y(cx.ui, ry);
                match self.search.rows[idx] {
                    Hit::File(i) => {
                        let f = &files[i];
                        FileKind::of(&f.path).paint(cx, g.icon(0, ry));
                        let count = f.total.to_string();
                        let shaped = cx.shape(&count, Font::SANS, list.w);
                        let count_x = (right - shaped.width).round();
                        cx.place(shaped, count_x, ty, list, theme::TEXT_DIM);
                        let mut spans = vec![(file_name(&f.path), NAME_FG)];
                        let dir = f.path.parent().and_then(|p| p.strip_prefix(&root).ok());
                        if let Some(dir) = dir.filter(|d| !d.as_os_str().is_empty()) {
                            spans.push((format!(" {}", dir.display()), theme::DIM));
                        }
                        let clip = Rect { w: (count_x - g.gap - list.x).max(1.0), ..list };
                        cx.row_font(&spans, Font::SANS, g.text_x(0), ty, list.w, clip, NAME_FG);
                    }
                    Hit::Line(i, j) => {
                        let f = &files[i];
                        let digits = f.lines.iter().map(|l| (l.line + 1).to_string().len()).max().unwrap_or(1);
                        let num = (f.lines[j].line + 1).to_string();
                        let num_x = g.text_x(0);
                        let nx = num_x + (digits - num.len()) as f32 * cw;
                        cx.label(&num, Font::MONO, nx, ty, text_clip, theme::DIM);
                        let tx = (num_x + (digits + 1) as f32 * cw).round();
                        let cols = ((text_clip.x + text_clip.w - tx) / cw).floor().max(0.0) as usize;
                        let (text, ranges) = fit_line(&f.lines[j].text, f.lines[j].start > 0, &f.lines[j].ranges, cols);
                        for r in ranges {
                            let pill = Rect {
                                x: (tx + r.start as f32 * cw).round(),
                                y: ty.round(),
                                w: ((r.end - r.start) as f32 * cw).round(),
                                h: cx.ui.cell_h.round(),
                            };
                            cx.round_rect(pill, theme::box_radius(cx.ui), theme::MATCH_BG, theme::MATCH_ALPHA, None);
                        }
                        cx.row(&[(text, theme::TEXT)], tx, ty, list.w, text_clip, theme::TEXT);
                    }
                    Hit::More(_) => {
                        cx.label("More matches in this file.", Font::SANS, g.text_x(0), ty, text_clip, theme::DIM);
                    }
                }
            }
        });
        if let Some((note, color)) = status {
            let ry = top(self.search.rows.len());
            let note = cx.cut_to_fit(&note, Font::SANS, (text_clip.x + text_clip.w - g.text_x(0)).max(1.0));
            cx.label(&note, Font::SANS, g.text_x(0), g.text_y(cx.ui, ry), list, color);
        }
    }

    /// The line under the results: an invalid regex, the search running, no
    /// results, or a cap reached.
    fn search_status(&self) -> Option<(String, [u8; 3])> {
        let s = &self.search;
        let r = s.searcher.results();
        if let Some(e) = &s.error {
            return Some((format!("Invalid regular expression: {e}"), theme::WARN));
        }
        if self.search_query().text.is_empty() {
            return Some(("Type to search in files".to_string(), theme::DIM));
        }
        if s.started.is_none() || s.due.is_some() || !r.done {
            return Some(("Searching\u{2026}".to_string(), theme::DIM));
        }
        if r.files.is_empty() {
            return Some(("No results".to_string(), theme::DIM));
        }
        r.capped.then(|| (format!("Showing the first {} matches in {} files", r.matches, r.files.len()), theme::DIM))
    }

    /// The results as text (the agent channel): the query, then each file
    /// (relative path and match count) and its lines.
    pub(super) fn search_text(&self) -> String {
        let mut out = format!("{}\n", self.input.text());
        let root = self.search_root();
        for f in &self.search.searcher.results().files {
            let rel = f.path.strip_prefix(root).unwrap_or(&f.path);
            out.push_str(&format!("{} ({})\n", rel.display(), f.total));
            for l in &f.lines {
                out.push_str(&format!("  {}: {}\n", l.line + 1, l.text.trim()));
            }
            if f.more() {
                out.push_str("  \u{2026}\n");
            }
        }
        if let Some((note, _)) = self.search_status() {
            out.push_str(&note);
            out.push('\n');
        }
        out
    }

    /// The header's toggles, right to left from `right` in `field`'s row:
    /// regex, whole word, match case. Returns their left edge.
    pub(super) fn paint_toggles(&self, cx: &mut PaintCx, field: Rect, right: f32) -> f32 {
        let w = (cx.ui.cell_w * 3.0).round();
        let s = &self.search;
        let mut x = right;
        for (id, label, tip, on, underline) in [
            (BTN_REGEX, ".*", "Use regular expression", s.regex, false),
            (BTN_WHOLE_WORD, "ab", "Match whole word", s.whole_word, true),
            (BTN_MATCH_CASE, "Aa", "Match case", s.match_case, false),
        ] {
            x -= w;
            toggle_button(cx, Rect { x, w, ..field }, id, label, tip, on, underline);
        }
        x
    }
}

/// A text toggle filling `r`: outlined and filled while on, the hover
/// highlight under the pointer, and a tooltip naming it.
fn toggle_button(cx: &mut PaintCx, r: Rect, id: u32, label: &str, tip: &str, on: bool, underline: bool) {
    let hover = cx.button(r, id);
    let inset = (3.0 * cx.ui.scale).round();
    let hl = Rect { x: r.x + inset * 0.5, y: r.y + inset, w: r.w - inset, h: r.h - inset * 2.0 };
    let radius = theme::box_radius(cx.ui);
    if on {
        let border = (theme::hairline(cx.ui), theme::FIELD_FOCUS_BORDER, 1.0);
        cx.round_rect(hl, radius, cx.chrome.accent, theme::TAB_ACTIVE_ALPHA, Some(border));
    }
    if hover {
        cx.round_rect(hl, radius, theme::HOVER_BG, theme::HOVER_ALPHA, None);
    }
    cx.tooltip(r, tip);
    let color = match (hover, on) {
        (true, _) => theme::GLYPH_HOVER,
        (false, true) => theme::TEXT,
        (false, false) => theme::GLYPH,
    };
    let shaped = cx.shape(label, Font::SANS, r.w);
    let x = (r.x + (r.w - shaped.width) * 0.5).round();
    let y = (r.y + (r.h - cx.ui.cell_h) * 0.5).round();
    cx.place(shaped, x, y, r, color);
    if underline {
        let line = theme::hairline(cx.ui);
        cx.fill(Rect { x, y: (y + cx.ui.cell_h * 0.92).round(), w: shaped.width.round(), h: line }, color);
    }
}

/// A matching line as it fits `cols` columns, and its match ranges there
/// (chars): leading indentation dropped (unless `cut`, the text starting
/// mid-line), tabs as one space, and, when too long, cut to show its first
/// match with a few chars before it, an ellipsis marking each cut end.
pub(super) fn fit_line(text: &str, cut: bool, ranges: &[Range<usize>], cols: usize) -> (String, Vec<Range<usize>>) {
    let chars: Vec<char> = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let first = ranges.first().cloned().unwrap_or(0..0);
    let lead = match cut {
        true => 0,
        false => chars.iter().take_while(|c| c.is_whitespace()).count().min(first.start),
    };
    let body = &chars[lead..];
    let first = first.start - lead..first.end - lead;
    let cols = cols.max(3);
    let mut start = 0;
    let mut cut_left = cut;
    // Cut the front when the first match would end past the room left
    // after an ellipsis at each end.
    if body.len() + usize::from(cut_left) > cols && first.end + usize::from(cut_left) + 1 > cols {
        start = first.start.saturating_sub(LEAD);
        cut_left |= start > 0;
    }
    let room = cols - usize::from(cut_left);
    let cut_right = body.len() - start > room;
    let end = if cut_right { start + room - 1 } else { body.len() };
    let mut out = String::new();
    if cut_left {
        out.push('\u{2026}');
    }
    out.extend(&body[start..end]);
    if cut_right {
        out.push('\u{2026}');
    }
    let shift = usize::from(cut_left);
    let ranges = ranges
        .iter()
        .map(|r| r.start - lead.min(r.start)..r.end - lead.min(r.end))
        .filter(|r| r.end > start && r.start < end)
        .map(|r| r.start.max(start) - start + shift..r.end.min(end) - start + shift)
        .collect();
    (out, ranges)
}

#[cfg(test)]
// A one-range list is a line with one match, not a misspelt range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use super::super::{header_height, row_height};
    use crate::plugin::testing::{press, tmpdir, Harness, UI};
    use crate::plugin::{MouseButton, Request, View};

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };

    /// A folder with two matching files (one nested) and one that doesn't
    /// match, and a sidebar on it.
    fn fixture() -> (PathBuf, FileBrowserView) {
        let d = tmpdir("fb-search");
        std::fs::write(d.join("a.txt"), "one Hit\n  two hit hit\nnone\n").unwrap();
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.md"), "hitch\n").unwrap();
        std::fs::write(d.join("zzz.txt"), "nothing\n").unwrap();
        (d.clone(), FileBrowserView::new(d))
    }

    fn type_in(h: &Harness, v: &mut FileBrowserView, text: &str) {
        for c in text.chars() {
            h.key(v, press(Key::Char(c)));
        }
    }

    /// Run the debounced search and take in its results.
    fn settle(h: &Harness, v: &mut FileBrowserView) {
        h.tick(v, Instant::now() + DEBOUNCE * 2);
        let until = Instant::now() + Duration::from_secs(10);
        while !v.search.searcher.results().done && v.search.error.is_none() {
            assert!(Instant::now() < until, "the search finishes");
            v.refresh();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn ids(h: &Harness) -> Vec<u32> {
        h.frame.view_buttons.iter().map(|b| b.2).collect()
    }

    /// The centre of result row `i`.
    fn row_centre(i: usize) -> (f32, f32) {
        let row_h = row_height(UI);
        (60.0, header_height(UI) + i as f32 * row_h + row_h * 0.5)
    }

    #[test]
    fn a_question_mark_shows_the_toggles_and_the_results_replace_the_tree() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(!ids(&h).contains(&BTN_MATCH_CASE), "filtering: no toggles");
        type_in(&h, &mut v, "?");
        assert!(v.searching() && v.tree().query().is_empty(), "the tree isn't filtered by it");
        h.paint(&mut v, RECT);
        for id in [BTN_MATCH_CASE, BTN_WHOLE_WORD, BTN_REGEX] {
            assert!(ids(&h).contains(&id), "toggle {id}");
        }
        let texts = h.texts();
        for want in ["Aa", "ab", ".*", "Type to search in files"] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
        assert!(!texts.iter().any(|t| t == "zzz.txt"), "the tree is gone");
        type_in(&h, &mut v, " hit");
        assert_eq!(v.search_query().text, "hit");
        h.paint(&mut v, RECT);
        assert!(h.texts().iter().any(|t| t == "Searching\u{2026}"), "until typing pauses");
        assert!(v.deadline(&h.cfg).is_some(), "debounced by a timer");
        settle(&h, &mut v);
        h.paint(&mut v, RECT);
        let texts = h.texts();
        for want in ["a.txt", "one Hit", "two hit hit", "b.md sub", "hitch", "3", "1"] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
        assert!(!texts.iter().any(|t| t.contains("zzz") || t == "none"), "{texts:?}");
        assert_eq!(v.search.rows, [Hit::File(0), Hit::Line(0, 0), Hit::Line(0, 1), Hit::File(1), Hit::Line(1, 0)]);
        // Escape goes back to the tree.
        h.key(&mut v, press(Key::Escape));
        assert!(!v.searching());
        h.paint(&mut v, RECT);
        assert!(h.texts().iter().any(|t| t == "zzz.txt"));
    }

    #[test]
    fn the_toggles_change_the_query_and_search_again() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        type_in(&h, &mut v, "?hit");
        settle(&h, &mut v);
        assert_eq!(v.search.searcher.results().matches, 4);
        h.button(&mut v, BTN_MATCH_CASE);
        assert_eq!(v.search_query().case, Case::Sensitive);
        settle(&h, &mut v);
        assert_eq!(v.search.searcher.results().matches, 3, "not Hit");
        h.button(&mut v, BTN_WHOLE_WORD);
        assert!(v.search_query().whole_word);
        settle(&h, &mut v);
        assert_eq!(v.search.searcher.results().matches, 2, "not hitch");
        // Shown on, with a tooltip naming it.
        let accent = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, h.chrome.accent, theme::TAB_ACTIVE_ALPHA).color;
        let frame = h.paint(&mut v, RECT);
        assert_eq!(frame.bg.iter().filter(|q| q.color == accent && q.radius > 0.0).count(), 2);
        let ww = h.frame.view_buttons.iter().find(|b| b.2 == BTN_WHOLE_WORD).unwrap().0;
        h.cursor = (ww.x + 2.0, ww.y + 2.0);
        let tip = h.paint(&mut v, RECT).tooltip.clone().expect("a tooltip").1;
        assert_eq!(tip, "Match whole word");
    }

    #[test]
    fn an_invalid_regex_says_so() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.button(&mut v, BTN_REGEX);
        type_in(&h, &mut v, "?a(");
        settle(&h, &mut v);
        h.paint(&mut v, RECT);
        let texts = h.texts();
        assert!(texts.iter().any(|t| t.starts_with("Invalid regular expression")), "{texts:?}");
        h.key(&mut v, press(Key::Backspace));
        type_in(&h, &mut v, "|hitc");
        settle(&h, &mut v);
        assert!(v.search.error.is_none());
        assert_eq!(v.search.searcher.results().files.len(), 1);
    }

    #[test]
    fn matches_are_lit_where_their_columns_are() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        type_in(&h, &mut v, "?hit");
        settle(&h, &mut v);
        let frame = h.paint(&mut v, RECT);
        let want = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, theme::MATCH_BG, theme::MATCH_ALPHA).color;
        let lit: Vec<(f32, f32, f32)> = frame
            .bg
            .iter()
            .filter(|q| q.color == want)
            .map(|q| {
                let x = (q.pos[0] + 1.0) * 0.5 * frame.screen.0;
                let y = (1.0 - q.pos[1]) * 0.5 * frame.screen.1;
                (x.round(), y.round(), (q.size[0] * 0.5 * frame.screen.0).round())
            })
            .collect();
        assert_eq!(lit.len(), 4, "{lit:?}");
        // "one Hit": the match at column 4 of row 1, three columns wide.
        let g = RowGeom::new(UI, Rect { y: header_height(UI), ..RECT });
        let tx = (g.text_x(0) + 2.0 * UI.cell_w).round();
        let (x, y, w) = lit[0];
        assert_eq!((x, w), (tx + 4.0 * UI.cell_w, 3.0 * UI.cell_w));
        assert!(y > g.list.y + g.row_h && y < g.list.y + 2.0 * g.row_h);
        // "  two hit hit" loses its indent: matches at 4 and 8.
        assert_eq!((lit[1].0, lit[2].0), (tx + 4.0 * UI.cell_w, tx + 8.0 * UI.cell_w));
    }

    #[test]
    fn a_click_or_enter_opens_the_match() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        type_in(&h, &mut v, "?hit");
        settle(&h, &mut v);
        h.paint(&mut v, RECT);
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(2), clicks: 1, button: MouseButton::Left });
        assert_eq!(out.requests, [Request::OpenFile(d.join("a.txt"))]);
        assert_eq!(v.take_match(), Some((d.join("a.txt"), 1, 6..9)));
        assert_eq!(v.chosen(), Some(d.join("a.txt").as_path()));
        // A file's header opens it with no match to select.
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(3), clicks: 1, button: MouseButton::Left });
        assert_eq!(out.requests, [Request::OpenFile(d.join("sub").join("b.md"))]);
        assert_eq!(v.take_match(), None);
        // Down moves through the rows; Enter opens the highlighted one.
        h.key(&mut v, press(Key::Down));
        let (used, out) = h.key(&mut v, press(Key::Enter));
        assert!(used);
        assert_eq!(out.requests, [Request::OpenFile(d.join("sub").join("b.md"))]);
        assert_eq!(v.take_match(), Some((d.join("sub").join("b.md"), 0, 0..3)));
        // The agent reads the results.
        let text = v.text().unwrap();
        assert!(text.starts_with("?hit\na.txt (3)\n  1: one Hit\n"), "{text}");
    }

    #[test]
    fn a_path_still_completes_and_pickers_only_filter() {
        let (d, mut v) = fixture();
        let h = Harness::new();
        type_in(&h, &mut v, "./s");
        assert!(!v.searching() && v.tree().input_is_path());
        let mut picker = FileBrowserView::picker(d, Mode::PickFile);
        type_in(&h, &mut picker, "?a");
        assert!(!picker.searching());
        assert_eq!(picker.tree().query(), "?a");
    }

    #[test]
    fn a_line_drops_its_indent_and_fits_whole_when_it_can() {
        let (text, ranges) = fit_line("    let x = 1;", false, &[8..9], 40);
        assert_eq!((text.as_str(), ranges), ("let x = 1;", vec![4..5]));
    }

    #[test]
    fn a_long_line_ends_in_an_ellipsis_or_is_cut_before_its_match() {
        let line = "the quick brown fox jumps over the lazy dog";
        let (text, ranges) = fit_line(line, false, &[4..9], 12);
        assert_eq!((text.as_str(), ranges), ("the quick b\u{2026}", vec![4..9]));
        let (text, ranges) = fit_line(line, false, &[35..39, 40..43], 12);
        assert_eq!(text, "\u{2026}r the lazy\u{2026}");
        assert_eq!(ranges, vec![7..11]);
        // A line the search cut starts with an ellipsis.
        let (text, ranges) = fit_line("abc", true, &[0..1], 12);
        assert_eq!((text.as_str(), ranges), ("\u{2026}abc", vec![1..2]));
    }
}
