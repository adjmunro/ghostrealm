//! The file browser plugin: a navigable, filterable tree over
//! [`FsTree`](ghostrealm_core::fs_tree::FsTree). The same view backs the
//! floating picker, which chooses a directory or a file (see [`Mode`]).
//!
//! Typing filters (fuzzy across the tree) or, when the input looks like a path,
//! completes it; Up/Down move the single highlight, which the mouse also takes
//! over on hover. A click also *selects* the row: a separate, sticky highlight
//! that hover and arrows leave alone. In the picker, the selected entry is the
//! one confirming chooses.

use std::any::Any;
use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::Result;
use ghostrealm_core::fs_tree::{FileRow, FsTree, FsView, GitFilter};
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Rect, Registry};
use ghostrealm_terminal::{Key, KeyPress};
use glyphon::Family;

use crate::app_state::AppState;
use crate::plugin::{
    rect_contains, theme, widgets, EventCx, MouseEvent, OpenCx, PaintCx, Plugin, Request, View,
};

pub const ID: &str = "file_browser";

/// Row colours: a file, a hidden (dot) file, a file's extension, and the
/// fuzzy-match highlight. Directories use the theme accent.
const FILE_FG: [u8; 3] = [230, 230, 235];
const HIDDEN_FG: [u8; 3] = [130, 130, 140];
const EXT_FG: [u8; 3] = [212, 170, 90];
const MATCH_FG: [u8; 3] = [245, 225, 90];
/// Opacity of the accent fill behind the selected (clicked) row.
const SELECTED_ALPHA: f32 = 0.3;

/// Header button ids (see [`View::button`]).
const BTN_PARENT: u32 = 0;
const BTN_HIDDEN: u32 = 1;
const BTN_IGNORED: u32 = 2;
const BTN_GIT: u32 = 3;
const BTN_VIEW: u32 = 4;

pub struct FileBrowserPlugin;

impl Plugin for FileBrowserPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "File browser"
    }

    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        let root = cx
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Box::new(FileBrowserView::new(root)))
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        r.register(
            CommandMeta::new(
                "file_browser.new",
                "New File Browser",
                "Open a file browser in the focused pane",
            ),
            Box::new(|s: &mut AppState, _| {
                s.open_plugin_in_focused(ID)
                    .map_err(|e| CmdError::Failed(format!("{e:#}")))?;
                Ok(CmdOutcome::ok())
            }),
        );
    }
}

/// What the browser is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The file browser pane: opening a file requests it.
    Browse,
    /// Choosing a directory: only directories are listed, and files never open.
    PickDir,
    /// Choosing a file: a click selects it; a double-click or Enter picks it.
    PickFile,
}

/// A row's click target: a real entry (path) or a synthetic category (key).
#[derive(Clone)]
struct RowRef {
    path: PathBuf,
    is_dir: bool,
    is_category: bool,
    key: String,
}

pub struct FileBrowserView {
    tree: FsTree,
    mode: Mode,
    /// Vertical scroll of the row list (physical px).
    scroll: f32,
    /// The highlighted row (browse/filter), shared by keyboard and hover.
    sel: Option<usize>,
    /// The highlighted completion (path mode).
    completion: Option<usize>,
    /// The clicked (selected) entry; cleared once it is no longer listed.
    chosen: Option<PathBuf>,
    /// The entry the previous click landed on, for double-click.
    last_click: Option<PathBuf>,
    // Geometry from the last paint, for mapping input to rows.
    list: Rect,
    row_h: f32,
    rows: Vec<RowRef>,
    /// Path-mode completions as drawn: (rect, index, path, is_dir).
    suggest_hits: Vec<(Rect, usize, PathBuf, bool)>,
}

impl FileBrowserView {
    pub fn new(root: PathBuf) -> Self {
        FileBrowserView {
            tree: FsTree::new(root),
            mode: Mode::Browse,
            scroll: 0.0,
            sel: None,
            completion: None,
            chosen: None,
            last_click: None,
            list: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            row_h: 1.0,
            rows: Vec::new(),
            suggest_hits: Vec::new(),
        }
    }

    /// A browser for the floating picker, choosing per `mode`.
    pub fn picker(root: PathBuf, mode: Mode) -> Self {
        let mut v = FileBrowserView {
            mode,
            ..Self::new(root)
        };
        v.tree.set_dirs_only(mode == Mode::PickDir);
        v
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn tree(&self) -> &FsTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut FsTree {
        &mut self.tree
    }

    /// What a picker confirms: the selected directory, else the one being
    /// shown ([`Mode::PickDir`]); the selected file ([`Mode::PickFile`]).
    pub fn choice(&self) -> Option<PathBuf> {
        match self.mode {
            Mode::Browse => None,
            Mode::PickDir => Some(
                self.chosen
                    .clone()
                    .filter(|p| p.is_dir())
                    .unwrap_or_else(|| self.tree.root().to_path_buf()),
            ),
            Mode::PickFile => self.chosen.clone().filter(|p| !p.is_dir()),
        }
    }

    /// Open (browse) or pick (file picker) `path`.
    fn open_file(&self, cx: &mut EventCx, path: PathBuf) {
        match self.mode {
            Mode::Browse => cx.request(Request::OpenFile(path)),
            Mode::PickFile => cx.request(Request::Pick(path)),
            Mode::PickDir => {}
        }
    }

    fn clear_highlight(&mut self) {
        self.sel = None;
        self.completion = None;
    }

    /// Scroll so the highlighted row is visible (last paint's list geometry).
    fn ensure_sel_visible(&mut self) {
        let Some(sel) = self.sel else { return };
        let row_top = sel as f32 * self.row_h;
        if row_top < self.scroll {
            self.scroll = row_top;
        } else if row_top + self.row_h > self.scroll + self.list.h {
            self.scroll = (row_top + self.row_h - self.list.h).max(0.0);
        }
    }

    fn selected_row(&mut self) -> Option<FileRow> {
        let idx = self.sel?;
        self.tree.rows().get(idx).cloned()
    }

    /// Put the highlighted completion (or the first) into the input, so the next
    /// navigate/keystroke continues from it.
    fn accept_completion(&mut self) {
        let comps = self.tree.completions();
        if comps.is_empty() {
            return;
        }
        let idx = self.completion.unwrap_or(0).min(comps.len() - 1);
        self.tree.set_query(comps[idx].clone());
    }

    /// Browse-mode spans for a row name: kind colour (directory accent, hidden
    /// dim, file normal) with a file's extension in gold.
    fn name_spans(name: &str, is_dir: bool, accent: [u8; 3]) -> Vec<(String, [u8; 3])> {
        let hidden = name.starts_with('.');
        let base = if is_dir {
            accent
        } else if hidden {
            HIDDEN_FG
        } else {
            FILE_FG
        };
        if is_dir || hidden {
            return vec![(name.to_string(), base)];
        }
        match name.rfind('.').filter(|&i| i > 0) {
            Some(i) => vec![(name[..i].to_string(), base), (name[i..].to_string(), EXT_FG)],
            None => vec![(name.to_string(), base)],
        }
    }

    /// Filter-mode spans: matched characters (char indices) highlighted, the rest
    /// in the kind colour.
    fn match_spans(name: &str, is_dir: bool, matches: &[usize], accent: [u8; 3]) -> Vec<(String, [u8; 3])> {
        let base = if is_dir { accent } else { FILE_FG };
        if matches.is_empty() {
            return vec![(name.to_string(), base)];
        }
        let hit: HashSet<usize> = matches.iter().copied().collect();
        let mut spans: Vec<(String, [u8; 3])> = Vec::new();
        let mut cur = String::new();
        let mut cur_hit = false;
        for (i, ch) in name.chars().enumerate() {
            let h = hit.contains(&i);
            if !cur.is_empty() && h != cur_hit {
                spans.push((std::mem::take(&mut cur), if cur_hit { MATCH_FG } else { base }));
            }
            cur_hit = h;
            cur.push(ch);
        }
        if !cur.is_empty() {
            spans.push((cur, if cur_hit { MATCH_FG } else { base }));
        }
        spans
    }

    /// The header: parent button, input field, and the filter/view toggles.
    /// Returns the header height.
    fn paint_header(&mut self, cx: &mut PaintCx, rect: Rect, value: &str) -> f32 {
        let scale = cx.ui.scale;
        let ch = cx.ui.cell_h;
        let pad = 8.0 * scale;
        let header_h = ch + 10.0 * scale;
        let header = Rect { x: rect.x, y: rect.y, w: rect.w, h: header_h };
        cx.fill(header, cx.chrome.sidebar);
        let hy = rect.y + (header_h - ch) * 0.5;

        // Parent (..) on the left.
        let par_w = cx.ui.cell_w * 3.0;
        let par_hit = Rect { x: rect.x, y: rect.y, w: par_w + pad, h: header_h };
        let hov = cx.button(par_hit, BTN_PARENT);
        if hov {
            cx.highlight(par_hit);
        }
        let color = if hov { theme::LABEL_HOVER } else { theme::LABEL };
        cx.label("..", Family::Monospace, rect.x + pad, hy, par_hit, color);

        // Toggles on the right, right to left.
        let t = &self.tree;
        let toggles = [
            ("hidden", t.show_hidden(), BTN_HIDDEN),
            (".gitignore", t.show_gitignored(), BTN_IGNORED),
            (t.git_filter().label(), t.git_filter() != GitFilter::All, BTN_GIT),
            (t.view().label(), t.view() != FsView::Tree, BTN_VIEW),
        ];
        // The git filter and grouped views act on files, which a directory
        // picker never lists.
        let file_only = |id: u32| id == BTN_GIT || id == BTN_VIEW;
        let dirs_only = self.mode == Mode::PickDir;
        let mut right = rect.x + rect.w - pad;
        for (label, on, id) in toggles.into_iter().filter(|t| !(dirs_only && file_only(t.2))) {
            let w = label.chars().count() as f32 * cx.ui.cell_w;
            let hit = Rect { x: right - w - pad, y: rect.y, w: w + pad * 2.0, h: header_h };
            if cx.button(hit, id) {
                cx.highlight(hit);
            }
            let color = if on { theme::LABEL_HOVER } else { theme::DIM };
            cx.label(label, Family::SansSerif, right - w - pad * 0.5, hy, hit, color);
            right -= w + pad * 2.0;
        }

        // The input field fills the space between.
        let box_x = rect.x + par_w + pad * 2.0;
        let inset = 4.0 * scale;
        let field = Rect {
            x: box_x,
            y: rect.y + inset,
            w: (right - box_x - pad).max(1.0),
            h: header_h - inset * 2.0,
        };
        let root = self.tree.root().display().to_string();
        widgets::text_field(cx, field, value, &root);
        header_h
    }
}

impl View for FileBrowserView {
    fn plugin(&self) -> &'static str {
        ID
    }

    fn title(&self) -> Option<String> {
        Some("Files".to_string())
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        cx.fill(rect, cx.chrome.background);
        let scale = cx.ui.scale;
        let ch = cx.ui.cell_h;
        let pad = 8.0 * scale;
        let accent = cx.chrome.accent;

        let query = self.tree.query().to_string();
        let is_path = !query.is_empty() && self.tree.input_is_path();
        let suggestions: Vec<PathBuf> = if is_path {
            self.tree.suggestions(&query)
        } else {
            Vec::new()
        };
        // The field shows the highlighted completion, else what was typed.
        let preview = self
            .completion
            .filter(|_| is_path)
            .and_then(|i| suggestions.get(i))
            .map(|p| {
                let mut s = p.to_string_lossy().into_owned();
                if p.is_dir() {
                    s.push('/');
                }
                s
            });
        let header_h = self.paint_header(cx, rect, preview.as_deref().unwrap_or(&query));

        let list = Rect {
            x: rect.x,
            y: rect.y + header_h,
            w: rect.w,
            h: (rect.h - header_h).max(0.0),
        };
        let row_h = ch + 4.0 * scale;
        self.list = list;
        self.row_h = row_h;
        self.rows.clear();
        self.suggest_hits.clear();

        if is_path {
            // Path mode: the body is the completion list.
            for (i, p) in suggestions.iter().enumerate() {
                let ry = list.y + i as f32 * row_h;
                if ry + row_h > list.y + list.h {
                    break;
                }
                let is_dir = p.is_dir();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let label = if is_dir { format!("{name}/") } else { name };
                let row_rect = Rect { x: list.x, y: ry, w: list.w, h: row_h };
                if Some(i) == self.completion {
                    cx.highlight(row_rect);
                }
                let color = if is_dir { accent } else { FILE_FG };
                cx.label(&label, Family::Monospace, list.x + pad, ry + (row_h - ch) * 0.5, list, color);
                self.suggest_hits.push((row_rect, i, p.clone(), is_dir));
            }
            return;
        }

        // Browse/filter mode: the scrollable tree (or results) list.
        let rows: Vec<FileRow> = self.tree.rows().to_vec();
        if let Some(c) = &self.chosen {
            if !rows.iter().any(|r| !r.is_category && &r.path == c) {
                self.chosen = None;
            }
        }
        let max_scroll = (rows.len() as f32 * row_h - list.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let filtering = !query.is_empty();
        let indent_w = cx.ui.cell_w * 2.0;
        for (i, r) in rows.iter().enumerate() {
            self.rows.push(RowRef {
                path: r.path.clone(),
                is_dir: r.is_dir,
                is_category: r.is_category,
                key: r.key.clone(),
            });
            let ry = list.y + i as f32 * row_h - self.scroll;
            if ry + row_h < list.y || ry > list.y + list.h {
                continue;
            }
            let row_rect = Rect { x: list.x, y: ry, w: list.w, h: row_h };
            if !r.is_category && self.chosen.as_ref() == Some(&r.path) {
                cx.fill_alpha(row_rect, accent, SELECTED_ALPHA);
            }
            if Some(i) == self.sel {
                cx.highlight(row_rect);
            }
            let marker = match (r.is_dir, r.expanded) {
                (true, true) => "\u{25be} ",
                (true, false) => "\u{25b8} ",
                _ => "  ",
            };
            // Marker in the kind colour; an optional dim disambiguating prefix;
            // then the name by kind/extension (browse) or with matches lit (filter).
            let mut spans: Vec<(String, [u8; 3])> =
                vec![(marker.to_string(), if r.is_dir { accent } else { FILE_FG })];
            if !r.prefix.is_empty() {
                spans.push((format!("{}/", r.prefix), theme::DIM));
            }
            if filtering {
                spans.extend(Self::match_spans(&r.name, r.is_dir, &r.match_indices, accent));
            } else {
                spans.extend(Self::name_spans(&r.name, r.is_dir, accent));
            }
            let left = list.x + pad + r.depth as f32 * indent_w;
            let width = (list.x + list.w - left - pad).max(1.0);
            cx.row(&spans, left, ry + (row_h - ch) * 0.5, width, list, FILE_FG);
        }
    }

    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        if k.mods.super_ {
            return false;
        }
        let path_mode = self.tree.input_is_path();
        match k.key {
            Key::Escape => {
                self.tree.set_query("");
                self.clear_highlight();
            }
            Key::Backspace => {
                let mut q = self.tree.query().to_string();
                q.pop();
                self.tree.set_query(q);
                self.clear_highlight();
            }
            Key::Down | Key::Up | Key::Tab => {
                let delta = if k.key == Key::Up { -1 } else { 1 };
                if path_mode {
                    let n = self.tree.completions().len();
                    move_highlight(&mut self.completion, n, delta);
                } else if k.key != Key::Tab {
                    let n = self.tree.rows().len();
                    move_highlight(&mut self.sel, n, delta);
                    self.ensure_sel_visible();
                }
            }
            Key::Right => {
                if path_mode {
                    self.accept_completion();
                } else if let Some(r) = self.selected_row() {
                    if r.is_category && !r.expanded {
                        self.tree.toggle_category(&r.key);
                    } else if r.is_dir && !r.is_category && !r.expanded {
                        self.tree.toggle_dir(&r.path);
                    }
                }
            }
            Key::Left => {
                if !path_mode {
                    if let Some(r) = self.selected_row() {
                        if r.is_category && r.expanded {
                            self.tree.toggle_category(&r.key);
                        } else if r.is_dir && !r.is_category && r.expanded {
                            self.tree.toggle_dir(&r.path);
                        }
                    }
                }
            }
            Key::Enter => {
                if path_mode {
                    self.accept_completion();
                    if let Some(file) = self.tree.navigate_input() {
                        self.open_file(cx, file);
                    }
                    self.completion = None;
                } else if let Some(r) = self.selected_row() {
                    if r.is_category {
                        self.tree.toggle_category(&r.key);
                    } else if r.is_dir {
                        self.tree.set_root(r.path);
                        self.sel = None;
                    } else {
                        self.open_file(cx, r.path);
                    }
                }
            }
            Key::Char(c) => {
                let typed = k.text.clone().unwrap_or_else(|| c.to_string());
                let add: String = typed.chars().filter(|c| !c.is_control()).collect();
                if add.is_empty() {
                    return false;
                }
                let mut q = self.tree.query().to_string();
                q.push_str(&add);
                self.tree.set_query(q);
                self.clear_highlight();
            }
            _ => return false,
        }
        cx.redraw();
        true
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        match *event {
            MouseEvent::Down { pos: (x, y), clicks } => {
                if let Some((_, _, path, is_dir)) = self
                    .suggest_hits
                    .iter()
                    .find(|(r, ..)| rect_contains(*r, x, y))
                    .cloned()
                {
                    if is_dir {
                        self.tree.set_root(path);
                    } else {
                        self.open_file(cx, path);
                    }
                    cx.redraw();
                    return;
                }
                let Some((idx, row)) = self.row_at(x, y) else { return };
                self.sel = Some(idx);
                if row.is_category {
                    self.tree.toggle_category(&row.key);
                } else if row.is_dir {
                    // A click selects it and expands/collapses in place; a
                    // double-click on the same directory enters it (makes it the root).
                    if clicks >= 2 && self.last_click.as_ref() == Some(&row.path) {
                        self.tree.set_root(row.path.clone());
                        self.chosen = None;
                    } else {
                        self.tree.toggle_dir(&row.path);
                        self.chosen = Some(row.path.clone());
                    }
                    self.last_click = Some(row.path);
                } else {
                    // A file picker picks on a double-click; a browser opens at once.
                    let again = clicks >= 2 && self.last_click.as_ref() == Some(&row.path);
                    self.chosen = Some(row.path.clone());
                    self.last_click = Some(row.path.clone());
                    if self.mode != Mode::PickFile || again {
                        self.open_file(cx, row.path);
                    }
                }
                cx.redraw();
            }
            MouseEvent::Move { pos: (x, y) } => {
                // The mouse takes over the single highlight from the keyboard.
                if let Some(&(_, idx, ..)) =
                    self.suggest_hits.iter().find(|(r, ..)| rect_contains(*r, x, y))
                {
                    if self.completion != Some(idx) {
                        self.completion = Some(idx);
                        cx.redraw();
                    }
                    return;
                }
                if let Some((idx, _)) = self.row_at(x, y) {
                    if self.sel != Some(idx) {
                        self.sel = Some(idx);
                        cx.redraw();
                    }
                }
            }
            MouseEvent::Drag { .. } | MouseEvent::Up { .. } => {}
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, _pos: (f32, f32), _dx: f32, dy: f32) {
        // Pixel-precise; the upper bound is clamped to the content at paint.
        let next = (self.scroll - dy).max(0.0);
        if next != self.scroll {
            self.scroll = next;
            cx.request_frame();
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        let t = &mut self.tree;
        match id {
            BTN_PARENT => {
                t.go_to_parent();
            }
            BTN_HIDDEN => {
                let v = t.show_hidden();
                t.set_show_hidden(!v);
            }
            BTN_IGNORED => {
                let v = t.show_gitignored();
                t.set_show_gitignored(!v);
            }
            BTN_GIT => t.cycle_git_filter(),
            BTN_VIEW => t.cycle_view(),
            _ => return,
        }
        cx.redraw();
    }

    fn text(&mut self) -> Option<String> {
        let mut out = format!("{}\n", self.tree.root().display());
        for r in self.tree.rows() {
            let marker = match (r.is_dir, r.expanded) {
                (true, true) => "v ",
                (true, false) => "> ",
                _ => "  ",
            };
            out.push_str(&"  ".repeat(r.depth));
            out.push_str(marker);
            out.push_str(&r.name);
            out.push('\n');
        }
        Some(out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl FileBrowserView {
    /// The browse/filter row under a point, from the last paint.
    fn row_at(&self, x: f32, y: f32) -> Option<(usize, RowRef)> {
        if !rect_contains(self.list, x, y) {
            return None;
        }
        let idx = ((y - self.list.y + self.scroll) / self.row_h).floor().max(0.0) as usize;
        self.rows.get(idx).map(|r| (idx, r.clone()))
    }
}

/// Move a highlight index by `delta` within `[0, count)`; an unset highlight
/// starts at the first entry.
fn move_highlight(sel: &mut Option<usize>, count: usize, delta: i32) {
    if count == 0 {
        *sel = None;
        return;
    }
    *sel = Some(match *sel {
        None => 0,
        Some(c) => (c as i32 + delta).clamp(0, count as i32 - 1) as usize,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{press, tmpdir, Harness, UI};

    /// The browser's rect in these tests, and the centre of list row `i` in it.
    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    fn row_centre(i: usize) -> (f32, f32) {
        let header_h = UI.cell_h + 10.0;
        let row_h = UI.cell_h + 4.0;
        (50.0, header_h + i as f32 * row_h + row_h * 0.5)
    }

    /// A tree with a directory (holding a file) and a file: rows `adir`, `b.txt`.
    fn fixture() -> (PathBuf, FileBrowserView) {
        let d = tmpdir("fb");
        std::fs::create_dir(d.join("adir")).unwrap();
        std::fs::write(d.join("adir").join("inner.txt"), "x").unwrap();
        std::fs::write(d.join("b.txt"), "x").unwrap();
        let v = FileBrowserView::new(d.clone());
        (d, v)
    }

    #[test]
    fn paints_rows_and_registers_header_buttons() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        let ids: Vec<u32> = frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        for id in [BTN_PARENT, BTN_HIDDEN, BTN_IGNORED, BTN_GIT, BTN_VIEW] {
            assert!(ids.contains(&id), "header button {id} registered");
        }
        assert!(v.text().unwrap().contains("adir"), "rows reach the agent text");
    }

    #[test]
    fn typing_filters_and_escape_clears() {
        let (_d, mut v) = fixture();
        let h = Harness::new();
        let (used, out) = h.key(&mut v, press(Key::Char('b')));
        assert!(used && out.redraw);
        assert_eq!(v.tree().query(), "b");
        h.key(&mut v, press(Key::Escape));
        assert_eq!(v.tree().query(), "");
    }

    #[test]
    fn enter_enters_a_directory_or_opens_a_file() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        // Down selects the first row (adir); Down again selects b.txt.
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Down));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::OpenFile(d.join("b.txt"))]);
        // Back up to adir: Enter makes it the root.
        h.key(&mut v, press(Key::Up));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert!(out.requests.is_empty());
        assert_eq!(v.tree().root(), d.join("adir"));
    }

    #[test]
    fn picker_lists_directories_only() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        let ids: Vec<u32> = frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        assert!(!ids.contains(&BTN_GIT) && !ids.contains(&BTN_VIEW), "no file-only toggles");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1 });
        let rows: Vec<PathBuf> = v.tree_mut().rows().iter().map(|r| r.path.clone()).collect();
        assert_eq!(rows, vec![d.join("adir")], "expanded adir shows no inner.txt; no b.txt");
    }

    #[test]
    fn click_expands_and_double_click_enters() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let pos = row_centre(0);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 1 });
        assert!(v.tree().is_expanded(&d.join("adir")), "a click expands in place");
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 2 });
        assert_eq!(v.tree().root(), d.join("adir"), "a double-click enters");
        assert_eq!(v.chosen, None, "the entered directory is shown, not selected");
    }

    #[test]
    fn picker_confirms_the_clicked_directory_not_the_shown_one() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), Some(d.clone()), "with nothing selected, the shown directory");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1 });
        assert_eq!(v.choice(), Some(d.join("adir")), "a click selects without entering");
        assert_eq!(v.tree().root(), d, "the shown directory is unchanged");
        // Hover and arrows leave the selection alone.
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(0) });
        h.key(&mut v, press(Key::Down));
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), Some(d.join("adir")));
    }

    #[test]
    fn file_picker_selects_on_click_and_picks_on_double_click() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickFile);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), None, "nothing selected yet");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1 });
        assert_eq!(v.choice(), None, "a directory is never the chosen file");
        h.paint(&mut v, RECT); // adir expanded: b.txt moves to row 2
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(2), clicks: 1 });
        assert!(out.requests.is_empty(), "a click only selects");
        assert_eq!(v.choice(), Some(d.join("b.txt")));
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(2), clicks: 2 });
        assert_eq!(out.requests, vec![Request::Pick(d.join("b.txt"))]);
    }

    #[test]
    fn file_picker_picks_on_enter() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickFile);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Down));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.join("b.txt"))]);
    }

    #[test]
    fn selection_is_dropped_once_no_longer_listed() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1 });
        assert_eq!(v.chosen, Some(d.join("b.txt")), "a browser selects files too");
        assert_eq!(v.choice(), None, "a browser picks nothing");
        v.tree_mut().set_root(d.join("adir"));
        h.paint(&mut v, RECT);
        assert_eq!(v.chosen, None);
    }

    #[test]
    fn clicking_a_file_requests_it() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1 });
        assert_eq!(out.requests, vec![Request::OpenFile(d.join("b.txt"))]);
    }

    #[test]
    fn hover_moves_the_highlight() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let out = h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        assert!(out.redraw);
        assert_eq!(v.sel, Some(1));
        let out = h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        assert!(!out.redraw, "no redraw when the highlight doesn't move");
    }

    #[test]
    fn header_buttons_toggle_filters() {
        let (_d, mut v) = fixture();
        let h = Harness::new();
        let before = v.tree().show_hidden();
        let out = h.button(&mut v, BTN_HIDDEN);
        assert!(out.redraw);
        assert_eq!(v.tree().show_hidden(), !before);
    }

    #[test]
    fn scroll_is_pixel_precise_and_paced() {
        let d = tmpdir("fb-scroll");
        for i in 0..50 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut v = FileBrowserView::new(d);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let out = h.scroll(&mut v, (10.0, 100.0), -30.0);
        assert!(out.frame, "scrolling paces through the frame clock");
        assert_eq!(v.scroll, 30.0);
        h.scroll(&mut v, (10.0, 100.0), 1000.0);
        assert_eq!(v.scroll, 0.0, "can't scroll above the top");
    }
}
