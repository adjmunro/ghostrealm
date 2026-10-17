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
pub fn outline<S: AsRef<str>>(
    h: &dyn Highlighter,
    lines: &[S],
    mut state_in: impl FnMut(usize) -> u32,
) -> Vec<OutlineItem> {
    let carries = h.carries_state();
    let mut items = Vec::new();
    for (ln, line) in lines.iter().enumerate() {
        let line = line.as_ref();
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
    /// The edit count and highlighter last asked for: a worker's outline of
    /// an older one leaves this to outline next.
    wanted: Option<(u64, &'static str)>,
    /// An edit count not yet outlined, and when it was first seen.
    pending: Option<(u64, Instant)>,
    /// The document's line count `items` are placed for.
    lines_at: usize,
    /// The first line edited since `items` were placed.
    dirty: Option<usize>,
    /// The outline on the worker: what it is of, the line count it read,
    /// the first line edited since it read them, and its result.
    job: Option<Job>,
    waker: Option<Waker>,
    /// Tests hold this to keep a worker's result back, so they can act
    /// while it runs whatever the machine's speed.
    #[cfg(test)]
    gate: Option<std::sync::Arc<std::sync::Mutex<()>>>,
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
        #[cfg(test)]
        let gate = self.gate.take();
        *self = Outline::new(waker);
        #[cfg(test)]
        {
            self.gate = gate;
        }
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
                // Edits made while it ran outline once they pause, as any
                // edit does.
                if let Some((edits, _)) = self.wanted.filter(|w| *w != job.of) {
                    self.pending = Some((edits, Instant::now()));
                }
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
        let of = (edits, h.name());
        self.wanted = Some(of);
        let mut changed = self.poll();
        if self.of == Some(of) {
            self.pending = None;
            return changed;
        }
        changed |= self.shift(h, lines);
        let fresh = self.of.is_none_or(|(_, name)| name != h.name());
        let worker = lines.len() >= WORKER_LINES;
        if worker && self.job.is_some() {
            // Its result asks again (`poll`): nothing to wait for till then.
            self.pending = None;
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
        // Newer than anything a worker still running would bring.
        self.job = None;
        self.of = Some(of);
        (self.lines_at, self.dirty) = (lines.len(), None);
        let items = outline(h, lines, |ln| states.state_in(h, lines, edits, ln));
        changed |= items != self.items;
        self.items = items;
        changed
    }

    /// Move the items below the first edited line by the change in the line
    /// count since they were placed, and one on that line whose title now
    /// shows that far down (lines put in before its text). Returns whether
    /// any moved.
    fn shift(&mut self, h: &dyn Highlighter, lines: &[String]) -> bool {
        let Some(dirty) = self.dirty.take() else { return false };
        let delta = lines.len() as isize - self.lines_at as isize;
        self.lines_at = lines.len();
        if delta == 0 {
            return false;
        }
        let title_at = |ln: usize| lines.get(ln).map(|l| h.heading_title(l));
        let mut moved = false;
        for item in self.items.iter_mut().filter(|i| i.line >= dirty) {
            let to = item.line.saturating_add_signed(delta).max(dirty);
            if item.line == dirty {
                let left = title_at(item.line).as_ref() != Some(&item.text);
                if delta < 0 || !left || title_at(to).as_ref() != Some(&item.text) {
                    continue;
                }
            }
            item.line = to;
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
        #[cfg(test)]
        let gate = self.gate.clone();
        std::thread::spawn(move || {
            #[cfg(test)]
            let _held = gate.as_ref().map(|g| g.lock());
            let lines: Vec<&str> = text.split('\n').collect();
            let (mut next, mut state) = (0, 0);
            let items = outline(h, &lines, |ln| {
                while next < ln {
                    state = h.line(lines[next], state).1;
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
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
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
        o.gate = Some(gate.clone());
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
        let held = gate.lock().unwrap();
        o.update(&Markdown, &lines, 1, &mut states, now + DEBOUNCE);
        assert!(o.working());
        lines[6] = "# six".into();
        assert!(!o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE * 3));
        assert!(o.working(), "held: still edit 1's job");
        drop(held);
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // Edit 1's outline lands; edit 2's pause counts from then.
        assert!(o.poll());
        let due = o.due().expect("edit 2 waits for its pause");
        assert!(!o.working());
        o.update(&Markdown, &lines, 2, &mut states, due);
        assert!(o.working());
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        o.poll();
        assert_eq!(o.items().len(), WORKER_LINES / 1_000 + 2);
    }

    /// Wait for the worker's outline and take it. Returns whether the items
    /// changed.
    fn land(o: &mut Outline) -> bool {
        let start = Instant::now();
        while o.working() {
            if o.poll() {
                return true;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the worker finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
        false
    }

    /// An outline whose worker holds its result while the returned gate is
    /// locked.
    fn gated() -> (Outline, std::sync::Arc<std::sync::Mutex<()>>) {
        let gate = std::sync::Arc::new(std::sync::Mutex::new(()));
        let mut o = Outline::new(None);
        o.gate = Some(gate.clone());
        (o, gate)
    }

    fn worker_doc() -> Vec<String> {
        let heading = |i: usize| if i.is_multiple_of(1_000) { format!("# {i}") } else { "text".into() };
        (0..WORKER_LINES * 2).map(heading).collect()
    }

    #[test]
    fn edits_made_while_the_worker_runs_are_outlined_after_it_even_when_it_changes_nothing() {
        let mut lines = worker_doc();
        let ((mut o, gate), mut states, now) = (gated(), StateCache::default(), Instant::now());
        o.update(&Markdown, &lines, 0, &mut states, now);
        land(&mut o);
        // Edit 1 changes no heading; edit 2, while its job runs, adds one.
        lines[5] = "more text".into();
        o.dirty_from(5);
        o.update(&Markdown, &lines, 1, &mut states, now);
        let held = gate.lock().unwrap();
        o.update(&Markdown, &lines, 1, &mut states, now + DEBOUNCE);
        assert!(o.working());
        lines[7] = "# new".into();
        o.dirty_from(7);
        o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE);
        assert!(o.working(), "held: still edit 1's job");
        drop(held);
        assert!(!land(&mut o), "edit 1's outline is the same");
        let due = o.due().expect("edit 2 still to outline");
        o.update(&Markdown, &lines, 2, &mut states, due);
        assert!(land(&mut o));
        assert!(o.items().iter().any(|i| i.text == "new"));
    }

    #[test]
    fn a_worker_outline_older_than_a_synchronous_one_is_dropped() {
        let mut lines = worker_doc();
        let ((mut o, gate), mut states, now) = (gated(), StateCache::default(), Instant::now());
        let held = gate.lock().unwrap();
        o.update(&Markdown, &lines, 0, &mut states, now);
        assert!(o.working());
        lines = vec!["# only".into(), "# two".into()];
        o.dirty_from(0);
        o.update(&Markdown, &lines, 1, &mut states, now);
        assert_eq!(o.items().len(), 2);
        drop(held);
        assert!(!o.working() && !land(&mut o));
        assert_eq!(o.items().len(), 2);
    }

    #[test]
    fn growing_past_the_worker_threshold_while_a_job_runs_leaves_no_past_deadline() {
        let mut lines = worker_doc();
        let ((mut o, gate), mut states, now) = (gated(), StateCache::default(), Instant::now());
        o.update(&Markdown, &lines, 0, &mut states, now);
        land(&mut o);
        // Edit 1 starts a job after its pause.
        lines[5] = "more".into();
        o.dirty_from(5);
        o.update(&Markdown, &lines, 1, &mut states, now);
        let held = gate.lock().unwrap();
        o.update(&Markdown, &lines, 1, &mut states, now + DEBOUNCE);
        assert!(o.working());
        // Edit 2 makes it shorter than the worker's threshold: it waits.
        let tail = lines.split_off(WORKER_LINES / 2);
        o.dirty_from(WORKER_LINES / 2);
        o.update(&Markdown, &lines, 2, &mut states, now + DEBOUNCE);
        assert_eq!(o.due(), Some(now + DEBOUNCE * 2));
        // Edit 3 makes it long again while the job runs.
        lines.extend(tail);
        o.update(&Markdown, &lines, 3, &mut states, now + DEBOUNCE * 2);
        assert_eq!(o.due(), None, "the job's result asks again; no tick meanwhile");
        assert!(o.working(), "held: still edit 1's job");
        drop(held);
        land(&mut o);
        let due = o.due().expect("edit 3 to outline");
        o.update(&Markdown, &lines, 3, &mut states, due);
        land(&mut o);
        assert_eq!(o.due(), None);
        assert_eq!(o.items().len(), WORKER_LINES * 2 / 1_000);
    }

    #[test]
    fn lines_put_in_before_a_headings_text_move_it_while_waiting() {
        let heading = |i: usize| if i == 100 { "# H".to_string() } else { "text".into() };
        let mut lines: Vec<String> = (0..DEBOUNCE_LINES + 100).map(heading).collect();
        let (mut o, mut states, now) = (Outline::default(), StateCache::default(), Instant::now());
        o.update(&Markdown, &lines, 0, &mut states, now);
        // Enter at the heading's start: its line is the first edited.
        lines.insert(100, String::new());
        states.dirty_from(100);
        o.dirty_from(100);
        o.update(&Markdown, &lines, 1, &mut states, now);
        assert_eq!(o.items()[0].line, 101);
        // Enter at its end: it stays.
        lines.insert(102, String::new());
        states.dirty_from(101);
        o.dirty_from(101);
        o.update(&Markdown, &lines, 2, &mut states, now);
        assert_eq!(o.items()[0].line, 101);
    }
}
