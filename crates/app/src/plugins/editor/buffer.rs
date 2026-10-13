//! A minimal editable text buffer — the non-terminal surface content.
//!
//! Scope is deliberately small: open a file (or a scratch buffer), edit it, save
//! it. Columns are counted in Unicode scalar values (not grapheme clusters),
//! which is enough for code and plain text. The window renders `lines` through the
//! same glyphon text path the terminal uses, and routes key input here when the
//! focused surface is an editor.

use std::path::PathBuf;

use crate::plugin::text_edit::{word_left, word_right, Applied, Clipboard, Edit, Motion};

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
    /// Backing file, if any (a scratch buffer has none).
    pub path: Option<PathBuf>,
    /// Unsaved edits since the last load/save.
    pub modified: bool,
    /// Monotonic count of text-changing edits, for the idle-autosave timer.
    pub edits: u64,
    /// The first line changed since a consumer last took this (`None` = none).
    /// Lets a per-line cache re-check only from there on.
    pub dirty_from: Option<usize>,
    /// Selection anchor (row, col); `None` = no selection. The selection spans
    /// anchor..cursor.
    pub anchor: Option<(usize, usize)>,
    history: History,
}

/// Most undo steps kept; older ones are dropped.
const MAX_UNDO: usize = 200;

/// Undo granularity: a run of typed characters, or of single-character
/// deletes, is one step while each edit continues where the last left off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Typing,
    Deleting,
    Other,
}

/// The buffer's text, cursor, and selection at one point, and which version
/// of the text it is.
#[derive(Clone, Debug)]
struct Snapshot {
    lines: Vec<String>,
    cursor: (usize, usize),
    anchor: Option<(usize, usize)>,
    version: u64,
}

/// Undo/redo as whole-buffer snapshots.
#[derive(Debug, Default)]
struct History {
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The open run of typing/deleting and where its last edit left the cursor.
    run: Option<(Step, (usize, usize))>,
    /// The current text's version (each undo step gets a fresh one) and the
    /// version last written, so undoing back to it clears `modified`.
    version: u64,
    next_version: u64,
    saved_version: u64,
}

impl History {
    /// An edit changed the text: what was undone can't be redone, and the
    /// oldest steps past [`MAX_UNDO`] go.
    fn committed(&mut self) {
        self.redo.clear();
        if self.undo.len() > MAX_UNDO {
            let excess = self.undo.len() - MAX_UNDO;
            self.undo.drain(..excess);
        }
    }
}

impl EditorBuffer {
    /// An empty scratch buffer.
    pub fn scratch() -> Self {
        EditorBuffer {
            lines: vec![String::new()],
            cursor: (0, 0),
            scroll: 0,
            hscroll: 0,
            last_cursor: (0, 0),
            path: None,
            modified: false,
            edits: 0,
            dirty_from: None,
            anchor: None,
            history: History::default(),
        }
    }

    /// Load `path` into a buffer. A missing file starts empty (a new file).
    /// Any other read error (not UTF-8, unreadable, a directory) fails: an
    /// empty buffer bound to the file would overwrite it on save.
    pub fn open(path: PathBuf) -> std::io::Result<Self> {
        let lines = match std::fs::read_to_string(&path) {
            Ok(text) => split_lines(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![String::new()],
            Err(e) => return Err(e),
        };
        Ok(EditorBuffer {
            lines,
            cursor: (0, 0),
            scroll: 0,
            hscroll: 0,
            last_cursor: (0, 0),
            path: Some(path),
            modified: false,
            edits: 0,
            dirty_from: None,
            anchor: None,
            history: History::default(),
        })
    }

    /// A short display name for the tab (file name, or "*scratch*").
    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "*scratch*".to_string())
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

    /// Mark a text-changing edit that starts at line `row`: sets the unsaved
    /// flag and bumps the edit count (which the idle-autosave timer watches).
    fn touch(&mut self, row: usize) {
        self.dirty_from = Some(self.dirty_from.map_or(row, |d| d.min(row)));
        self.modified = true;
        self.edits = self.edits.wrapping_add(1);
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
            self.lines[sr].replace_range(start..end, "");
        } else {
            let tail = self.lines[er][end..].to_string();
            self.lines[sr].truncate(start);
            self.lines[sr].push_str(&tail);
            self.lines.drain(sr + 1..=er);
        }
        self.cursor = (sr, sc);
        self.anchor = None;
        self.touch(sr);
        true
    }

    /// Insert text (may contain newlines), replacing any selection first.
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        for c in s.chars() {
            if c == '\n' {
                self.insert_newline();
            } else if c != '\r' {
                self.insert_char(c);
            }
        }
    }

    pub fn insert_char(&mut self, c: char) {
        self.delete_selection();
        let (row, col) = self.cursor;
        let b = self.byte_of(row, col);
        self.lines[row].insert(b, c);
        self.cursor.1 = col + 1;
        self.touch(row);
    }

    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let (row, col) = self.cursor;
        let b = self.byte_of(row, col);
        let tail = self.lines[row].split_off(b);
        self.lines.insert(row + 1, tail);
        self.cursor = (row + 1, 0);
        self.touch(row);
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
    /// the text are recorded for undo.
    pub fn apply(&mut self, edit: Edit, clip: &mut dyn Clipboard) -> Applied {
        let edits = self.edits;
        let step = match &edit {
            Edit::Insert(s) if s.chars().count() == 1 && s != "\n" => Some(Step::Typing),
            Edit::Delete(Motion::Left | Motion::Right) => Some(Step::Deleting),
            Edit::Insert(_) | Edit::Delete(_) | Edit::Cut | Edit::Paste => Some(Step::Other),
            Edit::Move { .. } | Edit::SelectAll | Edit::Copy | Edit::Undo | Edit::Redo => None,
        };
        let opened = step.is_some_and(|step| self.checkpoint(step));
        match edit {
            Edit::Move { to, select: true } => self.move_cursor_selecting(to),
            Edit::Move { to, select: false } => self.move_cursor(to),
            Edit::Delete(to) => self.delete(to),
            Edit::Insert(s) => self.insert_str(&s),
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
        if step.is_some() && self.edits != edits {
            self.history.committed();
        }
        match step {
            // A step that changed nothing leaves no undo entry.
            Some(_) if opened && self.edits == edits => self.drop_checkpoint(),
            Some(Step::Typing | Step::Deleting) => self.history.run = step.map(|s| (s, self.cursor)),
            _ => self.history.run = None,
        }
        if self.edits != edits {
            Applied::Changed
        } else {
            Applied::Moved
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
            version: self.history.version,
        }
    }

    /// Record the state before a `step` edit, unless it continues the open run.
    /// Returns whether it opened a new undo step.
    fn checkpoint(&mut self, step: Step) -> bool {
        let h = &self.history;
        if step != Step::Other && h.run == Some((step, self.cursor)) {
            return false;
        }
        let snap = self.snapshot();
        let h = &mut self.history;
        h.undo.push(snap);
        h.next_version += 1;
        h.version = h.next_version;
        true
    }

    /// End the open typing/deleting run, so the next edit starts a new undo
    /// step (e.g. after the cursor moved by mouse).
    pub fn end_undo_run(&mut self) {
        self.history.run = None;
    }

    /// Withdraw the step [`Self::checkpoint`] just opened.
    fn drop_checkpoint(&mut self) {
        if let Some(s) = self.history.undo.pop() {
            self.history.version = s.version;
        }
    }

    /// Make `s` the buffer's state; it is unmodified if it is the saved version.
    fn restore(&mut self, s: Snapshot) {
        self.lines = s.lines;
        self.cursor = s.cursor;
        self.anchor = s.anchor;
        self.history.version = s.version;
        self.history.run = None;
        self.edits = self.edits.wrapping_add(1);
        self.dirty_from = Some(0);
        self.modified = s.version != self.history.saved_version;
    }

    /// Step back to the state before the last undo step.
    pub fn undo(&mut self) {
        if let Some(s) = self.history.undo.pop() {
            let cur = self.snapshot();
            self.history.redo.push(cur);
            self.restore(s);
        }
    }

    /// Re-apply the last undone step.
    pub fn redo(&mut self) {
        if let Some(s) = self.history.redo.pop() {
            let cur = self.snapshot();
            self.history.undo.push(cur);
            self.restore(s);
        }
    }

    /// Scroll to keep the cursor visible — vertically within `rows` lines, and
    /// (when `hcols` is `Some`, i.e. soft-wrap off) horizontally within `hcols`
    /// columns — but only when the cursor actually moved since the last follow, so
    /// manual scrolling (which leaves the cursor put) isn't yanked back each frame.
    pub fn follow_cursor(&mut self, rows: usize, hcols: Option<usize>) {
        if self.cursor == self.last_cursor {
            return;
        }
        let rows = rows.max(1);
        if self.cursor.0 < self.scroll {
            self.scroll = self.cursor.0;
        } else if self.cursor.0 >= self.scroll + rows {
            self.scroll = self.cursor.0 + 1 - rows;
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

    /// Write the buffer to its path. Returns whether it had a path to save to.
    pub fn save(&mut self) -> std::io::Result<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        self.write_to(&path)?;
        Ok(true)
    }

    /// Write the buffer to `path` and make that its file. On failure the
    /// buffer keeps its old path, so later saves don't aim at the failed one.
    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        self.write_to(&path)?;
        self.path = Some(path);
        Ok(())
    }

    fn write_to(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        let mut text = self.lines.join("\n");
        text.push('\n');
        std::fs::write(path, text)?;
        self.modified = false;
        // Typing after a save starts a new step, so undo can return here.
        self.history.saved_version = self.history.version;
        self.history.run = None;
        Ok(())
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
        assert_eq!(e.dirty_from.take(), Some(0), "a restore may change any line");
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
        assert!(e.modified);
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
    fn undoing_back_to_the_save_clears_modified() {
        let path = std::env::temp_dir().join(format!("ghostrealm-undo-{}.txt", std::process::id()));
        let mut e = EditorBuffer::open(path.clone()).unwrap();
        typed(&mut e, "saved");
        e.save().unwrap();
        typed(&mut e, "!");
        assert!(e.modified);
        e.undo();
        assert!(!e.modified, "back to what's on disk");
        e.undo();
        assert!(e.modified);
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
        e.modified = false;
        e.cursor = (0, 5);
        let edits = e.edits;
        assert_eq!(e.apply(Edit::Delete(Motion::Left), &mut None::<String>), Applied::Moved);
        assert_eq!(e.edits, edits);
        assert!(!e.modified);
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
    fn undo_keeps_at_most_max_undo_steps() {
        let mut e = EditorBuffer::scratch();
        for _ in 0..MAX_UNDO + 5 {
            e.apply(Edit::Insert("ab".into()), &mut None::<String>);
        }
        assert_eq!(e.history.undo.len(), MAX_UNDO);
        e.undo();
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        e.apply(Edit::Move { to: Motion::DocStart, select: false }, &mut None::<String>);
        e.apply(Edit::Delete(Motion::Left), &mut None::<String>);
        assert_eq!(e.history.undo.len(), MAX_UNDO, "a no-op evicts nothing");
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
        // cursor at row 30; a 10-row window scrolls to keep it visible.
        e.follow_cursor(10, None);
        assert!(e.scroll <= 30 && e.scroll + 10 > 30);
        // Manual scroll away from the cursor is NOT yanked back (cursor unchanged).
        e.scroll = 5;
        e.follow_cursor(10, None);
        assert_eq!(e.scroll, 5, "manual scroll sticks when the cursor hasn't moved");
        // Moving the cursor makes the view follow again.
        e.cursor.0 = 0;
        e.follow_cursor(10, None);
        assert_eq!(e.scroll, 0, "moving to the top scrolls back up");
        // Horizontal follow when soft-wrap is off (hcols = Some).
        e.cursor = (0, 200);
        e.follow_cursor(10, Some(20));
        assert!(e.hscroll <= 200 && e.hscroll + 20 > 200);
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
        assert!(!e.modified);
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
}
