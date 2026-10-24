//! The editor's side of a language service: keeping the server in sync with
//! the buffer, its diagnostics, and the hover popup (the diagnostics and docs
//! at the character the pointer rests on).

use std::time::{Duration, Instant};

use ghostrealm_core::Rect;
use ghostrealm_lsp::text::{chars_of_units, units_of_chars};
use ghostrealm_lsp::types::{
    Diagnostic, DiagnosticSeverity, Hover, HoverContents, MarkedString, Position, Uri,
};
use ghostrealm_lsp::{Client, Event, RequestId};

use crate::plugin::{theme, Font, PaintCx};

/// How long the pointer rests on a character before its popup shows.
pub const HOVER_DWELL: Duration = Duration::from_millis(400);

const POPUP_BG: [u8; 3] = [34, 34, 42];
/// Widest a popup gets, in columns.
const POPUP_COLS: usize = 72;

/// A (line, character) position in the buffer.
pub type Pos = (usize, usize);

/// The popup at the pointer.
pub struct Popup {
    /// The character it is about.
    pub at: Pos,
    /// The span the pointer can move within without closing it (the hovered
    /// token), end exclusive.
    pub span: (Pos, Pos),
    /// Paragraphs, each in its colour.
    pub text: Vec<(String, [u8; 3])>,
}

pub struct Lang {
    client: Client,
    /// The buffer's edit count when last sent.
    synced: u64,
    diagnostics: Vec<Diagnostic>,
    /// The character the pointer rests on, and when its popup is due.
    rest: Option<Pos>,
    due: Option<Instant>,
    /// The hover request in flight, for the character it asked about.
    pending: Option<(RequestId, Pos)>,
    pub popup: Option<Popup>,
}

impl Lang {
    /// Open the document (`lines` at edit count `edits`) with `client`.
    pub fn new(mut client: Client, uri: Uri, language_id: &str, lines: &[String], edits: u64) -> Self {
        client.open(uri, language_id, lines.join("\n"));
        Lang {
            client,
            synced: edits,
            diagnostics: Vec::new(),
            rest: None,
            due: None,
            pending: None,
            popup: None,
        }
    }

    /// Send the buffer if it changed since last sent.
    pub fn sync(&mut self, lines: &[String], edits: u64) {
        if edits != self.synced {
            self.synced = edits;
            self.client.change(lines.join("\n"));
        }
    }

    /// Apply what the server sent. Returns whether to repaint.
    pub fn refresh(&mut self, lines: &[String]) -> bool {
        let mut changed = false;
        for event in self.client.poll() {
            match event {
                Event::Diagnostics(d) => {
                    self.diagnostics = d;
                    changed = true;
                }
                Event::Hover { request, hover } => {
                    let Some((_, at)) = self.pending.take_if(|(id, _)| *id == request) else {
                        continue;
                    };
                    self.popup = self.popup_at(lines, at, hover);
                    changed = true;
                }
                Event::Stopped => {
                    self.diagnostics.clear();
                    self.popup = None;
                    changed = true;
                }
            }
        }
        changed
    }

    /// The pointer is over character `at` (`None`: off the text). Returns
    /// whether to repaint.
    pub fn pointer(&mut self, at: Option<Pos>, now: Instant) -> bool {
        if let (Some(p), Some(at)) = (&self.popup, at) {
            if within(p.span, at) {
                return false;
            }
        }
        if at == self.rest {
            return false;
        }
        self.rest = at;
        self.due = at.map(|_| now + HOVER_DWELL);
        self.pending = None;
        self.popup.take().is_some()
    }

    /// Close the popup and forget where the pointer rests (the view scrolled,
    /// or the user typed or clicked). Returns whether to repaint.
    pub fn dismiss(&mut self) -> bool {
        self.rest = None;
        self.due = None;
        self.pending = None;
        self.popup.take().is_some()
    }

    /// When [`tick`](Self::tick) should run.
    pub fn deadline(&self) -> Option<Instant> {
        self.due
    }

    /// The pointer has rested long enough: show the diagnostics there and ask
    /// for docs (the popup grows when they arrive). Returns whether to repaint.
    pub fn tick(&mut self, lines: &[String], now: Instant) -> bool {
        if self.due.is_none_or(|d| now < d) {
            return false;
        }
        self.due = None;
        let Some(at) = self.rest else {
            return false;
        };
        self.popup = self.popup_at(lines, at, None);
        if self.client.is_ready() {
            let pos = to_lsp(lines, at, self.client.encoding());
            self.pending = self.client.hover(pos).map(|id| (id, at));
        }
        self.popup.is_some()
    }

    /// Each diagnostic's span (end exclusive, at least one character) and
    /// colour.
    pub fn underlines<'a>(&'a self, lines: &'a [String]) -> impl Iterator<Item = (Pos, Pos, [u8; 3])> + 'a {
        self.diagnostics.iter().map(move |d| {
            let (start, end) = self.span(lines, d.range);
            (start, end, severity_color(d.severity))
        })
    }

    /// A range in buffer positions, widened to one character when empty.
    fn span(&self, lines: &[String], r: ghostrealm_lsp::types::Range) -> (Pos, Pos) {
        let enc = self.client.encoding();
        let start = to_pos(lines, r.start, enc);
        let mut end = to_pos(lines, r.end, enc);
        if end <= start {
            end = (start.0, start.1 + 1);
        }
        (start, end)
    }

    /// The popup for `at`: the diagnostics covering it, then `hover`'s docs.
    fn popup_at(&self, lines: &[String], at: Pos, hover: Option<Hover>) -> Option<Popup> {
        let mut text = Vec::new();
        let mut span = (at, (at.0, at.1 + 1));
        for d in &self.diagnostics {
            let s = self.span(lines, d.range);
            if within(s, at) {
                text.push((d.message.clone(), severity_color(d.severity)));
                span = s;
            }
        }
        if let Some(h) = hover {
            if let Some(r) = h.range {
                span = self.span(lines, r);
            }
            text.push((hover_text(h.contents), theme::TEXT));
        }
        (!text.is_empty()).then_some(Popup { at, span, text })
    }
}

fn within((start, end): (Pos, Pos), at: Pos) -> bool {
    start <= at && at < end
}

fn severity_color(s: Option<DiagnosticSeverity>) -> [u8; 3] {
    match s {
        Some(DiagnosticSeverity::WARNING) => theme::DIAG_WARNING,
        Some(DiagnosticSeverity::INFORMATION | DiagnosticSeverity::HINT) => theme::DIAG_INFO,
        _ => theme::DIAG_ERROR,
    }
}

fn hover_text(contents: HoverContents) -> String {
    let marked = |m: MarkedString| match m {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(l) => l.value,
    };
    match contents {
        HoverContents::Markup(m) => m.value,
        HoverContents::Scalar(m) => marked(m),
        HoverContents::Array(ms) => ms.into_iter().map(marked).collect::<Vec<_>>().join("\n\n"),
    }
}

/// A buffer position for an LSP position (clamped to the buffer).
fn to_pos(lines: &[String], p: Position, enc: ghostrealm_lsp::Encoding) -> Pos {
    match lines.get(p.line as usize) {
        Some(line) => (p.line as usize, chars_of_units(line, p.character, enc)),
        None => {
            let last = lines.len().saturating_sub(1);
            (last, lines.get(last).map_or(0, |l| l.chars().count()))
        }
    }
}

/// The LSP position of a buffer position.
fn to_lsp(lines: &[String], (line, col): Pos, enc: ghostrealm_lsp::Encoding) -> Position {
    let character = lines.get(line).map_or(0, |l| units_of_chars(l, col, enc));
    Position { line: line as u32, character }
}

/// Paint `popup` below the character whose cell's top-left and row height
/// are `anchor` (above it when there's no room below), kept inside `area`.
pub fn paint_popup(cx: &mut PaintCx, popup: &Popup, anchor: (f32, f32, f32), area: Rect) {
    let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
    let pad = 6.0 * cx.ui.scale;
    let max_cols = (((area.w - 2.0 * pad) / cw).floor().max(10.0) as usize).min(POPUP_COLS);
    let mut rows: Vec<(String, [u8; 3])> = Vec::new();
    for (i, (para, color)) in popup.text.iter().enumerate() {
        if i > 0 {
            rows.push((String::new(), *color));
        }
        rows.extend(wrap(para, max_cols).into_iter().map(|l| (l, *color)));
    }
    let cols = rows.iter().map(|(l, _)| l.chars().count()).max().unwrap_or(0);
    let w = cols as f32 * cw + 2.0 * pad;
    let h = rows.len() as f32 * ch + 2.0 * pad;
    let below = anchor.1 + anchor.2;
    let y = if below + h <= area.y + area.h || anchor.1 - h < area.y {
        below
    } else {
        anchor.1 - h
    };
    let x = anchor.0.min(area.x + area.w - w).max(area.x);
    let r = Rect { x, y, w, h };
    cx.above(|cx| {
        cx.fill(r, POPUP_BG);
        cx.border(r, theme::INPUT_BORDER);
        for (i, (line, color)) in rows.iter().enumerate() {
            if !line.is_empty() {
                cx.label(line, Font::MONO, x + pad, y + pad + i as f32 * ch, r, *color);
            }
        }
    });
}

/// Word-wrap `text` to `cols` columns, keeping its line breaks; a word longer
/// than a line is split.
fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let (ll, wl) = (line.chars().count(), word.chars().count());
            if ll > 0 && ll + 1 + wl > cols {
                out.push(std::mem::take(&mut line));
            } else if ll > 0 {
                line.push(' ');
            }
            line.push_str(word);
            while line.chars().count() > cols {
                let rest: String = line.chars().skip(cols).collect();
                line = line.chars().take(cols).collect();
                out.push(std::mem::replace(&mut line, rest));
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::wrap;

    #[test]
    fn wraps_words_and_keeps_breaks() {
        assert_eq!(wrap("one two three", 7), vec!["one two", "three"]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }
}
