//! The file browser plugin: a navigable, filterable tree over
//! [`FsTree`](ghostrealm_core::fs_tree::FsTree). The same view backs the
//! floating picker, which chooses a directory, a file, or where to save (see
//! [`Mode`]).
//!
//! Typing filters (fuzzy across the tree) or, when the input looks like a path,
//! completes it; Up/Down move the single highlight, which the mouse also takes
//! over on hover. A click also *selects* the row: a separate, sticky highlight
//! that hover and arrows leave alone. In the picker, the selected entry is the
//! one confirming chooses.
//!
//! A wide enough browser shows a [`preview`] of the highlighted (else selected)
//! entry beside the list; the header's `preview` toggle overrides the width rule.

pub(crate) mod preview;

use std::any::Any;
use std::borrow::Cow;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use ghostrealm_core::config::{Kind, OptionSpec, SectionSpec};
use ghostrealm_core::fs_tree::{FileRow, FsTree, FsView, GitFilter};
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Config, Rect, Registry};
use ghostrealm_terminal::{Key, KeyPress};
use glyphon::Family;

use crate::app_state::AppState;
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::plugin::text_edit::{Applied, LineEdit};
use crate::plugin::{
    rect_contains, theme, widgets, widgets::FieldMouse, EventCx, MouseEvent, OpenCx, PaintCx, Plugin, Request, View,
    Waker,
};
use preview::PreviewPane;

pub const ID: &str = "file_browser";

/// Row colours: a file, a hidden (dot) file, a file's extension, and the
/// fuzzy-match highlight. Directories use the theme accent.
const FILE_FG: [u8; 3] = [230, 230, 235];
const HIDDEN_FG: [u8; 3] = [130, 130, 140];
const EXT_FG: [u8; 3] = [212, 170, 90];
const MATCH_FG: [u8; 3] = [245, 225, 90];
/// Opacity of the accent fill behind the selected (clicked) row.
const SELECTED_ALPHA: f32 = 0.3;
/// How much of the highlight a directory picker's hover draws: weaker than
/// the keyboard's, which is the answer.
const HOVER_SHARE: f32 = 0.5;

/// Header button ids (see [`View::button`]).
const BTN_PARENT: u32 = 0;
const BTN_HIDDEN: u32 = 1;
const BTN_IGNORED: u32 = 2;
const BTN_GIT: u32 = 3;
const BTN_VIEW: u32 = 4;
const BTN_PREVIEW: u32 = 5;

/// The preview's share of the browser's width.
const PREVIEW_SHARE: f32 = 0.45;

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
        Ok(Box::new(FileBrowserView::new(root).with_waker(cx.waker.clone())))
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

    fn config_sections(&self) -> Vec<SectionSpec> {
        const OPEN_IN: &[(&str, &str)] = &[
            (
                "split",
                "A horizontal split beside the file browser (a tab when the pane is too narrow).",
            ),
            ("tab", "A tab in the file browser's own pane."),
        ];
        vec![SectionSpec::options(
            "file_browser",
            "The file browser.",
            vec![
                OptionSpec::new(
                    "open_in",
                    Kind::Enum(OPEN_IN),
                    Some("\"split\""),
                    "Where opening a file puts it.",
                ),
                OptionSpec::new(
                    "preview_min_width",
                    Kind::Int { min: 0, max: 10_000 },
                    Some("900"),
                    "Show the preview (details, text, images, audio) beside the list once the \
                     browser is at least this many logical px wide. The header's `preview` toggle \
                     overrides it per pane.",
                ),
                OptionSpec::new(
                    "preview_autoplay",
                    Kind::Bool,
                    Some("true"),
                    "Play audio once the preview has shown it for a moment (toggle per pane with \
                     the preview's `autoplay` button).",
                ),
            ],
        )]
    }
}

/// What the browser is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The file browser pane: opening a file requests it.
    Browse,
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
    rows: Vec<RowRef>,
    /// Path-mode completions as drawn: (rect, index, path, is_dir).
    suggest_hits: Vec<(Rect, usize, PathBuf, bool)>,
    preview: PreviewPane,
    /// This view's preview override; `None` shows it when the browser is at least
    /// `[file_browser] preview_min_width` wide.
    preview_on: Option<bool>,
    /// The last paint showed the preview.
    preview_shown: bool,
    /// Scroll the chosen entry into view at the next paint.
    reveal_chosen: bool,
}

impl FileBrowserView {
    pub fn new(root: PathBuf) -> Self {
        FileBrowserView {
            tree: FsTree::new(root),
            mode: Mode::Browse,
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
            rows: Vec::new(),
            suggest_hits: Vec::new(),
            preview: PreviewPane::default(),
            preview_on: None,
            preview_shown: false,
            reveal_chosen: false,
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
    /// the root, else make its folder the root.
    pub fn reveal(&mut self, path: &Path) {
        self.tree.set_query("");
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
            Mode::Browse => None,
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
                .filter(|p| p.is_dir())
                .map(|p| lexical(&p));
        }
        if let Some(r) = self.selected_row().filter(|r| r.is_dir && !r.is_category) {
            return Some(r.path);
        }
        if let Some(dir) = self.chosen.clone().filter(|p| p.is_dir()) {
            return Some(dir);
        }
        if query.is_empty() {
            return Some(self.tree.root().to_path_buf());
        }
        self.tree.rows().iter().find(|r| r.is_dir && !r.is_category).map(|r| r.path.clone())
    }

    /// [`Mode::PickDir`]'s highlighted row, to find again once the listing
    /// changes (other modes keep the index, as they always have).
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

    /// Open (browse) or pick (file picker) `path`; saving picks the choice.
    fn open_file(&mut self, cx: &mut EventCx, path: PathBuf) {
        match self.mode {
            Mode::Browse => cx.request(Request::OpenFile(path)),
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
        let scale = cx.ui.scale;
        let ch = cx.ui.cell_h;
        let pad = 8.0 * scale;
        let h = ch + 10.0 * scale;
        let row = Rect { x: rect.x, y: rect.y + rect.h - h, w: rect.w, h };
        cx.fill(row, cx.chrome.sidebar);
        let label = "Name";
        let label_w = label.chars().count() as f32 * cx.ui.cell_w;
        cx.label(label, Family::SansSerif, row.x + pad, row.y + (h - ch) * 0.5, row, theme::LABEL);
        let inset = 4.0 * scale;
        let x = row.x + label_w + pad * 2.0;
        let field = Rect { x, y: row.y + inset, w: (row.x + row.w - pad - x).max(1.0), h: h - inset * 2.0 };
        let geom = widgets::text_field(cx, field, &self.name, "file name", true);
        self.name_field.painted(cx, geom);
        h
    }

    /// Preview `subject` in `rect`; with no rect, the preview is hidden and
    /// lets go of what it showed (stopping its audio).
    fn paint_preview(&mut self, cx: &mut PaintCx, rect: Option<Rect>, subject: Option<&Path>) {
        match rect {
            Some(r) => {
                self.preview.show(subject);
                self.preview.paint(cx, r);
            }
            None => self.preview.show(None),
        }
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
    fn paint_header(&mut self, cx: &mut PaintCx, rect: Rect, value: &LineEdit, editable: bool) -> f32 {
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
        // An ellipsis on the git controls while their status loads.
        let loading = |label: &'static str, git: bool| -> Cow<'static, str> {
            if git && t.git_status_loading() {
                format!("{label}\u{2026}").into()
            } else {
                label.into()
            }
        };
        let git_on = t.git_filter() != GitFilter::All;
        let toggles = [
            ("preview".into(), self.preview_shown, BTN_PREVIEW),
            ("hidden".into(), t.show_hidden(), BTN_HIDDEN),
            (".gitignore".into(), t.show_gitignored(), BTN_IGNORED),
            (loading(t.git_filter().label(), git_on), git_on, BTN_GIT),
            (loading(t.view().label(), t.view() == FsView::GitStatus), t.view() != FsView::Tree, BTN_VIEW),
        ];
        // The git filter, grouped views, and preview act on files, which a
        // directory picker never lists.
        let file_only = |id: u32| id == BTN_GIT || id == BTN_VIEW || id == BTN_PREVIEW;
        let dirs_only = self.mode == Mode::PickDir;
        // Each toggle's hit area is its label plus `tpad` on both sides; the
        // areas tile, so the padding is also the gap between labels.
        let tpad = 5.0 * scale;
        let mut right = rect.x + rect.w - pad;
        for (label, on, id) in toggles.into_iter().filter(|t| !(dirs_only && file_only(t.2))) {
            let w = label.chars().count() as f32 * cx.ui.cell_w;
            let hit = Rect { x: right - w - tpad, y: rect.y, w: w + tpad * 2.0, h: header_h };
            if cx.button(hit, id) {
                cx.highlight(hit);
            }
            let color = if on { theme::LABEL_HOVER } else { theme::DIM };
            cx.label(&label, Family::SansSerif, hit.x + tpad * 0.5, hy, hit, color);
            right -= w + tpad * 2.0;
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
        // Saving types into the name field instead.
        let geom = widgets::text_field(cx, field, value, &root, self.mode != Mode::Save);
        if editable {
            self.field.painted(cx, geom);
        } else {
            self.field.clear();
        }
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
        let min_w = cx.cfg.file_browser.preview_min_width as f32 * scale;
        self.preview_shown = self.mode != Mode::PickDir && self.preview_on.unwrap_or(rect.w >= min_w);
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
        // The list on the left, the preview (when shown) on the right.
        let (list, preview_rect) = if self.preview_shown {
            let pw = (body.w * PREVIEW_SHARE).round();
            let pane = Rect { x: body.x + body.w - pw, w: pw, ..body };
            (Rect { w: body.w - pw, ..body }, Some(pane))
        } else {
            (body, None)
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
                } else if Some(i) == self.hover {
                    cx.fill_alpha(row_rect, theme::HOVER_BG, theme::HOVER_ALPHA * HOVER_SHARE);
                }
                let color = if is_dir { accent } else { FILE_FG };
                cx.label(&label, Family::Monospace, list.x + pad, ry + (row_h - ch) * 0.5, list, color);
                self.suggest_hits.push((row_rect, i, p.clone(), is_dir));
            }
            let subject = self.completion.and_then(|i| suggestions.get(i));
            self.paint_preview(cx, preview_rect, subject.map(PathBuf::as_path));
            return;
        }

        // Browse/filter mode: the scrollable tree (or results) list.
        let rows: Vec<FileRow> = self.tree.rows().to_vec();
        if let Some(c) = &self.chosen {
            if !rows.iter().any(|r| !r.is_category && &r.path == c) {
                self.chosen = None;
            }
        }
        if std::mem::take(&mut self.reveal_chosen) {
            if let Some(i) = rows.iter().position(|r| !r.is_category && Some(&r.path) == self.chosen.as_ref()) {
                self.scroll = (i as f32 * row_h - (list.h - row_h) * 0.5).max(0.0);
            }
        }
        let max_scroll = (rows.len() as f32 * row_h - list.h).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max_scroll);
        let filtering = !query.is_empty();
        let indent_w = cx.ui.cell_w * 2.0;
        for (i, r) in rows.iter().enumerate() {
            self.rows.push(RowRef::from(r));
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
            } else if Some(i) == self.hover {
                cx.fill_alpha(row_rect, theme::HOVER_BG, theme::HOVER_ALPHA * HOVER_SHARE);
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
        let subject = self
            .sel
            .and_then(|i| rows.get(i))
            .filter(|r| !r.is_category)
            .map(|r| r.path.clone())
            .or_else(|| self.chosen.clone());
        self.paint_preview(cx, preview_rect, subject.as_deref());
    }

    /// Up/Down (Tab: completions) move the highlight, Enter opens or enters it,
    /// Escape clears the input, and other keys edit the input (see
    /// [`crate::plugin::text_edit`]). Plain ←/→ expand/collapse the highlighted
    /// row while the input is empty, and → at the end of a path accepts the
    /// highlighted completion. A directory picker differs (see [`Mode::PickDir`]):
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
                if self.preview_shown && self.preview.contains((x, y)) {
                    self.preview.mouse_down(cx, (x, y));
                    return;
                }
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
                    if self.mode == Mode::Browse || again {
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
        if self.mode != Mode::Browse || !rect_contains(self.list, pos.0, pos.1) {
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
        cx.request(Request::FileMenu { target, at: pos });
        cx.redraw();
    }

    /// The selected entry.
    fn subject(&self) -> Option<PathBuf> {
        self.chosen.clone().filter(|_| self.mode == Mode::Browse)
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
        let sel = self.sel_path().map(|p| follow(&p).unwrap_or(p));
        self.tree.refresh();
        self.reselect(sel);
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), _dx: f32, dy: f32) {
        if self.preview_shown && self.preview.contains(pos) {
            self.preview.scroll(cx, dy);
            return;
        }
        // Pixel-precise; the upper bound is clamped to the content at paint.
        let next = (self.scroll - dy).max(0.0);
        if next != self.scroll {
            self.scroll = next;
            cx.request_frame();
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        if self.preview.button(cx, id) {
            return;
        }
        let sel = self.sel_path();
        let t = &mut self.tree;
        match id {
            BTN_PREVIEW => self.preview_on = Some(!self.preview_shown),
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
        self.reselect(sel);
        cx.redraw();
    }

    fn refresh(&mut self) -> bool {
        self.tree.poll()
    }

    fn deadline(&self, _cfg: &Config) -> Option<Instant> {
        self.preview.deadline()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        self.preview.tick(cx, now);
    }

    fn shown(&mut self, visible: bool) {
        self.preview.set_visible(visible);
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
    use crate::plugin::MouseButton;
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
        let frame = h.paint(&mut v, RECT);
        let ids: Vec<u32> = frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        assert!(
            !ids.contains(&BTN_GIT) && !ids.contains(&BTN_VIEW) && !ids.contains(&BTN_PREVIEW),
            "no file-only toggles"
        );
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
        // Tab still cycles completions; Enter then takes the highlighted one.
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

    /// The list rows a frame fills with the hover colour at `alpha`.
    fn rows_lit(frame: &crate::plugin::paint::Frame, alpha: f32) -> Vec<usize> {
        let (header_h, row_h) = (UI.cell_h + 10.0, UI.cell_h + 4.0);
        let white = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, theme::HOVER_BG, alpha).color;
        frame
            .bg
            .iter()
            .filter(|q| q.color == white)
            .map(|q| (1.0 - q.pos[1]) * 0.5 * frame.screen.1)
            .filter(|y| *y > header_h - 1.0)
            .map(|y| ((y - header_h) / row_h).round() as usize)
            .collect()
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
        assert_eq!(v.sel, Some(1), "a header toggle");
        h.button(&mut v, BTN_VIEW);
        assert_eq!(v.sel, Some(1), "grouped, with category rows");
        h.button(&mut v, BTN_VIEW);
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
            [Request::FileMenu { target: target(d.join("adir"), TargetKind::Dir, ()), at }]
        );
        assert_eq!(v.subject(), Some(d.join("adir")), "the row is selected");
        let at = row_centre(1);
        assert_eq!(
            menu(&h, &mut v, at),
            [Request::FileMenu { target: target(d.join("b.txt"), TargetKind::File, ()), at }]
        );
        let at = (50.0, 250.0);
        assert_eq!(
            menu(&h, &mut v, at),
            [Request::FileMenu { target: target(d.clone(), TargetKind::Empty, ()), at }],
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
        h.button(&mut v, BTN_GIT);
        let before = v.text().unwrap();
        assert!(!before.contains("changed.txt"), "no status yet, so nothing passes:\n{before}");
        assert!(v.tree().git_status_loading());
        woken.recv_timeout(std::time::Duration::from_secs(10)).expect("the worker wakes the UI");
        assert!(v.refresh(), "the arrived status repaints");
        let after = v.text().unwrap();
        assert!(after.contains("changed.txt") && !after.contains("clean.txt"), "{after}");
    }

    /// Wider than the default `preview_min_width` (900).
    const WIDE: Rect = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 300.0 };

    /// Tick `v` until its preview shows `path` (the loader runs on a thread).
    fn await_preview(h: &Harness, v: &mut FileBrowserView, path: &Path) {
        let until = Instant::now() + std::time::Duration::from_secs(5);
        while v.preview.shown().map(|p| p.path.as_path()) != Some(path) {
            assert!(Instant::now() < until, "preview of {path:?} never loaded");
            std::thread::sleep(std::time::Duration::from_millis(5));
            if v.deadline(&h.cfg).is_some_and(|d| Instant::now() >= d) {
                h.tick(v, Instant::now());
            }
        }
    }

    #[test]
    fn preview_shows_when_wide_and_the_toggle_overrides() {
        let (_d, mut v) = fixture();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(!v.preview_shown, "narrower than preview_min_width");
        h.button(&mut v, BTN_PREVIEW);
        h.paint(&mut v, RECT);
        assert!(v.preview_shown, "the toggle forces it on");

        let (_d, mut v) = fixture();
        h.paint(&mut v, WIDE);
        assert!(v.preview_shown, "wide enough");
        h.button(&mut v, BTN_PREVIEW);
        h.paint(&mut v, WIDE);
        assert!(!v.preview_shown, "the toggle forces it off");
        assert!(v.row_at(900.0, row_centre(0).1).is_some(), "the list takes the full width again");
    }

    #[test]
    fn preview_follows_the_highlight_and_owns_its_scroll() {
        let (d, mut v) = fixture();
        let lines: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.join("b.txt"), lines).unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        h.paint(&mut v, WIDE);
        await_preview(&h, &mut v, &d.join("b.txt"));
        assert!(matches!(v.preview.shown().unwrap().content, preview::Content::Text { .. }));

        // Over the preview (the right of the browser), the wheel scrolls it, not the list.
        h.paint(&mut v, WIDE);
        let out = h.scroll(&mut v, (900.0, 200.0), -40.0);
        assert!(out.frame);
        assert_eq!(v.scroll, 0.0, "the list didn't move");
        // Moving onto the preview keeps the highlight (and so the preview).
        h.mouse(&mut v, MouseEvent::Move { pos: (900.0, row_centre(0).1) });
        assert_eq!(v.sel, Some(1));
    }

    #[test]
    fn preview_paints_highlighted_text_rows() {
        let (d, mut v) = fixture();
        std::fs::write(d.join("b.md"), "# Title\n\n**bold** and `code`\n").unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(1) });
        h.paint(&mut v, WIDE);
        await_preview(&h, &mut v, &d.join("b.md"));
        let plain = h.paint(&mut v, WIDE).text.len();
        let preview::Content::Text { spans, .. } = &v.preview.shown().unwrap().content else { panic!("text") };
        assert_eq!(spans.len(), 3);
        assert!(plain >= 3, "the lines are drawn, got {plain} text items");
    }

    #[test]
    fn preview_draws_an_image() {
        let (d, mut v) = fixture();
        std::fs::write(d.join("c.png"), crate::media::testing::png(4, 2, [0, 0, 255, 255])).unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(2) });
        h.paint(&mut v, WIDE);
        await_preview(&h, &mut v, &d.join("c.png"));
        let frame = h.paint(&mut v, WIDE);
        assert_eq!(frame.images.len(), 1);
        let r = frame.images[0].rect;
        assert!(r.x >= WIDE.w * (1.0 - PREVIEW_SHARE), "inside the preview, got {r:?}");
        assert_eq!((r.w, r.h), (4.0, 2.0), "never upscaled past a pixel per logical px");
    }

    #[test]
    fn audio_preview_offers_play_without_autoplaying_when_off() {
        let (d, mut v) = fixture();
        std::fs::write(d.join("c.wav"), crate::media::testing::wav(8000, 800)).unwrap();
        let mut h = Harness::new();
        h.cfg.file_browser.preview_autoplay = false;
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(2) });
        h.paint(&mut v, WIDE);
        await_preview(&h, &mut v, &d.join("c.wav"));
        let frame = h.paint(&mut v, WIDE);
        let ids: Vec<u32> = frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        assert!(ids.contains(&preview::BTN_PLAY) && ids.contains(&preview::BTN_AUTOPLAY));
        assert_eq!(v.deadline(&h.cfg), None, "nothing scheduled: no autoplay");
    }

    /// A browser with the audio file `c.wav` previewed and autoplay pending.
    fn previewing_audio_with_autoplay() -> (Harness, FileBrowserView, PathBuf) {
        let (d, mut v) = fixture();
        std::fs::write(d.join("c.wav"), crate::media::testing::wav(8000, 800)).unwrap();
        let mut h = Harness::new();
        h.cfg.file_browser.preview_autoplay = true;
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(2) });
        h.paint(&mut v, WIDE);
        await_preview(&h, &mut v, &d.join("c.wav"));
        (h, v, d.join("c.wav"))
    }

    #[test]
    fn hiding_the_view_cancels_a_pending_autoplay_and_showing_it_does_not_restore_it() {
        let (h, mut v, _) = previewing_audio_with_autoplay();
        assert!(v.deadline(&h.cfg).is_some(), "autoplay is pending");
        v.shown(false);
        assert_eq!(v.deadline(&h.cfg), None, "nothing scheduled while hidden");
        v.shown(true);
        assert_eq!(v.deadline(&h.cfg), None, "not re-armed on return");
    }

    #[test]
    fn audio_that_finishes_loading_in_a_hidden_view_does_not_autoplay() {
        let (d, mut v) = fixture();
        std::fs::write(d.join("c.wav"), crate::media::testing::wav(8000, 800)).unwrap();
        let mut h = Harness::new();
        h.cfg.file_browser.preview_autoplay = true;
        h.paint(&mut v, WIDE);
        h.mouse(&mut v, MouseEvent::Move { pos: row_centre(2) });
        h.paint(&mut v, WIDE);
        v.shown(false);
        await_preview(&h, &mut v, &d.join("c.wav"));
        assert_eq!(v.deadline(&h.cfg), None, "no autoplay armed while hidden");
    }

    #[test]
    fn hiding_the_view_pauses_a_playing_track_and_showing_it_leaves_it_paused() {
        let (h, mut v, wav) = previewing_audio_with_autoplay();
        let player = crate::media::audio::AudioPlayer::new();
        player.play(&wav);
        v.preview.install_player(player);
        assert!(v.preview.audio_status().is_some_and(|s| s.playing));
        v.shown(false);
        assert!(v.preview.audio_status().is_some_and(|s| !s.playing), "paused on hide");
        v.shown(true);
        assert!(v.preview.audio_status().is_some_and(|s| !s.playing), "not resumed on show");
        assert_eq!(v.deadline(&h.cfg), None);
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
