//! The file browser: a navigable, filterable tree over
//! [`FsTree`](ghostrealm_core::fs_tree::FsTree). It is a Files surface's
//! sidebar, and it backs the floating picker, which chooses a directory, a
//! file, or where to save (see [`Mode`]). The plugin opens no view of its
//! own: it keeps the `file_browser.*` commands and the `[file_browser]`
//! config section, and opens a Files surface.
//!
//! Typing filters (fuzzy across the tree) or, when the input looks like a path,
//! completes it; Up/Down move the single highlight, which the mouse also takes
//! over on hover. A click also *selects* the row: a separate, sticky highlight
//! that hover and arrows leave alone. In the picker, the selected entry is the
//! one confirming chooses.

use std::any::Any;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use ghostrealm_core::config::{Kind, OptionSpec, SectionSpec};
use ghostrealm_core::fs_tree::{FileRow, FsTree, FsView, GitFilter};
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Rect, Registry};
use ghostrealm_terminal::{Key, KeyPress};

use crate::app_state::AppState;
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::plugin::icons::{FileKind, Icon};
use crate::plugin::text_edit::{Applied, LineEdit};
use crate::plugin::{
    rect_contains, theme, widgets, widgets::FieldMouse, EventCx, Font, MenuAlign, MouseEvent, OpenCx, PaintCx, Plugin,
    MenuItem, Request, UiMetrics, View, Waker,
};
use crate::plugins::files;

pub const ID: &str = "file_browser";

/// Row text: a name, and in filter results the matched characters and the
/// rest of the name (dimmer, so the matches stand out).
const NAME_FG: [u8; 3] = theme::TEXT;
const MATCH_FG: [u8; 3] = [255, 255, 255];
const UNMATCHED_FG: [u8; 3] = [175, 175, 188];
/// How much of the hover pill a directory picker's hover draws: weaker than
/// the keyboard's, which is the answer.
const HOVER_SHARE: f32 = 0.5;

/// Button ids (see [`View::button`]); the `⋯` menu's items reuse them.
const BTN_PARENT: u32 = 0;
const BTN_HIDDEN: u32 = 1;
const BTN_IGNORED: u32 = 2;
const BTN_GIT_MODIFIED: u32 = 3;
const BTN_GIT_UNTRACKED: u32 = 4;
const BTN_MORE: u32 = 6;
const BTN_CLEAR: u32 = 7;
/// `BTN_GROUP + i` shows [`VIEWS`]`[i]`. Past every other id.
const BTN_GROUP: u32 = 20;

/// The listings the `⋯` menu offers, with their labels.
const VIEWS: [(FsView, &str); 6] = [
    (FsView::Tree, "Tree"),
    (FsView::Extension, "Group by type"),
    (FsView::Kind, "Group by kind"),
    (FsView::GitStatus, "Group by git status"),
    (FsView::Modified, "Group by date modified"),
    (FsView::Size, "Group by size"),
];

pub struct FileBrowserPlugin;

impl Plugin for FileBrowserPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "File browser"
    }

    /// The browser lives in a Files surface's sidebar, so it isn't a surface
    /// of its own.
    fn pickable(&self) -> bool {
        false
    }

    /// A Files surface showing only its sidebar.
    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        files::FilesPlugin.open(cx)
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        r.register(
            CommandMeta::new(
                "file_browser.new",
                "New File Browser",
                "Open a Files surface showing only its file sidebar in the focused pane",
            ),
            Box::new(|s: &mut AppState, _| {
                s.open_plugin_in_focused(files::ID)
                    .map_err(|e| CmdError::Failed(format!("{e:#}")))?;
                Ok(CmdOutcome::ok())
            }),
        );
    }

    fn config_sections(&self) -> Vec<SectionSpec> {
        const SIDEBAR: &[(&str, &str)] = &[
            ("pinned", "Beside the document."),
            ("hidden", "Hidden until the sidebar button shows it."),
        ];
        vec![SectionSpec::options(
            "file_browser",
            "The Files surface's file sidebar, and its images and audio.",
            vec![
                OptionSpec::new(
                    "sidebar",
                    Kind::Enum(SIDEBAR),
                    Some("\"pinned\""),
                    "Whether a new Files surface shows its file sidebar beside a document. Once the \
                     sidebar button is clicked, new Files surfaces start as it was left, until the app \
                     quits. A Files surface showing no document always shows the sidebar.",
                ),
                OptionSpec::new(
                    "preview_autoplay",
                    Kind::Bool,
                    Some("true"),
                    "Play a track once it has been shown for a moment (toggle per document with \
                     its `autoplay` button).",
                ),
            ],
        )]
    }
}

/// What the browser is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A Files surface's sidebar: opening a file requests it (a click, Enter,
    /// or Right on a file).
    Sidebar,
    /// Choosing a directory: only directories are listed, and files never open.
    /// Enter picks what the confirm button shows ([`FileBrowserView::choice`]).
    /// The keys act on the keyboard's highlight, never the hover: Right/Tab
    /// enter it; Left/Backspace collapse it when expanded, else move to its
    /// parent row when nested, else go up.
    PickDir,
    /// Choosing a file: a click selects it; a double-click or Enter picks it.
    PickFile,
    /// Choosing where to save: typing edits the file name (in a field below the
    /// list), a click on a file takes its name, and the selected directory (else
    /// the shown one) holds it. Enter or a double-click on a file picks it.
    Save,
}

/// A row's click target: a real entry (path) or a synthetic category (key).
#[derive(Clone)]
struct RowRef {
    path: PathBuf,
    is_dir: bool,
    is_category: bool,
    key: String,
}

impl From<&FileRow> for RowRef {
    fn from(r: &FileRow) -> Self {
        RowRef { path: r.path.clone(), is_dir: r.is_dir, is_category: r.is_category, key: r.key.clone() }
    }
}

pub struct FileBrowserView {
    tree: FsTree,
    /// The directory the browser opened on; elsewhere, the filter hint shows
    /// the one listed.
    start: PathBuf,
    mode: Mode,
    /// Vertical scroll of the row list (physical px).
    scroll: f32,
    /// The highlighted row (browse/filter), moved by keys and clicks, and by
    /// hover outside [`Mode::PickDir`].
    sel: Option<usize>,
    /// The highlighted completion (path mode), moved like `sel`.
    completion: Option<usize>,
    /// [`Mode::PickDir`]'s hovered row or completion. Only the mouse sets it and
    /// only paint reads it (a weaker fill): the pointer crosses rows on its way
    /// to the confirm button, so there the answer and the keys follow `sel` and
    /// `completion` alone.
    hover: Option<usize>,
    /// The clicked (selected) entry; cleared once it is no longer listed.
    chosen: Option<PathBuf>,
    /// The filter / path input, mirroring the tree's query (the tree clears it
    /// on navigation; see [`Self::sync_input`]).
    input: LineEdit,
    /// The file name [`Mode::Save`] saves under (may hold a relative path).
    name: LineEdit,
    /// The mouse in the header's input field (live while it is the one typed
    /// into and shows what was typed, not a completion).
    field: FieldMouse,
    /// The mouse in [`Mode::Save`]'s file-name field.
    name_field: FieldMouse,
    /// The entry the previous click landed on, for double-click.
    last_click: Option<PathBuf>,
    // Geometry from the last paint, for mapping input to rows.
    list: Rect,
    row_h: f32,
    /// Path-mode completions as drawn: (rect, index, path, is_dir).
    suggest_hits: Vec<(Rect, usize, PathBuf, bool)>,
    /// Scroll the chosen entry into view at the next paint.
    reveal_chosen: bool,
    /// A key opened a file since [`Self::take_opened_by_key`] last asked.
    opened_by_key: bool,
    /// Files opened, counted so a key can tell that it opened one.
    opens: u64,
    /// The `⋯` button as last painted, which its menu opens under.
    more_hit: Option<Rect>,
}

impl FileBrowserView {
    pub fn new(root: PathBuf) -> Self {
        FileBrowserView {
            start: root.clone(),
            tree: FsTree::new(root),
            mode: Mode::Sidebar,
            scroll: 0.0,
            sel: None,
            completion: None,
            hover: None,
            chosen: None,
            input: LineEdit::default(),
            name: LineEdit::default(),
            last_click: None,
            list: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            row_h: 1.0,
            suggest_hits: Vec::new(),
            reveal_chosen: false,
            opened_by_key: false,
            opens: 0,
            more_hit: None,
            field: FieldMouse::default(),
            name_field: FieldMouse::default(),
        }
    }

    /// Wake the UI with `waker` when background work (the git filter's
    /// status) finishes.
    pub fn with_waker(mut self, waker: Option<Waker>) -> Self {
        self.tree.set_waker(waker);
        self
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

    /// Set the file name [`Mode::Save`] saves under.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name.set_text(name);
    }

    /// Show `path` selected: expand the folders down to it when it is under
    /// the root (or the folder the browser opened on), else make its folder
    /// the root.
    pub fn reveal(&mut self, path: &Path) {
        self.tree.set_query("");
        // Back to the folder the browser opened on (a Files surface's: the
        // workspace's) when it holds the path, before re-rooting elsewhere.
        if !path.starts_with(self.tree.root()) && path.starts_with(&self.start) {
            self.tree.set_root(&self.start);
        }
        let root = self.tree.root().to_path_buf();
        if path.starts_with(&root) {
            for dir in path.ancestors().skip(1).take_while(|d| d.starts_with(&root) && *d != root) {
                if !self.tree.is_expanded(dir) {
                    self.tree.toggle_dir(dir);
                }
            }
        } else if let Some(parent) = path.parent() {
            self.tree.set_root(parent);
        }
        self.chosen = Some(path.to_path_buf());
        self.reveal_chosen = true;
    }

    /// Take the tree's query into the input when the tree changed it (navigation
    /// clears it).
    fn sync_input(&mut self) {
        if self.input.text() != self.tree.query() {
            self.input.set_text(self.tree.query());
        }
    }

    pub fn tree(&self) -> &FsTree {
        &self.tree
    }

    pub fn tree_mut(&mut self) -> &mut FsTree {
        &mut self.tree
    }

    /// What a picker confirms: see [`Self::dir_choice`] ([`Mode::PickDir`]);
    /// the selected file ([`Mode::PickFile`]); the file name joined to the
    /// directory holding it ([`Mode::Save`]).
    pub fn choice(&mut self) -> Option<PathBuf> {
        match self.mode {
            Mode::Sidebar => None,
            Mode::PickDir => self.dir_choice(),
            Mode::PickFile => self.chosen.clone().filter(|p| !p.is_dir()),
            Mode::Save => {
                let name = self.name.text().trim();
                if name.is_empty() {
                    return None;
                }
                let root = self.tree.root();
                let dir = match &self.chosen {
                    Some(p) if p.is_dir() => p.as_path(),
                    Some(p) => p.parent().unwrap_or(root),
                    None => root,
                };
                Some(dir.join(name))
            }
        }
    }

    /// What a directory picker confirms, the same for Enter and the confirm
    /// button, from the listing as it is now. A typed path: the highlighted
    /// completion, else the directory it names, else its first completion
    /// (with `.` and `..` resolved). Otherwise the highlighted directory, else
    /// the selected one, else the shown one, or while filtering the first
    /// match (none: nothing). Paths are as listed, so a symlinked directory
    /// stays the link.
    fn dir_choice(&mut self) -> Option<PathBuf> {
        let query = self.tree.query().to_string();
        if !query.is_empty() && self.tree.input_is_path() {
            let suggestions = self.tree.suggestions(&query);
            return self
                .completion
                .and_then(|i| suggestions.get(i).cloned())
                .or_else(|| Some(self.tree.resolve_path(&query)).filter(|p| p.is_dir()))
                .or_else(|| suggestions.into_iter().next())
                .map(|p| lexical(&p))
                .filter(|p| p.is_dir());
        }
        if let Some(r) = self.selected_row().filter(|r| r.is_dir && !r.is_category) {
            return Some(r.path);
        }
        let chosen = self.chosen.clone();
        if let Some(dir) = chosen.filter(|c| self.tree.rows().iter().any(|r| r.is_dir && !r.is_category && &r.path == c)) {
            return Some(dir);
        }
        if query.is_empty() {
            return Some(self.tree.root().to_path_buf());
        }
        self.tree.rows().iter().find(|r| r.is_dir && !r.is_category).map(|r| r.path.clone())
    }

    /// [`Mode::PickDir`]'s highlighted row, to find again once the listing
    /// changes; other modes keep the highlight's index.
    fn sel_path(&mut self) -> Option<PathBuf> {
        if self.mode != Mode::PickDir {
            return None;
        }
        self.selected_row().filter(|r| !r.is_category).map(|r| r.path)
    }

    /// After the listing changed in [`Mode::PickDir`]: highlight `path` where it
    /// is listed now, else nothing.
    fn reselect(&mut self, path: Option<PathBuf>) {
        if self.mode != Mode::PickDir {
            return;
        }
        self.hover = None;
        self.completion = None;
        self.sel = path.and_then(|p| self.tree.rows().iter().position(|r| !r.is_category && r.path == p));
    }

    /// The selected entry.
    pub fn chosen(&self) -> Option<&Path> {
        self.chosen.as_deref()
    }

    /// How many files the sidebar has opened: a host compares counts to tell
    /// that an event opened one.
    pub fn opens(&self) -> u64 {
        self.opens
    }

    /// Whether a key opened a file since the last ask (the Files surface then
    /// moves the keyboard to the document).
    pub fn take_opened_by_key(&mut self) -> bool {
        std::mem::take(&mut self.opened_by_key)
    }

    /// Open (sidebar: selecting it) or pick (file picker) `path`; saving
    /// picks the choice.
    fn open_file(&mut self, cx: &mut EventCx, path: PathBuf) {
        match self.mode {
            Mode::Sidebar => {
                self.chosen = Some(path.clone());
                self.opens += 1;
                cx.request(Request::OpenFile(path));
            }
            Mode::PickFile => cx.request(Request::Pick(path)),
            Mode::Save => {
                if let Some(target) = self.choice() {
                    cx.request(Request::Pick(target));
                }
            }
            Mode::PickDir => {}
        }
    }

    /// [`Mode::Save`]'s keys: Enter picks; Up/Down move the highlight, which
    /// picks the directory to save into (a file's, for a file) but leaves the
    /// name; other text editing goes to the file name.
    fn save_key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        if matches!(k.key, Key::Up | Key::Down) && !k.mods.super_ {
            let delta = if k.key == Key::Up { -1 } else { 1 };
            let n = self.tree.rows().len();
            move_highlight(&mut self.sel, n, delta);
            self.ensure_sel_visible();
            if let Some(row) = self.selected_row().filter(|r| !r.is_category) {
                self.chosen = Some(row.path);
            }
        } else if k.key == Key::Enter {
            match self.choice() {
                Some(target) => cx.request(Request::Pick(target)),
                None => return false,
            }
        } else if !self.name.key(k, cx).used() {
            return false;
        }
        cx.redraw();
        true
    }

    /// [`Mode::Save`]'s file-name row, at the bottom of `rect`. Returns its
    /// height.
    fn paint_name_field(&self, cx: &mut PaintCx, rect: Rect) -> f32 {
        let ui = cx.ui;
        let pad = (8.0 * ui.scale).round();
        let h = header_height(ui);
        let row = Rect { x: rect.x, y: rect.y + rect.h - h, w: rect.w, h };
        let label = cx.shape("Name", Font::SANS, row.w);
        cx.place(label, row.x + pad, row.y + (h - ui.cell_h) * 0.5, row, theme::LABEL);
        let x = (row.x + pad * 2.0 + label.width).round();
        let field_h = row_height(ui);
        let field = Rect {
            x,
            y: row.y + ((h - field_h) * 0.5).round(),
            w: (row.x + row.w - pad - x).max(1.0),
            h: field_h,
        };
        field_box(cx, field, cx.focused);
        let text = Rect { x: field.x + pad * 0.5, w: (field.w - pad).max(1.0), ..field };
        let geom = widgets::field_text(cx, text, &self.name, "file name", cx.focused);
        self.name_field.painted(cx, geom);
        h
    }

    /// [`Mode::PickDir`]'s Enter: pick [`Self::dir_choice`]. A typed path
    /// naming no directory (nor completing to one) is gone to, as browsing does.
    /// (A directory picker lists no categories: it shows only the tree view.)
    fn pick_dir(&mut self, cx: &mut EventCx, path_mode: bool) {
        match self.choice() {
            Some(dir) => cx.request(Request::Pick(dir)),
            None if path_mode => {
                self.tree.navigate_input();
                self.clear_highlight();
            }
            None => {}
        }
    }

    /// Make the highlighted directory the shown one.
    fn enter_highlighted_dir(&mut self) {
        if let Some(r) = self.selected_row().filter(|r| r.is_dir && !r.is_category) {
            self.tree.set_root(r.path);
            self.clear_highlight();
            self.chosen = None;
        }
    }

    /// Collapse the highlighted directory when it is expanded, move the
    /// highlight to its parent row when it is nested, else show the parent
    /// with the directory just left highlighted.
    fn leave_dir(&mut self) {
        self.hover = None;
        let row = self.selected_row().filter(|r| !r.is_category);
        if let Some(r) = row.as_ref().filter(|r| r.is_dir && r.expanded) {
            self.tree.toggle_dir(&r.path);
            return;
        }
        let parent = row.filter(|r| r.depth > 0).and_then(|r| r.path.parent().map(Path::to_path_buf));
        let target = match parent {
            Some(p) => p,
            None => {
                let left = self.tree.root().to_path_buf();
                if !self.tree.go_to_parent() {
                    return;
                }
                self.chosen = None;
                left
            }
        };
        self.sel = self.tree.rows().iter().position(|r| !r.is_category && r.path == target);
        self.ensure_sel_visible();
    }

    fn clear_highlight(&mut self) {
        self.sel = None;
        self.completion = None;
        self.hover = None;
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

    /// Tree-row spans: a grouped view's dim disambiguating prefix, then the
    /// name.
    fn tree_spans(r: &FileRow) -> Vec<(String, [u8; 3])> {
        let mut spans = Vec::with_capacity(2);
        if !r.prefix.is_empty() {
            spans.push((format!("{}/", r.prefix), theme::DIM));
        }
        spans.push((r.name.clone(), NAME_FG));
        spans
    }

    /// Filter-result spans: the entry's own name with its matches lit, then
    /// the folder holding it (relative to the root), dim.
    fn result_spans(r: &FileRow) -> Vec<(String, [u8; 3])> {
        // A result's name is its path from the root, which the matches index.
        let leaf = file_name(&r.path);
        let offset = r.name.chars().count().saturating_sub(leaf.chars().count());
        let matches: Vec<usize> = r.match_indices.iter().filter_map(|i| i.checked_sub(offset)).collect();
        let mut spans = match_spans(&leaf, &matches);
        let dir: String = r.name.chars().take(offset).collect();
        let dir = dir.trim_end_matches(['/', '\\']);
        if !dir.is_empty() {
            spans.push((format!(" {dir}"), theme::DIM));
        }
        spans
    }

    /// The pill behind row `i` (or completion `i`): the selected entry's is
    /// strongest, then the keyboard's highlight, then a directory picker's
    /// hover. One per row, so they never stack.
    fn pill(&self, i: usize, path: Option<&Path>, highlight: Option<usize>) -> Option<([u8; 3], f32)> {
        if path.is_some() && self.chosen.as_deref() == path {
            Some((theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA))
        } else if highlight == Some(i) {
            Some((theme::HOVER_BG, theme::HOVER_ALPHA))
        } else if self.hover == Some(i) {
            Some((theme::HOVER_BG, theme::HOVER_ALPHA * HOVER_SHARE))
        } else {
            None
        }
    }

    /// Paint the visible `rows` (from index `first`) into `list`: indent
    /// guides, pills, icons, and names. Call it clipped to `list`.
    fn paint_rows(&self, cx: &mut PaintCx, list: Rect, rows: &[FileRow], first: usize, filtering: bool) {
        let g = RowGeom::new(cx.ui, list);
        let top = |i: usize| list.y + (first + i) as f32 * g.row_h - self.scroll;
        // Each guide is one fill per run of rows below an expanded folder, so
        // no two overlap (translucent fills look denser where they do).
        let line = theme::hairline(cx.ui);
        let mut open: Vec<f32> = Vec::new();
        let guide = |cx: &mut PaintCx, depth: usize, y0: f32, y1: f32| {
            let x = (g.slot_x(depth) + (g.slot - line) * 0.5).round();
            let (y0, y1) = (y0.round(), y1.round());
            cx.fill_alpha(Rect { x, y: y0, w: line, h: y1 - y0 }, theme::GUIDE, theme::GUIDE_ALPHA);
        };
        for (i, r) in rows.iter().enumerate() {
            while open.len() > r.depth {
                let y0 = open.pop().unwrap_or_default();
                guide(cx, open.len(), y0, top(i));
            }
            open.resize(r.depth, top(i));
        }
        while let Some(y0) = open.pop() {
            guide(cx, open.len(), y0, top(rows.len()));
        }

        let text_clip = Rect { w: (list.w - g.pad).max(1.0), ..list };
        for (i, r) in rows.iter().enumerate() {
            let ry = top(i);
            let path = (!r.is_category).then_some(r.path.as_path());
            if let Some((color, alpha)) = self.pill(first + i, path, self.sel) {
                cx.round_rect(g.pill(ry), theme::pill_radius(cx.ui), color, alpha, None);
            }
            let icon = g.icon(r.depth, ry);
            if r.is_dir && !filtering {
                let chevron = if r.expanded { Icon::ChevronDown } else { Icon::ChevronRight };
                chevron.paint(cx, g.chevron(icon), theme::GLYPH);
            } else if r.is_dir {
                FileKind::Folder { open: false }.paint(cx, icon);
            } else {
                FileKind::of(&r.path).paint(cx, icon);
            }
            let spans = if filtering { Self::result_spans(r) } else { Self::tree_spans(r) };
            // Cached by content: a row redrawn unchanged isn't reshaped.
            cx.row_font(&spans, Font::SANS, g.text_x(r.depth), g.text_y(cx.ui, ry), list.w, text_clip, NAME_FG);
        }
    }

    /// The header: the parent button, the filter box (a search icon, the
    /// input, and a clear button once it holds text), and `⋯` (the listing
    /// options). Returns the header height.
    fn paint_header(&mut self, cx: &mut PaintCx, rect: Rect, value: &LineEdit, editable: bool) -> f32 {
        let ui = cx.ui;
        let pad = (8.0 * ui.scale).round();
        let header_h = header_height(ui);
        let side = row_height(ui);
        let y = rect.y + ((header_h - side) * 0.5).round();

        let up = Rect { x: (rect.x + pad * 0.5).round(), y, w: side, h: side };
        // Built only while hovered: the tooltip shows only then.
        let tip = match cx.hovered(up) {
            true => format!("Up from {}", tilde(self.tree.root())),
            false => String::new(),
        };
        icon_button(cx, up, BTN_PARENT, Icon::ChevronUp, &tip, theme::GLYPH);
        let more = Rect { x: (rect.x + rect.w - pad * 0.5 - side).round(), ..up };
        // Tinted while an option hides or adds entries, so a filter that
        // empties the list doesn't go unnoticed.
        let tint = if self.options_changed() { cx.chrome.accent } else { theme::GLYPH };
        icon_button(cx, more, BTN_MORE, Icon::More, "More options", tint);
        self.more_hit = Some(more);

        // Saving types into the name field, so this one takes no keys there.
        let typed_into = self.mode != Mode::Save;
        let x = up.x + up.w + (pad * 0.5).round();
        let field = Rect { x, y, w: (more.x - (pad * 0.5).round() - x).max(1.0), h: side };
        field_box(cx, field, cx.focused && typed_into);
        let icon_side = (ui.cell_h * 0.8).round();
        let icon = Rect {
            x: field.x + pad,
            y: field.y + ((side - icon_side) * 0.5).round(),
            w: icon_side,
            h: icon_side,
        };
        Icon::Search.paint(cx, icon, theme::DIM);
        let mut text_right = field.x + field.w - pad * 0.5;
        if typed_into && !value.is_empty() {
            let clear = Rect { x: field.x + field.w - side, w: side, ..field };
            icon_button(cx, clear, BTN_CLEAR, Icon::Close, "Clear", theme::GLYPH);
            text_right = clear.x;
        }
        let text_x = icon.x + icon.w + pad * 0.5;
        let text = Rect { x: text_x, w: (text_right - text_x).max(1.0), ..field };
        let hint = self.hint();
        let geom = widgets::field_text(cx, text, value, &hint, cx.focused && typed_into);
        if editable {
            self.field.painted(cx, geom);
        } else {
            self.field.clear();
        }
        header_h
    }

    /// The filter box's hint: where the list is once that isn't where the
    /// browser opened. A picker, which goes where a typed path leads, always
    /// says.
    fn hint(&self) -> String {
        match self.mode {
            Mode::Sidebar if self.tree.root() == self.start => "Filter files".to_string(),
            _ => tilde(self.tree.root()),
        }
    }

    /// Why the listing is empty, if it is for a reason other than an empty
    /// folder: the git status it waits for, or a filter nothing passes.
    fn empty_note(&mut self) -> Option<String> {
        let filtered = self.tree.is_filtering() || self.tree.filters_active();
        if !self.tree.rows().is_empty() {
            return None;
        }
        // Only before the first status: a refresh keeps the last one, so
        // saying "loading" then would flicker.
        if self.tree.git_status_awaited() {
            return Some("Loading git status\u{2026}".to_string());
        }
        let what = if self.mode == Mode::PickDir { "folders" } else { "files" };
        filtered.then(|| format!("No matching {what}"))
    }

    /// Whether a listing option is off its default.
    fn options_changed(&self) -> bool {
        let t = &self.tree;
        t.show_hidden() || t.show_gitignored() || t.git_filter() != GitFilter::All || t.view() != FsView::Tree
    }

    /// The `⋯` menu: what the listing shows, checked as it stands. A
    /// directory picker lists no files, so it offers only what hides folders.
    fn options_menu(&self) -> Vec<MenuItem> {
        let t = &self.tree;
        let mut items = vec![
            MenuItem::new(BTN_HIDDEN, "Hidden files").check(t.show_hidden()),
            MenuItem::new(BTN_IGNORED, "Git-ignored files").check(t.show_gitignored()),
        ];
        if self.mode == Mode::PickDir {
            return items;
        }
        // The git choices say so while the status they need loads.
        let loading = |item: MenuItem| match item.checked == Some(true) && t.git_status_loading() {
            true => item.with_hint("loading\u{2026}"),
            false => item,
        };
        items.push(MenuItem::RULE);
        for (id, label, filter) in [
            (BTN_GIT_MODIFIED, "Only modified", GitFilter::Modified),
            (BTN_GIT_UNTRACKED, "Only untracked", GitFilter::Untracked),
        ] {
            items.push(loading(MenuItem::new(id, label).check(t.git_filter() == filter)));
        }
        items.push(MenuItem::RULE);
        for (i, (view, label)) in VIEWS.into_iter().enumerate() {
            let item = MenuItem::new(BTN_GROUP + i as u32, label).check(t.view() == view);
            items.push(if view == FsView::GitStatus { loading(item) } else { item });
        }
        items
    }
}

/// A filter or name field's rounded box: outlined, in the focus colour while
/// typing goes to it.
fn field_box(cx: &mut PaintCx, r: Rect, focused: bool) {
    let border = if focused {
        (theme::focus_ring_w(cx.ui), theme::FIELD_FOCUS_BORDER, 1.0)
    } else {
        (theme::hairline(cx.ui), theme::INPUT_BORDER, 1.0)
    };
    cx.round_rect(r, theme::box_radius(cx.ui), cx.chrome.background, 1.0, Some(border));
}

/// An icon button filling `r`: a rounded hover highlight, the icon in `color`
/// and brighter under the pointer, and a tooltip naming it.
fn icon_button(cx: &mut PaintCx, r: Rect, id: u32, icon: Icon, tip: &str, color: [u8; 3]) {
    let hover = cx.button(r, id);
    if hover {
        let inset = (2.0 * cx.ui.scale).round();
        let hl = Rect { x: r.x + inset, y: r.y + inset, w: r.w - inset * 2.0, h: r.h - inset * 2.0 };
        cx.round_rect(hl, theme::box_radius(cx.ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
    }
    cx.tooltip(r, tip);
    let side = (cx.ui.cell_h * 0.8).round();
    let at = Rect { x: r.x + ((r.w - side) * 0.5).round(), y: r.y + ((r.h - side) * 0.5).round(), w: side, h: side };
    icon.paint(cx, at, if hover { theme::GLYPH_HOVER } else { color });
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

        self.sync_input();
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
                LineEdit::new(s)
            });
        let editable = preview.is_none() && self.mode != Mode::Save;
        let value = preview.unwrap_or_else(|| self.input.clone());
        let header_h = self.paint_header(cx, rect, &value, editable);
        let footer_h = if self.mode == Mode::Save {
            self.paint_name_field(cx, rect)
        } else {
            0.0
        };

        let body = Rect {
            x: rect.x,
            y: rect.y + header_h,
            w: rect.w,
            h: (rect.h - header_h - footer_h).max(0.0),
        };
        let list = body;
        let g = RowGeom::new(cx.ui, list);
        let row_h = g.row_h;
        self.list = list;
        self.row_h = row_h;
        self.suggest_hits.clear();

        if is_path {
            // Path mode: the body is the completion list.
            cx.with_clip(list, |cx| {
                for (i, p) in suggestions.iter().enumerate() {
                    let ry = list.y + i as f32 * row_h;
                    if ry + row_h > list.y + list.h {
                        break;
                    }
                    let is_dir = p.is_dir();
                    let name = file_name(p);
                    let label = if is_dir { format!("{name}/") } else { name };
                    if let Some((color, alpha)) = self.pill(i, None, self.completion) {
                        cx.round_rect(g.pill(ry), theme::pill_radius(cx.ui), color, alpha, None);
                    }
                    let kind = if is_dir { FileKind::Folder { open: false } } else { FileKind::of(p) };
                    kind.paint(cx, g.icon(0, ry));
                    let spans = [(label, NAME_FG)];
                    cx.row_font(&spans, Font::SANS, g.text_x(0), g.text_y(cx.ui, ry), list.w, list, NAME_FG);
                    let row_rect = Rect { x: list.x, y: ry, w: list.w, h: row_h };
                    self.suggest_hits.push((row_rect, i, p.clone(), is_dir));
                }
            });
            return;
        }

        // Browse/filter mode: the scrollable tree (or results) list. Only the
        // rows in view are copied out of the tree.
        let filtering = self.tree.is_filtering();
        let note = self.empty_note();
        let rows = self.tree.rows();
        let count = rows.len();
        let listed = |p: &PathBuf| rows.iter().any(|r| !r.is_category && &r.path == p);
        if self.chosen.as_ref().is_some_and(|c| !listed(c)) {
            self.chosen = None;
        }
        if std::mem::take(&mut self.reveal_chosen) {
            if let Some(i) = rows.iter().position(|r| !r.is_category && Some(&r.path) == self.chosen.as_ref()) {
                self.scroll = (i as f32 * row_h - (list.h - row_h) * 0.5).max(0.0);
            }
        }
        let max_scroll = (count as f32 * row_h - list.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let first = ((self.scroll / row_h).floor() as usize).min(count);
        let last = (((self.scroll + list.h) / row_h).ceil() as usize).clamp(first, count);
        let visible: Vec<FileRow> = rows[first..last].to_vec();
        // Clipped, so a row scrolled half out draws neither over the header
        // nor under the footer.
        cx.with_clip(list, |cx| self.paint_rows(cx, list, &visible, first, filtering));
        // Say why the list is empty, else it reads as broken.
        if let Some(note) = note {
            cx.label(&note, Font::SANS, g.text_x(0), g.text_y(cx.ui, list.y), list, theme::DIM);
        }
    }

    /// Up/Down (Tab: completions) move the highlight, Enter opens or enters it,
    /// Escape clears the input, and other keys edit the input (see
    /// [`crate::plugin::text_edit`]). Plain ←/→ expand/collapse the highlighted
    /// row while the input is empty (→ on a sidebar's file opens it), and →
    /// at the end of a path accepts the highlighted completion. A directory
    /// picker differs (see [`Mode::PickDir`]):
    /// a directory is the answer there, so Enter picks it and the arrows and Tab
    /// move between directories.
    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        if self.mode == Mode::Save {
            return self.save_key(cx, k);
        }
        self.sync_input();
        let m = k.mods;
        let plain = !(m.shift || m.alt || m.super_ || m.ctrl);
        let path_mode = self.tree.input_is_path();
        let at_end = self.input.selection().is_none()
            && self.input.cursor() == self.input.text().chars().count();
        let pick_dir = self.mode == Mode::PickDir;
        let browsing = !path_mode && self.input.is_empty();
        let opens = self.opens;
        match k.key {
            Key::Escape if plain => {
                self.tree.set_query("");
                self.clear_highlight();
            }
            Key::Enter if pick_dir => self.pick_dir(cx, path_mode),
            Key::Right if pick_dir && plain && browsing => self.enter_highlighted_dir(),
            Key::Tab if pick_dir && plain && !path_mode => self.enter_highlighted_dir(),
            Key::Left | Key::Backspace if pick_dir && plain && browsing => self.leave_dir(),
            Key::Down | Key::Up | Key::Tab if !m.super_ => {
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
            Key::Right if plain && path_mode && at_end => {
                self.accept_completion();
                self.completion = None;
            }
            Key::Right if plain && self.input.is_empty() => {
                if let Some(r) = self.selected_row() {
                    if r.is_category && !r.expanded {
                        self.tree.toggle_category(&r.key);
                    } else if r.is_dir && !r.is_category && !r.expanded {
                        self.tree.toggle_dir(&r.path);
                    } else if !r.is_dir && !r.is_category && self.mode == Mode::Sidebar {
                        self.open_file(cx, r.path);
                    }
                }
            }
            Key::Left if plain && self.input.is_empty() => {
                if let Some(r) = self.selected_row() {
                    if r.is_category && r.expanded {
                        self.tree.toggle_category(&r.key);
                    } else if r.is_dir && !r.is_category && r.expanded {
                        self.tree.toggle_dir(&r.path);
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
            _ => match self.input.key(k, cx) {
                Applied::Ignored => return false,
                Applied::Moved => {}
                Applied::Changed => {
                    self.tree.set_query(self.input.text());
                    self.clear_highlight();
                }
            },
        }
        self.opened_by_key = self.opens != opens;
        self.sync_input();
        cx.redraw();
        true
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        if event.is_other_button() {
            return;
        }
        let extend = cx.mods.shift;
        if self.field.event(&mut self.input, event, extend) || self.name_field.event(&mut self.name, event, extend) {
            cx.redraw();
            return;
        }
        match *event {
            MouseEvent::Down { pos: (x, y), clicks, .. } => {
                if let Some((_, _, path, is_dir)) = self
                    .suggest_hits
                    .iter()
                    .find(|(r, ..)| rect_contains(*r, x, y))
                    .cloned()
                {
                    if is_dir {
                        self.tree.set_root(path);
                        self.clear_highlight();
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
                        self.clear_highlight();
                    } else {
                        self.tree.toggle_dir(&row.path);
                        self.chosen = Some(row.path.clone());
                    }
                    self.last_click = Some(row.path);
                } else {
                    // A browser opens at once; pickers pick on a double-click.
                    let again = clicks >= 2 && self.last_click.as_ref() == Some(&row.path);
                    self.chosen = Some(row.path.clone());
                    self.last_click = Some(row.path.clone());
                    if self.mode == Mode::Save {
                        self.name.set_text(file_name(&row.path));
                    }
                    if self.mode == Mode::Sidebar || again {
                        self.open_file(cx, row.path);
                    }
                }
                cx.redraw();
            }
            MouseEvent::Move { pos } if self.mode == Mode::PickDir => {
                let over = self
                    .suggest_hits
                    .iter()
                    .find(|(r, ..)| rect_contains(*r, pos.0, pos.1))
                    .map(|&(_, idx, ..)| idx)
                    .or_else(|| self.row_at(pos.0, pos.1).map(|(idx, _)| idx));
                if over != self.hover {
                    self.hover = over;
                    cx.redraw();
                }
            }
            MouseEvent::Leave { .. } if self.hover.is_some() => {
                self.hover = None;
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
            MouseEvent::Drag { .. } | MouseEvent::Up { .. } | MouseEvent::Leave { .. } => {}
        }
    }

    fn context_menu(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        if self.mode != Mode::Sidebar || !rect_contains(self.list, pos.0, pos.1) {
            return;
        }
        let target = match self.row_at(pos.0, pos.1) {
            Some((_, row)) if row.is_category => return,
            Some((idx, row)) => {
                self.sel = Some(idx);
                self.chosen = Some(row.path.clone());
                let kind = if row.is_dir { TargetKind::Dir } else { TargetKind::File };
                FileTarget { path: row.path, kind, origin: Origin::Browser }
            }
            None => FileTarget {
                path: self.tree.root().to_path_buf(),
                kind: TargetKind::Empty,
                origin: Origin::Browser,
            },
        };
        cx.request(Request::FileMenu { target, at: pos, align: MenuAlign::Left });
        cx.redraw();
    }

    /// The selected entry.
    fn subject(&self) -> Option<PathBuf> {
        self.chosen.clone().filter(|_| self.mode == Mode::Sidebar)
    }

    /// Re-read the listings; follow a moved selection or root.
    fn files_changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        let follow = |p: &Path| crate::file_ops::follow(p, moved);
        if let Some(root) = follow(self.tree.root()) {
            self.tree.set_root(root);
        }
        if let Some(chosen) = self.chosen.as_deref().and_then(follow) {
            self.chosen = Some(chosen);
        }
        self.tree.refresh();
    }

    fn scroll(&mut self, cx: &mut EventCx, _pos: (f32, f32), _dx: f32, dy: f32) {
        // Pixel-precise; the upper bound is clamped to the content at paint.
        let next = (self.scroll - dy).max(0.0);
        if next != self.scroll {
            self.scroll = next;
            // The rows moved under the pointer; the next move finds its row.
            self.hover = None;
            cx.request_frame();
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        match id {
            BTN_MORE => {
                if let Some(mb) = self.more_hit {
                    let items = self.options_menu();
                    cx.request(Request::Menu { items, at: (mb.x + mb.w, mb.y + mb.h), align: MenuAlign::Right });
                }
                return;
            }
            BTN_CLEAR => {
                self.tree.set_query("");
                self.clear_highlight();
                self.sync_input();
                cx.redraw();
                return;
            }
            _ => {}
        }
        let sel = self.sel_path();
        let t = &mut self.tree;
        let git = |t: &FsTree, f: GitFilter| if t.git_filter() == f { GitFilter::All } else { f };
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
            BTN_GIT_MODIFIED => t.set_git_filter(git(t, GitFilter::Modified)),
            BTN_GIT_UNTRACKED => t.set_git_filter(git(t, GitFilter::Untracked)),
            id if (BTN_GROUP..BTN_GROUP + VIEWS.len() as u32).contains(&id) => {
                t.set_view(VIEWS[(id - BTN_GROUP) as usize].0);
            }
            _ => return,
        }
        self.reselect(sel);
        cx.redraw();
    }

    fn refresh(&mut self) -> bool {
        self.tree.poll()
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
    fn row_at(&mut self, x: f32, y: f32) -> Option<(usize, RowRef)> {
        if !rect_contains(self.list, x, y) {
            return None;
        }
        let idx = ((y - self.list.y + self.scroll) / self.row_h).floor().max(0.0) as usize;
        self.tree.rows().get(idx).map(|r| (idx, RowRef::from(r)))
    }
}

/// A list row's height: roomier than a line of text, like a sidebar's.
fn row_height(ui: UiMetrics) -> f32 {
    (ui.cell_h + 10.0 * ui.scale).round()
}

/// The header's height: a row-high filter box with a margin around it.
fn header_height(ui: UiMetrics) -> f32 {
    row_height(ui) + (12.0 * ui.scale).round()
}

/// Where a list row's parts go, in physical px.
struct RowGeom {
    list: Rect,
    row_h: f32,
    pad: f32,
    /// An icon or chevron's square, which is also one indent level.
    slot: f32,
    /// Between the icon and the name.
    gap: f32,
}

impl RowGeom {
    fn new(ui: UiMetrics, list: Rect) -> Self {
        RowGeom {
            list,
            row_h: row_height(ui),
            pad: (8.0 * ui.scale).round(),
            slot: ui.cell_h.round(),
            gap: (6.0 * ui.scale).round(),
        }
    }

    /// The left of a row's icon at `depth`: each level indents one icon, so
    /// the guide under a folder's chevron runs just left of its children's.
    fn slot_x(&self, depth: usize) -> f32 {
        self.list.x + self.pad + depth as f32 * self.slot
    }

    /// The pill behind the row whose top is `ry`, inset from the list's
    /// sides and snapped to whole pixels.
    fn pill(&self, ry: f32) -> Rect {
        let inset = (self.pad * 0.5).round();
        Rect { x: self.list.x + inset, y: ry.round() + 1.0, w: self.list.w - inset * 2.0, h: self.row_h - 2.0 }
    }

    fn icon(&self, depth: usize, ry: f32) -> Rect {
        let y = (ry + (self.row_h - self.slot) * 0.5).round();
        Rect { x: self.slot_x(depth).round(), y, w: self.slot, h: self.slot }
    }

    /// A chevron in an icon's square: smaller than a file icon, as it is
    /// only a control.
    fn chevron(&self, icon: Rect) -> Rect {
        let side = (self.slot * 0.8).round();
        let d = ((self.slot - side) * 0.5).round();
        Rect { x: icon.x + d, y: icon.y + d, w: side, h: side }
    }

    fn text_x(&self, depth: usize) -> f32 {
        self.slot_x(depth) + self.slot + self.gap
    }

    fn text_y(&self, ui: UiMetrics, ry: f32) -> f32 {
        ry + (self.row_h - ui.cell_h) * 0.5
    }
}

/// `name` with the characters at `matches` (char indices) lit and the rest
/// dimmer; with no matches, plain.
fn match_spans(name: &str, matches: &[usize]) -> Vec<(String, [u8; 3])> {
    if matches.is_empty() {
        return vec![(name.to_string(), NAME_FG)];
    }
    let hit: HashSet<usize> = matches.iter().copied().collect();
    let mut spans: Vec<(String, [u8; 3])> = Vec::new();
    let mut cur = String::new();
    let mut cur_hit = false;
    for (i, ch) in name.chars().enumerate() {
        let h = hit.contains(&i);
        if !cur.is_empty() && h != cur_hit {
            spans.push((std::mem::take(&mut cur), if cur_hit { MATCH_FG } else { UNMATCHED_FG }));
        }
        cur_hit = h;
        cur.push(ch);
    }
    if !cur.is_empty() {
        spans.push((cur, if cur_hit { MATCH_FG } else { UNMATCHED_FG }));
    }
    spans
}

/// `path` for display, with the home directory as `~`.
fn tilde(path: &Path) -> String {
    // An unset, empty, or root `HOME` would turn every path into `~/…`.
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.parent().is_some());
    match home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// `path` with its `.` and `..` components resolved by name alone, following no
/// symlink.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// `path`'s last component as text.
fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
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
    use std::time::Instant;
    use crate::plugin::MouseButton;
    use crate::plugin::testing::{press, tmpdir, Harness, UI};

    /// The browser's rect in these tests, and the centre of list row `i` in it.
    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 400.0, h: 300.0 };
    fn row_centre(i: usize) -> (f32, f32) {
        let header_h = header_height(UI);
        let row_h = row_height(UI);
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
        for id in [BTN_PARENT, BTN_MORE] {
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
    fn the_filter_edits_at_the_cursor() {
        let (_d, mut v) = fixture();
        let h = Harness::new();
        h.key(&mut v, press(Key::Char('b')));
        h.key(&mut v, press(Key::Char('t')));
        h.key(&mut v, press(Key::Left));
        h.key(&mut v, press(Key::Char('x')));
        assert_eq!(v.tree().query(), "bxt");
        let mut opt_bs = press(Key::Backspace);
        opt_bs.mods.alt = true;
        h.key(&mut v, opt_bs);
        assert_eq!(v.tree().query(), "t", "Option+Backspace removes the word before the cursor");
    }

    #[test]
    fn arrows_expand_rows_only_while_the_filter_is_empty() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Right));
        assert!(v.tree().is_expanded(&d.join("adir")));
        h.key(&mut v, press(Key::Char('a')));
        h.key(&mut v, press(Key::Left));
        assert_eq!(v.input.cursor(), 0, "with text, Left moves the cursor");
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
        h.paint(&mut v, RECT);
        let ids: Vec<u32> = v.options_menu().iter().map(|i| i.id).collect();
        assert_eq!(ids, [BTN_HIDDEN, BTN_IGNORED], "no file-only options");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        let rows: Vec<PathBuf> = v.tree_mut().rows().iter().map(|r| r.path.clone()).collect();
        assert_eq!(rows, vec![d.join("adir")], "expanded adir shows no inner.txt; no b.txt");
    }

    #[test]
    fn directory_picker_enter_picks_the_highlighted_or_shown_directory() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.clone())], "nothing highlighted: the shown one");
        h.key(&mut v, press(Key::Down));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.join("adir"))]);
        assert_eq!(v.tree().root(), d, "Enter doesn't enter it");
    }

    #[test]
    fn directory_picker_right_and_tab_enter_left_and_backspace_leave() {
        let d = tmpdir("fb-pickdir-nav");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::create_dir(d.join("c")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Right));
        assert_eq!(v.tree().root(), d.join("a"));
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Tab));
        assert_eq!(v.tree().root(), d.join("a/b"));
        h.key(&mut v, press(Key::Backspace));
        assert_eq!(v.tree().root(), d.join("a"));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.join("a/b"))], "the directory left stays highlighted");
        h.key(&mut v, press(Key::Left));
        assert_eq!(v.tree().root(), d);
        // With a filter typed, Left and Backspace edit it instead.
        h.key(&mut v, press(Key::Char('c')));
        h.key(&mut v, press(Key::Left));
        assert_eq!((v.tree().root(), v.input.cursor()), (d.as_path(), 0), "Left moves the cursor");
        h.key(&mut v, press(Key::End));
        h.key(&mut v, press(Key::Backspace));
        assert_eq!((v.tree().root(), v.tree().query()), (d.as_path(), ""));
    }

    /// Type `text` into `v`'s input, then paint.
    fn type_in(h: &mut Harness, v: &mut FileBrowserView, text: &str) {
        h.key(v, press(Key::Escape));
        for c in text.chars() {
            h.key(v, press(Key::Char(c)));
        }
        h.paint(v, RECT);
    }

    #[test]
    fn directory_picker_enter_picks_the_typed_directory_not_a_completion() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("adir").join("deep")).unwrap();
        std::fs::create_dir(d.join("adir").join("deeper")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let enter = |h: &mut Harness, v: &mut FileBrowserView| h.key(v, press(Key::Enter)).1.requests;
        type_in(&mut h, &mut v, "adir/");
        assert_eq!(enter(&mut h, &mut v), vec![Request::Pick(d.join("adir"))]);
        let mut inner = FileBrowserView::picker(d.join("adir"), Mode::PickDir);
        type_in(&mut h, &mut inner, "..");
        assert_eq!(enter(&mut h, &mut inner), vec![Request::Pick(d.clone())]);
        if let Some(home) = std::env::var_os("HOME") {
            type_in(&mut h, &mut v, "~");
            assert_eq!(enter(&mut h, &mut v), vec![Request::Pick(PathBuf::from(home))]);
        }
        // Tab cycles the completions; Enter takes the highlighted one.
        type_in(&mut h, &mut v, "adir/");
        h.key(&mut v, press(Key::Tab));
        h.key(&mut v, press(Key::Tab));
        assert_eq!((v.completion, v.tree().root()), (Some(1), d.as_path()));
        assert_eq!(enter(&mut h, &mut v), vec![Request::Pick(d.join("adir/deeper"))]);
        // A hovered completion is not the answer.
        type_in(&mut h, &mut v, "adir/");
        let (r, ..) = v.suggest_hits[1];
        h.mouse(&mut v, MouseEvent::Move { pos: (r.x + 5.0, r.y + r.h * 0.5) });
        assert_eq!((v.hover, v.completion), (Some(1), None));
        assert_eq!(enter(&mut h, &mut v), vec![Request::Pick(d.join("adir"))]);
        // A partial name takes its first completion, `.` resolved; → accepting
        // one leaves no highlight behind.
        type_in(&mut h, &mut v, "./ad");
        assert_eq!(v.choice().map(|p| p.display().to_string()), Some(d.join("adir").display().to_string()));
        type_in(&mut h, &mut v, "adir/de");
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Right));
        assert_eq!(v.completion, None);
    }

    #[test]
    fn directory_picker_right_accepts_the_keyboard_completion_not_the_hovered_one() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("adir").join("deep")).unwrap();
        std::fs::create_dir(d.join("adir").join("deeper")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        type_in(&mut h, &mut v, "adir/de");
        let (r, ..) = v.suggest_hits[1];
        h.mouse(&mut v, MouseEvent::Move { pos: (r.x + 5.0, r.y + r.h * 0.5) });
        h.key(&mut v, press(Key::Right));
        assert!(v.tree().query().ends_with("/deep/"), "the first, got {}", v.tree().query());
    }

    /// The list rows a frame draws a pill behind in `color` at `alpha`.
    fn rows_lit_in(frame: &crate::plugin::paint::Frame, color: [u8; 3], alpha: f32) -> Vec<usize> {
        let (header_h, row_h) = (header_height(UI), row_height(UI));
        let want = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, color, alpha).color;
        frame
            .bg
            .iter()
            .filter(|q| q.color == want && q.radius > 0.0)
            .map(|q| (1.0 - q.pos[1]) * 0.5 * frame.screen.1)
            .filter(|y| *y > header_h - 1.0)
            .map(|y| ((y - header_h) / row_h).floor() as usize)
            .collect()
    }

    /// The list rows a frame draws a hover-coloured pill behind at `alpha`.
    fn rows_lit(frame: &crate::plugin::paint::Frame, alpha: f32) -> Vec<usize> {
        rows_lit_in(frame, theme::HOVER_BG, alpha)
    }

    #[test]
    fn directory_picker_double_click_answers_the_entered_directory() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("adir").join("deep")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 2, button: MouseButton::Left });
        h.paint(&mut v, RECT);
        assert_eq!(v.tree().root(), d.join("adir"));
        assert_eq!((v.sel, v.choice()), (None, Some(d.join("adir"))), "not its first child");
    }

    #[test]
    fn directory_picker_draws_the_keyboard_highlight_over_a_weaker_hover() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("cdir")).unwrap();
        let mut v = FileBrowserView::picker(d, Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        let frame = h.paint(&mut v, RECT);
        assert_eq!(rows_lit(frame, theme::HOVER_ALPHA), vec![0], "the keyboard's row");
        assert_eq!(rows_lit(frame, theme::HOVER_ALPHA * HOVER_SHARE), vec![1], "the hovered one, weaker");
    }

    #[test]
    fn other_modes_keep_their_highlight_through_toggles_and_file_changes() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Down));
        h.button(&mut v, BTN_HIDDEN);
        assert_eq!(v.sel, Some(1), "a listing option");
        h.button(&mut v, BTN_GROUP + 1);
        assert_eq!(v.sel, Some(1), "grouped, with category rows");
        h.button(&mut v, BTN_GROUP + 2);
        v.files_changed(&[(d.join("b.txt"), d.join("c.txt"))]);
        assert_eq!(v.sel, Some(1), "a rename");
        // Hover still moves a browser's highlight.
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(0) });
        assert_eq!((v.sel, v.hover), (Some(0), None));

        let mut save = FileBrowserView::picker(d.clone(), Mode::Save);
        save.set_name("n.md");
        h.paint(&mut save, RECT);
        h.key(&mut save, press(Key::Down));
        h.button(&mut save, BTN_HIDDEN);
        assert_eq!((save.sel, save.choice()), (Some(0), Some(d.join("adir/n.md"))));
    }

    #[test]
    fn directory_picker_enter_while_filtering_picks_the_first_match() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("cdir")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        type_in(&mut h, &mut v, "cd");
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.join("cdir"))]);
        type_in(&mut h, &mut v, "zzz");
        assert_eq!(v.choice(), None);
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert!(out.requests.is_empty(), "no match, no pick");
        // The answer reads the listing as it is now, not as last painted (Cmd+Enter
        // can confirm before the next paint).
        h.key(&mut v, press(Key::Escape));
        h.key(&mut v, press(Key::Char('c')));
        h.key(&mut v, press(Key::Char('d')));
        assert_eq!(v.choice(), Some(d.join("cdir")));
        // Tab enters a highlighted match.
        type_in(&mut h, &mut v, "cd");
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Tab));
        assert_eq!(v.tree().root(), d.join("cdir"));
    }

    #[test]
    fn directory_picker_enter_and_the_confirm_button_ignore_the_hover() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(0) });
        assert_eq!((v.hover, v.sel), (Some(0), None));
        assert_eq!(v.choice(), Some(d.clone()), "the button shows the listed directory");
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.clone())], "and Enter agrees");
        h.key(&mut v, press(Key::Down));
        h.key(&mut v, press(Key::Up));
        assert_eq!(v.choice(), Some(d.join("adir")), "a keyboard highlight is the answer");
    }

    #[test]
    fn directory_picker_keys_follow_the_keyboard_highlight_through_hover() {
        let d = tmpdir("fb-pickdir-hover");
        std::fs::create_dir_all(d.join("a/inner")).unwrap();
        std::fs::create_dir(d.join("b")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        assert_eq!((v.hover, v.sel), (Some(1), Some(0)), "the hover leaves the highlight");
        assert_eq!(v.choice(), Some(d.join("a")), "and the answer");
        h.key(&mut v, press(Key::Right));
        assert_eq!(v.tree().root(), d.join("a"), "→ enters the keyboard's row, not the hovered one");
        // A modified Tab moves nothing, so it highlights nothing.
        let mut shift_tab = press(Key::Tab);
        shift_tab.mods.shift = true;
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(0) });
        h.key(&mut v, shift_tab);
        assert_eq!(v.choice(), Some(d.join("a")), "the shown directory, not the hovered a/inner");
    }

    #[test]
    fn directory_picker_drops_a_keyboard_highlight_the_listing_lost() {
        let d = tmpdir("fb-pickdir-relist");
        std::fs::create_dir_all(d.join("a/inner")).unwrap();
        std::fs::create_dir(d.join(".hidden")).unwrap();
        let mut v = FileBrowserView::picker(d.join("a"), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        assert_eq!(v.choice(), Some(d.join("a/inner")));
        h.button(&mut v, BTN_PARENT);
        h.paint(&mut v, RECT);
        assert_eq!((v.sel, v.choice()), (None, Some(d.clone())), "not row 0 of the parent");
        // A listing that still holds it keeps it highlighted where it moved.
        h.key(&mut v, press(Key::Down));
        assert_eq!(v.choice(), Some(d.join("a")));
        let shown = v.tree().show_hidden();
        h.button(&mut v, BTN_HIDDEN);
        h.paint(&mut v, RECT);
        let at = v.tree_mut().rows().iter().position(|r| r.path == d.join("a"));
        assert_eq!(v.sel, at, "hidden shown: {}", !shown);
        assert_eq!(v.choice(), Some(d.join("a")));
    }

    #[test]
    fn directory_picker_field_shows_only_a_keyboard_completion() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("adir").join("deep")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        type_in(&mut h, &mut v, "adir/");
        let (r, ..) = v.suggest_hits[0];
        h.mouse(&mut v, MouseEvent::Move { pos: (r.x + 5.0, r.y + r.h * 0.5) });
        h.paint(&mut v, RECT);
        assert!(v.field.geom().is_some(), "a hovered completion leaves the typed text");
        h.key(&mut v, press(Key::Down));
        h.paint(&mut v, RECT);
        assert!(v.field.geom().is_none(), "a keyboard one shows in the field");
    }

    #[test]
    fn directory_picker_left_climbs_to_the_parent_row_then_collapses_it() {
        let d = tmpdir("fb-pickdir-left");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        assert_eq!(v.sel, Some(1), "a/b, nested");
        h.key(&mut v, press(Key::Left));
        assert_eq!((v.sel, v.tree().root()), (Some(0), d.as_path()), "to its parent row, not above the root");
        h.key(&mut v, press(Key::Left));
        assert!(!v.tree().is_expanded(&d.join("a")), "then collapses it");
        assert_eq!(v.tree().root(), d);
    }

    #[test]
    fn click_expands_and_double_click_enters() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let pos = row_centre(0);
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left });
        assert!(v.tree().is_expanded(&d.join("adir")), "a click expands in place");
        h.mouse(&mut v, MouseEvent::Down { pos, clicks: 2, button: MouseButton::Left });
        assert_eq!(v.tree().root(), d.join("adir"), "a double-click enters");
        assert_eq!(v.chosen, None, "the entered directory is shown, not selected");
    }

    #[test]
    fn picker_confirms_the_clicked_directory_not_the_shown_one() {
        let (d, _) = fixture();
        std::fs::create_dir(d.join("cdir")).unwrap();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickDir);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), Some(d.clone()), "with nothing selected, the shown directory");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.choice(), Some(d.join("adir")), "a click selects without entering");
        assert_eq!(v.tree().root(), d, "the shown directory is unchanged");
        // Hover leaves the answer alone; an arrow moves it (adir's empty
        // expansion keeps cdir at row 1).
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        assert_eq!(v.choice(), Some(d.join("adir")));
        h.key(&mut v, press(Key::Down));
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), Some(d.join("cdir")));
    }

    #[test]
    fn file_picker_selects_on_click_and_picks_on_double_click() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::PickFile);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), None, "nothing selected yet");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.choice(), None, "a directory is never the chosen file");
        h.paint(&mut v, RECT); // adir expanded: b.txt moves to row 2
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(2), clicks: 1, button: MouseButton::Left });
        assert!(out.requests.is_empty(), "a click only selects");
        assert_eq!(v.choice(), Some(d.join("b.txt")));
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(2), clicks: 2, button: MouseButton::Left });
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
    fn only_a_header_field_that_takes_the_mouse_shows_the_text_cursor() {
        let (d, _) = fixture();
        let mut h = Harness::new();
        let mut pick = FileBrowserView::picker(d.clone(), Mode::PickFile);
        h.paint(&mut pick, RECT);
        let r = pick.field.geom().expect("a picker's field is live").rect;
        h.cursor = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        let icon = |h: &mut Harness, v: &mut FileBrowserView| h.paint(v, RECT).cursor.map(|(_, i)| i);
        assert_eq!(icon(&mut h, &mut pick), Some(winit::window::CursorIcon::Text));
        let mut save = FileBrowserView::picker(d, Mode::Save);
        assert_eq!(icon(&mut h, &mut save), None, "Save's header field is inert");
    }

    #[test]
    fn save_types_the_name_into_the_shown_or_selected_directory() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::Save);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert_eq!(v.choice(), None, "no name yet");
        for c in "new.md".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        assert_eq!(v.tree().query(), "", "typing edits the name, not the filter");
        assert_eq!(v.choice(), Some(d.join("new.md")), "into the shown directory");
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(0), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.choice(), Some(d.join("adir").join("new.md")), "into the selected one");
        h.key(&mut v, press(Key::Backspace));
        let (_, out) = h.key(&mut v, press(Key::Enter));
        assert_eq!(out.requests, vec![Request::Pick(d.join("adir").join("new.m"))]);
    }

    #[test]
    fn save_arrows_move_the_highlight_and_keep_the_name() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::Save);
        v.set_name("new.md");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.key(&mut v, press(Key::Down));
        assert_eq!(v.sel, Some(0));
        assert_eq!(v.choice(), Some(d.join("adir").join("new.md")), "into the highlighted dir");
        h.key(&mut v, press(Key::Down));
        assert_eq!(v.sel, Some(1));
        assert_eq!(v.choice(), Some(d.join("new.md")), "a file's own dir; the name stays");
        h.key(&mut v, press(Key::Up));
        assert_eq!(v.sel, Some(0));
    }

    #[test]
    fn save_name_supports_select_all_and_retype() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::Save);
        v.set_name("draft.txt");
        let h = Harness::new();
        h.key(&mut v, crate::plugin::testing::cmd(Key::Char('a')));
        h.key(&mut v, press(Key::Char('z')));
        assert_eq!(v.choice(), Some(d.join("z")));
    }

    /// The centre of column `col`'s cell in a painted field.
    fn cell_at(g: widgets::FieldGeom, col: usize) -> (f32, f32) {
        (g.left + (col as f32 + 0.5) * g.cell_w - g.first as f32 * g.cell_w, g.rect.y + g.rect.h * 0.5)
    }

    /// The left edge of column `col`'s cell: the caret position before it.
    fn edge_at(g: widgets::FieldGeom, col: usize) -> (f32, f32) {
        (g.left + col as f32 * g.cell_w - g.first as f32 * g.cell_w, g.rect.y + g.rect.h * 0.5)
    }

    #[test]
    fn the_save_name_takes_the_mouse() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::Save);
        v.set_name("draft one.txt");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let g = v.name_field.geom().expect("the name field is live");
        // Click places the cursor between two cells; typing lands there.
        h.mouse(&mut v, MouseEvent::Down { pos: edge_at(g, 5), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: edge_at(g, 5), dragged: false, button: MouseButton::Left });
        h.key(&mut v, press(Key::Char('X')));
        assert_eq!(v.name.text(), "draftX one.txt");
        // Drag selects; typing replaces the selection.
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: edge_at(g, 0), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Drag { pos: edge_at(g, 5), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: edge_at(g, 5), dragged: true, button: MouseButton::Left });
        h.key(&mut v, press(Key::Char('n')));
        assert_eq!(v.name.text(), "nX one.txt");
        // Double-click selects the word, triple-click all.
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell_at(g, 3), clicks: 2, button: MouseButton::Left });
        assert_eq!(v.name.selection(), Some((3, 6)), "`one`");
        h.mouse(&mut v, MouseEvent::Down { pos: cell_at(g, 3), clicks: 3, button: MouseButton::Left });
        assert_eq!(v.name.selection(), Some((0, 10)));
        // Shift+click extends from the anchor.
        let shift = ghostrealm_terminal::Mods { shift: true, ..Default::default() };
        h.mouse(&mut v, MouseEvent::Down { pos: edge_at(g, 1), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: edge_at(g, 1), dragged: false, button: MouseButton::Left });
        h.mouse_with(&mut v, shift, MouseEvent::Down { pos: edge_at(g, 5), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.name.selection(), Some((1, 5)));
    }

    #[test]
    fn the_filter_field_takes_the_mouse_but_a_save_dialogs_header_does_not() {
        let (_, mut v) = fixture();
        let mut h = Harness::new();
        const WIDE: Rect = Rect { x: 0.0, y: 0.0, w: 1200.0, h: 300.0 };
        for c in "ab".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        h.paint(&mut v, WIDE);
        let g = v.field.geom().expect("the filter is live");
        assert_eq!(v.input.text(), "ab");
        h.mouse(&mut v, MouseEvent::Down { pos: cell_at(g, 0), clicks: 2, button: MouseButton::Left });
        assert_eq!(v.input.selection(), Some((0, 2)));
        h.mouse(&mut v, MouseEvent::Up { pos: cell_at(g, 0), dragged: false, button: MouseButton::Left });
        assert_eq!(v.input.selection(), Some((0, 2)), "the release keeps the word");
        // A save dialog types into the name, so its header field is inert.
        let mut save = FileBrowserView::picker(v.tree().root().to_path_buf(), Mode::Save);
        h.paint(&mut save, WIDE);
        assert!(save.field.geom().is_none());
    }

    #[test]
    fn save_takes_a_clicked_files_name() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::picker(d.clone(), Mode::Save);
        v.set_name("draft.txt");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1, button: MouseButton::Left });
        assert!(out.requests.is_empty(), "a click only takes the name");
        assert_eq!(v.choice(), Some(d.join("b.txt")));
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 2, button: MouseButton::Left });
        assert_eq!(out.requests, vec![Request::Pick(d.join("b.txt"))]);
    }

    #[test]
    fn selection_is_dropped_once_no_longer_listed() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1, button: MouseButton::Left });
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
        let out = h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1, button: MouseButton::Left });
        assert_eq!(out.requests, vec![Request::OpenFile(d.join("b.txt"))]);
    }

    #[test]
    fn a_right_click_opens_the_file_menu_on_the_row_or_the_empty_space() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let menu = |h: &Harness, v: &mut FileBrowserView, pos: (f32, f32)| h.context_menu(v, pos).requests;
        let target = |path: PathBuf, kind, ()| FileTarget { path, kind, origin: Origin::Browser };
        let at = row_centre(0);
        assert_eq!(
            menu(&h, &mut v, at),
            [Request::FileMenu { target: target(d.join("adir"), TargetKind::Dir, ()), at, align: MenuAlign::Left }]
        );
        assert_eq!(v.subject(), Some(d.join("adir")), "the row is selected");
        let at = row_centre(1);
        assert_eq!(
            menu(&h, &mut v, at),
            [Request::FileMenu { target: target(d.join("b.txt"), TargetKind::File, ()), at, align: MenuAlign::Left }]
        );
        let at = (50.0, 250.0);
        assert_eq!(
            menu(&h, &mut v, at),
            [Request::FileMenu { target: target(d.clone(), TargetKind::Empty, ()), at, align: MenuAlign::Left }],
            "below the rows: the folder shown"
        );
        // Outside the list (the header), and in a picker, there is no menu.
        assert!(menu(&h, &mut v, (50.0, 3.0)).is_empty());
        let mut picker = FileBrowserView::picker(d, Mode::PickFile);
        h.paint(&mut picker, RECT);
        assert!(menu(&h, &mut picker, row_centre(0)).is_empty());
    }

    #[test]
    fn changed_files_are_re_read_and_a_moved_selection_or_root_is_followed() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        std::fs::write(d.join("new.txt"), "").unwrap();
        assert!(!v.text().unwrap().contains("new.txt"), "listings are cached");
        v.files_changed(&[]);
        h.paint(&mut v, RECT);
        assert!(v.text().unwrap().contains("new.txt"));
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1, button: MouseButton::Left });
        let chosen = v.subject().unwrap();
        v.files_changed(&[(chosen.clone(), d.join("renamed.txt"))]);
        assert_eq!(v.subject(), Some(d.join("renamed.txt")));
        let moved = d.with_file_name("moved-root");
        v.files_changed(&[(d.clone(), moved.clone())]);
        assert_eq!(v.tree().root(), moved);
    }

    #[test]
    fn reveal_expands_the_folders_down_to_a_file_and_selects_it() {
        let d = tmpdir("fb-reveal");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/b/f.txt"), "").unwrap();
        let mut v = FileBrowserView::new(d.clone());
        v.reveal(&d.join("a/b/f.txt"));
        assert!(v.tree().is_expanded(&d.join("a")) && v.tree().is_expanded(&d.join("a/b")));
        assert_eq!(v.subject(), Some(d.join("a/b/f.txt")));
        // A file outside the root makes its folder the root.
        let other = tmpdir("fb-reveal-other");
        std::fs::write(other.join("g.txt"), "").unwrap();
        v.reveal(&other.join("g.txt"));
        assert_eq!(v.tree().root(), other);
        assert_eq!(v.subject(), Some(other.join("g.txt")));
        Harness::new().paint(&mut v, RECT);
        assert!(v.text().unwrap().contains("g.txt"));
        // Back inside the folder it opened on, the browser goes back there.
        v.reveal(&d.join("a/b/f.txt"));
        assert_eq!(v.tree().root(), d);
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
    fn the_git_filter_applies_once_its_status_arrives_from_the_worker() {
        let d = tmpdir("fb-git-async");
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&d).args(args).output().is_ok_and(|o| o.status.success())
        };
        if !git(&["init", "-q"]) {
            return;
        }
        std::fs::write(d.join("clean.txt"), "x").unwrap();
        std::fs::write(d.join("changed.txt"), "x").unwrap();
        assert!(git(&["add", "-A"]));
        assert!(git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "i"]));
        std::fs::write(d.join("changed.txt"), "y").unwrap();
        let (tx, woken) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let waker: Waker = std::sync::Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        });
        let mut v = FileBrowserView::new(d.clone()).with_waker(Some(waker));
        let h = Harness::new();
        h.button(&mut v, BTN_GIT_MODIFIED);
        let before = v.text().unwrap();
        assert!(!before.contains("changed.txt"), "no status yet, so nothing passes:\n{before}");
        assert!(v.tree().git_status_loading());
        assert_eq!(v.empty_note().as_deref(), Some("Loading git status\u{2026}"), "the empty list says why");
        woken.recv_timeout(std::time::Duration::from_secs(10)).expect("the worker wakes the UI");
        assert!(v.refresh(), "the arrived status repaints");
        let after = v.text().unwrap();
        assert!(after.contains("changed.txt") && !after.contains("clean.txt"), "{after}");
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

    /// The plain (unrounded) fills a frame draws in `color` at `alpha`, as
    /// rects in physical px.
    fn fills(frame: &crate::plugin::paint::Frame, color: [u8; 3], alpha: f32) -> Vec<Rect> {
        let want = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, color, alpha).color;
        frame
            .bg
            .iter()
            .filter(|q| q.color == want && q.radius == 0.0 && q.border_w == 0.0)
            .map(|q| Rect {
                x: (q.pos[0] + 1.0) * 0.5 * frame.screen.0,
                y: (1.0 - q.pos[1]) * 0.5 * frame.screen.1,
                w: q.px[0],
                h: q.px[1],
            })
            .collect()
    }

    #[test]
    fn nested_rows_draw_one_guide_per_level_without_overlap() {
        let d = tmpdir("fb-guides");
        std::fs::create_dir_all(d.join("a/b/c")).unwrap();
        std::fs::write(d.join("a/b/c/f.txt"), "").unwrap();
        let mut v = FileBrowserView::new(d.clone());
        v.reveal(&d.join("a/b/c/f.txt"));
        let mut h = Harness::new();
        let frame = h.paint(&mut v, RECT);
        // Rows a, b, c, f.txt at depths 0..=3: level 0's guide runs beside
        // the three rows under a, level 1's the two under b, level 2's one.
        let mut guides = fills(frame, theme::GUIDE, theme::GUIDE_ALPHA);
        guides.sort_by(|a, b| a.x.total_cmp(&b.x));
        let row_h = row_height(UI);
        // Within float error of the NDC round trip.
        let whole = |v: f32| (v - v.round()).abs() < 1e-3;
        let heights: Vec<f32> = guides.iter().map(|g| g.h.round()).collect();
        assert_eq!(heights, vec![3.0 * row_h, 2.0 * row_h, row_h], "{guides:?}");
        assert!(guides.windows(2).all(|w| w[0].x + w[0].w <= w[1].x + 1e-3), "no overlap: {guides:?}");
        assert!(guides.iter().all(|g| whole(g.x) && whole(g.y) && g.w == theme::hairline(UI)), "{guides:?}");
        // Collapsing a hides its subtree and its guides.
        v.tree_mut().toggle_dir(&d.join("a"));
        assert!(fills(h.paint(&mut v, RECT), theme::GUIDE, theme::GUIDE_ALPHA).is_empty());
    }

    #[test]
    fn the_selected_rows_pill_is_stronger_than_the_hovered_ones() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: row_centre(1), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(0) });
        let frame = h.paint(&mut v, RECT);
        assert_eq!(rows_lit_in(frame, theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA), vec![1]);
        assert_eq!(rows_lit(frame, theme::HOVER_ALPHA), vec![0]);
        const { assert!(theme::ROW_SELECTED_ALPHA > theme::HOVER_ALPHA) };
        // The selected row under the pointer shows one pill, the selected one.
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        let frame = h.paint(&mut v, RECT);
        assert_eq!(rows_lit_in(frame, theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA), vec![1]);
        assert!(rows_lit(frame, theme::HOVER_ALPHA).is_empty());
    }

    #[test]
    fn rows_scrolled_above_the_list_are_clipped_to_it() {
        let d = tmpdir("fb-clip");
        for i in 0..30 {
            std::fs::write(d.join(format!("f{i:02}.txt")), "x").unwrap();
        }
        let mut v = FileBrowserView::new(d);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let half = (row_height(UI) * 0.5).round();
        h.scroll(&mut v, (10.0, 100.0), -half);
        let list = v.list;
        // Hover the half-hidden first row, so it draws a pill.
        h.mouse(&mut v, MouseEvent::Move { pos: (50.0, list.y + 1.0) });
        assert_eq!(v.sel, Some(0));
        let frame = h.paint(&mut v, RECT);
        let pill = frame
            .bg
            .iter()
            .find(|q| q.radius > 0.0 && q.color[3] == theme::HOVER_ALPHA)
            .expect("the hovered row's pill");
        assert!((1.0 - pill.pos[1]) * 0.5 * frame.screen.1 < list.y, "the pill starts above the list");
        assert_eq!(pill.clip[1], list.y, "and is cut at its top");
        // Icons and the row's name are cut there too.
        let g = RowGeom::new(UI, list);
        let name = frame.text.iter().find(|t| t.left == g.text_x(0) && t.top < list.y).expect("row 0's name");
        assert!(name.bounds.top >= list.y as i32, "{:?}", name.bounds);
        let kind = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, FileKind::Text.color(), 1.0).color;
        for q in frame.bg.iter().filter(|q| q.color == kind) {
            let top = (1.0 - q.pos[1]) * 0.5 * frame.screen.1;
            assert!(top >= list.y || q.clip[1] >= list.y, "an icon quad drawn over the header");
        }
    }

    #[test]
    fn filter_results_are_flat_with_the_folder_dimmed_after_the_name() {
        let d = tmpdir("fb-results");
        std::fs::create_dir(d.join("docs")).unwrap();
        std::fs::write(d.join("docs/GOAL.md"), "").unwrap();
        std::fs::write(d.join("AGENTS.md"), "").unwrap();
        let mut v = FileBrowserView::new(d.clone());
        let mut h = Harness::new();
        type_in(&mut h, &mut v, "goal");
        let rows = v.tree_mut().rows().to_vec();
        let goal = rows.iter().find(|r| r.path == d.join("docs/GOAL.md")).expect("a result");
        assert_eq!(goal.depth, 0, "flat");
        let spans = FileBrowserView::result_spans(goal);
        let text: String = spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text, "GOAL.md docs");
        assert_eq!(spans.last().unwrap(), &(" docs".to_string(), theme::DIM));
        let lit: String = spans.iter().filter(|(_, c)| *c == MATCH_FG).map(|(t, _)| t.as_str()).collect();
        assert_eq!(lit, "GOAL", "the matched letters, within the name");
        // At the root, no folder follows.
        type_in(&mut h, &mut v, "agents");
        let agents = v.tree_mut().rows()[0].clone();
        let spans = FileBrowserView::result_spans(&agents);
        assert!(spans.iter().all(|(_, c)| *c != theme::DIM), "{spans:?}");
    }

    #[test]
    fn the_filter_is_smart_case() {
        let d = tmpdir("fb-smart-case");
        std::fs::write(d.join("Goal.md"), "").unwrap();
        std::fs::write(d.join("gold.txt"), "").unwrap();
        let mut v = FileBrowserView::new(d.clone());
        let mut h = Harness::new();
        let names = |v: &mut FileBrowserView| {
            let mut n: Vec<String> = v.tree_mut().rows().iter().map(|r| file_name(&r.path)).collect();
            n.sort();
            n
        };
        type_in(&mut h, &mut v, "go");
        assert_eq!(names(&mut v), ["Goal.md", "gold.txt"], "lower case matches either case");
        type_in(&mut h, &mut v, "Go");
        assert_eq!(names(&mut v), ["Goal.md"], "a capital matches case");
    }

    #[test]
    fn the_more_menu_lists_the_listing_options_checked_and_choosing_one_toggles_it() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let more = v.more_hit.expect("a ⋯ button");
        let menu = |h: &Harness, v: &mut FileBrowserView| match h.button(v, BTN_MORE).requests.as_slice() {
            [Request::Menu { items, at, align }] => {
                assert_eq!((*at, *align), ((more.x + more.w, more.y + more.h), MenuAlign::Right), "under ⋯");
                items.clone()
            }
            other => panic!("expected a menu, got {other:?}"),
        };
        let checked = |items: &[MenuItem], id| items.iter().find(|i| i.id == id).and_then(|i| i.checked);
        let items = menu(&h, &mut v);
        let hidden = v.tree().show_hidden();
        assert_eq!(checked(&items, BTN_HIDDEN), Some(hidden));
        assert_eq!(checked(&items, BTN_IGNORED), Some(v.tree().show_gitignored()));
        assert_eq!(checked(&items, BTN_GIT_MODIFIED), Some(false));
        assert_eq!(checked(&items, BTN_GROUP), Some(true), "the tree");
        assert_eq!(checked(&items, BTN_GROUP + 1), Some(false));

        // Choosing an item calls `button` with its id.
        h.button(&mut v, BTN_HIDDEN);
        assert_eq!(v.tree().show_hidden(), !hidden);
        assert_eq!(checked(&menu(&h, &mut v), BTN_HIDDEN), Some(!hidden));
        h.button(&mut v, BTN_GIT_MODIFIED);
        assert_eq!(v.tree().git_filter(), GitFilter::Modified);
        h.button(&mut v, BTN_GIT_UNTRACKED);
        assert_eq!(v.tree().git_filter(), GitFilter::Untracked, "one git filter at a time");
        h.button(&mut v, BTN_GIT_UNTRACKED);
        assert_eq!(v.tree().git_filter(), GitFilter::All, "choosing the checked one turns it off");
        h.button(&mut v, BTN_GROUP + 1);
        assert_eq!(v.tree().view(), FsView::Extension);
        let items = menu(&h, &mut v);
        assert_eq!((checked(&items, BTN_GROUP), checked(&items, BTN_GROUP + 1)), (Some(false), Some(true)));
    }

    #[test]
    fn the_filter_box_shows_a_clear_button_once_it_holds_text() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        let ids = |h: &mut Harness, v: &mut FileBrowserView| -> Vec<u32> {
            h.paint(v, RECT).view_buttons.iter().map(|(_, _, id)| *id).collect()
        };
        assert!(!ids(&mut h, &mut v).contains(&BTN_CLEAR));
        h.key(&mut v, press(Key::Char('b')));
        assert!(ids(&mut h, &mut v).contains(&BTN_CLEAR));
        let clear = h.paint(&mut v, RECT).view_buttons.iter().find(|b| b.2 == BTN_CLEAR).unwrap().0;
        let text = v.field.geom().unwrap().rect;
        assert!(clear.x >= text.x + text.w && clear.x + clear.w <= RECT.w, "at the box's right end: {clear:?}");
        let out = h.button(&mut v, BTN_CLEAR);
        assert!(out.redraw);
        assert_eq!((v.tree().query(), v.input.text()), ("", ""));
        assert!(!ids(&mut h, &mut v).contains(&BTN_CLEAR));
        // The focused box is outlined in the focus colour.
        let ring = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, theme::FIELD_FOCUS_BORDER, 1.0).color;
        assert!(h.paint(&mut v, RECT).bg.iter().any(|q| q.border_w > 0.0 && q.border_color == ring));
    }

    #[test]
    fn an_empty_listing_says_why() {
        let (d, mut v) = fixture();
        let mut h = Harness::new();
        assert_eq!(v.empty_note(), None, "rows are listed");
        type_in(&mut h, &mut v, "zzz");
        assert_eq!(v.empty_note().as_deref(), Some("No matching files"));
        std::fs::create_dir(d.join("adir/empty")).unwrap();
        v.tree_mut().set_root(d.join("adir/empty"));
        assert!(v.tree_mut().rows().is_empty());
        assert_eq!(v.empty_note(), None, "an empty folder is just empty");
        type_in(&mut h, &mut v, "   ");
        assert!(v.tree_mut().rows().is_empty());
        assert_eq!(v.empty_note(), None, "a blank query filters nothing");
        // A git filter that passes nothing, once its status is in.
        h.key(&mut v, press(Key::Escape));
        v.tree_mut().set_git_filter(GitFilter::Modified);
        let until = Instant::now() + std::time::Duration::from_secs(10);
        while v.tree().git_status_awaited() {
            assert!(Instant::now() < until, "the git status never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
            v.tree_mut().rows();
        }
        assert_eq!(v.empty_note().as_deref(), Some("No matching files"));
        // A refresh keeps the status shown, so nothing says "loading".
        v.tree_mut().refresh();
        assert_eq!(v.empty_note().as_deref(), Some("No matching files"));

        let mut pick = FileBrowserView::picker(d.clone(), Mode::PickDir);
        type_in(&mut h, &mut pick, "zzz");
        assert_eq!(pick.empty_note().as_deref(), Some("No matching folders"));
    }

    #[test]
    fn the_more_button_is_tinted_while_an_option_is_off_its_default() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        let accent = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, h.chrome.accent, 1.0).color;
        let tinted = |h: &mut Harness, v: &mut FileBrowserView| h.paint(v, RECT).bg.iter().any(|q| q.color == accent);
        assert!(!tinted(&mut h, &mut v));
        for id in [BTN_HIDDEN, BTN_IGNORED, BTN_GIT_MODIFIED, BTN_GROUP + 1] {
            h.button(&mut v, id);
            assert!(tinted(&mut h, &mut v), "after {id}");
            h.button(&mut v, if id == BTN_GROUP + 1 { BTN_GROUP } else { id });
            assert!(!tinted(&mut h, &mut v), "back to the default after {id}");
        }
    }

    #[test]
    fn the_browser_shows_where_it_is_once_it_leaves_where_it_opened() {
        let (d, _) = fixture();
        let mut v = FileBrowserView::new(d.join("adir"));
        let mut h = Harness::new();
        assert_eq!(v.hint(), "Filter files");
        h.button(&mut v, BTN_PARENT);
        let parent = d.as_path();
        assert_eq!(v.hint(), tilde(parent));
        // The up button's tooltip names the folder shown.
        h.paint(&mut v, RECT);
        let up = h.frame.view_buttons.iter().find(|b| b.2 == BTN_PARENT).unwrap().0;
        h.cursor = (up.x + 2.0, up.y + 2.0);
        let tip = h.paint(&mut v, RECT).tooltip.clone().expect("a tooltip").1;
        assert_eq!(tip, format!("Up from {}", tilde(parent)));
        assert_eq!(tilde(&PathBuf::from("/nowhere/x")), "/nowhere/x");
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(tilde(&PathBuf::from(&home).join("a")), "~/a");
        }
    }
}
