//! Content-keyed cache of shaped terminal rows.
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

use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, Wrap};

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
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.metrics_gen.hash(&mut h);
        for (text, fg) in cells {
            text.hash(&mut h);
            fg.hash(&mut h);
        }
        h.finish()
    }

    /// Ensure a shaped buffer for `key` exists, shaping `spans` on a miss. `width`
    /// bounds the layout box (for clipping), `metrics`/`line_h` set the row height.
    pub fn ensure(
        &mut self,
        key: u64,
        font_system: &mut FontSystem,
        metrics: Metrics,
        width: f32,
        line_h: f32,
        spans: &[(String, [u8; 3])],
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
        let buf = &mut slot.buf;
        buf.set_metrics(metrics);
        buf.set_wrap(Wrap::None);
        buf.set_size(Some(width.max(1.0)), Some(line_h));
        buf.set_rich_text(
            spans.iter().map(|(t, c)| {
                (
                    t.as_str(),
                    Attrs::new()
                        .family(Family::Monospace)
                        .color(Color::rgb(c[0], c[1], c[2])),
                )
            }),
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
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
        cache.ensure(k, &mut fs, m, 400.0, 18.0, &span("hello world", [200, 200, 200]));
        cache.end_frame();
        assert_eq!(cache.stats(), (0, 1), "first sighting is a miss");

        // Next frame, same content -> hit, no reshape.
        cache.begin_frame(0);
        let k = key(&cache, "hello world");
        cache.ensure(k, &mut fs, m, 400.0, 18.0, &span("hello world", [200, 200, 200]));
        cache.end_frame();
        assert_eq!(cache.stats(), (1, 1), "identical content should hit");
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
                cache.ensure(k, &mut fs, m, 600.0, 18.0, &span(&line, [200, 200, 200]));
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
        cache.ensure(k, &mut fs, Metrics::new(15.0, 18.0), 400.0, 18.0, &span("abc", [1, 2, 3]));
        cache.end_frame();

        cache.begin_frame(1); // new metrics generation
        let k = key(&cache, "abc");
        cache.ensure(k, &mut fs, Metrics::new(20.0, 24.0), 400.0, 24.0, &span("abc", [1, 2, 3]));
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
            cache.ensure(k, &mut fs, m, 400.0, 18.0, &span(&s, [9, 9, 9]));
        }
        cache.end_frame();
        assert_eq!(cache.map.len(), 8, "this-frame rows are never evicted");

        // Frame 2: touch only row6 + row7. The other six are now idle and get
        // trimmed down to the cap.
        cache.begin_frame(0);
        for i in [6, 7] {
            let s = format!("row{i}");
            let k = key(&cache, &s);
            cache.ensure(k, &mut fs, m, 400.0, 18.0, &span(&s, [9, 9, 9]));
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
        cache.ensure(k0, &mut fs, m, 400.0, 18.0, &span("row0", [9, 9, 9]));
        cache.end_frame();
        assert_eq!(cache.stats().1, before.1 + 1, "evicted row0 must miss on return");
    }
}
