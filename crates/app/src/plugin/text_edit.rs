//! Text editing shared by every text input: the editor, the palette, the file
//! browser's filter, and the picker's name field.
//!
//! [`edit_for`] maps a key press to an [`Edit`] by macOS conventions, and each
//! text model applies it: [`LineEdit`] for one line, the editor's buffer for
//! many. Modifiers combine — Shift extends the selection with any motion:
//!
//! | Key | Motion |
//! |---|---|
//! | ←/→ | a character |
//! | Option+←/→ | a word |
//! | Cmd+←/→, Home/End | the line's start/end |
//! | ↑/↓ | a line (multi-line text only) |
//! | Cmd+↑/↓, Cmd+Home/End | the text's start/end |
//!
//! Backspace/Delete remove a character, Option a word, Cmd to the line's start
//! (Backspace) or end (Delete); with a selection they remove it. Cmd+A selects
//! all; Cmd+C/X/V copy, cut, and paste. Ctrl chords are never edits (Ctrl+arrows
//! belong to macOS).

use ghostrealm_terminal::{Key, KeyPress};

use super::EventCx;

/// Where a motion takes the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    Left,
    Right,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    Up,
    Down,
    DocStart,
    DocEnd,
}

/// What a key does to editable text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Move the cursor; `select` extends the selection instead of dropping it.
    Move { to: Motion, select: bool },
    /// Remove the selection, else the text between the cursor and `Motion`.
    Delete(Motion),
    /// Type text over the selection.
    Insert(String),
    SelectAll,
    Copy,
    Cut,
    Paste,
}

/// The edit `k` performs, if it is a text-editing key. Enter, Tab, and Escape
/// are not: each input gives them its own meaning.
pub fn edit_for(k: &KeyPress) -> Option<Edit> {
    let m = k.mods;
    if m.ctrl {
        return None;
    }
    let mv = |to| Some(Edit::Move { to, select: m.shift });
    if m.super_ {
        return match k.key {
            Key::Left => mv(Motion::LineStart),
            Key::Right => mv(Motion::LineEnd),
            Key::Up | Key::Home => mv(Motion::DocStart),
            Key::Down | Key::End => mv(Motion::DocEnd),
            Key::Backspace => Some(Edit::Delete(Motion::LineStart)),
            Key::Delete => Some(Edit::Delete(Motion::LineEnd)),
            Key::Char(c) => match c.to_ascii_lowercase() {
                'a' => Some(Edit::SelectAll),
                'c' => Some(Edit::Copy),
                'x' => Some(Edit::Cut),
                'v' => Some(Edit::Paste),
                _ => None,
            },
            _ => None,
        };
    }
    let (left, right) = if m.alt {
        (Motion::WordLeft, Motion::WordRight)
    } else {
        (Motion::Left, Motion::Right)
    };
    match k.key {
        Key::Left => mv(left),
        Key::Right => mv(right),
        Key::Up => mv(Motion::Up),
        Key::Down => mv(Motion::Down),
        Key::Home => mv(Motion::LineStart),
        Key::End => mv(Motion::LineEnd),
        Key::Backspace => Some(Edit::Delete(left)),
        Key::Delete => Some(Edit::Delete(right)),
        Key::Char(c) => {
            // Option-composed characters (å, ∑, ...) are text, as on macOS.
            let typed = k.text.clone().unwrap_or_else(|| c.to_string());
            let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
            (!typed.is_empty()).then_some(Edit::Insert(typed))
        }
        _ => None,
    }
}

/// Word characters, for Option motion: letters, digits, and `_`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start of the word before `col`: skips non-word characters, then the word.
pub fn word_left(chars: &[char], col: usize) -> usize {
    let mut i = col.min(chars.len());
    while i > 0 && !is_word(chars[i - 1]) {
        i -= 1;
    }
    while i > 0 && is_word(chars[i - 1]) {
        i -= 1;
    }
    i
}

/// The end of the word after `col`: skips non-word characters, then the word.
pub fn word_right(chars: &[char], col: usize) -> usize {
    let n = chars.len();
    let mut i = col.min(n);
    while i < n && !is_word(chars[i]) {
        i += 1;
    }
    while i < n && is_word(chars[i]) {
        i += 1;
    }
    i
}

/// Where copy/cut/paste go: the system clipboard (through an [`EventCx`] or the
/// app's handle), or a string in tests.
pub trait Clipboard {
    fn text(&mut self) -> Option<String>;
    fn set_text(&mut self, text: String);
}

impl Clipboard for EventCx<'_> {
    fn text(&mut self) -> Option<String> {
        self.clipboard_text()
    }

    fn set_text(&mut self, text: String) {
        self.set_clipboard_text(text);
    }
}

impl Clipboard for Option<arboard::Clipboard> {
    fn text(&mut self) -> Option<String> {
        self.as_mut()?.get_text().ok()
    }

    fn set_text(&mut self, text: String) {
        if let Some(cb) = self.as_mut() {
            let _ = cb.set_text(text);
        }
    }
}

#[cfg(test)]
impl Clipboard for Option<String> {
    fn text(&mut self) -> Option<String> {
        self.clone()
    }

    fn set_text(&mut self, text: String) {
        *self = Some(text);
    }
}

/// What applying an edit did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Not an edit this text takes (e.g. ↑/↓ on one line): the caller may use
    /// the key for something else.
    Ignored,
    /// The cursor or selection moved, or text was copied.
    Moved,
    /// The text changed.
    Changed,
}

impl Applied {
    pub fn used(self) -> bool {
        self != Applied::Ignored
    }
}

/// One line of editable text with a cursor and an optional selection. Columns
/// count Unicode scalar values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Cursor column (chars).
    cursor: usize,
    /// Selection anchor column; the selection spans anchor..cursor.
    anchor: Option<usize>,
}

impl LineEdit {
    /// `text` with the cursor at its end.
    pub fn new(text: impl Into<String>) -> Self {
        let mut e = LineEdit::default();
        e.set_text(text);
        e
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    /// Replace the text, cursor at the end, no selection.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
        self.anchor = None;
    }

    /// The selection as an ordered, non-empty `(start, end)` column range.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| (a.min(self.cursor), a.max(self.cursor)))
    }

    /// Byte offset of column `col`.
    pub fn byte_of(&self, col: usize) -> usize {
        self.text.char_indices().nth(col).map(|(b, _)| b).unwrap_or(self.text.len())
    }

    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        Some(self.text[self.byte_of(s)..self.byte_of(e)].to_string())
    }

    /// Remove columns `s..e`, leaving the cursor at `s`.
    fn remove(&mut self, s: usize, e: usize) {
        let (bs, be) = (self.byte_of(s), self.byte_of(e));
        self.text.replace_range(bs..be, "");
        self.cursor = s;
        self.anchor = None;
    }

    /// Remove the selection; whether there was one.
    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((s, e)) => {
                self.remove(s, e);
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    /// Type `s` over the selection. Line breaks become spaces; other control
    /// characters are dropped.
    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let clean: String = s
            .chars()
            .filter_map(|c| match c {
                '\n' | '\t' => Some(' '),
                c if c.is_control() => None,
                c => Some(c),
            })
            .collect();
        let b = self.byte_of(self.cursor);
        self.text.insert_str(b, &clean);
        self.cursor += clean.chars().count();
    }

    /// The column `motion` reaches from the cursor, or `None` if it leaves the
    /// line (↑/↓).
    fn target(&self, motion: Motion) -> Option<usize> {
        let chars: Vec<char> = self.text.chars().collect();
        let c = self.cursor;
        Some(match motion {
            Motion::Left => c.saturating_sub(1),
            Motion::Right => (c + 1).min(chars.len()),
            Motion::WordLeft => word_left(&chars, c),
            Motion::WordRight => word_right(&chars, c),
            Motion::LineStart | Motion::DocStart => 0,
            Motion::LineEnd | Motion::DocEnd => chars.len(),
            Motion::Up | Motion::Down => return None,
        })
    }

    /// Apply the edit for key `k` (see [`edit_for`]).
    pub fn key(&mut self, k: &KeyPress, clip: &mut dyn Clipboard) -> Applied {
        match edit_for(k) {
            Some(edit) => self.apply(edit, clip),
            None => Applied::Ignored,
        }
    }

    pub fn apply(&mut self, edit: Edit, clip: &mut dyn Clipboard) -> Applied {
        match edit {
            Edit::Move { to, select } => {
                let Some(t) = self.target(to) else {
                    return Applied::Ignored;
                };
                if select {
                    self.anchor.get_or_insert(self.cursor);
                    self.cursor = t;
                } else {
                    // A plain ←/→ collapses a selection to that side.
                    self.cursor = match (self.selection(), to) {
                        (Some((s, _)), Motion::Left) => s,
                        (Some((_, e)), Motion::Right) => e,
                        _ => t,
                    };
                    self.anchor = None;
                }
                Applied::Moved
            }
            Edit::Delete(to) => {
                if self.delete_selection() {
                    return Applied::Changed;
                }
                let Some(t) = self.target(to) else {
                    return Applied::Ignored;
                };
                if t == self.cursor {
                    return Applied::Moved;
                }
                self.remove(t.min(self.cursor), t.max(self.cursor));
                Applied::Changed
            }
            Edit::Insert(s) => {
                self.insert(&s);
                Applied::Changed
            }
            Edit::SelectAll => {
                self.anchor = Some(0);
                self.cursor = self.len();
                Applied::Moved
            }
            Edit::Copy => {
                if let Some(t) = self.selected_text() {
                    clip.set_text(t);
                }
                Applied::Moved
            }
            Edit::Cut => match self.selected_text() {
                Some(t) => {
                    clip.set_text(t);
                    self.delete_selection();
                    Applied::Changed
                }
                None => Applied::Moved,
            },
            Edit::Paste => match clip.text() {
                Some(t) if !t.is_empty() => {
                    self.insert(&t);
                    Applied::Changed
                }
                _ => Applied::Moved,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostrealm_terminal::Mods;

    fn k(key: Key, f: impl FnOnce(&mut Mods)) -> KeyPress {
        let mut mods = Mods::default();
        f(&mut mods);
        KeyPress { key, mods, text: None }
    }

    fn plain(key: Key) -> KeyPress {
        k(key, |_| {})
    }

    #[test]
    fn modifiers_pick_the_motion_and_shift_selects() {
        assert_eq!(edit_for(&plain(Key::Left)), Some(Edit::Move { to: Motion::Left, select: false }));
        let opt_shift = k(Key::Right, |m| {
            m.alt = true;
            m.shift = true;
        });
        assert_eq!(edit_for(&opt_shift), Some(Edit::Move { to: Motion::WordRight, select: true }));
        let cmd_shift = k(Key::Left, |m| {
            m.super_ = true;
            m.shift = true;
        });
        assert_eq!(edit_for(&cmd_shift), Some(Edit::Move { to: Motion::LineStart, select: true }));
        assert_eq!(edit_for(&k(Key::Up, |m| m.super_ = true)), Some(Edit::Move { to: Motion::DocStart, select: false }));
        assert_eq!(edit_for(&k(Key::Left, |m| m.ctrl = true)), None, "Ctrl+arrows are macOS's");
        assert_eq!(edit_for(&k(Key::Backspace, |m| m.alt = true)), Some(Edit::Delete(Motion::WordLeft)));
        assert_eq!(edit_for(&k(Key::Backspace, |m| m.super_ = true)), Some(Edit::Delete(Motion::LineStart)));
        assert_eq!(edit_for(&k(Key::Char('V'), |m| m.super_ = true)), Some(Edit::Paste));
        assert_eq!(edit_for(&plain(Key::Enter)), None);
    }

    #[test]
    fn word_motion_skips_punctuation_then_the_word() {
        let s: Vec<char> = "/usr/local_bin  foo".chars().collect();
        assert_eq!(word_left(&s, s.len()), 16);
        assert_eq!(word_left(&s, 16), 5, "local_bin is one word");
        assert_eq!(word_left(&s, 5), 1);
        assert_eq!(word_left(&s, 1), 0);
        assert_eq!(word_right(&s, 0), 4);
        assert_eq!(word_right(&s, 4), 14);
    }

    #[test]
    fn select_by_word_then_type_over_it() {
        let mut e = LineEdit::new("hello big world");
        let mut clip: Option<String> = None;
        let sel_word_left = k(Key::Left, |m| {
            m.alt = true;
            m.shift = true;
        });
        e.key(&sel_word_left, &mut clip);
        assert_eq!(e.selection(), Some((10, 15)));
        e.key(&sel_word_left, &mut clip);
        assert_eq!(e.selection(), Some((6, 15)), "shift extends from the anchor");
        assert_eq!(e.apply(Edit::Insert("there".into()), &mut clip), Applied::Changed);
        assert_eq!(e.text(), "hello there");
        assert_eq!(e.cursor(), 11);
    }

    #[test]
    fn plain_arrows_collapse_a_selection_to_its_side() {
        let mut e = LineEdit::new("abcdef");
        let mut clip: Option<String> = None;
        e.apply(Edit::Move { to: Motion::WordLeft, select: true }, &mut clip);
        e.apply(Edit::Move { to: Motion::Left, select: false }, &mut clip);
        assert_eq!((e.cursor(), e.selection()), (0, None));
        e.apply(Edit::SelectAll, &mut clip);
        e.apply(Edit::Move { to: Motion::Right, select: false }, &mut clip);
        assert_eq!((e.cursor(), e.selection()), (6, None));
    }

    #[test]
    fn deletes_by_char_word_and_line() {
        let mut e = LineEdit::new("one two three");
        let mut clip: Option<String> = None;
        e.key(&k(Key::Backspace, |m| m.alt = true), &mut clip);
        assert_eq!(e.text(), "one two ");
        e.key(&plain(Key::Backspace), &mut clip);
        assert_eq!(e.text(), "one two");
        e.apply(Edit::Move { to: Motion::WordLeft, select: false }, &mut clip);
        e.key(&k(Key::Delete, |m| m.super_ = true), &mut clip);
        assert_eq!(e.text(), "one ");
        e.key(&k(Key::Backspace, |m| m.super_ = true), &mut clip);
        assert_eq!(e.text(), "");
        assert_eq!(e.key(&plain(Key::Backspace), &mut clip), Applied::Moved, "nothing left");
    }

    #[test]
    fn cut_copy_paste_through_the_clipboard() {
        let mut e = LineEdit::new("abc");
        let mut clip: Option<String> = None;
        e.apply(Edit::SelectAll, &mut clip);
        e.apply(Edit::Cut, &mut clip);
        assert_eq!((e.text(), clip.as_deref()), ("", Some("abc")));
        clip = Some("line one\nline two".into());
        e.apply(Edit::Paste, &mut clip);
        assert_eq!(e.text(), "line one line two", "one line: breaks become spaces");
    }

    #[test]
    fn up_and_down_are_left_to_the_caller() {
        let mut e = LineEdit::new("x");
        assert_eq!(e.key(&plain(Key::Up), &mut None::<String>), Applied::Ignored);
        assert_eq!(e.key(&plain(Key::Enter), &mut None::<String>), Applied::Ignored);
    }

    #[test]
    fn unicode_columns() {
        let mut e = LineEdit::new("áé");
        let mut clip: Option<String> = None;
        e.apply(Edit::Move { to: Motion::Left, select: false }, &mut clip);
        e.insert("x");
        assert_eq!(e.text(), "áxé");
        e.apply(Edit::Delete(Motion::Right), &mut clip);
        assert_eq!(e.text(), "áx");
    }
}
