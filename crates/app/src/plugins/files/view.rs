//! [`FilesView`]: the Files surface itself.

use std::any::Any;
use std::path::PathBuf;
use std::time::Instant;

use ghostrealm_core::config::SidebarPin;
use ghostrealm_core::{Config, Rect};
use ghostrealm_terminal::{KeyPress, Pumped};
use winit::window::CursorIcon;

use super::Session;
use crate::plugin::icons::Icon;
use crate::plugin::{rect_contains, theme, DocId, EventCx, MouseEvent, OpenCx, PaintCx, View, Waker};
use crate::plugins::editor::EditorView;
use crate::plugins::file_browser::FileBrowserView;

/// The sidebar button (see [`View::button`]).
const BTN_SIDEBAR: u32 = 0;
/// The sidebar's and the document's ids, past the view's own (see
/// [`PaintCx::with_id_base`]).
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

/// What the held mouse button pressed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Press {
    Part(Part),
    /// The sidebar's right edge, grabbed this far right of it (physical px).
    Edge(f32),
}

/// The open document.
struct Doc {
    id: DocId,
    view: Box<dyn View>,
}

/// Where the last paint put things.
#[derive(Clone, Copy, Default)]
struct Layout {
    rect: Option<Rect>,
    sidebar: Option<Rect>,
    /// The resize handle on the sidebar's right edge.
    edge: Option<Rect>,
    doc: Option<Rect>,
    toggle: Option<Rect>,
}

/// The Files surface: the file sidebar beside one document. With no
/// document it shows only the sidebar, full width; with one, the sidebar
/// shows while pinned (the button at the top left).
pub struct FilesView {
    sidebar: FileBrowserView,
    doc: Option<Doc>,
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
    /// The file a click in the sidebar last opened: its document shows with
    /// the keyboard left in the sidebar, so browsing by clicks goes on, while
    /// the sidebar shows beside it. Any other open moves the keyboard to the
    /// document.
    clicked: Option<PathBuf>,
    /// Whether the config pins the sidebar, as of the last paint (for a pin
    /// that follows it, between paints).
    cfg_pinned: bool,
    /// Focus changes of the parts not yet told (made where no event context
    /// was at hand), with when they were made: delivered on the next tick.
    unfocused: Option<(Instant, Part)>,
    press: Option<Press>,
    /// The part under the pointer, to tell when it leaves.
    hover: Option<Part>,
    layout: Layout,
}

impl FilesView {
    /// A Files surface showing only its sidebar, at `root`.
    pub fn new(root: PathBuf, cx: &OpenCx) -> Self {
        Self::with(root, cx.waker.clone(), cx.files.clone())
    }

    fn with(root: PathBuf, waker: Option<Waker>, session: Session) -> Self {
        FilesView {
            sidebar: FileBrowserView::new(root).with_waker(waker),
            doc: None,
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
            layout: Layout::default(),
        }
    }

    /// Show `view` as the document, replacing the one shown (whose unsaved
    /// changes the app asked about first). It gets a new id. The keyboard
    /// moves to it unless a click in the shown sidebar opened it.
    pub fn show_doc(&mut self, mut view: Box<dyn View>) -> DocId {
        if let Some(editor) = view.downcast_mut::<EditorView>() {
            editor.set_header(true);
        }
        if self.visible {
            view.shown(true);
        }
        let path = view.path();
        let id = DocId(self.next_id);
        self.next_id += 1;
        let before = self.part_with_keys();
        self.doc = Some(Doc { id, view });
        let clicked = self.clicked.take();
        let keep = path.is_some() && clicked == path && self.pinned.unwrap_or(self.cfg_pinned);
        if !keep {
            self.focus = Part::Doc;
        }
        // The replaced document went with its focus; a sidebar that lost
        // the keys hears so on the next tick.
        if before == Part::Sidebar && self.part_with_keys() == Part::Doc {
            self.queue_unfocus(Part::Sidebar);
        }
        if let Some(p) = path.filter(|p| self.sidebar.chosen() != Some(p.as_path())) {
            self.sidebar.reveal(&p);
        }
        id
    }

    /// Tell `part` on the next tick that it lost the keys, and the part
    /// that has them now that it gained them: done where no event context
    /// is at hand. Only while the surface has the keyboard.
    fn queue_unfocus(&mut self, part: Part) {
        if self.focused {
            self.unfocused = Some((Instant::now(), part));
        }
    }

    /// The document shown, if any.
    pub fn doc_view(&self) -> Option<&dyn View> {
        self.doc.as_ref().map(|d| &*d.view)
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
        self.part_with_keys() == Part::Sidebar
    }

    /// Move the keyboard to the sidebar, showing it (and recording the
    /// pin, as the sidebar button does).
    pub fn focus_sidebar(&mut self) {
        self.pinned = Some(true);
        self.session.set_pinned(true);
        if self.part_with_keys() == Part::Doc {
            self.queue_unfocus(Part::Doc);
        }
        self.focus = Part::Sidebar;
    }

    /// The sidebar's width now (logical px).
    pub fn sidebar_width(&self) -> f32 {
        self.width
    }

    fn part_with_keys(&self) -> Part {
        match self.doc {
            Some(_) => self.focus,
            None => Part::Sidebar,
        }
    }

    /// Give the keys to `part`, telling the parts that gain and lose them.
    fn set_focus(&mut self, cx: &mut EventCx, part: Part) {
        let before = self.part_with_keys();
        self.focus = part;
        let after = self.part_with_keys();
        if before != after && self.focused {
            self.part_focus(cx, before, false);
            self.part_focus(cx, after, true);
        }
    }

    fn part_focus(&mut self, cx: &mut EventCx, part: Part, on: bool) {
        match part {
            Part::Sidebar => cx.with_id_base(SIDEBAR_IDS, |cx| self.sidebar.focus_changed(cx, on)),
            Part::Doc => {
                if let Some(d) = &mut self.doc {
                    cx.with_id_base(DOC_IDS, |cx| d.view.focus_changed(cx, on));
                }
            }
        }
    }

    /// Run `f` on `part` with its ids.
    fn on_part<T>(&mut self, cx: &mut EventCx, part: Part, f: impl FnOnce(&mut dyn View, &mut EventCx) -> T) -> Option<T> {
        match part {
            Part::Sidebar => Some(cx.with_id_base(SIDEBAR_IDS, |cx| f(&mut self.sidebar, cx))),
            Part::Doc => {
                let d = self.doc.as_mut()?;
                Some(cx.with_id_base(DOC_IDS, |cx| f(&mut *d.view, cx)))
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

    /// The slim row above the sidebar and the document: the sidebar button
    /// at its left end, where file tabs will follow it.
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
        let live = self.doc.is_some();
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
        self.layout.toggle = Some(b);
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

    /// The document's title, else "Files".
    fn title(&self) -> Option<String> {
        match &self.doc {
            Some(d) => d.view.title(),
            None => Some("Files".to_string()),
        }
    }

    fn docs(&self) -> Vec<DocId> {
        self.doc.iter().map(|d| d.id).collect()
    }

    fn active_doc(&self) -> DocId {
        self.doc.as_ref().map_or(DocId::SELF, |d| d.id)
    }

    fn doc(&self, id: DocId) -> Option<&dyn View> {
        self.doc.as_ref().filter(|d| d.id == id).map(|d| &*d.view)
    }

    fn doc_mut(&mut self, id: DocId) -> Option<&mut dyn View> {
        match self.doc.as_mut() {
            Some(d) if d.id == id => Some(&mut *d.view),
            _ => None,
        }
    }

    /// The surface stays, showing its sidebar alone.
    fn close_doc(&mut self, id: DocId) -> bool {
        if self.doc.as_ref().is_some_and(|d| d.id == id) {
            self.doc = None;
            self.focus = Part::Sidebar;
        }
        false
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        self.layout = Layout { rect: Some(rect), ..Layout::default() };
        cx.fill(rect, cx.chrome.background);
        let row_h = Self::row_height(cx).min(rect.h);
        self.paint_top_row(cx, Rect { h: row_h, ..rect });
        let body = Rect { y: rect.y + row_h, h: (rect.h - row_h).max(0.0), ..rect };
        let scale = cx.ui.scale;
        self.cfg_pinned = cx.cfg.file_browser.sidebar == SidebarPin::Pinned;
        let show_sidebar = self.doc.is_none() || self.pinned(cx.cfg);
        // A hidden sidebar can't keep the keys.
        if !show_sidebar && self.focus == Part::Sidebar {
            self.focus = Part::Doc;
            self.queue_unfocus(Part::Sidebar);
        }
        let (side, doc) = match (&self.doc, show_sidebar) {
            (None, _) => (Some(body), None),
            (Some(_), false) => (None, Some(body)),
            (Some(_), true) => {
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
        if let (Some(r), Some(d)) = (doc, self.doc.as_mut()) {
            cx.focused = focused && keys == Part::Doc;
            cx.with_id_base(DOC_IDS, |cx| d.view.paint(cx, r));
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
        self.clicked = None;
        let used = self.on_part(cx, part, |v, cx| v.key(cx, k)).unwrap_or(false);
        if part == Part::Sidebar && self.sidebar.take_opened_by_key() {
            // The open reaches the app as a request, so the document shows
            // after this returns (`show_doc`, which moves the keyboard); a
            // file shown already doesn't show again.
            let shown = self.doc.as_ref().and_then(|d| d.view.path());
            if shown.is_some() && shown.as_deref() == self.sidebar.chosen() {
                self.set_focus(cx, Part::Doc);
            }
        }
        used
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        match *event {
            MouseEvent::Down { pos, .. } => {
                self.clicked = None;
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
                    self.clicked = self.sidebar.chosen().map(PathBuf::from);
                }
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
                    }
                    None => {}
                }
                if up {
                    self.press = None;
                }
            }
            MouseEvent::Move { pos } => {
                let part = self.part_at(pos);
                self.hover_to(cx, part, pos);
                if let Some(part) = part {
                    self.on_part(cx, part, |v, cx| v.mouse(cx, event));
                }
            }
            MouseEvent::Leave { pos } => self.hover_to(cx, None, pos),
        }
    }

    fn context_menu(&mut self, cx: &mut EventCx, pos: (f32, f32)) {
        if let Some(part) = self.part_at(pos) {
            self.on_part(cx, part, |v, cx| v.context_menu(cx, pos));
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), dx: f32, dy: f32) {
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
            }
            id if id >= SIDEBAR_IDS => {
                self.on_part(cx, Part::Sidebar, |v, cx| v.button(cx, id - SIDEBAR_IDS));
            }
            _ => {}
        }
    }

    fn focus_changed(&mut self, cx: &mut EventCx, focused: bool) {
        self.focused = focused;
        self.unfocused = None;
        let part = self.part_with_keys();
        self.part_focus(cx, part, focused);
    }

    fn shown(&mut self, visible: bool) {
        self.visible = visible;
        self.sidebar.shown(visible);
        if let Some(d) = &mut self.doc {
            d.view.shown(visible);
        }
    }

    fn pump(&mut self, budget: usize) -> Pumped {
        match &mut self.doc {
            Some(d) => d.view.pump(budget),
            None => Pumped { changed: false, more: false },
        }
    }

    fn refresh(&mut self) -> bool {
        let sidebar = self.sidebar.refresh();
        sidebar | self.doc.as_mut().is_some_and(|d| d.view.refresh())
    }

    fn is_busy(&self) -> bool {
        self.doc.as_ref().is_some_and(|d| d.view.is_busy())
    }

    fn deadline(&self, cfg: &Config) -> Option<Instant> {
        let doc = self.doc.as_ref().and_then(|d| d.view.deadline(cfg));
        let unfocused = self.unfocused.map(|(at, _)| at);
        self.sidebar.deadline(cfg).into_iter().chain(doc).chain(unfocused).min()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if let Some((_, lost)) = self.unfocused.take() {
            let gained = self.part_with_keys();
            if self.focused && lost != gained {
                self.part_focus(cx, lost, false);
                self.part_focus(cx, gained, true);
            }
        }
        cx.with_id_base(SIDEBAR_IDS, |cx| self.sidebar.tick(cx, now));
        if let Some(d) = &mut self.doc {
            cx.with_id_base(DOC_IDS, |cx| d.view.tick(cx, now));
        }
    }

    /// The sidebar's selected entry while it has the keys, else the
    /// document's file.
    fn subject(&self) -> Option<PathBuf> {
        match self.part_with_keys() {
            Part::Sidebar => self.sidebar.subject(),
            Part::Doc => self.path(),
        }
    }

    fn files_changed(&mut self, moved: &[(PathBuf, PathBuf)]) {
        self.sidebar.files_changed(moved);
        if let Some(d) = &mut self.doc {
            d.view.files_changed(moved);
        }
    }

    /// The document's text, else the sidebar's listing.
    fn text(&mut self) -> Option<String> {
        match &mut self.doc {
            Some(d) => d.view.text(),
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
    use crate::plugin::{MenuItem, MouseButton, Request};
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
        assert!(v.docs().is_empty(), "no document: closing closes the surface");
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
        // Replacing it gives the new document a new id.
        let next = v.show_doc(editor(&d.join("b.txt")));
        assert_ne!(next, id);
        assert!(v.doc(id).is_none() && v.doc(next).is_some());
        assert!(!v.close_doc(next), "the surface stays");
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
        v.show_doc(editor(&d.join("b.txt")));
        assert!(!v.sidebar_has_keys());
        h.mouse(&mut v, down((side.x + 10.0, side.y + side.h - 5.0)));

        // Right on the file shown already moves the keyboard at once.
        h.key(&mut v, press(Key::Down));
        let (_, out) = h.key(&mut v, press(Key::Right));
        assert_eq!(out.requests, [Request::OpenFile(d.join("b.txt"))]);
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
        // a.md is shown, so the app shows nothing new for this click.
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
}
