//! The palette for core's highlight tokens, and coloured row spans built from
//! them. Shared by every text surface (editor, file preview).

use ghostrealm_core::highlight::{Span, Token};

use crate::row_cache::RunStyle;

/// The colour of `token`; plain text is drawn in `plain`.
pub fn color(token: Token, plain: [u8; 3]) -> [u8; 3] {
    match token {
        Token::Plain => plain,
        Token::Comment => [110, 140, 110],
        Token::Heading => [220, 180, 120],
        Token::Key => [130, 170, 225],
        Token::String => [170, 200, 140],
        Token::Number => [200, 160, 220],
        Token::Marker => [100, 130, 108],
        Token::Strong => [235, 205, 160],
        Token::Emphasis => [190, 165, 215],
        // Near the text colour: the code box, not a hue, sets code apart.
        Token::Code => [200, 206, 218],
        Token::Link => [100, 170, 235],
        Token::Url => [85, 125, 170],
        Token::Quote => [150, 160, 180],
    }
}

/// How `token` is drawn beyond its colour: strong text bold, emphasis italic,
/// link text underlined, headings bold, inline code boxed.
pub fn style(token: Token) -> RunStyle {
    match token {
        Token::Strong | Token::Heading => RunStyle::BOLD,
        Token::Emphasis => RunStyle::ITALIC,
        Token::Link => RunStyle::UNDERLINE,
        Token::Code => RunStyle::BOX,
        _ => RunStyle::PLAIN,
    }
}

/// How much larger than body text a heading of `level` (from
/// `Highlighter::heading_level`) draws; 1 for no heading. H5 and H6 stay at
/// body size, set apart by their bold alone.
pub fn heading_scale(level: Option<u8>) -> f32 {
    match level {
        Some(1) => 1.8,
        Some(2) => 1.5,
        Some(3) => 1.25,
        Some(4) => 1.1,
        _ => 1.0,
    }
}

/// Coloured spans for the chars `start..start + len` of `line`, whose
/// highlight is `spans`; runs of one colour are coalesced. An empty range
/// yields one empty span, so a blank row still has something to shape.
pub fn row_spans(line: &str, spans: &[Span], start: usize, len: usize, plain: [u8; 3]) -> Vec<(String, [u8; 3])> {
    build(line, spans, start, len, plain, None).0
}

/// [`row_spans`] plus per-span [`RunStyle`]s (runs coalesce on colour and
/// style). The styles are empty when no span in the row is styled, so a plain
/// row carries nothing extra. In a `block` row (a fenced code block, drawn in
/// a box of its own) code is not boxed.
pub fn styled_row_spans(
    line: &str,
    spans: &[Span],
    start: usize,
    len: usize,
    plain: [u8; 3],
    block: bool,
) -> (Vec<(String, [u8; 3])>, Vec<RunStyle>) {
    build(line, spans, start, len, plain, Some(block))
}

/// `styled`: `None` for colours only, else whether the row is in a block.
fn build(
    line: &str,
    spans: &[Span],
    start: usize,
    len: usize,
    plain: [u8; 3],
    styled: Option<bool>,
) -> (Vec<(String, [u8; 3])>, Vec<RunStyle>) {
    let end = start.saturating_add(len);
    let mut token_at = spans.iter().peekable();
    let mut out: Vec<(String, [u8; 3])> = Vec::new();
    // Empty until a run is styled, then one per run in `out`.
    let mut styles: Vec<RunStyle> = Vec::new();
    for (i, ch) in line.chars().enumerate().take(end).skip(start) {
        while token_at.next_if(|s| s.end <= i).is_some() {}
        let token = token_at.peek().filter(|s| s.start <= i).map_or(Token::Plain, |s| s.token);
        let c = color(token, plain);
        let st = match styled {
            Some(block) if !(block && token == Token::Code) => style(token),
            _ => RunStyle::PLAIN,
        };
        let last_style = styles.last().copied().unwrap_or(RunStyle::PLAIN);
        match out.last_mut() {
            Some((s, col)) if *col == c && last_style == st => s.push(ch),
            _ => {
                let start_styles = !st.is_plain() && styles.is_empty();
                if start_styles {
                    styles.resize(out.len(), RunStyle::PLAIN);
                }
                out.push((ch.to_string(), c));
                if start_styles || !styles.is_empty() {
                    styles.push(st);
                }
            }
        }
    }
    if out.is_empty() {
        out.push((String::new(), plain));
    }
    (out, styles)
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

    use ghostrealm_core::highlight::markdown::Markdown;

    fn md(line: &str, block: bool) -> (Vec<(String, [u8; 3])>, Vec<RunStyle>) {
        styled_row_spans(line, &Markdown.line(line, 0).0, 0, 99, FG, block)
    }

    #[test]
    fn tokens_map_to_styles() {
        assert_eq!(style(Token::Strong), RunStyle::BOLD);
        assert_eq!(style(Token::Heading), RunStyle::BOLD);
        assert_eq!(style(Token::Emphasis), RunStyle::ITALIC);
        assert_eq!(style(Token::Link), RunStyle::UNDERLINE);
        assert_eq!(style(Token::Code), RunStyle::BOX);
        for t in [Token::Plain, Token::Marker, Token::Url, Token::Comment, Token::Key, Token::Quote] {
            assert!(style(t).is_plain(), "{t:?}");
        }
    }

    #[test]
    fn markdown_styles_follow_the_tokens_and_leave_markers_plain() {
        let (spans, styles) = md("a **b** _c_ `d` [e](u)", false);
        let text: Vec<&str> = spans.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(text.concat(), "a **b** _c_ `d` [e](u)");
        let styled: Vec<(&str, RunStyle)> =
            text.iter().copied().zip(styles.iter().copied()).filter(|(_, s)| !s.is_plain()).collect();
        assert_eq!(
            styled,
            vec![
                ("b", RunStyle::BOLD),
                ("c", RunStyle::ITALIC),
                ("d", RunStyle::BOX),
                ("e", RunStyle::UNDERLINE)
            ]
        );
        assert_eq!(styles.len(), spans.len(), "one style per span");
        let marker = spans.iter().position(|(t, _)| t == "**").unwrap();
        assert!(styles[marker].is_plain());
        assert_eq!(spans[marker].1, color(Token::Marker, FG), "markers stay visible, dimmed");
    }

    #[test]
    fn a_row_without_styled_spans_carries_no_styles() {
        for line in ["plain words", "", "- item", "[ ] x", "https://x.io", "> quote"] {
            let (spans, styles) = md(line, false);
            assert!(styles.is_empty(), "{line:?}");
            assert_eq!(spans, row_spans(line, &Markdown.line(line, 0).0, 0, 99, FG), "{line:?}");
        }
        let toml = Toml.line("name = 1", 0).0;
        let (spans, styles) = styled_row_spans("name = 1", &toml, 0, 99, FG, false);
        assert!(styles.is_empty());
        assert_eq!(spans, row_spans("name = 1", &toml, 0, 99, FG));
    }

    #[test]
    fn code_is_boxed_inline_but_not_in_a_block_row() {
        let (_, inside) = Markdown.line("```", 0);
        let line = "let x = 1;";
        let fenced = styled_row_spans(line, &Markdown.line(line, inside).0, 0, 99, FG, true);
        assert_eq!(fenced.0, vec![(line.to_string(), color(Token::Code, FG))]);
        assert!(fenced.1.is_empty(), "the block's own box replaces the inline one");
    }

    #[test]
    fn a_wrapped_row_styles_its_slice() {
        let line = "plain **bold** end";
        let spans = Markdown.line(line, 0).0;
        let (text, styles) = styled_row_spans(line, &spans, 8, 4, FG, false);
        assert_eq!(text[0].0, "bold");
        assert_eq!(styles[0], RunStyle::BOLD);
    }

    #[test]
    fn headings_shrink_with_depth_to_body_size() {
        let scales: Vec<f32> = (1..=6).map(|l| heading_scale(Some(l))).collect();
        assert_eq!(scales, [1.8, 1.5, 1.25, 1.1, 1.0, 1.0]);
        assert_eq!(heading_scale(None), 1.0);
    }

    #[test]
    fn a_heading_is_bold_after_its_hashes() {
        let (spans, styles) = md("## Title", false);
        assert_eq!(spans[1].0, " Title");
        assert_eq!(styles, vec![RunStyle::PLAIN, RunStyle::BOLD]);
    }
}
