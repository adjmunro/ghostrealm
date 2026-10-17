//! A document's outline: its section headings in order, as a highlighter
//! names them ([`Highlighter::heading_level`], [`Highlighter::heading_title`]),
//! for a table of contents to list and jump between.
//!
//! Finding the headings asks every line, threading the highlight state
//! through the document (a `#` line in a Markdown code block is code). An
//! [`Outline`] keeps the result per edit count and shares the view's
//! [`StateCache`], so after an edit only the lines from the edit on are
//! highlighted again; on a long document it waits for the edits to pause
//! ([`DEBOUNCE`]), keeping the last outline meanwhile.

use std::time::{Duration, Instant};

use crate::highlight::{Highlighter, StateCache};

/// One heading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineItem {
    /// Its line (0-based).
    pub line: usize,
    /// Its level, 1 the outermost.
    pub level: u8,
    /// Its title, without the markup that makes it a heading.
    pub text: String,
}

/// The headings of `lines` per `h`. `state_in(ln)` is the state line `ln`
/// enters with (from a [`StateCache`]), asked in ascending order and only
/// of a highlighter that carries state.
pub fn outline(h: &dyn Highlighter, lines: &[String], mut state_in: impl FnMut(usize) -> u32) -> Vec<OutlineItem> {
    let carries = h.carries_state();
    let mut items = Vec::new();
    for (ln, line) in lines.iter().enumerate() {
        let state = if carries { state_in(ln) } else { 0 };
        if let Some(level) = h.heading_level(line, state) {
            items.push(OutlineItem { line: ln, level, text: h.heading_title(line) });
        }
    }
    items
}

/// A document this many lines long or longer outlines again only once edits
/// pause for [`DEBOUNCE`]: a pass costs a hash of every line from the edit
/// on, too much to repeat at every keystroke.
pub const DEBOUNCE_LINES: usize = 2_000;
/// How long edits to a long document must pause before it outlines again.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// The outline of an edited document, kept between edits.
#[derive(Default)]
pub struct Outline {
    items: Vec<OutlineItem>,
    /// The edit count and highlighter `items` are of.
    of: Option<(u64, &'static str)>,
    /// An edit count not yet outlined, and when it was first seen.
    pending: Option<(u64, Instant)>,
}

impl Outline {
    /// The headings as last outlined (of an earlier edit count while a long
    /// document's edits haven't paused).
    pub fn items(&self) -> &[OutlineItem] {
        &self.items
    }

    /// Outline `lines` (at edit count `edits`) if they changed since: at
    /// once for a short document, a new highlighter, or the first time; for
    /// a long one, once [`DEBOUNCE`] has passed since the edit count was
    /// first seen. `states` must be the view's cache for `lines` (told of
    /// the edits). Returns whether the items changed.
    pub fn update(
        &mut self,
        h: &dyn Highlighter,
        lines: &[String],
        edits: u64,
        states: &mut StateCache,
        now: Instant,
    ) -> bool {
        let of = (edits, h.name());
        if self.of == Some(of) {
            return false;
        }
        let fresh = self.of.is_none_or(|(_, name)| name != h.name());
        if !fresh && lines.len() >= DEBOUNCE_LINES {
            let since = match self.pending {
                Some((e, at)) if e == edits => at,
                _ => {
                    self.pending = Some((edits, now));
                    now
                }
            };
            if now < since + DEBOUNCE {
                return false;
            }
        }
        self.pending = None;
        self.of = Some(of);
        let items = outline(h, lines, |ln| states.state_in(h, lines, edits, ln));
        let changed = items != self.items;
        self.items = items;
        changed
    }

    /// When a waiting outline is due ([`Self::update`] then outlines).
    pub fn due(&self) -> Option<Instant> {
        self.pending.map(|(_, at)| at + DEBOUNCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{markdown::Markdown, toml::Toml};

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_string).collect()
    }

    fn of(h: &dyn Highlighter, text: &str) -> Vec<(usize, u8, String)> {
        let lines = lines(text);
        let mut states = StateCache::default();
        let items = outline(h, &lines, |ln| states.state_in(h, &lines, 0, ln));
        items.into_iter().map(|i| (i.line, i.level, i.text)).collect()
    }

    fn item(line: usize, level: u8, text: &str) -> (usize, u8, String) {
        (line, level, text.to_string())
    }

    #[test]
    fn markdown_headings_by_level_without_their_markup() {
        let text = [
            "# Title",
            "",
            "text",
            "## Two ##",
            "### *Three* `x` [link](http://a)",
            "#### Four #",
            "##### five",
            "###### six",
            "####### seven",
            "#nospace",
            "## trailing \\#",
        ]
        .join("\n");
        assert_eq!(
            of(&Markdown, &text),
            vec![
                item(0, 1, "Title"),
                item(3, 2, "Two"),
                item(4, 3, "Three x link"),
                item(5, 4, "Four"),
                item(6, 5, "five"),
                item(7, 6, "six"),
                item(10, 2, "trailing \\#"),
            ]
        );
        assert_eq!(of(&Markdown, "#\n## ##\n##a##"), vec![item(0, 1, ""), item(1, 2, "")], "empty headings");
    }

    #[test]
    fn markdown_hashes_in_fences_quotes_and_lists_are_not_headings() {
        let text = [
            "# A",
            "```",
            "# code",
            "```",
            "> # quoted",
            "- # item",
            "  ```",
            "  # in the item's fence",
            "  ```",
            "    # indented code",
            "~~~md",
            "## tilde fence",
            "~~~",
            "# B",
        ]
        .join("\n");
        assert_eq!(of(&Markdown, &text), vec![item(0, 1, "A"), item(13, 1, "B")]);
        // An unclosed list item's fence ends where the item does.
        assert_eq!(of(&Markdown, "- ```\n  # code\n# After"), vec![item(2, 1, "After")]);
    }

    #[test]
    fn toml_tables_by_level_with_their_keys_as_written() {
        let text = [
            "top = 1",
            "[editor]",
            "x = 1",
            "  [editor.preview] # note",
            "[[bin]]",
            "[a.\"b.c\"]",
            "['q.r'.s.t]",
            "list = [",
            "  1,",
            "]",
            "# [comment]",
        ]
        .join("\n");
        assert_eq!(
            of(&Toml, &text),
            vec![
                item(1, 1, "editor"),
                item(3, 2, "editor.preview"),
                item(4, 2, "bin"),
                item(5, 2, "a.\"b.c\""),
                item(6, 3, "'q.r'.s.t"),
            ]
        );
    }

    #[test]
    fn a_short_document_outlines_at_every_edit_count() {
        let mut lines = lines("# A\ntext");
        let (mut o, mut states, now) = (Outline::default(), StateCache::default(), Instant::now());
        assert!(o.update(&Markdown, &lines, 0, &mut states, now));
        assert!(!o.update(&Markdown, &lines, 0, &mut states, now), "same edit count: nothing to do");
        lines[1] = "## B".into();
        states.dirty_from(1);
        assert!(o.update(&Markdown, &lines, 1, &mut states, now));
        assert_eq!(o.items().iter().map(|i| i.line).collect::<Vec<_>>(), [0, 1]);
        lines[1] = "## C".into();
        states.dirty_from(1);
        assert!(o.update(&Markdown, &lines, 2, &mut states, now), "a changed title is a change");
        lines.push(String::new());
        assert!(!o.update(&Markdown, &lines, 3, &mut states, now), "an edit leaving the headings be changes nothing");
        assert_eq!(o.due(), None);
    }

    #[test]
    fn a_long_document_outlines_again_once_edits_pause() {
        let heading = |i: usize| if i.is_multiple_of(100) { format!("# {i}") } else { "text".into() };
        let mut lines: Vec<String> = (0..DEBOUNCE_LINES).map(heading).collect();
        let (mut o, mut states, t0) = (Outline::default(), StateCache::default(), Instant::now());
        assert!(o.update(&Markdown, &lines, 0, &mut states, t0), "the first outline comes at once");
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100);
        lines[1] = "# new".into();
        states.dirty_from(1);
        assert!(!o.update(&Markdown, &lines, 1, &mut states, t0), "waits");
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100, "keeping the last outline");
        assert_eq!(o.due(), Some(t0 + DEBOUNCE));
        // Another edit restarts the wait.
        let t1 = t0 + DEBOUNCE / 2;
        lines[2] = "# newer".into();
        states.dirty_from(2);
        assert!(!o.update(&Markdown, &lines, 2, &mut states, t1));
        assert!(!o.update(&Markdown, &lines, 2, &mut states, t0 + DEBOUNCE), "measured from the latest edit");
        assert!(o.update(&Markdown, &lines, 2, &mut states, t1 + DEBOUNCE));
        assert_eq!(o.items().len(), DEBOUNCE_LINES / 100 + 2);
        assert_eq!(o.due(), None);
        // A new highlighter doesn't wait.
        assert!(o.update(&Toml, &lines, 3, &mut states, t1 + DEBOUNCE));
        assert!(o.items().is_empty());
    }
}
