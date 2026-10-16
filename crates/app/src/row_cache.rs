//! Content-keyed cache of shaped rows: terminal and editor lines, and list
//! rows in another family (a sidebar's sans names).
//!
//! A terminal row's shaped glyph layout depends only on its cell contents (their
//! text and foreground colour) and the current font metrics, not on where the row
//! sits on screen. Keying shaped [`Buffer`]s by that content makes scrolling and
//! vtab/pane switching nearly free: a line that scrolls up, or a surface shown
//! again, reuses its existing shaping instead of reshaping from scratch.
//!
//! Wrapping is disabled ([`Wrap::None`]) so shaping is independent of pane width
//! — the grid already decides where lines break, and the render pass clips to the
//! pane rect. That keeps the content key width-independent.
//!
//! Eviction is deferred to [`end_frame`](RowCache::end_frame): entries touched in
//! the current frame are all still referenced by the in-flight text pass, so they
//! must outlive it. Only rows untouched this frame are dropped, oldest first.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use glyphon::cosmic_text::Align;
use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, Weight, Wrap};

/// How one span of a row is drawn beyond its colour. Rows carry styles only
/// when some span uses one: a plain row passes an empty `&[RunStyle]` and
/// shapes, keys, and draws exactly as an unstyled row.
///
/// Bold and italic pick the face (the shaper falls back to the nearest weight,
/// and skews an italic when the family has no italic face). Underline and `BOX`
/// are not part of the shaped text; the row drawer paints them as quads, so they
/// do not change the shaping but are still part of the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RunStyle(u8);

impl RunStyle {
    pub const PLAIN: RunStyle = RunStyle(0);
    pub const BOLD: RunStyle = RunStyle(1);
    pub const ITALIC: RunStyle = RunStyle(2);
    pub const UNDERLINE: RunStyle = RunStyle(4);
    /// A tinted box behind the span (inline code).
    pub const BOX: RunStyle = RunStyle(8);

    pub fn contains(self, other: RunStyle) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_plain(self) -> bool {
        self.0 == 0
    }

    /// The span's glyph attributes: `attrs` with this style's face applied.
    fn apply(self, mut attrs: Attrs<'static>) -> Attrs<'static> {
        if self.contains(RunStyle::BOLD) {
            attrs = attrs.weight(Weight::BOLD);
        }
        if self.contains(RunStyle::ITALIC) {
            attrs = attrs.style(Style::Italic);
        }
        attrs
    }
}

impl std::ops::BitOr for RunStyle {
    type Output = RunStyle;

    fn bitor(self, rhs: RunStyle) -> RunStyle {
        RunStyle(self.0 | rhs.0)
    }
}

/// One shaped row and when it was last used.
struct Slot {
    key: u64,
    buf: Buffer,
    /// Monotonic clock value at the slot's last use; higher = more recent.
    used: u64,
}

pub struct RowCache {
    /// Content key -> index into `slab`.
    map: HashMap<u64, usize>,
    /// Shaped rows at stable indices (freed slots are recycled, never shifted).
    slab: Vec<Slot>,
    /// Indices of `slab` slots not currently mapped, whose `Buffer` is reusable.
    free: Vec<usize>,
    /// Monotonic use counter, bumped on every touch — the LRU ordering key.
    clock: u64,
    /// `clock` value when the current frame began: a slot with `used > frame_start`
    /// was touched this frame and must not be evicted (the text pass references it).
    frame_start: u64,
    /// Bumped whenever font metrics change; folded into every key so stale
    /// shaping never matches after a font-size/scale change.
    metrics_gen: u64,
    /// Soft cap on live (mapped) slots. Growth past it is trimmed in `end_frame`.
    cap: usize,
    hits: u64,
    misses: u64,
}

impl RowCache {
    pub fn new(cap: usize) -> Self {
        RowCache {
            map: HashMap::new(),
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

    /// Start a frame. `metrics_gen` identifies the current font metrics; when it
    /// changes, all cached shaping is invalidated.
    pub fn begin_frame(&mut self, metrics_gen: u64) {
        self.frame_start = self.clock;
        if metrics_gen != self.metrics_gen {
            self.metrics_gen = metrics_gen;
            self.map.clear();
            self.free.clear();
            self.slab.clear();
        }
    }

    /// Advance the use clock and return the new value.
    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Cache hits / misses since construction (for tests and telemetry).
    #[cfg(test)]
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Content key for one row: each cell's `(text, fg)` plus the metrics
    /// generation. Cheap — hashes cell data in place, allocating no span strings,
    /// so a cache *hit* costs only the hash (no reshape, no span building).
    pub fn row_key<'a>(&self, cells: impl Iterator<Item = (&'a str, [u8; 3])>) -> u64 {
        self.row_key_styled(cells, &[])
    }

    /// [`row_key`](Self::row_key) for a row whose spans carry `styles` (one per
    /// span, or empty for a plain row). An empty `styles` hashes exactly as
    /// `row_key` does, so plain rows keep their keys.
    pub fn row_key_styled<'a>(
        &self,
        cells: impl Iterator<Item = (&'a str, [u8; 3])>,
        styles: &[RunStyle],
    ) -> u64 {
        self.row_key_in(cells, styles, Family::Monospace)
    }

    /// [`row_key_styled`](Self::row_key_styled) for a row shaped in `family`;
    /// for monospace the two keys are equal.
    pub fn row_key_in<'a>(
        &self,
        cells: impl Iterator<Item = (&'a str, [u8; 3])>,
        styles: &[RunStyle],
        family: Family<'static>,
    ) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.metrics_gen.hash(&mut h);
        for (text, fg) in cells {
            text.hash(&mut h);
            fg.hash(&mut h);
        }
        if !styles.is_empty() {
            styles.hash(&mut h);
        }
        if family != Family::Monospace {
            family.hash(&mut h);
        }
        h.finish()
    }

    /// Ensure a shaped buffer for `key` exists, shaping `spans` on a miss. `width`
    /// bounds the layout box (for clipping), `metrics`/`line_h` set the row height.
    /// `styles` is empty (a plain row) or one [`RunStyle`] per span.
    #[allow(clippy::too_many_arguments)]
    pub fn ensure(
        &mut self,
        key: u64,
        font_system: &mut FontSystem,
        metrics: Metrics,
        width: f32,
        line_h: f32,
        spans: &[(String, [u8; 3])],
        styles: &[RunStyle],
    ) {
        self.ensure_in(key, font_system, metrics, width, line_h, spans, styles, Family::Monospace);
    }

    /// [`ensure`](Self::ensure) for a row shaped in `family` (keyed with
    /// [`row_key_in`](Self::row_key_in)).
    #[allow(clippy::too_many_arguments)]
    pub fn ensure_in(
        &mut self,
        key: u64,
        font_system: &mut FontSystem,
        metrics: Metrics,
        width: f32,
        line_h: f32,
        spans: &[(String, [u8; 3])],
        styles: &[RunStyle],
        family: Family<'static>,
    ) {
        if let Some(&idx) = self.map.get(&key) {
            let now = self.tick();
            self.slab[idx].used = now;
            self.hits += 1;
            return;
        }
        self.misses += 1;
        let now = self.tick();
        let idx = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slab.push(Slot {
                    key,
                    buf: Buffer::new(font_system, metrics),
                    used: now,
                });
                self.slab.len() - 1
            }
        };
        let slot = &mut self.slab[idx];
        slot.key = key;
        slot.used = now;
        // ASCII-only rows need no complex-script shaping: Basic skips rustybuzz
        // and is markedly cheaper for the common monospace line. Anything
        // non-ASCII (emoji, combining marks, wide/CJK, RTL) falls back to
        // Advanced for correctness. The choice is a deterministic function of
        // the row's content, so it never needs to enter the cache key.
        // Basic also skips kerning, which only a proportional face has.
        let mono = family == Family::Monospace;
        let shaping = if mono && spans.iter().all(|(t, _)| t.is_ascii()) {
            Shaping::Basic
        } else {
            Shaping::Advanced
        };
        // With no alignment an RTL line is right-aligned to the buffer's
        // width, which the key leaves out; aligned left, the layout doesn't
        // depend on it. Monospace grid rows place each cell where the grid
        // says, so they keep the default.
        let align = if mono { None } else { Some(Align::Left) };
        let buf = &mut slot.buf;
        buf.set_metrics(metrics);
        buf.set_wrap(Wrap::None);
        buf.set_size(Some(width.max(1.0)), Some(line_h));
        buf.set_rich_text(
            spans.iter().enumerate().map(|(i, (t, c))| {
                let attrs = Attrs::new()
                    .family(family)
                    .color(Color::rgb(c[0], c[1], c[2]));
                let attrs = match styles.get(i) {
                    Some(st) => st.apply(attrs),
                    None => attrs,
                };
                (t.as_str(), attrs)
            }),
            &Attrs::new().family(family),
            shaping,
            align,
        );
        buf.shape_until_scroll(font_system, false);
        self.map.insert(key, idx);
    }

    /// The shaped buffer for `key`, if present (always is after `ensure` this
    /// frame). Immutable — safe to hold across the text-render borrow.
    pub fn buffer(&self, key: u64) -> Option<&Buffer> {
        self.map.get(&key).map(|&idx| &self.slab[idx].buf)
    }

    /// Drop least-recently-used rows until live entries fit under the cap. Rows
    /// touched this frame are never dropped — the in-flight text pass holds them.
    pub fn end_frame(&mut self) {
        let live = self.map.len();
        if live <= self.cap {
            return;
        }
        // Candidates: mapped slots untouched this frame, least-recently-used first.
        let mut stale: Vec<(u64, u64, usize)> = self
            .map
            .values()
            .map(|&idx| (self.slab[idx].used, self.slab[idx].key, idx))
            .filter(|(used, _, _)| *used <= self.frame_start)
            .collect();
        stale.sort_unstable_by_key(|(used, _, _)| *used);
        let mut to_drop = live.saturating_sub(self.cap);
        for (_, k, idx) in stale {
            if to_drop == 0 {
                break;
            }
            self.map.remove(&k);
            self.free.push(idx);
            to_drop -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(s: &str, c: [u8; 3]) -> Vec<(String, [u8; 3])> {
        vec![(s.to_string(), c)]
    }

    fn key(cache: &RowCache, s: &str) -> u64 {
        // A stand-in row key: the whole line as one cell of one colour.
        cache.row_key(std::iter::once((s, [200u8, 200, 200])))
    }

    #[test]
    fn identical_content_hits_without_reshaping() {
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(256);

        cache.begin_frame(0);
        let k = key(&cache, "hello world");
        cache.ensure(k, &mut fs, m, 400.0, 18.0, &span("hello world", [200, 200, 200]), &[]);
        cache.end_frame();
        assert_eq!(cache.stats(), (0, 1), "first sighting is a miss");

        // Next frame, same content -> hit, no reshape.
        cache.begin_frame(0);
        let k = key(&cache, "hello world");
        cache.ensure(k, &mut fs, m, 400.0, 18.0, &span("hello world", [200, 200, 200]), &[]);
        cache.end_frame();
        assert_eq!(cache.stats(), (1, 1), "identical content should hit");
    }

    const GREY: [u8; 3] = [200, 200, 200];

    #[test]
    fn a_plain_row_keeps_its_key_and_styles_change_it() {
        let cache = RowCache::new(8);
        let cells = || [("ab", GREY), ("cd", [1, 2, 3])].into_iter();
        let plain = cache.row_key(cells());
        assert_eq!(plain, cache.row_key_styled(cells(), &[]), "no styles hashes as before");
        let bold = cache.row_key_styled(cells(), &[RunStyle::PLAIN, RunStyle::BOLD]);
        let italic = cache.row_key_styled(cells(), &[RunStyle::PLAIN, RunStyle::ITALIC]);
        let under = cache.row_key_styled(cells(), &[RunStyle::PLAIN, RunStyle::UNDERLINE]);
        let boxed = cache.row_key_styled(cells(), &[RunStyle::PLAIN, RunStyle::BOX]);
        let both = cache.row_key_styled(cells(), &[RunStyle::PLAIN, RunStyle::BOLD | RunStyle::ITALIC]);
        let keys = [plain, bold, italic, under, boxed, both];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Which span is styled matters too.
        let first = cache.row_key_styled(cells(), &[RunStyle::BOLD, RunStyle::PLAIN]);
        assert_ne!(first, bold);
    }

    /// The weight the row's first shaped glyph was requested at.
    fn first_weight(cache: &RowCache, key: u64) -> glyphon::Weight {
        cache.buffer(key).unwrap().layout_runs().next().unwrap().glyphs[0].font_weight
    }

    #[test]
    fn bold_reaches_the_shaped_glyphs() {
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(16);
        cache.begin_frame(0);
        let spans = span("abc", GREY);
        let plain = cache.row_key(std::iter::once(("abc", GREY)));
        cache.ensure(plain, &mut fs, m, 400.0, 18.0, &spans, &[]);
        let styles = [RunStyle::BOLD | RunStyle::ITALIC];
        let styled = cache.row_key_styled(std::iter::once(("abc", GREY)), &styles);
        cache.ensure(styled, &mut fs, m, 400.0, 18.0, &spans, &styles);
        assert_eq!(first_weight(&cache, plain), glyphon::Weight::NORMAL);
        assert_eq!(first_weight(&cache, styled), glyphon::Weight::BOLD);
    }

    #[test]
    fn scrolling_reuses_shaped_rows() {
        // Simulate a screen scrolling up by one line each frame: only the newly
        // revealed bottom line is new; every other line is reused.
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(1024);
        let rows = 24usize;
        let total_lines = 200usize;

        for top in 0..(total_lines - rows) {
            cache.begin_frame(0);
            for r in 0..rows {
                let line = format!("line {}", top + r);
                let k = key(&cache, &line);
                cache.ensure(k, &mut fs, m, 600.0, 18.0, &span(&line, [200, 200, 200]), &[]);
            }
            cache.end_frame();
        }
        let (hits, misses) = cache.stats();
        // Each distinct line is shaped once; steady-state frames are almost all
        // hits (only one new line per scroll).
        assert!(
            hits > misses * 5,
            "scrolling should mostly hit the cache: {hits} hits vs {misses} misses"
        );
    }

    #[test]
    fn metrics_change_invalidates() {
        let mut fs = FontSystem::new();
        let mut cache = RowCache::new(256);
        cache.begin_frame(0);
        let k = key(&cache, "abc");
        cache.ensure(k, &mut fs, Metrics::new(15.0, 18.0), 400.0, 18.0, &span("abc", [1, 2, 3]), &[]);
        cache.end_frame();

        cache.begin_frame(1); // new metrics generation
        let k = key(&cache, "abc");
        cache.ensure(k, &mut fs, Metrics::new(20.0, 24.0), 400.0, 24.0, &span("abc", [1, 2, 3]), &[]);
        cache.end_frame();
        assert_eq!(cache.stats(), (0, 2), "a metrics change must not hit stale shaping");
    }

    #[test]
    fn cap_evicts_rows_idle_since_a_past_frame() {
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(4);

        // Frame 1: shape 8 distinct rows. All are touched this frame, so none can
        // be evicted yet (they're referenced by the in-flight text pass).
        cache.begin_frame(0);
        for i in 0..8 {
            let s = format!("row{i}");
            let k = key(&cache, &s);
            cache.ensure(k, &mut fs, m, 400.0, 18.0, &span(&s, [9, 9, 9]), &[]);
        }
        cache.end_frame();
        assert_eq!(cache.map.len(), 8, "this-frame rows are never evicted");

        // Frame 2: touch only row6 + row7. The other six are now idle and get
        // trimmed down to the cap.
        cache.begin_frame(0);
        for i in [6, 7] {
            let s = format!("row{i}");
            let k = key(&cache, &s);
            cache.ensure(k, &mut fs, m, 400.0, 18.0, &span(&s, [9, 9, 9]), &[]);
        }
        cache.end_frame();
        assert!(
            cache.map.len() <= 4,
            "idle rows should trim to the cap, got {}",
            cache.map.len()
        );
        assert!(cache.map.contains_key(&key(&cache, "row7")), "recently used row kept");

        // An evicted early row misses on return; a kept one hits.
        let before = cache.stats();
        cache.begin_frame(0);
        let k0 = key(&cache, "row0");
        cache.ensure(k0, &mut fs, m, 400.0, 18.0, &span("row0", [9, 9, 9]), &[]);
        cache.end_frame();
        assert_eq!(cache.stats().1, before.1 + 1, "evicted row0 must miss on return");
    }

    #[test]
    fn the_font_family_is_part_of_the_key() {
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(256);
        let cells = || std::iter::once(("abc", GREY));
        let mono = cache.row_key_styled(cells(), &[]);
        assert_eq!(mono, cache.row_key_in(cells(), &[], Family::Monospace), "monospace keys equal the plain key");
        let sans = cache.row_key_in(cells(), &[], Family::SansSerif);
        assert_ne!(mono, sans);
        assert_ne!(sans, cache.row_key_in(cells(), &[], Family::Serif));
        cache.begin_frame(0);
        cache.ensure(mono, &mut fs, m, 400.0, 18.0, &span("abc", GREY), &[]);
        cache.ensure_in(sans, &mut fs, m, 400.0, 18.0, &span("abc", GREY), &[], Family::SansSerif);
        cache.end_frame();
        assert_eq!(cache.stats(), (0, 2), "each family shapes its own row");
    }

    #[test]
    fn a_proportional_rtl_row_starts_at_the_left_whatever_the_width() {
        let mut fs = FontSystem::new();
        let m = Metrics::new(15.0, 18.0);
        let mut cache = RowCache::new(256);
        let name = "\u{05e9}\u{05dc}\u{05d5}\u{05dd}.txt";
        let key = cache.row_key_in(std::iter::once((name, GREY)), &[], Family::SansSerif);
        cache.begin_frame(0);
        cache.ensure_in(key, &mut fs, m, 1000.0, 18.0, &span(name, GREY), &[], Family::SansSerif);
        let buf = cache.buffer(key).unwrap();
        let left = buf.layout_runs().flat_map(|r| r.glyphs.iter().map(|g| g.x)).fold(f32::MAX, f32::min);
        assert!(left < 1.0, "glyphs start at {left}, not against the right of a 1000 px box");
    }
}
