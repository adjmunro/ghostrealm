//! Immediate-mode painting for views (and the app's own chrome).
//!
//! A frame is built into a [`Frame`]: quads in two layers (under text, over
//! text) and text items that reference either the content-keyed [`RowCache`] or
//! a per-frame scratch buffer pool in the shared [`TextKit`]. The app turns the
//! frame into GPU draws after every view has painted. Nothing here touches the
//! GPU, so views can be painted (and tested) headless.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ghostrealm_core::{Chrome, Config, Rect, SurfaceId};
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, TextBounds};

use super::{rect_contains, theme, UiMetrics};
use crate::row_cache::RowCache;

/// Shape at least this many missed rows per frame regardless of the time budget,
/// so a backlog always makes forward progress even on an already-slow frame.
const MIN_SHAPES_PER_FRAME: usize = 8;

/// Focused-pane / panel border thickness, in physical pixels.
pub const BORDER: f32 = 2.0;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadInstance {
    /// Top-left in NDC.
    pub pos: [f32; 2],
    /// Width/height in NDC (height negative to grow downward).
    pub size: [f32; 2],
    pub color: [f32; 4],
}

/// A solid quad covering `r` (physical px) on a `sw`x`sh` surface.
pub fn rect_quad(r: Rect, sw: f32, sh: f32, color: [u8; 3], alpha: f32) -> QuadInstance {
    let ndc_x = r.x / sw * 2.0 - 1.0;
    let ndc_y = 1.0 - r.y / sh * 2.0;
    let ndc_w = r.w / sw * 2.0;
    let ndc_h = -(r.h / sh * 2.0);
    QuadInstance {
        pos: [ndc_x, ndc_y],
        size: [ndc_w, ndc_h],
        color: [
            srgb_to_linear(color[0]) as f32,
            srgb_to_linear(color[1]) as f32,
            srgb_to_linear(color[2]) as f32,
            alpha,
        ],
    }
}

/// Four thin quads forming a border just inside `r`.
pub fn push_border(out: &mut Vec<QuadInstance>, r: Rect, sw: f32, sh: f32, color: [u8; 3]) {
    let t = BORDER;
    for edge in [
        Rect { x: r.x, y: r.y, w: r.w, h: t },
        Rect { x: r.x, y: r.y + r.h - t, w: r.w, h: t },
        Rect { x: r.x, y: r.y, w: t, h: r.h },
        Rect { x: r.x + r.w - t, y: r.y, w: t, h: r.h },
    ] {
        out.push(rect_quad(edge, sw, sh, color, 1.0));
    }
}

pub fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// The clip bounds glyphon expects for a rect.
pub fn bounds(r: Rect) -> TextBounds {
    TextBounds {
        left: r.x as i32,
        top: r.y as i32,
        right: (r.x + r.w) as i32,
        bottom: (r.y + r.h) as i32,
    }
}

/// Where a text item's shaped buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSrc {
    /// A content-keyed row in the [`RowCache`].
    Row(u64),
    /// A slot in the [`TextKit`]'s per-frame scratch pool.
    Scratch(usize),
}

/// One piece of text placed this frame.
#[derive(Clone, Copy, Debug)]
pub struct TextItem {
    pub src: TextSrc,
    pub left: f32,
    pub top: f32,
    pub bounds: TextBounds,
    /// Colour for glyphs whose spans didn't bake one in.
    pub color: [u8; 3],
}

/// Which pass a view paints into. Views in panes paint the base layer; a view
/// hosted in a modal (the directory picker) paints the overlay layer, above all
/// pane content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Base,
    Overlay,
}

/// Everything painted this frame, in draw order: `bg` quads, then `text`, then
/// `top` quads (cursors, borders, overlay panels), then `overlay_text`.
pub struct Frame {
    /// Surface size in physical pixels.
    pub screen: (f32, f32),
    pub bg: Vec<QuadInstance>,
    pub text: Vec<TextItem>,
    pub top: Vec<QuadInstance>,
    pub overlay_text: Vec<TextItem>,
    /// Buttons views registered: (hit rect, owning surface, view-local id).
    pub view_buttons: Vec<(Rect, SurfaceId, u32)>,
    /// A view needs another paced frame (an animation step, deferred work).
    pub wants_frame: bool,
}

impl Frame {
    pub fn new(screen: (f32, f32)) -> Self {
        Frame {
            screen,
            bg: Vec::new(),
            text: Vec::new(),
            top: Vec::new(),
            overlay_text: Vec::new(),
            view_buttons: Vec::new(),
            wants_frame: false,
        }
    }

    pub fn quad(&self, r: Rect, color: [u8; 3], alpha: f32) -> QuadInstance {
        rect_quad(r, self.screen.0, self.screen.1, color, alpha)
    }
}

/// The app's shared text machinery: the one `FontSystem`, the content-keyed row
/// cache, a per-frame scratch buffer pool, and the per-frame shaping budget.
pub struct TextKit {
    pub font_system: FontSystem,
    pub row_cache: RowCache,
    /// Buffers shaped this frame, indexed by [`TextSrc::Scratch`]. A field (not
    /// a getter) so the text pass can borrow it beside `font_system`.
    pub(crate) scratch: Vec<Buffer>,
    scratch_used: usize,
    /// Row key drawn at each screen position last frame / this frame, so a row
    /// deferred by the budget redraws its previous content instead of a gap.
    prev_rows: HashMap<(i32, i32), u64>,
    cur_rows: HashMap<(i32, i32), u64>,
    metrics: Metrics,
    line_h: f32,
    shape_deadline: Instant,
    shaped: usize,
    deferred: bool,
}

impl TextKit {
    pub fn new(font_system: FontSystem, row_cache_cap: usize) -> Self {
        TextKit {
            font_system,
            row_cache: RowCache::new(row_cache_cap),
            scratch: Vec::new(),
            scratch_used: 0,
            prev_rows: HashMap::new(),
            cur_rows: HashMap::new(),
            metrics: Metrics::new(1.0, 1.0),
            line_h: 1.0,
            shape_deadline: Instant::now(),
            shaped: 0,
            deferred: false,
        }
    }

    /// Start painting a frame at `metrics` (row height `line_h`), with
    /// `budget` of wall-clock time for shaping cache-missed budgeted rows.
    pub fn begin_frame(&mut self, metrics_gen: u64, metrics: Metrics, line_h: f32, budget: Duration) {
        self.row_cache.begin_frame(metrics_gen);
        self.metrics = metrics;
        self.line_h = line_h;
        self.scratch_used = 0;
        self.cur_rows.clear();
        self.shape_deadline = Instant::now() + budget;
        self.shaped = 0;
        self.deferred = false;
    }

    /// Finish painting. Returns whether any budgeted row was deferred (the frame
    /// needs a follow-up to finish shaping). Call before the text pass; the row
    /// cache's own `end_frame` runs after it.
    pub fn finish_paint(&mut self) -> bool {
        std::mem::swap(&mut self.prev_rows, &mut self.cur_rows);
        self.deferred
    }

    /// The buffer a text item draws.
    pub fn buffer(&self, src: TextSrc) -> Option<&Buffer> {
        match src {
            TextSrc::Row(key) => self.row_cache.buffer(key),
            TextSrc::Scratch(idx) => self.scratch.get(idx),
        }
    }

    /// The row key to draw at screen position `pos` for content `key`: `key`
    /// itself once shaped (shaping `spans()` on a miss while the frame's budget
    /// lasts), else — past the budget — whatever `pos` drew last frame, marking
    /// the frame deferred. `key` without a buffer draws blank (a first reveal).
    pub(crate) fn budgeted_key(
        &mut self,
        key: u64,
        pos: (i32, i32),
        shape_w: f32,
        spans: impl FnOnce() -> Vec<(String, [u8; 3])>,
    ) -> u64 {
        let (m, lh) = (self.metrics, self.line_h);
        let place_key = if self.row_cache.buffer(key).is_some() {
            // Hit: no shaping, just refresh recency.
            self.row_cache.ensure(key, &mut self.font_system, m, shape_w, lh, &[]);
            key
        } else if self.shaped < MIN_SHAPES_PER_FRAME || Instant::now() < self.shape_deadline {
            let spans = spans();
            self.row_cache.ensure(key, &mut self.font_system, m, shape_w, lh, &spans);
            self.shaped += 1;
            key
        } else {
            self.deferred = true;
            match self.prev_rows.get(&pos).copied() {
                Some(prev) if self.row_cache.buffer(prev).is_some() => {
                    self.row_cache.ensure(prev, &mut self.font_system, m, shape_w, lh, &[]);
                    prev
                }
                _ => key,
            }
        };
        self.cur_rows.insert(pos, place_key);
        place_key
    }

    /// Shape coloured `spans` into the next free scratch slot. With
    /// `bake_colors`, each span's colour is baked into its glyphs; otherwise the
    /// placement colour applies to all of them.
    fn shape_scratch(
        &mut self,
        spans: &[(&str, [u8; 3])],
        family: Family,
        width: f32,
        bake_colors: bool,
    ) -> Shaped {
        let idx = self.scratch_used;
        self.scratch_used += 1;
        while self.scratch.len() <= idx {
            let b = Buffer::new(&mut self.font_system, self.metrics);
            self.scratch.push(b);
        }
        let buf = &mut self.scratch[idx];
        buf.set_metrics(self.metrics);
        buf.set_wrap(glyphon::Wrap::None);
        buf.set_size(Some(width.max(1.0)), Some(self.line_h));
        buf.set_rich_text(
            spans.iter().map(|(t, c)| {
                let mut a = Attrs::new().family(family);
                if bake_colors {
                    a = a.color(Color::rgb(c[0], c[1], c[2]));
                }
                (*t, a)
            }),
            &Attrs::new().family(family),
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut self.font_system, false);
        let w = buf.layout_runs().map(|r| r.line_w).fold(0.0_f32, f32::max);
        Shaped { idx, width: w }
    }
}

/// Text shaped into a scratch slot, not yet placed.
#[derive(Clone, Copy, Debug)]
pub struct Shaped {
    idx: usize,
    /// The shaped (advance) width in physical pixels.
    pub width: f32,
}

/// What a view paints with. Everything is in physical pixels.
pub struct PaintCx<'a> {
    pub cfg: &'a Config,
    pub chrome: &'a Chrome,
    pub ui: UiMetrics,
    /// Mouse position (for hover styling).
    pub cursor: (f32, f32),
    /// The view's pane has keyboard focus.
    pub focused: bool,
    /// The pane shows a tab strip above the view (a single-tab pane may hide it;
    /// a view that wants a header of its own then draws one).
    pub tab_strip: bool,
    pub(crate) text: &'a mut TextKit,
    pub(crate) frame: &'a mut Frame,
    pub(crate) layer: Layer,
    pub(crate) owner: SurfaceId,
}

impl<'a> PaintCx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: &'a Config,
        chrome: &'a Chrome,
        ui: UiMetrics,
        cursor: (f32, f32),
        text: &'a mut TextKit,
        frame: &'a mut Frame,
        layer: Layer,
        owner: SurfaceId,
    ) -> Self {
        PaintCx {
            cfg,
            chrome,
            ui,
            cursor,
            focused: false,
            tab_strip: false,
            text,
            frame,
            layer,
            owner,
        }
    }

    /// The font metrics text is shaped at this frame.
    pub fn metrics(&self) -> Metrics {
        self.text.metrics
    }

    /// Fill `r` under the text.
    pub fn fill(&mut self, r: Rect, color: [u8; 3]) {
        self.fill_alpha(r, color, 1.0);
    }

    /// Fill `r` under the text, translucent.
    pub fn fill_alpha(&mut self, r: Rect, color: [u8; 3], alpha: f32) {
        let q = self.frame.quad(r, color, alpha);
        match self.layer {
            Layer::Base => self.frame.bg.push(q),
            Layer::Overlay => self.frame.top.push(q),
        }
    }

    /// Fill `r` over the text (cursors, carets).
    pub fn fill_top(&mut self, r: Rect, color: [u8; 3], alpha: f32) {
        let q = self.frame.quad(r, color, alpha);
        self.frame.top.push(q);
    }

    /// A `BORDER`-thick outline just inside `r`, under the text.
    pub fn border(&mut self, r: Rect, color: [u8; 3]) {
        let (sw, sh) = self.frame.screen;
        let out = match self.layer {
            Layer::Base => &mut self.frame.bg,
            Layer::Overlay => &mut self.frame.top,
        };
        push_border(out, r, sw, sh, color);
    }

    /// Whether the mouse is over `r`.
    pub fn hovered(&self, r: Rect) -> bool {
        rect_contains(r, self.cursor.0, self.cursor.1)
    }

    /// The standard hover/selection highlight over `r`.
    pub fn highlight(&mut self, r: Rect) {
        self.fill_alpha(r, theme::HOVER_BG, theme::HOVER_ALPHA);
    }

    /// Register a clickable button; [`View::button`](super::View::button) gets
    /// `id` when it's clicked. Returns whether it is hovered (the caller styles
    /// it; [`highlight`](Self::highlight) is the standard hover look).
    pub fn button(&mut self, r: Rect, id: u32) -> bool {
        self.frame.view_buttons.push((r, self.owner, id));
        self.hovered(r)
    }

    /// Shape `text` (one colour, set at placement) into a scratch slot `width`
    /// wide. Place it with [`place`](Self::place); the returned width lets the
    /// caller centre or right-align it first.
    pub fn shape(&mut self, text: &str, family: Family, width: f32) -> Shaped {
        self.text.shape_scratch(&[(text, [0, 0, 0])], family, width, false)
    }

    /// Shape per-span coloured text into a scratch slot `width` wide.
    pub fn shape_spans(&mut self, spans: &[(String, [u8; 3])], family: Family, width: f32) -> Shaped {
        let spans: Vec<(&str, [u8; 3])> = spans.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        self.text.shape_scratch(&spans, family, width, true)
    }

    /// Draw shaped text with its top-left at `(left, top)`, clipped to `clip`.
    pub fn place(&mut self, shaped: Shaped, left: f32, top: f32, clip: Rect, color: [u8; 3]) {
        self.push_text(TextItem {
            src: TextSrc::Scratch(shaped.idx),
            left,
            top,
            bounds: bounds(clip),
            color,
        });
    }

    /// Shape and draw one line of `text` starting at `(left, top)`, clipped to
    /// `clip`. Returns its width.
    pub fn label(
        &mut self,
        text: &str,
        family: Family,
        left: f32,
        top: f32,
        clip: Rect,
        color: [u8; 3],
    ) -> f32 {
        let width = (clip.x + clip.w - left).max(1.0);
        let s = self.shape(text, family, width);
        self.place(s, left, top, clip, color);
        s.width
    }

    /// Content key for a monospace row of `(text, colour)` runs — cheap (hashes
    /// in place), so callers can test the cache before building spans.
    pub fn row_key<'k>(&self, cells: impl Iterator<Item = (&'k str, [u8; 3])>) -> u64 {
        self.text.row_cache.row_key(cells)
    }

    /// Draw a monospace row of coloured `spans` through the row cache (shaped
    /// once per distinct content, reused across frames and positions). Always
    /// shaped this frame — for rows few enough never to stall a frame.
    #[allow(clippy::too_many_arguments)]
    pub fn row(
        &mut self,
        spans: &[(String, [u8; 3])],
        left: f32,
        top: f32,
        shape_w: f32,
        clip: Rect,
        color: [u8; 3],
    ) {
        let key = self.row_key(spans.iter().map(|(s, c)| (s.as_str(), *c)));
        let (m, lh) = (self.text.metrics, self.text.line_h);
        let kit = &mut *self.text;
        kit.row_cache.ensure(key, &mut kit.font_system, m, shape_w, lh, spans);
        self.push_text(TextItem {
            src: TextSrc::Row(key),
            left,
            top,
            bounds: bounds(clip),
            color,
        });
    }

    /// Draw the row with content `key` through the row cache, shaping `spans()`
    /// on a miss only while this frame's shaping budget lasts. Past it, the row
    /// shows what this screen position drew last frame (or nothing on a first
    /// reveal) and the frame is marked for a follow-up — so a fast scroll into
    /// fresh content never stalls a frame. For bulk rows (terminal grid, editor
    /// text).
    #[allow(clippy::too_many_arguments)]
    pub fn budgeted_row(
        &mut self,
        key: u64,
        spans: impl FnOnce() -> Vec<(String, [u8; 3])>,
        left: f32,
        top: f32,
        shape_w: f32,
        clip: Rect,
        color: [u8; 3],
    ) {
        let place_key = self.text.budgeted_key(key, (clip.x as i32, top as i32), shape_w, spans);
        self.push_text(TextItem {
            src: TextSrc::Row(place_key),
            left,
            top,
            bounds: bounds(clip),
            color,
        });
    }

    /// Ask for another (paced) frame after this one, e.g. to continue an eased
    /// scroll.
    pub fn request_frame(&mut self) {
        self.frame.wants_frame = true;
    }

    fn push_text(&mut self, item: TextItem) {
        match self.layer {
            Layer::Base => self.frame.text.push(item),
            Layer::Overlay => self.frame.overlay_text.push(item),
        }
    }
}
