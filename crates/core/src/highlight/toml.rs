//! Heuristic, line-local TOML highlighting: comments, `[section]` headers,
//! `key =` names, quoted strings, and numbers/booleans. No multi-line strings,
//! which is plenty for a config file.

use super::{runs, CommentSyntax, Highlighter, Span, State, Token};

pub struct Toml;

impl Highlighter for Toml {
    fn name(&self) -> &'static str {
        "toml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["toml"]
    }

    fn carries_state(&self) -> bool {
        false
    }

    fn comment(&self) -> CommentSyntax {
        CommentSyntax { line: Some("#"), block: None }
    }

    fn line(&self, line: &str, _state: State) -> (Vec<Span>, State) {
        (runs(&tokens(line)), 0)
    }
}

/// One token per char of `line`.
fn tokens(line: &str) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut tok = vec![Token::Plain; n];
    let ts = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    if ts >= n {
        return tok;
    }
    // Whole-line comment.
    if chars[ts] == '#' {
        tok[ts..].fill(Token::Comment);
        return tok;
    }
    // Section header `[..]` (possibly `[[..]]`).
    if chars[ts] == '[' {
        tok[ts..].fill(Token::Heading);
        return tok;
    }
    // Find the top-level `=`, a trailing `#` comment, tracking string quotes.
    let mut in_str: Option<char> = None;
    let mut eq: Option<usize> = None;
    let mut comment_at: Option<usize> = None;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        if let Some(q) = in_str {
            if c == q {
                in_str = None;
            }
        } else {
            match c {
                '"' | '\'' => in_str = Some(c),
                '#' => {
                    comment_at = Some(i);
                    break;
                }
                '=' if eq.is_none() => eq = Some(i),
                _ => {}
            }
        }
        i += 1;
    }
    // Key: the name left of `=`.
    if let Some(e) = eq {
        tok[ts..e].fill(Token::Key);
    }
    let val_start = eq.map(|e| e + 1).unwrap_or(ts);
    let val_end = comment_at.unwrap_or(n);
    // Value tokens: strings, then bare numbers/booleans.
    let mut j = val_start;
    let mut in_str: Option<char> = None;
    let mut tok_start: Option<usize> = None;
    while j < val_end {
        let c = chars[j];
        if let Some(q) = in_str {
            tok[j] = Token::String;
            if c == q {
                in_str = None;
            }
        } else if c == '"' || c == '\'' {
            in_str = Some(c);
            tok[j] = Token::String;
        } else if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+' || c == '_' {
            if tok_start.is_none() {
                tok_start = Some(j);
            }
        } else if let Some(s) = tok_start.take() {
            mark_bare_token(&chars, s, j, &mut tok);
        }
        j += 1;
    }
    if let Some(s) = tok_start.take() {
        mark_bare_token(&chars, s, val_end, &mut tok);
    }
    if let Some(ca) = comment_at {
        tok[ca..].fill(Token::Comment);
    }
    tok
}

/// Mark a bare value token `chars[s..e]` if it looks like a number or boolean.
fn mark_bare_token(chars: &[char], s: usize, e: usize, tok: &mut [Token]) {
    let word: String = chars[s..e].iter().collect();
    let is_num = word.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '+')
        && word.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | '_' | 'e' | 'E'));
    if is_num || word == "true" || word == "false" {
        tok[s..e].fill(Token::Number);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(line: &str, needle: &str) -> Token {
        tokens(line)[line.find(needle).unwrap()]
    }

    #[test]
    fn toml_highlight_basics() {
        assert!(tokens("# hi").iter().all(|t| *t == Token::Comment));
        assert!(tokens("[editor]").iter().all(|t| *t == Token::Heading));
        let line = "soft_wrap = true";
        assert_eq!(tokens(line)[0], Token::Key, "key name");
        assert_eq!(at(line, "true"), Token::Number, "bool");
        let line = "name = \"value\"  # note";
        assert_eq!(at(line, "\""), Token::String, "string");
        assert_eq!(at(line, "#"), Token::Comment, "trailing comment");
    }

    #[test]
    fn spans_cover_the_line_contiguously() {
        let (spans, state) = Toml.line("a = 1", 0);
        assert_eq!(state, 0);
        assert_eq!(spans.first().unwrap().start, 0);
        assert_eq!(spans.last().unwrap().end, 5);
        assert!(spans.windows(2).all(|w| w[0].end == w[1].start));
        assert!(Toml.line("", 0).0.is_empty());
    }
}
