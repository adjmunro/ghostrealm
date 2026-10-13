//! The palette for core's highlight tokens, and coloured row spans built from
//! them. Shared by every text surface (editor, file preview).

use ghostrealm_core::highlight::{Span, Token};

/// The colour of `token`; plain text is drawn in `plain`.
pub fn color(token: Token, plain: [u8; 3]) -> [u8; 3] {
    match token {
        Token::Plain => plain,
        Token::Comment => [110, 140, 110],
        Token::Heading => [220, 180, 120],
        Token::Key => [130, 170, 225],
        Token::String => [170, 200, 140],
        Token::Number => [200, 160, 220],
        Token::Marker => [120, 190, 130],
        Token::Strong => [235, 205, 160],
        Token::Emphasis => [190, 165, 215],
        Token::Code => [215, 150, 125],
        Token::Link => [100, 170, 235],
        Token::Url => [85, 125, 170],
        Token::Quote => [150, 160, 180],
    }
}

/// Coloured spans for the chars `start..start + len` of `line`, whose
/// highlight is `spans`; runs of one colour are coalesced. An empty range
/// yields one empty span, so a blank row still has something to shape.
pub fn row_spans(line: &str, spans: &[Span], start: usize, len: usize, plain: [u8; 3]) -> Vec<(String, [u8; 3])> {
    let end = start.saturating_add(len);
    let mut token_at = spans.iter().peekable();
    let mut out: Vec<(String, [u8; 3])> = Vec::new();
    for (i, ch) in line.chars().enumerate().take(end).skip(start) {
        while token_at.next_if(|s| s.end <= i).is_some() {}
        let token = token_at.peek().filter(|s| s.start <= i).map_or(Token::Plain, |s| s.token);
        let c = color(token, plain);
        match out.last_mut() {
            Some((s, col)) if *col == c => s.push(ch),
            _ => out.push((ch.to_string(), c)),
        }
    }
    if out.is_empty() {
        out.push((String::new(), plain));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostrealm_core::highlight::toml::Toml;
    use ghostrealm_core::highlight::Highlighter;

    const FG: [u8; 3] = [220, 220, 230];

    fn toml(line: &str, start: usize, len: usize) -> Vec<(String, [u8; 3])> {
        row_spans(line, &Toml.line(line, 0).0, start, len, FG)
    }

    #[test]
    fn toml_rows_keep_their_colours() {
        let line = "name = \"value\"  # note";
        assert_eq!(
            toml(line, 0, 99),
            vec![
                ("name ".into(), [130, 170, 225]),
                ("= ".into(), FG),
                ("\"value\"".into(), [170, 200, 140]),
                ("  ".into(), FG),
                ("# note".into(), [110, 140, 110]),
            ]
        );
        assert_eq!(toml("[editor]", 0, 99), vec![("[editor]".into(), [220, 180, 120])]);
        assert_eq!(
            toml("soft_wrap = true", 0, 99),
            vec![("soft_wrap ".into(), [130, 170, 225]), ("= ".into(), FG), ("true".into(), [200, 160, 220])]
        );
    }

    #[test]
    fn a_wrapped_row_takes_its_slice_of_the_line() {
        let line = "name = \"value\"";
        assert_eq!(toml(line, 7, 7), vec![("\"value\"".into(), [170, 200, 140])]);
        assert_eq!(toml(line, 20, 5), vec![(String::new(), FG)]);
        assert_eq!(toml("", 0, 0), vec![(String::new(), FG)]);
    }

    #[test]
    fn markdown_rows_follow_the_carried_fence_state() {
        use ghostrealm_core::highlight::markdown::Markdown;
        let (_, inside) = Markdown.line("```", 0);
        let line = "# not *a* heading";
        let fenced = row_spans(line, &Markdown.line(line, inside).0, 0, 99, FG);
        assert_eq!(fenced, vec![(line.to_string(), color(Token::Code, FG))]);
        let free = row_spans(line, &Markdown.line(line, 0).0, 0, 99, FG);
        assert_eq!(free[0], ("#".to_string(), color(Token::Marker, FG)));
        assert!(free.len() > 1);
    }
}
