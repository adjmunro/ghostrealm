//! Markdown highlighting: headings, emphasis and strong, inline code, fenced
//! code blocks (the open fence is the carried state), links and images, lists,
//! block quotes, thematic breaks. Line-local apart from fences; the
//! constructs that need a neighbouring line (setext headings, indented code,
//! multi-line emphasis) are not recognised.
//!
//! A fence opened after a list marker or `>` is recognised; it ends at its
//! closing fence, or at the first non-blank line that leaves the container
//! (no indent for a list item, no `>` for a quote). Containers are not
//! tracked beyond that, so a fence in a nested list closes at the same rules.
//!
//! Inline work per line is bounded: nesting stops at [`MAX_DEPTH`], and after
//! [`MAX_FAILED`] openers with no matching closer the rest of the line is left
//! unmarked, so a pathological line costs a bounded number of scans.

use super::{runs, Highlighter, Span, State, Token};

pub struct Markdown;

impl Highlighter for Markdown {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "markdown", "mdown", "mkd"]
    }

    fn line(&self, line: &str, state: State) -> (Vec<Span>, State) {
        let chars: Vec<char> = line.chars().collect();
        let mut tok = vec![Token::Plain; chars.len()];
        let next = block(&chars, &mut tok, state);
        (runs(&tok), next)
    }
}

/// How deep inline markup nests (emphasis in a link in emphasis ...).
const MAX_DEPTH: usize = 32;
/// How many openers (`[`, `*`, `<`, a backtick run) may find no closer on a
/// line before the rest of it is left unmarked. Each failure is a scan to the
/// line's end.
const MAX_FAILED: u32 = 32;

/// What a fence sits inside, which decides where an unclosed one ends.
const TOP: State = 0;
const LIST: State = 1;
const QUOTE: State = 2;

/// The state while inside a fence: its length, whether it is `~`, and its
/// container (a fence is at least 3 long, so the state is never 0).
fn fence_state(ch: char, len: usize, container: State) -> State {
    ((len as State) << 3) | (container << 1) | State::from(ch == '~')
}

fn fence_of(state: State) -> (char, usize, State) {
    (if state & 1 == 1 { '~' } else { '`' }, (state >> 3) as usize, (state >> 1) & 3)
}

/// An opening fence at `p`: the fence char and run length, if the run is 3 or
/// more and (for backticks) the info string holds no backtick.
fn fence_at(c: &[char], p: usize) -> Option<(char, usize)> {
    let first = *c.get(p)?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = run_of(c, p, first);
    let info = &c[p + run..];
    (run >= 3 && !(first == '`' && info.contains(&'`'))).then_some((first, run))
}

fn fill(tok: &mut [Token], from: usize, to: usize, t: Token) {
    let to = to.min(tok.len());
    if from < to {
        tok[from..to].fill(t);
    }
}

fn run_of(c: &[char], at: usize, ch: char) -> usize {
    c[at.min(c.len())..].iter().take_while(|x| **x == ch).count()
}

fn blank(c: &[char]) -> bool {
    c.iter().all(|x| x.is_whitespace())
}

/// Classify one line; returns the state the next line enters with.
fn block(c: &[char], tok: &mut [Token], state: State) -> State {
    let n = c.len();
    let indent = run_of(c, 0, ' ');
    if state != 0 {
        let (fc, flen, container) = fence_of(state);
        let quoted = c.get(indent) == Some(&'>');
        let left = !blank(c)
            && match container {
                LIST => indent == 0 && !quoted,
                QUOTE => !quoted,
                _ => false,
            };
        if !left {
            // Skip the container's prefix (indent, `>`), then look for the closer.
            let mut p = 0;
            if container != TOP {
                loop {
                    p += run_of(c, p, ' ');
                    if c.get(p) != Some(&'>') {
                        break;
                    }
                    tok[p] = Token::Marker;
                    p += 1;
                }
            } else if indent <= 3 {
                p = indent;
            }
            let run = run_of(c, p, fc);
            if run >= flen && blank(&c[p + run..]) && (container != TOP || indent <= 3) {
                fill(tok, p, n, Token::Marker);
                return 0;
            }
            fill(tok, p, n, Token::Code);
            return state;
        }
    }
    if indent <= 3 && indent < n {
        let first = c[indent];
        if let Some((fc, run)) = fence_at(c, indent) {
            fill(tok, indent, indent + run, Token::Marker);
            fill(tok, indent + run, n, Token::Key);
            return fence_state(fc, run, TOP);
        }
        if first == '#' {
            let hashes = run_of(c, indent, '#');
            if hashes <= 6 && (indent + hashes == n || c[indent + hashes] == ' ') {
                fill(tok, indent, indent + hashes, Token::Marker);
                fill(tok, indent + hashes, n, Token::Heading);
                inline_line(c, indent + hashes, n, tok);
                return 0;
            }
        }
        if matches!(first, '-' | '*' | '_') {
            let marks = c.iter().filter(|x| **x == first).count();
            if marks >= 3 && c[indent..].iter().all(|x| *x == first || *x == ' ') {
                fill(tok, indent, n, Token::Marker);
                return 0;
            }
        }
    }
    // Block quotes, then a list item, then the inline text.
    let mut p = indent;
    let mut base = Token::Plain;
    while c.get(p) == Some(&'>') {
        tok[p] = Token::Marker;
        p += 1;
        if c.get(p) == Some(&' ') {
            tok[p] = Token::Marker;
            p += 1;
        }
        base = Token::Quote;
    }
    let item = list_marker(c, p);
    if item > 0 {
        fill(tok, p, p + item, Token::Marker);
        p += item;
        p += run_of(c, p, ' ');
        if let Some(task) = task_box(c, p) {
            fill(tok, p, p + task, Token::Marker);
            p += task;
        }
    } else if base == Token::Plain && c.get(p) == Some(&'[') {
        if let Some(label_end) = c[p..].iter().position(|x| *x == ']').map(|i| p + i) {
            if c.get(label_end + 1) == Some(&':') {
                // A link reference definition: `[label]: target`.
                fill(tok, p, label_end + 1, Token::Link);
                tok[p] = Token::Marker;
                tok[label_end] = Token::Marker;
                tok[label_end + 1] = Token::Marker;
                fill(tok, label_end + 2, n, Token::Url);
                return 0;
            }
        }
    }
    if base == Token::Quote || item > 0 {
        let at = p + run_of(c, p, ' ').min(3);
        if let Some((fc, run)) = fence_at(c, at) {
            fill(tok, at, at + run, Token::Marker);
            fill(tok, at + run, n, Token::Key);
            return fence_state(fc, run, if item > 0 { LIST } else { QUOTE });
        }
    }
    fill(tok, p, n, base);
    inline_line(c, p, n, tok);
    0
}

/// The length of a list marker (`-`, `*`, `+`, `1.`, `1)`) at `p` that is
/// followed by a space or the line's end, else 0.
fn list_marker(c: &[char], p: usize) -> usize {
    let ends = |at: usize| c.get(at).is_none_or(|x| *x == ' ');
    match c.get(p) {
        Some('-' | '*' | '+') if ends(p + 1) => 1,
        Some(d) if d.is_ascii_digit() => {
            let digits = c[p..].iter().take_while(|x| x.is_ascii_digit()).count();
            match c.get(p + digits) {
                Some('.' | ')') if digits <= 9 && ends(p + digits + 1) => digits + 1,
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// The length of a task-list box (`[ ]`, `[x]`) at `p`, if followed by a space.
fn task_box(c: &[char], p: usize) -> Option<usize> {
    (c.get(p) == Some(&'[')
        && matches!(c.get(p + 1), Some(' ' | 'x' | 'X'))
        && c.get(p + 2) == Some(&']')
        && c.get(p + 3) == Some(&' '))
    .then_some(3)
}

/// The index of the bracket closing the one at `open`, within `to`.
fn match_bracket(c: &[char], open: usize, to: usize, o: char, cl: char) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < to {
        match c[i] {
            '\\' => i += 1,
            x if x == o => depth += 1,
            x if x == cl => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The start of the first run of exactly `run` `ch`s in `from..to`.
fn find_run(c: &[char], from: usize, to: usize, ch: char, run: usize) -> Option<usize> {
    let mut i = from;
    while i < to {
        if c[i] == ch {
            let len = c[i..to].iter().take_while(|x| **x == ch).count();
            if len == run {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// The start of the run of `run` `ch`s that closes emphasis opened before
/// `from`: not preceded by whitespace (and, for `_`, not inside a word).
fn find_closer(c: &[char], from: usize, to: usize, ch: char, run: usize) -> Option<usize> {
    let mut i = from;
    while i < to {
        match c[i] {
            '\\' => i += 2,
            x if x == ch => {
                let len = c[i..to].iter().take_while(|y| **y == ch).count();
                let closes = len == run
                    && i > from
                    && !c[i - 1].is_whitespace()
                    && (ch == '*' || c.get(i + len).is_none_or(|a| !a.is_alphanumeric()));
                if closes {
                    return Some(i);
                }
                i += len;
            }
            _ => i += 1,
        }
    }
    None
}

/// A bare URL starting at `i`: its end (exclusive), trailing punctuation off.
fn bare_url(c: &[char], i: usize, to: usize) -> Option<usize> {
    let starts = |s: &str| s.chars().enumerate().all(|(k, x)| c.get(i + k) == Some(&x));
    let scheme = if starts("https://") {
        8
    } else if starts("http://") {
        7
    } else {
        return None;
    };
    let mut end = i + scheme;
    while end < to && !c[end].is_whitespace() && !matches!(c[end], '<' | '>' | '"') {
        end += 1;
    }
    let mut opens = c[i..end].iter().filter(|x| **x == '(').count();
    let mut closes = c[i..end].iter().filter(|x| **x == ')').count();
    while end > i + scheme {
        let last = c[end - 1];
        let unbalanced_paren = last == ')' && opens < closes;
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?') || unbalanced_paren {
            end -= 1;
            closes -= usize::from(last == ')');
            opens -= usize::from(last == '(');
        } else {
            break;
        }
    }
    (end > i + scheme).then_some(end)
}

/// Failed openers so far on this line (see [`MAX_FAILED`]).
struct Budget(u32);

impl Budget {
    fn spent(&self) -> bool {
        self.0 >= MAX_FAILED
    }
}

fn inline_line(c: &[char], from: usize, to: usize, tok: &mut [Token]) {
    inline(c, from, to, 0, &mut Budget(0), tok);
}

/// Classify the inline markup in `c[from..to]`. Unmarked text keeps whatever
/// `tok` already holds (its block's colour).
fn inline(c: &[char], from: usize, to: usize, depth: usize, budget: &mut Budget, tok: &mut [Token]) {
    let mut i = from;
    while i < to && !budget.spent() {
        match c[i] {
            '\\' => i += 2,
            '`' => {
                let run = run_of(c, i, '`').min(to - i);
                match find_run(c, i + run, to, '`', run) {
                    Some(close) => {
                        fill(tok, i, i + run, Token::Marker);
                        fill(tok, i + run, close, Token::Code);
                        fill(tok, close, close + run, Token::Marker);
                        i = close + run;
                    }
                    None => {
                        budget.0 += 1;
                        i += run;
                    }
                }
            }
            ch @ ('*' | '_') => {
                let run = c[i..to].iter().take_while(|x| **x == ch).count();
                let opens = run <= 3
                    && c.get(i + run).is_some_and(|a| !a.is_whitespace() && i + run < to)
                    && (ch == '*' || i == from || !c[i - 1].is_alphanumeric());
                let close = if opens { find_closer(c, i + run, to, ch, run) } else { None };
                match close {
                    Some(close) => {
                        let t = if run == 1 { Token::Emphasis } else { Token::Strong };
                        fill(tok, i, i + run, Token::Marker);
                        fill(tok, i + run, close, t);
                        fill(tok, close, close + run, Token::Marker);
                        if depth < MAX_DEPTH {
                            inline(c, i + run, close, depth + 1, budget, tok);
                        }
                        i = close + run;
                    }
                    None => {
                        budget.0 += u32::from(opens);
                        i += run;
                    }
                }
            }
            '!' | '[' => {
                let bang = c[i] == '!';
                let open = if bang { i + 1 } else { i };
                if c.get(open) != Some(&'[') {
                    i += 1;
                    continue;
                }
                match link(c, open, to) {
                    Some(l) => {
                        if bang {
                            tok[i] = Token::Marker;
                        }
                        fill(tok, open + 1, l.text_end, Token::Link);
                        if depth < MAX_DEPTH {
                            inline(c, open + 1, l.text_end, depth + 1, budget, tok);
                        }
                        for at in [open, l.text_end, l.target_open, l.target_close] {
                            tok[at] = Token::Marker;
                        }
                        fill(tok, l.target_open + 1, l.url_end, Token::Url);
                        fill(tok, l.url_end, l.target_close, Token::String);
                        i = l.target_close + 1;
                    }
                    None => {
                        budget.0 += 1;
                        i = open + 1;
                    }
                }
            }
            '<' => match autolink(c, i, to) {
                Some(close) => {
                    tok[i] = Token::Marker;
                    fill(tok, i + 1, close, Token::Url);
                    tok[close] = Token::Marker;
                    i = close + 1;
                }
                None => i += 1,
            },
            'h' if i == from || !c[i - 1].is_alphanumeric() => match bare_url(c, i, to) {
                Some(end) => {
                    fill(tok, i, end, Token::Url);
                    i = end;
                }
                None => i += 1,
            },
            _ => i += 1,
        }
    }
}

/// The positions of an inline link's or image's parts. For `[text](url "t")`:
/// `text_end` is the `]`, `target_open`/`target_close` the parentheses,
/// `url_end` the end of the URL (a title follows it). For a reference
/// `[text][ref]` the brackets of `ref` take the place of the parentheses.
struct LinkAt {
    text_end: usize,
    target_open: usize,
    target_close: usize,
    url_end: usize,
}

fn link(c: &[char], open: usize, to: usize) -> Option<LinkAt> {
    let text_end = match_bracket(c, open, to, '[', ']')?;
    let target_open = text_end + 1;
    let (o, cl) = match c.get(target_open)? {
        '(' => ('(', ')'),
        '[' => ('[', ']'),
        _ => return None,
    };
    let target_close = match_bracket(c, target_open, to, o, cl)?;
    let url_end = if o == '(' {
        (target_open + 1..target_close).find(|k| c[*k] == ' ').unwrap_or(target_close)
    } else {
        target_close
    };
    Some(LinkAt { text_end, target_open, target_close, url_end })
}

/// The `>` closing an autolink `<scheme://...>` or `<mailto:...>` at `i`. The
/// scheme is checked first, and the scan stops at whitespace or another `<`,
/// so a stray `<` costs a prefix compare.
fn autolink(c: &[char], i: usize, to: usize) -> Option<usize> {
    let starts = |s: &str| s.chars().enumerate().all(|(k, x)| c.get(i + 1 + k) == Some(&x));
    if !(starts("http://") || starts("https://") || starts("mailto:")) {
        return None;
    }
    let close = (i + 1..to).find(|k| c[*k].is_whitespace() || matches!(c[*k], '<' | '>'))?;
    (c[close] == '>').then_some(close)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Token::{Code, Emphasis, Heading, Key, Link, Marker, Plain, Quote, Strong, Url};

    /// The line's tokens as `(text, token)` runs, for readable assertions.
    fn lex(line: &str) -> Vec<(String, Token)> {
        lex_in(line, 0).0
    }

    fn lex_in(line: &str, state: State) -> (Vec<(String, Token)>, State) {
        let chars: Vec<char> = line.chars().collect();
        let (spans, next) = Markdown.line(line, state);
        let out = spans.iter().map(|s| (chars[s.start..s.end].iter().collect(), s.token)).collect();
        (out, next)
    }

    fn run(text: &str, token: Token) -> (String, Token) {
        (text.to_string(), token)
    }

    #[test]
    fn headings_mark_the_hashes() {
        assert_eq!(lex("## Title"), vec![run("##", Marker), run(" Title", Heading)]);
        assert_eq!(lex("# "), vec![run("#", Marker), run(" ", Heading)]);
        assert_eq!(lex("#tag"), vec![run("#tag", Plain)], "no space, not a heading");
        assert_eq!(lex("####### seven"), vec![run("####### seven", Plain)]);
        assert_eq!(lex("   ## ok"), vec![run("   ", Plain), run("##", Marker), run(" ok", Heading)]);
        assert_eq!(lex("    ## code"), vec![run("    ## code", Plain)]);
    }

    #[test]
    fn emphasis_and_strong_keep_their_markers() {
        assert_eq!(
            lex("a *b* **c** d"),
            vec![
                run("a ", Plain),
                run("*", Marker),
                run("b", Emphasis),
                run("*", Marker),
                run(" ", Plain),
                run("**", Marker),
                run("c", Strong),
                run("**", Marker),
                run(" d", Plain),
            ]
        );
        assert_eq!(lex("_x_"), vec![run("_", Marker), run("x", Emphasis), run("_", Marker)]);
        assert_eq!(lex("***x***"), vec![run("***", Marker), run("x", Strong), run("***", Marker)]);
    }

    #[test]
    fn emphasis_needs_a_closer_and_no_inner_padding() {
        assert_eq!(lex("2 * 3 * 4"), vec![run("2 * 3 * 4", Plain)]);
        assert_eq!(lex("*open"), vec![run("*open", Plain)]);
        assert_eq!(lex("snake_case_name"), vec![run("snake_case_name", Plain)]);
        assert_eq!(lex("**"), vec![run("**", Plain)]);
    }

    #[test]
    fn nesting_and_escapes() {
        assert_eq!(
            lex("**a *b* c**"),
            vec![
                run("**", Marker),
                run("a ", Strong),
                run("*", Marker),
                run("b", Emphasis),
                run("*", Marker),
                run(" c", Strong),
                run("**", Marker),
            ]
        );
        assert_eq!(lex("\\*a\\*"), vec![run("\\*a\\*", Plain)]);
    }

    #[test]
    fn inline_code_wins_over_emphasis() {
        assert_eq!(
            lex("`a*b` *c*"),
            vec![
                run("`", Marker),
                run("a*b", Code),
                run("`", Marker),
                run(" ", Plain),
                run("*", Marker),
                run("c", Emphasis),
                run("*", Marker),
            ]
        );
        assert_eq!(lex("``a`b``")[1], run("a`b", Code));
        assert_eq!(lex("`open"), vec![run("`open", Plain)]);
    }

    #[test]
    fn fenced_blocks_carry_state_across_lines() {
        let (open, s) = lex_in("```rust", 0);
        assert_eq!(open, vec![run("```", Marker), run("rust", Key)]);
        assert_ne!(s, 0);
        let (body, s) = lex_in("# not *a* heading", s);
        assert_eq!(body, vec![run("# not *a* heading", Code)]);
        assert_ne!(s, 0);
        // A shorter or different fence does not close it.
        assert_ne!(lex_in("``", s).1, 0);
        assert_ne!(lex_in("~~~", s).1, 0);
        let (close, s) = lex_in("```", s);
        assert_eq!(close, vec![run("```", Marker)]);
        assert_eq!(s, 0);
        assert_eq!(lex_in("# back", s).0[0], run("#", Marker));
    }

    #[test]
    fn a_longer_fence_closes_and_a_tilde_fence_is_its_own_kind() {
        let (_, s) = lex_in("````", 0);
        assert_ne!(lex_in("```", s).1, 0, "shorter than the opener");
        assert_eq!(lex_in("`````", s).1, 0);
        let (_, s) = lex_in("~~~", 0);
        assert_ne!(lex_in("```", s).1, 0);
        assert_eq!(lex_in("~~~  ", s).1, 0);
        // A backtick fence's info string cannot hold a backtick: inline code.
        assert_eq!(lex_in("```a`b", 0).1, 0);
    }

    #[test]
    fn links_images_and_references() {
        assert_eq!(
            lex("see [the docs](https://x.io/a \"T\") ok"),
            vec![
                run("see ", Plain),
                run("[", Marker),
                run("the docs", Link),
                run("](", Marker),
                run("https://x.io/a", Url),
                run(" \"T\"", Token::String),
                run(")", Marker),
                run(" ok", Plain),
            ]
        );
        assert_eq!(
            lex("![alt](a.png)"),
            vec![run("![", Marker), run("alt", Link), run("](", Marker), run("a.png", Url), run(")", Marker)]
        );
        assert_eq!(
            lex("[t][r]"),
            vec![run("[", Marker), run("t", Link), run("][", Marker), run("r", Url), run("]", Marker)]
        );
        assert_eq!(lex("[just brackets]"), vec![run("[just brackets]", Plain)]);
        assert_eq!(
            lex("[1]: https://x.io"),
            vec![run("[", Marker), run("1", Link), run("]:", Marker), run(" https://x.io", Url)]
        );
    }

    #[test]
    fn a_badge_link_nests_an_image() {
        let toks = lex("[![b](i.svg)](https://x.io)");
        assert!(toks.contains(&run("b", Link)));
        assert!(toks.contains(&run("i.svg", Url)));
        assert!(toks.contains(&run("https://x.io", Url)));
    }

    #[test]
    fn bare_urls_and_autolinks() {
        assert_eq!(lex("go to https://x.io/a."), vec![run("go to ", Plain), run("https://x.io/a", Url), run(".", Plain)]);
        assert_eq!(lex("(see https://x.io/a)"), vec![run("(see ", Plain), run("https://x.io/a", Url), run(")", Plain)]);
        assert_eq!(
            lex("<https://x.io>"),
            vec![run("<", Marker), run("https://x.io", Url), run(">", Marker)]
        );
        assert_eq!(lex("<b>"), vec![run("<b>", Plain)]);
        assert_eq!(lex("xhttps://"), vec![run("xhttps://", Plain)]);
    }

    #[test]
    fn lists_quotes_and_rules() {
        assert_eq!(lex("- item"), vec![run("-", Marker), run(" item", Plain)]);
        assert_eq!(lex("  1. **x**")[1], run("1.", Marker));
        assert_eq!(lex("-notitem"), vec![run("-notitem", Plain)]);
        assert_eq!(lex("- [x] done"), vec![run("-", Marker), run(" ", Plain), run("[x]", Marker), run(" done", Plain)]);
        assert_eq!(
            lex("> quoted *e*"),
            vec![run("> ", Marker), run("quoted ", Quote), run("*", Marker), run("e", Emphasis), run("*", Marker)]
        );
        assert_eq!(lex(">> two"), vec![run(">> ", Marker), run("two", Quote)]);
        assert_eq!(lex("---"), vec![run("---", Marker)]);
        assert_eq!(lex("* * *"), vec![run("* * *", Marker)]);
        assert_eq!(lex("--"), vec![run("--", Plain)]);
    }

    #[test]
    fn spans_are_contiguous_and_cover_the_line() {
        for line in ["", "plain", "# h `c` **b** [l](u) <https://x>", "> - [ ] t *e*", "```", "日本語 *強調* `コード`"] {
            let n = line.chars().count();
            let (spans, _) = Markdown.line(line, 0);
            let covered: usize = spans.iter().map(|s| s.end - s.start).sum();
            assert_eq!(covered, n, "{line:?}");
            assert!(spans.windows(2).all(|w| w[0].end == w[1].start && w[0].token != w[1].token), "{line:?}");
        }
    }

    /// Highlight `line` and fail if it takes longer than a linear pass could.
    fn bounded(line: &str) {
        let start = std::time::Instant::now();
        let (spans, _) = Markdown.line(line, 0);
        assert_eq!(spans.last().map(|s| s.end), Some(line.chars().count()));
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "{:?}", start.elapsed());
    }

    #[test]
    fn deeply_nested_links_and_emphasis_do_not_overflow_the_stack() {
        let k = 20_000;
        bounded(&format!("{}x{}", "[".repeat(k), "](u)".repeat(k)));
        bounded(&format!("{}x{}", "*a ".repeat(k), "*".repeat(k)));
        bounded(&format!("{}x{}", "_(".repeat(k), ")_".repeat(k)));
        // Within the depth limit, nesting is still marked.
        let toks = lex("[a [b](u) c](v)");
        assert!(toks.contains(&run("b", Link)));
    }

    #[test]
    fn openers_without_closers_cost_a_bounded_scan_each() {
        let n = 100_000;
        bounded(&"[".repeat(n));
        bounded(&"<".repeat(n));
        bounded(&"<http://".repeat(n / 8));
        bounded(&"*a".repeat(n / 2));
        bounded(&"_a ".repeat(n / 3));
        bounded(&"`".repeat(n / 2).replace("``", "`a"));
        bounded(&format!("http://x{}", ")".repeat(n)));
        bounded(&"![".repeat(n / 2));
    }

    #[test]
    fn markup_after_many_stray_openers_is_left_alone() {
        let line = format!("{}`code`", "*a ".repeat(40));
        assert!(!lex(&line).contains(&run("code", Code)), "past the budget");
        // A few strays still leave later markup highlighted.
        assert!(lex("*a [ b ( c `code`").contains(&run("code", Code)));
    }

    #[test]
    fn a_fence_opened_after_a_list_marker_ends_at_its_indented_closer() {
        let lines = ["- ```js", "  let x = 1;", "  ```", "# Heading after list", "text *em*"];
        let mut state = 0;
        let mut out = Vec::new();
        for l in lines {
            let (t, next) = lex_in(l, state);
            out.push(t);
            state = next;
        }
        assert_eq!(out[0], vec![run("-", Marker), run(" ", Plain), run("```", Marker), run("js", Key)]);
        assert_eq!(out[1], vec![run("  ", Plain), run("let x = 1;", Code)]);
        assert_eq!(out[2], vec![run("  ", Plain), run("```", Marker)]);
        assert_eq!(out[3][0], run("#", Marker), "the heading is a heading");
        assert!(out[4].contains(&run("em", Emphasis)), "so is the emphasis");
        assert_eq!(state, 0);
    }

    #[test]
    fn a_fence_in_a_quote_follows_the_quote_prefix() {
        let (open, s) = lex_in("> ```", 0);
        assert_eq!(open, vec![run("> ```", Marker)]);
        let (body, s) = lex_in("> # code", s);
        assert_eq!(body, vec![run(">", Marker), run(" ", Plain), run("# code", Code)]);
        assert_ne!(s, 0);
        assert_eq!(lex_in("> ```", s).1, 0);
        // A line outside the quote ends the fence without a closer.
        let (_, s) = lex_in("> ```", 0);
        let (after, s) = lex_in("# out", s);
        assert_eq!(after[0], run("#", Marker));
        assert_eq!(s, 0);
    }

    #[test]
    fn an_unclosed_list_fence_ends_where_the_list_does_and_keeps_blank_lines() {
        let (_, s) = lex_in("1. ```", 0);
        let (blank_line, s) = lex_in("", s);
        assert!(blank_line.is_empty());
        assert_ne!(s, 0, "a blank line stays inside");
        assert_ne!(lex_in("   more", s).1, 0, "indented lines stay inside");
        assert_eq!(lex_in("not indented", s).1, 0);
    }

    #[test]
    fn every_line_of_up_to_four_markup_characters_is_classified_without_panic() {
        let alphabet: Vec<char> = "*_`[]()!<>#-~:ha ".chars().collect();
        let fenced = fence_state('`', 3, TOP);
        let mut layer = vec![String::new()];
        for _ in 0..4 {
            layer = layer
                .iter()
                .flat_map(|l| alphabet.iter().map(move |c| format!("{l}{c}")))
                .collect();
            for line in &layer {
                for state in [0, fenced] {
                    let (spans, _) = Markdown.line(line, state);
                    let covered: usize = spans.iter().map(|s| s.end - s.start).sum();
                    assert_eq!(covered, line.chars().count(), "{line:?}");
                }
            }
        }
    }
}
