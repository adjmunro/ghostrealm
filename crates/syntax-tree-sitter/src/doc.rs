//! A tree-sitter [`SyntaxDoc`]: parses a small document on the UI thread and
//! a large one on a worker, and paints highlight spans from its query.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;

use ghostrealm_core::fs_tree::Waker;
use ghostrealm_core::highlight::{runs, Code, Span, Token};
use ghostrealm_core::syntax::{self, Point, SyntaxDoc, TextEdit, TextEdits};
use tree_sitter::{InputEdit, Node, Parser, QueryCursor, StreamingIterator, Tree};

use crate::Loaded;

/// A document this long (in bytes) or longer parses on a worker: a parse of
/// 64 KiB of Kotlin takes about 10 ms, well under a frame.
pub const WORKER_BYTES: usize = 64 << 10;

pub(crate) struct TreeDoc {
    lang: Arc<Loaded>,
    parser: Parser,
    /// The tree, with its nodes shifted by every edit since it was parsed.
    tree: Option<Tree>,
    /// Whether the text changed since `tree` was parsed (or there is none).
    behind: bool,
    /// Highlighted rows, kept until the tree or the text changes.
    rows: HashMap<usize, Vec<Span>>,
    job: Option<Job>,
    waker: Option<Waker>,
    /// Tests hold this to keep a worker's tree back, so they can edit while
    /// it runs whatever the machine's speed.
    #[cfg(test)]
    gate: Option<Arc<std::sync::Mutex<()>>>,
}

/// A parse on the worker, and the edits made since it read its text, to
/// shift its tree by when it lands.
struct Job {
    rx: Receiver<Tree>,
    queued: Vec<TextEdit>,
}

impl TreeDoc {
    pub(crate) fn new(lang: Arc<Loaded>, waker: Option<Waker>) -> Self {
        let mut parser = Parser::new();
        // `Grammar::loaded` compiled a query against this language, so its
        // ABI is one this tree-sitter accepts.
        parser.set_language(&lang.language).expect("a loaded grammar's ABI is supported");
        TreeDoc {
            lang,
            parser,
            tree: None,
            behind: true,
            rows: HashMap::new(),
            job: None,
            waker,
            #[cfg(test)]
            gate: None,
        }
    }

    /// Parse a copy of `lines` on a worker, from the current tree.
    fn spawn(&mut self, lines: &[String]) {
        let (tx, rx) = mpsc::channel();
        let (language, old, text) = (self.lang.language.clone(), self.tree.clone(), syntax::text(lines));
        let waker = self.waker.clone();
        #[cfg(test)]
        let gate = self.gate.clone();
        std::thread::spawn(move || {
            #[cfg(test)]
            let _held = gate.as_ref().map(|g| g.lock());
            let mut parser = Parser::new();
            let tree = parser.set_language(&language).ok().and_then(|()| parser.parse(&text, old.as_ref()));
            if tree.is_some_and(|t| tx.send(t).is_ok()) {
                if let Some(w) = waker {
                    w();
                }
            }
        });
        self.job = Some(Job { rx, queued: Vec::new() });
    }
}

impl SyntaxDoc for TreeDoc {
    fn name(&self) -> &'static str {
        self.lang.name
    }

    fn update(&mut self, edits: TextEdits, lines: &[String]) -> bool {
        let changed = !edits.is_empty();
        if edits.reset {
            (self.tree, self.job) = (None, None);
        }
        for e in &edits.edits {
            if let Some(tree) = &mut self.tree {
                tree.edit(&input(e));
            }
            if let Some(job) = &mut self.job {
                job.queued.push(*e);
            }
        }
        self.behind |= changed;
        if changed {
            self.rows.clear();
        }
        if !self.behind || (self.job.is_some() && syntax::len(lines) >= WORKER_BYTES) {
            return changed;
        }
        if syntax::len(lines) >= WORKER_BYTES {
            self.spawn(lines);
            return changed;
        }
        // Newer than anything a worker still running would bring.
        self.job = None;
        let tree = self.parser.parse_with_options(
            &mut |_, p| syntax::chunk(lines, (p.row, p.column)),
            self.tree.as_ref(),
            None,
        );
        self.behind = tree.is_none();
        self.tree = tree.or(self.tree.take());
        self.rows.clear();
        true
    }

    fn poll(&mut self) -> bool {
        let Some(job) = &mut self.job else { return false };
        match job.rx.try_recv() {
            Ok(mut tree) => {
                for e in &job.queued {
                    tree.edit(&input(e));
                }
                self.behind = !job.queued.is_empty();
                (self.tree, self.job) = (Some(tree), None);
                self.rows.clear();
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.job = None;
                false
            }
        }
    }

    fn highlight(&mut self, lines: &[String], rows: Range<usize>) {
        let rows = rows.start..rows.end.min(lines.len());
        self.rows.retain(|r, _| rows.contains(r));
        let Some(tree) = &self.tree else { return };
        let todo = |r: &usize| !self.rows.contains_key(r);
        let Some(lo) = rows.clone().find(todo) else { return };
        let hi = rows.clone().rev().find(todo).unwrap_or(lo) + 1;
        // starts[i]: the byte row lo + i starts at, for i in 0..=hi - lo.
        let mut starts = vec![syntax::row_start(lines, lo)];
        for l in &lines[lo..hi] {
            starts.push(starts[starts.len() - 1] + l.len() + 1);
        }
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(starts[0]..starts[hi - lo]);
        let text = |n: Node| syntax::chunks(lines, point(n.start_position()), point(n.end_position()));
        let mut caps = cursor.captures(&self.lang.query, tree.root_node(), text);
        let mut found = Vec::new();
        while let Some((m, i)) = caps.next() {
            let c = m.captures()[*i];
            found.push(Capture {
                bytes: c.node.start_byte()..c.node.end_byte(),
                pattern: m.pattern_index,
                code: self.lang.codes[c.index as usize],
            });
        }
        let mut tokens: Vec<Vec<Token>> = lines[lo..hi].iter().map(|l| vec![Token::Plain; l.len()]).collect();
        paint(winners(found), &starts, &mut tokens);
        for (r, t) in (lo..hi).zip(tokens) {
            self.rows.entry(r).or_insert_with(|| runs(&lines[r].char_indices().map(|(b, _)| t[b]).collect::<Vec<_>>()));
        }
    }

    fn line(&self, ln: usize) -> &[Span] {
        self.rows.get(&ln).map_or(&[], Vec::as_slice)
    }

    fn working(&self) -> bool {
        self.job.is_some()
    }
}

/// A node a query captured, and the colour its capture gives (`None`:
/// plain).
#[derive(Clone, Debug, PartialEq)]
struct Capture {
    bytes: Range<usize>,
    pattern: usize,
    code: Option<Code>,
}

/// The capture that colours each captured range: of those of one range, a
/// coloured one before a plain one (a catch-all `@variable` doesn't blank a
/// function's name), then the first pattern. Sorted outer first, so painting
/// in order lets an inner node (even a plain one) paint over its parent.
fn winners(found: Vec<Capture>) -> Vec<(Range<usize>, Option<Code>)> {
    let mut best: HashMap<(usize, usize), Capture> = HashMap::new();
    for c in found {
        let key = (c.bytes.start, c.bytes.end);
        let rank = |c: &Capture| (c.code.is_none(), c.pattern);
        match best.get(&key) {
            Some(b) if rank(b) <= rank(&c) => {}
            _ => {
                best.insert(key, c);
            }
        }
    }
    let mut out: Vec<_> = best.into_values().map(|c| (c.bytes, c.code)).collect();
    out.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
    out
}

/// Paint `ranges` in order onto `tokens`, one byte each per row, rows
/// starting at the bytes `starts` gives (one more than `tokens`, the last
/// past the end).
fn paint(ranges: Vec<(Range<usize>, Option<Code>)>, starts: &[usize], tokens: &mut [Vec<Token>]) {
    for (bytes, code) in ranges {
        let token = code.map_or(Token::Plain, Token::Code);
        let mut i = starts.partition_point(|&s| s <= bytes.start).saturating_sub(1);
        while i < tokens.len() && starts[i] < bytes.end {
            let row = &mut tokens[i];
            let a = bytes.start.saturating_sub(starts[i]).min(row.len());
            let b = (bytes.end - starts[i]).min(row.len());
            row[a..b].fill(token);
            i += 1;
        }
    }
}

fn point(p: tree_sitter::Point) -> Point {
    (p.row, p.column)
}

fn input(e: &TextEdit) -> InputEdit {
    let p = |(row, column): Point| tree_sitter::Point { row, column };
    InputEdit {
        start_byte: e.start_byte,
        old_end_byte: e.old_end_byte,
        new_end_byte: e.new_end_byte,
        start_position: p(e.start),
        old_end_position: p(e.old_end),
        new_end_position: p(e.new_end),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GRAMMARS, open};
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn lines(s: &str) -> Vec<String> {
        s.split('\n').map(str::to_string).collect()
    }

    fn doc(name: &str) -> TreeDoc {
        let g = GRAMMARS.iter().find(|g| g.name == name).expect("a grammar");
        TreeDoc::new(g.loaded().expect("compiles"), None)
    }

    /// `src` in `lang`, parsed and highlighted whole.
    fn highlighted(lang: &str, src: &str) -> (TreeDoc, Vec<String>) {
        let (mut d, l) = (doc(lang), lines(src));
        d.update(TextEdits::reset(), &l);
        d.highlight(&l, 0..l.len());
        (d, l)
    }

    /// Each row's token per char.
    fn per_char(d: &TreeDoc, l: &[String]) -> Vec<Vec<Token>> {
        (0..l.len())
            .map(|r| {
                let mut t = vec![Token::Plain; l[r].chars().count()];
                for s in d.line(r) {
                    t[s.start..s.end].fill(s.token);
                }
                t
            })
            .collect()
    }

    /// The token `needle`'s first occurrence (searching by row) is drawn
    /// in, every char of it alike.
    fn token(d: &TreeDoc, l: &[String], needle: &str) -> Token {
        let tokens = per_char(d, l);
        let (r, b) = l
            .iter()
            .enumerate()
            .find_map(|(r, line)| line.find(needle).map(|b| (r, b)))
            .unwrap_or_else(|| panic!("{needle:?} in the text"));
        let col = l[r][..b].chars().count();
        let t = &tokens[r][col..col + needle.chars().count()];
        assert!(t.iter().all(|x| *x == t[0]), "{needle:?} is drawn in one token: {t:?}");
        t[0]
    }

    fn check(lang: &str, src: &str, expect: &[(&str, Token)]) {
        let (d, l) = highlighted(lang, src);
        assert!(!d.tree.as_ref().unwrap().root_node().has_error(), "{lang} parses without error:\n{src}");
        for (needle, t) in expect {
            assert_eq!(token(&d, &l, needle), *t, "{lang}: {needle:?}");
        }
    }

    use Code::{Attribute, Comment, Constant, Function, Keyword, Number, Operator, Property, Punctuation, Type};
    const PLAIN: Token = Token::Plain;
    fn c(code: Code) -> Token {
        Token::Code(code)
    }

    const KOTLIN: &str = r#"package a.b

import kotlin.math.max

/* block */
@JvmStatic
fun main(args: Array<String>) {
    val x = 42 // c
    val s = "a$x and ${x + 1}"
    when (x) {
        is Int -> println(s)
        else -> MAX_SIZE.toString()
    }
}

data class P(val name: String, var n: Int = 0)
"#;

    #[test]
    fn kotlin_is_highlighted() {
        check(
            "kotlin",
            KOTLIN,
            &[
                ("package", c(Keyword)),
                ("/* block */", c(Comment)),
                ("@JvmStatic", c(Attribute)),
                ("fun", c(Keyword)),
                ("main", c(Function)),
                ("String", c(Type)),
                ("val", c(Keyword)),
                ("42", c(Number)),
                ("// c", c(Comment)),
                ("\"a", c(Code::String)),
                ("when", c(Keyword)),
                ("is", c(Keyword)),
                ("println", c(Function)),
                ("MAX_SIZE", c(Constant)),
                ("toString", c(Function)),
                ("data", c(Keyword)),
                ("name", c(Property)),
                ("{", c(Punctuation)),
                ("->", c(Operator)),
            ],
        );
    }

    #[test]
    fn a_gradle_script_is_highlighted() {
        let src = "plugins {\n    kotlin(\"jvm\") version \"2.0.0\"\n}\n\ndependencies {\n    implementation(\"org.x:y:1.0\")\n}\n";
        check(
            "kotlin",
            src,
            &[("plugins", c(Function)), ("\"jvm\"", c(Code::String)), ("implementation", c(Function)), ("\"2.0.0\"", c(Code::String))],
        );
    }

    #[test]
    fn java_is_highlighted() {
        let src = r#"package a;

import java.util.List;

public class Foo extends Bar {
    static final int MAX = 42;
    // c
    @Override
    void run(String s) {
        System.out.println("hi" + s);
        this.x = 1;
    }
}
"#;
        check(
            "java",
            src,
            &[
                ("public", c(Keyword)),
                ("class", c(Keyword)),
                ("Foo", c(Type)),
                ("42", c(Number)),
                ("MAX", c(Constant)),
                ("// c", c(Comment)),
                ("@Override", c(Attribute)),
                ("run", c(Function)),
                ("String", c(Type)),
                ("println", c(Function)),
                ("\"hi\"", c(Code::String)),
                ("this", c(Keyword)),
            ],
        );
    }

    #[test]
    fn rust_is_highlighted() {
        let src = r#"use std::fmt;

const MAX: usize = 42;

#[derive(Debug)]
struct S { field: u8 }

fn main() {
    let x = vec![1];
    x.len(); // c
    println!("hi {}", MAX);
}
"#;
        check(
            "rust",
            src,
            &[
                ("use", c(Keyword)),
                ("MAX", c(Constant)),
                ("42", c(Number)),
                ("derive", c(Attribute)),
                ("S", c(Type)),
                ("field", c(Property)),
                ("fn", c(Keyword)),
                ("main", c(Function)),
                ("len", c(Function)),
                ("// c", c(Comment)),
                ("\"hi {}\"", c(Code::String)),
            ],
        );
    }

    #[test]
    fn an_inner_node_paints_over_the_one_around_it() {
        let (d, l) = highlighted("kotlin", "val s = \"${a + 1}\"");
        assert_eq!(token(&d, &l, "+"), c(Operator), "inside the string");
        assert_eq!(token(&d, &l, "1"), c(Number));
        assert_eq!(per_char(&d, &l)[0][11], PLAIN, "an interpolation's `a` is code");
        assert_eq!(per_char(&d, &l)[0][8], c(Code::String), "the string's opening quote");
        assert_eq!(per_char(&d, &l)[0][17], c(Code::String), "its closing quote");
    }

    #[test]
    fn captures_of_one_range_rank_coloured_then_first_and_paint_outer_first() {
        let cap = |r: Range<usize>, pattern, code| Capture { bytes: r, pattern, code };
        let found = vec![
            cap(0..3, 1, Some(Keyword)),
            cap(0..10, 0, Some(Comment)),
            // One range: plain first pattern, then two coloured.
            cap(5..7, 0, None),
            cap(5..7, 3, Some(Type)),
            cap(5..7, 2, Some(Function)),
            // Inside the comment, plain.
            cap(8..9, 4, None),
        ];
        let w = winners(found);
        assert_eq!(
            w,
            vec![(0..10, Some(Comment)), (0..3, Some(Keyword)), (5..7, Some(Function)), (8..9, None)],
            "same start: the outer first"
        );
        // Two rows: bytes 0..6 and 7..12 (a `\n` at 6).
        let mut tokens = vec![vec![PLAIN; 6], vec![PLAIN; 5]];
        paint(w, &[0, 7, 13], &mut tokens);
        let k = c(Keyword);
        let (cm, f) = (c(Comment), c(Function));
        assert_eq!(tokens, vec![vec![k, k, k, cm, cm, f], vec![cm, PLAIN, cm, PLAIN, PLAIN]]);
    }

    #[test]
    fn spans_are_in_chars() {
        let (d, l) = highlighted("kotlin", "val é🦀 = 1 // x");
        assert_eq!(
            d.line(0),
            &[
                Span { start: 0, end: 3, token: c(Keyword) },
                Span { start: 3, end: 7, token: PLAIN },
                Span { start: 7, end: 8, token: c(Operator) },
                Span { start: 8, end: 9, token: PLAIN },
                Span { start: 9, end: 10, token: c(Number) },
                Span { start: 10, end: 11, token: PLAIN },
                Span { start: 11, end: 15, token: c(Comment) },
            ]
        );
        assert_eq!(l[0].chars().count(), 15);
    }

    #[test]
    fn a_block_comment_opened_above_the_rows_shown_colours_them() {
        let src = format!("/*\n{}*/\nval x = 1", "val y = 2\n".repeat(50));
        let (mut d, l) = (doc("kotlin"), lines(&src));
        d.update(TextEdits::reset(), &l);
        d.highlight(&l, 20..22);
        assert_eq!(d.line(20), &[Span { start: 0, end: 9, token: c(Comment) }]);
        assert!(d.line(0).is_empty(), "a row not asked for");
        d.highlight(&l, 52..53);
        assert_eq!(d.line(52)[0].token, c(Keyword));
        assert!(d.line(20).is_empty(), "rows no longer shown are dropped");
    }

    const KOTLIN_2: [&str; 3] = [
        "context(ctx: Logger) fun Foo.bar() {}",
        "fun f(x: Any) = when (x) {\n    is String if x.isEmpty() -> 1\n    else -> 2\n}",
        "val s = $$\"\"\"a $$b $c\"\"\"",
    ];

    #[test]
    fn kotlin_2_constructs_parse_without_error() {
        for src in KOTLIN_2 {
            let (d, l) = highlighted("kotlin", src);
            assert!(!d.tree.as_ref().unwrap().root_node().has_error(), "{src}");
            assert!(d.line(0).iter().any(|s| s.token == c(Keyword)), "{src}: {:?}", d.line(0));
            assert_eq!(token(&d, &l, &src[..3]), c(Keyword), "{src}");
        }
        let (d, l) = highlighted("kotlin", KOTLIN_2[1]);
        assert_eq!(token(&d, &l, "String"), c(Type));
        assert_eq!(token(&d, &l, "if"), c(Keyword));
        let (d, l) = highlighted("kotlin", KOTLIN_2[2]);
        assert_eq!(token(&d, &l, "$$\"\"\"a "), c(Code::String));
    }

    /// A line-local break (a stray token, a missing name, an unclosed
    /// block) leaves the declarations around it highlighted. An unclosed
    /// `(` or string runs on until something closes it, so it isn't here.
    #[test]
    fn broken_code_still_highlights_around_the_break() {
        let mut bad = Vec::new();
        for brk in ["x)", "val = 2", "fun g() {", "is String if ->", "val x = = 1", "class {"] {
            for src in KOTLIN_2 {
                let src = format!("{src}\n{brk}\n{src}\nval after = 1");
                let (d, l) = highlighted("kotlin", &src);
                let t = per_char(&d, &l);
                let keyword = |r: usize| {
                    let n = l[r].split(|ch: char| !ch.is_alphanumeric()).next().unwrap().len();
                    t[r][..n].iter().all(|x| *x == c(Keyword))
                };
                let below = l.iter().position(|x| x == brk).unwrap() + 1;
                if ![0, below, l.len() - 1].into_iter().all(keyword) {
                    bad.push(format!("{brk:?} in {src:?}"));
                }
            }
        }
        assert!(bad.is_empty(), "the first word of the lines around the break is a keyword:\n{}", bad.join("\n"));
    }

    /// A small deterministic generator, so a failing case replays.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % n.max(1)
        }
    }

    /// Replace `n` chars at char offset `at` of `text` with `ins`, giving
    /// the new text and the edit, as a buffer logs it.
    fn splice(text: &str, at: usize, n: usize, ins: &str) -> (String, TextEdit) {
        let byte = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(b, _)| b);
        let (s, e) = (byte(at), byte(at + n));
        let new = format!("{}{ins}{}", &text[..s], &text[e..]);
        let row = text[..s].matches('\n').count();
        let col = text[..s].rsplit('\n').next().unwrap().chars().count();
        let edit = TextEdit::of(&lines(&new), (row, col), &text[s..e], ins);
        (new, edit)
    }

    /// Bring `d` up to `l`, waiting for its worker.
    fn settle(d: &mut TreeDoc, l: &[String]) {
        let start = Instant::now();
        loop {
            d.poll();
            d.update(TextEdits::default(), l);
            if !d.working() {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the worker finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Every node's kind and byte range, in order.
    fn nodes(t: &Tree) -> Vec<(u16, usize, usize)> {
        let mut out = Vec::new();
        let mut cur = t.walk();
        loop {
            let n = cur.node();
            out.push((n.kind_id(), n.start_byte(), n.end_byte()));
            if cur.goto_first_child() || cur.goto_next_sibling() {
                continue;
            }
            loop {
                if !cur.goto_parent() {
                    return out;
                }
                if cur.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    /// `d`, brought up to `l` by edits, matches a fresh parse of `l`.
    fn assert_fresh(d: &mut TreeDoc, l: &[String], why: &str) {
        settle(d, l);
        d.highlight(l, 0..l.len());
        let mut f = doc(d.name());
        f.update(TextEdits::reset(), l);
        settle(&mut f, l);
        f.highlight(l, 0..l.len());
        assert_eq!(nodes(d.tree.as_ref().unwrap()), nodes(f.tree.as_ref().unwrap()), "{why}");
        assert_eq!(per_char(d, l), per_char(&f, l), "{why}");
    }

    const PIECES: [&str; 14] =
        ["fun ", "val x = 1\n", "\"", "${", "}", "\n", "/*", "*/", "é", "🦀", "when (x) {\n", "// c\n", "(", " "];

    #[test]
    fn an_edited_tree_matches_a_fresh_parse() {
        for (lang, src) in [("kotlin", KOTLIN), ("rust", "fn main() {\n    let x = 1;\n}\n")] {
            let mut rng = Rng(7);
            let mut text = src.to_string();
            let mut d = doc(lang);
            d.update(TextEdits::reset(), &lines(&text));
            for step in 0..300 {
                let mut log = TextEdits::default();
                for _ in 0..1 + rng.below(3) {
                    let chars = text.chars().count();
                    let at = rng.below(chars + 1);
                    let n = if rng.below(3) == 0 { rng.below(6).min(chars - at) } else { 0 };
                    let ins = if n > 0 && rng.below(2) == 0 { "" } else { PIECES[rng.below(PIECES.len())] };
                    let (new, e) = splice(&text, at, n, ins);
                    log.push(e);
                    text = new;
                }
                let l = lines(&text);
                d.update(log, &l);
                if step % 10 == 0 {
                    assert_fresh(&mut d, &l, &format!("{lang} step {step}:\n{text}"));
                }
            }
        }
    }

    #[test]
    fn a_tree_grown_past_the_worker_size_matches_a_fresh_parse() {
        let block = "fun f(x: Int): Int {\n    val s = \"${x + 1}\"\n    return x * 2 // twice\n}\n\n";
        let mut text = String::new();
        let mut d = doc("kotlin");
        d.update(TextEdits::reset(), &lines(&text));
        let mut rng = Rng(3);
        let mut grew_past = false;
        while text.len() < WORKER_BYTES + 100 * block.len() {
            let at = text.chars().count();
            let (new, e) = splice(&text, at, 0, &block.repeat(100));
            let mut log = TextEdits::default();
            log.push(e);
            text = new;
            // And a break somewhere earlier, typed then taken back.
            let pos = rng.below(at);
            let (broken, e) = splice(&text, pos, 0, "\"/*");
            log.push(e);
            let l = lines(&broken);
            d.update(log, &l);
            let (fixed, e) = splice(&broken, pos, 3, "");
            let mut log = TextEdits::default();
            log.push(e);
            text = fixed;
            let l = lines(&text);
            d.update(log, &l);
            grew_past |= d.working();
            assert_fresh(&mut d, &l, &format!("at {} bytes", text.len()));
        }
        assert!(grew_past, "a parse ran on the worker");
    }

    #[test]
    fn random_edits_never_lose_a_line_s_spans() {
        let mut rng = Rng(11);
        let mut text = format!("{KOTLIN}\n{}", KOTLIN_2.join("\n"));
        let mut d = doc("kotlin");
        d.update(TextEdits::reset(), &lines(&text));
        for _ in 0..200 {
            let chars = text.chars().count();
            let at = rng.below(chars + 1);
            let n = rng.below(4).min(chars - at);
            let (new, e) = splice(&text, at, n, PIECES[rng.below(PIECES.len())]);
            let mut log = TextEdits::default();
            log.push(e);
            text = new;
            let l = lines(&text);
            d.update(log, &l);
            d.highlight(&l, 0..l.len());
            for (r, line) in l.iter().enumerate() {
                let spans = d.line(r);
                let end = spans.iter().fold(0, |at, s| {
                    assert_eq!(s.start, at, "contiguous");
                    s.end
                });
                assert_eq!(end, line.chars().count(), "row {r} covered: {line:?}");
            }
        }
    }

    /// A Kotlin document past [`WORKER_BYTES`], and a doc whose worker
    /// holds its tree while the returned gate is locked.
    fn gated() -> (TreeDoc, Arc<std::sync::Mutex<()>>, Vec<String>) {
        let gate = Arc::new(std::sync::Mutex::new(()));
        let mut d = doc("kotlin");
        d.gate = Some(gate.clone());
        let l = lines(&"fun f() = 1\n".repeat(WORKER_BYTES / 10));
        (d, gate, l)
    }

    #[test]
    fn a_large_document_parses_on_the_worker_and_highlights_its_shifted_tree_meanwhile() {
        let (mut d, gate, l) = gated();
        let held = gate.lock().unwrap();
        assert!(d.update(TextEdits::reset(), &l));
        assert!(d.working());
        d.highlight(&l, 0..3);
        assert!(d.line(0).is_empty(), "no tree: plain");
        drop(held);
        settle(&mut d, &l);
        d.highlight(&l, 0..3);
        assert_eq!(d.line(1)[0].token, c(Keyword));

        // Lines put in above while a parse runs: the old tree, shifted.
        let held = gate.lock().unwrap();
        let text = syntax::text(&l);
        let (text, e) = splice(&text, 0, 0, "// a\n// b\n");
        let l = lines(&text);
        let mut log = TextEdits::default();
        log.push(e);
        assert!(d.update(log, &l));
        assert!(d.working());
        let (text, e) = splice(&text, 0, 0, "val ");
        let l = lines(&text);
        let mut log = TextEdits::default();
        log.push(e);
        assert!(d.update(log, &l), "an edit while the worker runs is drawn");
        d.highlight(&l, 0..4);
        assert_eq!(d.line(2)[0].token, c(Keyword), "`fun` moved down two rows");
        assert_eq!(d.line(3)[0].token, c(Keyword));
        drop(held);
        assert_fresh(&mut d, &l, "after the worker and the edit made meanwhile");

        // A reset drops the tree.
        let _held = gate.lock().unwrap();
        d.update(TextEdits::reset(), &l);
        d.highlight(&l, 0..4);
        assert!(d.line(2).is_empty());
    }

    #[test]
    fn the_worker_wakes_the_view_when_its_tree_lands() {
        let woke = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woke.clone();
        let waker: Waker = Arc::new(move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        let mut d = open(Path::new("big.kt"), Some(waker)).unwrap();
        let l = lines(&"fun f() = 1\n".repeat(WORKER_BYTES / 10));
        d.update(TextEdits::reset(), &l);
        let start = Instant::now();
        while !d.poll() {
            assert!(start.elapsed() < Duration::from_secs(10), "the worker finishes");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(woke.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!d.working());
        assert!(!d.update(TextEdits::default(), &l), "up to date");
    }

    #[test]
    fn a_small_document_parses_at_once() {
        let mut d = doc("kotlin");
        let l = lines("fun f() = 1");
        assert!(d.update(TextEdits::reset(), &l));
        assert!(!d.working());
        assert!(!d.update(TextEdits::default(), &l), "nothing changed");
        d.highlight(&l, 0..1);
        assert_eq!(d.line(0)[0], Span { start: 0, end: 3, token: c(Keyword) });
    }
}
