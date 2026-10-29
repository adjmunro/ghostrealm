//! The editor's visual rows as one paint lays them out: which text each
//! shows, where, and at what size. Rows differ in height (a heading drawn
//! larger) and a heading has space above it, so every mapping between
//! screen y and text goes through this.

use ghostrealm_core::Headings;

use super::wrap::wrap_line;

/// How a line draws: its scale, and the space above its first row (whole
/// physical px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineSize {
    pub scale: f32,
    pub space: f32,
}

impl LineSize {
    /// A body line: base size, nothing above.
    pub const BODY: LineSize = LineSize { scale: 1.0, space: 0.0 };
}

/// Each heading level's [`LineSize`] at one font size, from `[headings]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeadingSizes {
    scale: [f32; 6],
    space: [f32; 6],
}

impl HeadingSizes {
    /// No heading drawn differently: every level body size, nothing above.
    pub const FLAT: HeadingSizes = HeadingSizes { scale: [1.0; 6], space: [0.0; 6] };

    /// `cfg`'s sizes, its spaces in ems of `font_size` (physical px) rounded
    /// to whole pixels so the rows below stay on the base rows' pixel grid.
    pub fn new(cfg: &Headings, font_size: f32) -> Self {
        let level = |i: usize| Some(i as u8 + 1);
        HeadingSizes {
            scale: std::array::from_fn(|i| cfg.scale(level(i))),
            space: std::array::from_fn(|i| (cfg.space(level(i)) * font_size).round()),
        }
    }

    /// A line's size: its heading `level`'s, else [`LineSize::BODY`].
    pub fn size(&self, level: Option<u8>) -> LineSize {
        match level.map(usize::from) {
            Some(l @ 1..=6) => LineSize { scale: self.scale[l - 1], space: self.space[l - 1] },
            _ => LineSize::BODY,
        }
    }
}

/// What a visual row shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowKind {
    /// The chars `start..start + len` of line `ln` (an unwrapped row holds its
    /// whole line, drawn shifted by the horizontal scroll).
    Text { ln: usize, start: usize, len: usize },
    /// A widget of the view's own, `id`, between line `after_ln` and the next
    /// (the row's `h` tall). Nothing places one yet: it is where comment
    /// threads and deleted-line ribbons go.
    #[allow(dead_code)]
    Widget { after_ln: usize, id: u32 },
}

/// One row on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisualRow {
    pub kind: RowKind,
    /// Top, and height (physical px).
    pub y: f32,
    pub h: f32,
    /// How much larger than the base cell its text draws: a column is
    /// `cell_w * scale` wide.
    pub scale: f32,
}

/// A text row's fields, unpacked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextRow {
    pub ln: usize,
    pub start: usize,
    pub len: usize,
    pub y: f32,
    pub h: f32,
    pub scale: f32,
}

impl TextRow {
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// How lines break into rows: soft-wrapped to the text width, or one row a
/// line; and how large a heading's rows are.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    /// The text area's width and the base cell size (physical px).
    pub text_w: f32,
    pub cell: (f32, f32),
    pub wrap: bool,
    pub headings: HeadingSizes,
}

impl RowLayout {
    /// Columns that fit the text area at `scale`.
    pub fn cols(&self, scale: f32) -> usize {
        ((self.text_w / (self.cell.0 * scale)).floor() as usize).max(1)
    }

    /// A row's height at `scale`: whole pixels for a scaled row, so the
    /// rows below it stay on the base rows' pixel grid (its columns keep the
    /// exact scale).
    pub fn row_h(&self, scale: f32) -> f32 {
        if scale == 1.0 {
            self.cell.1
        } else {
            (self.cell.1 * scale).round()
        }
    }

    /// `line`'s rows at `scale`, as `(start, len)` in chars.
    pub fn rows(&self, line: &str, scale: f32) -> Vec<(usize, usize)> {
        if !self.wrap {
            return vec![(0, line.chars().count())];
        }
        wrap_line(line, self.cols(scale)).into_iter().map(|(text, start)| (start, text.chars().count())).collect()
    }

    /// `line`'s height at `size` below the top line: the space above it and
    /// its rows.
    pub fn line_h(&self, line: &str, size: LineSize) -> f32 {
        size.space + self.rows(line, size.scale).len() as f32 * self.row_h(size.scale)
    }
}

/// The rows one paint laid out, top to bottom from the scroll line, and what
/// they were laid out from (so an event after a scroll or an edit, before the
/// next paint, can tell they are stale).
#[derive(Clone, Debug, Default)]
pub struct RowTable {
    pub rows: Vec<VisualRow>,
    /// The buffer's scroll line and edit count, the layout, and the
    /// highlighter (which sizes the headings) they were built for.
    pub scroll: usize,
    pub edits: u64,
    pub layout: Option<RowLayout>,
    pub highlighter: Option<&'static str>,
}

impl RowTable {
    /// Lay out `lines` from line `scroll`, the first row's top at `top`,
    /// until a row starts at or past `bottom`. `size_of(ln)` is line `ln`'s
    /// size under the highlighter named `highlighter`. A line's space goes
    /// above its first row, except on the top line: the view starts at its
    /// text. The space belongs to no row, so [`at`](Self::at) gives the line
    /// below it.
    pub fn build(
        lines: &[String],
        scroll: usize,
        edits: u64,
        layout: RowLayout,
        highlighter: Option<&'static str>,
        (top, bottom): (f32, f32),
        mut size_of: impl FnMut(usize) -> LineSize,
    ) -> RowTable {
        let mut rows = Vec::new();
        let mut y = top;
        'lines: for (ln, line) in lines.iter().enumerate().skip(scroll) {
            let LineSize { scale, space } = size_of(ln);
            if ln != scroll {
                y += space;
            }
            let h = layout.row_h(scale);
            for (start, len) in layout.rows(line, scale) {
                if y >= bottom {
                    break 'lines;
                }
                rows.push(VisualRow { kind: RowKind::Text { ln, start, len }, y, h, scale });
                y += h;
            }
        }
        RowTable { rows, scroll, edits, layout: Some(layout), highlighter }
    }

    /// Whether this table was built for this scroll line, edit count, layout
    /// and highlighter.
    pub fn fresh(&self, scroll: usize, edits: u64, layout: RowLayout, highlighter: Option<&'static str>) -> bool {
        self.scroll == scroll && self.edits == edits && self.layout == Some(layout) && self.highlighter == highlighter
    }

    /// The text rows, top to bottom.
    pub fn text_rows(&self) -> impl Iterator<Item = TextRow> + '_ {
        self.rows.iter().filter_map(|r| match r.kind {
            RowKind::Text { ln, start, len } => Some(TextRow { ln, start, len, y: r.y, h: r.h, scale: r.scale }),
            RowKind::Widget { .. } => None,
        })
    }

    /// The row covering height `y`, or the first below it (in a heading's
    /// space, the heading's): the first row for a `y` above it, `None` below
    /// the last.
    pub fn at(&self, y: f32) -> Option<&VisualRow> {
        let i = self.rows.partition_point(|r| r.y + r.h <= y);
        self.rows.get(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: RowLayout = RowLayout { text_w: 80.0, cell: (8.0, 16.0), wrap: true, headings: HeadingSizes::FLAT };

    fn scaled(scale: f32) -> LineSize {
        LineSize { scale, space: 0.0 }
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rows_stack_by_their_heights_and_wrap_at_their_scaled_columns() {
        let lines = lines(&["# a heading line", "plain", "x"]);
        let t = RowTable::build(&lines, 0, 0, LAYOUT, None, (0.0, 1000.0), |ln| scaled(if ln == 0 { 2.0 } else { 1.0 }));
        let rows: Vec<(usize, usize, usize, f32, f32)> =
            t.text_rows().map(|r| (r.ln, r.start, r.len, r.y, r.h)).collect();
        // 10 base columns; 5 at scale 2.
        assert_eq!(
            rows,
            vec![
                (0, 0, 4, 0.0, 32.0),
                (0, 4, 5, 32.0, 32.0),
                (0, 9, 3, 64.0, 32.0),
                (0, 12, 4, 96.0, 32.0),
                (1, 0, 5, 128.0, 16.0),
                (2, 0, 1, 144.0, 16.0),
            ]
        );
        assert_eq!(t.at(-5.0).map(|r| r.y), Some(0.0), "above the first row");
        assert_eq!(t.at(31.9).map(|r| r.y), Some(0.0));
        assert_eq!(t.at(32.0).map(|r| r.y), Some(32.0));
        assert_eq!(t.at(130.0).map(|r| r.y), Some(128.0));
        assert_eq!(t.at(160.0), None, "below the last row");
    }

    #[test]
    fn a_scaled_row_is_whole_pixels_tall_and_keeps_exact_columns() {
        let layout = RowLayout { text_w: 100.0, ..LAYOUT };
        assert_eq!(layout.row_h(1.8), 29.0);
        assert_eq!(layout.row_h(1.1), 18.0);
        assert_eq!(layout.row_h(1.0), 16.0);
        assert_eq!(layout.cols(1.8), 6, "100 / 14.4");
        let lines = lines(&["# h", "x"]);
        let t = RowTable::build(&lines, 0, 0, layout, None, (0.0, 100.0), |ln| scaled(if ln == 0 { 1.8 } else { 1.0 }));
        assert_eq!(t.text_rows().map(|r| r.y).collect::<Vec<_>>(), [0.0, 29.0]);
    }

    #[test]
    fn layout_stops_at_the_first_row_starting_past_the_bottom() {
        let lines = lines(&["a", "b", "c", "d"]);
        let t = RowTable::build(&lines, 1, 0, LAYOUT, Some("md"), (10.0, 40.0), |_| LineSize::BODY);
        let shown: Vec<(usize, f32)> = t.text_rows().map(|r| (r.ln, r.y)).collect();
        assert_eq!(shown, vec![(1, 10.0), (2, 26.0)], "the second row ends past the bottom but starts above it");
        assert!(t.fresh(1, 0, LAYOUT, Some("md")));
        assert!(!t.fresh(2, 0, LAYOUT, Some("md")) && !t.fresh(1, 1, LAYOUT, Some("md")));
        assert!(!t.fresh(1, 0, RowLayout { wrap: false, ..LAYOUT }, Some("md")));
        assert!(!t.fresh(1, 0, LAYOUT, None), "another highlighter sizes other headings");
    }

    #[test]
    fn heading_sizes_come_from_the_config_in_whole_pixels() {
        let cfg = Headings { h2_scale: 1.5, h2_space: 1.3, ..Headings::default() };
        let sizes = HeadingSizes::new(&cfg, 15.0);
        assert_eq!(sizes.size(Some(2)), LineSize { scale: 1.5, space: 20.0 }, "19.5 rounds");
        assert_eq!(sizes.size(Some(1)).scale, Headings::default().h1_scale);
        assert_eq!(sizes.size(None), LineSize::BODY);
        assert_eq!(sizes.size(Some(7)), LineSize::BODY, "past 6 is body text");
        assert_eq!(HeadingSizes::FLAT.size(Some(1)), LineSize::BODY);
    }

    #[test]
    fn a_heading_has_its_space_above_it_unless_it_is_the_top_line() {
        let lines = lines(&["# top", "body", "## sub", "after"]);
        let size = |ln: usize| match ln {
            0 => LineSize { scale: 2.0, space: 10.0 },
            2 => LineSize { scale: 1.5, space: 6.0 },
            _ => LineSize::BODY,
        };
        let t = RowTable::build(&lines, 0, 0, LAYOUT, None, (0.0, 1000.0), size);
        let tops: Vec<(usize, f32, f32)> = t.text_rows().map(|r| (r.ln, r.y, r.h)).collect();
        // The top line starts at the top; "## sub" 6 px below "body".
        assert_eq!(tops, vec![(0, 0.0, 32.0), (1, 32.0, 16.0), (2, 54.0, 24.0), (3, 78.0, 16.0)]);
        assert_eq!(t.at(47.9).map(|r| r.y), Some(32.0), "body's row");
        assert_eq!(t.at(48.0).map(|r| r.y), Some(54.0), "the space above a heading takes it");
        assert_eq!(t.at(53.9).map(|r| r.y), Some(54.0));
        assert_eq!(t.at(78.0).map(|r| r.y), Some(78.0));
        assert_eq!(LAYOUT.line_h("## sub", size(2)), 30.0, "space and row");

        // Scrolled to it, the heading starts at the top.
        let t = RowTable::build(&lines, 2, 0, LAYOUT, None, (0.0, 1000.0), size);
        assert_eq!(t.text_rows().map(|r| r.y).collect::<Vec<_>>(), [0.0, 24.0]);
    }

    #[test]
    fn other_heading_sizes_make_a_table_stale() {
        let t = RowTable::build(&lines(&["a"]), 0, 0, LAYOUT, None, (0.0, 100.0), |_| LineSize::BODY);
        let sized = RowLayout { headings: HeadingSizes::new(&Headings::default(), 15.0), ..LAYOUT };
        assert!(t.fresh(0, 0, LAYOUT, None));
        assert!(!t.fresh(0, 0, sized, None), "a config reload resizes the headings");
    }
}
