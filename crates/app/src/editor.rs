//! A minimal editable text buffer — the non-terminal surface content.
//!
//! Scope is deliberately small: open a file (or a scratch buffer), edit it, save
//! it. Columns are counted in Unicode scalar values (not grapheme clusters),
//! which is enough for code and plain text. The window renders `lines` through the
//! same glyphon text path the terminal uses, and routes key input here when the
//! focused surface is an editor.

use std::path::PathBuf;

/// Cursor / edit motions the window maps keys onto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

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
    /// Selection anchor (row, col); `None` = no selection. The selection spans
    /// anchor..cursor.
    pub anchor: Option<(usize, usize)>,
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
            anchor: None,
        }
    }

    /// Load `path` into a buffer. A missing file starts empty (a new file).
    pub fn open(path: PathBuf) -> Self {
        let lines = match std::fs::read_to_string(&path) {
            Ok(text) => split_lines(&text),
            Err(_) => vec![String::new()],
        };
        EditorBuffer {
            lines,
            cursor: (0, 0),
            scroll: 0,
            hscroll: 0,
            last_cursor: (0, 0),
            path: Some(path),
            modified: false,
            anchor: None,
        }
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

    /// Delete the selected text, placing the cursor at its start. Returns whether
    /// anything was deleted.
    pub fn delete_selection(&mut self) -> bool {
        let Some(((sr, sc), (er, ec))) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let start = self.byte_of(sr, sc);
        let end = self.byte_of(er, ec);
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
        self.modified = true;
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
        self.modified = true;
    }

    pub fn insert_newline(&mut self) {
        self.delete_selection();
        let (row, col) = self.cursor;
        let b = self.byte_of(row, col);
        let tail = self.lines[row].split_off(b);
        self.lines.insert(row + 1, tail);
        self.cursor = (row + 1, 0);
        self.modified = true;
    }

    /// Delete the char before the cursor, joining with the previous line at the
    /// start of a line. Deletes the selection instead when there is one.
    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        if col > 0 {
            let b = self.byte_of(row, col - 1);
            self.lines[row].remove(b);
            self.cursor.1 = col - 1;
            self.modified = true;
        } else if row > 0 {
            let prev_len = self.line_len(row - 1);
            let cur = self.lines.remove(row);
            self.lines[row - 1].push_str(&cur);
            self.cursor = (row - 1, prev_len);
            self.modified = true;
        }
    }

    /// Delete the char at the cursor, joining the next line at end of line.
    /// Deletes the selection instead when there is one.
    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let (row, col) = self.cursor;
        if col < self.line_len(row) {
            let b = self.byte_of(row, col);
            self.lines[row].remove(b);
            self.modified = true;
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
            self.modified = true;
        }
    }

    /// Move the cursor, clearing any selection (a plain, non-selecting move).
    pub fn move_cursor(&mut self, motion: Motion) {
        self.anchor = None;
        self.move_cursor_raw(motion);
    }

    /// Move the cursor while extending a selection (Shift+motion): anchors at the
    /// current cursor if there's no selection yet.
    pub fn move_cursor_selecting(&mut self, motion: Motion) {
        if self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        }
        self.move_cursor_raw(motion);
    }

    fn move_cursor_raw(&mut self, motion: Motion) {
        let (row, col) = self.cursor;
        self.cursor = match motion {
            Motion::Left => {
                if col > 0 {
                    (row, col - 1)
                } else if row > 0 {
                    (row - 1, self.line_len(row - 1))
                } else {
                    (0, 0)
                }
            }
            Motion::Right => {
                if col < self.line_len(row) {
                    (row, col + 1)
                } else if row + 1 < self.lines.len() {
                    (row + 1, 0)
                } else {
                    (row, col)
                }
            }
            Motion::Up if row > 0 => (row - 1, col.min(self.line_len(row - 1))),
            Motion::Down if row + 1 < self.lines.len() => {
                (row + 1, col.min(self.line_len(row + 1)))
            }
            Motion::Up | Motion::Down => (row, col),
            Motion::Home => (row, 0),
            Motion::End => (row, self.line_len(row)),
        };
    }

    /// Move to the document start/end, optionally extending the selection.
    pub fn move_document(&mut self, to_end: bool, selecting: bool) {
        if selecting {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = if to_end {
            let r = self.lines.len().saturating_sub(1);
            (r, self.line_len(r))
        } else {
            (0, 0)
        };
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
        let mut text = self.lines.join("\n");
        text.push('\n');
        std::fs::write(&path, text)?;
        self.modified = false;
        Ok(true)
    }
}

/// Split file text into lines, dropping a single trailing newline so a normal
/// file doesn't gain a phantom empty last line.
fn split_lines(text: &str) -> Vec<String> {
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
        e.move_cursor(Motion::Home);
        e.backspace();
        assert_eq!(e.lines, vec!["abcd"]);
        assert_eq!(e.cursor, (0, 2));
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
        let e = EditorBuffer::open(PathBuf::from("/no/such/file-xyz.txt"));
        assert_eq!(e.lines, vec![String::new()]);
        assert_eq!(e.title(), "file-xyz.txt");
    }

    #[test]
    fn save_round_trips() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ghostrealm-edit-{}.txt", std::process::id()));
        let mut e = EditorBuffer::open(path.clone());
        for c in "hello".chars() {
            e.insert_char(c);
        }
        e.insert_newline();
        for c in "world".chars() {
            e.insert_char(c);
        }
        assert!(e.save().unwrap(), "a path-backed buffer saves");
        assert!(!e.modified);
        let back = EditorBuffer::open(path.clone());
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
