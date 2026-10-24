//! Immediate-mode painting for views (and the app's own chrome).
//!
//! A frame is built into a [`Frame`]: quads in two layers (under text, over
//! text), images, and text items that reference either the content-keyed
//! [`RowCache`], the [`BlockCache`], or a per-frame scratch buffer pool in the
//! shared [`TextKit`].
//! The app turns the frame into GPU draws after every view has painted. Nothing
//! here touches the GPU, so views can be painted (and tested) headless.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use ghostrealm_core::{Chrome, Config, Rect, SurfaceId};
use glyphon::cosmic_text::Align;
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, TextBounds, Weight};
use winit::window::CursorIcon;

use super::{rect_contains, theme, UiMetrics};
use crate::block_cache::{BlockCache, BlockLayout, BlockSpan};
use crate::row_cache::{RowCache, RunStyle};

/// Shape at least this many missed rows per frame regardless of the time budget,
/// so a backlog always makes forward progress even on an already-slow frame.
const MIN_SHAPES_PER_FRAME: usize = 8;

/// Soft cap on cached blocks ([`BlockCache`]): a few screens of a long
/// document at two widths.
const BLOCK_CACHE_CAP: usize = 1024;

/// Focused-pane / panel border thickness, in physical pixels.
pub const BORDER: f32 = 2.0;

/// One quad the GPU draws: a plain fill, a rounded rect with an optional
/// border, or a segment, by its shape fields:
///
/// - `radius` and `border_w` both 0: a plain fill of the rect.
/// - `radius` > 0 or `border_w` > 0: a rect with rounded corners and a border
///   `border_w` wide inside it, in `border_color`.
/// - `radius` < 0: a segment `-radius` px either side of the line between two
///   endpoints, held in `border_color` as `[ax, ay, bx, by]` (px from the
///   rect's top-left); `border_w` 0 gives round ends, > 0 flat ends exactly
///   at the endpoints (an oriented box). The rect must contain the shape.
///
/// Shaped quads are antialiased, and draw one pixel past their rect for it.
/// They also draw only inside `clip`; a plain fill ignores it (clip its rect
/// instead, as [`PaintCx::with_clip`] does).
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadInstance {
    /// Top-left in NDC.
    pub pos: [f32; 2],
    /// Width/height in NDC (height negative to grow downward).
    pub size: [f32; 2],
    /// Fill: linear RGB, straight alpha.
    pub color: [f32; 4],
    /// Width/height in physical px, so the shader measures shapes in pixels
    /// without knowing the surface size.
    pub px: [f32; 2],
    /// Corner radius in physical px, or minus a segment's half-width.
    pub radius: f32,
    /// Border width in physical px, or a segment's end style.
    pub border_w: f32,
    /// Border colour (linear RGB, straight alpha; equal to `color` when there
    /// is no border, so the antialiased edge blends into the fill alone), or a
    /// segment's endpoints.
    pub border_color: [f32; 4],
    /// `[x0, y0, x1, y1]` in physical px: a shaped quad's pixels outside it
    /// aren't drawn. [`NO_CLIP`] by default.
    pub clip: [f32; 4],
}

/// A [`QuadInstance::clip`] that clips nothing.
pub const NO_CLIP: [f32; 4] = [f32::MIN, f32::MIN, f32::MAX, f32::MAX];

/// The overlap of `a` and `b`, if they overlap.
pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

/// A solid quad covering `r` (physical px) on a `sw`x`sh` surface.
pub fn rect_quad(r: Rect, sw: f32, sh: f32, color: [u8; 3], alpha: f32) -> QuadInstance {
    let ndc_x = r.x / sw * 2.0 - 1.0;
    let ndc_y = 1.0 - r.y / sh * 2.0;
    let ndc_w = r.w / sw * 2.0;
    let ndc_h = -(r.h / sh * 2.0);
    let color = linear(color, alpha);
    QuadInstance {
        pos: [ndc_x, ndc_y],
        size: [ndc_w, ndc_h],
        color,
        px: [r.w, r.h],
        radius: 0.0,
        border_w: 0.0,
        border_color: color,
        clip: NO_CLIP,
    }
}

/// A quad covering `r` with corners rounded to `radius` (clamped to half the
/// shorter side) and an optional `(width, colour, alpha)` border inside it,
/// antialiased. A zero alpha fill with a border draws an outline. With no
/// radius and no border it is a plain fill, with hard pixel edges.
pub fn round_quad(
    r: Rect,
    sw: f32,
    sh: f32,
    radius: f32,
    color: [u8; 3],
    alpha: f32,
    border: Option<(f32, [u8; 3], f32)>,
) -> QuadInstance {
    let mut q = rect_quad(r, sw, sh, color, alpha);
    q.radius = radius.max(0.0);
    if let Some((w, c, a)) = border.filter(|(w, _, _)| *w > 0.0) {
        q.border_w = w;
        q.border_color = linear(c, a);
    }
    q
}

/// A segment from `a` to `b` (physical px), `width` wide, with round ends or
/// `flat` ends exactly at `a` and `b`. Antialiased, and centred on the line
/// between them.
#[allow(clippy::too_many_arguments)]
pub fn line_quad(
    a: (f32, f32),
    b: (f32, f32),
    width: f32,
    flat: bool,
    sw: f32,
    sh: f32,
    color: [u8; 3],
    alpha: f32,
) -> QuadInstance {
    // A zero half-width would read as a plain fill.
    let hw = (width / 2.0).max(1e-3);
    let x = a.0.min(b.0) - hw;
    let y = a.1.min(b.1) - hw;
    let r = Rect { x, y, w: (a.0 - b.0).abs() + 2.0 * hw, h: (a.1 - b.1).abs() + 2.0 * hw };
    let mut q = rect_quad(r, sw, sh, color, alpha);
    q.radius = -hw;
    q.border_w = if flat { 1.0 } else { 0.0 };
    q.border_color = [a.0 - x, a.1 - y, b.0 - x, b.1 - y];
    q
}

fn linear(c: [u8; 3], alpha: f32) -> [f32; 4] {
    [
        srgb_to_linear(c[0]) as f32,
        srgb_to_linear(c[1]) as f32,
        srgb_to_linear(c[2]) as f32,
        alpha,
    ]
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

/// A face to shape text in: a family plus italic/bold. `Family` converts, so
/// `cx.shape(text, Family::SansSerif, w)` reads as plain sans.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Font {
    pub family: Family<'static>,
    pub italic: bool,
    pub bold: bool,
}

impl Font {
    pub const MONO: Font = Font {
        family: Family::Monospace,
        italic: false,
        bold: false,
    };
    pub const SANS: Font = Font {
        family: Family::SansSerif,
        italic: false,
        bold: false,
    };
    /// The Preview's body face: the app points `Family::Serif` at
    /// `[editor] preview_font` if it is installed, else the platform's serif
    /// (New York, else Georgia, on macOS; fontconfig's elsewhere), else an
    /// installed sans.
    pub const SERIF: Font = Font {
        family: Family::Serif,
        italic: false,
        bold: false,
    };

    pub fn italic(self, on: bool) -> Self {
        Font { italic: on, ..self }
    }

    pub fn bold(self, on: bool) -> Self {
        Font { bold: on, ..self }
    }

    pub(crate) fn attrs(self) -> Attrs<'static> {
        let mut a = Attrs::new().family(self.family);
        if self.italic {
            a = a.style(Style::Italic);
        }
        if self.bold {
            a = a.weight(Weight::BOLD);
        }
        a
    }
}

impl From<Family<'static>> for Font {
    fn from(family: Family<'static>) -> Self {
        Font {
            family,
            italic: false,
            bold: false,
        }
    }
}

/// Where a text item's shaped buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSrc {
    /// A content-keyed row in the [`RowCache`].
    Row(u64),
    /// A slot in the [`TextKit`]'s per-frame scratch pool.
    Scratch(usize),
    /// A wrapped block in the [`BlockCache`], by slot key.
    Block(u64),
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
    /// How much larger than shaped the glyphs draw, from `(left, top)` (a
    /// heading row). The renderer rasterises at this size, so the row stays
    /// shaped (and cached) at the frame's metrics.
    pub scale: f32,
}

/// Pixels a view shows with [`PaintCx::image`]: tightly packed BGRA, sRGB-encoded,
/// with alpha premultiplied in linear light (each colour channel is
/// `encode(decode(c) * a)`; [`Image::from_rgba`] premultiplies decoded pixels so).
/// The renderer samples them as sRGB, so a channel reads back as linear light
/// times alpha, which is what premultiplied blending in linear light needs.
/// Premultiplied pixels filter and blend without dark fringes where opaque meets
/// transparent.
pub trait ImageSource {
    /// `(width, height)` in physical pixels.
    fn size(&self) -> (u32, u32);
    /// `width * height * 4` bytes, or `None` if the pixels are gone.
    fn bgra(&self) -> Option<&[u8]>;
    /// Smaller copies for drawing below full size, each half the size of the
    /// last: `(width, height, bgra)`. Without them a shrunk image aliases.
    fn mips(&self) -> &[(u32, u32, Vec<u8>)] {
        &[]
    }
    /// Draw without blending, ignoring alpha (a web page's frame).
    fn opaque(&self) -> bool {
        false
    }
}

/// Decoded pixels with a box-filtered mip chain, so they stay smooth drawn
/// smaller than their size. Build one where decoding happens (a worker), not in
/// `paint`, and pass [`id`](Self::id) as the `version` to [`PaintCx::image`].
pub struct Image {
    id: u64,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    /// Each half the size of the last, down to 1x1.
    mips: Vec<(u32, u32, Vec<u8>)>,
}

impl Image {
    /// An image from `width` x `height` straight-alpha RGBA8 pixels (what the
    /// `image` crate decodes to), premultiplied in linear light before its mips
    /// are built. A zero width or height gives one transparent pixel.
    pub fn from_rgba(width: u32, height: u32, mut rgba: Vec<u8>) -> Arc<Image> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        assert_eq!(rgba.len(), width as usize * height as usize * 4, "RGBA8 size");
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
            let a = px[3];
            if a != 255 {
                let a = a as f32 / 255.0;
                for c in &mut px[..3] {
                    *c = encode(DECODE[*c as usize] * a);
                }
            }
        }
        // An empty image is one transparent pixel, so it still has a size to fit.
        let (width, height) = if width == 0 || height == 0 {
            rgba = vec![0; 4];
            (1, 1)
        } else {
            (width, height)
        };
        let mut mips: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let (mut w, mut h) = (width, height);
        while w > 1 || h > 1 {
            let next = halve(w, h, mips.last().map_or(&rgba, |m| &m.2));
            (w, h) = (next.0, next.1);
            mips.push(next);
        }
        Arc::new(Image {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            width,
            height,
            bgra: rgba,
            mips,
        })
    }

    /// Unique per image.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The largest rect with the image's aspect ratio that fits `r`, centred,
    /// never scaled above `max_scale` (1.0 keeps small images pixel-sized).
    pub fn fit(&self, r: Rect, max_scale: f32) -> Rect {
        let (iw, ih) = (self.width as f32, self.height as f32);
        let s = (r.w / iw).min(r.h / ih).min(max_scale).max(0.0);
        let (w, h) = (iw * s, ih * s);
        Rect {
            x: (r.x + (r.w - w) * 0.5).round(),
            y: (r.y + (r.h - h) * 0.5).round(),
            w: w.round(),
            h: h.round(),
        }
    }
}

impl ImageSource for Image {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn bgra(&self) -> Option<&[u8]> {
        Some(&self.bgra)
    }

    fn mips(&self) -> &[(u32, u32, Vec<u8>)] {
        &self.mips
    }
}

/// The next mip level: each pixel the average of (up to) a 2x2 block, in
/// linear light (averaging encoded values darkens fine detail).
fn halve(w: u32, h: u32, px: &[u8]) -> (u32, u32, Vec<u8>) {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = Vec::with_capacity(nw as usize * nh as usize * 4);
    for y in 0..nh {
        let (y0, y1) = ((y * 2).min(h - 1), (y * 2 + 1).min(h - 1));
        for x in 0..nw {
            let (x0, x1) = ((x * 2).min(w - 1), (x * 2 + 1).min(w - 1));
            let block = [(x0, y0), (x1, y0), (x0, y1), (x1, y1)];
            let at = |(x, y): (u32, u32), c: usize| px[(y as usize * w as usize + x as usize) * 4 + c];
            for c in 0..3 {
                let sum: f32 = block.iter().map(|p| DECODE[at(*p, c) as usize]).sum();
                out.push(encode(sum / 4.0));
            }
            let alpha: u32 = block.iter().map(|p| at(*p, 3) as u32).sum();
            out.push(((alpha + 2) / 4) as u8);
        }
    }
    (nw, nh, out)
}

/// Each sRGB-encoded byte value in linear light.
static DECODE: LazyLock<[f32; 256]> = LazyLock::new(|| std::array::from_fn(|i| srgb_to_linear(i as u8) as f32));

/// The linear values halfway between consecutive encoded bytes: a linear value
/// encodes to the number of these below it.
static ENCODE_STEPS: LazyLock<[f32; 255]> =
    LazyLock::new(|| std::array::from_fn(|i| (DECODE[i] + DECODE[i + 1]) / 2.0));

/// The sRGB-encoded byte nearest (in linear light) to linear value `l`.
/// `encode(DECODE[c]) == c` for every byte `c`.
fn encode(l: f32) -> u8 {
    ENCODE_STEPS.partition_point(|s| *s < l) as u8
}

/// An image placed this frame. The app keeps one GPU texture per
/// `(owner, id)` and re-uploads it only when `version` changes.
#[derive(Clone)]
pub struct ImageItem {
    pub owner: SurfaceId,
    pub id: u32,
    pub version: u64,
    pub source: Arc<dyn ImageSource>,
    /// Where it draws (physical px), stretched to fit.
    pub rect: Rect,
    pub clip: Rect,
}

/// Which pass a view paints into. Views in panes paint the base layer; a view
/// hosted in a modal (the picker) paints the overlay layer, above all
/// pane content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Base,
    Overlay,
}

/// Everything painted this frame, in draw order: `bg` quads, then `images`, then
/// `text`, then `top` quads (cursors, borders, overlay panels), then
/// `overlay_images`, then `overlay_text`.
pub struct Frame {
    /// Surface size in physical pixels.
    pub screen: (f32, f32),
    pub bg: Vec<QuadInstance>,
    pub images: Vec<ImageItem>,
    pub text: Vec<TextItem>,
    pub top: Vec<QuadInstance>,
    pub overlay_images: Vec<ImageItem>,
    pub overlay_text: Vec<TextItem>,
    /// The mouse cursor a view asked for, and the rect it applies to.
    pub cursor: Option<(Rect, CursorIcon)>,
    /// Buttons views registered: (hit rect, owning surface, view-local id).
    pub view_buttons: Vec<(Rect, SurfaceId, u32)>,
    /// A view needs another paced frame (an animation step, deferred work).
    pub wants_frame: bool,
    /// The hovered tooltip: its anchor, text (lines split on `\n`), and the
    /// layer that asked. The app shows it once the hover has lasted a moment.
    pub tooltip: Option<(Rect, String, Layer)>,
}

impl Frame {
    pub fn new(screen: (f32, f32)) -> Self {
        Frame {
            screen,
            bg: Vec::new(),
            images: Vec::new(),
            text: Vec::new(),
            top: Vec::new(),
            overlay_images: Vec::new(),
            overlay_text: Vec::new(),
            cursor: None,
            view_buttons: Vec::new(),
            wants_frame: false,
            tooltip: None,
        }
    }

    pub fn quad(&self, r: Rect, color: [u8; 3], alpha: f32) -> QuadInstance {
        rect_quad(r, self.screen.0, self.screen.1, color, alpha)
    }
}

/// The app's shared text machinery: the one `FontSystem`, the content-keyed row
/// and block caches, a per-frame scratch buffer pool, and the per-frame shaping
/// budget.
pub struct TextKit {
    pub font_system: FontSystem,
    pub row_cache: RowCache,
    pub block_cache: BlockCache,
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
            block_cache: BlockCache::new(BLOCK_CACHE_CAP),
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
        self.block_cache.begin_frame(metrics_gen);
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
    /// and block caches' own `end_frame` runs after it.
    pub fn finish_paint(&mut self) -> bool {
        std::mem::swap(&mut self.prev_rows, &mut self.cur_rows);
        self.deferred
    }

    /// The buffer a text item draws.
    pub fn buffer(&self, src: TextSrc) -> Option<&Buffer> {
        match src {
            TextSrc::Row(key) => self.row_cache.buffer(key),
            TextSrc::Scratch(idx) => self.scratch.get(idx),
            TextSrc::Block(slot) => self.block_cache.buffer(slot),
        }
    }

    /// Whether the frame's shaping budget allows another cache miss.
    fn may_shape(&self) -> bool {
        self.shaped < MIN_SHAPES_PER_FRAME || Instant::now() < self.shape_deadline
    }

    /// Content `key` laid out `width` wide: its slot key and layout, shaping
    /// `spans()` on a miss while the frame's budget lasts. Past it the frame is
    /// marked deferred and, with `stale`, the block's layout at the nearest
    /// other width stands in; `None` if there is none.
    fn budgeted_block(
        &mut self,
        key: u64,
        width: f32,
        stale: bool,
        spans: impl FnOnce() -> Vec<BlockSpan>,
    ) -> Option<(u64, BlockLayout)> {
        if let Some(hit) = self.block_cache.touch(key, width) {
            return Some(hit);
        }
        if self.may_shape() {
            self.shaped += 1;
            return Some(self.block_cache.shape(key, &mut self.font_system, width, spans));
        }
        self.deferred = true;
        if stale {
            self.block_cache.touch_nearest(key, width)
        } else {
            None
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
        styles: &[RunStyle],
        spans: impl FnOnce() -> Vec<(String, [u8; 3])>,
    ) -> u64 {
        let (m, lh) = (self.metrics, self.line_h);
        let place_key = if self.row_cache.buffer(key).is_some() {
            // Hit: no shaping, just refresh recency.
            self.row_cache.ensure(key, &mut self.font_system, m, shape_w, lh, &[], &[]);
            key
        } else if self.may_shape() {
            let spans = spans();
            self.row_cache.ensure(key, &mut self.font_system, m, shape_w, lh, &spans, styles);
            self.shaped += 1;
            key
        } else {
            self.deferred = true;
            match self.prev_rows.get(&pos).copied() {
                Some(prev) if self.row_cache.buffer(prev).is_some() => {
                    self.row_cache.ensure(prev, &mut self.font_system, m, shape_w, lh, &[], &[]);
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
        font: Font,
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
                let mut a = font.attrs();
                if bake_colors {
                    a = a.color(Color::rgb(c[0], c[1], c[2]));
                }
                (*t, a)
            }),
            &font.attrs(),
            Shaping::Advanced,
            // With no alignment an RTL line is right-aligned to the buffer's
            // width, past where callers place and clip it.
            Some(Align::Left),
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

/// The per-span styles of a row passed to the row-drawing calls: one
/// [`RunStyle`] per span, the monospace cell width the decorations measure
/// spans with, and the row's draw [`scale`](TextItem::scale). A row with no
/// styled span passes [`NONE`](Self::NONE) (or [`scaled`](Self::scaled) it),
/// which carries nothing and costs nothing.
#[derive(Clone, Copy, Debug)]
pub struct RowStyles<'a> {
    pub runs: &'a [RunStyle],
    /// The cell width at scale 1: decorations multiply it by `scale`.
    pub cell_w: f32,
    pub scale: f32,
}

impl<'a> RowStyles<'a> {
    /// An unstyled row.
    pub const NONE: RowStyles<'static> = RowStyles { runs: &[], cell_w: 0.0, scale: 1.0 };

    pub fn new(runs: &'a [RunStyle], cell_w: f32) -> Self {
        RowStyles { runs, cell_w, scale: 1.0 }
    }

    /// These styles on a row drawn `scale` times its shaped size.
    pub fn scaled(self, scale: f32) -> Self {
        RowStyles { scale, ..self }
    }
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
    /// A modal overlay (palette, menu, picker, confirmation) covers the panes:
    /// [`above`](Self::above) paints nothing, and a view should close its
    /// popups.
    pub modal: bool,
    pub(crate) text: &'a mut TextKit,
    pub(crate) frame: &'a mut Frame,
    pub(crate) layer: Layer,
    pub(crate) owner: SurfaceId,
    /// Quads draw only inside this, if set ([`with_clip`](Self::with_clip)).
    clip: Option<Rect>,
    /// Added to button and image ids ([`with_id_base`](Self::with_id_base)).
    id_base: u32,
    /// Quads go over the text ([`over_text`](Self::over_text)).
    over_text: bool,
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
            modal: false,
            text,
            frame,
            layer,
            owner,
            clip: None,
            id_base: 0,
            over_text: false,
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
        self.push_quad(q, false);
    }

    /// Paint `f` with its quads (fills, borders, rounded rects, strokes) and
    /// button hit rects clipped to `clip`, within any clip already set. Text
    /// and images keep the clip each call passes. A rounded rect keeps its shape inside the clip;
    /// shrinking its rect instead would move its corners.
    pub fn with_clip(&mut self, clip: Rect, f: impl FnOnce(&mut Self)) {
        let inner = match self.clip {
            Some(c) => intersect(c, clip).unwrap_or(Rect { x: clip.x, y: clip.y, w: 0.0, h: 0.0 }),
            None => clip,
        };
        let outer = self.clip.replace(inner);
        f(self);
        self.clip = outer;
    }

    /// Paint `f` with `base` added to the ids of the buttons and images it
    /// registers, on top of any base already set: a view hosting other views
    /// gives each its own id range, so their ids can't collide, and tells
    /// them apart in [`View::button`](super::View::button) by range.
    pub fn with_id_base<T>(&mut self, base: u32, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = self.id_base;
        self.id_base = outer + base;
        let out = f(self);
        self.id_base = outer;
        out
    }

    /// Paint `f` with its quads over the panes' text, as
    /// [`fill_top`](Self::fill_top) draws: a control floating on text (a code
    /// block's copy button). Text placed in `f` still draws under them, and
    /// the app's own top items (focus border, overlays) draw over them.
    pub fn over_text(&mut self, f: impl FnOnce(&mut Self)) {
        let outer = std::mem::replace(&mut self.over_text, true);
        f(self);
        self.over_text = outer;
    }

    /// `r` within the clip, or `None` if nothing of it shows.
    fn clipped(&self, r: Rect) -> Option<Rect> {
        match self.clip {
            Some(c) => intersect(r, c),
            None => Some(r),
        }
    }

    /// Push a quad to this layer (or over the text), clipped: a plain fill's
    /// rect is cut here (the shader doesn't clip one), a shaped quad carries
    /// the clip to the shader. A quad with nothing inside the clip is dropped.
    fn push_quad(&mut self, mut q: QuadInstance, top: bool) {
        if let Some(c) = self.clip {
            let (sw, sh) = self.frame.screen;
            let r = Rect {
                x: (q.pos[0] + 1.0) / 2.0 * sw,
                y: (1.0 - q.pos[1]) / 2.0 * sh,
                w: q.px[0].abs(),
                h: q.px[1].abs(),
            };
            if q.radius == 0.0 && q.border_w == 0.0 {
                let Some(cut) = intersect(r, c) else { return };
                if cut != r {
                    let placed = rect_quad(cut, sw, sh, [0; 3], 0.0);
                    (q.pos, q.size, q.px) = (placed.pos, placed.size, placed.px);
                }
            } else {
                // The pixel of antialiasing past the rect can reach into the clip.
                let bounds = Rect { x: r.x - 1.0, y: r.y - 1.0, w: r.w + 2.0, h: r.h + 2.0 };
                if intersect(bounds, c).is_none() {
                    return;
                }
                q.clip = [c.x, c.y, c.x + c.w, c.y + c.h];
            }
        }
        match (top || self.over_text, self.layer) {
            (false, Layer::Base) => self.frame.bg.push(q),
            _ => self.frame.top.push(q),
        }
    }

    /// Fill `r` under the text with corners rounded to `radius` and an optional
    /// `(width, colour, alpha)` border inside it (pills, boxes, outlines; a
    /// circle is a square with `radius` half its side). An `alpha` of 0 with a
    /// border draws just the outline. Snap `r` to whole pixels: shaped quads
    /// sharing a fractional edge leave a seam.
    pub fn round_rect(
        &mut self,
        r: Rect,
        radius: f32,
        fill: [u8; 3],
        alpha: f32,
        border: Option<(f32, [u8; 3], f32)>,
    ) {
        let (sw, sh) = self.frame.screen;
        let q = round_quad(r, sw, sh, radius, fill, alpha, border);
        self.push_quad(q, false);
    }

    /// A `width`-wide stroke from `a` to `b` with round ends, under the text,
    /// centred on the line between them: a 1 px line along a whole-pixel
    /// coordinate covers two pixels at half strength, so put it on a pixel's
    /// centre (`n + 0.5`), or draw a straight one as a snapped
    /// [`fill`](Self::fill). Joints of a polyline overlap, so draw one opaque:
    /// a translucent one is more opaque where its segments meet.
    pub fn line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, color: [u8; 3], alpha: f32) {
        self.push_line(a, b, width, false, color, alpha);
    }

    /// Like [`line`](Self::line) with flat ends exactly at `a` and `b`: a
    /// rotated rect `width` wide, centred on the line between them.
    pub fn bar(&mut self, a: (f32, f32), b: (f32, f32), width: f32, color: [u8; 3], alpha: f32) {
        self.push_line(a, b, width, true, color, alpha);
    }

    fn push_line(&mut self, a: (f32, f32), b: (f32, f32), width: f32, flat: bool, color: [u8; 3], alpha: f32) {
        let (sw, sh) = self.frame.screen;
        let q = line_quad(a, b, width, flat, sw, sh, color, alpha);
        self.push_quad(q, false);
    }

    /// A wavy underline from `x0` to `x1` over the text, its lowest edge at
    /// `bottom` (a diagnostic): a zigzag of [`line`](Self::line) segments,
    /// within the clip. Opaque, because a translucent stroke darkens where
    /// its segments overlap at the joints.
    pub fn squiggle(&mut self, x0: f32, x1: f32, bottom: f32, color: [u8; 3]) {
        let s = self.ui.scale;
        // Half a period, the amplitude, and the stroke width.
        let (half, amp, width) = (2.0 * s, 1.5 * s, 1.0 * s);
        let low = bottom - width / 2.0;
        let y = |i: usize| if i % 2 == 0 { low } else { low - 2.0 * amp };
        self.over_text(|cx| {
            let (mut x, mut i) = (x0, 0);
            while x < x1 {
                let next = (x + half).min(x1);
                let (ya, yb) = (y(i), y(i + 1));
                let end = ya + (yb - ya) * (next - x) / half;
                cx.line((x, ya), (next, end), width, color, 1.0);
                (x, i) = (next, i + 1);
            }
        });
    }

    /// Fill `r` over the text (cursors, carets).
    pub fn fill_top(&mut self, r: Rect, color: [u8; 3], alpha: f32) {
        let q = self.frame.quad(r, color, alpha);
        self.push_quad(q, true);
    }

    /// A `BORDER`-thick outline just inside `r`, under the text.
    pub fn border(&mut self, r: Rect, color: [u8; 3]) {
        let t = BORDER;
        for edge in [
            Rect { x: r.x, y: r.y, w: r.w, h: t },
            Rect { x: r.x, y: r.y + r.h - t, w: r.w, h: t },
            Rect { x: r.x, y: r.y, w: t, h: r.h },
            Rect { x: r.x + r.w - t, y: r.y, w: t, h: r.h },
        ] {
            self.fill_alpha(edge, color, 1.0);
        }
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
    /// Under [`with_clip`](Self::with_clip) only the part inside the clip
    /// takes clicks.
    pub fn button(&mut self, r: Rect, id: u32) -> bool {
        let Some(r) = self.clipped(r) else { return false };
        self.frame.view_buttons.push((r, self.owner, self.id_base + id));
        self.hovered(r)
    }

    /// Show `text` (lines split on `\n`) as a tooltip while `r` stays hovered —
    /// for icon buttons with no label.
    pub fn tooltip(&mut self, r: Rect, text: impl Into<String>) {
        if self.hovered(r) {
            self.frame.tooltip = Some((r, text.into(), self.layer));
        }
    }

    /// Draw `source` stretched over `rect`, clipped to `clip`: above this
    /// layer's fills, under its text. `id` names the image within the view; bump
    /// `version` whenever the pixels change (the app re-uploads only then).
    /// [`Image::fit`] keeps an aspect ratio.
    pub fn image(
        &mut self,
        id: u32,
        version: u64,
        source: Arc<dyn ImageSource>,
        rect: Rect,
        clip: Rect,
    ) {
        let item = ImageItem {
            owner: self.owner,
            id: self.id_base + id,
            version,
            source,
            rect,
            clip,
        };
        match self.layer {
            Layer::Base => self.frame.images.push(item),
            Layer::Overlay => self.frame.overlay_images.push(item),
        }
    }

    /// Show the `icon` mouse cursor while `r` is hovered.
    pub fn cursor(&mut self, r: Rect, icon: CursorIcon) {
        if self.hovered(r) {
            self.frame.cursor = Some((r, icon));
        }
    }

    /// Shape `text` (one colour, set at placement) into a scratch slot `width`
    /// wide. Place it with [`place`](Self::place); the returned width lets the
    /// caller centre or right-align it first.
    pub fn shape(&mut self, text: &str, font: impl Into<Font>, width: f32) -> Shaped {
        self.text.shape_scratch(&[(text, [0, 0, 0])], font.into(), width, false)
    }

    /// Shape per-span coloured text into a scratch slot `width` wide.
    pub fn shape_spans(
        &mut self,
        spans: &[(String, [u8; 3])],
        font: impl Into<Font>,
        width: f32,
    ) -> Shaped {
        let spans: Vec<(&str, [u8; 3])> = spans.iter().map(|(s, c)| (s.as_str(), *c)).collect();
        self.text.shape_scratch(&spans, font.into(), width, true)
    }

    /// How far below the top of the row-cache row `key` its baseline sits,
    /// as shaped (drawn at a scale `s`, `s` times as far); `None` until it is
    /// shaped. Its faces (bold, a fallback font) set it.
    pub fn row_baseline(&self, key: u64) -> Option<f32> {
        self.text.row_cache.buffer(key)?.layout_runs().next().map(|r| r.line_y)
    }

    /// Where bytes `bytes` of the row-cache row `key` were drawn, as
    /// shaped: `(x, width)` from the row's left edge, one span per visually
    /// contiguous piece (a range crossing a change of direction can be two).
    /// Empty until the row is shaped.
    pub fn row_spans_x(&self, key: u64, bytes: std::ops::Range<usize>) -> Vec<(f32, f32)> {
        use glyphon::cosmic_text::Cursor;
        let Some(buf) = self.text.row_cache.buffer(key) else { return Vec::new() };
        buf.layout_runs()
            .flat_map(|run| run.highlight(Cursor::new(run.line_i, bytes.start), Cursor::new(run.line_i, bytes.end)))
            .collect()
    }

    /// How far below a monospace row's top its baseline sits, at the frame's
    /// metrics: what lines up text of two sizes (a row drawn at a scale `s`
    /// has its baseline `s` times as far down).
    pub fn mono_baseline(&mut self) -> f32 {
        let s = self.shape("0", Font::MONO, self.ui.cell_w * 4.0);
        let run = self.text.buffer(TextSrc::Scratch(s.idx)).and_then(|b| b.layout_runs().next().map(|r| r.line_y));
        run.unwrap_or(self.text.line_h * 0.8)
    }

    /// `text` as it fits in `avail` (physical px) in `font`: whole, else cut
    /// at a cluster boundary with an ellipsis after it (just the ellipsis
    /// when nothing else fits). Measured from one shaping of the whole text.
    pub fn cut_to_fit(&mut self, text: &str, font: impl Into<Font>, avail: f32) -> String {
        let font = font.into();
        let whole = self.shape(text, font, avail.max(1.0));
        if whole.width <= avail {
            return text.to_string();
        }
        let ellipsis = self.shape("\u{2026}", font, avail.max(1.0)).width;
        let limit = avail - ellipsis;
        let cut = self
            .text
            .buffer(TextSrc::Scratch(whole.idx))
            .and_then(|b| {
                let mut glyphs = b.layout_runs().flat_map(|r| r.glyphs.iter());
                glyphs.find(|g| g.x + g.w > limit).map(|g| g.start)
            })
            .unwrap_or(text.len());
        format!("{}\u{2026}", text[..cut].trim_end())
    }

    /// Draw shaped text with its top-left at `(left, top)`, clipped to `clip`.
    pub fn place(&mut self, shaped: Shaped, left: f32, top: f32, clip: Rect, color: [u8; 3]) {
        self.push_text(TextItem {
            src: TextSrc::Scratch(shaped.idx),
            left,
            top,
            bounds: bounds(clip),
            color,
            scale: 1.0,
        });
    }

    /// Shape and draw one line of `text` starting at `(left, top)`, clipped to
    /// `clip`. Returns its width.
    pub fn label(
        &mut self,
        text: &str,
        font: impl Into<Font>,
        left: f32,
        top: f32,
        clip: Rect,
        color: [u8; 3],
    ) -> f32 {
        let width = (clip.x + clip.w - left).max(1.0);
        let s = self.shape(text, font, width);
        self.place(s, left, top, clip, color);
        s.width
    }

    /// Content key for a monospace row of `(text, colour)` runs — cheap (hashes
    /// in place), so callers can test the cache before building spans.
    pub fn row_key<'k>(&self, cells: impl Iterator<Item = (&'k str, [u8; 3])>) -> u64 {
        self.text.row_cache.row_key(cells)
    }

    /// [`row_key`](Self::row_key) for a row with per-span `styles`; with none
    /// (`RowStyles::NONE`) it is the same key.
    pub fn row_key_styled<'k>(
        &self,
        cells: impl Iterator<Item = (&'k str, [u8; 3])>,
        styles: RowStyles,
    ) -> u64 {
        self.text.row_cache.row_key_styled(cells, styles.runs)
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
        kit.row_cache.ensure(key, &mut kit.font_system, m, shape_w, lh, spans, &[]);
        self.push_text(TextItem {
            src: TextSrc::Row(key),
            left,
            top,
            bounds: bounds(clip),
            color,
            scale: 1.0,
        });
    }

    /// [`row`](Self::row) shaped in `font` (a sidebar's sans names): cached
    /// by content and font, so a row redrawn unchanged isn't reshaped.
    #[allow(clippy::too_many_arguments)]
    pub fn row_font(
        &mut self,
        spans: &[(String, [u8; 3])],
        font: Font,
        left: f32,
        top: f32,
        shape_w: f32,
        clip: Rect,
        color: [u8; 3],
    ) {
        let mut face = RunStyle::PLAIN;
        if font.bold {
            face = face | RunStyle::BOLD;
        }
        if font.italic {
            face = face | RunStyle::ITALIC;
        }
        let styles = if face.is_plain() { Vec::new() } else { vec![face; spans.len()] };
        let cells = spans.iter().map(|(s, c)| (s.as_str(), *c));
        let key = self.text.row_cache.row_key_in(cells, &styles, font.family);
        let (m, lh) = (self.text.metrics, self.text.line_h);
        let kit = &mut *self.text;
        kit.row_cache.ensure_in(key, &mut kit.font_system, m, shape_w, lh, spans, &styles, font.family);
        self.push_text(TextItem {
            src: TextSrc::Row(key),
            left,
            top,
            bounds: bounds(clip),
            color,
            scale: 1.0,
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
        self.budgeted_row_styled(key, spans, RowStyles::NONE, left, top, shape_w, clip, color);
    }

    /// [`budgeted_row`](Self::budgeted_row) for a row with styled spans:
    /// `styles` (from [`row_key_styled`](Self::row_key_styled)'s key) picks the
    /// bold/italic faces and draws underlines and `BOX` backgrounds as quads,
    /// all drawn at `styles.scale`. A row without runs (`RowStyles::NONE`,
    /// scaled or not) is the unstyled path and adds no work.
    #[allow(clippy::too_many_arguments)]
    pub fn budgeted_row_styled(
        &mut self,
        key: u64,
        spans: impl FnOnce() -> Vec<(String, [u8; 3])>,
        styles: RowStyles,
        left: f32,
        top: f32,
        shape_w: f32,
        clip: Rect,
        color: [u8; 3],
    ) {
        if !styles.runs.is_empty() {
            return self.styled_row(key, spans(), styles, left, top, shape_w, clip, color);
        }
        let place_key = self.text.budgeted_key(key, (clip.x as i32, top as i32), shape_w, &[], spans);
        self.push_text(TextItem {
            src: TextSrc::Row(place_key),
            left,
            top,
            bounds: bounds(clip),
            color,
            scale: styles.scale,
        });
    }

    /// The styled branch of [`budgeted_row_styled`](Self::budgeted_row_styled).
    #[allow(clippy::too_many_arguments)]
    fn styled_row(
        &mut self,
        key: u64,
        spans: Vec<(String, [u8; 3])>,
        styles: RowStyles,
        left: f32,
        top: f32,
        shape_w: f32,
        clip: Rect,
        color: [u8; 3],
    ) {
        let place_key = self.text.budgeted_key(
            key,
            (clip.x as i32, top as i32),
            shape_w,
            styles.runs,
            || spans.clone(),
        );
        // A row deferred by the shaping budget shows other (or no) text this
        // frame; its decorations would sit under the wrong glyphs.
        if place_key == key && self.text.row_cache.buffer(key).is_some() {
            self.decorate(&spans, styles, left, top, clip);
        }
        self.push_text(TextItem {
            src: TextSrc::Row(place_key),
            left,
            top,
            bounds: bounds(clip),
            color,
            scale: styles.scale,
        });
    }

    /// The quads under a styled row: a rounded, outlined code box behind each
    /// run of `BOX` spans and a thin line under `UNDERLINE` spans, each as
    /// wide as its characters at the styles' (scaled) cell width, clipped to
    /// `clip`.
    fn decorate(&mut self, spans: &[(String, [u8; 3])], styles: RowStyles, left: f32, top: f32, clip: Rect) {
        let line_h = self.text.line_h * styles.scale;
        let size = self.text.metrics.font_size * styles.scale;
        let cell_w = styles.cell_w * styles.scale;
        let thick = (size / 14.0).round().max(1.0);
        // Each span's x range, unclipped.
        let mut x = left;
        let ranges: Vec<(f32, f32)> = spans
            .iter()
            .map(|(text, _)| {
                let x0 = x;
                x += text.chars().count() as f32 * cell_w;
                (x0, x)
            })
            .collect();
        // One box per run of adjacent boxed spans (a code span split by colour
        // is one span of code), outset a little into the cells either side
        // (a code span's backticks), so it pads its text without moving the
        // grid. Drawn whole and clipped, so a cut box keeps its corners.
        let pad = (cell_w * 0.25).round().max(1.0);
        // A row not scrolled sideways has nothing left of its first column,
        // so a box starting there (code wrapped onto this row) keeps its
        // left edge in the padding before it.
        let box_clip = if left >= clip.x { Rect { x: clip.x - pad, w: clip.w + pad, ..clip } } else { clip };
        let inset = theme::hairline(self.ui);
        let border = Some((theme::hairline(self.ui), theme::CODE_BOX_BORDER, theme::CODE_BOX_BORDER_ALPHA));
        let radius = theme::box_radius(self.ui);
        let mut i = 0;
        while i < spans.len() {
            if !styles.runs[i].contains(RunStyle::BOX) {
                i += 1;
                continue;
            }
            let x0 = ranges[i].0;
            while i + 1 < spans.len() && styles.runs[i + 1].contains(RunStyle::BOX) {
                i += 1;
            }
            let x1 = ranges[i].1;
            i += 1;
            if x1 <= x0 {
                continue;
            }
            let (bx0, bx1) = ((x0 - pad).round(), (x1 + pad).round());
            let r = Rect { x: bx0, y: top + inset, w: bx1 - bx0, h: (line_h - 2.0 * inset).max(1.0) };
            self.with_clip(box_clip, |cx| {
                cx.round_rect(r, radius, theme::CODE_BOX_BG, theme::CODE_BOX_ALPHA, border);
            });
        }
        for (((_, color), style), (x0, x1)) in spans.iter().zip(styles.runs).zip(ranges) {
            let (x0, x1) = (x0.max(clip.x), x1.min(clip.x + clip.w));
            if x1 > x0 && style.contains(RunStyle::UNDERLINE) {
                let r = Rect { x: x0, y: top + line_h - 2.0 * thick, w: x1 - x0, h: thick };
                self.fill_alpha(r, *color, 1.0);
            }
        }
    }

    /// Content key for a block of `spans` ([`block`](Self::block)): their
    /// text, faces, sizes, colours, and tags.
    pub fn block_key(&self, spans: &[BlockSpan]) -> u64 {
        crate::block_cache::block_key(spans)
    }

    /// Draw a block of word-wrapped proportional text `width` wide with its
    /// top-left at `(left, top)`, clipped to `clip`, through the block cache.
    /// `key` identifies what `spans()` would return ([`block_key`](Self::block_key),
    /// or any hash covering the same); the block is shaped once per key and
    /// width, only while this frame's shaping budget lasts. Returns how it was
    /// laid out. Past the budget the frame is re-requested and the block draws
    /// as laid out at the nearest other width (the returned `width`; read its
    /// rects and hits there), or draws nothing and returns `None` (lay it out
    /// at [`block_estimate`](Self::block_estimate)).
    #[allow(clippy::too_many_arguments)]
    pub fn block(
        &mut self,
        key: u64,
        spans: impl FnOnce() -> Vec<BlockSpan>,
        width: f32,
        left: f32,
        top: f32,
        clip: Rect,
    ) -> Option<BlockLayout> {
        let (slot, layout) = self.text.budgeted_block(key, width, true, spans)?;
        self.push_text(TextItem {
            src: TextSrc::Block(slot),
            left,
            top,
            bounds: bounds(clip),
            // Every span bakes its own colour.
            color: [0, 0, 0],
            scale: 1.0,
        });
        Some(layout)
    }

    /// Shape a block `width` wide without drawing it (to lay out what comes
    /// after it), within this frame's shaping budget: its height, or `None`
    /// past the budget.
    pub fn measure_block(&mut self, key: u64, spans: impl FnOnce() -> Vec<BlockSpan>, width: f32) -> Option<f32> {
        self.text.budgeted_block(key, width, false, spans).map(|(_, l)| l.height)
    }

    /// The height of block `key` laid out `width` wide, while the cache holds
    /// it: an idle block may be evicted or moved to another width, so keep
    /// the heights a layout depends on.
    pub fn block_height(&self, key: u64, width: f32) -> Option<f32> {
        self.text.block_cache.height(key, width)
    }

    /// Roughly how tall `spans` lay out `width` wide, without shaping them.
    pub fn block_estimate(&self, spans: &[BlockSpan], width: f32) -> f32 {
        crate::block_cache::estimate_height(spans, width)
    }

    /// Where block `key`'s glyphs tagged `tag` were laid out `width` wide,
    /// relative to its top-left: a rect per run of them on a line, the line's
    /// height (an inline code box, a link's hit area). Empty until shaped.
    pub fn block_rects(&self, key: u64, width: f32, tag: NonZeroU32) -> Vec<Rect> {
        self.text.block_cache.rects(key, width, tag)
    }

    /// The text position in block `key` (`width` wide) nearest `(x, y)`,
    /// relative to its top-left, as a byte offset into its spans' joined
    /// text, and the tag of the glyph under the point (`None` if no glyph or
    /// an untagged one is). `None` until shaped.
    pub fn block_hit(&self, key: u64, width: f32, x: f32, y: f32) -> Option<(usize, Option<NonZeroU32>)> {
        self.text.block_cache.hit(key, width, x, y)
    }

    /// Paint `f` above every pane's content (a view's popups): its quads and
    /// text go to the overlay pass. Nothing paints while a modal is open: it
    /// would draw over (or under the dim of) the modal.
    pub fn above(&mut self, f: impl FnOnce(&mut Self)) {
        if self.modal {
            return;
        }
        // Popups escape the view's clip, as they do its rect.
        let layer = std::mem::replace(&mut self.layer, Layer::Overlay);
        let clip = self.clip.take();
        f(self);
        self.layer = layer;
        self.clip = clip;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{Harness, UI};

    #[test]
    fn a_styled_row_deferred_by_the_shaping_budget_draws_no_decorations() {
        let mut h = Harness::new();
        // No time budget: only the guaranteed minimum of rows shape.
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::ZERO);
        let clip = Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 };
        let runs = [RunStyle::UNDERLINE];
        let styles = RowStyles::new(&runs, UI.cell_w);
        let rows = MIN_SHAPES_PER_FRAME + 4;
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            for i in 0..rows {
                let text = format!("row {i}");
                let key = cx.row_key_styled(std::iter::once((text.as_str(), [200; 3])), styles);
                let top = i as f32 * UI.cell_h;
                cx.budgeted_row_styled(key, || vec![(text.clone(), [200; 3])], styles, 0.0, top, 100.0, clip, [200; 3]);
            }
        }
        assert!(h.text.finish_paint(), "some rows were deferred");
        assert_eq!(h.frame.bg.len(), MIN_SHAPES_PER_FRAME, "an underline for each shaped row only");
    }

    fn block_spans(text: &str) -> Vec<BlockSpan> {
        vec![BlockSpan::new(text, Font::SANS, 14.0, 18.0, [200; 3])]
    }

    /// `block` queues its cached buffer at its top-left, clipped to its clip,
    /// and returns the height it laid out; the queries read the same entry.
    #[test]
    fn a_block_queues_its_text_with_its_bounds() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let clip = Rect { x: 10.0, y: 20.0, w: 300.0, h: 40.0 };
        let spans = block_spans("A paragraph long enough to wrap onto a second line here");
        let (key, height) = {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            let key = cx.block_key(&spans);
            let laid = cx.block(key, || spans.clone(), 200.0, 12.0, 15.0, clip).unwrap();
            assert_eq!(laid.width, 200.0);
            let height = laid.height;
            assert_eq!(cx.block_height(key, 200.0), Some(height));
            assert_eq!(cx.block_height(key, 150.0), None, "not shaped at that width");
            assert!(cx.block_estimate(&spans, 200.0) > 18.0, "an estimate of more than a line");
            (key, height)
        };
        assert!(height > 18.0, "wrapped: {height}");
        let [item] = h.frame.text[..] else { panic!("one text item") };
        assert!(matches!(item.src, TextSrc::Block(_)));
        assert_eq!((item.left, item.top, item.scale), (12.0, 15.0, 1.0));
        assert_eq!(item.bounds, bounds(clip));
        let buf = h.text.buffer(item.src).expect("the item's buffer");
        assert!(std::ptr::eq(buf, h.text.block_cache.get(key, 200.0).unwrap()));
    }

    /// Past the shaping budget a new block draws nothing and returns `None`,
    /// a block shaped before at another width draws that layout, and the
    /// frame is marked deferred; the next frame shapes what was left.
    #[test]
    fn the_shaping_budget_defers_blocks_to_the_next_frame() {
        let mut h = Harness::new();
        let clip = Rect { x: 0.0, y: 0.0, w: 400.0, h: 400.0 };
        let n = MIN_SHAPES_PER_FRAME + 1;
        let texts: Vec<String> = (0..n).map(|i| format!("block {i}")).collect();
        let paint = |h: &mut Harness, width: f32| -> Vec<Option<BlockLayout>> {
            h.frame.text.clear();
            h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::ZERO);
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let spans = block_spans(t);
                    let key = cx.block_key(&spans);
                    cx.block(key, || spans, width, 0.0, i as f32 * 20.0, clip)
                })
                .collect()
        };
        let first = paint(&mut h, 300.0);
        assert!(first[..MIN_SHAPES_PER_FRAME].iter().all(Option::is_some));
        assert_eq!(first[MIN_SHAPES_PER_FRAME], None, "past the budget");
        assert_eq!(h.frame.text.len(), MIN_SHAPES_PER_FRAME, "the deferred block draws nothing");
        assert!(h.text.finish_paint());

        let second = paint(&mut h, 300.0);
        assert!(second.iter().all(Option::is_some), "hits cost no budget; the rest shapes");
        assert!(!h.text.finish_paint());

        // At a new width each block lays out again, the first few within the
        // budget; the rest stand in with their old layout, and say so.
        let shaped = paint(&mut h, 250.0);
        let widths: Vec<f32> = shaped.iter().map(|l| l.unwrap().width).collect();
        assert!(widths[..MIN_SHAPES_PER_FRAME].iter().all(|w| *w == 250.0));
        assert_eq!(widths[MIN_SHAPES_PER_FRAME], 300.0, "the stand-in's width");
        assert_eq!(h.frame.text.len(), n);
        assert!(h.text.finish_paint());
        let spans = block_spans(&texts[n - 1]);
        assert!(h.text.block_cache.height(crate::block_cache::block_key(&spans), 250.0).is_none());
    }

    /// `measure_block` shapes for layout without queueing text.
    #[test]
    fn measure_block_shapes_without_drawing() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let spans = block_spans("measured");
        let mut cx = PaintCx::new(
            &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
        );
        let key = cx.block_key(&spans);
        let height = cx.measure_block(key, || spans.clone(), 200.0);
        assert_eq!(height, Some(18.0));
        assert_eq!(cx.block_height(key, 200.0), height);
        assert!(h.frame.text.is_empty());
    }

    /// A run of adjacent `BOX` spans draws one rounded, outlined box, padded
    /// into the cells either side and clipped (not cut) to the row's clip.
    #[test]
    fn boxed_spans_draw_one_rounded_outlined_box_per_run() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let spans: Vec<(String, [u8; 3])> =
            ["a `", "co", "de", "` b `", "x"].iter().map(|t| (t.to_string(), [200; 3])).collect();
        let runs = [RunStyle::PLAIN, RunStyle::BOX, RunStyle::BOX, RunStyle::PLAIN, RunStyle::BOX];
        let styles = RowStyles::new(&runs, UI.cell_w);
        // The clip ends inside the second box (columns 12..13, padded).
        let clip = Rect { x: 0.0, y: 0.0, w: 12.5 * UI.cell_w, h: 100.0 };
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            let key = cx.row_key_styled(spans.iter().map(|(t, c)| (t.as_str(), *c)), styles);
            cx.budgeted_row_styled(key, || spans.clone(), styles, 0.0, 0.0, 400.0, clip, [200; 3]);
        }
        let boxes = &h.frame.bg;
        assert_eq!(boxes.len(), 2, "one box per run of boxed spans");
        let pad = 2.0;
        let x = |q: &QuadInstance| ((q.pos[0] + 1.0) / 2.0 * 800.0 * 100.0).round() / 100.0;
        assert_eq!((x(&boxes[0]), boxes[0].px[0]), (3.0 * UI.cell_w - pad, 4.0 * UI.cell_w + 2.0 * pad));
        assert_eq!((x(&boxes[1]), boxes[1].px[0]), (12.0 * UI.cell_w - pad, UI.cell_w + 2.0 * pad));
        let border = theme::hairline(UI);
        assert_eq!(boxes[1].px[1], UI.cell_h - 2.0 * border, "inset from the row's edges");
        for q in boxes {
            assert_eq!((q.radius, q.border_w), (theme::box_radius(UI), border));
        }
        assert_eq!(boxes[0].clip, [-pad, 0.0, clip.w, 100.0], "clipped, keeping its shape");
    }

    /// A scaled row's decorations measure its scaled cells: a box as wide and
    /// tall as its characters drawn at that scale, an underline at its foot,
    /// and its text item carries the scale.
    #[test]
    fn a_scaled_rows_decorations_measure_scaled_cells() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let spans: Vec<(String, [u8; 3])> = ["ab", "cd"].iter().map(|t| (t.to_string(), [200; 3])).collect();
        let runs = [RunStyle::BOX, RunStyle::UNDERLINE];
        let styles = RowStyles::new(&runs, UI.cell_w).scaled(2.0);
        let clip = Rect { x: 0.0, y: 0.0, w: 400.0, h: 100.0 };
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            let key = cx.row_key_styled(spans.iter().map(|(t, c)| (t.as_str(), *c)), styles);
            cx.budgeted_row_styled(key, || spans.clone(), styles, 0.0, 0.0, 400.0, clip, [200; 3]);
        }
        let (cw, line_h) = (2.0 * UI.cell_w, 2.0 * UI.cell_h);
        let pad = (cw * 0.25).round();
        let boxed = &h.frame.bg[0];
        let border = theme::hairline(UI);
        assert_eq!((boxed.px[0], boxed.px[1]), (2.0 * cw + 2.0 * pad, line_h - 2.0 * border));
        let under = &h.frame.bg[1];
        let y = |q: &QuadInstance| ((1.0 - q.pos[1]) / 2.0 * 600.0 * 100.0).round() / 100.0;
        assert_eq!(under.px[0], 2.0 * cw, "under the two scaled cells");
        assert!(y(under) > UI.cell_h && y(under) < line_h, "at the scaled row's foot: {}", y(under));
        assert_eq!(h.frame.text[0].scale, 2.0);
    }

    /// Code wrapped onto the start of a row keeps its box's left edge; a row
    /// scrolled sideways clips at its text.
    #[test]
    fn a_box_at_a_rows_first_column_keeps_its_left_edge_unless_scrolled() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let runs = [RunStyle::BOX];
        let styles = RowStyles::new(&runs, UI.cell_w);
        let clip = Rect { x: 32.0, y: 0.0, w: 200.0, h: 100.0 };
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            for (left, top) in [(32.0, 0.0), (24.0, 16.0)] {
                let spans = vec![("code".to_string(), [200; 3])];
                let key = cx.row_key_styled(spans.iter().map(|(t, c)| (t.as_str(), *c)), styles);
                cx.budgeted_row_styled(key, || spans.clone(), styles, left, top, 400.0, clip, [200; 3]);
            }
        }
        let clips: Vec<f32> = h.frame.bg.iter().map(|q| q.clip[0]).collect();
        assert_eq!(clips, [30.0, 32.0]);
    }

    /// A hosted view's buttons and images carry its host's id base, nested
    /// bases add, and the base ends with the closure; menus it requests are
    /// renumbered the same way.
    #[test]
    fn with_id_base_offsets_buttons_images_and_menu_items() {
        use crate::plugin::{EventCx, MenuAlign, MenuItem, Request};
        let mut h = Harness::new();
        let r = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            cx.button(r, 1);
            cx.with_id_base(100, |cx| {
                cx.button(r, 1);
                cx.with_id_base(1000, |cx| cx.button(r, 2));
                cx.image(3, 0, Image::from_rgba(1, 1, vec![0; 4]), r, r);
            });
            cx.button(r, 4);
        }
        let ids: Vec<u32> = h.frame.view_buttons.iter().map(|b| b.2).collect();
        assert_eq!(ids, [1, 101, 1102, 4]);
        assert_eq!(h.frame.images[0].id, 103);

        let mut cx = EventCx::new(&h.cfg, UI, Default::default(), None);
        let menu = |ids: &[u32]| Request::Menu {
            items: ids.iter().map(|&i| if i == u32::MAX { MenuItem::RULE } else { MenuItem::new(i, "x") }).collect(),
            at: (0.0, 0.0),
            align: MenuAlign::Left,
        };
        cx.request(menu(&[1]));
        let used = cx.with_id_base(100, |cx| {
            cx.request(menu(&[1, u32::MAX, 2]));
            true
        });
        assert!(used, "the closure's result passes through");
        assert_eq!(cx.finish().requests, [menu(&[1]), menu(&[101, u32::MAX, 102])], "a rule keeps its id");
    }

    /// `with_clip` cuts plain fills (and plain `round_rect`s) on the CPU,
    /// hands shaped quads the clip, drops what has nothing inside it, clips
    /// buttons, nests by intersection, and ends with its closure; popups
    /// painted `above` escape it, and it applies again after them.
    #[test]
    fn with_clip_clips_fills_shaped_quads_and_buttons() {
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let px = |q: &QuadInstance| {
            let r = Rect {
                x: (q.pos[0] + 1.0) / 2.0 * 800.0,
                y: (1.0 - q.pos[1]) / 2.0 * 600.0,
                w: q.px[0],
                h: q.px[1],
            };
            [r.x, r.y, r.w, r.h].map(|v| (v * 1000.0).round() / 1000.0)
        };
        let clip = Rect { x: 100.0, y: 100.0, w: 50.0, h: 50.0 };
        let c = [100.0, 100.0, 150.0, 150.0];
        let white = [255; 3];
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            cx.with_clip(clip, |cx| {
                cx.fill(Rect { x: 80.0, y: 120.0, w: 40.0, h: 10.0 }, white); // bg 0
                cx.fill(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, white); // dropped
                cx.round_rect(Rect { x: 140.0, y: 140.0, w: 30.0, h: 30.0 }, 4.0, white, 1.0, None); // bg 1
                cx.round_rect(Rect { x: 90.0, y: 90.0, w: 20.0, h: 20.0 }, 0.0, white, 1.0, None); // bg 2: plain
                cx.line((300.0, 300.0), (320.0, 320.0), 1.0, white, 1.0); // dropped
                cx.bar((90.0, 125.0), (120.0, 125.0), 2.0, white, 1.0); // bg 3
                // Just outside the clip, but its antialiased pixel reaches in.
                cx.round_rect(Rect { x: 150.5, y: 120.0, w: 10.0, h: 10.0 }, 2.0, white, 1.0, None); // bg 4
                cx.round_rect(Rect { x: 152.0, y: 120.0, w: 10.0, h: 10.0 }, 2.0, white, 1.0, None); // dropped
                cx.border(Rect { x: 90.0, y: 140.0, w: 30.0, h: 30.0 }, white); // bg 5, 6
                cx.fill_top(Rect { x: 140.0, y: 90.0, w: 20.0, h: 20.0 }, white, 1.0); // top 0
                cx.with_clip(Rect { x: 120.0, y: 0.0, w: 100.0, h: 600.0 }, |cx| {
                    cx.round_rect(Rect { x: 110.0, y: 110.0, w: 20.0, h: 20.0 }, 4.0, white, 1.0, None);
                });
                cx.above(|cx| cx.fill(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, white)); // top 1
                cx.fill(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, white); // dropped: the clip is back
                cx.button(Rect { x: 90.0, y: 140.0, w: 30.0, h: 30.0 }, 1);
                cx.button(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, 2);
            });
            cx.fill(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, white);
            cx.button(Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, 3);
        }
        let (bg, top) = (&h.frame.bg, &h.frame.top);
        assert_eq!(px(&bg[0]), [100.0, 120.0, 20.0, 10.0], "a fill is cut to the clip");
        assert_eq!((bg[1].clip, bg[1].px), (c, [30.0, 30.0]), "a shaped quad keeps its rect and carries the clip");
        assert_eq!(px(&bg[2]), [100.0, 100.0, 10.0, 10.0], "a plain round_rect is cut like a fill");
        assert_eq!(bg[2].clip, NO_CLIP, "a plain fill carries no clip");
        assert!(bg[3].radius < 0.0 && bg[3].clip == c, "a bar carries the clip");
        assert_eq!(px(&bg[4])[0], 150.5, "kept: its antialiased pixel reaches into the clip");
        // The border's left edge (x 90..92) and bottom edge (y 168..170) lie
        // outside the clip.
        assert_eq!(px(&bg[5]), [100.0, 140.0, 20.0, 2.0], "the border's top edge, cut");
        assert_eq!(px(&bg[6]), [118.0, 140.0, 2.0, 10.0], "the border's right edge, cut");
        assert_eq!(bg[7].clip, [120.0, 100.0, 150.0, 150.0], "nested clips intersect");
        assert_eq!(px(&bg[8]), [0.0, 0.0, 10.0, 10.0], "the clip ends with its closure");
        assert_eq!(bg.len(), 9);
        assert_eq!(px(&top[0]), [140.0, 100.0, 10.0, 10.0], "fill_top is cut");
        assert_eq!(px(&top[1]), [0.0, 0.0, 10.0, 10.0], "a popup escapes the clip");
        assert_eq!(top.len(), 2);
        let buttons: Vec<(Rect, u32)> = h.frame.view_buttons.iter().map(|(r, _, id)| (*r, *id)).collect();
        assert_eq!(
            buttons,
            vec![
                (Rect { x: 100.0, y: 140.0, w: 20.0, h: 10.0 }, 1),
                (Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, 3),
            ],
            "a button is cut to the clip, or dropped outside it"
        );
    }

    #[test]
    fn image_is_bgra_with_a_mip_chain_and_fits_a_rect() {
        let img = Image::from_rgba(5, 2, [1, 2, 3, 255].repeat(5 * 2));
        assert_eq!(&img.bgra().unwrap()[..4], &[3, 2, 1, 255], "RGBA becomes BGRA");
        let sizes: Vec<(u32, u32)> = img.mips().iter().map(|(w, h, _)| (*w, *h)).collect();
        assert_eq!(sizes, vec![(2, 1), (1, 1)]);
        assert!(img.mips().iter().all(|(w, h, px)| px.len() == (*w * *h * 4) as usize));
        assert_eq!(&img.mips()[1].2, &[3, 2, 1, 255], "a solid colour averages to itself");
        let half = Image::from_rgba(1, 1, vec![200, 100, 50, 128]);
        assert_eq!(half.bgra().unwrap(), &[34, 72, 147, 128], "alpha is premultiplied in linear light");
        let r = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        assert_eq!(img.fit(r, 1.0), Rect { x: 48.0, y: 49.0, w: 5.0, h: 2.0 }, "never upscaled");
        assert_eq!(img.fit(r, f32::MAX), Rect { x: 0.0, y: 30.0, w: 100.0, h: 40.0 });
    }

    #[test]
    fn an_empty_image_is_one_transparent_pixel() {
        for (w, h) in [(0, 5), (5, 0), (0, 0)] {
            let img = Image::from_rgba(w, h, Vec::new());
            assert_eq!((img.width(), img.height()), (1, 1));
            assert_eq!(img.bgra().unwrap(), &[0, 0, 0, 0]);
            assert!(img.mips().is_empty());
        }
    }

    /// Mips average in linear light: a black-and-white checkerboard shrinks to
    /// the grey that emits half the light (188), not the encoded midpoint (128).
    #[test]
    fn mips_average_in_linear_light() {
        let (b, w) = ([0, 0, 0, 255], [255, 255, 255, 255]);
        let img = Image::from_rgba(2, 2, [b, w, w, b].concat());
        assert_eq!(img.mips()[0].2, [188, 188, 188, 255]);
        for c in 0..=255u8 {
            assert_eq!(encode(DECODE[c as usize]), c, "encoding inverts decoding");
        }
    }
}
