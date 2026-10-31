//! Rows that glide to a new place over [`SLIDE`] instead of jumping, so a
//! re-sorted list shows where each row went.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// How long a row takes to reach its new place.
pub(crate) const SLIDE: Duration = Duration::from_millis(160);

#[derive(Clone, Copy, Debug)]
struct Glide {
    from: f32,
    to: f32,
    start: Instant,
}

impl Glide {
    fn at(&self, now: Instant) -> f32 {
        let t = (now.saturating_duration_since(self.start).as_secs_f32() / SLIDE.as_secs_f32()).min(1.0);
        let ease = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * ease
    }
}

/// Each row's glide, by key.
#[derive(Debug)]
pub(crate) struct Slides<K> {
    rows: HashMap<K, Glide>,
}

impl<K> Default for Slides<K> {
    fn default() -> Self {
        Slides { rows: HashMap::new() }
    }
}

impl<K: Hash + Eq + Clone> Slides<K> {
    /// Where row `key` draws at `now`, given its place `to`. A row seen for
    /// the first time draws at its place; one whose place moved glides there
    /// from where it was drawing.
    pub(crate) fn at(&mut self, key: &K, to: f32, now: Instant) -> f32 {
        match self.rows.get_mut(key) {
            Some(g) if g.to == to => g.at(now),
            Some(g) => {
                let from = g.at(now);
                *g = Glide { from, to, start: now };
                from
            }
            None => {
                self.rows.insert(key.clone(), Glide { from: to, to, start: now });
                to
            }
        }
    }

    /// Forget the rows `keep` rejects.
    pub(crate) fn retain(&mut self, keep: impl Fn(&K) -> bool) {
        self.rows.retain(|k, _| keep(k));
    }

    /// Whether a row is still on its way at `now`.
    pub(crate) fn moving(&self, now: Instant) -> bool {
        self.rows.values().any(|g| g.from != g.to && now < g.start + SLIDE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moved_row_glides_from_where_it_was_and_settles() {
        let t0 = Instant::now();
        let mut s = Slides::default();
        assert_eq!(s.at(&1, 10.0, t0), 10.0, "a new row draws in place");
        assert!(!s.moving(t0));
        assert_eq!(s.at(&1, 50.0, t0), 10.0, "starts where it was");
        assert!(s.moving(t0));
        let mid = s.at(&1, 50.0, t0 + SLIDE / 2);
        assert!(10.0 < mid && mid < 50.0, "on its way: {mid}");
        assert_eq!(s.at(&1, 50.0, t0 + SLIDE), 50.0);
        assert!(!s.moving(t0 + SLIDE));
    }

    #[test]
    fn retargeting_mid_glide_starts_from_the_drawn_spot() {
        let t0 = Instant::now();
        let mut s = Slides::default();
        s.at(&1, 0.0, t0);
        s.at(&1, 100.0, t0);
        let mid = s.at(&1, 100.0, t0 + SLIDE / 2);
        assert_eq!(s.at(&1, 0.0, t0 + SLIDE / 2), mid);
        s.retain(|_| false);
        assert_eq!(s.at(&1, 7.0, t0 + SLIDE / 2), 7.0, "forgotten rows start afresh");
    }
}
