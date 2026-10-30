//! A headless host for exercising views in tests: paints into a [`Frame`] and
//! delivers input through real [`PaintCx`]/[`EventCx`]s, with no window or GPU.

use std::cell::RefCell;

use ghostrealm_core::{Chrome, Config, Rect, SurfaceId};
use ghostrealm_terminal::{Key, KeyPress, Mods};
use glyphon::FontSystem;

use super::{DocId, EventCx, Frame, Layer, MouseEvent, Outcome, PaintCx, TextKit, UiMetrics, View};

pub const UI: UiMetrics = UiMetrics {
    cell_w: 8.0,
    cell_h: 16.0,
    scale: 1.0,
};

pub struct Harness {
    pub cfg: Config,
    pub chrome: Chrome,
    pub text: TextKit,
    pub frame: Frame,
    pub cursor: (f32, f32),
    /// Paint as if a modal overlay covers the panes.
    pub modal: bool,
    /// The clipboard events read and write, in place of the system's.
    pub clipboard: RefCell<Option<String>>,
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

impl Harness {
    pub fn new() -> Self {
        // One font file is all layout/input tests need (geometry comes from
        // `UI`) and loads far faster than a system-font scan, the fallback.
        let mut db = glyphon::cosmic_text::fontdb::Database::new();
        let one = [
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Monaco.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        ]
        .iter()
        .any(|p| db.load_font_file(p).is_ok());
        if !one {
            db.load_system_fonts();
        }
        let fs = FontSystem::new_with_locale_and_db("en-US".into(), db);
        Harness {
            cfg: Config::default(),
            chrome: Chrome::default(),
            text: TextKit::new(fs, 256),
            frame: Frame::new((800.0, 600.0)),
            cursor: (-1.0, -1.0),
            modal: false,
            clipboard: RefCell::new(None),
        }
    }

    /// Paint `view` into `rect` as a focused pane, into a fresh frame.
    pub fn paint(&mut self, view: &mut dyn View, rect: Rect) -> &Frame {
        self.frame = Frame::new((800.0, 600.0));
        let m = glyphon::Metrics::new(14.0, UI.cell_h);
        self.text
            .begin_frame(0, m, UI.cell_h, std::time::Duration::from_millis(50));
        let mut cx = PaintCx::new(
            &self.cfg,
            &self.chrome,
            UI,
            self.cursor,
            &mut self.text,
            &mut self.frame,
            Layer::Base,
            SurfaceId(1),
        );
        cx.focused = true;
        cx.modal = self.modal;
        view.paint(&mut cx, rect);
        self.text.finish_paint();
        &self.frame
    }

    /// The text of every text item the last paint drew, in order.
    pub fn texts(&self) -> Vec<String> {
        let items = self.frame.text.iter().chain(self.frame.overlay_text());
        items
            .filter_map(|t| self.text.buffer(t.src))
            .map(|b| b.layout_runs().map(|r| r.text).collect::<Vec<_>>().join("\n"))
            .collect()
    }

    /// Where the leftmost glyph of the drawn text `text` starts within its
    /// shaped line: 0 for a line laid out from the left, as right-to-left
    /// text must be to show where it is placed.
    pub fn text_left(&self, text: &str) -> Option<f32> {
        let mut items = self.frame.text.iter().chain(self.frame.overlay_text()).filter_map(|t| self.text.buffer(t.src));
        let buf = items.find(|b| b.layout_runs().any(|r| r.text == text))?;
        Some(buf.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| g.x)).fold(f32::MAX, f32::min))
    }

    fn event(&self, mods: Mods, f: impl FnOnce(&mut EventCx)) -> Outcome {
        let mut clipboard = self.clipboard.borrow_mut();
        let mut cx = EventCx::new(&self.cfg, UI, mods, Some(&mut *clipboard));
        f(&mut cx);
        cx.finish()
    }

    pub fn key(&self, view: &mut dyn View, press: KeyPress) -> (bool, Outcome) {
        let mut used = false;
        let out = self.event(press.mods, |cx| used = view.key(cx, &press));
        (used, out)
    }

    pub fn mouse(&self, view: &mut dyn View, ev: MouseEvent) -> Outcome {
        self.event(Mods::default(), |cx| view.mouse(cx, &ev))
    }

    pub fn context_menu(&self, view: &mut dyn View, pos: (f32, f32)) -> Outcome {
        self.event(Mods::default(), |cx| view.context_menu(cx, pos))
    }

    /// Like [`mouse`](Self::mouse) with `mods` held.
    pub fn mouse_with(&self, view: &mut dyn View, mods: Mods, ev: MouseEvent) -> Outcome {
        self.event(mods, |cx| view.mouse(cx, &ev))
    }

    /// Like [`scroll`](Self::scroll) with `mods` held.
    pub fn scroll_with(&self, view: &mut dyn View, mods: Mods, pos: (f32, f32), dy: f32) -> Outcome {
        self.event(mods, |cx| view.scroll(cx, pos, 0.0, dy))
    }

    pub fn scroll(&self, view: &mut dyn View, pos: (f32, f32), dy: f32) -> Outcome {
        self.event(Mods::default(), |cx| view.scroll(cx, pos, 0.0, dy))
    }

    pub fn button(&self, view: &mut dyn View, id: u32) -> Outcome {
        self.event(Mods::default(), |cx| view.button(cx, id))
    }

    pub fn focus(&self, view: &mut dyn View, focused: bool) -> Outcome {
        self.event(Mods::default(), |cx| view.focus_changed(cx, focused))
    }

    pub fn tick(&self, view: &mut dyn View, now: std::time::Instant) -> Outcome {
        self.event(Mods::default(), |cx| view.tick(cx, now))
    }
}

/// A view hosting several documents, each a view of its own: the shape of a
/// Files surface, for tests of the app's per-document handling. Ids start at
/// 1, so none is [`DocId::SELF`]. It forwards `paint` and `key` to the active
/// document and `files_changed` to every one; nothing else.
pub struct DocsView {
    pub docs: Vec<(DocId, Box<dyn View>)>,
    pub active: usize,
}

impl DocsView {
    /// Hosting `docs`, showing the last.
    pub fn new(docs: Vec<Box<dyn View>>) -> Self {
        let active = docs.len().saturating_sub(1);
        let docs = docs.into_iter().zip(1..).map(|(v, i)| (DocId(i), v)).collect();
        DocsView { docs, active }
    }
}

impl View for DocsView {
    fn plugin(&self) -> &'static str {
        "docs"
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        if let Some((_, d)) = self.docs.get_mut(self.active) {
            d.paint(cx, rect);
        }
    }

    fn key(&mut self, cx: &mut EventCx, key: &KeyPress) -> bool {
        self.docs.get_mut(self.active).is_some_and(|(_, d)| d.key(cx, key))
    }

    fn files_changed(&mut self, moved: &[(std::path::PathBuf, std::path::PathBuf)]) {
        for (_, d) in &mut self.docs {
            d.files_changed(moved);
        }
    }

    fn docs(&self) -> Vec<DocId> {
        self.docs.iter().map(|(id, _)| *id).collect()
    }

    fn active_doc(&self) -> DocId {
        self.docs.get(self.active).map_or(DocId::SELF, |(id, _)| *id)
    }

    fn doc(&self, id: DocId) -> Option<&dyn View> {
        self.docs.iter().find(|(d, _)| *d == id).map(|(_, v)| &**v)
    }

    fn doc_mut(&mut self, id: DocId) -> Option<&mut dyn View> {
        match self.docs.iter_mut().find(|(d, _)| *d == id) {
            Some((_, v)) => Some(&mut **v),
            None => None,
        }
    }

    fn close_doc(&mut self, id: DocId) -> bool {
        if let Some(i) = self.docs.iter().position(|(d, _)| *d == id) {
            self.docs.remove(i);
            if i < self.active {
                self.active -= 1;
            }
        }
        self.active = self.active.min(self.docs.len().saturating_sub(1));
        self.docs.is_empty()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// A key press with Cmd held.
pub fn cmd(key: Key) -> KeyPress {
    KeyPress {
        mods: Mods {
            super_: true,
            ..Mods::default()
        },
        ..press(key)
    }
}

/// A plain key press.
pub fn press(key: Key) -> KeyPress {
    let text = match key {
        Key::Char(c) => Some(c.to_string()),
        _ => None,
    };
    KeyPress {
        key,
        mods: Mods::default(),
        text,
    }
}

/// A unique, empty temp directory, removed when the returned guard drops.
pub fn tmpdir(tag: &str) -> TmpDir {
    TmpDir::new(tag)
}

/// A unique, empty temp directory removed when dropped, so a test that fails
/// cleans up too. Declare it before anything that writes into it (a host, a
/// view): locals drop in reverse order, so those are dropped first. A dropped
/// host is only asked to exit and may still be running when the directory
/// goes.
#[derive(Debug)]
pub struct TmpDir(std::path::PathBuf);

impl TmpDir {
    pub fn new(tag: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ghostrealm-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }

    pub fn as_path(&self) -> &std::path::Path {
        &self.0
    }
}

impl std::ops::Deref for TmpDir {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::path::Path> for TmpDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TmpDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TmpDir {
    /// Retried for a while: a stub host told to exit may still be writing
    /// its log, and a file it creates mid-removal leaves the directory
    /// non-empty. Once the directory has gone its writes fail instead.
    fn drop(&mut self) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::fs::remove_dir_all(&self.0).is_err()
            && self.0.exists()
            && std::time::Instant::now() < until
        {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if self.0.exists() {
            eprintln!("left behind: {} (still being written to)", self.0.display());
        }
    }
}
