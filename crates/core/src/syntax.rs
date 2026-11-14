//! A document's syntax tree, kept in step with its text as it is edited: the
//! interface a parser library sits behind (`ghostrealm-syntax-tree-sitter`
//! implements it). Code languages are highlighted from it; the line-local
//! [`crate::highlight::Highlighter`]s stay for rich formats.
//!
//! Every tree sees the text through one byte model: the rows joined by `\n`,
//! none after the last ([`row_start`], [`chunk`], [`chunks`], [`text`]).
//! Points are (row, byte column).

use std::ops::Range;

use crate::highlight::Span;

/// A place in the text: (row, byte column).
pub type Point = (usize, usize);

/// Byte offset of row `row`'s start (`row` at most the row count).
pub fn row_start(lines: &[String], row: usize) -> usize {
    lines[..row].iter().map(|l| l.len() + 1).sum()
}

/// The text's length in bytes.
pub fn len(lines: &[String]) -> usize {
    row_start(lines, lines.len()).saturating_sub(1)
}

/// The text as one string, for a parse off the UI thread.
pub fn text(lines: &[String]) -> String {
    lines.join("\n")
}

/// The text from `at` on, as far as one slice reaches: the rest of its row,
/// else the `\n` after it; empty at the end.
pub fn chunk(lines: &[String], (row, col): Point) -> &[u8] {
    match lines.get(row) {
        Some(l) if col < l.len() => &l.as_bytes()[col..],
        Some(_) if row + 1 < lines.len() => b"\n",
        _ => b"",
    }
}

/// The text from `from` to `to`, in slices.
pub fn chunks<'a>(lines: &'a [String], from: Point, to: Point) -> impl Iterator<Item = &'a [u8]> + 'a {
    let last = to.0.min(lines.len().saturating_sub(1));
    (from.0..=last).filter(move |_| !lines.is_empty()).flat_map(move |row| {
        let l = lines[row].as_bytes();
        let start = if row == from.0 { from.1.min(l.len()) } else { 0 };
        let end = if row == to.0 { to.1.min(l.len()) } else { l.len() };
        let newline: &[u8] = if row < to.0 && row + 1 < lines.len() { b"\n" } else { b"" };
        [&l[start..end.max(start)], newline].into_iter().filter(|s| !s.is_empty())
    })
}

/// One change to the text, in bytes and points: what a tree needs to shift
/// its nodes before it reparses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub start: Point,
    pub old_end: Point,
    pub new_end: Point,
}

impl TextEdit {
    /// The edit that replaced `del` at `at` (row, char column) with `ins`;
    /// `lines` is the text after it. The text before `at` is the same before
    /// and after, so its offsets are read from `lines`.
    pub fn of(lines: &[String], at: (usize, usize), del: &str, ins: &str) -> TextEdit {
        let line = &lines[at.0];
        let col = line.char_indices().nth(at.1).map_or(line.len(), |(b, _)| b);
        let start_byte = row_start(lines, at.0) + col;
        let start = (at.0, col);
        TextEdit {
            start_byte,
            old_end_byte: start_byte + del.len(),
            new_end_byte: start_byte + ins.len(),
            start,
            old_end: end_point(start, del),
            new_end: end_point(start, ins),
        }
    }
}

/// Where `text` ends when it starts at `at`.
fn end_point((row, col): Point, text: &str) -> Point {
    match text.rfind('\n') {
        Some(i) => (row + text.matches('\n').count(), text.len() - i - 1),
        None => (row, col + text.len()),
    }
}

/// Most edits a [`TextEdits`] keeps; past it, it asks for a fresh parse,
/// which is then cheaper than replaying them.
pub const MAX_EDITS: usize = 256;

/// The edits a buffer made since its tree last took them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextEdits {
    /// Parse from scratch: the text was replaced whole, or too many edits.
    pub reset: bool,
    /// In the order made, each on the text the one before it left.
    pub edits: Vec<TextEdit>,
}

impl TextEdits {
    /// A log that asks for a fresh parse.
    pub fn reset() -> Self {
        TextEdits { reset: true, edits: Vec::new() }
    }

    /// Add `edit`; past [`MAX_EDITS`], ask for a fresh parse instead.
    pub fn push(&mut self, edit: TextEdit) {
        if self.reset {
            return;
        }
        if self.edits.len() >= MAX_EDITS {
            *self = TextEdits::reset();
        } else {
            self.edits.push(edit);
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.reset && self.edits.is_empty()
    }
}

/// One document's syntax tree, kept in step with its text, and the
/// highlight read from it.
pub trait SyntaxDoc {
    /// Its language's name; a document whose path now names another
    /// language takes a new one.
    fn name(&self) -> &'static str;

    /// Bring the tree up to `lines`, the text `edits` took it to from the
    /// text the last update saw (the first update, and a `reset`, parse from
    /// scratch). A small document parses here; a large one on a worker,
    /// meanwhile highlighting from the old tree with its nodes shifted by
    /// the edits. Returns whether the highlight may have changed.
    fn update(&mut self, edits: TextEdits, lines: &[String]) -> bool;

    /// Take the worker's tree, if it finished. Returns whether the
    /// highlight changed; the next [`Self::update`] starts another parse if
    /// the text moved on meanwhile.
    fn poll(&mut self) -> bool;

    /// Highlight `rows` of `lines` (the text of the last update), kept until
    /// the tree or the text changes.
    fn highlight(&mut self, lines: &[String], rows: Range<usize>);

    /// Line `ln`'s spans, contiguous over the line, as [`Self::highlight`]
    /// found them; none (plain) for a line it didn't cover or with no tree
    /// yet.
    fn line(&self, ln: usize) -> &[Span];

    /// Whether a parse runs on a worker.
    fn working(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    /// Byte offset of `p` in `text`.
    fn offset(text: &str, (row, col): Point) -> usize {
        text.split('\n').take(row).map(|l| l.len() + 1).sum::<usize>() + col
    }

    /// Replace `del` (char count `n`) at `at` (row, char col) in `old` with
    /// `ins`, and check the edit takes the old text's bytes to the new's.
    fn check(old: &str, at: (usize, usize), n: usize, ins: &str) {
        let old_lines = lines(old);
        let start = offset(old, (at.0, 0))
            + old_lines[at.0].char_indices().nth(at.1).map_or(old_lines[at.0].len(), |(b, _)| b);
        let end = old[start..].char_indices().nth(n).map_or(old.len(), |(b, _)| start + b);
        let del = &old[start..end];
        let new = format!("{}{ins}{}", &old[..start], &old[end..]);
        let e = TextEdit::of(&lines(&new), at, del, ins);
        assert_eq!(
            format!("{}{}{}", &old[..e.start_byte], &new[e.start_byte..e.new_end_byte], &old[e.old_end_byte..]),
            new,
            "{old:?} at {at:?}"
        );
        assert_eq!(offset(old, e.start), e.start_byte);
        assert_eq!(offset(&new, e.start), e.start_byte);
        assert_eq!(offset(old, e.old_end), e.old_end_byte, "old end of {old:?} at {at:?}");
        assert_eq!(offset(&new, e.new_end), e.new_end_byte, "new end of {old:?} at {at:?}");
        assert_eq!(text(&lines(&new)), new);
        assert_eq!(len(&lines(&new)), new.len());
    }

    #[test]
    fn a_text_edit_takes_the_old_bytes_to_the_new() {
        check("fun main() {}", (0, 4), 4, "start");
        check("a\nbc\nd", (1, 1), 0, "x\ny\n");
        check("a\nbc\nd", (0, 1), 4, "");
        check("ab\ncd", (0, 2), 1, "");
        check("é🦀 fun\nx", (0, 2), 1, "🦀\nval ");
        check("a\r\nb\r\n", (0, 1), 2, "\r\nz\r");
        check("", (0, 0), 0, "x");
    }

    #[test]
    fn chunks_read_the_rows_joined_by_newlines() {
        let l = lines("ab\n\ncd");
        let all: Vec<u8> = chunks(&l, (0, 0), (2, 2)).flatten().copied().collect();
        assert_eq!(all, b"ab\n\ncd");
        let part: Vec<u8> = chunks(&l, (0, 1), (2, 1)).flatten().copied().collect();
        assert_eq!(part, b"b\n\nc");
        assert_eq!(chunk(&l, (0, 1)), b"b");
        assert_eq!(chunk(&l, (0, 2)), b"\n");
        assert_eq!(chunk(&l, (1, 0)), b"\n");
        assert_eq!(chunk(&l, (2, 2)), b"", "no newline after the last row");
        assert_eq!(row_start(&l, 2), 4);
    }

    #[test]
    fn a_log_past_its_cap_asks_for_a_fresh_parse() {
        let e = TextEdit::of(&lines("x"), (0, 0), "", "x");
        let mut log = TextEdits::default();
        assert!(log.is_empty());
        for _ in 0..MAX_EDITS {
            log.push(e);
        }
        assert_eq!(log.edits.len(), MAX_EDITS);
        log.push(e);
        assert_eq!(log, TextEdits::reset());
        log.push(e);
        assert!(log.edits.is_empty() && !log.is_empty());
    }
}
