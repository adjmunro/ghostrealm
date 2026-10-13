//! Text editing shared by every text input: the editor, the palette, the file
//! browser's filter, and the picker's name field. The terminal takes its
//! readline word/line motions from the same [`Keymap`].
//!
//! A [`Keymap`] maps chords to [`Action`]s, one table per platform; Shift
//! extends the selection with any motion, so tables bind motions without it.
//! [`edit_for`] turns a key press into an [`Edit`] through the platform's map,
//! and each text model applies it: [`LineEdit`] for one line, the editor's
//! buffer for many. On macOS:
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
//! all; Cmd+C/X/V copy, cut, and paste; Cmd+Z undoes and Cmd+Shift+Z redoes
//! (where the text keeps history). Ctrl chords are never edits (Ctrl+arrows
//! belong to macOS). Linux and Windows use Ctrl where macOS uses Option (words)
//! and Cmd (the text's ends, select all, clipboard), and Home/End for lines.
//!
//! Brackets: typing `(`, `[`, or `{` over a selection wraps it in the pair and
//! keeps the wrapped text selected (every text input but the URL bar, where
//! brackets are address syntax; a paste never wraps). The editor goes further:
//! an opening bracket with no selection inserts its closer after the cursor
//! unless a word character follows; typing a closer when the same one comes
//! next steps over it; Backspace between an empty pair removes both. Wrapping
//! is its own undo step.

use std::sync::OnceLock;

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
    Undo,
    Redo,
}

/// What a bound chord does. With Shift held, a [`Action::Move`] selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Motion),
    Delete(Motion),
    SelectAll,
    Copy,
    Cut,
    Paste,
    /// With Shift held, redo.
    Undo,
    Redo,
}

/// A key with its modifiers, Shift aside (Shift selects). Letters are stored
/// lowercase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chord {
    key: Key,
    alt: bool,
    ctrl: bool,
    super_: bool,
}

impl Chord {
    fn of(k: &KeyPress) -> Self {
        let key = match k.key {
            Key::Char(c) => Key::Char(c.to_ascii_lowercase()),
            key => key,
        };
        Chord { key, alt: k.mods.alt, ctrl: k.mods.ctrl, super_: k.mods.super_ }
    }

    /// Parse `"alt+left"`, `"cmd+backspace"`, `"ctrl+a"`, ... (case-insensitive;
    /// `cmd`/`super`, `alt`/`option`). Shift is not allowed: it selects.
    fn parse(s: &str) -> Option<Self> {
        let mut c = Chord { key: Key::Char(' '), alt: false, ctrl: false, super_: false };
        let mut parts = s.split('+').map(str::trim).peekable();
        while let Some(part) = parts.next() {
            let p = part.to_ascii_lowercase();
            if parts.peek().is_some() {
                match p.as_str() {
                    "alt" | "option" | "opt" => c.alt = true,
                    "ctrl" | "control" => c.ctrl = true,
                    "cmd" | "super" | "command" => c.super_ = true,
                    _ => return None,
                }
                continue;
            }
            c.key = match p.as_str() {
                "left" => Key::Left,
                "right" => Key::Right,
                "up" => Key::Up,
                "down" => Key::Down,
                "home" => Key::Home,
                "end" => Key::End,
                "backspace" => Key::Backspace,
                "delete" => Key::Delete,
                _ => {
                    let mut chars = p.chars();
                    match (chars.next(), chars.next()) {
                        (Some(ch), None) => Key::Char(ch),
                        _ => return None,
                    }
                }
            };
        }
        Some(c)
    }
}

/// Chords to text-editing actions.
#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
}

/// Bindings both tables share.
const COMMON: &[(&str, Action)] = &[
    ("left", Action::Move(Motion::Left)),
    ("right", Action::Move(Motion::Right)),
    ("up", Action::Move(Motion::Up)),
    ("down", Action::Move(Motion::Down)),
    ("home", Action::Move(Motion::LineStart)),
    ("end", Action::Move(Motion::LineEnd)),
    ("backspace", Action::Delete(Motion::Left)),
    ("delete", Action::Delete(Motion::Right)),
];

const MACOS: &[(&str, Action)] = &[
    ("alt+left", Action::Move(Motion::WordLeft)),
    ("alt+right", Action::Move(Motion::WordRight)),
    ("alt+up", Action::Move(Motion::Up)),
    ("alt+down", Action::Move(Motion::Down)),
    ("cmd+left", Action::Move(Motion::LineStart)),
    ("cmd+right", Action::Move(Motion::LineEnd)),
    ("cmd+up", Action::Move(Motion::DocStart)),
    ("cmd+down", Action::Move(Motion::DocEnd)),
    ("cmd+home", Action::Move(Motion::DocStart)),
    ("cmd+end", Action::Move(Motion::DocEnd)),
    ("alt+backspace", Action::Delete(Motion::WordLeft)),
    ("alt+delete", Action::Delete(Motion::WordRight)),
    ("cmd+backspace", Action::Delete(Motion::LineStart)),
    ("cmd+delete", Action::Delete(Motion::LineEnd)),
    ("cmd+a", Action::SelectAll),
    ("cmd+c", Action::Copy),
    ("cmd+x", Action::Cut),
    ("cmd+v", Action::Paste),
    ("cmd+z", Action::Undo),
];

/// Linux and Windows.
const PC: &[(&str, Action)] = &[
    ("ctrl+left", Action::Move(Motion::WordLeft)),
    ("ctrl+right", Action::Move(Motion::WordRight)),
    ("ctrl+up", Action::Move(Motion::Up)),
    ("ctrl+down", Action::Move(Motion::Down)),
    ("ctrl+home", Action::Move(Motion::DocStart)),
    ("ctrl+end", Action::Move(Motion::DocEnd)),
    ("ctrl+backspace", Action::Delete(Motion::WordLeft)),
    ("ctrl+delete", Action::Delete(Motion::WordRight)),
    ("ctrl+a", Action::SelectAll),
    ("ctrl+c", Action::Copy),
    ("ctrl+x", Action::Cut),
    ("ctrl+v", Action::Paste),
    ("ctrl+z", Action::Undo),
    ("ctrl+y", Action::Redo),
];

impl Keymap {
    fn from_table(tables: &[&[(&str, Action)]]) -> Self {
        let bindings = tables
            .iter()
            .flat_map(|t| t.iter())
            .map(|(chord, action)| {
                let c = Chord::parse(chord).unwrap_or_else(|| panic!("bad chord `{chord}`"));
                (c, *action)
            })
            .collect();
        Keymap { bindings }
    }

    pub fn macos() -> Self {
        Self::from_table(&[COMMON, MACOS])
    }

    /// Linux and Windows.
    pub fn pc() -> Self {
        Self::from_table(&[COMMON, PC])
    }

    /// This platform's keymap.
    pub fn platform() -> &'static Keymap {
        static MAP: OnceLock<Keymap> = OnceLock::new();
        MAP.get_or_init(|| if cfg!(target_os = "macos") { Keymap::macos() } else { Keymap::pc() })
    }

    /// The action bound to `k`'s chord (Shift ignored).
    pub fn action(&self, k: &KeyPress) -> Option<Action> {
        let chord = Chord::of(k);
        self.bindings.iter().find(|(c, _)| *c == chord).map(|(_, a)| *a)
    }

    /// The edit `k` performs, if it is a text-editing key: a bound action, else
    /// typed text (a character without Ctrl or Cmd; Option-composed characters
    /// like å are text). Enter, Tab, and Escape are not edits: each input gives
    /// them its own meaning.
    pub fn edit_for(&self, k: &KeyPress) -> Option<Edit> {
        if let Some(action) = self.action(k) {
            return Some(match action {
                Action::Move(to) => Edit::Move { to, select: k.mods.shift },
                Action::Delete(to) => Edit::Delete(to),
                Action::SelectAll => Edit::SelectAll,
                Action::Copy => Edit::Copy,
                Action::Cut => Edit::Cut,
                Action::Paste => Edit::Paste,
                Action::Undo if k.mods.shift => Edit::Redo,
                Action::Undo => Edit::Undo,
                Action::Redo => Edit::Redo,
            });
        }
        let Key::Char(c) = k.key else {
            return None;
        };
        if k.mods.ctrl || k.mods.super_ {
            return None;
        }
        let typed = k.text.clone().unwrap_or_else(|| c.to_string());
        let typed: String = typed.chars().filter(|c| !c.is_control()).collect();
        (!typed.is_empty()).then_some(Edit::Insert(typed))
    }
}

/// The edit `k` performs under this platform's [`Keymap`].
pub fn edit_for(k: &KeyPress) -> Option<Edit> {
    Keymap::platform().edit_for(k)
}

/// Word characters, for Option motion: letters, digits, and `_`.
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The closing bracket for an opening `(`, `[`, or `{`.
pub fn closer_of(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

/// The opening bracket for a closing `)`, `]`, or `}`.
pub fn opener_of(close: char) -> Option<char> {
    match close {
        ')' => Some('('),
        ']' => Some('['),
        '}' => Some('{'),
        _ => None,
    }
}

/// `s` as a single typed character, if it is one.
pub fn typed_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Whether auto-closing an opening bracket is welcome before `next` (the char
/// after the cursor): not before a word character, where the bracket most
/// likely stands alone (`(` typed in front of `foo`).
pub fn closes_before(next: Option<char>) -> bool {
    next.is_none_or(|c| !is_word(c))
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
/// app's handle), or a private string.
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

/// A private clipboard: tests, and edits with no system clipboard at hand.
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
    /// Brackets are plain text: typing one over a selection replaces it.
    plain_brackets: bool,
}

impl LineEdit {
    /// `text` with the cursor at its end.
    pub fn new(text: impl Into<String>) -> Self {
        let mut e = LineEdit::default();
        e.set_text(text);
        e
    }

    /// Typing a bracket over a selection replaces it instead of wrapping it:
    /// for a field whose brackets are syntax of their own (a URL's `[::1]`).
    pub fn with_plain_brackets(mut self) -> Self {
        self.plain_brackets = true;
        self
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

    /// Typing an opening bracket over a selection wraps it in the pair, keeping
    /// the wrapped text selected. Returns whether it did.
    fn wrap_selection(&mut self, open: char) -> bool {
        let (Some(close), Some((s, e))) = (closer_of(open), self.selection()) else {
            return false;
        };
        if self.plain_brackets {
            return false;
        }
        let (be, bs) = (self.byte_of(e), self.byte_of(s));
        self.text.insert(be, close);
        self.text.insert(bs, open);
        let forward = self.cursor > self.anchor.unwrap_or(self.cursor);
        let (inner_s, inner_e) = (s + 1, e + 1);
        (self.anchor, self.cursor) = if forward { (Some(inner_s), inner_e) } else { (Some(inner_e), inner_s) };
        true
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
                if !typed_char(&s).is_some_and(|c| self.wrap_selection(c)) {
                    self.insert(&s);
                }
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
            Edit::Undo | Edit::Redo => Applied::Ignored,
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
    fn a_bracket_typed_over_a_selection_wraps_it_in_a_line() {
        let mut clip: Option<String> = None;
        for (open, want) in [('(', "a(bc)d"), ('[', "a[bc]d"), ('{', "a{bc}d")] {
            let mut e = LineEdit::new("abcd");
            e.apply(Edit::Move { to: Motion::LineStart, select: false }, &mut clip);
            e.apply(Edit::Move { to: Motion::Right, select: false }, &mut clip);
            e.apply(Edit::Move { to: Motion::Right, select: true }, &mut clip);
            e.apply(Edit::Move { to: Motion::Right, select: true }, &mut clip);
            e.apply(Edit::Insert(open.to_string()), &mut clip);
            assert_eq!(e.text(), want);
            assert_eq!(e.selection(), Some((2, 4)), "the wrapped text stays selected");
        }
        // Without a selection a bracket is plain text, and so is any other character.
        let mut e = LineEdit::new("ab");
        e.apply(Edit::Insert("(".into()), &mut clip);
        assert_eq!(e.text(), "ab(");
        e.apply(Edit::SelectAll, &mut clip);
        e.apply(Edit::Insert("x".into()), &mut clip);
        assert_eq!(e.text(), "x", "other characters replace the selection");
        // A closing bracket replaces it too, and a paste of "(" does not wrap.
        let mut e = LineEdit::new("ab");
        e.apply(Edit::SelectAll, &mut clip);
        clip = Some("(".into());
        e.apply(Edit::Paste, &mut clip);
        assert_eq!(e.text(), "(");
        // A backward selection wraps and keeps its direction.
        let mut e = LineEdit::new("abc");
        e.apply(Edit::Move { to: Motion::Left, select: true }, &mut clip);
        e.apply(Edit::Insert("[".into()), &mut clip);
        assert_eq!(e.text(), "ab[c]");
        assert_eq!((e.cursor(), e.selection()), (3, Some((3, 4))));
    }

    #[test]
    fn chords_parse_and_reject_shift() {
        let c = Chord::parse("Cmd+Option+Left").unwrap();
        assert!(c.super_ && c.alt && !c.ctrl && c.key == Key::Left);
        assert_eq!(Chord::parse("ctrl+a").map(|c| c.key), Some(Key::Char('a')));
        assert_eq!(Chord::parse("shift+left"), None, "Shift selects; it isn't bound");
        assert_eq!(Chord::parse("alt+nope"), None);
    }

    #[test]
    fn pc_uses_ctrl_for_words_and_the_clipboard() {
        let pc = Keymap::pc();
        let ctrl = |key| k(key, |m| m.ctrl = true);
        assert_eq!(pc.edit_for(&ctrl(Key::Left)), Some(Edit::Move { to: Motion::WordLeft, select: false }));
        let ctrl_shift_end = k(Key::End, |m| {
            m.ctrl = true;
            m.shift = true;
        });
        assert_eq!(pc.edit_for(&ctrl_shift_end), Some(Edit::Move { to: Motion::DocEnd, select: true }));
        assert_eq!(pc.edit_for(&ctrl(Key::Char('v'))), Some(Edit::Paste));
        assert_eq!(pc.edit_for(&ctrl(Key::Char('q'))), None, "unbound Ctrl chords aren't text");
        assert_eq!(pc.edit_for(&k(Key::Left, |m| m.super_ = true)), None);
    }

    #[test]
    fn modifiers_pick_the_motion_and_shift_selects() {
        let edit_for = |k: &KeyPress| Keymap::macos().edit_for(k);
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
