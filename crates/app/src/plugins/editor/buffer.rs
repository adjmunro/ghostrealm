//! A minimal editable text buffer — the non-terminal surface content.
//!
//! Scope is deliberately small: open a file (or a scratch buffer), edit it, save
//! it. Columns are counted in Unicode scalar values (not grapheme clusters),
//! which is enough for code and plain text. The window renders `lines` through the
//! same glyphon text path the terminal uses, and routes key input here when the
//! focused surface is an editor.

use std::cell::{Ref, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ghostrealm_core::highlight::CommentSyntax;

use super::history::{self, Caret, NodeId, Op, Pos, UndoTree};
use crate::file_stamp::Stamp;
use crate::languages::same_file;
use crate::plugin::text_edit::{
    closer_of, closes_before, is_word, opener_of, typed_char, word_left, word_right, word_span, Applied, Clipboard,
    Edit, Motion, Run,
};

/// One view's editable text. The views of one file share its [`Doc`]: its
/// undo history and its state on disk. Each keeps its own copy of the text,
/// which [`EditorBuffer::sync`] brings up to date with the others' edits.
pub struct EditorBuffer {
    /// The text, one entry per line. Always at least one (possibly empty) line.
    pub lines: Vec<String>,
    /// Cursor as (row, col); col counts chars into `lines[row]`.
    pub cursor: (usize, usize),
    /// Top visible line.
    pub scroll: usize,
    /// Leftmost visible column when soft-wrap is off (horizontal scroll).
    pub hscroll: usize,
    /// Cursor position the last time the view followed it, so manual scrolling
    /// (which doesn't move the cursor) isn't yanked back to the cursor each frame.
    pub last_cursor: (usize, usize),
    /// Backing file, if any (a scratch buffer has none): the [`Doc`]'s, as of
    /// the last [`Self::sync`].
    pub path: Option<PathBuf>,
    /// Monotonic count of text-changing edits, for the idle-autosave timer.
    pub edits: u64,
    /// The first line changed since a consumer last took this (`None` = none).
    /// Lets a per-line cache re-check only from there on.
    pub dirty_from: Option<usize>,
    /// Selection anchor (row, col); `None` = no selection. The selection spans
    /// anchor..cursor.
    pub anchor: Option<(usize, usize)>,
    /// Closers typed brackets inserted, which stepping over and
    /// pair-backspace are limited to.
    auto_closers: AutoClosers,
    doc: Rc<RefCell<Doc>>,
    /// Tells this buffer's edits from other views' in the shared [`Doc`].
    id: u64,
    /// The step of the history `lines` is at, and how many of its changes
    /// they include.
    at: (NodeId, usize),
    /// The caret as the edit under way began, for the step it opens.
    before: Option<Caret>,
}

/// What every view of one open file shares. It goes when the last view of
/// the file closes, and its history with it.
struct Doc {
    /// The file; a view takes it when it syncs, so Save As or a move in one
    /// view moves them all.
    path: Option<PathBuf>,
    /// The text at the history's current step, for a view that opens later
    /// or fell behind a pruned step.
    text: Vec<String>,
    history: UndoTree,
    /// The view whose edits the current step takes, and the run its last
    /// edit continued; `None` when the next edit starts a step.
    open: Option<Open>,
    /// The file as of the last load or write; `None` for a scratch buffer or a
    /// file that didn't exist then.
    disk: Option<Stamp>,
    /// The file changed on disk while the buffer had unsaved edits, so the
    /// edits were kept; cleared when the buffer loads or writes the file.
    changed_on_disk: bool,
}

#[derive(Clone, Copy)]
struct Open {
    owner: u64,
    /// The run, where its last edit left the cursor, and when.
    run: Option<(Run, Pos, Instant)>,
}

impl Doc {
    fn new(path: Option<PathBuf>, text: Vec<String>, disk: Option<Stamp>) -> Rc<RefCell<Doc>> {
        let history = UndoTree::default();
        Rc::new(RefCell::new(Doc { path, text, history, open: None, disk, changed_on_disk: false }))
    }
}

thread_local! {
    /// The documents of files a view has open, so a second view of a file
    /// shares the first's.
    static OPEN_DOCS: RefCell<Vec<Weak<RefCell<Doc>>>> = const { RefCell::new(Vec::new()) };
}

/// The document of `path` a view already has open.
fn open_doc(path: &Path) -> Option<Rc<RefCell<Doc>>> {
    OPEN_DOCS.with_borrow(|docs| {
        docs.iter().filter_map(Weak::upgrade).find(|d| d.borrow().path.as_deref().is_some_and(|p| same_file(p, path)))
    })
}

/// Let later views of `doc`'s file share it.
fn register(doc: &Rc<RefCell<Doc>>) {
    OPEN_DOCS.with_borrow_mut(|docs| {
        docs.retain(|d| d.strong_count() > 0);
        if !docs.iter().any(|d| std::ptr::eq(d.as_ptr(), Rc::as_ptr(doc))) {
            docs.push(Rc::downgrade(doc));
        }
    });
}

static NEXT_BUFFER: AtomicU64 = AtomicU64::new(1);

/// The columns of the closers the editor inserted on one line, kept in step
/// with edits on that line. Only the cursor's line is tracked: moving to
/// another line forgets them (like VS Code's `autoClosingOvertype: auto`).
#[derive(Clone, Debug, Default)]
pub(super) struct AutoClosers {
    row: usize,
    cols: Vec<usize>,
}

impl AutoClosers {
    fn contains(&self, row: usize, col: usize) -> bool {
        self.row == row && self.cols.contains(&col)
    }

    fn add(&mut self, row: usize, col: usize) {
        if self.row != row {
            self.cols.clear();
            self.row = row;
        }
        self.cols.push(col);
    }

    /// `n` chars were inserted at `(row, col)`.
    fn inserted(&mut self, row: usize, col: usize, n: usize) {
        if self.row == row {
            self.cols.iter_mut().filter(|c| **c >= col).for_each(|c| *c += n);
        }
    }

    /// `n` chars were removed at `(row, col)`.
    fn removed(&mut self, row: usize, col: usize, n: usize) {
        if self.row == row {
            self.cols.retain(|c| !(col..col + n).contains(c));
            self.cols.iter_mut().filter(|c| **c >= col + n).for_each(|c| *c -= n);
        }
    }

    /// The closer at `(row, col)` was stepped over.
    fn forget(&mut self, row: usize, col: usize) {
        if self.row == row {
            self.cols.retain(|c| *c != col);
        }
    }

    /// Keep only the closers on `row`.
    fn keep_line(&mut self, row: usize) {
        if self.row != row {
            self.cols.clear();
        }
    }

    fn clear(&mut self) {
        self.cols.clear();
    }
}

/// Most bytes of changes one file's history keeps; past it the oldest
/// steps go, with any branch off them ([`UndoTree::prune`]).
const HISTORY_BYTES: usize = 64 << 20;

/// A typing or deleting run ends at a pause this long.
const RUN_PAUSE: Duration = Duration::from_secs(1);

/// What an edit adds to the history: it may continue the open run, or it is
/// a step of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Run(Run),
    Other,
}

/// Per-row chars a comment toggle leaves before a line's indent, from row
/// `first` on (see [`EditorBuffer::toggle_comment`]).
#[derive(Clone, Copy)]
struct Lead<'a> {
    first: usize,
    cols: &'a [usize],
}

impl Lead<'_> {
    fn at(&self, row: usize) -> usize {
        row.checked_sub(self.first).and_then(|i| self.cols.get(i)).copied().unwrap_or(0)
    }
}

impl EditorBuffer {
    /// An empty scratch buffer.
    pub fn scratch() -> Self {
        Self::with_doc(Doc::new(None, vec![String::new()], None))
    }

    /// A view of `doc`'s text, at its current step.
    fn with_doc(doc: Rc<RefCell<Doc>>) -> Self {
        let (lines, at, path) = {
            let d = doc.borrow();
            (d.text.clone(), d.history.tip(), d.path.clone())
        };
        EditorBuffer {
            lines,
            cursor: (0, 0),
            scroll: 0,
            hscroll: 0,
            last_cursor: (0, 0),
            path,
            edits: 0,
            dirty_from: None,
            anchor: None,
            auto_closers: AutoClosers::default(),
            doc,
            id: NEXT_BUFFER.fetch_add(1, Ordering::Relaxed),
            at,
            before: None,
        }
    }

    /// Load `path` into a buffer: a view of the file's open document when
    /// another view has one, else read. A missing file starts empty (a new
    /// file). Any other read error (not UTF-8, unreadable, a directory, not
    /// a regular file, past [`MAX_FILE_BYTES`]) fails: an empty buffer bound
    /// to the file would overwrite it on save.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        if let Some(doc) = open_doc(&path) {
            return Ok(Self::with_doc(doc));
        }
        // Before reading: a write in between then shows as a change.
        let disk = Stamp::of(&path);
        let lines = match read_text(&path) {
            Ok(text) => split_lines(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![String::new()],
            Err(e) => return Err(e),
        };
        let doc = Doc::new(Some(path), lines, disk);
        register(&doc);
        Ok(Self::with_doc(doc))
    }

    /// Unsaved edits since the last load or save: the history is at another
    /// step than the one on disk.
    pub fn modified(&self) -> bool {
        let d = self.doc.borrow();
        d.history.saved() != Some(d.history.current())
    }

    /// The file changed on disk while the buffer had unsaved edits, so the
    /// edits were kept; cleared when the buffer loads or writes the file.
    pub fn changed_on_disk(&self) -> bool {
        self.doc.borrow().changed_on_disk
    }

    /// Set [`Self::changed_on_disk`]; returns what it was.
    pub fn set_changed_on_disk(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.doc.borrow_mut().changed_on_disk, on)
    }

    /// The file's undo history, shared by its views: a tree whose current
    /// step is the text shown. [`Self::goto`] moves to any of its steps.
    pub fn history(&self) -> Ref<'_, UndoTree> {
        Ref::map(self.doc.borrow(), |d| &d.history)
    }

    /// The file is now `path` (Save As, or moved by the app), for every view
    /// of it.
    pub fn moved_to(&mut self, path: PathBuf) {
        self.doc.borrow_mut().path = Some(path.clone());
        register(&self.doc);
        self.path = Some(path);
    }

    /// Catch up with the other views' edits, undos, and moves of the file:
    /// apply the history's changes since this view's step, moving the
    /// cursor and selection with the text. Returns whether anything changed.
    /// A view syncs before it handles input or paints.
    pub fn sync(&mut self) -> bool {
        let d = self.doc.borrow();
        let moved = self.path != d.path;
        if moved {
            self.path = d.path.clone();
        }
        let tip = d.history.tip();
        if self.at == tip {
            return moved;
        }
        let mut first = usize::MAX;
        match d.history.route(self.at) {
            Some(ops) => {
                for op in &ops {
                    first = first.min(op.at.0);
                    op.apply(&mut self.lines);
                    self.cursor = history::shift(self.cursor, op);
                    self.anchor = self.anchor.map(|a| history::shift(a, op));
                }
            }
            // This view's step was pruned: take the text whole.
            None => {
                first = 0;
                self.lines = d.text.clone();
            }
        }
        drop(d);
        self.at = tip;
        self.auto_closers.clear();
        (self.cursor, self.anchor) = (self.clamp(self.cursor), self.anchor.map(|a| self.clamp(a)));
        self.clamp_scroll_bounds();
        if first != usize::MAX {
            self.touch(first);
        }
        true
    }

    /// A short display name for the tab (file name, or "*scratch*").
    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "*scratch*".to_string())
    }

    /// Select the word at `(row, col)`: the run of characters of the same class
    /// (word characters, whitespace, or other punctuation) that contains it. At
    /// the end of a line this is the run before the caret. Does nothing on an
    /// empty line.
    pub fn select_word_at(&mut self, (row, col): (usize, usize)) {
        let chars: Vec<char> = match self.lines.get(row) {
            Some(l) => l.chars().collect(),
            None => return,
        };
        let Some((start, end)) = word_span(&chars, col) else { return };
        self.anchor = Some((row, start));
        self.cursor = (row, end);
    }

    /// Select the whole of line `row` (without its newline).
    pub fn select_line_at(&mut self, row: usize) {
        if row < self.lines.len() {
            self.anchor = Some((row, 0));
            self.cursor = (row, self.line_len(row));
        }
    }

    pub fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
    }

    /// Byte offset of char index `col` within `lines[row]`.
    fn byte_of(&self, row: usize, col: usize) -> usize {
        self.lines[row]
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or_else(|| self.lines[row].len())
    }

    /// The selection as ordered `((start_row, start_col), (end_row, end_col))`, or
    /// `None` if there's no (non-empty) selection.
    pub fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor?;
        let (a, b) = (anchor, self.cursor);
        if a == b {
            return None;
        }
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// The first and last rows a line-wise command acts on: the selection's
    /// (one ending at column 0 of a later line leaves that line out), else the
    /// cursor's.
    pub fn line_rows(&self) -> (usize, usize) {
        let (first, last) = match self.selection() {
            Some(((sr, _), (er, ec))) => (sr, if er > sr && ec == 0 { er - 1 } else { er }),
            None => (self.cursor.0, self.cursor.0),
        };
        (first, last.min(self.lines.len() - 1))
    }

    /// The selected text (rows joined with `\n`), if any.
    pub fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection()?;
        let mut out = String::new();
        for row in sr..=er {
            let line = &self.lines[row];
            let from = if row == sr { self.byte_of(sr, sc) } else { 0 };
            let to = if row == er {
                self.byte_of(er, ec)
            } else {
                line.len()
            };
            out.push_str(&line[from..to]);
            if row != er {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Mark a text-changing edit that starts at line `row`: bumps the edit
    /// count (which the idle-autosave timer watches).
    fn touch(&mut self, row: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(row, |d| d.min(row)));
        self.edits = self.edits.wrapping_add(1);
    }

    /// `p` within the text.
    fn clamp(&self, (row, col): Pos) -> Pos {
        let row = row.min(self.lines.len() - 1);
        (row, col.min(self.line_len(row)))
    }

    /// The cursor, anchor, and auto-inserted closers.
    fn caret(&self) -> Caret {
        Caret { cursor: self.cursor, anchor: self.anchor, closers: self.auto_closers.clone() }
    }

    /// Replace `from..to` with `text`: every text change goes through here.
    /// It joins the current step when this view has it open, else starts one
    /// after the current step (a branch, when that step has others).
    fn splice(&mut self, from: Pos, to: Pos, text: &str) {
        // Positions are this view's; another view's edit since would shift
        // them, so a view syncs before it handles input. This keeps the
        // texts alike regardless.
        self.sync();
        let (from, to) = (self.clamp(from), self.clamp(to));
        let mut d = self.doc.borrow_mut();
        if d.open.is_none_or(|o| o.owner != self.id) {
            let before = self.before.clone().unwrap_or_else(|| self.caret());
            d.history.push(before, HISTORY_BYTES);
            d.open = Some(Open { owner: self.id, run: None });
        }
        let (del, _) = history::splice(&mut self.lines, from, to, text);
        history::splice(&mut d.text, from, to, text);
        d.history.record(Op { at: from, del, ins: text.to_string() });
        self.at = d.history.tip();
        drop(d);
        self.touch(from.0);
    }

    /// Close the current step if this view has it open, so the next edit
    /// starts one.
    fn close_step(&mut self) {
        let mut d = self.doc.borrow_mut();
        if d.open.is_some_and(|o| o.owner == self.id) {
            d.open = None;
        }
    }

    /// Delete the selected text, placing the cursor at its start. Returns whether
    /// anything was deleted.
    pub fn delete_selection(&mut self) -> bool {
        let Some(((sr, sc), (er, ec))) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let start = self.byte_of(sr, sc);
        let end = self.byte_of(er, ec);
        // A range past the end of a line holds no text; the cursor moves to
        // the line's end.
        if sr == er && start == end {
            self.cursor = (sr, sc.min(self.line_len(sr)));
            self.anchor = None;
            return false;
        }
        if sr == er {
            self.auto_closers.removed(sr, sc, ec - sc);
        } else {
            self.auto_closers.clear();
        }
        self.splice((sr, sc), (er, ec), "");
        self.cursor = (sr, sc);
        self.anchor = None;
        true
    }

    /// Insert text (may contain newlines), replacing any selection first.
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        let text: String = s.chars().filter(|c| *c != '\r').collect();
        if text.is_empty() {
            return;
        }
        let at = self.clamp(self.cursor);
        self.splice(at, at, &text);
        let end = history::end_of(at, &text);
        if end.0 == at.0 {
            self.auto_closers.inserted(at.0, at.1, end.1 - at.1);
        } else {
            self.auto_closers.clear();
        }
        self.cursor = end;
    }

    /// Whether typing `s` wraps the selection in brackets.
    fn wraps(&self, s: &str) -> bool {
        typed_char(s).is_some_and(|c| closer_of(c).is_some()) && self.selection().is_some()
    }

    /// The char at `(row, col)`, if any.
    fn char_at(&self, row: usize, col: usize) -> Option<char> {
        self.lines.get(row)?.chars().nth(col)
    }

    /// Type `s`. A single bracket gets the editor's bracket handling: an
    /// opening one wraps a selection, or else inserts its closer after the
    /// cursor (where [`closes_before`] allows); a closing one moves over an
    /// identical closer that follows if the editor inserted it. Anything else
    /// is inserted as is.
    pub fn insert_typed(&mut self, s: &str) {
        let Some(c) = typed_char(s) else {
            return self.insert_str(s);
        };
        let (row, col) = self.cursor;
        if let Some(close) = closer_of(c) {
            if let Some(((sr, sc), (er, ec))) = self.selection() {
                return self.wrap_selection(c, close, (sr, sc), (er, ec));
            }
            if closes_before(self.char_at(row, col)) {
                self.insert_char(c);
                self.splice((row, col + 1), (row, col + 1), &close.to_string());
                self.auto_closers.inserted(row, col + 1, 1);
                self.auto_closers.add(row, col + 1);
                return;
            }
        } else if opener_of(c).is_some()
            && self.selection().is_none()
            && self.char_at(row, col) == Some(c)
            && self.auto_closers.contains(row, col)
        {
            self.auto_closers.forget(row, col);
            self.cursor.1 += 1;
            return;
        }
        self.insert_str(s)
    }

    /// Surround the selection `start..end` with `open` and `close`, keeping the
    /// wrapped text selected (in the direction it was).
    fn wrap_selection(&mut self, open: char, close: char, start: (usize, usize), end: (usize, usize)) {
        let forward = self.anchor.is_some_and(|a| a < self.cursor);
        self.splice(end, end, &close.to_string());
        self.splice(start, start, &open.to_string());
        let inner_start = (start.0, start.1 + 1);
        let inner_end = (end.0, end.1 + usize::from(start.0 == end.0));
        let (anchor, cursor) = if forward { (inner_start, inner_end) } else { (inner_end, inner_start) };
        (self.anchor, self.cursor) = (Some(anchor), cursor);
        self.auto_closers.clear();
    }

    /// Whether the cursor sits between an opening bracket and a closer the
    /// editor inserted for it, with nothing in between (`(|)`).
    fn in_empty_pair(&self) -> bool {
        let (row, col) = self.cursor;
        let before = col.checked_sub(1).and_then(|c| self.char_at(row, c));
        match (before, self.char_at(row, col)) {
            (Some(o), Some(c)) => closer_of(o) == Some(c) && self.auto_closers.contains(row, col),
            _ => false,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        let (row, col) = self.cursor;
        self.splice((row, col), (row, col), &c.to_string());
        self.auto_closers.inserted(row, col, 1);
        self.cursor.1 = col + 1;
    }

    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let (row, col) = self.cursor;
        self.splice((row, col), (row, col), "\n");
        self.auto_closers.clear();
        self.cursor = (row + 1, 0);
    }

    /// Delete the char before the cursor, joining with the previous line at the
    /// start of a line. Deletes the selection instead when there is one.
    pub fn backspace(&mut self) {
        self.delete(Motion::Left);
    }

    /// Delete the char at the cursor, joining the next line at end of line.
    /// Deletes the selection instead when there is one.
    pub fn delete_forward(&mut self) {
        self.delete(Motion::Right);
    }

    /// Delete the selection, else the text between the cursor and `motion`.
    pub fn delete(&mut self, motion: Motion) {
        if self.delete_selection() {
            return;
        }
        // Backspace between an empty pair removes both brackets.
        if motion == Motion::Left && self.in_empty_pair() {
            self.anchor = Some((self.cursor.0, self.cursor.1 + 1));
            self.cursor.1 -= 1;
            self.delete_selection();
            return;
        }
        let t = self.target(motion);
        if t != self.cursor {
            self.anchor = Some(t);
            self.delete_selection();
        }
    }

    /// Move the cursor, clearing any selection (a plain, non-selecting move). A
    /// plain Left/Right collapses a selection to that side.
    pub fn move_cursor(&mut self, motion: Motion) {
        self.cursor = match (self.selection(), motion) {
            (Some((s, _)), Motion::Left) => s,
            (Some((_, e)), Motion::Right) => e,
            _ => self.target(motion),
        };
        self.anchor = None;
    }

    /// Move the cursor while extending a selection (Shift+motion): anchors at the
    /// current cursor if there's no selection yet.
    pub fn move_cursor_selecting(&mut self, motion: Motion) {
        self.anchor.get_or_insert(self.cursor);
        self.cursor = self.target(motion);
    }

    pub fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        let last = self.lines.len() - 1;
        self.cursor = (last, self.line_len(last));
    }

    fn chars(&self, row: usize) -> Vec<char> {
        self.lines[row].chars().collect()
    }

    /// Where `motion` takes the cursor. Word motion crosses line breaks: from a
    /// line's start it reaches the last word of the line above.
    fn target(&self, motion: Motion) -> (usize, usize) {
        let (row, col) = self.cursor;
        let last = self.lines.len() - 1;
        match motion {
            Motion::Left if col > 0 => (row, col - 1),
            Motion::Left if row > 0 => (row - 1, self.line_len(row - 1)),
            Motion::Right if col < self.line_len(row) => (row, col + 1),
            Motion::Right if row < last => (row + 1, 0),
            Motion::Left | Motion::Right => (row, col),
            Motion::WordLeft if col == 0 && row > 0 => {
                (row - 1, word_left(&self.chars(row - 1), self.line_len(row - 1)))
            }
            Motion::WordLeft => (row, word_left(&self.chars(row), col)),
            Motion::WordRight if col == self.line_len(row) && row < last => {
                (row + 1, word_right(&self.chars(row + 1), 0))
            }
            Motion::WordRight => (row, word_right(&self.chars(row), col)),
            Motion::Up if row > 0 => (row - 1, col.min(self.line_len(row - 1))),
            Motion::Down if row < last => (row + 1, col.min(self.line_len(row + 1))),
            Motion::Up | Motion::Down => (row, col),
            Motion::LineStart => (row, 0),
            Motion::LineEnd => (row, self.line_len(row)),
            Motion::DocStart => (0, 0),
            Motion::DocEnd => (last, self.line_len(last)),
        }
    }

    /// Apply a text edit (see [`crate::plugin::text_edit`]). Edits that change
    /// the text are recorded for undo: a run of typed characters is one step
    /// until a word character follows a non-word one, single-character
    /// deletes merge, and a run ends at a pause of [`RUN_PAUSE`], a cursor
    /// move, or another kind of edit.
    pub fn apply(&mut self, edit: Edit, clip: &mut dyn Clipboard) -> Applied {
        self.sync();
        let edits = self.edits;
        let fresh = self.selection().is_some();
        let step = match &edit {
            // Wrapping a selection in brackets is a step of its own: typing on
            // doesn't merge into it.
            Edit::Insert(s) if self.wraps(s) => Some(Step::Other),
            Edit::Insert(s) => match typed_char(s) {
                Some(c) if c != '\n' => Some(Step::Run(Run::Typing { word: is_word(c) })),
                _ => Some(Step::Other),
            },
            Edit::Delete(Motion::Left | Motion::Right) if !fresh => Some(Step::Run(Run::Deleting)),
            Edit::Delete(_) | Edit::Cut | Edit::Paste => Some(Step::Other),
            Edit::Move { .. } | Edit::SelectAll | Edit::Copy | Edit::Undo | Edit::Redo => None,
        };
        let now = Instant::now();
        if let Some(step) = step {
            // Typing over a selection starts a step; typing on continues it.
            if fresh || !self.continues(step, now) {
                self.close_step();
            }
            self.before = Some(self.caret());
        }
        match edit {
            Edit::Move { to, select: true } => self.move_cursor_selecting(to),
            Edit::Move { to, select: false } => self.move_cursor(to),
            Edit::Delete(to) => self.delete(to),
            Edit::Insert(s) => self.insert_typed(&s),
            Edit::SelectAll => self.select_all(),
            Edit::Copy => {
                if let Some(t) = self.selected_text() {
                    clip.set_text(t);
                }
            }
            Edit::Cut => {
                if let Some(t) = self.selected_text() {
                    self.delete_selection();
                    clip.set_text(t);
                }
            }
            Edit::Paste => {
                if let Some(t) = clip.text() {
                    self.insert_str(&t);
                }
            }
            Edit::Undo => self.undo(),
            Edit::Redo => self.redo(),
        }
        self.before = None;
        self.auto_closers.keep_line(self.cursor.0);
        let changed = self.edits != edits;
        match step {
            Some(step) if changed => self.note_run(step, now),
            // A text edit that changed nothing opened no step.
            Some(_) => {}
            None => self.close_step(),
        }
        if changed {
            Applied::Changed
        } else {
            Applied::Moved
        }
    }

    /// Whether a `step` edit now joins the step this view has open: both
    /// are of one run, the edit is where the last left the cursor, and no
    /// pause came between.
    fn continues(&self, step: Step, now: Instant) -> bool {
        let Step::Run(next) = step else { return false };
        let d = self.doc.borrow();
        let Some(Open { owner, run: Some((run, at, when)) }) = d.open else { return false };
        owner == self.id && at == self.cursor && now.duration_since(when) < RUN_PAUSE && run.takes(next)
    }

    /// After a `step` edit changed the text: the caret its step now leaves,
    /// and the run the next edit may continue.
    fn note_run(&mut self, step: Step, now: Instant) {
        let caret = self.caret();
        let mut d = self.doc.borrow_mut();
        if d.open.is_some_and(|o| o.owner == self.id) {
            d.history.cur_mut().after = caret;
            let run = match step {
                Step::Run(r) => Some((r, self.cursor, now)),
                Step::Other => None,
            };
            d.open = run.is_some().then_some(Open { owner: self.id, run });
        }
    }

    /// End the open typing/deleting run, so the next edit starts a new undo
    /// step (e.g. after the cursor moved by mouse).
    pub fn end_undo_run(&mut self) {
        self.close_step();
        self.auto_closers.keep_line(self.cursor.0);
    }

    /// Step back to the text before the current step, with the caret as it
    /// was before that step. Editing then adds a branch; the undone step
    /// stays in the tree.
    pub fn undo(&mut self) {
        self.sync();
        self.close_step();
        let (parent, caret) = {
            let d = self.doc.borrow();
            let n = d.history.node(d.history.current()).expect("the current step is in the tree");
            (n.parent(), n.before.clone())
        };
        if let Some(parent) = parent {
            self.jump(parent, caret);
        }
    }

    /// Redo the step undone last from here (the branch last visited), with
    /// the caret as that step left it.
    pub fn redo(&mut self) {
        self.sync();
        self.close_step();
        let next = {
            let d = self.doc.borrow();
            let n = d.history.node(d.history.current()).expect("the current step is in the tree");
            n.redo_child().and_then(|c| Some((c, d.history.node(c)?.after.clone())))
        };
        if let Some((child, caret)) = next {
            self.jump(child, caret);
        }
    }

    /// Move to step `id` of [`Self::history`], on any branch, with the caret
    /// as that step left it. Redo from the steps above it then leads to it.
    /// Returns whether `id` is in the tree.
    pub fn goto(&mut self, id: NodeId) -> bool {
        self.sync();
        self.close_step();
        let caret = match self.history().node(id) {
            Some(n) => n.after.clone(),
            None => return false,
        };
        self.jump(id, caret);
        true
    }

    /// Make `id` the history's current step and this view's text and caret
    /// its; the buffer is unmodified if it is the saved step.
    fn jump(&mut self, id: NodeId, caret: Caret) {
        {
            let mut d = self.doc.borrow_mut();
            let Some(ops) = d.history.jump(id) else { return };
            for op in &ops {
                op.apply(&mut d.text);
            }
            d.open = None;
        }
        self.sync();
        self.cursor = self.clamp(caret.cursor);
        self.anchor = caret.anchor.map(|a| self.clamp(a));
        self.auto_closers = caret.closers;
        self.edits = self.edits.wrapping_add(1);
        // Marked while saving this text would ask: it predates a reload, or
        // the file changed since the buffer read it.
        let conflicted = self.conflicted();
        self.set_changed_on_disk(conflicted);
    }

    /// Comment out the selected lines (the cursor's line without a selection),
    /// or uncomment them when every non-blank one is already commented, using
    /// `syntax`: its line token on each non-blank line at the least indent, else
    /// its block tokens around the first and last non-blank lines (or off each
    /// line that is a block comment of its own), on [`Self::line_rows`].
    /// `lead[i]` is the chars of row `first + i` that come before its indent
    /// (a quoted code block's `>`), 0 past its end. One undo step. Returns
    /// whether the text changed (not when blank or the language has no comment
    /// syntax).
    pub fn toggle_comment(&mut self, syntax: CommentSyntax, lead: &[usize]) -> bool {
        self.sync();
        let (first, last) = self.line_rows();
        let lead = Lead { first, cols: lead };
        let edits = self.edits;
        self.close_step();
        self.before = Some(self.caret());
        if let Some(token) = syntax.line {
            self.toggle_line_comment(token, first, last, lead);
        } else if let Some((open, close)) = syntax.block {
            self.toggle_block_comment(open, close, first, last, lead);
        }
        self.before = None;
        let changed = self.edits != edits;
        if changed {
            self.note_run(Step::Other, Instant::now());
        }
        changed
    }

    /// The rows among `first..=last` with text past their lead.
    fn text_rows(&self, first: usize, last: usize, lead: Lead) -> Vec<usize> {
        (first..=last).filter(|r| self.indent_of(*r, lead) < self.lines[*r].chars().count()).collect()
    }

    /// The column after `row`'s lead and the whitespace following it.
    fn indent_of(&self, row: usize, lead: Lead) -> usize {
        let lead = lead.at(row).min(self.lines[row].chars().count());
        lead + self.lines[row].chars().skip(lead).take_while(|c| c.is_whitespace()).count()
    }

    fn toggle_line_comment(&mut self, token: &str, first: usize, last: usize, lead: Lead) {
        let rows = self.text_rows(first, last, lead);
        if rows.is_empty() {
            return;
        }
        let width = token.chars().count();
        let commented = |e: &Self, r: usize| {
            let at = e.byte_of(r, e.indent_of(r, lead));
            e.lines[r][at..].starts_with(token)
        };
        if rows.iter().all(|r| commented(self, *r)) {
            for r in rows {
                let col = self.indent_of(r, lead);
                let after = self.byte_of(r, col) + token.len();
                let space = usize::from(self.lines[r][after..].starts_with(' '));
                self.remove_at(r, col, width + space);
            }
        } else {
            // The least indent past each row's own lead.
            let depth = rows.iter().map(|r| self.indent_of(*r, lead) - lead.at(*r)).min().unwrap_or(0);
            for r in rows {
                let col = lead.at(r).min(self.lines[r].chars().count()) + depth;
                self.insert_at(r, col, &format!("{token} "));
            }
        }
    }

    fn toggle_block_comment(&mut self, open: &str, close: &str, first: usize, last: usize, lead: Lead) {
        let rows = self.text_rows(first, last, lead);
        let (Some(&top), Some(&bottom)) = (rows.first(), rows.last()) else {
            return;
        };
        // Lines commented one by one: unwrapping the first and last as one
        // block would leave their inner tokens behind (`a -->`, `<!-- b`).
        let each = rows.len() > 1 && rows.iter().all(|&r| self.only_block_on(open, close, r, lead));
        if each {
            for r in rows {
                self.unwrap_block(open, close, r, r, lead);
            }
        } else if self.block_wrapped(open, close, top, bottom, lead) {
            self.unwrap_block(open, close, top, bottom, lead);
        } else {
            let end = self.lines[bottom].trim_end().chars().count();
            self.insert_at(bottom, end, &format!(" {close}"));
            self.insert_at(top, self.indent_of(top, lead), &format!("{open} "));
        }
    }

    /// Whether `open` starts row `top` (after its indent) and `close` ends row
    /// `bottom`, as two distinct tokens.
    fn block_wrapped(&self, open: &str, close: &str, top: usize, bottom: usize, lead: Lead) -> bool {
        let start = self.indent_of(top, lead);
        let head = &self.lines[top][self.byte_of(top, start)..];
        let tail = self.lines[bottom].trim_end();
        head.starts_with(open)
            && tail.ends_with(close)
            && (top != bottom || tail.chars().count() - start >= open.chars().count() + close.chars().count())
    }

    /// Whether row `r` is one block comment, with no closer inside it.
    fn only_block_on(&self, open: &str, close: &str, r: usize, lead: Lead) -> bool {
        if !self.block_wrapped(open, close, r, r, lead) {
            return false;
        }
        let at = self.byte_of(r, self.indent_of(r, lead));
        let tail = self.lines[r].trim_end();
        !tail[at + open.len()..tail.len() - close.len()].contains(close)
    }

    /// Remove the block tokens [`Self::block_wrapped`] found, with the space
    /// inside each.
    fn unwrap_block(&mut self, open: &str, close: &str, top: usize, bottom: usize, lead: Lead) {
        let (open_w, close_w) = (open.chars().count(), close.chars().count());
        let start = self.indent_of(top, lead);
        let end = self.lines[bottom].trim_end().chars().count();
        let at = self.byte_of(top, start);
        let tail = self.lines[bottom].trim_end();
        // The closer first, so the opener's column stays valid on a shared line.
        let pad = usize::from(
            tail[..tail.len() - close.len()].ends_with(' ') && (top != bottom || end - close_w > start + open_w),
        );
        self.remove_at(bottom, end - close_w - pad, close_w + pad);
        let space = usize::from(self.lines[top][at + open.len()..].starts_with(' '));
        self.remove_at(top, start, open_w + space);
    }

    /// Insert `text` at `(row, col)`, moving the cursor and anchor along.
    fn insert_at(&mut self, row: usize, col: usize, text: &str) {
        self.splice((row, col), (row, col), text);
        let n = text.chars().count();
        self.auto_closers.inserted(row, col, n);
        let shift = |p: &mut (usize, usize)| {
            if p.0 == row && p.1 >= col {
                p.1 += n;
            }
        };
        shift(&mut self.cursor);
        self.anchor.iter_mut().for_each(shift);
    }

    /// Remove `n` chars at `(row, col)`, moving the cursor and anchor along.
    fn remove_at(&mut self, row: usize, col: usize, n: usize) {
        self.splice((row, col), (row, col + n), "");
        self.auto_closers.removed(row, col, n);
        let shift = |p: &mut (usize, usize)| {
            if p.0 == row && p.1 > col {
                p.1 = (p.1 - n).max(col);
            }
        };
        shift(&mut self.cursor);
        self.anchor.iter_mut().for_each(shift);
    }

    /// Scroll to keep the cursor visible — vertically within `body_h` px,
    /// line `ln` taking `height(self, ln)`: the space above it and its rows'
    /// height (for the cursor's line, down to the bottom of the cursor's
    /// row) — and (when `hcols` is `Some`, i.e.
    /// soft-wrap off) horizontally within `hcols` columns — but only when the
    /// cursor actually moved since the last follow, so manual scrolling (which
    /// leaves the cursor put) isn't yanked back each frame. The scroll stays a
    /// top line: the cursor's line shows from its top, even when it alone is
    /// taller than the body.
    pub fn follow_cursor(
        &mut self,
        body_h: f32,
        mut height: impl FnMut(&Self, usize) -> (f32, f32),
        hcols: Option<usize>,
    ) {
        if self.cursor == self.last_cursor {
            return;
        }
        if self.cursor.0 < self.scroll {
            self.scroll = self.cursor.0;
        } else {
            // Walk up from the cursor while the lines fit: bounded by the
            // body's height, however far the cursor jumped. The top line's
            // space doesn't show; a line's shows once one is above it.
            let (mut top, mut used) = (self.cursor.0, height(self, self.cursor.0).1);
            while top > self.scroll {
                let h = height(self, top).0 + height(self, top - 1).1;
                // Slack for the float sum of fractional row heights.
                if used + h > body_h + 0.01 {
                    break;
                }
                used += h;
                top -= 1;
            }
            self.scroll = top;
        }
        if let Some(cols) = hcols {
            let cols = cols.max(1);
            if self.cursor.1 < self.hscroll {
                self.hscroll = self.cursor.1;
            } else if self.cursor.1 >= self.hscroll + cols {
                self.hscroll = self.cursor.1 + 1 - cols;
            }
        }
        self.last_cursor = self.cursor;
    }

    /// Clamp the vertical scroll so it can't run past the last line.
    pub fn clamp_scroll_bounds(&mut self) {
        let max = self.lines.len().saturating_sub(1);
        if self.scroll > max {
            self.scroll = max;
        }
    }

    /// Replace the text with its file's as it is on disk now (another program
    /// wrote it), as one undo step after the current one holding only the
    /// lines that differ. That step is the saved one; every other step is
    /// stale, so undoing past the reload gives an unsaved edit that saving
    /// asks about. Returns whether the text changed. A file that can't be
    /// read as text keeps the buffer, marked changed on disk, and counts as
    /// seen, so the next check doesn't read it again.
    pub fn reload(&mut self) -> std::io::Result<bool> {
        self.sync();
        let Some(path) = self.path.clone() else { return Ok(false) };
        self.doc.borrow_mut().disk = Stamp::of(&path);
        let lines = match read_text(&path) {
            Ok(text) => split_lines(&text),
            Err(e) => {
                let mut d = self.doc.borrow_mut();
                d.changed_on_disk = true;
                d.history.mark_stale(true);
                return Err(e);
            }
        };
        // The next edit starts a step, whether the reload changed the text
        // or only made the current step the saved one.
        {
            let mut d = self.doc.borrow_mut();
            d.changed_on_disk = false;
            d.open = None;
        }
        let changed = lines != self.lines;
        if changed {
            self.before = Some(self.caret());
            let (from, to, text) = line_diff(&self.lines, &lines);
            self.splice(from, to, &text);
            self.before = None;
            let op = self.history().node(self.at.0).and_then(|n| n.ops().last().cloned());
            if let Some(op) = op {
                self.cursor = self.clamp(history::shift(self.cursor, &op));
            }
            self.anchor = None;
            self.auto_closers = AutoClosers::default();
            self.clamp_scroll_bounds();
            let caret = self.caret();
            let mut d = self.doc.borrow_mut();
            d.history.cur_mut().after = caret;
            d.open = None;
            d.history.mark_stale(true);
        }
        // Already what's on disk if unchanged, whatever the history says.
        let mut d = self.doc.borrow_mut();
        d.history.cur_mut().stale = false;
        d.history.set_saved();
        Ok(changed)
    }

    /// Whether another program wrote the file since the buffer loaded or wrote
    /// it. A file that is gone is not a change: saving recreates it.
    pub fn disk_changed(&self) -> bool {
        let Some(now) = self.path.as_deref().and_then(Stamp::of) else { return false };
        self.doc.borrow().disk != Some(now)
    }

    /// Whether writing the buffer now could lose what is on disk: the file
    /// changed since the buffer read it ([`Self::disk_changed`]), or the text
    /// isn't based on the file as it was last read (undo went back past a
    /// reload, or the file became unreadable).
    pub fn conflicted(&self) -> bool {
        let stale = {
            let d = self.doc.borrow();
            d.history.node(d.history.current()).is_some_and(|n| n.stale)
        };
        stale || self.disk_changed()
    }

    /// Write the buffer to its path. Returns whether it had a path to save to.
    pub fn save(&mut self) -> std::io::Result<bool> {
        self.sync();
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        self.write_to(&path)?;
        Ok(true)
    }

    /// Write the buffer to `path` and make that its file, keeping its
    /// history. On failure the buffer keeps its old path, so later saves
    /// don't aim at the failed one.
    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.sync();
        self.write_to(&path)?;
        self.moved_to(path);
        Ok(())
    }

    /// Write the buffer to `path`, leaving its file, unsaved state, and
    /// history as they are.
    pub fn save_copy(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.file_text())
    }

    /// The text as written to a file: lines joined, ending in a newline.
    fn file_text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }

    fn write_to(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, self.file_text())?;
        let mut d = self.doc.borrow_mut();
        d.disk = Stamp::of(path);
        d.changed_on_disk = false;
        d.history.mark_stale(false);
        // Typing after a save starts a new step, so undo can return here.
        d.history.set_saved();
        d.open = None;
        Ok(())
    }
}

/// The change that takes `old` to `new` (which differ), as a splice of the
/// rows between their common first and last rows.
fn line_diff(old: &[String], new: &[String]) -> (Pos, Pos, String) {
    let pre = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - pre;
    let suf = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    let (old_end, new_end) = (old.len() - suf, new.len() - suf);
    let mid = new[pre..new_end].join("\n");
    let end = |row: usize| (row, old[row].chars().count());
    if pre > 0 {
        // From the end of the last row kept above, taking its newline.
        let from = end(pre - 1);
        let to = if old_end > pre { end(old_end - 1) } else { from };
        let text = if new_end > pre { format!("\n{mid}") } else { String::new() };
        (from, to, text)
    } else if suf > 0 {
        // Up to the start of the first row kept below, taking the newline.
        let text = if new_end > 0 { format!("{mid}\n") } else { String::new() };
        ((0, 0), (old_end, 0), text)
    } else {
        ((0, 0), end(old.len() - 1), mid)
    }
}

#[cfg(test)]
impl EditorBuffer {
    /// Take the text as saved, without writing it.
    fn mark_saved(&mut self) {
        self.close_step();
        self.doc.borrow_mut().history.set_saved();
    }
}

/// Split file text into lines, dropping a single trailing newline so a normal
/// file doesn't gain a phantom empty last line.
pub(crate) fn split_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
    if lines.len() > 1 && lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// The largest file the editor reads: a whole file sits in memory as lines.
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// `path`'s text, if it is a regular file of at most [`MAX_FILE_BYTES`].
/// A device (`/dev/zero`) or a FIFO would never end, or block the UI on
/// open, so neither is read.
pub fn read_text(path: &Path) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read};
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    // A FIFO's open waits for a writer; non-blocking, it returns at once
    // (and is then refused below). A regular file reads as usual.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NONBLOCK);
    let mut file = open.open(path)?;
    // Of what was opened, not of a path that may since name another.
    let meta = file.metadata()?;
    if !meta.is_file() {
        let what = if meta.is_dir() { "a folder" } else { "not a regular file" };
        return Err(Error::new(ErrorKind::InvalidInput, format!("{} is {what}", path.display())));
    }
    let too_big = || Error::new(ErrorKind::InvalidData, format!("{} is over {} MB", path.display(), MAX_FILE_BYTES >> 20));
    if meta.len() > MAX_FILE_BYTES {
        return Err(too_big());
    }
    // It may have grown since: read no further than the cap.
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&mut file).take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(too_big());
    }
    String::from_utf8(bytes).map_err(|e| Error::new(ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_report_the_first_line_they_changed() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("a\nb\nc\nd");
        assert_eq!(e.dirty_from.take(), Some(0));

        e.cursor = (2, 1);
        e.insert_char('x');
        assert_eq!(e.dirty_from.take(), Some(2));
        e.insert_newline();
        assert_eq!(e.dirty_from.take(), Some(2), "the split line, not the new one");

        // Several edits before anyone looks: the earliest wins.
        e.cursor = (3, 0);
        e.insert_char('y');
        e.cursor = (1, 0);
        e.insert_char('z');
        e.cursor = (3, 0);
        e.insert_char('w');
        assert_eq!(e.dirty_from.take(), Some(1));

        // A selection reaching back to an earlier line starts there.
        e.anchor = Some((1, 0));
        e.cursor = (3, 1);
        assert!(e.delete_selection());
        assert_eq!(e.dirty_from.take(), Some(1));

        e.cursor = (2, 0);
        e.apply(Edit::Insert("q".into()), &mut None::<String>);
        assert_eq!(e.dirty_from.take(), Some(2));
        e.apply(Edit::Undo, &mut None::<String>);
        assert_eq!(e.dirty_from.take(), Some(2), "from the line the undone change was on");
    }

    #[test]
    fn typing_and_newlines() {
        let mut e = EditorBuffer::scratch();
        for c in "abc".chars() {
            e.insert_char(c);
        }
        assert_eq!(e.lines, vec!["abc"]);
        assert_eq!(e.cursor, (0, 3));
        e.insert_newline();
        e.insert_char('d');
        assert_eq!(e.lines, vec!["abc".to_string(), "d".to_string()]);
        assert_eq!(e.cursor, (1, 1));
        assert!(e.modified());
    }

    #[test]
    fn selection_copy_and_replace() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("hello world");
        // Select the last 5 chars ("world") by shift-moving left from the end.
        for _ in 0..5 {
            e.move_cursor_selecting(Motion::Left);
        }
        assert_eq!(e.selected_text().as_deref(), Some("world"));
        // Typing replaces the selection.
        e.insert_str("there");
        assert_eq!(e.lines, vec!["hello there"]);
        assert!(e.selection().is_none(), "insert clears the selection");
    }

    #[test]
    fn selection_spans_lines_and_deletes() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("ab\ncd\nef");
        // cursor at end (2,2). Select back to (0,1): "b\ncd\ne".
        e.cursor = (2, 1);
        e.anchor = Some((0, 1));
        assert_eq!(e.selected_text().as_deref(), Some("b\ncd\ne"));
        e.delete_selection();
        assert_eq!(e.lines, vec!["af"]);
        assert_eq!(e.cursor, (0, 1));
    }

    #[test]
    fn plain_move_clears_selection() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("abc");
        e.move_cursor_selecting(Motion::Left);
        assert!(e.selection().is_some());
        e.move_cursor(Motion::Left);
        assert!(e.selection().is_none(), "a non-selecting move clears it");
    }

    #[test]
    fn insert_in_the_middle() {
        let mut e = EditorBuffer::scratch();
        for c in "ac".chars() {
            e.insert_char(c);
        }
        e.move_cursor(Motion::Left); // between a and c
        e.insert_char('b');
        assert_eq!(e.lines, vec!["abc"]);
        assert_eq!(e.cursor, (0, 2));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut e = EditorBuffer::scratch();
        for c in "ab".chars() {
            e.insert_char(c);
        }
        e.insert_newline();
        for c in "cd".chars() {
            e.insert_char(c);
        }
        // cursor at (1,2); Home then backspace joins the two lines.
        e.move_cursor(Motion::LineStart);
        e.backspace();
        assert_eq!(e.lines, vec!["abcd"]);
        assert_eq!(e.cursor, (0, 2));
    }

    #[test]
    fn selects_the_word_under_a_position() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("let foo_1 = (bar);");
        e.select_word_at((0, 6));
        assert_eq!(e.selected_text().as_deref(), Some("foo_1"));
        e.select_word_at((0, 3));
        assert_eq!(e.selected_text().as_deref(), Some(" "), "whitespace is a run");
        e.select_word_at((0, 12));
        assert_eq!(e.selected_text().as_deref(), Some("("), "punctuation is one run");
        e.select_word_at((0, 18));
        assert_eq!(e.selected_text().as_deref(), Some(");"), "end of line takes the last run");
    }

    #[test]
    fn word_motion_crosses_lines() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("let foo = 1;\nbar");
        e.cursor = (1, 0);
        e.move_cursor(Motion::WordLeft);
        assert_eq!(e.cursor, (0, 10), "the last word of the line above");
        e.move_cursor(Motion::WordLeft);
        assert_eq!(e.cursor, (0, 4));
        e.cursor = (0, 12);
        e.move_cursor(Motion::WordRight);
        assert_eq!(e.cursor, (1, 3));
    }

    #[test]
    fn deletes_by_word_and_to_line_start() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("one two three");
        e.delete(Motion::WordLeft);
        assert_eq!(e.lines, vec!["one two "]);
        e.delete(Motion::LineStart);
        assert_eq!(e.lines, vec![""]);
    }

    #[test]
    fn select_all_and_paste_over_it() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("a\nb");
        let mut clip = Some("x\ny".to_string());
        e.apply(Edit::SelectAll, &mut clip);
        assert_eq!(e.selected_text().as_deref(), Some("a\nb"));
        e.apply(Edit::Paste, &mut clip);
        assert_eq!(e.lines, vec!["x", "y"]);
    }

    fn typed(e: &mut EditorBuffer, text: &str) {
        for c in text.chars() {
            e.apply(Edit::Insert(c.to_string()), &mut None::<String>);
        }
    }

    fn buffer_with(text: &str) -> EditorBuffer {
        let mut e = EditorBuffer::scratch();
        e.insert_str(text);
        e.cursor = (0, 0);
        e.anchor = None;
        e.mark_saved();
        e
    }

    fn select(e: &mut EditorBuffer, from: (usize, usize), to: (usize, usize)) {
        e.anchor = Some(from);
        e.cursor = to;
    }

    #[test]
    fn an_opening_bracket_inserts_its_closer_after_the_cursor() {
        for (open, want) in [('(', "()"), ('[', "[]"), ('{', "{}")] {
            let mut e = EditorBuffer::scratch();
            typed(&mut e, &open.to_string());
            assert_eq!(e.lines, vec![want]);
            assert_eq!(e.cursor, (0, 1), "between the pair");
        }
        // Before whitespace, a closer, or the line's end it closes; before a word it doesn't.
        let mut e = buffer_with("a b");
        e.cursor = (0, 1);
        typed(&mut e, "(");
        assert_eq!(e.lines, vec!["a() b"]);
        let mut e = buffer_with("foo");
        typed(&mut e, "(");
        assert_eq!(e.lines, vec!["(foo"], "a word follows");
        let mut e = buffer_with("(x)");
        e.cursor = (0, 2);
        typed(&mut e, "[");
        assert_eq!(e.lines, vec!["(x[])"]);
        // Before an opening bracket, a quote, or an operator it starts an
        // expression the user is about to close themselves.
        for (text, want) in [("\"s\"", "(\"s\""), ("[1]", "([1]"), ("-x", "(-x"), ("&y", "(&y")] {
            let mut e = buffer_with(text);
            typed(&mut e, "(");
            assert_eq!(e.lines, vec![want]);
        }
        for (text, want) in [(";", "();"), (",", "(),"), (".x", "().x"), ("}", "()}")] {
            let mut e = buffer_with(text);
            typed(&mut e, "(");
            assert_eq!(e.lines, vec![want]);
        }
        // Other characters, and a closing bracket with no twin after it, are plain.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "a)");
        assert_eq!(e.lines, vec!["a)"]);
    }

    #[test]
    fn typing_the_closer_moves_over_it_and_backspace_removes_an_empty_pair() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "f(a[1])");
        assert_eq!(e.lines, vec!["f(a[1])"]);
        assert_eq!(e.cursor, (0, 7), "each closer was stepped over");
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "{(");
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec!["{}"], "the empty () goes together");
        assert_eq!(e.cursor, (0, 1));
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec![""]);
        // A pair with something in it deletes one char; mismatched pairs too.
        let mut e = buffer_with("(a)");
        e.cursor = (0, 2);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec!["()"]);
        let mut e = buffer_with("(]");
        e.cursor = (0, 1);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec!["]"]);
    }

    #[test]
    fn only_closers_the_editor_inserted_are_stepped_over_or_removed_with_their_opener() {
        // A closer that was already in the text is typed before, not stepped over.
        let mut e = buffer_with("()");
        e.cursor = (0, 1);
        typed(&mut e, ")");
        assert_eq!(e.lines, vec!["())"]);
        assert_eq!(e.cursor, (0, 2));
        // Backspace between an old pair removes just the opener.
        let mut e = buffer_with("()");
        e.cursor = (0, 1);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec![")"]);
        // A closer typed by the user is not auto-inserted, so it isn't stepped over.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "a))");
        assert_eq!(e.lines, vec!["a))"]);
    }

    #[test]
    fn an_auto_inserted_closer_stays_tracked_through_edits_before_it_and_is_dropped_after() {
        // Typing inside the pair moves the closer along; it is still stepped over.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "(abc)");
        assert_eq!((e.lines.clone(), e.cursor), (vec!["(abc)".to_string()], (0, 5)));
        // Text typed before the pair shifts it.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "(");
        e.cursor = (0, 0);
        typed(&mut e, "xy");
        e.cursor = (0, 3);
        typed(&mut e, ")");
        assert_eq!((e.lines.clone(), e.cursor), (vec!["xy()".to_string()], (0, 4)), "stepped over");
        // Deleting before the pair shifts it back.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "ab (");
        e.cursor = (0, 0);
        e.apply(Edit::Delete(Motion::Right), &mut None::<String>);
        e.cursor = (0, 3);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec!["b "], "the empty pair went together");
        // Leaving the line forgets the closer.
        let mut e = buffer_with("\ny");
        typed(&mut e, "(");
        assert_eq!(e.lines[0], "()");
        e.apply(Edit::Move { to: Motion::Down, select: false }, &mut None::<String>);
        e.cursor = (0, 1);
        typed(&mut e, ")");
        assert_eq!(e.lines[0], "())", "not stepped over after leaving the line");
        // Redo brings the tracking back with the text.
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "(");
        e.undo();
        e.redo();
        typed(&mut e, ")");
        assert_eq!(e.lines, vec!["()"]);
    }

    #[test]
    fn a_bracket_over_a_selection_wraps_it_across_lines() {
        let mut e = buffer_with("ab\ncd");
        select(&mut e, (0, 1), (1, 1));
        typed(&mut e, "{");
        assert_eq!(e.lines, vec!["a{b", "c}d"]);
        assert_eq!(e.selection(), Some(((0, 2), (1, 1))), "the wrapped text stays selected");
        // Typing a bracket again nests.
        typed(&mut e, "(");
        assert_eq!(e.lines, vec!["a{(b", "c)}d"]);
        // A backward selection on one line.
        let mut e = buffer_with("abcd");
        select(&mut e, (0, 3), (0, 1));
        typed(&mut e, "[");
        assert_eq!(e.lines, vec!["a[bc]d"]);
        assert_eq!((e.anchor, e.cursor), (Some((0, 4)), (0, 2)));
        // A closing bracket just replaces the selection.
        let mut e = buffer_with("abcd");
        select(&mut e, (0, 1), (0, 3));
        typed(&mut e, ")");
        assert_eq!(e.lines, vec!["a)d"]);
    }

    #[test]
    fn wrapping_and_auto_closing_each_undo_in_one_step() {
        let mut e = buffer_with("abcd");
        select(&mut e, (0, 1), (0, 3));
        typed(&mut e, "(");
        typed(&mut e, "x"); // typing on replaces the selection, a step apart from the wrap
        e.undo();
        assert_eq!(e.lines, vec!["a(bc)d"]);
        e.undo();
        assert_eq!(e.lines, vec!["abcd"], "the wrap is one step");
        assert_eq!(e.selection(), Some(((0, 1), (0, 3))));

        let mut e = EditorBuffer::scratch();
        typed(&mut e, "(");
        assert_eq!(e.lines, vec!["()"]);
        e.undo();
        assert_eq!(e.lines, vec![""], "the pair is added and removed as one");
        e.redo();
        assert_eq!(e.lines, vec!["()"]);
        // Backspacing the pair is one step as well.
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.lines, vec![""]);
        e.undo();
        assert_eq!(e.lines, vec!["()"]);
    }

    const HASH: CommentSyntax = CommentSyntax { line: Some("#"), block: None };
    const HTML: CommentSyntax = CommentSyntax { line: None, block: Some(("<!--", "-->")) };

    #[test]
    fn a_line_comment_toggles_on_at_the_least_indent_and_off_again() {
        let mut e = buffer_with("  a = 1\n\n    b = 2\n  c = 3");
        select(&mut e, (0, 3), (3, 2));
        assert!(e.toggle_comment(HASH, &[]));
        assert_eq!(e.lines, vec!["  # a = 1", "", "  #   b = 2", "  # c = 3"], "blank lines are skipped");
        assert_eq!(e.selection(), Some(((0, 5), (3, 4))), "the selection follows the text");
        assert!(e.toggle_comment(HASH, &[]));
        assert_eq!(e.lines, vec!["  a = 1", "", "    b = 2", "  c = 3"]);
        assert_eq!(e.selection(), Some(((0, 3), (3, 2))));
    }

    #[test]
    fn a_mixed_selection_comments_everything_and_uncommenting_needs_all_commented() {
        let mut e = buffer_with("# a\nb");
        select(&mut e, (0, 0), (1, 1));
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.lines, vec!["# # a", "# b"]);
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.lines, vec!["# a", "b"]);
        // No space after the token: only the token goes.
        let mut e = buffer_with("#a");
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.lines, vec!["a"]);
    }

    #[test]
    fn without_a_selection_the_cursor_line_toggles_and_a_selection_ending_at_column_zero_stops_short() {
        let mut e = buffer_with("a\nb\nc");
        e.cursor = (1, 1);
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.lines, vec!["a", "# b", "c"]);
        assert_eq!(e.cursor, (1, 3));
        let mut e = buffer_with("a\nb\nc");
        select(&mut e, (0, 0), (2, 0));
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.lines, vec!["# a", "# b", "c"]);
        // A blank cursor line, or a language without comments, changes nothing.
        let mut e = buffer_with("\nx");
        assert!(!e.toggle_comment(HASH, &[]));
        assert!(!e.toggle_comment(CommentSyntax::default(), &[]));
        assert!(!e.modified());
        let steps = e.history().steps();
        e.toggle_comment(HASH, &[]);
        assert_eq!(e.history().steps(), steps, "a no-op leaves no undo step");
    }

    #[test]
    fn a_block_comment_wraps_the_first_and_last_lines() {
        let mut e = buffer_with("  one\n  two");
        select(&mut e, (0, 0), (1, 5));
        assert!(e.toggle_comment(HTML, &[]));
        assert_eq!(e.lines, vec!["  <!-- one", "  two -->"]);
        assert!(e.toggle_comment(HTML, &[]));
        assert_eq!(e.lines, vec!["  one", "  two"]);
        let mut e = buffer_with("text");
        e.cursor = (0, 2);
        e.toggle_comment(HTML, &[]);
        assert_eq!(e.lines, vec!["<!-- text -->"]);
        assert_eq!(e.cursor, (0, 7));
        e.toggle_comment(HTML, &[]);
        assert_eq!(e.lines, vec!["text"]);
        assert_eq!(e.cursor, (0, 2));
        let mut e = buffer_with("<!--x-->");
        e.toggle_comment(HTML, &[]);
        assert_eq!(e.lines, vec!["x"]);
        // Lines commented one by one are uncommented one by one.
        let mut e = buffer_with("<!-- a -->\n\n  <!-- b -->");
        select(&mut e, (0, 0), (2, 12));
        assert!(e.toggle_comment(HTML, &[]));
        assert_eq!(e.lines, vec!["a", "", "  b"]);
        // Rows with nothing past their lead are blank.
        let mut e = buffer_with(">\n> a\n> b\n> ");
        select(&mut e, (0, 0), (3, 2));
        assert!(e.toggle_comment(HTML, &[1, 1, 1, 1]));
        assert_eq!(e.lines, vec![">", "> <!-- a", "> b -->", "> "]);
        assert!(e.toggle_comment(HTML, &[1, 1, 1, 1]));
        assert_eq!(e.lines, vec![">", "> a", "> b", "> "]);
    }

    #[test]
    fn toggling_a_comment_is_one_undo_step() {
        let mut e = buffer_with("a\nb\nc");
        select(&mut e, (0, 0), (2, 1));
        e.toggle_comment(HASH, &[]);
        assert!(e.modified());
        e.undo();
        assert_eq!(e.lines, vec!["a", "b", "c"]);
        assert!(!e.modified());
        e.redo();
        assert_eq!(e.lines, vec!["# a", "# b", "# c"]);
        assert!(e.dirty_from.is_some());
    }

    #[test]
    fn typing_undoes_as_one_step_and_redoes() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "hello");
        e.apply(Edit::Insert("\n".into()), &mut None::<String>);
        typed(&mut e, "world");
        e.undo();
        assert_eq!(e.lines, vec!["hello", ""]);
        e.undo();
        assert_eq!(e.lines, vec!["hello"], "the newline is its own step");
        e.undo();
        assert_eq!(e.lines, vec![""]);
        e.undo();
        assert_eq!(e.lines, vec![""], "nothing left to undo");
        e.redo();
        e.redo();
        assert_eq!(e.lines, vec!["hello", ""]);
        assert_eq!(e.cursor, (1, 0));
    }

    #[test]
    fn moving_the_cursor_ends_a_typing_run() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "ab");
        e.apply(Edit::Move { to: Motion::Left, select: false }, &mut None::<String>);
        typed(&mut e, "x");
        e.undo();
        assert_eq!(e.lines, vec!["ab"]);
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "a");
        e.undo();
        typed(&mut e, "b");
        e.redo();
        assert_eq!(e.lines, vec!["b"]);
    }

    #[test]
    fn a_reload_takes_the_file_as_one_undo_step_and_keeps_the_cursor_in_the_text() {
        let tmp = crate::plugin::testing::tmpdir("buffer-reload");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "one\ntwo three\n").unwrap();
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        e.cursor = (1, 9);
        e.anchor = Some((1, 4));
        assert!(!e.reload().unwrap(), "the same text is no change");
        std::fs::write(&path, "uno\n").unwrap();
        assert!(e.reload().unwrap());
        assert_eq!(e.lines, ["uno"]);
        assert_eq!((e.cursor, e.anchor), ((0, 3), None));
        assert!(!e.modified(), "it is what's on disk");
        e.undo();
        assert_eq!(e.lines, ["one", "two three"]);
        assert!(e.modified(), "the old text is now an unsaved edit");
        e.redo();
        assert!(!e.modified());
    }

    #[test]
    fn undoing_back_to_the_save_clears_modified() {
        let path = std::env::temp_dir().join(format!("ghostrealm-undo-{}.txt", std::process::id()));
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        typed(&mut e, "saved");
        e.save().unwrap();
        typed(&mut e, "!");
        assert!(e.modified());
        e.undo();
        assert!(!e.modified(), "back to what's on disk");
        e.undo();
        assert!(e.modified());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_no_op_edit_leaves_no_undo_step() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "a");
        e.apply(Edit::Move { to: Motion::DocStart, select: false }, &mut None::<String>);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        e.undo();
        assert_eq!(e.lines, vec![""], "the undo removed the typing, not the no-op");
    }

    #[test]
    fn deleting_past_the_end_of_a_line_changes_nothing() {
        let mut e = EditorBuffer::scratch();
        e.insert_str("ab");
        e.mark_saved();
        e.cursor = (0, 5);
        let edits = e.edits;
        assert_eq!(e.apply(Edit::Delete(Motion::Left), &mut None::<String>), Applied::Moved);
        assert_eq!(e.edits, edits);
        assert!(!e.modified());
        assert_eq!(e.lines, vec!["ab"]);
        assert_eq!(e.cursor, (0, 2), "back on the line");
    }

    #[test]
    fn a_no_op_edit_keeps_redo() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "a");
        e.undo();
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        e.apply(Edit::Cut, &mut None::<String>);
        e.redo();
        assert_eq!(e.lines, vec!["a"]);
    }

    #[test]
    fn delete_forward_joins_next_line() {
        let mut e = EditorBuffer::scratch();
        for c in "ab".chars() {
            e.insert_char(c);
        }
        e.insert_newline();
        e.insert_char('c');
        e.cursor = (0, 2); // end of first line
        e.delete_forward();
        assert_eq!(e.lines, vec!["abc"]);
    }

    #[test]
    fn unicode_columns() {
        let mut e = EditorBuffer::scratch();
        for c in "áé".chars() {
            e.insert_char(c);
        }
        e.move_cursor(Motion::Left);
        e.insert_char('x'); // between the two accented chars
        assert_eq!(e.lines, vec!["áxé"]);
        e.backspace();
        assert_eq!(e.lines, vec!["áé"]);
    }

    #[test]
    fn follow_cursor_tracks_and_only_on_move() {
        let mut e = EditorBuffer::scratch();
        for _ in 0..30 {
            e.insert_newline();
        }
        let unit = |_: &EditorBuffer, _| (0.0, 1.0);
        // cursor at row 30; a 10-row window scrolls to keep it visible.
        e.follow_cursor(10.0, unit, None);
        assert_eq!(e.scroll, 21, "the cursor on the last row");
        // Manual scroll away from the cursor is NOT yanked back (cursor unchanged).
        e.scroll = 5;
        e.follow_cursor(10.0, unit, None);
        assert_eq!(e.scroll, 5, "manual scroll sticks when the cursor hasn't moved");
        // Moving the cursor makes the view follow again.
        e.cursor.0 = 0;
        e.follow_cursor(10.0, unit, None);
        assert_eq!(e.scroll, 0, "moving to the top scrolls back up");
        // Horizontal follow when soft-wrap is off (hcols = Some).
        e.cursor = (0, 200);
        e.follow_cursor(10.0, unit, Some(20));
        assert!(e.hscroll <= 200 && e.hscroll + 20 > 200);
    }

    #[test]
    fn follow_cursor_fits_lines_by_their_heights() {
        let mut e = EditorBuffer::scratch();
        for _ in 0..30 {
            e.insert_newline();
        }
        // Every third line is three rows tall.
        let tall = |_: &EditorBuffer, ln: usize| (0.0, if ln.is_multiple_of(3) { 3.0 } else { 1.0 });
        e.follow_cursor(10.0, tall, None);
        // Lines 24..=30 are 3+1+1+3+1+1+3 = 13 tall; 25..=30 are 10.
        assert_eq!(e.scroll, 25);
        // A line taller than the body shows from its top.
        e.cursor.0 = 27;
        e.follow_cursor(2.0, tall, None);
        assert_eq!(e.scroll, 27);
        // A move within what fits leaves the scroll.
        e.cursor.0 = 28;
        e.follow_cursor(10.0, tall, None);
        assert_eq!(e.scroll, 27);
    }

    #[test]
    fn follow_cursor_counts_a_lines_space_only_below_the_top() {
        let mut e = EditorBuffer::scratch();
        for _ in 0..30 {
            e.insert_newline();
        }
        // Line 25 has 2 of space above its row; every row is 1 tall.
        let spaced = |_: &EditorBuffer, ln: usize| (if ln == 25 { 2.0 } else { 0.0 }, 1.0);
        // On top, 25 shows no space: 25..=30 fit 6.
        e.follow_cursor(6.0, spaced, None);
        assert_eq!(e.scroll, 25);
        // Below 24 it does: 24..=29 would take 8.
        e.cursor.0 = 29;
        e.scroll = 0;
        e.follow_cursor(7.0, spaced, None);
        assert_eq!(e.scroll, 25);
        e.cursor.0 = 28;
        e.scroll = 0;
        e.follow_cursor(7.0, spaced, None);
        assert_eq!(e.scroll, 24, "24..=28 take 7");
    }

    #[test]
    fn open_missing_file_starts_empty_with_a_name() {
        let e = EditorBuffer::open(PathBuf::from("/no/such/file-xyz.txt")).unwrap();
        assert_eq!(e.lines, vec![String::new()]);
        assert_eq!(e.title(), "file-xyz.txt");
    }

    #[test]
    fn open_refuses_a_file_it_cannot_read_as_text() {
        let path = std::env::temp_dir().join(format!("ghostrealm-latin1-{}.txt", std::process::id()));
        std::fs::write(&path, b"caf\xe9\n").unwrap();
        let err = EditorBuffer::open(path.clone()).err().expect("not UTF-8: refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&path).unwrap(), b"caf\xe9\n", "untouched");
        let _ = std::fs::remove_file(path);
        assert!(EditorBuffer::open(std::env::temp_dir()).is_err(), "a directory is refused");
    }

    #[test]
    fn save_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ghostrealm-edit-{}.txt", std::process::id()));
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        for c in "hello".chars() {
            e.insert_char(c);
        }
        e.insert_newline();
        for c in "world".chars() {
            e.insert_char(c);
        }
        assert!(e.save().unwrap(), "a path-backed buffer saves");
        assert!(!e.modified());
        let back = EditorBuffer::open(path.clone()).unwrap();
        assert_eq!(back.lines, vec!["hello".to_string(), "world".to_string()]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn scratch_save_is_a_noop() {
        let mut e = EditorBuffer::scratch();
        e.insert_char('x');
        assert!(!e.save().unwrap(), "a scratch buffer has nowhere to save");
    }

    #[test]
    fn devices_folders_and_huge_files_are_not_read() {
        let d = crate::plugin::testing::tmpdir("buf-irregular");
        #[cfg(unix)]
        assert!(EditorBuffer::open(PathBuf::from("/dev/zero")).is_err(), "a device never ends");
        assert!(EditorBuffer::open(d.to_path_buf()).is_err(), "a folder");
        let big = d.join("big.txt");
        std::fs::File::create(&big).unwrap().set_len(MAX_FILE_BYTES + 1).unwrap();
        let err = EditorBuffer::open(big).err().expect("over the cap");
        assert!(err.to_string().contains("over 50 MB"), "{err}");
        assert!(EditorBuffer::open(d.join("new.txt")).is_ok(), "a missing file is a new one");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let tmp = crate::plugin::testing::tmpdir("buf-fifo");
        let fifo = tmp.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(EditorBuffer::open(fifo).is_err());
    }

    /// Let the open run's last edit be a pause ago.
    fn pause(e: &EditorBuffer) {
        let mut d = e.doc.borrow_mut();
        if let Some(Open { run: Some((_, _, when)), .. }) = &mut d.open {
            *when -= RUN_PAUSE;
        }
    }

    #[test]
    fn a_typing_run_breaks_where_a_word_starts_and_after_a_pause() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "ab, cd");
        e.undo();
        assert_eq!(e.lines, ["ab, "], "a word after punctuation and space starts a step");
        e.undo();
        assert_eq!(e.lines, [""]);
        typed(&mut e, "ab");
        pause(&e);
        typed(&mut e, "c");
        e.undo();
        assert_eq!(e.lines, ["ab"], "the pause ended the run");
        // Deletes merge, and a pause ends them too.
        typed(&mut e, "cdef");
        pause(&e);
        let back = |e: &mut EditorBuffer| e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        back(&mut e);
        back(&mut e);
        pause(&e);
        back(&mut e);
        assert_eq!(e.lines, ["abc"]);
        e.undo();
        assert_eq!(e.lines, ["abcd"]);
        e.undo();
        assert_eq!(e.lines, ["abcdef"]);
        // Typing over a selection starts a step that typing on continues.
        let mut e = buffer_with("abcd");
        select(&mut e, (0, 1), (0, 3));
        typed(&mut e, "xy");
        e.undo();
        assert_eq!((e.lines.clone(), e.selection()), (vec!["abcd".to_string()], Some(((0, 1), (0, 3)))));
    }

    #[test]
    fn editing_after_an_undo_adds_a_branch_and_keeps_the_old_one() {
        let mut e = EditorBuffer::scratch();
        typed(&mut e, "one");
        let one = e.history().current();
        e.apply(Edit::Insert("\n".into()), &mut None::<String>);
        typed(&mut e, "two");
        let two = e.history().current();
        e.undo();
        e.undo();
        typed(&mut e, "!");
        let bang = e.history().current();
        assert_eq!(e.lines, ["one!"]);
        assert_eq!(e.history().node(one).unwrap().children().len(), 2, "both branches stay");
        // Undo and redo walk the branch last visited.
        e.undo();
        e.redo();
        assert_eq!(e.lines, ["one!"]);
        // Switching back to the old branch, with the caret as its step left it.
        assert!(e.goto(two));
        assert_eq!((e.lines.clone(), e.cursor), (vec!["one".to_string(), "two".to_string()], (1, 3)));
        e.undo();
        e.undo();
        e.redo();
        e.redo();
        assert_eq!(e.lines, ["one", "two"], "redo now follows the old branch");
        assert!(e.goto(bang));
        assert_eq!(e.lines, ["one!"]);
        let root = e.history().root();
        assert!(e.goto(root));
        assert_eq!(e.lines, [""]);
    }

    #[test]
    fn a_paste_is_one_change_and_steps_hold_changes_not_copies() {
        let mut e = buffer_with(&"x\n".repeat(10_000));
        let bytes = e.history().bytes();
        let mut clip = Some("a\r\nb".to_string());
        e.apply(Edit::Paste, &mut clip);
        assert_eq!((&e.lines[..2], e.cursor), (&["a".to_string(), "bx".to_string()][..], (1, 1)));
        let h = e.history();
        assert_eq!(h.node(h.current()).unwrap().ops().len(), 1);
        assert!(h.bytes() - bytes < 1_000, "the step holds the paste, not the file: {}", h.bytes() - bytes);
    }

    #[test]
    fn views_of_one_file_share_its_text_and_history_until_the_last_closes() {
        let tmp = crate::plugin::testing::tmpdir("buffer-shared");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "a\nb\n").unwrap();
        let mut one = EditorBuffer::open(path.clone()).unwrap();
        let mut two = EditorBuffer::open(path.clone()).unwrap();
        two.cursor = (1, 1);
        one.cursor = (0, 1);
        typed(&mut one, "x");
        assert!(two.modified(), "the history is the file's");
        assert!(two.sync());
        assert_eq!((two.lines.clone(), two.cursor), (vec!["ax".to_string(), "b".to_string()], (1, 1)));
        one.cursor = (0, 0);
        one.apply(Edit::Insert("\n".into()), &mut None::<String>);
        two.sync();
        assert_eq!(two.cursor, (2, 1), "the caret moves with the text");
        // Undo in one view undoes the other's edit.
        two.undo();
        two.undo();
        assert_eq!(two.lines, ["a", "b"]);
        assert!(!two.modified());
        one.sync();
        assert_eq!(one.lines, ["a", "b"]);
        // A third view opens on the shared text and history.
        one.redo();
        let three = EditorBuffer::open(path.clone()).unwrap();
        assert_eq!(three.lines, ["ax", "b"]);
        drop((one, two, three));
        let fresh = EditorBuffer::open(path).unwrap();
        assert_eq!(
            (fresh.lines.clone(), fresh.history().steps()),
            (vec!["a".to_string(), "b".to_string()], 1),
            "closed: history gone"
        );
    }

    #[test]
    fn history_survives_save_as_and_moves_every_view() {
        let tmp = crate::plugin::testing::tmpdir("buffer-save-as");
        let (old, new) = (tmp.join("old.txt"), tmp.join("new.txt"));
        std::fs::write(&old, "a\n").unwrap();
        let mut e = EditorBuffer::open(old.clone()).unwrap();
        let mut other = EditorBuffer::open(old).unwrap();
        e.cursor = (0, 1);
        typed(&mut e, "b");
        e.save_as(new.clone()).unwrap();
        assert!(!e.modified());
        other.sync();
        assert_eq!(other.path.as_deref(), Some(new.as_path()), "the file moved for every view");
        assert_eq!(EditorBuffer::open(new.clone()).unwrap().history().steps(), e.history().steps());
        e.undo();
        assert_eq!(e.lines, ["a"]);
        assert!(e.modified(), "the new file holds the edit");
        e.redo();
        assert!(!e.modified());
    }

    #[test]
    fn a_reload_keeps_the_history_and_changes_only_the_lines_that_differ() {
        let tmp = crate::plugin::testing::tmpdir("buffer-reload-history");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "head\nmid\ntail\n").unwrap();
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        e.cursor = (2, 4);
        typed(&mut e, "!");
        e.save().unwrap();
        std::fs::write(&path, "head\nMID\nmore\ntail!\n").unwrap();
        assert!(e.reload().unwrap());
        assert_eq!(e.lines, ["head", "MID", "more", "tail!"]);
        assert_eq!(e.cursor, (3, 5), "the caret stays on its line");
        {
            let h = e.history();
            let op = &h.node(h.current()).unwrap().ops()[0];
            assert_eq!((op.del.as_str(), op.ins.as_str()), ("\nmid", "\nMID\nmore"));
        }
        assert!(!e.modified() && !e.conflicted());
        e.undo();
        e.undo();
        assert_eq!(e.lines, ["head", "mid", "tail"], "steps before the reload stay");
        assert!(e.modified() && e.conflicted(), "older text asks before it overwrites the file");
    }

    #[test]
    fn an_edit_after_a_reload_that_changed_nothing_is_unsaved() {
        let tmp = crate::plugin::testing::tmpdir("buffer-reload-same");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        e.cursor = (0, 1);
        typed(&mut e, "b");
        std::fs::write(&path, "ab\n").unwrap();
        assert!(!e.reload().unwrap());
        assert!(!e.modified());
        // Within the run's pause: still a step of its own.
        typed(&mut e, "c");
        assert!(e.modified(), "the edit differs from the file");
        e.undo();
        assert_eq!(e.lines, ["ab"]);
        assert!(!e.modified());
    }

    /// Sync both views and check each shows the file's text.
    fn in_step(a: &mut EditorBuffer, b: &mut EditorBuffer) {
        a.sync();
        b.sync();
        let text = a.doc.borrow().text.clone();
        assert_eq!((&a.lines, &b.lines), (&text, &text));
    }

    #[test]
    fn a_view_behind_by_part_of_a_step_catches_up_across_branches() {
        let mut a = EditorBuffer::scratch();
        let mut b = EditorBuffer::with_doc(a.doc.clone());
        typed(&mut a, "ab");
        b.sync();
        let s = a.history().current();
        assert_eq!(b.at, (s, 2));
        typed(&mut a, "cd");
        assert_eq!(a.history().tip(), (s, 4), "one run");
        // A sibling of the step `b` has in part.
        a.undo();
        typed(&mut a, "X");
        let t = a.history().current();
        assert_ne!(t, s);
        let route = a.history().route(b.at).unwrap();
        assert_eq!(route.len(), 3, "back out the two changes `b` has, then the sibling's one");
        in_step(&mut a, &mut b);
        assert_eq!(b.lines, ["X"]);
        // A child of a step `b` has in part.
        assert!(a.goto(s));
        typed(&mut a, "gh");
        b.sync();
        let u = a.history().current();
        assert_eq!(b.at, (u, 2));
        typed(&mut a, "ij");
        pause(&a);
        typed(&mut a, "k");
        assert_eq!(a.history().node(a.history().current()).unwrap().parent(), Some(u));
        in_step(&mut a, &mut b);
        assert_eq!(b.lines, ["abcdghijk"]);
        // Each undoes and edits on the other's branches.
        b.undo();
        in_step(&mut a, &mut b);
        assert!(a.goto(t));
        in_step(&mut a, &mut b);
        b.cursor = (0, 1);
        typed(&mut b, "Y");
        in_step(&mut a, &mut b);
        assert_eq!(a.lines, ["XY"]);
        a.undo();
        a.undo();
        in_step(&mut a, &mut b);
        assert_eq!(b.lines, [""]);
        b.redo();
        in_step(&mut a, &mut b);
        assert_eq!(a.lines, ["X"]);
        assert!(b.goto(u));
        a.sync();
        a.cursor = (0, 8);
        typed(&mut a, "!");
        in_step(&mut a, &mut b);
        assert_eq!(b.lines, ["abcdghij!"]);
    }

    #[test]
    fn a_view_whose_step_was_pruned_takes_the_text_whole() {
        let mut a = EditorBuffer::scratch();
        let mut b = EditorBuffer::with_doc(a.doc.clone());
        typed(&mut a, "one");
        b.sync();
        let behind = b.at;
        a.apply(Edit::Insert("\n".into()), &mut None::<String>);
        typed(&mut a, "two");
        a.doc.borrow_mut().history.prune(0);
        assert!(a.history().node(behind.0).is_none() && a.history().route(behind).is_none(), "pruned");
        assert!(b.sync());
        assert_eq!(b.lines, ["one", "two"]);
        typed(&mut b, "!");
        in_step(&mut a, &mut b);
        a.undo();
        in_step(&mut a, &mut b);
        assert_eq!(b.lines, ["one", "two"]);
    }

    #[test]
    fn a_line_diff_splices_only_the_rows_between_common_ends() {
        let l = |s: &str| s.split('\n').map(str::to_string).collect::<Vec<_>>();
        for (old, new) in [
            ("a\nb\nc", "a\nc"),
            ("a\nc", "a\nb\nc"),
            ("a\nb", "x\nb"),
            ("a", "a\nb"),
            ("a\nb", "a"),
            ("a\nb", "b"),
            ("b", "a\nb"),
            ("a", "b"),
            ("a\nb\nc", "x\ny"),
        ] {
            let mut text = l(old);
            let (from, to, ins) = line_diff(&text, &l(new));
            history::splice(&mut text, from, to, &ins);
            assert_eq!(text, l(new), "{old:?} -> {new:?}");
        }
    }
}
