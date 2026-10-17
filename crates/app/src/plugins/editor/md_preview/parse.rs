//! Markdown to the Preview's block model: `pulldown-cmark` (CommonMark plus
//! GFM tables, task lists, and strikethrough) flattened into a list of
//! blocks, each with the source line it starts on. Nesting (lists, quotes)
//! becomes each block's indent and quote depth, so the Preview lays the
//! document out as one column of blocks it can scroll by index.

use pulldown_cmark::{CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

/// How a run of inline text is set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    /// Inline code.
    pub code: bool,
    /// An image's alt text (images draw as their alt text for now).
    pub image: bool,
}

/// A run of inline text in one style.
#[derive(Clone, Debug, PartialEq)]
pub struct Inline {
    pub text: String,
    pub style: Style,
    /// The target of the link the run is in.
    pub link: Option<String>,
}

/// What starts a list item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    Bullet,
    Number(u64),
    /// A task list item, checked or not.
    Task(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    /// A list item's marker and its first paragraph; the item's later
    /// blocks follow at the same indent.
    Item(Marker, Vec<Inline>),
    /// A fenced or indented code block: the fence's language (the info
    /// string's first word) and the body.
    Code { lang: Option<String>, text: String },
    /// Cells of inline text: the header row, then the body rows.
    Table { head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    Rule,
    /// Raw HTML, shown as its source.
    Html(String),
    /// Front matter: a `---` fenced block opening the document, as its
    /// source without the fences.
    FrontMatter(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    pub kind: Kind,
    /// The source line (0-based) the block starts on.
    pub line: usize,
    /// How many lists the block is inside: an item's text sits this many
    /// list indents in, its marker one less.
    pub indent: u8,
    /// How many block quotes the block is inside.
    pub quote: u8,
}

/// Parse `text` into blocks.
pub fn parse(text: &str) -> Vec<Block> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH;
    let (front, rest, base) = match front_matter(text) {
        Some((body, rest, lines)) => (Some(body), rest, lines),
        None => (None, text, 0),
    };
    let mut b = Builder::new(rest, base);
    if let Some(body) = front {
        b.push_block(Kind::FrontMatter(body), 0);
    }
    for (event, range) in Parser::new_ext(rest, opts).into_offset_iter() {
        let line = b.line_of(range.start);
        b.event(event, line);
    }
    b.flush();
    b.blocks
}

/// An inline container being filled.
enum Open {
    Heading(u8),
    Paragraph,
    Cell,
}

#[derive(Default)]
struct TableBuild {
    head: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    row: Vec<Vec<Inline>>,
    line: usize,
}

struct Builder {
    /// The source line the parsed text starts on (past any front matter).
    base: usize,
    line_starts: Vec<usize>,
    blocks: Vec<Block>,
    /// Each open list's next number (`None`: unordered).
    lists: Vec<Option<u64>>,
    quote: u8,
    /// An item's marker and line, until its first block takes it.
    pending: Option<(Marker, usize)>,
    open: Option<(Open, usize, Vec<Inline>)>,
    bold: u32,
    italic: u32,
    strike: u32,
    image: u32,
    links: Vec<String>,
    code: Option<(Option<String>, String, usize)>,
    html: Option<(String, usize)>,
    table: Option<TableBuild>,
}

impl Builder {
    /// A builder for `text`, which starts on source line `base`.
    fn new(text: &str, base: usize) -> Self {
        let line_starts = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        Builder {
            base,
            line_starts,
            blocks: Vec::new(),
            lists: Vec::new(),
            quote: 0,
            pending: None,
            open: None,
            bold: 0,
            italic: 0,
            strike: 0,
            image: 0,
            links: Vec::new(),
            code: None,
            html: None,
            table: None,
        }
    }

    fn line_of(&self, offset: usize) -> usize {
        self.base + self.line_starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    fn event(&mut self, event: Event, line: usize) {
        match event {
            Event::Start(tag) => self.start(tag, line),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, body, _)) = &mut self.code {
                    body.push_str(&t);
                } else if let Some((body, _)) = &mut self.html {
                    body.push_str(&t);
                } else {
                    self.push(&t, false, line);
                }
            }
            Event::Code(t) => self.push(&t, true, line),
            Event::Html(t) => match &mut self.html {
                Some((body, _)) => body.push_str(&t),
                None => self.push(&t, false, line),
            },
            Event::InlineHtml(t) => self.push(&t, false, line),
            Event::SoftBreak => self.push(" ", false, line),
            Event::HardBreak => self.push("\n", false, line),
            Event::Rule => {
                self.flush();
                self.emit(Kind::Rule, line);
            }
            Event::TaskListMarker(done) => {
                if let Some((m, _)) = &mut self.pending {
                    *m = Marker::Task(done);
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag, line: usize) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.open = Some((Open::Paragraph, line, Vec::new()));
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.open = Some((Open::Heading(level as u8), line, Vec::new()));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().map(str::to_string),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new(), line));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.html = Some((String::new(), line));
            }
            Tag::List(first) => {
                self.flush();
                self.take_pending();
                self.lists.push(first);
            }
            Tag::Item => {
                self.flush();
                self.take_pending();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        Marker::Number(*n - 1)
                    }
                    _ => Marker::Bullet,
                };
                self.pending = Some((marker, line));
            }
            Tag::Table(_) => {
                self.flush();
                self.table = Some(TableBuild { line, ..TableBuild::default() });
            }
            Tag::TableCell => self.open = Some((Open::Cell, line, Vec::new())),
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            // An email autolink's target is the bare address.
            Tag::Link { link_type: LinkType::Email, dest_url, .. } => self.links.push(format!("mailto:{dest_url}")),
            Tag::Link { dest_url, .. } => self.links.push(dest_url.to_string()),
            Tag::Image { .. } => self.image += 1,
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if let Some((lang, mut text, line)) = self.code.take() {
                    if text.ends_with('\n') {
                        text.pop();
                    }
                    self.emit(Kind::Code { lang, text }, line);
                }
            }
            TagEnd::HtmlBlock => {
                if let Some((text, line)) = self.html.take() {
                    self.emit(Kind::Html(text.trim_end().to_string()), line);
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.take_pending();
            }
            TagEnd::TableCell => {
                if let (Some((Open::Cell, _, cell)), Some(t)) = (self.open.take(), &mut self.table) {
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.head = std::mem::take(&mut t.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = &mut self.table {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.emit(Kind::Table { head: t.head, rows: t.rows }, t.line);
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.links.pop();
            }
            TagEnd::Image => self.image = self.image.saturating_sub(1),
            _ => {}
        }
    }

    /// Append inline text to the open container, opening a paragraph for
    /// text with none (a tight list item's).
    fn push(&mut self, text: &str, code: bool, line: usize) {
        let style = Style {
            bold: self.bold > 0,
            italic: self.italic > 0,
            strike: self.strike > 0,
            code,
            image: self.image > 0,
        };
        let link = self.links.last().cloned();
        let (_, _, runs) = self.open.get_or_insert_with(|| (Open::Paragraph, line, Vec::new()));
        match runs.last_mut() {
            Some(last) if last.style == style && last.link == link => last.text.push_str(text),
            _ => runs.push(Inline { text: text.to_string(), style, link }),
        }
    }

    /// Close the open heading or paragraph into a block.
    fn flush(&mut self) {
        match self.open.take() {
            Some((Open::Heading(level), line, runs)) => self.emit(Kind::Heading(level, runs), line),
            Some((Open::Paragraph, line, runs)) => self.emit(Kind::Paragraph(runs), line),
            // A cell closes only at its end.
            cell @ Some((Open::Cell, ..)) => self.open = cell,
            None => {}
        }
    }

    /// An item's marker with nothing to sit on yet (an empty item, or one
    /// opening on a nested list) gets an empty item of its own.
    fn take_pending(&mut self) {
        if let Some((marker, line)) = self.pending.take() {
            self.push_block(Kind::Item(marker, Vec::new()), line);
        }
    }

    /// Add a block; an item's pending marker goes on its first paragraph,
    /// or on an empty item before any other first block.
    fn emit(&mut self, kind: Kind, line: usize) {
        match (self.pending.take(), kind) {
            (Some((marker, at)), Kind::Paragraph(runs)) => self.push_block(Kind::Item(marker, runs), at),
            (Some((marker, at)), kind) => {
                self.push_block(Kind::Item(marker, Vec::new()), at);
                self.push_block(kind, line);
            }
            (None, kind) => self.push_block(kind, line),
        }
    }

    fn push_block(&mut self, kind: Kind, line: usize) {
        let indent = self.lists.len().min(u8::MAX as usize) as u8;
        self.blocks.push(Block { kind, line, indent, quote: self.quote });
    }
}

/// Front matter at the very start of `text` (after a byte order mark, if
/// any): a `---` line, a first line with text, then up to a closing `---`
/// or `...` line. Returns its body, the text after it, and how many lines
/// it took. Only here: a `---` later is a rule (or a setext underline).
fn front_matter(text: &str) -> Option<(String, &str, usize)> {
    let start = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = start.split_inclusive('\n');
    let fence = |l: &str| l.trim_end_matches(['\n', '\r']).trim_end().to_string();
    if fence(lines.next()?) != "---" {
        return None;
    }
    let mut body = Vec::new();
    let mut taken = start.find('\n')? + 1;
    for (i, line) in lines.enumerate() {
        taken += line.len();
        let l = fence(line);
        if i == 0 && l.trim().is_empty() {
            return None;
        }
        if i > 0 && (l == "---" || l == "...") {
            let rest = &start[taken..];
            return Some((body.join("\n"), rest, i + 2));
        }
        body.push(line.trim_end_matches(['\n', '\r']).to_string());
    }
    None
}

/// The plain text of `runs`.
pub fn plain(runs: &[Inline]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Kind> {
        parse(text).into_iter().map(|b| b.kind).collect()
    }

    fn text(runs: &[Inline]) -> String {
        plain(runs)
    }

    #[test]
    fn headings_keep_their_level_and_line() {
        let blocks = parse("# One\n\ntext\n\n### Three\nSetext\n---\n");
        let got: Vec<(String, usize)> = blocks
            .iter()
            .map(|b| match &b.kind {
                Kind::Heading(l, r) => (format!("h{l} {}", text(r)), b.line),
                Kind::Paragraph(r) => (format!("p {}", text(r)), b.line),
                k => (format!("{k:?}"), b.line),
            })
            .collect();
        assert_eq!(
            got,
            [("h1 One".into(), 0), ("p text".into(), 2), ("h3 Three".into(), 4), ("h2 Setext".into(), 5)]
        );
    }

    #[test]
    fn nested_lists_number_indent_and_mark_tasks() {
        let src = "- a\n  - b\n    1. c\n    2. d\n- [x] done\n- [ ] todo\n\n3. three\n4. four\n";
        let blocks = parse(src);
        let got: Vec<(Marker, String, u8, usize)> = blocks
            .iter()
            .map(|b| match &b.kind {
                Kind::Item(m, r) => (*m, text(r), b.indent, b.line),
                k => panic!("{k:?}"),
            })
            .collect();
        use Marker::*;
        assert_eq!(
            got,
            [
                (Bullet, "a".into(), 1, 0),
                (Bullet, "b".into(), 2, 1),
                (Number(1), "c".into(), 3, 2),
                (Number(2), "d".into(), 3, 3),
                (Task(true), "done".into(), 1, 4),
                (Task(false), "todo".into(), 1, 5),
                (Number(3), "three".into(), 1, 7),
                (Number(4), "four".into(), 1, 8),
            ]
        );
    }

    #[test]
    fn a_loose_items_later_paragraphs_and_code_follow_at_its_indent() {
        let blocks = parse("- first\n\n  second\n\n  ```sh\n  ls\n  ```\n- \n");
        let got: Vec<(String, u8)> = blocks
            .iter()
            .map(|b| match &b.kind {
                Kind::Item(m, r) => (format!("{m:?} {}", text(r)), b.indent),
                Kind::Paragraph(r) => (format!("p {}", text(r)), b.indent),
                Kind::Code { lang, text } => (format!("code {lang:?} {text}"), b.indent),
                k => (format!("{k:?}"), b.indent),
            })
            .collect();
        assert_eq!(
            got,
            [
                ("Bullet first".into(), 1),
                ("p second".into(), 1),
                ("code Some(\"sh\") ls".into(), 1),
                ("Bullet ".into(), 1),
            ]
        );
    }

    #[test]
    fn code_blocks_keep_the_info_strings_first_word_and_their_line() {
        let blocks = parse("text\n\n```rust ignore\nfn a() {}\n\nx\n```\n\n    indented\n");
        assert_eq!(blocks[1].kind, Kind::Code { lang: Some("rust".into()), text: "fn a() {}\n\nx".into() });
        assert_eq!(blocks[1].line, 2);
        assert_eq!(blocks[2].kind, Kind::Code { lang: None, text: "indented".into() });
        assert_eq!(blocks[2].line, 8);
    }

    #[test]
    fn tables_split_into_header_and_body_cells() {
        let blocks = parse("intro\n\n| Chord | Action |\n|---|:-:|\n| `Cmd+N` | new **ws** |\n| a | |\n");
        let Kind::Table { head, rows } = &blocks[1].kind else { panic!("{:?}", blocks[1]) };
        assert_eq!(blocks[1].line, 2);
        let cells = |r: &Vec<Vec<Inline>>| r.iter().map(|c| text(c)).collect::<Vec<_>>();
        assert_eq!(cells(head), ["Chord", "Action"]);
        assert_eq!(rows.iter().map(cells).collect::<Vec<_>>(), [vec!["Cmd+N", "new ws"], vec!["a", ""]]);
        assert!(rows[0][0][0].style.code);
        assert!(rows[0][1][1].style.bold);
    }

    #[test]
    fn inline_styles_links_and_images_become_runs() {
        let [Kind::Paragraph(runs)] = &kinds("a **b** *c* ~~d~~ `e` [f](g.md) ![h](i.png)\nj")[..] else {
            panic!()
        };
        let got: Vec<(&str, Style, Option<&str>)> =
            runs.iter().map(|r| (r.text.as_str(), r.style, r.link.as_deref())).collect();
        let s = Style::default();
        assert_eq!(
            got,
            [
                ("a ", s, None),
                ("b", Style { bold: true, ..s }, None),
                (" ", s, None),
                ("c", Style { italic: true, ..s }, None),
                (" ", s, None),
                ("d", Style { strike: true, ..s }, None),
                (" ", s, None),
                ("e", Style { code: true, ..s }, None),
                (" ", s, None),
                ("f", s, Some("g.md")),
                (" ", s, None),
                ("h", Style { image: true, ..s }, None),
                (" j", s, None),
            ]
        );
    }

    #[test]
    fn an_email_autolink_targets_mailto() {
        let [Kind::Paragraph(runs)] = &kinds("<me@x.org> <https://x.org>")[..] else { panic!() };
        let links: Vec<Option<&str>> = runs.iter().map(|r| r.link.as_deref()).collect();
        assert_eq!(links, [Some("mailto:me@x.org"), None, Some("https://x.org")]);
    }

    #[test]
    fn quotes_rules_and_html_are_blocks() {
        let blocks = parse("> quoted\n>\n> > deeper\n\n---\n\n<div>\nhi\n</div>\n");
        let got: Vec<(Kind, u8, usize)> = blocks.into_iter().map(|b| (b.kind, b.quote, b.line)).collect();
        let p = |s: &str| Kind::Paragraph(vec![Inline { text: s.into(), style: Style::default(), link: None }]);
        assert_eq!(
            got,
            [
                (p("quoted"), 1, 0),
                (p("deeper"), 2, 2),
                (Kind::Rule, 0, 4),
                (Kind::Html("<div>\nhi\n</div>".into()), 0, 6),
            ]
        );
    }

    #[test]
    fn front_matter_is_one_block_not_a_rule_and_a_heading() {
        for text in ["---\ntitle: A\ntags: [x]\n---\n# Body\n", "\u{feff}---\r\ntitle: A\r\ntags: [x]\r\n...\r\n# Body\r\n"] {
            let got: Vec<(Kind, usize)> = parse(text).into_iter().map(|b| (b.kind, b.line)).collect();
            assert_eq!(got[0], (Kind::FrontMatter("title: A\ntags: [x]".into()), 0), "{text:?}");
            assert!(matches!(&got[1], (Kind::Heading(1, _), 4)), "{got:?}");
            assert_eq!(got.len(), 2);
        }
        // Not front matter: a blank first line, or no closing fence.
        for text in ["---\n\na: b\n---\n", "---\na: b\n"] {
            assert!(!parse(text).iter().any(|b| matches!(b.kind, Kind::FrontMatter(_))), "{text:?}");
        }
    }

    #[test]
    fn a_rule_before_a_heading_later_on_is_a_rule_and_a_heading() {
        let text = "# Title\n\nIntro.\n\n---\n## Part 2\n\nBody text.\n\n- a list\n\n---\n## Part 3\n";
        let got: Vec<(String, usize)> = parse(text)
            .into_iter()
            .map(|b| match b.kind {
                Kind::Heading(l, r) => (format!("h{l} {}", plain(&r)), b.line),
                Kind::Paragraph(r) => (format!("p {}", plain(&r)), b.line),
                Kind::Item(_, r) => (format!("li {}", plain(&r)), b.line),
                k => (format!("{k:?}"), b.line),
            })
            .collect();
        let want = [
            ("h1 Title", 0),
            ("p Intro.", 2),
            ("Rule", 4),
            ("h2 Part 2", 5),
            ("p Body text.", 7),
            ("li a list", 9),
            ("Rule", 11),
            ("h2 Part 3", 12),
        ];
        assert_eq!(got, want.map(|(s, l)| (s.to_string(), l)));
    }

    #[test]
    fn crlf_lines_count_as_lines() {
        let blocks = parse("# a\r\n\r\nb\r\n");
        assert_eq!(blocks.iter().map(|b| b.line).collect::<Vec<_>>(), [0, 2]);
    }
}
