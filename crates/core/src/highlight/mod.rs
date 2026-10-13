//! Syntax highlighting for the text surfaces (editor, file preview).
//!
//! A [`Highlighter`] turns one line of text, plus the state carried from the
//! line before it, into classified [`Span`]s. It knows nothing about colours:
//! each surface maps [`Token`]s onto its own palette. Highlighters are small
//! modules registered in [`ALL`] and found by file extension.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

pub mod markdown;
pub mod toml;

/// What a stretch of text is, independent of how it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Plain,
    Comment,
    /// A section title: a TOML `[table]`, a Markdown heading's text.
    Heading,
    Key,
    String,
    Number,
    /// Punctuation that only marks structure: a heading's `#`, emphasis
    /// asterisks, code fences and backticks, list bullets, `>`, link brackets.
    Marker,
    /// Text between strong-emphasis markers (render bold).
    Strong,
    /// Text between emphasis markers (render italic).
    Emphasis,
    Code,
    /// A link's visible text.
    Link,
    /// A link's target.
    Url,
    Quote,
}

/// A run of one [`Token`] in a line, in char indices (`end` exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub token: Token,
}

/// What a highlighter carries from the end of one line to the start of the
/// next (e.g. "inside a fenced block"). Opaque to callers; `0` starts a
/// document.
pub type State = u32;

pub trait Highlighter: Sync {
    /// A stable name; a cache is invalidated when the name changes.
    fn name(&self) -> &'static str;

    /// Lowercase file extensions (no dot) this highlighter claims.
    fn extensions(&self) -> &'static [&'static str];

    /// Classify `line` entering with `state`. The spans are contiguous from
    /// char 0 to the line's end (none for an empty line). Also returns the
    /// state the next line enters with.
    fn line(&self, line: &str, state: State) -> (Vec<Span>, State);
}

/// Every registered highlighter.
pub static ALL: &[&dyn Highlighter] = &[&toml::Toml, &markdown::Markdown];

/// The highlighter claiming `ext` (case-insensitive, without the dot).
pub fn for_extension(ext: &str) -> Option<&'static dyn Highlighter> {
    let ext = ext.to_ascii_lowercase();
    ALL.iter().copied().find(|h| h.extensions().contains(&ext.as_str()))
}

/// The highlighter for `path`'s extension.
pub fn for_path(path: &Path) -> Option<&'static dyn Highlighter> {
    for_extension(path.extension()?.to_str()?)
}

/// Spans for every line of a document, threading the state through.
pub fn highlight_lines<S: AsRef<str>>(h: &dyn Highlighter, lines: &[S]) -> Vec<Vec<Span>> {
    let mut state = 0;
    lines
        .iter()
        .map(|l| {
            let (spans, next) = h.line(l.as_ref(), state);
            state = next;
            spans
        })
        .collect()
}

/// Merge a per-char classification into spans.
pub fn runs(tokens: &[Token]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (i, &token) in tokens.iter().enumerate() {
        match spans.last_mut() {
            Some(s) if s.token == token => s.end = i + 1,
            _ => spans.push(Span { start: i, end: i + 1, token }),
        }
    }
    spans
}

/// The state each line of an edited document starts in, computed on demand
/// and kept across edits so only changed lines are re-highlighted.
#[derive(Default)]
pub struct StateCache {
    entries: Vec<Entry>,
    /// The first `verified` entries match the document at edit count `edits`.
    verified: usize,
    edits: Option<u64>,
    highlighter: &'static str,
}

struct Entry {
    hash: u64,
    state_in: State,
    state_out: State,
}

fn hash_line(line: &str) -> u64 {
    let mut h = DefaultHasher::new();
    line.hash(&mut h);
    h.finish()
}

impl StateCache {
    /// The state `line` of `lines` (at edit count `edits`) enters with. Cheap
    /// when asked in ascending order within one edit count.
    pub fn state_in(&mut self, h: &dyn Highlighter, lines: &[String], edits: u64, line: usize) -> State {
        if self.highlighter != h.name() {
            self.highlighter = h.name();
            self.entries.clear();
            self.verified = 0;
        }
        if self.edits != Some(edits) {
            self.edits = Some(edits);
            self.verified = 0;
        }
        self.entries.truncate(lines.len());
        self.verified = self.verified.min(self.entries.len());
        let want = line.min(lines.len());
        while self.verified < want {
            let i = self.verified;
            let state_in = if i == 0 { 0 } else { self.entries[i - 1].state_out };
            let hash = hash_line(&lines[i]);
            if !matches!(self.entries.get(i), Some(e) if e.hash == hash && e.state_in == state_in) {
                let entry = Entry { hash, state_in, state_out: h.line(&lines[i], state_in).1 };
                match self.entries.get_mut(i) {
                    Some(slot) => *slot = entry,
                    None => self.entries.push(entry),
                }
            }
            self.verified += 1;
        }
        match want {
            0 => 0,
            w => self.entries[w - 1].state_out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_finds_by_extension() {
        assert_eq!(for_extension("toml").map(|h| h.name()), Some("toml"));
        assert_eq!(for_extension("TOML").map(|h| h.name()), Some("toml"));
        assert_eq!(for_path(Path::new("a/b/Cargo.toml")).map(|h| h.name()), Some("toml"));
        assert_eq!(for_extension("MD").map(|h| h.name()), Some("markdown"));
        assert_eq!(for_path(Path::new("a/b/README.markdown")).map(|h| h.name()), Some("markdown"));
        assert!(for_extension("rs").is_none());
        assert!(for_path(Path::new("Makefile")).is_none());
    }

    #[test]
    fn runs_merge_equal_neighbours() {
        use Token::*;
        let spans = runs(&[Plain, Plain, Key, Plain]);
        assert_eq!(
            spans,
            vec![
                Span { start: 0, end: 2, token: Plain },
                Span { start: 2, end: 3, token: Key },
                Span { start: 3, end: 4, token: Plain },
            ]
        );
        assert!(runs(&[]).is_empty());
    }

    /// Lines of ``` toggle a "fenced" state.
    struct Fences;

    impl Highlighter for Fences {
        fn name(&self) -> &'static str {
            "fences"
        }

        fn extensions(&self) -> &'static [&'static str] {
            &[]
        }

        fn line(&self, line: &str, state: State) -> (Vec<Span>, State) {
            (Vec::new(), if line == "```" { 1 - state } else { state })
        }
    }

    #[test]
    fn state_cache_matches_a_full_pass_and_follows_edits() {
        let h: &dyn Highlighter = &Fences;
        let mut lines: Vec<String> = ["a", "```", "x", "y", "```", "b"].iter().map(|s| s.to_string()).collect();
        let mut cache = StateCache::default();
        let full = |lines: &[String]| -> Vec<State> {
            let mut st = 0;
            lines
                .iter()
                .map(|l| {
                    let before = st;
                    st = h.line(l, st).1;
                    before
                })
                .collect()
        };
        for edits in 0..3u64 {
            if edits == 1 {
                // Close the fence one line earlier.
                lines[3] = "```".into();
            }
            if edits == 2 {
                lines.insert(0, "```".into());
            }
            let want = full(&lines);
            for (i, w) in want.iter().enumerate() {
                assert_eq!(cache.state_in(h, &lines, edits, i), *w, "edit {edits}, line {i}");
            }
        }
        // Lines past the end clamp.
        let end = cache.state_in(h, &lines, 2, 99);
        assert_eq!(end, h.line(lines.last().unwrap(), *full(&lines).last().unwrap()).1);
    }
}
