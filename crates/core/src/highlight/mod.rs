//! Syntax highlighting for the text surfaces (editor, file preview).
//!
//! A [`Highlighter`] turns one line of text, plus the state carried from the
//! line before it, into classified [`Span`]s. It knows nothing about colours:
//! each surface maps [`Token`]s onto its own palette. Highlighters are small
//! modules registered in [`ALL`] and found by file extension.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
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

/// How a language writes comments: the metadata a comment toggle needs, kept
/// beside the highlighter because neither LSP nor the editor knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommentSyntax {
    /// The token that starts a comment running to the end of the line (`#`).
    pub line: Option<&'static str>,
    /// The tokens that open and close a comment (`<!--`, `-->`).
    pub block: Option<(&'static str, &'static str)>,
}

/// A language with no highlighter yet, known so a comment toggle works on it
/// (a whole file, or a Markdown code block). A language that gains a
/// highlighter declares its syntax there and leaves this table.
#[derive(Debug)]
pub struct Language {
    /// The fence info words and file extensions (lowercase, no dot) naming it.
    pub names: &'static [&'static str],
    pub comment: CommentSyntax,
}

const SLASHES: CommentSyntax = CommentSyntax { line: Some("//"), block: Some(("/*", "*/")) };
const HASH: CommentSyntax = CommentSyntax { line: Some("#"), block: None };
const DASHES: CommentSyntax = CommentSyntax { line: Some("--"), block: None };
const MARKUP: CommentSyntax = CommentSyntax { line: None, block: Some(("<!--", "-->")) };

/// Every [`Language`]. JSON is absent: it has no comments.
pub static LANGUAGES: &[Language] = &[
    Language { names: &["rust", "rs"], comment: SLASHES },
    Language { names: &["c", "h"], comment: SLASHES },
    Language { names: &["cpp", "c++", "cc", "cxx", "hpp", "hh", "hxx"], comment: SLASHES },
    Language { names: &["java"], comment: SLASHES },
    Language { names: &["kotlin", "kt", "kts"], comment: SLASHES },
    Language { names: &["swift"], comment: SLASHES },
    Language { names: &["go", "golang"], comment: SLASHES },
    Language { names: &["zig"], comment: CommentSyntax { line: Some("//"), block: None } },
    Language { names: &["javascript", "js", "mjs", "cjs", "jsx"], comment: SLASHES },
    Language { names: &["typescript", "ts", "mts", "cts", "tsx"], comment: SLASHES },
    Language { names: &["csharp", "cs", "c#"], comment: SLASHES },
    Language { names: &["scss", "less"], comment: SLASHES },
    Language { names: &["css"], comment: CommentSyntax { line: None, block: Some(("/*", "*/")) } },
    Language { names: &["python", "py"], comment: HASH },
    Language { names: &["sh", "bash", "zsh", "shell", "fish"], comment: HASH },
    Language { names: &["ruby", "rb"], comment: HASH },
    Language { names: &["perl", "pl"], comment: HASH },
    Language { names: &["yaml", "yml"], comment: HASH },
    Language { names: &["elixir", "ex", "exs"], comment: HASH },
    Language { names: &["nix"], comment: HASH },
    Language { names: &["sql"], comment: DASHES },
    Language { names: &["lua"], comment: DASHES },
    Language { names: &["haskell", "hs"], comment: DASHES },
    Language { names: &["html", "htm", "xml", "svg"], comment: MARKUP },
];

/// The [`Language`] named `word` (case-insensitive).
pub fn language(word: &str) -> Option<&'static Language> {
    let word = word.to_ascii_lowercase();
    LANGUAGES.iter().find(|l| l.names.contains(&word.as_str()))
}

/// The [`Language`] for `path`'s extension.
pub fn language_for_path(path: &Path) -> Option<&'static Language> {
    language(path.extension()?.to_str()?)
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

    /// Whether a line's classification depends on the lines before it. When
    /// false, every line enters with state 0 and no [`StateCache`] is needed.
    fn carries_state(&self) -> bool {
        true
    }

    /// Classify `line` entering with `state`. The spans are contiguous from
    /// char 0 to the line's end (none for an empty line). Also returns the
    /// state the next line enters with.
    fn line(&self, line: &str, state: State) -> (Vec<Span>, State);

    /// How this language writes comments; none by default (no comment toggle).
    fn comment(&self) -> CommentSyntax {
        CommentSyntax::default()
    }

    /// How comments are written on `line`, entering with `state`: this
    /// language's [`Self::comment`] unless the line belongs to another
    /// language embedded in it (a Markdown code block's).
    fn comment_on(&self, _line: &str, _state: State) -> CommentSyntax {
        self.comment()
    }

    /// The chars of `line` (entering with `state`) that belong to the
    /// container embedding [`Self::comment_on`]'s language, which a comment
    /// goes after: a quoted code block's `>`. 0 by default.
    fn comment_lead(&self, _line: &str, _state: State) -> usize {
        0
    }

    /// Whether `line`, entering with `state_in` and leaving with `state_out`,
    /// belongs to a block drawn as a unit (a fenced code block, including its
    /// fences).
    fn block(&self, _line: &str, _state_in: State, _state_out: State) -> bool {
        false
    }
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
///
/// Report where edits land with [`Self::dirty_from`] so a new edit count only
/// re-checks lines from there on; without a report the whole prefix is
/// re-checked (a hash per line).
#[derive(Default)]
pub struct StateCache {
    entries: Vec<Entry>,
    /// The first `verified` entries match the document at edit count `edits`.
    verified: usize,
    edits: Option<u64>,
    /// The first line changed since the last edit count seen, if reported.
    dirty: Option<usize>,
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
    /// Lines before `row` are unchanged by the edits since the last query.
    pub fn dirty_from(&mut self, row: usize) {
        self.dirty = Some(self.dirty.map_or(row, |d| d.min(row)));
    }

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
            self.verified = self.verified.min(self.dirty.take().unwrap_or(0));
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

/// Spans of recently seen `(line, entering state)` pairs, so lines that are
/// still on screen aren't re-highlighted every frame.
#[derive(Default)]
pub struct SpanCache {
    map: HashMap<(u64, State), (Vec<Span>, State)>,
    highlighter: &'static str,
}

/// Entries kept before the cache starts over (a screenful is far fewer).
const SPAN_CACHE_CAP: usize = 4096;

impl SpanCache {
    pub fn spans(&mut self, h: &dyn Highlighter, line: &str, state: State) -> &[Span] {
        self.line(h, line, state).0
    }

    /// The spans and the state the next line enters with.
    pub fn line(&mut self, h: &dyn Highlighter, line: &str, state: State) -> (&[Span], State) {
        if self.highlighter != h.name() || self.map.len() >= SPAN_CACHE_CAP {
            self.highlighter = h.name();
            self.map.clear();
        }
        let (spans, next) = self.map.entry((hash_line(line), state)).or_insert_with(|| h.line(line, state));
        (spans, *next)
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

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how many lines it is asked to classify.
    struct Counting(AtomicUsize);

    impl Highlighter for Counting {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn extensions(&self) -> &'static [&'static str] {
            &[]
        }

        fn line(&self, _line: &str, state: State) -> (Vec<Span>, State) {
            self.0.fetch_add(1, Ordering::Relaxed);
            (vec![Span { start: 0, end: 1, token: Token::Plain }], state)
        }
    }

    #[test]
    fn a_dirty_hint_limits_the_lines_rechecked_after_an_edit() {
        let h = Counting(AtomicUsize::new(0));
        let mut lines: Vec<String> = (0..1000).map(|i| format!("line {i}")).collect();
        let mut cache = StateCache::default();
        cache.state_in(&h, &lines, 0, 999);
        assert_eq!(h.0.swap(0, Ordering::Relaxed), 999, "first pass");

        lines[900] = "edited".into();
        cache.dirty_from(900);
        cache.state_in(&h, &lines, 1, 999);
        assert_eq!(h.0.swap(0, Ordering::Relaxed), 1, "only the edited line is classified again");

        // Without a hint the prefix is re-verified by hash, never trusted.
        lines[10] = "edited".into();
        cache.state_in(&h, &lines, 2, 999);
        assert_eq!(h.0.swap(0, Ordering::Relaxed), 1);
        // A hint is the minimum over the edits since the last query.
        lines[20] = "again".into();
        lines[30] = "again".into();
        cache.dirty_from(30);
        cache.dirty_from(20);
        cache.state_in(&h, &lines, 3, 999);
        assert_eq!(h.0.swap(0, Ordering::Relaxed), 2);
    }

    #[test]
    fn span_cache_classifies_each_line_state_pair_once() {
        let h = Counting(AtomicUsize::new(0));
        let mut cache = SpanCache::default();
        for _ in 0..3 {
            assert_eq!(cache.spans(&h, "a", 0).len(), 1);
            cache.spans(&h, "b", 0);
            cache.spans(&h, "a", 1);
        }
        assert_eq!(h.0.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn span_cache_returns_the_exit_state() {
        let md = for_extension("md").unwrap();
        let mut cache = SpanCache::default();
        let (_, open) = cache.line(md, "```toml", 0);
        assert_ne!(open, 0);
        assert_eq!(cache.line(md, "```", open).1, 0);
    }

    #[test]
    fn languages_declare_their_comment_syntax() {
        let toml = for_extension("toml").unwrap().comment();
        assert_eq!((toml.line, toml.block), (Some("#"), None));
        let md = for_extension("md").unwrap().comment();
        assert_eq!((md.line, md.block), (None, Some(("<!--", "-->"))));
        assert_eq!(Fences.comment(), CommentSyntax::default());
    }

    #[test]
    fn a_markdown_code_block_takes_its_languages_comment_syntax() {
        let md = for_extension("md").unwrap();
        let toml = md.line("```toml", 0).1;
        assert_eq!(md.comment_on("a = 1", toml), for_extension("toml").unwrap().comment());
        assert_eq!(md.comment_on("```", toml), md.comment(), "the closing fence is Markdown");
        let rust = md.line("```rust", 0).1;
        assert_eq!(md.comment_on("let a = 1;", rust), language("rust").unwrap().comment);
        for info in ["rust,ignore", "{.rust .numberLines}"] {
            let st = md.line(&format!("```{info}"), 0).1;
            assert_eq!(md.comment_on("x", st), language("rust").unwrap().comment, "{info}");
        }
        let quoted = md.line("> ```rust", 0).1;
        assert_eq!(md.comment_on("> let a = 1;", quoted), language("rust").unwrap().comment);
        assert_eq!(md.comment_lead("> let a = 1;", quoted), 1);
        assert_eq!(md.comment_lead(" > >  x", quoted), 4, "nested quotes, before the indent");
        assert_eq!(md.comment_lead("> ```", quoted), 0, "the closing fence is Markdown's");
        assert_eq!(md.comment_lead("a = 1", toml), 0);
        let quoted_item = md.line("> - ```rust", 0).1;
        assert_eq!(md.comment_on(">   a();", quoted_item), language("rust").unwrap().comment);
        assert_eq!(md.comment_lead(">   a();", quoted_item), 1, "a list item in a quote");
        let listed = md.line("- ```toml", 0).1;
        assert_eq!(md.comment_lead("  a = 1", listed), 0, "a list item's indent is indent");
        let unknown = md.line("```zzz", 0).1;
        assert_eq!(md.comment_on("x", unknown), CommentSyntax::default());
        assert_eq!(md.comment_on("text", 0), md.comment());
    }

    #[test]
    fn languages_without_a_highlighter_are_found_by_name_or_extension() {
        assert_eq!(language("rs").unwrap().comment, SLASHES);
        assert_eq!(language("Rust").unwrap().comment, SLASHES);
        assert_eq!(language_for_path(Path::new("a/b.py")).unwrap().comment, HASH);
        assert_eq!(language("sql").unwrap().comment, DASHES);
        assert!(language("json").is_none(), "JSON has no comments");
        assert!(language("toml").is_none() && language_for_path(Path::new("Makefile")).is_none());
    }

    #[test]
    fn the_language_table_leaves_highlighted_languages_to_their_highlighters() {
        for l in LANGUAGES {
            assert_ne!(l.comment, CommentSyntax::default(), "{:?}", l.names);
            for n in l.names {
                assert_eq!(*n, n.to_ascii_lowercase());
                assert!(
                    ALL.iter().all(|h| h.name() != *n && !h.extensions().contains(n)),
                    "{n} has a highlighter: declare its comments there"
                );
                assert_eq!(LANGUAGES.iter().filter(|o| o.names.contains(n)).count(), 1, "{n} is listed once");
            }
        }
    }

    #[test]
    fn only_stateful_highlighters_need_a_state_cache() {
        assert!(!for_extension("toml").unwrap().carries_state());
        assert!(for_extension("md").unwrap().carries_state());
    }
}
