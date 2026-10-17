//! A document's outline: its section headings in order, as a highlighter
//! names them ([`Highlighter::heading_level`], [`Highlighter::heading_title`]),
//! for a table of contents to list and jump between.
//!
//! Finding the headings asks every line, threading the highlight state
//! through the document (a `#` line in a Markdown code block is code). An
//! [`Outline`] keeps the result per edit count and shares the view's
//! [`StateCache`], so after an edit only the lines from the edit on are
//! highlighted again; on a long document it waits for the edits to pause
//! ([`DEBOUNCE`]), keeping the last outline meanwhile; on a very long one
//! it outlines on a worker ([`WORKER_LINES`]).

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::fs_tree::Waker;
use crate::highlight::{Highlighter, StateCache};

/// One heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    /// Its line (0-based).
    pub line: usize,
    /// Its level, 1 the outermost.
    pub level: u8,
    /// Its title, without the markup that makes it a heading.
    pub text: String,
}

/// The headings of `lines` per `h`. `state_in(ln)` is the state line `ln`
/// enters with (from a [`StateCache`]), asked in ascending order and only
/// of a highlighter that carries state.
pub fn outline(h: &dyn Highlighter, lines: &[String], mut state_in: impl FnMut(usize) -> u32) -> Vec<OutlineItem> {
    let carries = h.carries_state();
    let mut items = Vec::new();
    for (ln, line) in lines.iter().enumerate() {
        let state = if carries { state_in(ln) } else { 0 };
        if let Some(level) = h.heading_level(line, state) {
            items.push(OutlineItem { line: ln, level, text: h.heading_title(line) });
        }
    }
    items
}

/// A document this many lines long or longer outlines again only once edits
/// pause for [`DEBOUNCE`]: a pass costs a hash of every line from the edit
/// on, too much to repeat at every keystroke.
pub const DEBOUNCE_LINES: usize = 2_000;
/// How long edits to a long document must pause before it outlines again.
pub const DEBOUNCE: Duration = Duration::from_millis(300);
/// A document this many lines long or longer outlines on a worker: a pass
/// highlights every line (about 0.85 ms per 1,000 Markdown lines in a
/// release build) the first time, and after an edit that opens or closes a
/// code fence, every line from it on.
pub const WORKER_LINES: usize = 10_000;

/// The outline of an edited document, kept between edits.
#[derive(Default)]
pub struct Outline {
    /// Each item's line shifted by the edits since it was outlined
    /// ([`Self::dirty_from`]), so a jump lands on its heading meanwhile.
    items: Vec<OutlineItem>,
    /// The edit count and highlighter `items` are of.
    of: Option<(u64, &'static str)>,
    /// An edit count not yet outlined, and when it was first seen.
    pending: Option<(u64, Instant)>,
    /// The document's line count `items` are placed for, and the first line
    /// edited since.
    lines_at: usize,
    dirty: Option<usize>,
    /// The outline on the worker: what it is of, the line count it read,
    /// the first line edited since it read them, and its result.
    job: Option<Job>,
    waker: Option<Waker>,
}

struct Job {
    of: (u64, &'static str),
    lines_at: usize,
    dirty: Option<usize>,
    rx: Receiver<Vec<OutlineItem>>,
}

impl Outline {
    /// An outline whose worker (for a document of [`WORKER_LINES`] lines or
    /// more) calls `waker` when it finishes; then [`Self::poll`].
    pub fn new(waker: Option<Waker>) -> Self {
        Outline { waker, ..Outline::default() }
    }

    /// The headings as last outlined (of an earlier edit count while a long
    /// document's edits haven't paused or its worker hasn't finished).
    pub fn items(&self) -> &[OutlineItem] {
        &self.items
    }

    /// Forget the outline (the document lost its highlighter): no item, and
    /// nothing waiting.
    pub fn reset(&mut self) {
        let waker = self.waker.take();
        *self = Outline::new(waker);
    }

    /// Lines before `row` are unchanged by the edits since the last update,
    /// as [`StateCache::dirty_from`] hears it.
    pub fn dirty_from(&mut self, row: usize) {
        for d in [Some(&mut self.dirty), self.job.as_mut().map(|j| &mut j.dirty)].into_iter().flatten() {
            *d = Some(d.map_or(row, |d| d.min(row)));
        }
    }

    /// Take the worker's outline, if it finished. Returns whether the items
    /// changed.
    pub fn poll(&mut self) -> bool {
        let Some(job) = &self.job else { return false };
        match job.rx.try_recv() {
            Ok(items) => {
                let job = self.job.take().expect("a job");
                self.of = Some(job.of);
                (self.lines_at, self.dirty) = (job.lines_at, job.dirty);
                let changed = items != self.items;
                self.items = items;
                changed
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                false
            }
        }
    }

    /// Outline `lines` (at edit count `edits`) if they changed since: at
    /// once for a short document, a new highlighter, or the first time; for
    /// a long one, once [`DEBOUNCE`] has passed since the edit count was
    /// first seen; from [`WORKER_LINES`] lines, on a worker (one at a time:
    /// edits made meanwhile outline once it finishes). `states` must be the
    /// view's cache for `lines` (told of the edits). Until then the items
    /// below the first edited line move by the change in the line count.
    /// Returns whether the items changed.
    pub fn update(
        &mut self,
        h: &'static dyn Highlighter,
        lines: &[String],
        edits: u64,
        states: &mut StateCache,
        now: Instant,
    ) -> bool {
        let mut changed = self.poll();
        let of = (edits, h.name());
        if self.of == Some(of) {
            return changed;
        }
        changed |= self.shift(lines.len());
        let fresh = self.of.is_none_or(|(_, name)| name != h.name());
        let worker = lines.len() >= WORKER_LINES;
        if worker && self.job.is_some() {
            return changed;
        }
        if !fresh && lines.len() >= DEBOUNCE_LINES {
            let since = match self.pending {
                Some((e, at)) if e == edits => at,
                _ => {
                    self.pending = Some((edits, now));
                    now
                }
            };
            if now < since + DEBOUNCE {
                return changed;
            }
        }
        self.pending = None;
        if worker {
            self.spawn(h, lines, of);
            return changed;
        }
        self.of = Some(of);
        (self.lines_at, self.dirty) = (lines.len(), None);
        let items = outline(h, lines, |ln| states.state_in(h, lines, edits, ln));
        changed |= items != self.items;
        self.items = items;
        changed
    }

    /// Move the items below the first edited line by the change in the line
    /// count since they were placed. Returns whether any moved.
    fn shift(&mut self, len: usize) -> bool {
        let Some(dirty) = self.dirty.take() else { return false };
        let delta = len as isize - self.lines_at as isize;
        self.lines_at = len;
        if delta == 0 {
            return false;
        }
        let mut moved = false;
        for item in self.items.iter_mut().filter(|i| i.line > dirty) {
            item.line = item.line.saturating_add_signed(delta).max(dirty);
            moved = true;
        }
        moved
    }

    /// Outline a copy of `lines` on a worker, threading the state through
    /// itself (the view's cache stays the UI thread's).
    fn spawn(&mut self, h: &'static dyn Highlighter, lines: &[String], of: (u64, &'static str)) {
        // One string: cloning each line costs an allocation apiece.
        let text = lines.join("\n");
        let (tx, rx) = mpsc::channel();
        let waker = self.waker.clone();
        std::thread::spawn(move || {
            let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let (mut next, mut state) = (0, 0);
            let items = outline(h, &lines, |ln| {
                while next < ln {
                    state = h.line(&lines[next], state).1;
                    next += 1;
                }
                state
            });
            if tx.send(items).is_ok() {
                if let Some(w) = waker {
                    w();
                }
            }
        });
        self.job = Some(Job { of, lines_at: lines.len(), dirty: None, rx });
    }

    /// When a waiting outline is due ([`Self::update`] then outlines).
    pub fn due(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at + DEBOUNCE)
    }

    /// Whether an outline is on the worker.
    pub fn working(&self) -> bool {
        self.job.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{markdown::Markdown, toml::Toml};

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn of(h: &dyn Highlighter, text: &str) -> Vec<(usize, u8, String)> {
        let lines = lines(text);
        let mut states = StateCache::default();
        let items = outline(h, &lines, |ln| states.state_in(h, &lines, 0, ln));
        items.into_iter().map(|i| (i.line, i.level, i.text)).collect()
    }

    fn item(line: usize, level: u8, text: &str) -> (usize, u8, String) {
        (line, level, text.to_string())
    }

    #[test]
    fn markdown_headings_by_level_without_their_markup() {
        let text = [
            "# Title",
            "",
            "text",
            "## Two ##",
            "### *Three* `x` [link](http://a)",
            "#### Four #",
            "##### five",
            "###### six",
            "####### seven",
            "#nospace",
            "## trailing \\#",
        ]
        .join("\n");
        assert_eq!(
            of(&Markdown, &text),
            vec![
                item(0, 1, "Title"),
                item(3, 2, "Two"),
                item(4, 3, "Three x link"),
                item(5, 4, "Four"),
                item(6, 5, "five"),
                item(7, 6, "six"),
                item(10, 2, "trailing \\#"),
            ]
        );
        assert_eq!(of(&Markdown, "#\n## ##\n##a##"), vec![item(0, 1, ""), item(1, 2, "")], "empty headings");
    }

    #[test]
    fn markdown_hashes_in_fences_quotes_and_lists_are_not_headings() {
        let text = [
            "# A",
            "```",
            "# code",
            "```",
            "> # quoted",
            "- # item",
            "  ```",
            "  # in the item's fence",
            "  ```",
            "    # indented code",
            "~~~md",
            "## tilde fence",
            "~~~",
            "# B",
        ]
        .join("\n");
        assert_eq!(of(&Markdown, &text), vec![item(0, 1, "A"), item(13, 1, "B")]);
        // An unclosed list item's fence ends where the item does.
        assert_eq!(of(&Markdown, "- ```\n  # code\n# After"), vec![item(2, 1, "After")]);
    }

    #[test]
    fn toml_tables_by_level_with_their_keys_as_written() {
        let text = [
            "top = 1",
            "[editor]",
            "x = 1",
            "  [editor.preview] # note",
            "[[bin]]",
            "[a.\"b.c\"]",
            "['q.r'.s.t]",
            "list = [",
            "  1,",
            "]",
            "# [comment]",
        ]
        .join("\n");
        assert_eq!(
            of(&Toml, &text),
            vec![
                item(1, 1, "editor"),
                item(3, 2, "editor.preview"),
                item(4, 2, "bin"),
                item(5, 2, "a.\"b.c\""),
                item(6, 3, "'q.r'.s.t"),
            ]
        );
    }

    #[test]
    fn a_short_document_outlines_at_every_edit_count() {
        let mut lines = lines("# A\ntext");
        let (mut o, mut states, now) = (Outline::default(), StateCache::default(), Instant::now());
        assert!(o.update(&Markdown, &lines, 0, &mut states, now));
        assert!(!o.update(&Markdown, &lines, 0, &mut states, now), "same edit count: nothing to do");
        lines[1] = "## B".into();
        states.dirty_from(1);
        assert!(o.update(&Markdown, &lines, 1, &mut states, now));
        assert_eq!(o.items().iter().map(|i| i.line).collect::<Vec<_>>(), [0, 1]);
        lines[1] = "## C".into();
        states.dirty_from(1);
        assert!(o.update(&Markdown, &lines, 2, &mut states, now), "a changed title is a change");
        lines.push(String::new());
        assert!(!o.update(&Markdown, &lines, 3, &mut states, now), "an edit leaving the headings be changes nothing");
        assert_eq!(o.due(), None);
    }

    #[test]
    fn a_long_document_outlines_again_once_edits_pause() {
        let heading = |i: usize| if i.is_multiple_of(100) { format!("# {i}") } else { "text".into() };
        let mut lines: Vec<String> = (0..DEBOUNCE_LINES).map(heading).collect();
        let (mut o, mut states, t0) = (Outline::default(), StateCache::default(), Instant::now());
        assert!(o.update(&Markdown, &lines, 0, &mut states, t0), "the first outline comes at once");
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100);
        lines[1] = "# new".into();
        states.dirty_from(1);
        assert!(!o.update(&Markdown, &lines, 1, &mut states, t0), "waits");
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100, "keeping the last outline");
        assert_eq!(o.due(), Some(t0 + DEBOUNCE));
        // Another edit restarts the wait.
        let t1 = t0 + DEBOUNCE / 2;
        lines[2] = "# newer".into();
        states.dirty_from(2);
        assert!(!o.update(&Markdown, &lines, 2, &mut states, t1));
        assert!(!o.update(&Markdown, &lines, 2, &mut states, t0 + DEBOUNCE), "measured from the latest edit");
        assert!(o.update(&Markdown, &lines, 2, &mut states, t1 + DEBOUNCE));
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100 + 2);
        assert_eq!(o.due(), None);
        // A new highlighter doesn't wait.
        assert!(o.update(&Toml, &lines, 3, &mut states, t1 + DEBOUNCE));
        assert!(o.items().is_empty());
    }

    #[test]
    fn reset_forgets_the_items_and_a_waiting_outline() {
        let mut lines: Vec<String> = (0..DEBOUNCE_LINES).map(|_| "# h".to_string()).collect();
        let (mut o, mut states, now) = (Outline::default(), StateCache::default(), Instant::now());
        o.update(&Markdown, &lines, 0, &mut states, now);
        lines[0] = "text".into();
        states.dirty_from(0);
        o.update(&Markdown, &lines, 1, &mut states, now);
        assert!(o.due().is_some());
        o.reset();
        assert_eq!((o.due(), o.items().len()), (None, 0));
    }

    #[test]
    fn while_waiting_the_items_below_an_edit_move_with_the_line_count() {
        let heading = |i: usize| if i == 10 || i == 1_000 { format!("# {i}") } else { "text".into() };
        let mut lines: Vec<String> = (0..DEBOUNCE_LINES + 100).map(heading).collect();
        let (mut o, mut states, now) = (Outline::default(), StateCache::default(), Instant::now());
        o.update(&Markdown, &lines, 0, &mut states, now);
        // Three lines in between the headings.
        for _ in 0..3 {
            lines.insert(500, "new".into());
        }
        states.dirty_from(500);
        o.dirty_from(500);
        assert!(o.update(&Markdown, &lines, 1, &mut states, now), "moved");
        let at: Vec<usize> = o.items().iter().map(|i| i.line).collect();
        assert_eq!(at, [10, 1_003], "only the one below the edit");
        assert_eq!(lines[1_003], "# 1000");
        // Lines out above a heading move it up, not past the edit.
        lines.drain(5..20);
        states.dirty_from(5);
        o.dirty_from(5);
        o.update(&Markdown, &lines, 2, &mut states, now);
        assert_eq!(o.items().iter().map(|i| i.line).collect::<Vec<_>>(), [5, 988]);
        assert!(o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE));
        assert_eq!(o.items().iter().map(|i| i.line).collect::<Vec<_>>(), [988]);
    }

    #[test]
    fn a_very_long_document_outlines_on_a_worker_one_job_at_a_time() {
        let (tx, rx) = mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let waker: Waker = std::sync::Arc::new(move || {
            let _ = tx.lock().unwrap().send(());
        });
        let heading = |i: usize| if i.is_multiple_of(1_000) { format!("# {i}") } else { "text".into() };
        let mut lines: Vec<String> = (0..WORKER_LINES).map(heading).collect();
        lines[1] = "```".into();
        lines[3] = "```".into();
        lines[2] = "# in a fence".into();
        let (mut o, mut states, now) = (Outline::new(Some(waker)), StateCache::default(), Instant::now());
        assert!(!o.update(&Markdown, &lines, 0, &mut states, now), "not on the UI thread");
        assert!(o.working() && o.items().is_empty());
        rx.recv_timeout(Duration::from_secs(10)).expect("the worker wakes the UI");
        assert!(o.poll());
        assert_eq!(o.items().len(), WORKER_LINES / 1_000, "its own state threading skips the fence");
        assert!(!o.update(&Markdown, &lines, 0, &mut states, now), "done");
        // Edits while a job runs wait for it; then one job covers them.
        lines[5] = "# five".into();
        o.update(&Markdown, &lines, 1, &mut states, now);
        assert!(!o.working(), "waits for the edits to pause");
        o.update(&Markdown, &lines, 1, &mut states, now + DEBOUNCE);
        assert!(o.working());
        lines[6] = "# six".into();
        assert!(!o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE * 3));
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE * 3), "edit 1's outline lands");
        assert!(!o.working(), "edit 2 waits for its pause");
        o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE * 5);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        o.poll();
        assert_eq!(o.items().len(), WORKER_LINES / 1_000 + 2);
    }
}
