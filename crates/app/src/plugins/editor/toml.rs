//! Heuristic, line-local TOML syntax highlighting for the editor.

use super::EDITOR_FG;

/// TOML syntax-highlight token colours.
pub const TOML_COMMENT: [u8; 3] = [110, 140, 110];
pub const TOML_HEADER: [u8; 3] = [220, 180, 120];
pub const TOML_KEY: [u8; 3] = [130, 170, 225];
pub const TOML_STRING: [u8; 3] = [170, 200, 140];
pub const TOML_NUMBER: [u8; 3] = [200, 160, 220];

/// Per-character colours for a single TOML line: comments, `[section]` headers,
/// `key =` names, quoted strings, and numbers/booleans; everything else is the
/// default fg. Heuristic and line-local (no multi-line strings), which is plenty
/// for highlighting a config file.
pub fn toml_line_colors(line: &str) -> Vec<[u8; 3]> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut col = vec![EDITOR_FG; n];
    let ts = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(n);
    if ts >= n {
        return col;
    }
    // Whole-line comment.
    if chars[ts] == '#' {
        col[ts..].fill(TOML_COMMENT);
        return col;
    }
    // Section header `[..]` (possibly `[[..]]`).
    if chars[ts] == '[' {
        col[ts..].fill(TOML_HEADER);
        return col;
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
        col[ts..e].fill(TOML_KEY);
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
            col[j] = TOML_STRING;
            if c == q {
                in_str = None;
            }
        } else if c == '"' || c == '\'' {
            in_str = Some(c);
            col[j] = TOML_STRING;
        } else if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+' || c == '_' {
            if tok_start.is_none() {
                tok_start = Some(j);
            }
        } else if let Some(s) = tok_start.take() {
            colour_bare_token(&chars, s, j, &mut col);
        }
        j += 1;
    }
    if let Some(s) = tok_start.take() {
        colour_bare_token(&chars, s, val_end, &mut col);
    }
    if let Some(ca) = comment_at {
        col[ca..].fill(TOML_COMMENT);
    }
    col
}

/// Colour a bare value token `chars[s..e]` if it looks like a number or boolean.
fn colour_bare_token(chars: &[char], s: usize, e: usize, col: &mut [[u8; 3]]) {
    let tok: String = chars[s..e].iter().collect();
    let is_num = tok.chars().next().is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '+')
        && tok.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | '_' | 'e' | 'E'));
    if is_num || tok == "true" || tok == "false" {
        col[s..e].fill(TOML_NUMBER);
    }
}

/// Coloured spans for the visual row `line[start..start+len]` (char indices),
/// coalescing runs of one colour. Used to shape a highlighted editor row.
pub fn toml_row_spans(line: &str, start: usize, len: usize) -> Vec<(String, [u8; 3])> {
    let colors = toml_line_colors(line);
    let chars: Vec<char> = line.chars().collect();
    let end = (start + len).min(chars.len());
    let mut spans: Vec<(String, [u8; 3])> = Vec::new();
    for (i, ch) in chars.iter().enumerate().take(end).skip(start) {
        let c = colors.get(i).copied().unwrap_or(EDITOR_FG);
        match spans.last_mut() {
            Some((s, col)) if *col == c => s.push(*ch),
            _ => spans.push((ch.to_string(), c)),
        }
    }
    if spans.is_empty() {
        spans.push((String::new(), EDITOR_FG));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_highlight_basics() {
        assert!(toml_line_colors("# hi").iter().all(|c| *c == TOML_COMMENT));
        assert!(toml_line_colors("[editor]").iter().all(|c| *c == TOML_HEADER));
        let line = "soft_wrap = true";
        let c = toml_line_colors(line);
        assert_eq!(c[0], TOML_KEY, "key name coloured");
        assert_eq!(c[line.find("true").unwrap()], TOML_NUMBER, "bool coloured");
        let line = "name = \"value\"  # note";
        let c = toml_line_colors(line);
        assert_eq!(c[line.find('"').unwrap()], TOML_STRING, "string coloured");
        assert_eq!(c[line.find('#').unwrap()], TOML_COMMENT, "trailing comment");
    }
}
