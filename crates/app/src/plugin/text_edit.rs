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
//! all; Cmd+C/X/V copy, cut, and paste; Cmd+Z undoes and Cmd+Shift+Z redoes.
//! Ctrl chords are never edits (Ctrl+arrows
//! belong to macOS). Linux and Windows use Ctrl where macOS uses Option (words)
//! and Cmd (the text's ends, select all, clipboard), and Home/End for lines.
//!
//! Brackets: typing `(`, `[`, or `{` over a selection wraps it in the pair and
//! keeps the wrapped text selected (every text input but the URL bar, where
//! brackets are address syntax; a paste never wraps). The editor goes further:
//! an opening bracket with no selection inserts its closer after the cursor
//! when the line ends there or whitespace, a closer, or a separator follows
//! ([`closes_before`]); typing a closer when the same one comes next steps
//! over it, and Backspace between an empty pair removes both, but only for a
//! closer the editor inserted and only while the cursor stays on that line. Wrapping is its own
//! undo step.

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
/// after the cursor): at the line's end, before whitespace, a closing bracket,
/// or a separator. Anywhere else (a word, a quote, an opening bracket, an
/// operator) the bracket most likely starts an expression the user closes
/// further on (`(` typed in front of `"foo"`).
pub fn closes_before(next: Option<char>) -> bool {
    next.is_none_or(|c| c.is_whitespace() || ")]};:.,=>".contains(c))
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

/// The run of same-class characters (word characters, whitespace, or other
/// punctuation) containing `at`, as a `(start, end)` column range: what a
/// double-click selects. `at` past the end counts as the last character.
/// `None` for empty text.
pub fn word_span(chars: &[char], at: usize) -> Option<(usize, usize)> {
    if chars.is_empty() {
        return None;
    }
    let class = |c: char| {
        if is_word(c) {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let at = at.min(chars.len() - 1);
    let k = class(chars[at]);
    let mut start = at;
    while start > 0 && class(chars[start - 1]) == k {
        start -= 1;
    }
    let mut end = at + 1;
    while end < chars.len() && class(chars[end]) == k {
        end += 1;
    }
    Some((start, end))
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

/// Most undo steps a [`LineEdit`] keeps; older ones are dropped.
const LINE_UNDO_MAX: usize = 100;

/// A [`LineEdit`]'s text, cursor, and selection at one point.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LineSnapshot {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
}

/// What the open undo run merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run {
    /// Typed characters; `word` is whether the last one was a word character.
    Typing { word: bool },
    /// Single-character deletes.
    Deleting,
}

impl Run {
    /// Whether `next` joins a run of `self`: same kind, and typing doesn't
    /// start a new word.
    fn takes(self, next: Run) -> bool {
        match (self, next) {
            (Run::Typing { word: prev }, Run::Typing { word }) => prev || !word,
            (a, b) => a == b,
        }
    }
}

/// Linear undo/redo for a [`LineEdit`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LineHistory {
    undo: Vec<LineSnapshot>,
    redo: Vec<LineSnapshot>,
    /// The open run and the cursor its last edit left.
    run: Option<(Run, usize)>,
}

/// One line of editable text with a cursor and an optional selection. Columns
/// count Unicode scalar values.
///
/// Edits keep a linear undo history of up to [`LINE_UNDO_MAX`] steps. A run of
/// typed characters is one step until a word character follows a non-word one
/// (`hello ` then `world`); a run of single-character deletes is one step; a
/// cursor move, click, or selection ends a run; and a paste, cut, word or line
/// delete, or bracket wrap is a step of its own. [`set_text`](Self::set_text)
/// clears the history: the owner replaced the text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineEdit {
    text: String,
    /// Cursor column (chars).
    cursor: usize,
    /// Selection anchor column; the selection spans anchor..cursor.
    anchor: Option<usize>,
    /// Brackets are plain text: typing one over a selection replaces it.
    plain_brackets: bool,
    history: LineHistory,
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

    /// Replace the text, cursor at the end, no selection, no undo history.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.len();
        self.anchor = None;
        self.history = LineHistory::default();
    }

    fn snapshot(&self) -> LineSnapshot {
        LineSnapshot { text: self.text.clone(), cursor: self.cursor, anchor: self.anchor }
    }

    /// Undo (`undo`) or redo one step. With nothing to step to, the key is
    /// still used: it never falls through to whatever holds the field.
    fn step_history(&mut self, undo: bool) -> Applied {
        let cur = self.snapshot();
        let h = &mut self.history;
        let (from, to) = if undo { (&mut h.undo, &mut h.redo) } else { (&mut h.redo, &mut h.undo) };
        let Some(s) = from.pop() else {
            return Applied::Moved;
        };
        to.push(cur);
        h.run = None;
        (self.text, self.cursor, self.anchor) = (s.text, s.cursor, s.anchor);
        Applied::Changed
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

    /// A mouse press at caret column `caret` (the boundary nearest the pointer)
    /// over character `over` (the one under it, for word selection). `clicks`
    /// counts quick successive presses: 1 places the cursor, 2 selects the word
    /// under the pointer, 3 or more everything. With `extend` (Shift), a single
    /// press stretches the selection from the anchor, or from the cursor if
    /// there is none. Following [`drag_to`](Self::drag_to)s extend the
    /// selection of a single press.
    pub fn press(&mut self, caret: usize, over: usize, clicks: u32, extend: bool) {
        let caret = caret.min(self.len());
        self.history.run = None;
        match clicks {
            0 | 1 if extend => {
                self.anchor.get_or_insert(self.cursor);
                self.cursor = caret;
            }
            0 | 1 => {
                self.cursor = caret;
                self.anchor = Some(caret);
            }
            2 => {
                let chars: Vec<char> = self.text.chars().collect();
                match word_span(&chars, over) {
                    Some((s, e)) => (self.anchor, self.cursor) = (Some(s), e),
                    None => (self.anchor, self.cursor) = (None, 0),
                }
            }
            _ => (self.anchor, self.cursor) = (Some(0), self.len()),
        }
    }

    /// The pointer moved to caret column `caret` with the button held: extend
    /// the selection to it.
    pub fn drag_to(&mut self, caret: usize) {
        self.anchor.get_or_insert(self.cursor);
        self.cursor = caret.min(self.len());
    }

    /// The button was released: a press that selected nothing leaves no
    /// anchor behind.
    pub fn release(&mut self) {
        if self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
    }

    /// Apply the edit for key `k` (see [`edit_for`]).
    pub fn key(&mut self, k: &KeyPress, clip: &mut dyn Clipboard) -> Applied {
        match edit_for(k) {
            Some(edit) => self.apply(edit, clip),
            None => Applied::Ignored,
        }
    }

    /// Apply `edit`, recording a text change for undo (see [`LineEdit`]).
    pub fn apply(&mut self, edit: Edit, clip: &mut dyn Clipboard) -> Applied {
        let fresh = self.selection().is_some();
        let run = match &edit {
            Edit::Undo => return self.step_history(true),
            Edit::Redo => return self.step_history(false),
            Edit::Insert(s) => typed_char(s).map(|c| Run::Typing { word: is_word(c) }),
            Edit::Delete(Motion::Left | Motion::Right) if !fresh => Some(Run::Deleting),
            _ => None,
        };
        let before = self.snapshot();
        let applied = self.apply_edit(edit, clip);
        let h = &mut self.history;
        match applied {
            Applied::Changed => {
                let joins = !fresh
                    && run.is_some_and(|r| h.run.is_some_and(|(open, at)| at == before.cursor && open.takes(r)));
                if !joins {
                    h.undo.push(before);
                    if h.undo.len() > LINE_UNDO_MAX {
                        h.undo.remove(0);
                    }
                }
                h.redo.clear();
                h.run = run.map(|r| (r, self.cursor));
            }
            Applied::Moved => h.run = None,
            Applied::Ignored => {}
        }
        applied
    }

    fn apply_edit(&mut self, edit: Edit, clip: &mut dyn Clipboard) -> Applied {
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
    fn a_press_places_the_cursor_and_a_drag_selects() {
        let mut e = LineEdit::new("hello world");
        e.press(3, 3, 1, false);
        e.release();
        assert_eq!((e.cursor(), e.selection()), (3, None));
        e.press(2, 2, 1, false);
        e.drag_to(7);
        e.release();
        assert_eq!((e.cursor(), e.selection()), (7, Some((2, 7))));
        e.drag_to(99);
        assert_eq!(e.cursor(), 11, "clamped to the end");
    }

    #[test]
    fn shift_press_extends_from_the_anchor_or_the_cursor() {
        let mut e = LineEdit::new("hello world");
        e.press(2, 2, 1, false);
        e.release();
        e.press(6, 6, 1, true);
        assert_eq!(e.selection(), Some((2, 6)));
        e.press(9, 9, 1, true);
        assert_eq!(e.selection(), Some((2, 9)), "the anchor stays");
    }

    #[test]
    fn double_click_selects_the_word_and_triple_click_everything() {
        let mut e = LineEdit::new("foo.bar baz");
        e.press(5, 5, 2, false);
        assert_eq!(e.selection(), Some((4, 7)), "bar, as the editor would");
        e.release();
        assert_eq!(e.selection(), Some((4, 7)), "the release keeps it");
        e.press(3, 3, 2, false);
        assert_eq!(e.selection(), Some((3, 4)), "punctuation is its own run");
        e.press(1, 1, 3, false);
        assert_eq!(e.selection(), Some((0, 11)));
        let mut empty = LineEdit::new("");
        empty.press(0, 0, 2, false);
        assert_eq!(empty.selection(), None);
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

    fn typed(e: &mut LineEdit, s: &str) {
        for c in s.chars() {
            e.apply(Edit::Insert(c.to_string()), &mut None::<String>);
        }
    }

    fn undo(e: &mut LineEdit) -> Applied {
        e.apply(Edit::Undo, &mut None::<String>)
    }

    fn redo(e: &mut LineEdit) -> Applied {
        e.apply(Edit::Redo, &mut None::<String>)
    }

    #[test]
    fn a_typing_run_undoes_a_word_at_a_time_and_redoes() {
        let mut e = LineEdit::new("");
        typed(&mut e, "hello world");
        assert_eq!(undo(&mut e), Applied::Changed);
        assert_eq!(e.text(), "hello ", "a word starts a new step");
        undo(&mut e);
        assert_eq!(e.text(), "");
        assert_eq!(undo(&mut e), Applied::Moved, "nothing left, but the key is used");
        redo(&mut e);
        assert_eq!((e.text(), e.cursor()), ("hello ", 6));
        redo(&mut e);
        assert_eq!(e.text(), "hello world");
        assert_eq!(redo(&mut e), Applied::Moved);
    }

    #[test]
    fn a_cursor_move_or_click_ends_a_typing_run() {
        let mut clip: Option<String> = None;
        let mut e = LineEdit::new("");
        typed(&mut e, "ab");
        e.apply(Edit::Move { to: Motion::Left, select: false }, &mut clip);
        e.apply(Edit::Move { to: Motion::Right, select: false }, &mut clip);
        typed(&mut e, "cd");
        undo(&mut e);
        assert_eq!(e.text(), "ab", "back at the same spot, still a new step");
        e.press(0, 0, 1, false);
        e.release();
        e.press(2, 2, 1, false);
        e.release();
        typed(&mut e, "x");
        undo(&mut e);
        assert_eq!(e.text(), "ab");
    }

    #[test]
    fn single_deletes_merge_and_a_word_delete_is_its_own_step() {
        let mut e = LineEdit::new("one two three");
        let mut clip: Option<String> = None;
        e.key(&plain(Key::Backspace), &mut clip);
        e.key(&plain(Key::Backspace), &mut clip);
        e.key(&k(Key::Backspace, |m| m.alt = true), &mut clip);
        assert_eq!(e.text(), "one two ");
        undo(&mut e);
        assert_eq!(e.text(), "one two thr", "the word delete alone");
        undo(&mut e);
        assert_eq!(e.text(), "one two three", "both backspaces at once");
    }

    #[test]
    fn paste_and_cut_are_steps_of_their_own() {
        let mut e = LineEdit::new("");
        typed(&mut e, "ab");
        let mut clip = Some("XY".to_string());
        e.apply(Edit::Paste, &mut clip);
        typed(&mut e, "c");
        assert_eq!(e.text(), "abXYc");
        undo(&mut e);
        assert_eq!(e.text(), "abXY", "typing after a paste doesn't join it");
        undo(&mut e);
        assert_eq!(e.text(), "ab", "the paste alone");
        e.apply(Edit::SelectAll, &mut clip);
        e.apply(Edit::Cut, &mut clip);
        assert_eq!(e.text(), "");
        undo(&mut e);
        assert_eq!((e.text(), e.selection()), ("ab", Some((0, 2))), "the selection comes back");
    }

    #[test]
    fn typing_over_a_selection_is_one_step() {
        let mut e = LineEdit::new("hello big world");
        let mut clip: Option<String> = None;
        e.apply(Edit::Move { to: Motion::WordLeft, select: true }, &mut clip);
        typed(&mut e, "there");
        assert_eq!(e.text(), "hello big there");
        undo(&mut e);
        assert_eq!(e.text(), "hello big world");
        // Wrapping in brackets is a step of its own.
        e.apply(Edit::Insert("(".into()), &mut clip);
        assert_eq!(e.text(), "hello big (world)");
        undo(&mut e);
        assert_eq!(e.text(), "hello big world");
    }

    #[test]
    fn a_new_edit_clears_redo() {
        let mut e = LineEdit::new("");
        typed(&mut e, "ab cd");
        undo(&mut e);
        assert_eq!(e.text(), "ab ");
        typed(&mut e, "x");
        assert_eq!(redo(&mut e), Applied::Moved);
        assert_eq!(e.text(), "ab x");
        // A move alone keeps redo.
        undo(&mut e);
        e.apply(Edit::Move { to: Motion::LineStart, select: false }, &mut None::<String>);
        redo(&mut e);
        assert_eq!(e.text(), "ab x");
    }

    #[test]
    fn set_text_clears_the_history() {
        let mut e = LineEdit::new("");
        typed(&mut e, "ab cd");
        undo(&mut e);
        e.set_text("reset");
        assert_eq!(undo(&mut e), Applied::Moved);
        assert_eq!(redo(&mut e), Applied::Moved);
        assert_eq!(e.text(), "reset");
    }

    #[test]
    fn undo_keeps_at_most_line_undo_max_steps() {
        let mut e = LineEdit::new("");
        let mut clip = Some("x".to_string());
        for _ in 0..LINE_UNDO_MAX + 5 {
            e.apply(Edit::Paste, &mut clip);
        }
        assert_eq!(e.history.undo.len(), LINE_UNDO_MAX);
        while undo(&mut e) == Applied::Changed {}
        assert_eq!(e.text().len(), 5, "the oldest steps went");
    }

    #[test]
    fn the_undo_chords_reach_the_field() {
        let mut e = LineEdit::new("");
        let mut clip: Option<String> = None;
        typed(&mut e, "a");
        let mac = Keymap::macos();
        let cmd_z = k(Key::Char('z'), |m| m.super_ = true);
        let cmd_shift_z = k(Key::Char('z'), |m| {
            m.super_ = true;
            m.shift = true;
        });
        e.apply(mac.edit_for(&cmd_z).unwrap(), &mut clip);
        assert_eq!(e.text(), "");
        e.apply(mac.edit_for(&cmd_shift_z).unwrap(), &mut clip);
        assert_eq!(e.text(), "a");
        let pc = Keymap::pc();
        e.apply(pc.edit_for(&k(Key::Char('z'), |m| m.ctrl = true)).unwrap(), &mut clip);
        e.apply(pc.edit_for(&k(Key::Char('y'), |m| m.ctrl = true)).unwrap(), &mut clip);
        assert_eq!(e.text(), "a");
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
