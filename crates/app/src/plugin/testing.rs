//! A headless host for exercising views in tests: paints into a [`Frame`] and
//! delivers input through real [`PaintCx`]/[`EventCx`]s, with no window or GPU.

use ghostrealm_core::{Chrome, Config, Rect, SurfaceId};
use ghostrealm_terminal::{Key, KeyPress, Mods};
use glyphon::FontSystem;

use super::{EventCx, Frame, Layer, MouseEvent, Outcome, PaintCx, TextKit, UiMetrics, View};

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
    /// Paint as if the pane shows a tab strip above the view.
    pub tab_strip: bool,
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
            tab_strip: false,
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
        cx.tab_strip = self.tab_strip;
        view.paint(&mut cx, rect);
        self.text.finish_paint();
        &self.frame
    }

    fn event(&self, mods: Mods, f: impl FnOnce(&mut EventCx)) -> Outcome {
        let mut cx = EventCx::new(&self.cfg, UI, mods, None);
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

/// A unique, empty temp directory.
pub fn tmpdir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ghostrealm-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
