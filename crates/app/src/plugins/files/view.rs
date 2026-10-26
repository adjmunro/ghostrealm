//! [`FilesView`]: the Files surface itself.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ghostrealm_core::config::SidebarPin;
use ghostrealm_core::{Config, Rect};
use ghostrealm_terminal::{KeyPress, Pumped};
use winit::window::CursorIcon;

use super::tabs::{self, TabInfo, TabRow};
use super::Session;
use crate::file_menu::{FileTarget, Kind as TargetKind, Origin};
use crate::languages::same_file;
use crate::plugin::icons::{FileKind, Icon};
use crate::plugin::{
    rect_contains, theme, DocId, EventCx, MenuAlign, MouseButton, MouseEvent, OpenCx, PaintCx, Request, View, Waker,
};
use crate::plugins::editor::EditorView;
use crate::plugins::file_browser::FileBrowserView;

/// The sidebar button (see [`View::button`]); the file tabs' ids follow
/// from [`tabs::TAB_IDS`].
const BTN_SIDEBAR: u32 = 0;
/// The sidebar's and the documents' ids, past the view's own (see
/// [`PaintCx::with_id_base`]). Only the active document paints, so the
/// documents share one range.
const SIDEBAR_IDS: u32 = 1 << 24;
const DOC_IDS: u32 = 2 << 24;

/// The sidebar's width until one is dragged, and its narrowest (logical px).
const SIDEBAR_W: f32 = 260.0;
const SIDEBAR_MIN_W: f32 = 160.0;
/// The narrowest the document gets from widening the sidebar (logical px).
const DOC_MIN_W: f32 = 240.0;
/// How far either side of the sidebar's edge a press starts a resize
/// (logical px).
const EDGE_GRAB: f32 = 3.0;

/// The part of the surface that takes keys, or that a press went to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Sidebar,
    Doc,
}

/// What has the keys: the sidebar or one document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keys {
    Sidebar,
    Doc(DocId),
}

/// What the held mouse button pressed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Press {
    Part(Part),
    /// The sidebar's right edge, grabbed this far right of it (physical px).
    Edge(f32),
    /// A file tab, with this button: it acts on release over the same tab.
    Tab(DocId, MouseButton),
}

/// An open document and its tab.
struct DocTab {
    id: DocId,
    view: Box<dyn View>,
    /// The preview tab: a single click in the sidebar opened it, and the
    /// next one replaces it. Editing it, or a double click on it or its
    /// file, keeps it.
    ephemeral: bool,
    /// Whether it had unsaved changes when last looked at: once it hasn't,
    /// it was saved (or reloaded), and a search's results may be stale.
    was_modified: bool,
}

/// Where the last paint put things.
#[derive(Clone, Copy, Default)]
struct Layout {
    rect: Option<Rect>,
    sidebar: Option<Rect>,
    /// The resize handle on the sidebar's right edge.
    edge: Option<Rect>,
    doc: Option<Rect>,
}

/// The Files surface: the file sidebar beside its documents, one tab each
/// in the top row. With no document it shows only the sidebar, full width;
/// with one, the sidebar shows while pinned (the button at the top left).
pub struct FilesView {
    sidebar: FileBrowserView,
    /// In tab order.
    docs: Vec<DocTab>,
    /// The index of the document shown (when there is one).
    active: usize,
    /// The next document's id: ids are never reused.
    next_id: u32,
    session: Session,
    /// This surface's pin; `None` follows `[file_browser] sidebar`.
    pinned: Option<bool>,
    /// The sidebar's width (logical px).
    width: f32,
    /// The part the keys go to while it exists.
    focus: Part,
    /// The surface has the keyboard.
    focused: bool,
    /// The surface is on screen.
    visible: bool,
    /// The file a click in the sidebar last opened, and whether with a double
    /// click. A single click opens it in the preview tab; either way it shows
    /// with the keyboard left in the sidebar, so browsing by clicks goes on,
    /// while the sidebar shows beside it. Any other open gets a tab of its
    /// own and moves the keyboard to the document.
    clicked: Option<(PathBuf, bool)>,
    /// Whether the config pins the sidebar, as of the last paint (for a pin
    /// that follows it, between paints).
    cfg_pinned: bool,
    /// What lost the keys where no event context was at hand, in order,
    /// and when the first did: each that hasn't them back, and what has them
    /// now, hear so on the next tick.
    unfocused: Option<(Instant, Vec<Keys>)>,
    press: Option<Press>,
    /// The part under the pointer, to tell when it leaves.
    hover: Option<Part>,
    /// The file tab under the pointer: its look follows the pointer.
    hover_tab: Option<DocId>,
    layout: Layout,
    tabs: TabRow,
}

impl FilesView {
    /// A Files surface showing only its sidebar, at `root`.
    pub fn new(root: PathBuf, cx: &OpenCx) -> Self {
        Self::with(root, cx.waker.clone(), cx.files.clone())
    }

    fn with(root: PathBuf, waker: Option<Waker>, session: Session) -> Self {
        FilesView {
            sidebar: FileBrowserView::new(root).with_waker(waker),
            docs: Vec::new(),
            active: 0,
            next_id: 1,
            pinned: session.pinned(),
            width: session.width().unwrap_or(SIDEBAR_W),
            session,
            focus: Part::Sidebar,
            focused: false,
            visible: false,
            clicked: None,
            cfg_pinned: true,
            unfocused: None,
            press: None,
            hover: None,
            hover_tab: None,
            layout: Layout::default(),
            tabs: TabRow::default(),
        }
    }

    /// Show `view`, a document of a file no tab shows (see
    /// [`Self::show_path`]), and return its id. Opened by a single click in
    /// the sidebar it shows in the preview tab, replacing the document there
    /// (unmodified: an edit keeps a preview tab); otherwise in a new tab
    /// right of the active one. The keyboard moves to it unless a click in
    /// the shown sidebar opened it.
    pub fn show_doc(&mut self, mut view: Box<dyn View>) -> DocId {
        self.settle();
        if let Some(editor) = view.downcast_mut::<EditorView>() {
            editor.set_header(true);
        }
        let path = view.path();
        let clicked = self.take_click(path.as_deref());
        let preview = clicked == Some(false);
        let id = DocId(self.next_id);
        self.next_id += 1;
        let before = self.keys();
        let was_modified = view.modified();
        let tab = DocTab { id, view, ephemeral: preview, was_modified };
        let at = match self.docs.iter().position(|t| t.ephemeral).filter(|_| preview) {
            Some(i) => {
                self.docs[i] = tab;
                i
            }
            None => {
                let at = if self.docs.is_empty() { 0 } else { self.active + 1 };
                self.docs.insert(at, tab);
                at
            }
        };
        // The replaced document, or the one shown before, is off screen now.
        if self.visible {
            if let Some(old) = self.docs.get_mut(self.active).filter(|_| self.active != at) {
                old.view.shown(false);
            }
            self.docs[at].view.shown(true);
        }
        self.active = at;
        self.opened(before, clicked.is_some());
        self.select_match();
        id
    }

    /// Show the tab of `path`, if one shows it, for an open of it: a single
    /// click in the sidebar leaves a preview tab one, anything else keeps
    /// it. Returns whether a tab shows it.
    pub fn show_path(&mut self, path: &Path) -> bool {
        self.settle();
        let Some(i) = self.tab_of(path) else { return false };
        let clicked = self.take_click(Some(path));
        if clicked != Some(false) {
            self.docs[i].ephemeral = false;
        }
        let before = self.keys();
        self.set_active(i);
        self.opened(before, clicked.is_some());
        self.select_match();
        true
    }

    /// Whether a tab shows `path`.
    pub fn has_path(&self, path: &Path) -> bool {
        self.tab_of(path).is_some()
    }

    fn tab_of(&self, path: &Path) -> Option<usize> {
        self.docs.iter().position(|t| t.view.path().is_some_and(|p| same_file(&p, path)))
    }

    /// Whether the last sidebar click opened `path`: `Some(double)` if so.
    /// Spent either way, so a later open can't take it.
    fn take_click(&mut self, path: Option<&Path>) -> Option<bool> {
        let (clicked, double) = self.clicked.take()?;
        (path == Some(clicked.as_path())).then_some(double)
    }

    /// After an open showed a document: the keyboard goes to it unless a
    /// sidebar click (`by_click`) opened it beside the sidebar, and the
    /// sidebar selects its file.
    fn opened(&mut self, before: Keys, by_click: bool) {
        if !(by_click && self.pinned.unwrap_or(self.cfg_pinned)) {
            self.focus = Part::Doc;
        }
        if self.keys() != before {
            self.queue_unfocus(before);
        }
        self.reveal_active();
    }

    /// Show tab `i`, telling the documents that went off and on screen.
    fn set_active(&mut self, i: usize) {
        if i == self.active || i >= self.docs.len() {
            self.active = i.min(self.docs.len().saturating_sub(1));
            return;
        }
        if self.visible {
            if let Some(old) = self.docs.get_mut(self.active) {
                old.view.shown(false);
            }
            self.docs[i].view.shown(true);
        }
        self.active = i;
        self.select_active();
    }

    /// After a tab switch: select the shown document's file in the sidebar
    /// if it lists it now (no filter typed), changing nothing else there,
    /// keys included ([`FileBrowserView::select_listed`]).
    fn select_active(&mut self) {
        if let Some(p) = self.docs.get(self.active).and_then(|t| t.view.path()) {
            self.sidebar.select_listed(&p);
        }
    }

    /// After an open: show the shown document's file in the sidebar,
    /// expanding folders or re-rooting to it ([`FileBrowserView::reveal`]).
    fn reveal_active(&mut self) {
        let path = self.docs.get(self.active).and_then(|t| t.view.path());
        if let Some(p) = path.filter(|p| self.sidebar.chosen() != Some(p.as_path())) {
            self.sidebar.reveal(&p);
        }
    }

    /// After an open: when the sidebar's search opened the shown file at a
    /// match, select the match there. Spent either way. The document may
    /// differ from the file as searched (unsaved edits, or a save since), so
    /// the match is found again on its line: the one at the searched
    /// columns, else the line's first, else the caret goes to the line's
    /// start.
    fn select_match(&mut self) {
        let Some(m) = self.sidebar.take_match() else { return };
        let Some(t) = self.docs.get_mut(self.active) else { return };
        if !t.view.path().is_some_and(|p| same_file(&p, &m.path)) {
            return;
        }
        let Some(editor) = t.view.downcast_mut::<EditorView>() else { return };
        let cols = m.cols_in(editor.buffer().lines.get(m.line).map_or("", String::as_str));
        editor.reveal(m.line, cols);
    }

    /// Keep every preview tab whose document has been edited, and search
    /// the sidebar's results again once a document saves.
    fn settle(&mut self) {
        let mut saved = false;
        for t in &mut self.docs {
            let modified = t.view.modified();
            if t.ephemeral && modified {
                t.ephemeral = false;
            }
            saved |= t.was_modified && !modified;
            t.was_modified = modified;
        }
        if saved {
            self.sidebar.rerun_search();
        }
    }

    /// Show the next tab (`step` 1) or the previous one (-1), wrapping, with
    /// the keyboard on it.
    pub fn cycle_tab(&mut self, step: isize) {
        let n = self.docs.len() as isize;
        if n == 0 {
            return;
        }
        let before = self.keys();
        self.set_active((self.active as isize + step).rem_euclid(n) as usize);
        self.focus = Part::Doc;
        if self.keys() != before {
            self.queue_unfocus(before);
        }
    }

    /// Tell what lost the keys (`lost`) on the next tick, and what has them
    /// now that it gained them: done where no event context is at hand.
    /// Only while the surface has the keyboard.
    /// Every loss until then is kept: two tab switches before a tick still
    /// tell the document the keys first left.
    fn queue_unfocus(&mut self, lost: Keys) {
        if self.focused {
            let (_, all) = self.unfocused.get_or_insert_with(|| (Instant::now(), Vec::new()));
            if !all.contains(&lost) {
                all.push(lost);
            }
        }
    }

    /// Tell what lost the keys while queued (and hasn't them back) that it
    /// lost them; with `gained`, what has them now that it gained them.
    fn tell_queued(&mut self, cx: &mut EventCx, gained: bool) {
        let Some((_, lost)) = self.unfocused.take() else { return };
        let now = self.keys();
        let moved = lost.iter().any(|k| *k != now);
        for k in lost.into_iter().filter(|k| *k != now) {
            self.tell(cx, k, false);
        }
        if gained && moved {
            self.tell(cx, now, true);
        }
    }

    /// The document shown, if any.
    pub fn doc_view(&self) -> Option<&dyn View> {
        self.docs.get(self.active).map(|t| &*t.view)
    }

    /// Whether the document `id` is in the preview tab.
    pub fn is_preview(&self, id: DocId) -> bool {
        self.docs.iter().any(|t| t.id == id && t.ephemeral)
    }

    pub fn sidebar(&self) -> &FileBrowserView {
        &self.sidebar
    }

    pub fn sidebar_mut(&mut self) -> &mut FileBrowserView {
        &mut self.sidebar
    }

    /// Whether the sidebar shows beside a document.
    pub fn pinned(&self, cfg: &Config) -> bool {
        self.pinned.unwrap_or(cfg.file_browser.sidebar == SidebarPin::Pinned)
    }

    /// Whether the keys go to the sidebar.
    pub fn sidebar_has_keys(&self) -> bool {
        self.keys() == Keys::Sidebar
    }

    /// Move the keyboard to the sidebar, showing it (and recording the
    /// pin, as the sidebar button does).
    pub fn focus_sidebar(&mut self) {
        self.pinned = Some(true);
        self.session.set_pinned(true);
        let before = self.keys();
        self.focus = Part::Sidebar;
        if before != Keys::Sidebar {
            self.queue_unfocus(before);
        }
    }

    /// The sidebar's width now (logical px).
    pub fn sidebar_width(&self) -> f32 {
        self.width
    }

    fn keys(&self) -> Keys {
        match (self.focus, self.docs.get(self.active)) {
            (Part::Doc, Some(t)) => Keys::Doc(t.id),
            _ => Keys::Sidebar,
        }
    }

    fn part_with_keys(&self) -> Part {
        match self.keys() {
            Keys::Sidebar => Part::Sidebar,
            Keys::Doc(_) => Part::Doc,
        }
    }

    /// Give the keys to `part`, telling what loses and gains them.
    fn set_focus(&mut self, cx: &mut EventCx, part: Part) {
        let before = self.keys();
        self.focus = part;
        self.keys_moved(cx, before);
    }

    /// The keys moved from `before` to what has them now: tell both.
    fn keys_moved(&mut self, cx: &mut EventCx, before: Keys) {
        let after = self.keys();
        if before != after && self.focused {
            self.tell(cx, before, false);
            self.tell(cx, after, true);
        }
    }

    /// Tell `keys` (if it is still there) that it gained or lost the keys.
    fn tell(&mut self, cx: &mut EventCx, keys: Keys, on: bool) {
        match keys {
            Keys::Sidebar => cx.with_id_base(SIDEBAR_IDS, |cx| self.sidebar.focus_changed(cx, on)),
            Keys::Doc(id) => {
                if let Some(t) = self.docs.iter_mut().find(|t| t.id == id) {
                    cx.with_doc(t.id, |cx| cx.with_id_base(DOC_IDS, |cx| t.view.focus_changed(cx, on)));
                }
            }
        }
    }

    /// Run `f` on `part` (the shown document for [`Part::Doc`]) with its ids.
    fn on_part<T>(&mut self, cx: &mut EventCx, part: Part, f: impl FnOnce(&mut dyn View, &mut EventCx) -> T) -> Option<T> {
        match part {
            Part::Sidebar => Some(cx.with_id_base(SIDEBAR_IDS, |cx| f(&mut self.sidebar, cx))),
            Part::Doc => {
                let t = self.docs.get_mut(self.active)?;
                Some(cx.with_doc(t.id, |cx| cx.with_id_base(DOC_IDS, |cx| f(&mut *t.view, cx))))
            }
        }
    }

    /// The part at `pos`, as last painted.
    fn part_at(&self, pos: (f32, f32)) -> Option<Part> {
        let inside = |r: Option<Rect>| r.is_some_and(|r| rect_contains(r, pos.0, pos.1));
        if inside(self.layout.sidebar) {
            Some(Part::Sidebar)
        } else if inside(self.layout.doc) {
            Some(Part::Doc)
        } else {
            None
        }
    }

    /// Drop a drag of the sidebar's edge (and its surface-wide cursor).
    /// Returns whether one was running.
    fn end_edge_drag(&mut self) -> bool {
        let dragging = matches!(self.press, Some(Press::Edge(_)));
        if dragging {
            self.press = None;
        }
        dragging
    }

    /// The narrowest and widest the sidebar may be in a surface `w` wide
    /// (logical px).
    fn width_range(w: f32) -> (f32, f32) {
        (SIDEBAR_MIN_W, (w - DOC_MIN_W).max(SIDEBAR_MIN_W))
    }

    /// Move the pointer to `part` (none: off both), telling the one it left.
    fn hover_to(&mut self, cx: &mut EventCx, part: Option<Part>, pos: (f32, f32)) {
        if let Some(old) = self.hover.filter(|h| Some(*h) != part) {
            self.on_part(cx, old, |v, cx| v.mouse(cx, &MouseEvent::Leave { pos }));
        }
        self.hover = part;
    }

    /// Note the file tab under the pointer, repainting when it changes.
    fn hover_tab_to(&mut self, cx: &mut EventCx, tab: Option<DocId>) {
        if tab != self.hover_tab {
            self.hover_tab = tab;
            cx.redraw();
        }
    }

    /// Show tab `id` with the keyboard on it (a click on it).
    fn activate(&mut self, cx: &mut EventCx, id: DocId) {
        let Some(i) = self.docs.iter().position(|t| t.id == id) else { return };
        let before = self.keys();
        self.set_active(i);
        self.focus = Part::Doc;
        self.keys_moved(cx, before);
        cx.redraw();
    }

    /// The slim row above the sidebar and the document: the sidebar button
    /// at its left end, then the file tabs.
    fn paint_top_row(&mut self, cx: &mut PaintCx, row: Rect) {
        let ui = cx.ui;
        cx.fill(row, cx.chrome.sidebar);
        let line = theme::hairline(ui);
        cx.fill(Rect { x: row.x, y: row.y + row.h - line, w: row.w, h: line }, theme::INPUT_BORDER);
        let inset = (3.0 * ui.scale).round();
        let side = row.h - inset * 2.0;
        let b = Rect { x: row.x + inset * 2.0, y: row.y + inset, w: side, h: side };
        // With no document the sidebar shows whatever the pin: the button
        // has nothing to do then.
        let live = !self.docs.is_empty();
        let pinned = live && self.pinned(cx.cfg);
        let hover = live && cx.button(b, BTN_SIDEBAR);
        if pinned {
            cx.round_rect(b, theme::box_radius(ui), cx.chrome.accent, theme::TAB_ACTIVE_ALPHA, None);
        }
        if hover {
            cx.round_rect(b, theme::box_radius(ui), theme::HOVER_BG, theme::HOVER_ALPHA, None);
        }
        if live {
            cx.tooltip(b, if pinned { "Hide the sidebar" } else { "Show the sidebar" });
        }
        let icon_side = (ui.cell_h * 0.8).round();
        let icon = Rect {
            x: b.x + ((b.w - icon_side) * 0.5).round(),
            y: b.y + ((b.h - icon_side) * 0.5).round(),
            w: icon_side,
            h: icon_side,
        };
        let color = match (live, hover, pinned) {
            (false, ..) => theme::DIM,
            (true, true, _) => theme::GLYPH_HOVER,
            (true, false, true) => cx.chrome.accent,
            (true, false, false) => theme::GLYPH,
        };
        Icon::SidebarToggle.paint(cx, icon, color);

        let x = b.x + b.w + inset * 2.0;
        let area = Rect { x, y: row.y, w: (row.x + row.w - inset - x).max(0.0), h: row.h - line };
        let paths: Vec<Option<PathBuf>> = self.docs.iter().map(|t| t.view.path()).collect();
        let titles: Vec<String> = self.docs.iter().map(|t| t.view.title().unwrap_or_default()).collect();
        let dirs: Vec<Option<PathBuf>> = paths.iter().map(|p| Some(p.as_deref()?.parent()?.to_path_buf())).collect();
        let suffixes = tabs::suffixes(&titles, &dirs);
        let infos: Vec<TabInfo> = self
            .docs
            .iter()
            .zip(paths.into_iter().zip(titles).zip(suffixes))
            .map(|(t, ((path, title), suffix))| {
                TabInfo {
                    id: t.id,
                    title,
                    suffix,
                    path: path.as_ref().map(|p| p.display().to_string()),
                    kind: path.as_deref().map_or(FileKind::Text, FileKind::of),
                    preview: t.ephemeral,
                    modified: t.view.modified(),
                    changed: t.view.marked_changed_on_disk(),
                }
            })
            .collect();
        let active = self.docs.get(self.active).map(|t| t.id);
        self.tabs.paint(cx, area, &infos, active);
    }

    /// The top row's height.
    fn row_height(cx: &PaintCx) -> f32 {
        (cx.ui.cell_h + 12.0 * cx.ui.scale).round()
    }
}

impl View for FilesView {
    fn plugin(&self) -> &'static str {
        super::ID
    }

    /// The shown document's title, else "Files".
    fn title(&self) -> Option<String> {
        match self.docs.get(self.active) {
            Some(t) => t.view.title(),
            None => Some("Files".to_string()),
        }
    }

    fn docs(&self) -> Vec<DocId> {
        self.docs.iter().map(|t| t.id).collect()
    }

    fn active_doc(&self) -> DocId {
        self.docs.get(self.active).map_or(DocId::SELF, |t| t.id)
    }

    fn doc(&self, id: DocId) -> Option<&dyn View> {
        self.docs.iter().find(|t| t.id == id).map(|t| &*t.view)
    }

    fn doc_mut(&mut self, id: DocId) -> Option<&mut dyn View> {
        self.docs.iter_mut().find(|t| t.id == id).map(|t| &mut *t.view)
    }

    /// The shown document stays shown when another closes; closing it shows
    /// its right neighbour, else its left. The surface stays, showing its
    /// sidebar alone once the last closes.
    fn close_doc(&mut self, id: DocId) -> bool {
        let Some(i) = self.docs.iter().position(|t| t.id == id) else { return false };
        let before = self.keys();
        self.docs.remove(i);
        if i < self.active {
            self.active -= 1;
        } else if i == self.active {
            self.active = i.min(self.docs.len().saturating_sub(1));
            if let Some(t) = self.docs.get_mut(self.active).filter(|_| self.visible) {
                t.view.shown(true);
            }
            self.select_active();
        }
        if self.docs.is_empty() {
            self.focus = Part::Sidebar;
        }
        if self.keys() != before {
            self.queue_unfocus(before);
        }
        false
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        self.settle();
        self.layout = Layout { rect: Some(rect), ..Layout::default() };
        cx.fill(rect, cx.chrome.background);
        let row_h = Self::row_height(cx).min(rect.h);
        self.paint_top_row(cx, Rect { h: row_h, ..rect });
        let body = Rect { y: rect.y + row_h, h: (rect.h - row_h).max(0.0), ..rect };
        let scale = cx.ui.scale;
        self.cfg_pinned = cx.cfg.file_browser.sidebar == SidebarPin::Pinned;
        let has_doc = !self.docs.is_empty();
        let show_sidebar = !has_doc || self.pinned(cx.cfg);
        // A hidden sidebar can't keep the keys.
        if !show_sidebar && self.focus == Part::Sidebar {
            self.focus = Part::Doc;
            self.queue_unfocus(Keys::Sidebar);
        }
        let (side, doc) = match (has_doc, show_sidebar) {
            (false, _) => (Some(body), None),
            (true, false) => (None, Some(body)),
            (true, true) => {
                let (lo, hi) = Self::width_range(body.w / scale);
                let w = (self.width.clamp(lo, hi) * scale).round().min(body.w);
                let line = theme::hairline(cx.ui);
                let side = Rect { w, ..body };
                cx.fill(Rect { x: body.x + w, y: body.y, w: line, h: body.h }, theme::INPUT_BORDER);
                let grab = (EDGE_GRAB * scale).round();
                let edge = Rect { x: body.x + w - grab, y: body.y, w: grab * 2.0 + line, h: body.h };
                self.layout.edge = Some(edge);
                let doc = Rect { x: body.x + w + line, w: (body.w - w - line).max(0.0), ..body };
                (Some(side), Some(doc))
            }
        };
        let focused = cx.focused;
        let keys = self.part_with_keys();
        if let Some(r) = side {
            cx.focused = focused && keys == Part::Sidebar;
            cx.with_id_base(SIDEBAR_IDS, |cx| cx.with_clip(r, |cx| self.sidebar.paint(cx, r)));
        }
        if let (Some(r), Some(t)) = (doc, self.docs.get_mut(self.active)) {
            cx.focused = focused && keys == Part::Doc;
            cx.with_id_base(DOC_IDS, |cx| t.view.paint(cx, r));
        }
        cx.focused = focused;
        (self.layout.sidebar, self.layout.doc) = (side, doc);
        // After the parts, so the edge's cursor wins over a field's I-beam;
        // while dragging it, over the whole surface.
        let dragging = matches!(self.press, Some(Press::Edge(_)));
        if let Some(edge) = self.layout.edge {
            cx.cursor(if dragging { rect } else { edge }, CursorIcon::ColResize);
        }
    }

    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        let part = self.part_with_keys();
        // Like the click, a match left by an open that never showed is
        // stale once anything else happens.
        self.clicked = None;
        self.sidebar.take_match();
        let used = self.on_part(cx, part, |v, cx| v.key(cx, k)).unwrap_or(false);
        self.settle();
        used
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        match *event {
            MouseEvent::Down { pos, clicks, button } => {
                self.clicked = None;
                self.sidebar.take_match();
                if let Some(id) = self.tabs.tab_at(pos) {
                    if button != MouseButton::Right {
                        self.press = Some(Press::Tab(id, button));
                    }
                    // A double click keeps a preview tab.
                    if button == MouseButton::Left && clicks >= 2 {
                        if let Some(t) = self.docs.iter_mut().find(|t| t.id == id) {
                            t.ephemeral = false;
                        }
                        cx.redraw();
                    }
                    return;
                }
                let on_edge = self.layout.edge.is_some_and(|e| rect_contains(e, pos.0, pos.1));
                if let (true, Some(side)) = (on_edge, self.layout.sidebar) {
                    if !event.is_other_button() {
                        self.press = Some(Press::Edge(pos.0 - (side.x + side.w)));
                    }
                    return;
                }
                let Some(part) = self.part_at(pos) else { return };
                self.press = Some(Press::Part(part));
                if !event.is_other_button() {
                    self.set_focus(cx, part);
                }
                let opens = self.sidebar.opens();
                self.on_part(cx, part, |v, cx| v.mouse(cx, event));
                if self.sidebar.opens() != opens {
                    self.clicked = self.sidebar.chosen().map(|p| (p.to_path_buf(), clicks >= 2));
                }
                self.settle();
            }
            MouseEvent::Drag { pos, .. } | MouseEvent::Up { pos, .. } => {
                let up = matches!(event, MouseEvent::Up { .. });
                match self.press {
                    Some(Press::Edge(grab)) => {
                        if let (Some(rect), Some(side)) = (self.layout.rect, self.layout.sidebar) {
                            let scale = cx.ui.scale;
                            let (lo, hi) = Self::width_range(rect.w / scale);
                            let w = ((pos.0 - grab - side.x) / scale).clamp(lo, hi);
                            if w != self.width {
                                self.width = w;
                                self.session.set_width(w);
                                cx.redraw();
                            }
                        }
                    }
                    Some(Press::Part(part)) => {
                        self.on_part(cx, part, |v, cx| v.mouse(cx, event));
                        self.settle();
                    }
                    // A release over the pressed tab shows it (left) or
                    // closes it (middle).
                    Some(Press::Tab(id, button)) if up && self.tabs.tab_at(pos) == Some(id) => match button {
                        MouseButton::Left => self.activate(cx, id),
                        MouseButton::Middle => cx.request(Request::CloseDoc(id)),
                        MouseButton::Right => {}
                    },
                    Some(Press::Tab(..)) | None => {}
                }
                if up {
                    self.press = None;
                }
            }
            MouseEvent::Move { pos } => {
                self.hover_tab_to(cx, self.tabs.tab_at(pos));
                let part = self.part_at(pos);
                self.hover_to(cx, part, pos);
                if let Some(part) = part {
                    self.on_part(cx, part, |v, cx| v.mouse(cx, event));
                }
            }
            MouseEvent::Leave { pos } => {
                self.hover_tab_to(cx, None);
                self.hover_to(cx, None, pos);
                // A release the view never heard can't leave a drag running.
                if self.end_edge_drag() {
                    cx.redraw();
                }
            }
        }
    }

    /// On a file tab: the file menu of its file.
    fn context_menu(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        if let Some(id) = self.tabs.tab_at(pos) {
            let path = self.docs.iter().find(|t| t.id == id).and_then(|t| t.view.path());
            if let Some(path) = path {
                let target = FileTarget { path, kind: TargetKind::File, origin: Origin::Document };
                cx.request(Request::FileMenu { target, at: pos, align: MenuAlign::Left });
            }
            return;
        }
        if let Some(part) = self.part_at(pos) {
            self.on_part(cx, part, |v, cx| v.context_menu(cx, pos));
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), dx: f32, dy: f32) {
        if self.tabs.contains(pos) {
            if self.tabs.scroll_by(if dx.abs() > dy.abs() { dx } else { dy }) {
                cx.redraw();
            }
            return;
        }
        if let Some(part) = self.part_at(pos) {
            self.on_part(cx, part, |v, cx| v.scroll(cx, pos, dx, dy));
        }
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        match id {
            BTN_SIDEBAR => {
                let on = !self.pinned(cx.cfg);
                self.pinned = Some(on);
                self.session.set_pinned(on);
                if !on && self.focus == Part::Sidebar {
                    self.set_focus(cx, Part::Doc);
                }
                cx.redraw();
            }
            id if id >= DOC_IDS => {
                self.on_part(cx, Part::Doc, |v, cx| v.button(cx, id - DOC_IDS));
                self.settle();
            }
            id if id >= SIDEBAR_IDS => {
                self.on_part(cx, Part::Sidebar, |v, cx| v.button(cx, id - SIDEBAR_IDS));
            }
            // A close slot: the app asks about unsaved changes, then closes it.
            id => {
                if let Some(doc) = tabs::from_button(id, SIDEBAR_IDS) {
                    cx.request(Request::CloseDoc(doc));
                }
            }
        }
    }

    fn focus_changed(&mut self, cx: &mut EventCx, focused: bool) {
        // What lost the keys while still queued hears it first, so a
        // document that lost them still autosaves.
        if focused {
            self.unfocused = None;
        } else {
            self.tell_queued(cx, false);
        }
        self.focused = focused;
        if !focused {
            self.clicked = None;
            self.end_edge_drag();
        }
        let keys = self.keys();
        self.tell(cx, keys, focused);
    }

    /// The sidebar and the shown document; the others are off screen.
    fn shown(&mut self, visible: bool) {
        self.visible = visible;
        self.sidebar.shown(visible);
        if let Some(t) = self.docs.get_mut(self.active) {
            t.view.shown(visible);
        }
    }

    fn pump(&mut self, budget: usize) -> Pumped {
        let mut out = Pumped { changed: false, more: false };
        for t in &mut self.docs {
            let p = t.view.pump(budget);
            out.changed |= p.changed;
            out.more |= p.more;
        }
        out
    }

    fn refresh(&mut self) -> bool {
        let mut changed = self.sidebar.refresh();
        for t in &mut self.docs {
            changed |= t.view.refresh();
        }
        changed
    }

    fn is_busy(&self) -> bool {
        self.docs.iter().any(|t| t.view.is_busy())
    }

    fn deadline(&self, cfg: &Config) -> Option<Instant> {
        let docs = self.docs.iter().filter_map(|t| t.view.deadline(cfg));
        let unfocused = self.unfocused.as_ref().map(|(at, _)| *at);
        self.sidebar.deadline(cfg).into_iter().chain(docs).chain(unfocused).min()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.focused {
            self.tell_queued(cx, true);
        } else {
            self.unfocused = None;
        }
        cx.with_id_base(SIDEBAR_IDS, |cx| self.sidebar.tick(cx, now));
        for t in &mut self.docs {
            cx.with_doc(t.id, |cx| cx.with_id_base(DOC_IDS, |cx| t.view.tick(cx, now)));
        }
        self.settle();
    }

    /// The sidebar's selected entry while it has the keys, else the shown
    /// document's file.
    fn subject(&self) -> Option<PathBuf> {
        match self.part_with_keys() {
            Part::Sidebar => self.sidebar.subject(),
            Part::Doc => self.path(),
        }
    }

    fn files_changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        self.sidebar.files_changed(moved);
        for t in &mut self.docs {
            t.view.files_changed(moved);
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> bool {
        let used = self.docs.get_mut(self.active).is_some_and(|t| t.view.write_input(bytes));
        self.settle();
        used
    }

    /// The shown document's text, else the sidebar's listing.
    fn text(&mut self) -> Option<String> {
        match self.docs.get_mut(self.active) {
            Some(t) => t.view.text(),
            None => self.sidebar.text(),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{press, tmpdir, Harness};
    use crate::plugin::MenuItem;
    use crate::plugin::testing::UI;
    use crate::plugins::editor::EditorBuffer;
    use crate::plugins::files::MediaView;
    use ghostrealm_terminal::Key;

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 800.0, h: 400.0 };

    /// A folder holding `a.md` and `b.txt`, and a Files surface on it.
    fn fixture(session: &Session) -> (PathBuf, FilesView) {
        let d = tmpdir("files-view");
        std::fs::write(d.join("a.md"), "# Title\nbody line\n").unwrap();
        std::fs::write(d.join("b.txt"), "bee\n").unwrap();
        let v = FilesView::with(d.clone(), None, session.clone());
        (d, v)
    }

    fn editor(path: &std::path::Path) -> Box<dyn View> {
        Box::new(EditorView::new(EditorBuffer::open(path.to_path_buf()).unwrap()))
    }

    fn down(pos: (f32, f32)) -> MouseEvent {
        MouseEvent::Down { pos, clicks: 1, button: MouseButton::Left }
    }

    #[test]
    fn with_no_document_the_sidebar_takes_the_whole_width() {
        let (_d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let side = v.layout.sidebar.expect("the sidebar shows");
        assert_eq!((side.x, side.w), (RECT.x, RECT.w));
        assert!(v.layout.doc.is_none() && v.layout.edge.is_none());
        let texts = h.texts();
        assert!(texts.iter().any(|t| t == "a.md") && texts.iter().any(|t| t == "b.txt"), "{texts:?}");
        assert_eq!(v.title().as_deref(), Some("Files"));
        assert!(v.docs().is_empty(), "no document: Close closes the surface");
        let ids: Vec<u32> = h.frame.view_buttons.iter().map(|b| b.2).collect();
        assert!(!ids.contains(&BTN_SIDEBAR), "the sidebar button is inert: {ids:?}");
        assert!(ids.iter().any(|id| *id > SIDEBAR_IDS), "{ids:?}");
    }

    #[test]
    fn a_file_shows_under_its_header_beside_the_sidebar() {
        let (d, mut v) = fixture(&Session::default());
        let id = v.show_doc(editor(&d.join("a.md")));
        assert_eq!((v.docs(), v.active_doc()), (vec![id], id));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let (side, doc) = (v.layout.sidebar.unwrap(), v.layout.doc.unwrap());
        assert_eq!(side.w, SIDEBAR_W);
        assert!(doc.x > side.x + side.w - 1.0 && doc.x + doc.w == RECT.w);
        let texts = h.texts();
        for want in ["a.md", "MD text  \u{00b7}  18 B", "# Title", "body line"] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
        assert_eq!(v.title().as_deref(), Some("a.md"));
        assert_eq!(v.sidebar().chosen(), Some(d.join("a.md").as_path()), "the sidebar selects it");
        // Another file gets a tab of its own, shown.
        let next = v.show_doc(editor(&d.join("b.txt")));
        assert_eq!((v.docs(), v.active_doc()), (vec![id, next], next));
        assert!(!v.close_doc(next) && !v.close_doc(id), "the surface stays");
        assert!(v.docs().is_empty() && v.sidebar_has_keys());
        assert_eq!(v.title().as_deref(), Some("Files"));
    }

    #[test]
    fn an_image_shows_as_media() {
        let (d, mut v) = fixture(&Session::default());
        std::fs::write(d.join("c.png"), crate::media::testing::png(2, 2, [0; 4])).unwrap();
        v.show_doc(Box::new(MediaView::open(d.join("c.png"))));
        assert!(v.doc_view().and_then(|d| d.downcast_ref::<MediaView>()).is_some());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(h.texts().iter().any(|t| t == "c.png"), "its header");
    }

    #[test]
    fn the_sidebar_button_hides_and_shows_the_sidebar_and_new_surfaces_start_as_it_was_left() {
        let session = Session::default();
        let (d, mut v) = fixture(&session);
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(v.pinned(&h.cfg), "pinned by default");
        h.button(&mut v, BTN_SIDEBAR);
        h.paint(&mut v, RECT);
        assert!(v.layout.sidebar.is_none(), "hidden");
        assert_eq!(v.layout.doc.map(|r| (r.x, r.w)), Some((RECT.x, RECT.w)), "the document takes the width");

        // A new surface starts hidden too; without a document it still lists.
        let (_, mut fresh) = fixture(&session);
        assert!(!fresh.pinned(&h.cfg));
        h.paint(&mut fresh, RECT);
        assert!(fresh.layout.sidebar.is_some(), "no document: the sidebar shows anyway");
        fresh.show_doc(editor(&d.join("b.txt")));
        h.paint(&mut fresh, RECT);
        assert!(fresh.layout.sidebar.is_none());

        // Each surface keeps its own pin.
        h.button(&mut v, BTN_SIDEBAR);
        h.paint(&mut v, RECT);
        assert!(v.layout.sidebar.is_some());
        h.paint(&mut fresh, RECT);
        assert!(fresh.layout.sidebar.is_none(), "the other surface keeps its own");

        // Untoggled this session, the config decides.
        h.cfg.file_browser.sidebar = SidebarPin::Hidden;
        let (_, other) = fixture(&Session::default());
        assert!(!other.pinned(&h.cfg));
    }

    #[test]
    fn dragging_the_sidebars_edge_resizes_it_within_bounds() {
        let session = Session::default();
        let (d, mut v) = fixture(&session);
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let edge = v.layout.edge.expect("a resize handle");
        let x = edge.x + edge.w * 0.5;
        let y = RECT.h * 0.5;
        h.mouse(&mut v, down((x, y)));
        h.mouse(&mut v, MouseEvent::Drag { pos: (x + 40.0, y), button: MouseButton::Left });
        assert_eq!(v.sidebar_width(), SIDEBAR_W + 40.0);
        h.mouse(&mut v, MouseEvent::Drag { pos: (5.0, y), button: MouseButton::Left });
        assert_eq!(v.sidebar_width(), SIDEBAR_MIN_W, "no narrower than the minimum");
        h.mouse(&mut v, MouseEvent::Drag { pos: (RECT.w, y), button: MouseButton::Left });
        assert_eq!(v.sidebar_width(), RECT.w - DOC_MIN_W, "the document keeps its minimum");
        h.mouse(&mut v, MouseEvent::Up { pos: (RECT.w, y), dragged: true, button: MouseButton::Left });
        h.paint(&mut v, RECT);
        assert_eq!(v.layout.sidebar.unwrap().w, RECT.w - DOC_MIN_W);
        let (_, fresh) = fixture(&session);
        assert_eq!(fresh.sidebar_width(), RECT.w - DOC_MIN_W, "a new surface starts at the last width");
    }

    #[test]
    fn right_on_a_file_opens_it_and_moves_the_keyboard_to_it() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(v.sidebar_has_keys());
        h.key(&mut v, press(Key::Down));
        let (used, out) = h.key(&mut v, press(Key::Right));
        assert!(used);
        assert_eq!(out.requests, [Request::OpenFile(d.join("a.md"))]);
        assert!(v.sidebar_has_keys(), "until the document shows");
        v.show_doc(editor(&d.join("a.md")));
        assert!(!v.sidebar_has_keys(), "the keyboard is on the document");
        h.key(&mut v, press(Key::Char('x')));
        let active = v.active_doc();
        let text = v.doc_mut(active).and_then(|d| d.text());
        assert_eq!(text.as_deref(), Some("x# Title\nbody line"), "typing edits the document");

        // A click in the sidebar takes the keys back, and opening a file
        // with a click leaves them there.
        h.paint(&mut v, RECT);
        let side = v.layout.sidebar.unwrap();
        h.mouse(&mut v, down((side.x + 10.0, side.y + side.h - 5.0)));
        assert!(v.sidebar_has_keys());
        // Row 1 (b.txt), below the sidebar's filter.
        let row = (side.x + 60.0, side.y + 38.0 + 26.0 + 13.0);
        let out = h.mouse(&mut v, down(row));
        assert_eq!(out.requests, [Request::OpenFile(d.join("b.txt"))]);
        v.show_doc(editor(&d.join("b.txt")));
        assert!(v.sidebar_has_keys());
        // Opened any other way (a link, the palette), the keys go to it.
        assert!(v.show_path(&d.join("b.txt")));
        assert!(!v.sidebar_has_keys());
        h.mouse(&mut v, down((side.x + 10.0, side.y + side.h - 5.0)));

        // Right on a file a tab shows: the app shows that tab, and the
        // keyboard moves to it.
        h.key(&mut v, press(Key::Down));
        let (_, out) = h.key(&mut v, press(Key::Right));
        assert_eq!(out.requests, [Request::OpenFile(d.join("b.txt"))]);
        assert!(v.show_path(&d.join("b.txt")));
        assert!(!v.sidebar_has_keys());
    }

    /// The centre of sidebar row `i` (below its filter), in a surface at the
    /// top left.
    fn sidebar_row(i: usize) -> (f32, f32) {
        (60.0, 28.0 + 38.0 + 26.0 * i as f32 + 13.0)
    }

    #[test]
    fn a_click_open_with_the_sidebar_hidden_puts_the_keys_on_the_document() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.cfg.file_browser.sidebar = SidebarPin::Hidden;
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(0)));
        assert!(v.clicked.is_some(), "the click opened a.md");
        v.show_doc(editor(&d.join("a.md")));
        assert!(!v.sidebar_has_keys(), "the sidebar won't show beside it");
        h.key(&mut v, press(Key::Char('x')));
        let active = v.active_doc();
        assert!(v.doc_mut(active).and_then(|d| d.text()).unwrap().starts_with("x# Title"));

        // A sidebar hidden while it has the keys hands them on at the next paint.
        let (d, mut v) = fixture(&Session::default());
        h.cfg.file_browser.sidebar = SidebarPin::Pinned;
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(0)));
        v.show_doc(editor(&d.join("a.md")));
        assert!(v.sidebar_has_keys());
        v.pinned = Some(false);
        h.paint(&mut v, RECT);
        assert!(!v.sidebar_has_keys());
    }

    #[test]
    fn a_click_on_the_file_shown_already_doesnt_keep_the_keys_for_a_later_open() {
        let (d, mut v) = fixture(&Session::default());
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        // Even if the app never shows a.md's tab for this click, the click
        // doesn't linger.
        let out = h.mouse(&mut v, down(sidebar_row(0)));
        assert_eq!(out.requests, [Request::OpenFile(d.join("a.md"))]);
        assert!(v.sidebar_has_keys());
        // A palette open later takes the keys to its document.
        v.show_doc(editor(&d.join("b.txt")));
        assert!(!v.sidebar_has_keys());
    }

    #[test]
    fn moving_the_keys_between_parts_tells_them_on_the_next_tick() {
        let path = tmpdir("files-unfocus").join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let (_, mut v) = fixture(&Session::default());
        let h = Harness::new();
        h.focus(&mut v, true);
        v.show_doc(editor(&path));
        assert!(v.write_input(b"edit "));
        v.focus_sidebar();
        let due = v.deadline(&h.cfg).expect("a tick to tell the document");
        let out = h.tick(&mut v, due);
        assert_eq!(out.requests, [Request::Saved(path.clone())], "the document autosaved on losing the keys");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edit x\n");
        assert_eq!(v.deadline(&h.cfg), None);
    }

    #[test]
    fn losing_focus_first_tells_a_part_whose_loss_was_queued() {
        let path = tmpdir("files-unfocus-lost").join("f.txt");
        std::fs::write(&path, "x\n").unwrap();
        let (_, mut v) = fixture(&Session::default());
        let h = Harness::new();
        h.focus(&mut v, true);
        v.show_doc(editor(&path));
        assert!(v.write_input(b"edit "));
        v.focus_sidebar();
        let out = h.focus(&mut v, false);
        assert_eq!(out.requests, [Request::Saved(path.clone())], "the document autosaved");
        assert_eq!(v.deadline(&h.cfg), None, "nothing left queued");
    }

    #[test]
    fn a_lost_release_doesnt_strand_an_edge_drag() {
        let (d, mut v) = fixture(&Session::default());
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let edge = v.layout.edge.unwrap();
        h.mouse(&mut v, down((edge.x + 1.0, 200.0)));
        h.mouse(&mut v, MouseEvent::Leave { pos: (-5.0, 200.0) });
        h.cursor = (RECT.w * 0.75, 200.0);
        assert_eq!(h.paint(&mut v, RECT).cursor, None, "no surface-wide resize cursor");
        h.mouse(&mut v, down((edge.x + 1.0, 200.0)));
        h.focus(&mut v, false);
        assert!(v.press.is_none());
    }

    #[test]
    fn the_edge_keeps_its_cursor_over_the_filter_and_while_dragged() {
        let (d, mut v) = fixture(&Session::default());
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let edge = v.layout.edge.unwrap();
        // On the edge at the filter's height: the resize cursor, not the I-beam.
        h.cursor = (edge.x + edge.w * 0.5, 28.0 + 19.0);
        let cursor = h.paint(&mut v, RECT).cursor.map(|c| c.1);
        assert_eq!(cursor, Some(CursorIcon::ColResize));
        h.mouse(&mut v, down(h.cursor));
        h.cursor = (RECT.w * 0.75, 200.0);
        let cursor = h.paint(&mut v, RECT).cursor.map(|c| c.1);
        assert_eq!(cursor, Some(CursorIcon::ColResize), "while dragging, wherever the pointer is");
    }

    #[test]
    fn escape_clears_the_sidebars_filter() {
        let (_d, mut v) = fixture(&Session::default());
        let h = Harness::new();
        h.key(&mut v, press(Key::Char('b')));
        assert_eq!(v.sidebar().tree().query(), "b");
        h.key(&mut v, press(Key::Escape));
        assert_eq!(v.sidebar().tree().query(), "");
    }

    #[test]
    fn the_sidebars_and_the_documents_buttons_and_menus_keep_apart() {
        let (d, mut v) = fixture(&Session::default());
        v.show_doc(editor(&d.join("a.md")));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let ids: Vec<u32> = h.frame.view_buttons.iter().map(|b| b.2).collect();
        assert!(ids.iter().any(|id| (SIDEBAR_IDS..DOC_IDS).contains(id)), "sidebar buttons: {ids:?}");
        assert!(ids.iter().any(|id| *id >= DOC_IDS), "document buttons: {ids:?}");
        // The sidebar's `⋯` menu comes back with the sidebar's ids, and
        // choosing an item reaches the sidebar.
        let more = *ids.iter().filter(|id| (SIDEBAR_IDS..DOC_IDS).contains(*id)).max().unwrap();
        let mut items: Vec<MenuItem> = Vec::new();
        for id in ids.iter().filter(|id| (SIDEBAR_IDS..DOC_IDS).contains(*id)) {
            if let [Request::Menu { items: got, .. }] = h.button(&mut v, *id).requests.as_slice() {
                items = got.clone();
            }
        }
        assert!(!items.is_empty(), "a sidebar button ({more}) opened its menu");
        let hidden = items.iter().find(|i| i.label == "Hidden files").expect("the hidden-files item");
        assert!(hidden.id > SIDEBAR_IDS && hidden.id < DOC_IDS);
        let before = v.sidebar().tree().show_hidden();
        h.button(&mut v, hidden.id);
        assert_eq!(v.sidebar().tree().show_hidden(), !before);
    }

    #[test]
    fn the_agent_text_is_the_documents_else_the_sidebars_listing() {
        let (d, mut v) = fixture(&Session::default());
        assert_eq!(v.text().unwrap(), format!("{}\n  a.md\n  b.txt\n", d.display()));
        v.show_doc(editor(&d.join("b.txt")));
        assert_eq!(v.text().as_deref(), Some("bee"));
    }

    fn up(pos: (f32, f32), button: MouseButton) -> MouseEvent {
        MouseEvent::Up { pos, dragged: false, button }
    }

    /// The centre of tab `id`'s pill, as last painted.
    fn tab_centre(v: &FilesView, id: DocId) -> (f32, f32) {
        let r = v.tabs.pill(id).expect("a painted tab");
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    }

    /// The alpha of the fill painted exactly over `r`, if one was.
    fn fill_alpha(frame: &crate::plugin::Frame, r: Rect) -> Option<f32> {
        let (sw, sh) = frame.screen;
        let near = |a: f32, b: f32| (a - b).abs() < 0.5;
        frame
            .bg
            .iter()
            .find(|q| {
                let (x, y) = ((q.pos[0] + 1.0) / 2.0 * sw, (1.0 - q.pos[1]) / 2.0 * sh);
                near(x, r.x) && near(y, r.y) && near(q.px[0], r.w) && near(q.px[1], r.h)
            })
            .map(|q| q.color[3])
    }

    /// `v` with tabs of `names` (files in `d` holding their names), opened
    /// from outside the sidebar, in that order.
    fn tabs_of(d: &std::path::Path, v: &mut FilesView, names: &[&str]) -> Vec<DocId> {
        names
            .iter()
            .map(|n| {
                std::fs::write(d.join(n), n).unwrap();
                v.show_doc(editor(&d.join(n)))
            })
            .collect()
    }

    #[test]
    fn a_single_click_opens_the_preview_tab_and_the_next_replaces_it() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(0)));
        let a = v.show_doc(editor(&d.join("a.md")));
        assert!(v.is_preview(a));
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(1)));
        let b = v.show_doc(editor(&d.join("b.txt")));
        assert_ne!(a, b, "a new id for the new file");
        assert_eq!(v.docs(), [b], "it took the preview tab");
        assert!(v.is_preview(b));
        // A double click in the sidebar keeps it...
        h.mouse(&mut v, MouseEvent::Down { pos: sidebar_row(1), clicks: 2, button: MouseButton::Left });
        assert!(v.show_path(&d.join("b.txt")));
        assert!(!v.is_preview(b));
        // ...so the next click opens a preview tab beside it.
        h.mouse(&mut v, down(sidebar_row(0)));
        let a = v.show_doc(editor(&d.join("a.md")));
        assert_eq!(v.docs(), [b, a]);
        assert!(v.is_preview(a));
        // A single click on its file again leaves it a preview tab.
        h.mouse(&mut v, down(sidebar_row(0)));
        assert!(v.show_path(&d.join("a.md")));
        assert!(v.is_preview(a));
        // An edit keeps it.
        assert!(v.write_input(b"x"));
        assert!(!v.is_preview(a));
        h.mouse(&mut v, down(sidebar_row(1)));
        assert!(v.show_path(&d.join("b.txt")));
        assert_eq!(v.docs(), [b, a], "nothing replaced");
    }

    #[test]
    fn a_click_on_a_search_match_opens_the_preview_tab_with_the_match_selected() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        for c in "?line".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        let until = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            h.tick(&mut v, Instant::now() + std::time::Duration::from_secs(1));
            v.refresh();
            if !v.text().unwrap().contains("Searching") {
                break;
            }
            assert!(Instant::now() < until, "the search finishes");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        h.paint(&mut v, RECT);
        // Row 0 is a.md's header; row 1 its line "body line".
        let out = h.mouse(&mut v, down(sidebar_row(1)));
        assert_eq!(out.requests, [Request::OpenFile(d.join("a.md"))]);
        let a = v.show_doc(editor(&d.join("a.md")));
        assert!(v.is_preview(a));
        assert!(v.sidebar_has_keys(), "the keys stay in the results");
        let buffer = |v: &FilesView| v.doc_view().and_then(|d| d.downcast_ref::<EditorView>()).unwrap().buffer().selection();
        assert_eq!(buffer(&v), Some(((1, 5), (1, 9))));
        assert!(v.sidebar_mut().text().unwrap().starts_with("?line"), "the search stays");
        // A double click keeps the tab, the match selected again.
        v.doc_mut(a).and_then(|d| d.downcast_mut::<EditorView>()).unwrap().buffer_mut().clear_selection();
        h.mouse(&mut v, MouseEvent::Down { pos: sidebar_row(1), clicks: 2, button: MouseButton::Left });
        assert!(v.show_path(&d.join("a.md")));
        assert!(!v.is_preview(a));
        assert_eq!(buffer(&v), Some(((1, 5), (1, 9))));
        // Unsaved edits: the match is found again on its line...
        let edit = |v: &mut FilesView, line: &str| {
            let b = v.doc_mut(a).and_then(|d| d.downcast_mut::<EditorView>()).unwrap().buffer_mut();
            b.select_line_at(1);
            b.insert_str(line);
        };
        edit(&mut v, "xx body line");
        h.mouse(&mut v, down(sidebar_row(1)));
        assert!(v.show_path(&d.join("a.md")));
        assert_eq!(buffer(&v), Some(((1, 8), (1, 12))));
        // ...else the caret goes to the line's start.
        edit(&mut v, "gone");
        h.mouse(&mut v, down(sidebar_row(1)));
        assert!(v.show_path(&d.join("a.md")));
        assert_eq!((buffer(&v), v.doc_view().and_then(|d| d.downcast_ref::<EditorView>()).unwrap().buffer().cursor), (None, (1, 0)));
        // Saving searches again.
        v.doc_mut(a).and_then(|d| d.downcast_mut::<EditorView>()).unwrap().buffer_mut().save().unwrap();
        assert!(v.sidebar().deadline(&h.cfg).is_none());
        h.paint(&mut v, RECT);
        assert!(v.sidebar().deadline(&h.cfg).is_some(), "the search is due again");
    }

    /// Search `v`'s sidebar for `query` and wait for the results.
    fn search_sidebar(h: &Harness, v: &mut FilesView, query: &str) {
        for c in query.chars() {
            h.key(v, press(Key::Char(c)));
        }
        settle_search(h, v);
    }

    /// Run the sidebar's due search and wait until it is done.
    fn settle_search(h: &Harness, v: &mut FilesView) {
        let until = Instant::now() + std::time::Duration::from_secs(10);
        loop {
            h.tick(v, Instant::now() + std::time::Duration::from_secs(1));
            v.refresh();
            if v.sidebar().deadline(&h.cfg).is_none() && !v.sidebar_mut().text().unwrap().contains("Searching") {
                return;
            }
            assert!(Instant::now() < until, "the search finishes");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn a_click_on_a_match_after_an_autosave_selects_it_and_keeps_the_list() {
        let (d, mut v) = fixture(&Session::default());
        std::fs::write(d.join("c.txt"), "x\nline\n").unwrap();
        let mut h = Harness::new();
        h.focus(&mut v, true);
        search_sidebar(&h, &mut v, "?line");
        h.paint(&mut v, RECT);
        // Rows: a.md, "body line", c.txt, "line".
        h.mouse(&mut v, down(sidebar_row(1)));
        let a = v.show_doc(editor(&d.join("a.md")));
        h.paint(&mut v, RECT);
        // Into the document, edit before the match, then click the match
        // again: the keys move to the sidebar, which autosaves first.
        let doc = v.layout.doc.unwrap();
        h.mouse(&mut v, down((doc.x + 40.0, doc.y + 60.0)));
        assert!(!v.sidebar_has_keys());
        let b = v.doc_mut(a).and_then(|d| d.downcast_mut::<EditorView>()).unwrap().buffer_mut();
        b.select_line_at(1);
        b.insert_str("xx body line");
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(1)));
        let ed = |v: &FilesView| {
            let b = v.doc_view().and_then(|d| d.downcast_ref::<EditorView>()).unwrap().buffer();
            (b.modified, b.selection())
        };
        assert!(!ed(&v).0, "saved as the keys left");
        assert!(v.show_path(&d.join("a.md")));
        assert_eq!(ed(&v).1, Some(((1, 8), (1, 12))), "found again in the saved text");
        // The save searches again; the list stays (no jump to an empty
        // list) and keeps the opened row selected.
        h.paint(&mut v, RECT);
        let listed = v.sidebar_mut().text().unwrap();
        assert!(listed.contains("a.md (1)") && listed.contains("c.txt (1)"), "{listed}");
        let selected = |h: &mut Harness, v: &mut FilesView| {
            let want = crate::plugin::paint::rect_quad(RECT, 1.0, 1.0, theme::ROW_SELECTED_BG, theme::ROW_SELECTED_ALPHA).color;
            let frame = h.paint(v, RECT);
            frame.bg.iter().filter(|q| q.color == want && q.radius > 0.0).map(|q| (1.0 - q.pos[1]) * 0.5 * frame.screen.1).collect::<Vec<f32>>()
        };
        let before = selected(&mut h, &mut v);
        assert_eq!(before.len(), 1);
        settle_search(&h, &mut v);
        assert_eq!(selected(&mut h, &mut v), before, "the same row, after the new results");
        let listed = v.sidebar_mut().text().unwrap();
        assert!(listed.contains("  2: xx body line"), "{listed}");
    }

    #[test]
    fn a_match_whose_open_never_showed_is_dropped_by_the_next_event() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        for c in "?line".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        let until = Instant::now() + std::time::Duration::from_secs(10);
        while v.text().unwrap().contains("Searching") {
            assert!(Instant::now() < until, "the search finishes");
            h.tick(&mut v, Instant::now() + std::time::Duration::from_secs(1));
            v.refresh();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(1)));
        // The open failed: nothing showed. A key later, a palette open of
        // the file selects nothing.
        h.key(&mut v, press(Key::Down));
        v.show_doc(editor(&d.join("a.md")));
        let ed = v.doc_view().and_then(|d| d.downcast_ref::<EditorView>()).unwrap();
        assert_eq!((ed.buffer().selection(), ed.buffer().cursor), (None, (0, 0)));
    }

    #[test]
    fn a_double_click_on_the_preview_tab_keeps_it() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(0)));
        let a = v.show_doc(editor(&d.join("a.md")));
        h.paint(&mut v, RECT);
        let at = tab_centre(&v, a);
        h.mouse(&mut v, down(at));
        h.mouse(&mut v, up(at, MouseButton::Left));
        assert!(v.is_preview(a), "a single click only shows it");
        h.mouse(&mut v, MouseEvent::Down { pos: at, clicks: 2, button: MouseButton::Left });
        assert!(!v.is_preview(a));
    }

    #[test]
    fn an_open_from_outside_the_sidebar_gets_a_kept_tab_and_shows_a_tab_of_its_file() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let a = v.show_doc(editor(&d.join("a.md")));
        assert!(!v.is_preview(a));
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(1)));
        let b = v.show_doc(editor(&d.join("b.txt")));
        assert!(v.is_preview(b));
        assert_eq!((v.docs(), v.active_doc()), (vec![a, b], b));
        // The palette opening a file a tab shows shows that tab, kept.
        assert!(v.show_path(&d.join("a.md")));
        assert_eq!((v.docs(), v.active_doc()), (vec![a, b], a));
        assert!(v.show_path(&d.join("b.txt")));
        assert!(!v.is_preview(b));
        assert!(!v.show_path(&d.join("nope.txt")));
    }

    #[test]
    fn closing_a_tab_keeps_the_shown_one_else_shows_its_right_then_left_neighbour() {
        let (d, mut v) = fixture(&Session::default());
        let [a, b, c, e] = tabs_of(&d, &mut v, &["1.txt", "2.txt", "3.txt", "4.txt"])[..] else { unreachable!() };
        assert_eq!((v.docs(), v.active_doc()), (vec![a, b, c, e], e));
        assert!(v.show_path(&d.join("3.txt")));
        v.close_doc(a);
        assert_eq!((v.docs(), v.active_doc()), (vec![b, c, e], c), "an earlier tab closing keeps the shown one");
        v.close_doc(c);
        assert_eq!(v.active_doc(), e, "the right neighbour");
        v.close_doc(e);
        assert_eq!(v.active_doc(), b, "none to the right: the left one");
        v.close_doc(b);
        assert!(v.docs().is_empty() && v.sidebar_has_keys());
        // A new tab opens right of the shown one, and ids aren't reused.
        let [x, y] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        assert!(v.show_path(&d.join("1.txt")));
        let z = v.show_doc(editor(&d.join("a.md")));
        assert_eq!(v.docs(), [x, z, y]);
        assert!([x, y, z].iter().all(|id| id.0 > e.0));
    }

    #[test]
    fn next_and_previous_tab_wrap_around() {
        let (d, mut v) = fixture(&Session::default());
        let [a, b] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        v.cycle_tab(1);
        assert_eq!(v.active_doc(), a);
        v.cycle_tab(-1);
        assert_eq!(v.active_doc(), b);
    }

    #[test]
    fn a_tab_shows_on_release_over_it_and_middle_click_or_its_close_slot_closes_it() {
        let (d, mut v) = fixture(&Session::default());
        let [a, b] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        v.focus_sidebar();
        let (at_a, at_b) = (tab_centre(&v, a), tab_centre(&v, b));
        h.mouse(&mut v, down(at_a));
        assert_eq!(v.active_doc(), b, "not on the press");
        h.mouse(&mut v, up(at_b, MouseButton::Left));
        assert_eq!(v.active_doc(), b, "released over another tab: nothing");
        h.mouse(&mut v, down(at_a));
        h.mouse(&mut v, up(at_a, MouseButton::Left));
        assert_eq!(v.active_doc(), a);
        assert!(!v.sidebar_has_keys(), "the keyboard goes to it");

        let middle = MouseEvent::Down { pos: at_b, clicks: 1, button: MouseButton::Middle };
        assert!(h.mouse(&mut v, middle).requests.is_empty());
        let out = h.mouse(&mut v, up(at_b, MouseButton::Middle));
        assert_eq!(out.requests, [Request::CloseDoc(b)], "the app asks about unsaved changes, then closes it");
        let out = h.button(&mut v, tabs::button_id(a));
        assert_eq!(out.requests, [Request::CloseDoc(a)]);
        let ids: Vec<u32> = h.frame.view_buttons.iter().map(|b| b.2).collect();
        assert!(ids.contains(&tabs::button_id(a)) && ids.contains(&tabs::button_id(b)), "{ids:?}");
    }

    #[test]
    fn a_right_click_on_a_tab_opens_its_files_menu() {
        let (d, mut v) = fixture(&Session::default());
        let [_, b] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let at = tab_centre(&v, b);
        let out = h.context_menu(&mut v, at);
        let [Request::FileMenu { target, at: got, .. }] = out.requests.as_slice() else { panic!("{:?}", out.requests) };
        assert_eq!((target.path.clone(), target.origin, *got), (d.join("2.txt"), Origin::Document, at));
    }

    #[test]
    fn the_active_tab_is_filled_stronger_than_a_hovered_one() {
        let (d, mut v) = fixture(&Session::default());
        let [a, b] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.cursor = tab_centre(&v, a);
        h.paint(&mut v, RECT);
        let active = fill_alpha(&h.frame, v.tabs.pill(b).unwrap());
        let hover = fill_alpha(&h.frame, v.tabs.pill(a).unwrap());
        assert_eq!((active, hover), (Some(theme::TAB_ACTIVE_ALPHA), Some(theme::HOVER_ALPHA)));
        const { assert!(theme::TAB_ACTIVE_ALPHA > theme::HOVER_ALPHA) };
        h.cursor = (-1.0, -1.0);
        h.paint(&mut v, RECT);
        assert_eq!(fill_alpha(&h.frame, v.tabs.pill(a).unwrap()), None, "no fill unhovered");
    }

    #[test]
    fn an_unsaved_tab_shows_a_dot_in_its_close_slot_until_hovered() {
        let (d, mut v) = fixture(&Session::default());
        let [a, b] = tabs_of(&d, &mut v, &["1.txt", "2.txt"])[..] else { unreachable!() };
        let mut h = Harness::new();
        // The dot: a disc, 0.45 of a row high.
        let dot = (UI.cell_h * 0.45).round();
        h.paint(&mut v, RECT);
        // In the tab row: the document's header shows one too.
        let area = v.tabs.area().unwrap();
        let dots = |h: &Harness| {
            let in_row = |q: &&crate::plugin::paint::QuadInstance| {
                let y = (1.0 - q.pos[1]) / 2.0 * h.frame.screen.1;
                y >= area.y && y < area.y + area.h
            };
            h.frame.bg.iter().filter(in_row).filter(|q| q.px == [dot, dot] && q.radius == dot * 0.5).count()
        };
        assert_eq!(dots(&h), 0);
        assert!(v.write_input(b"x"), "edits the shown 2.txt");
        h.paint(&mut v, RECT);
        assert_eq!(dots(&h), 1);
        h.cursor = tab_centre(&v, b);
        assert!(h.mouse(&mut v, MouseEvent::Move { pos: h.cursor }).redraw, "the pointer onto a tab repaints");
        h.paint(&mut v, RECT);
        assert_eq!(dots(&h), 0, "hovered, the slot offers to close it");
        h.cursor = tab_centre(&v, a);
        h.paint(&mut v, RECT);
        assert_eq!(dots(&h), 1);
    }

    #[test]
    fn too_many_tabs_shrink_then_scroll_keeping_the_shown_one_in_view() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        let names: Vec<String> = (0..12).map(|i| format!("a-rather-long-file-name-{i:02}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ids = tabs_of(&d, &mut v, &refs[..3]);
        h.paint(&mut v, RECT);
        let area = v.tabs.area().unwrap();
        let natural = v.tabs.pill(ids[0]).unwrap().w;
        let ids = [ids, tabs_of(&d, &mut v, &refs[3..5])].concat();
        h.paint(&mut v, RECT);
        let pills: Vec<Rect> = ids.iter().map(|id| v.tabs.pill(*id).unwrap()).collect();
        assert!(pills.iter().all(|p| p.w < natural && p.w >= 90.0), "shrunk: {pills:?}");
        let last = pills.last().unwrap();
        assert!(last.x + last.w <= area.x + area.w, "all fit");

        let ids = [ids, tabs_of(&d, &mut v, &refs[5..])].concat();
        h.paint(&mut v, RECT);
        let pills: Vec<Rect> = ids.iter().map(|id| v.tabs.pill(*id).unwrap()).collect();
        assert!(pills.iter().all(|p| p.w == 90.0), "at the minimum: {pills:?}");
        let shown = v.tabs.pill(v.active_doc()).unwrap();
        assert!(shown.x >= area.x && shown.x + shown.w <= area.x + area.w, "the shown tab is in view");
        assert!(pills[0].x < area.x, "the first scrolled off");

        // The wheel over the row scrolls it.
        let row = (area.x + 10.0, area.y + area.h * 0.5);
        h.scroll(&mut v, row, 10_000.0);
        h.paint(&mut v, RECT);
        assert_eq!(v.tabs.pill(ids[0]).unwrap().x, area.x, "back at the start");
        assert!(v.tabs.pill(v.active_doc()).unwrap().x > area.x + area.w, "the shown one out of view");
        // Showing another tab scrolls it into view.
        assert!(v.show_path(&d.join(&names[10])));
        h.paint(&mut v, RECT);
        let shown = v.tabs.pill(v.active_doc()).unwrap();
        assert!(shown.x >= area.x && shown.x + shown.w <= area.x + area.w);
    }

    #[test]
    fn closing_a_tab_while_scrolled_brings_the_shown_one_back_into_view() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        let names: Vec<String> = (0..12).map(|i| format!("a-rather-long-file-name-{i:02}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ids = tabs_of(&d, &mut v, &refs);
        h.paint(&mut v, RECT);
        let area = v.tabs.area().unwrap();
        h.scroll(&mut v, (area.x + 10.0, area.y + 5.0), 10_000.0);
        h.paint(&mut v, RECT);
        assert!(v.tabs.pill(v.active_doc()).unwrap().x > area.x + area.w, "scrolled away from it");
        v.close_doc(ids[0]);
        h.paint(&mut v, RECT);
        let shown = v.tabs.pill(v.active_doc()).unwrap();
        assert!(shown.x >= area.x && shown.x + shown.w <= area.x + area.w, "{shown:?} in {area:?}");
    }

    #[test]
    fn two_tab_switches_before_a_tick_tell_the_document_the_keys_first_left() {
        let (d, mut v) = fixture(&Session::default());
        let h = Harness::new();
        h.focus(&mut v, true);
        let ids = tabs_of(&d, &mut v, &["1.txt", "2.txt", "3.txt"]);
        assert!(v.write_input(b"edit "), "edits the shown 3.txt");
        v.cycle_tab(1);
        v.cycle_tab(1);
        assert_eq!(v.active_doc(), ids[1]);
        let due = v.deadline(&h.cfg).expect("a tick to tell 3.txt");
        let out = h.tick(&mut v, due);
        assert_eq!(out.requests, [Request::Saved(d.join("3.txt"))], "3.txt autosaved on losing the keys");
    }

    #[test]
    fn switching_tabs_selects_the_file_in_the_sidebar() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        let ids = tabs_of(&d, &mut v, &["1.txt", "2.txt"]);
        assert_eq!(v.sidebar().chosen(), Some(d.join("2.txt").as_path()));
        v.cycle_tab(1);
        assert_eq!(v.sidebar().chosen(), Some(d.join("1.txt").as_path()));
        h.paint(&mut v, RECT);
        let at = tab_centre(&v, ids[1]);
        h.mouse(&mut v, down(at));
        h.mouse(&mut v, up(at, MouseButton::Left));
        assert_eq!(v.sidebar().chosen(), Some(d.join("2.txt").as_path()));
        v.close_doc(ids[1]);
        assert_eq!(v.sidebar().chosen(), Some(d.join("1.txt").as_path()), "and closing one");

        // A file the listing doesn't show (in a folder not expanded) isn't
        // revealed by a switch: the selection stays, the folder closed.
        std::fs::create_dir(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("3.txt"), "3").unwrap();
        v.sidebar_mut().tree_mut().refresh();
        let sub = v.show_doc(editor(&d.join("sub").join("3.txt")));
        assert!(v.sidebar().tree().is_expanded(&d.join("sub")), "an open reveals it");
        v.sidebar_mut().tree_mut().toggle_dir(&d.join("sub"));
        v.cycle_tab(-1);
        v.cycle_tab(1);
        assert_eq!(v.active_doc(), sub);
        assert_eq!(v.sidebar().chosen(), Some(d.join("1.txt").as_path()));
        assert!(!v.sidebar().tree().is_expanded(&d.join("sub")));
    }

    #[test]
    fn a_suffix_too_long_is_cut_before_the_name() {
        let (d, mut v) = fixture(&Session::default());
        let long = "folder".repeat(15);
        for dir in [format!("x{long}"), format!("y{long}")] {
            std::fs::create_dir(d.join(&dir)).unwrap();
            std::fs::write(d.join(&dir).join("mod.rs"), "").unwrap();
            v.show_doc(editor(&d.join(&dir).join("mod.rs")));
        }
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let texts = h.texts();
        let cut = |p: char| texts.iter().any(|t| t.starts_with(&format!("mod.rs \u{00b7} {p}folder")) && t.ends_with('\u{2026}'));
        assert!(cut('x') && cut('y'), "{texts:?}");
    }

    #[test]
    fn a_typed_filter_survives_switching_and_closing_tabs() {
        let (d, mut v) = fixture(&Session::default());
        let ids = tabs_of(&d, &mut v, &["1.txt", "2.txt"]);
        let h = Harness::new();
        v.focus_sidebar();
        h.key(&mut v, press(Key::Char('b')));
        assert_eq!(v.sidebar().tree().query(), "b");
        v.cycle_tab(1);
        assert_eq!(v.sidebar().tree().query(), "b", "a switch keeps it");
        v.focus_sidebar();
        v.close_doc(ids[0]);
        assert_eq!(v.sidebar().tree().query(), "b", "Cmd+W keeps it");
    }

    #[test]
    fn a_tab_scrolled_past_the_rows_left_edge_leaves_the_sidebar_buttons_tooltip() {
        let (d, mut v) = fixture(&Session::default());
        let names: Vec<String> = (0..12).map(|i| format!("a-rather-long-file-name-{i:02}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let ids = tabs_of(&d, &mut v, &refs);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let area = v.tabs.area().unwrap();
        let row = (area.x + 10.0, area.y + 5.0);
        h.scroll(&mut v, row, 10_000.0);
        h.scroll(&mut v, row, -45.0);
        h.paint(&mut v, RECT);
        let first = v.tabs.pill(ids[0]).unwrap();
        assert!(first.x < area.x && first.x + first.w > area.x, "the first tab straddles the edge: {first:?}");
        // On the sidebar button (left of the row), under the first tab's
        // unclipped pill.
        h.cursor = (17.0, area.y + area.h * 0.5);
        assert!(h.cursor.0 < area.x && h.cursor.0 > first.x);
        h.paint(&mut v, RECT);
        let tip = h.frame.tooltip.as_ref().map(|t| t.1.clone());
        assert_eq!(tip.as_deref(), Some("Hide the sidebar"));
    }

    #[test]
    fn a_preview_tab_edited_behind_the_views_back_is_kept() {
        let (d, mut v) = fixture(&Session::default());
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, down(sidebar_row(0)));
        let a = v.show_doc(editor(&d.join("a.md")));
        assert!(v.doc_mut(a).unwrap().write_input(b"x"));
        h.mouse(&mut v, down(sidebar_row(1)));
        let b = v.show_doc(editor(&d.join("b.txt")));
        assert_eq!(v.docs(), [a, b], "the edited a.md wasn't replaced");
        assert!(!v.is_preview(a) && v.is_preview(b));
    }

    #[test]
    fn tabs_of_files_with_one_name_show_their_folder_and_long_names_end_in_an_ellipsis() {
        let (d, mut v) = fixture(&Session::default());
        for dir in ["x", "y"] {
            std::fs::create_dir(d.join(dir)).unwrap();
            std::fs::write(d.join(dir).join("mod.rs"), "").unwrap();
            v.show_doc(editor(&d.join(dir).join("mod.rs")));
        }
        let long = format!("{}.txt", "long".repeat(30));
        std::fs::write(d.join(&long), "").unwrap();
        let id = v.show_doc(editor(&d.join(&long)));
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let texts = h.texts();
        for want in ["mod.rs \u{00b7} x", "mod.rs \u{00b7} y"] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
        assert!(texts.iter().any(|t| t.starts_with("longlong") && t.ends_with('\u{2026}')), "{texts:?}");
        // Its full path shows as a tooltip.
        h.cursor = tab_centre(&v, id);
        h.paint(&mut v, RECT);
        let tip = h.frame.tooltip.as_ref().map(|t| t.1.clone());
        assert_eq!(tip, Some(d.join(&long).display().to_string()));
    }

    #[test]
    fn a_right_to_left_tab_name_starts_at_the_left() {
        let (d, mut v) = fixture(&Session::default());
        let name = "\u{05e9}\u{05dc}\u{05d5}\u{05dd}.txt";
        tabs_of(&d, &mut v, &[name]);
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let left = h.text_left(name).expect("the name was drawn");
        assert!(left < 1.0, "glyphs start at {left}, not against the right of the box");
    }
}
