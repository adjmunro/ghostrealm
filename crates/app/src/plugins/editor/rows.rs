//! The editor's visual rows as one paint lays them out: which text each
//! shows, where, and at what size. Rows differ in height (a heading drawn
//! larger), so every mapping between screen y and text goes through this.

use super::wrap::wrap_line;

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

/// How lines break into rows: soft-wrapped to the text width, or one row a line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    /// The text area's width and the base cell size (physical px).
    pub text_w: f32,
    pub cell: (f32, f32),
    pub wrap: bool,
}

impl RowLayout {
    /// Columns that fit the text area at `scale`.
    pub fn cols(&self, scale: f32) -> usize {
        ((self.text_w / (self.cell.0 * scale)).floor() as usize).max(1)
    }

    /// A row's height at `scale`.
    pub fn row_h(&self, scale: f32) -> f32 {
        self.cell.1 * scale
    }

    /// `line`'s rows at `scale`, as `(start, len)` in chars.
    pub fn rows(&self, line: &str, scale: f32) -> Vec<(usize, usize)> {
        if !self.wrap {
            return vec![(0, line.chars().count())];
        }
        wrap_line(line, self.cols(scale)).into_iter().map(|(text, start)| (start, text.chars().count())).collect()
    }
}

/// The rows one paint laid out, top to bottom from the scroll line, and what
/// they were laid out from (so an event after a scroll or an edit, before the
/// next paint, can tell they are stale).
#[derive(Clone, Debug, Default)]
pub struct RowTable {
    pub rows: Vec<VisualRow>,
    /// The buffer's scroll line and edit count, and the layout, they were
    /// built for.
    pub scroll: usize,
    pub edits: u64,
    pub layout: Option<RowLayout>,
}

impl RowTable {
    /// Lay out `lines` from line `scroll`, the first row's top at `top`,
    /// until a row starts at or past `bottom`. `scale_of(ln)` is line `ln`'s
    /// scale.
    pub fn build(
        lines: &[String],
        scroll: usize,
        edits: u64,
        layout: RowLayout,
        (top, bottom): (f32, f32),
        mut scale_of: impl FnMut(usize) -> f32,
    ) -> RowTable {
        let mut rows = Vec::new();
        let mut y = top;
        'lines: for (ln, line) in lines.iter().enumerate().skip(scroll) {
            let scale = scale_of(ln);
            let h = layout.row_h(scale);
            for (start, len) in layout.rows(line, scale) {
                if y >= bottom {
                    break 'lines;
                }
                rows.push(VisualRow { kind: RowKind::Text { ln, start, len }, y, h, scale });
                y += h;
            }
        }
        RowTable { rows, scroll, edits, layout: Some(layout) }
    }

    /// Whether this table was built for this scroll line, edit count and layout.
    pub fn fresh(&self, scroll: usize, edits: u64, layout: RowLayout) -> bool {
        self.scroll == scroll && self.edits == edits && self.layout == Some(layout)
    }

    /// The text rows, top to bottom.
    pub fn text_rows(&self) -> impl Iterator<Item = TextRow> + '_ {
        self.rows.iter().filter_map(|r| match r.kind {
            RowKind::Text { ln, start, len } => Some(TextRow { ln, start, len, y: r.y, h: r.h, scale: r.scale }),
            RowKind::Widget { .. } => None,
        })
    }

    /// The row covering height `y`: the first row for a `y` above it, `None`
    /// below the last.
    pub fn at(&self, y: f32) -> Option<&VisualRow> {
        let i = self.rows.partition_point(|r| r.y + r.h <= y);
        self.rows.get(i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: RowLayout = RowLayout { text_w: 80.0, cell: (8.0, 16.0), wrap: true };

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rows_stack_by_their_heights_and_wrap_at_their_scaled_columns() {
        let lines = lines(&["# a heading line", "plain", "x"]);
        let t = RowTable::build(&lines, 0, 0, LAYOUT, (0.0, 1000.0), |ln| if ln == 0 { 2.0 } else { 1.0 });
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
    fn layout_stops_at_the_first_row_starting_past_the_bottom() {
        let lines = lines(&["a", "b", "c", "d"]);
        let t = RowTable::build(&lines, 1, 0, LAYOUT, (10.0, 40.0), |_| 1.0);
        let shown: Vec<(usize, f32)> = t.text_rows().map(|r| (r.ln, r.y)).collect();
        assert_eq!(shown, vec![(1, 10.0), (2, 26.0)], "the second row ends past the bottom but starts above it");
        assert!(t.fresh(1, 0, LAYOUT));
        assert!(!t.fresh(2, 0, LAYOUT) && !t.fresh(1, 1, LAYOUT));
        assert!(!t.fresh(1, 0, RowLayout { wrap: false, ..LAYOUT }));
    }
}
