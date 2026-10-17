//! Content-keyed cache of shaped, word-wrapped blocks of proportional text
//! (a Preview's paragraphs, headings, and list items).
//!
//! Unlike a [`RowCache`](crate::row_cache::RowCache) row, a block's layout
//! depends on its width (that is where it wraps), so an entry is keyed by its
//! content, the width it was laid out at, and the metrics generation. A block
//! seen again at a new width is laid out again from its old entry's shaping
//! when that entry isn't in use this frame (a resize), rather than reshaped.
//!
//! Each span carries its own face, size, colour, and a `metadata` tag that its
//! glyphs keep, so a caller can find where tagged text (inline code, a link)
//! was laid out after wrapping.
//!
//! Eviction is deferred to [`end_frame`](BlockCache::end_frame), as in the row
//! cache: entries touched this frame are referenced by the in-flight text pass.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ghostrealm_core::Rect;
use glyphon::cosmic_text::{Align, Cursor};
use glyphon::{Buffer, Color, FontSystem, Metrics, Shaping, Wrap};

use crate::plugin::Font;

/// One run of a block's text in one face, size, and colour.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockSpan {
    /// May hold `\n`: each one starts a new line of the block.
    pub text: String,
    pub font: Font,
    /// Font size, physical px.
    pub size: f32,
    /// Line height, physical px. A wrapped line is as tall as its tallest span.
    pub line_h: f32,
    pub color: [u8; 3],
    /// Kept by every glyph of the span, for
    /// [`PaintCx::block_rects`](crate::plugin::PaintCx::block_rects) and
    /// [`block_hit`](crate::plugin::PaintCx::block_hit); 0 for plain text.
    pub metadata: u32,
}

impl BlockSpan {
    pub fn new(text: impl Into<String>, font: Font, size: f32, line_h: f32, color: [u8; 3]) -> Self {
        BlockSpan { text: text.into(), font, size, line_h, color, metadata: 0 }
    }

    /// This span with its glyphs tagged `metadata`.
    pub fn tagged(self, metadata: u32) -> Self {
        BlockSpan { metadata, ..self }
    }

    fn hash_into(&self, h: &mut impl Hasher) {
        self.text.hash(h);
        self.font.family.hash(h);
        (self.font.bold, self.font.italic).hash(h);
        (self.size.to_bits(), self.line_h.to_bits()).hash(h);
        self.color.hash(h);
        self.metadata.hash(h);
    }
}

/// A content key for `spans`: their text, faces, sizes, colours, and tags.
/// The cache adds the width and metrics generation itself.
pub fn block_key(spans: &[BlockSpan]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spans.len().hash(&mut h);
    for s in spans {
        s.hash_into(&mut h);
    }
    h.finish()
}

/// Roughly how tall `spans` lay out `width` wide, without shaping: what a
/// caller lays out a block at before it is shaped. Assumes an average advance
/// of half the font size.
pub fn estimate_height(spans: &[BlockSpan], width: f32) -> f32 {
    let Some(first) = spans.first() else { return 0.0 };
    let width = layout_width(width);
    // An empty line is as tall as the first span's, as the buffer lays it out.
    let end_line = |advance: f32, line_h: f32| {
        let line_h = if line_h > 0.0 { line_h } else { first.line_h };
        (advance / width).ceil().max(1.0) * line_h
    };
    let (mut height, mut advance, mut line_h) = (0.0, 0.0, 0.0_f32);
    for s in spans {
        for (i, piece) in s.text.split('\n').enumerate() {
            if i > 0 {
                height += end_line(advance, line_h);
                (advance, line_h) = (0.0, 0.0);
            }
            if !piece.is_empty() {
                advance += piece.chars().count() as f32 * s.size * 0.5;
                line_h = line_h.max(s.line_h);
            }
        }
    }
    height + end_line(advance, line_h)
}

/// The width a block is laid out (and keyed) at: whole pixels, so a width
/// that drifts by a fraction doesn't miss.
fn layout_width(width: f32) -> f32 {
    width.round().max(1.0)
}

/// One shaped block and when it was last used.
struct Slot {
    /// Its map key: content, width, metrics generation.
    key: u64,
    /// The caller's content key.
    content: u64,
    buf: Buffer,
    height: f32,
    /// Monotonic clock value at the slot's last use; higher = more recent.
    used: u64,
}

pub struct BlockCache {
    /// Slot key -> index into `slab`.
    map: HashMap<u64, usize>,
    /// Content key -> the slot laid out for it most recently, at any width:
    /// what a resize lays out again, and what a deferred block draws.
    latest: HashMap<u64, usize>,
    /// Shaped blocks at stable indices (freed slots are recycled, never shifted).
    slab: Vec<Slot>,
    /// Indices of `slab` slots not currently mapped, whose `Buffer` is reusable.
    free: Vec<usize>,
    clock: u64,
    /// `clock` when the current frame began: a slot with `used > frame_start`
    /// was touched this frame and must not be evicted or laid out again.
    frame_start: u64,
    metrics_gen: u64,
    /// Soft cap on live slots, trimmed in `end_frame`.
    cap: usize,
    hits: u64,
    misses: u64,
}

impl BlockCache {
    pub fn new(cap: usize) -> Self {
        BlockCache {
            map: HashMap::new(),
            latest: HashMap::new(),
            slab: Vec::new(),
            free: Vec::new(),
            clock: 0,
            frame_start: 0,
            metrics_gen: 0,
            cap: cap.max(1),
            hits: 0,
            misses: 0,
        }
    }

    /// Start a frame; a new `metrics_gen` drops every block.
    pub fn begin_frame(&mut self, metrics_gen: u64) {
        self.frame_start = self.clock;
        if metrics_gen != self.metrics_gen {
            self.metrics_gen = metrics_gen;
            self.map.clear();
            self.latest.clear();
            self.free.clear();
            self.slab.clear();
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Cache hits / misses since construction.
    #[cfg(test)]
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// The slot key for content `key` laid out `width` wide.
    pub fn slot_key(&self, key: u64, width: f32) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.metrics_gen.hash(&mut h);
        key.hash(&mut h);
        layout_width(width).to_bits().hash(&mut h);
        h.finish()
    }

    fn slot(&self, key: u64, width: f32) -> Option<&Slot> {
        self.map.get(&self.slot_key(key, width)).map(|&i| &self.slab[i])
    }

    /// The buffer for content `key` at `width`, if shaped.
    pub fn get(&self, key: u64, width: f32) -> Option<&Buffer> {
        self.slot(key, width).map(|s| &s.buf)
    }

    /// The laid-out height of content `key` at `width`, if shaped.
    pub fn height(&self, key: u64, width: f32) -> Option<f32> {
        self.slot(key, width).map(|s| s.height)
    }

    /// The buffer a [`TextSrc::Block`](crate::plugin::TextSrc::Block) draws.
    pub fn buffer(&self, slot_key: u64) -> Option<&Buffer> {
        self.map.get(&slot_key).map(|&i| &self.slab[i].buf)
    }

    /// Mark content `key` at `width` used this frame. Returns its slot key and
    /// height if it is shaped.
    pub fn touch(&mut self, key: u64, width: f32) -> Option<(u64, f32)> {
        let slot_key = self.slot_key(key, width);
        let &idx = self.map.get(&slot_key)?;
        let now = self.tick();
        self.slab[idx].used = now;
        self.hits += 1;
        Some((slot_key, self.slab[idx].height))
    }

    /// The slot last laid out for content `key` at any width, marked used
    /// this frame: its slot key and height.
    pub fn touch_latest(&mut self, key: u64) -> Option<(u64, f32)> {
        let &idx = self.latest.get(&key)?;
        let now = self.tick();
        let slot = &mut self.slab[idx];
        slot.used = now;
        Some((slot.key, slot.height))
    }

    /// Lay out content `key` at `width`: from the shaping of its latest slot
    /// at another width if that slot isn't in use this frame, else shaping
    /// `spans()` into a slot of its own. Returns the slot key and height.
    pub fn shape(
        &mut self,
        key: u64,
        font_system: &mut FontSystem,
        width: f32,
        spans: impl FnOnce() -> Vec<BlockSpan>,
    ) -> (u64, f32) {
        let slot_key = self.slot_key(key, width);
        let width = layout_width(width);
        self.misses += 1;
        let now = self.tick();
        let reuse = self.latest.get(&key).copied().filter(|&i| self.slab[i].used <= self.frame_start);
        let idx = match reuse {
            Some(idx) => {
                self.map.remove(&self.slab[idx].key);
                let buf = &mut self.slab[idx].buf;
                buf.set_size(Some(width), None);
                buf.shape_until_scroll(font_system, false);
                idx
            }
            None => {
                let spans = spans();
                let first = spans.first().map_or(Metrics::new(1.0, 1.0), |s| Metrics::new(s.size, s.line_h));
                let idx = match self.free.pop() {
                    Some(i) => i,
                    None => {
                        let buf = Buffer::new(font_system, first);
                        self.slab.push(Slot { key: slot_key, content: key, buf, height: 0.0, used: now });
                        self.slab.len() - 1
                    }
                };
                let buf = &mut self.slab[idx].buf;
                // An empty line takes the buffer's metrics: the first span's.
                buf.set_metrics(first);
                // A word wider than the block (a long URL) breaks between
                // glyphs instead of running past the edge.
                buf.set_wrap(Wrap::WordOrGlyph);
                buf.set_size(Some(width), None);
                let default = spans.first().map_or(Font::SANS, |s| s.font).attrs();
                buf.set_rich_text(
                    spans.iter().map(|s| {
                        let attrs = s
                            .font
                            .attrs()
                            .color(Color::rgb(s.color[0], s.color[1], s.color[2]))
                            .metrics(Metrics::new(s.size, s.line_h))
                            .metadata(s.metadata as usize);
                        (s.text.as_str(), attrs)
                    }),
                    &default,
                    Shaping::Advanced,
                    // Unaligned, a right-to-left line sits against the right
                    // edge; aligned left, every line starts where the block does.
                    Some(Align::Left),
                );
                buf.shape_until_scroll(font_system, false);
                idx
            }
        };
        let slot = &mut self.slab[idx];
        slot.height = slot.buf.layout_runs().last().map_or(0.0, |r| r.line_top + r.line_height);
        slot.key = slot_key;
        slot.content = key;
        slot.used = now;
        self.map.insert(slot_key, idx);
        self.latest.insert(key, idx);
        (slot_key, slot.height)
    }

    /// Where the glyphs tagged `tag` of content `key` at `width` were laid out,
    /// relative to the block's top-left: one rect per run of tagged glyphs on a
    /// line, the line's height. Empty until shaped.
    pub fn rects(&self, key: u64, width: f32, tag: u32) -> Vec<Rect> {
        let Some(buf) = self.get(key, width) else { return Vec::new() };
        let mut out = Vec::new();
        for run in buf.layout_runs() {
            let mut cur: Option<(f32, f32)> = None;
            for g in run.glyphs {
                if g.metadata == tag as usize {
                    let (x0, x1) = cur.unwrap_or((g.x, g.x));
                    cur = Some((x0.min(g.x), x1.max(g.x + g.w)));
                } else if let Some((x0, x1)) = cur.take() {
                    out.push(Rect { x: x0, y: run.line_top, w: x1 - x0, h: run.line_height });
                }
            }
            if let Some((x0, x1)) = cur {
                out.push(Rect { x: x0, y: run.line_top, w: x1 - x0, h: run.line_height });
            }
        }
        out
    }

    /// The text position nearest `(x, y)` (relative to the block's top-left)
    /// in content `key` at `width`, as a byte offset into the spans' joined
    /// text, and the tag of the glyph under the point (0 if none is). `None`
    /// until shaped.
    pub fn hit(&self, key: u64, width: f32, x: f32, y: f32) -> Option<(usize, u32)> {
        let buf = self.get(key, width)?;
        let Cursor { line, index, .. } = buf.hit(x, y)?;
        let line_start: usize =
            buf.lines[..line].iter().map(|l| l.text().len() + l.ending().as_str().len()).sum();
        let tag = buf
            .layout_runs()
            .find(|r| y >= r.line_top && y < r.line_top + r.line_height)
            .and_then(|r| r.glyphs.iter().find(|g| x >= g.x && x < g.x + g.w))
            .map_or(0, |g| g.metadata as u32);
        Some((line_start + index, tag))
    }

    /// Drop least-recently-used blocks until live entries fit under the cap.
    /// Blocks touched this frame are never dropped.
    pub fn end_frame(&mut self) {
        let live = self.map.len();
        if live <= self.cap {
            return;
        }
        let mut stale: Vec<(u64, usize)> = self
            .map
            .values()
            .map(|&idx| (self.slab[idx].used, idx))
            .filter(|(used, _)| *used <= self.frame_start)
            .collect();
        stale.sort_unstable_by_key(|(used, _)| *used);
        for (_, idx) in stale.into_iter().take(live - self.cap) {
            let slot = &self.slab[idx];
            self.map.remove(&slot.key);
            if self.latest.get(&slot.content) == Some(&idx) {
                self.latest.remove(&slot.content);
            }
            self.free.push(idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREY: [u8; 3] = [200, 200, 200];

    fn para(text: &str) -> Vec<BlockSpan> {
        vec![BlockSpan::new(text, Font::SANS, 15.0, 20.0, GREY)]
    }

    /// Shape `spans` at `width` (a miss) or touch them (a hit), as `PaintCx::block` does.
    fn ensure(cache: &mut BlockCache, fs: &mut FontSystem, spans: &[BlockSpan], width: f32) -> f32 {
        let key = block_key(spans);
        match cache.touch(key, width) {
            Some((_, h)) => h,
            None => cache.shape(key, fs, width, || spans.to_vec()).1,
        }
    }

    const LOREM: &str = "The quick brown fox jumps over the lazy dog and keeps on running far away";

    #[test]
    fn identical_content_hits_without_reshaping() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        ensure(&mut cache, &mut fs, &para("hello world"), 300.0);
        cache.end_frame();
        cache.begin_frame(0);
        ensure(&mut cache, &mut fs, &para("hello world"), 300.0);
        cache.end_frame();
        assert_eq!(cache.stats(), (1, 1));
    }

    #[test]
    fn the_key_covers_every_span_attribute() {
        let base = BlockSpan::new("abc", Font::SANS, 15.0, 20.0, GREY);
        let variants = [
            base.clone(),
            BlockSpan { text: "abd".into(), ..base.clone() },
            BlockSpan { font: Font::SERIF, ..base.clone() },
            BlockSpan { font: Font::SANS.bold(true), ..base.clone() },
            BlockSpan { font: Font::SANS.italic(true), ..base.clone() },
            BlockSpan { size: 16.0, ..base.clone() },
            BlockSpan { line_h: 22.0, ..base.clone() },
            BlockSpan { color: [1, 2, 3], ..base.clone() },
            base.clone().tagged(1),
        ];
        let keys: Vec<u64> = variants.iter().map(|s| block_key(std::slice::from_ref(s))).collect();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Where a span boundary falls matters too.
        let split = [BlockSpan { text: "a".into(), ..base.clone() }, BlockSpan { text: "bc".into(), ..base }];
        assert!(!keys.contains(&block_key(&split)));
    }

    #[test]
    fn the_slot_key_changes_with_width_and_metrics_gen() {
        let mut cache = BlockCache::new(16);
        let key = block_key(&para("abc"));
        let at = cache.slot_key(key, 300.0);
        assert_ne!(at, cache.slot_key(key, 200.0));
        assert_eq!(at, cache.slot_key(key, 300.2), "whole pixels");
        cache.begin_frame(1);
        assert_ne!(at, cache.slot_key(key, 300.0));
    }

    #[test]
    fn a_metrics_change_invalidates() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        ensure(&mut cache, &mut fs, &para("abc"), 300.0);
        cache.end_frame();
        cache.begin_frame(1);
        ensure(&mut cache, &mut fs, &para("abc"), 300.0);
        assert_eq!(cache.stats(), (0, 2));
    }

    #[test]
    fn the_cap_evicts_blocks_idle_since_a_past_frame() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(4);
        cache.begin_frame(0);
        for i in 0..8 {
            ensure(&mut cache, &mut fs, &para(&format!("block {i}")), 300.0);
        }
        cache.end_frame();
        assert_eq!(cache.map.len(), 8, "this frame's blocks are never evicted");
        cache.begin_frame(0);
        for i in [6, 7] {
            ensure(&mut cache, &mut fs, &para(&format!("block {i}")), 300.0);
        }
        cache.end_frame();
        assert_eq!(cache.map.len(), 4);
        assert!(cache.get(block_key(&para("block 7")), 300.0).is_some(), "recently used kept");
        assert!(cache.get(block_key(&para("block 0")), 300.0).is_none(), "oldest dropped");
        assert!(!cache.latest.contains_key(&block_key(&para("block 0"))), "and forgotten");
        let before = cache.stats();
        cache.begin_frame(0);
        ensure(&mut cache, &mut fs, &para("block 0"), 300.0);
        assert_eq!(cache.stats().1, before.1 + 1, "an evicted block misses on return");
    }

    #[test]
    fn a_narrower_block_wraps_taller() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        let spans = para(LOREM);
        let wide = ensure(&mut cache, &mut fs, &spans, 2000.0);
        let mid = ensure(&mut cache, &mut fs, &spans, 300.0);
        let narrow = ensure(&mut cache, &mut fs, &spans, 120.0);
        assert_eq!(wide, 20.0, "one line");
        assert!(mid > wide && narrow > mid, "{wide} < {mid} < {narrow}");
        assert_eq!(narrow % 20.0, 0.0, "whole lines");
        let buf = cache.get(block_key(&spans), 120.0).unwrap();
        let widest = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
        assert!(widest <= 120.0, "every line fits: {widest}");
    }

    #[test]
    fn a_resize_lays_out_the_idle_entry_again() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        let spans = para(LOREM);
        let key = block_key(&spans);
        cache.begin_frame(0);
        ensure(&mut cache, &mut fs, &spans, 400.0);
        cache.end_frame();
        cache.begin_frame(0);
        let (_, h) = cache.shape(key, &mut fs, 150.0, || panic!("laid out from the old shaping"));
        assert!(h > 20.0);
        assert!(cache.get(key, 400.0).is_none(), "the old width's entry moved");
        // In use this frame, an entry isn't moved: another width shapes its own.
        let mut built = false;
        cache.shape(key, &mut fs, 90.0, || {
            built = true;
            spans.clone()
        });
        assert!(built);
        assert!(cache.get(key, 150.0).is_some() && cache.get(key, 90.0).is_some());
    }

    #[test]
    fn per_span_sizes_reach_the_glyphs() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        let spans = vec![
            BlockSpan::new("big ", Font::SANS, 30.0, 36.0, GREY),
            BlockSpan::new("small", Font::SANS.bold(true), 12.0, 16.0, GREY),
        ];
        let h = ensure(&mut cache, &mut fs, &spans, 1000.0);
        assert_eq!(h, 36.0, "the line is as tall as its tallest span");
        let buf = cache.get(block_key(&spans), 1000.0).unwrap();
        let glyphs: Vec<_> = buf.layout_runs().flat_map(|r| r.glyphs.iter()).collect();
        assert_eq!(glyphs.first().unwrap().font_size, 30.0);
        let last = glyphs.last().unwrap();
        assert_eq!((last.font_size, last.font_weight), (12.0, glyphon::Weight::BOLD));
    }

    #[test]
    fn rects_cover_exactly_the_tagged_glyphs_across_a_wrap() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        let spans = vec![
            BlockSpan::new("plain words here ", Font::SANS, 15.0, 20.0, GREY),
            BlockSpan::new("tagged across the wrap", Font::SANS, 15.0, 20.0, GREY).tagged(7),
            BlockSpan::new(" plain again", Font::SANS, 15.0, 20.0, GREY),
        ];
        ensure(&mut cache, &mut fs, &spans, 200.0);
        let key = block_key(&spans);
        let rects = cache.rects(key, 200.0, 7);
        assert!(rects.len() >= 2, "a rect on each line the tag reaches: {rects:?}");
        let buf = cache.get(key, 200.0).unwrap();
        for run in buf.layout_runs() {
            for g in run.glyphs {
                let (cx, cy) = (g.x + g.w / 2.0, run.line_top + run.line_height / 2.0);
                let inside = rects.iter().any(|r| cx > r.x && cx < r.x + r.w && cy > r.y && cy < r.y + r.h);
                assert_eq!(inside, g.metadata == 7, "glyph at byte {} of line {}", g.start, run.line_i);
            }
        }
        assert!(cache.rects(key, 200.0, 9).is_empty());
    }

    #[test]
    fn a_hit_finds_the_byte_and_tag_under_the_point() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        let spans = vec![
            BlockSpan::new("first line\nsee ", Font::SANS, 15.0, 20.0, GREY),
            BlockSpan::new("link", Font::SANS, 15.0, 20.0, GREY).tagged(3),
        ];
        ensure(&mut cache, &mut fs, &spans, 400.0);
        let key = block_key(&spans);
        let link = cache.rects(key, 400.0, 3)[0];
        let (byte, tag) = cache.hit(key, 400.0, link.x + 1.0, link.y + 10.0).unwrap();
        assert_eq!((byte, tag), ("first line\nsee ".len(), 3));
        let (byte, tag) = cache.hit(key, 400.0, 1.0, 10.0).unwrap();
        assert_eq!((byte, tag), (0, 0));
        let (_, tag) = cache.hit(key, 400.0, 390.0, 30.0).unwrap();
        assert_eq!(tag, 0, "past the line's end is no glyph");
    }

    #[test]
    fn an_rtl_paragraph_starts_at_the_left() {
        let mut fs = FontSystem::new();
        let mut cache = BlockCache::new(16);
        cache.begin_frame(0);
        let spans = para("\u{05e9}\u{05dc}\u{05d5}\u{05dd} \u{05e2}\u{05d5}\u{05dc}\u{05dd}");
        ensure(&mut cache, &mut fs, &spans, 1000.0);
        let buf = cache.get(block_key(&spans), 1000.0).unwrap();
        let left = buf.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| g.x)).fold(f32::MAX, f32::min);
        assert!(left < 1.0, "glyphs start at {left}");
    }

    #[test]
    fn the_estimate_grows_as_the_width_shrinks() {
        let spans = vec![
            BlockSpan::new(LOREM, Font::SANS, 15.0, 20.0, GREY),
            BlockSpan::new("\nsecond", Font::SANS, 15.0, 24.0, GREY),
        ];
        assert_eq!(estimate_height(&spans, 10_000.0), 44.0, "a line each");
        assert!(estimate_height(&spans, 100.0) > estimate_height(&spans, 300.0));
        assert_eq!(estimate_height(&[], 100.0), 0.0);
    }
}
