//! The terminal plugin: a shell (or command) on a PTY, parsed by the VT engine
//! on a worker thread ([`ThreadedTerminal`]) and drawn from grid snapshots.
//!
//! This is the hot path. Rows go through the shared row cache under the frame's
//! shaping budget; a still pane reuses its last snapshot; the PTY is resized only
//! when the cell grid actually changes; and wheel scrolling is eased over several
//! frames so the viewport never outruns shaping.

use std::any::Any;

use anyhow::Result;
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Rect, Registry};
use ghostrealm_terminal::{Cell, Grid, Key, KeyPress, Lifecycle, Mods, Pumped, Scroll, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, ThreadedTerminal};

use crate::app_state::AppState;
use crate::plugin::{EventCx, MouseEvent, OpenCx, PaintCx, Plugin, Request, View};

pub const ID: &str = "terminal";

/// Grid size a terminal starts at, before its first paint sizes it to its pane.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const DEFAULT_CELL_W: u32 = 8;
const DEFAULT_CELL_H: u32 = 16;
/// Wheel easing: each frame moves the viewport by `pending / EASE_DIVISOR`
/// lines, clamped to `[STEP_MIN, STEP_MAX]`. Small scrolls stay slow and
/// coherent (no torn, half-shaped viewport); a big flick eases out fast enough
/// to cross a large scrollback in a beat. `STEP_MAX` trades catch-up speed
/// against how far a fling can outrun shaping.
const SCROLL_STEP_MIN: i32 = 6;
const SCROLL_STEP_MAX: i32 = 40;
const SCROLL_EASE_DIVISOR: i32 = 3;
/// Cap on queued scroll lines, so flicking hard against the scrollback edge
/// can't pile up a backlog a reverse flick would first have to unwind.
const MAX_PENDING_SCROLL: i32 = 600;
/// Alpha of the block cursor drawn over the cell.
const CURSOR_ALPHA: f32 = 0.6;

pub struct TerminalPlugin;

impl Plugin for TerminalPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "Terminal"
    }

    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        let mut cmd = match cx.command.as_deref() {
            Some(line) => {
                let mut c = CommandBuilder::new("/bin/sh");
                c.arg("-c");
                c.arg(line);
                c
            }
            None => CommandBuilder::new_default_prog(),
        };
        if let Some(dir) = cx.cwd.as_ref().filter(|d| d.is_dir()) {
            cmd.cwd(dir);
        }
        // The VT runs on a worker thread; the waker pokes the UI when it
        // publishes a new grid.
        let term = ThreadedTerminal::spawn(
            DEFAULT_COLS,
            DEFAULT_ROWS,
            DEFAULT_CELL_W,
            DEFAULT_CELL_H,
            Some(cmd),
            cx.waker.clone(),
        )?;
        Ok(Box::new(TerminalView::new(term)))
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        r.register(
            CommandMeta::new("terminal.new", "New Terminal", "Open a terminal in the focused pane"),
            Box::new(|s: &mut AppState, _| {
                s.open_plugin_in_focused(ID)
                    .map_err(|e| CmdError::Failed(format!("{e:#}")))?;
                Ok(CmdOutcome::ok())
            }),
        );
    }
}

/// A text selection within the viewport, in cell coordinates `(col, row)`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Selection {
    /// Where the drag began.
    anchor: (u16, u16),
    /// Where it currently ends.
    head: (u16, u16),
}

impl Selection {
    /// (start, end), row-major ordered.
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor.1, self.anchor.0);
        let h = (self.head.1, self.head.0);
        if a <= h {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
}

pub struct TerminalView {
    term: ThreadedTerminal,
    /// The last snapshot, reused while the terminal reports no change so a
    /// still pane costs no snapshot.
    grid: Grid,
    snapshotted: bool,
    /// `(cols, rows, cell_w, cell_h)` the PTY was last sized to; a paint resizes
    /// only when this changes.
    geom: Option<(u16, u16, u32, u32)>,
    /// The content rect of the last paint, for mapping the mouse to cells.
    rect: Rect,
    cell: (f32, f32),
    selection: Option<Selection>,
    /// The cell a press landed on; a selection starts only once it's dragged.
    press_cell: Option<(u16, u16)>,
    /// Sub-line wheel remainder (px) carried between events, so a slow drag
    /// isn't rounded to nothing and momentum tails aren't dropped.
    scroll_accum: f32,
    /// Whole lines still to scroll, eased out a step per frame (negative
    /// reveals older history, like `Scroll::Delta`).
    pending_scroll: i32,
}

impl TerminalView {
    pub fn new(term: ThreadedTerminal) -> Self {
        TerminalView {
            term,
            grid: Grid::empty(),
            snapshotted: false,
            geom: None,
            rect: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            cell: (DEFAULT_CELL_W as f32, DEFAULT_CELL_H as f32),
            selection: None,
            press_cell: None,
            scroll_accum: 0.0,
            pending_scroll: 0,
        }
    }

    /// The child's state (running, or exited with a code).
    pub fn lifecycle(&mut self) -> Lifecycle {
        self.term.lifecycle()
    }

    /// Advance the viewport toward the queued scroll by one eased step. Returns
    /// whether more remains.
    fn step_scroll(&mut self) -> bool {
        if self.pending_scroll == 0 {
            return false;
        }
        let mag = self.pending_scroll.abs();
        let step = (mag / SCROLL_EASE_DIVISOR)
            .clamp(SCROLL_STEP_MIN, SCROLL_STEP_MAX)
            .min(mag)
            * self.pending_scroll.signum();
        self.term.scroll(Scroll::Delta(step));
        self.pending_scroll -= step;
        self.pending_scroll != 0
    }

    /// The cell under a point, if it's inside the last painted grid.
    fn cell_at(&self, x: f32, y: f32) -> Option<(u16, u16)> {
        let (r, (cw, ch)) = (self.rect, self.cell);
        let g = &self.grid;
        if !crate::plugin::rect_contains(r, x, y) || g.size.cols == 0 || g.size.rows == 0 {
            return None;
        }
        let col = (((x - r.x) / cw).floor().max(0.0) as u16).min(g.size.cols - 1);
        let row = (((y - r.y) / ch).floor().max(0.0) as u16).min(g.size.rows - 1);
        Some((col, row))
    }

    /// Input typed into the shell: snap the viewport back to the live bottom
    /// (as terminals do) and drop any selection and eased scroll.
    fn before_input(&mut self) {
        self.selection = None;
        self.pending_scroll = 0;
        self.term.scroll(Scroll::Bottom);
    }

    fn write(&mut self, bytes: &[u8]) {
        self.before_input();
        self.term.write_bytes(bytes);
    }

    fn copy_selection(&self, cx: &mut EventCx) {
        if let Some(sel) = self.selection.filter(|s| !s.is_empty()) {
            let text = selection_text(&self.grid, sel);
            if !text.is_empty() {
                cx.set_clipboard_text(text);
            }
        }
    }

    /// Extend (or start, at the cursor) a keyboard selection by one cell, or one
    /// word with `by_word`, toward `key`'s arrow direction.
    fn extend_selection(&mut self, key: Key, by_word: bool) {
        let g = &self.grid;
        let (cols, rows) = (g.size.cols, g.size.rows);
        if cols == 0 || rows == 0 {
            return;
        }
        let (anchor, mut head) = match self.selection {
            Some(s) => (s.anchor, s.head),
            None => {
                let c = (g.cursor.col.min(cols - 1), g.cursor.row.min(rows - 1));
                (c, c)
            }
        };
        match key {
            Key::Left if by_word => head.0 = word_col(g, head.1, head.0, false),
            Key::Left => head.0 = head.0.saturating_sub(1),
            Key::Right if by_word => head.0 = word_col(g, head.1, head.0, true),
            Key::Right => head.0 = (head.0 + 1).min(cols - 1),
            Key::Up => head.1 = head.1.saturating_sub(1),
            Key::Down => head.1 = (head.1 + 1).min(rows - 1),
            _ => return,
        }
        self.selection = Some(Selection { anchor, head });
    }

    /// Cmd chords the app didn't bind: line start/end (arrows) and the
    /// clipboard. Everything else is swallowed — Cmd never reaches the shell.
    fn cmd_key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        match k.key {
            // Readline line start / end.
            Key::Left | Key::Up => self.write(&[0x01]),
            Key::Right | Key::Down => self.write(&[0x05]),
            Key::Char(c) if c.eq_ignore_ascii_case(&'c') => self.copy_selection(cx),
            Key::Char(c) if c.eq_ignore_ascii_case(&'v') => match cx.clipboard_text() {
                Some(text) if !text.is_empty() => self.write(text.as_bytes()),
                _ => return false,
            },
            _ => return false,
        }
        true
    }
}

impl View for TerminalView {
    fn plugin(&self) -> &'static str {
        ID
    }

    fn title(&self) -> Option<String> {
        self.term.title()
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        let (cw, ch) = (cx.ui.cell_w, cx.ui.cell_h);
        self.rect = rect;
        self.cell = (cw, ch);
        if self.step_scroll() {
            cx.request_frame();
        }
        // Resize the PTY only when the cell grid (or cell size) changes.
        let cols = ((rect.w / cw).floor() as u16).max(1);
        let rows = ((rect.h / ch).floor() as u16).max(1);
        let geom = (cols, rows, cw.round() as u32, ch.round() as u32);
        if self.geom != Some(geom) {
            self.term.resize(cols, rows, geom.2, geom.3);
            self.geom = Some(geom);
        }
        // Snapshot only when the grid changed, reusing the previous grid's
        // allocations as the target.
        if self.term.needs_snapshot() || !self.snapshotted {
            self.term.snapshot_into(&mut self.grid);
            self.snapshotted = true;
        }

        let grid = &self.grid;
        cx.fill(rect, grid.default_bg);
        if let Some(sel) = self.selection {
            for row in 0..grid.size.rows {
                if let Some((first, last)) = selection_row_span(sel, row, grid.size.cols) {
                    let r = Rect {
                        x: rect.x + first as f32 * cw,
                        y: rect.y + row as f32 * ch,
                        w: (last - first + 1) as f32 * cw,
                        h: ch,
                    };
                    cx.fill_alpha(r, cx.chrome.accent, 0.35);
                }
            }
        }
        for row in 0..grid.size.rows {
            for col in 0..grid.size.cols {
                if let Some(c) = grid.cell(col, row) {
                    if c.bg != grid.default_bg {
                        cx.fill(cell_rect(rect, col, row, cw, ch), c.bg);
                    }
                }
            }
            // Keyed over the inked prefix only: trailing blanks neither cost
            // shaping nor split otherwise-identical rows in the cache. The key
            // hashes cells in place; spans are built only on a miss.
            let len = row_content_len(grid, row);
            let key = cx.row_key((0..len).map(|col| match grid.cell(col, row) {
                Some(c) => (c.text.as_str(), c.fg),
                None => ("", grid.default_fg),
            }));
            let top = rect.y + row as f32 * ch;
            cx.budgeted_row(key, || row_spans(grid, row), rect.x, top, rect.w, rect, grid.default_fg);
        }
        if grid.cursor.visible {
            let fg = grid
                .cell(grid.cursor.col, grid.cursor.row)
                .map(|c| c.fg)
                .unwrap_or(grid.default_fg);
            cx.fill_top(cell_rect(rect, grid.cursor.col, grid.cursor.row, cw, ch), fg, CURSOR_ALPHA);
        }
    }

    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        let m = k.mods;
        if m.super_ {
            let used = self.cmd_key(cx, k);
            cx.redraw();
            return used;
        }
        cx.redraw();
        if m.shift {
            // Shift + Page/Home/End drive the scrollback.
            let page = (self.grid.size.rows as i32 - 1).max(1);
            let scroll = match k.key {
                Key::PageUp => Some(Scroll::Delta(-page)),
                Key::PageDown => Some(Scroll::Delta(page)),
                Key::Home => Some(Scroll::Top),
                Key::End => Some(Scroll::Bottom),
                _ => None,
            };
            if let Some(s) = scroll {
                self.pending_scroll = 0;
                self.term.scroll(s);
                return true;
            }
            // Shift+Arrow selects over the grid (word-wise with Alt) instead of
            // corrupting the shell input.
            if matches!(k.key, Key::Left | Key::Right | Key::Up | Key::Down) {
                self.extend_selection(k.key, m.alt);
                return true;
            }
            // Shift+Enter sends a bare LF — not the Enter path, so it never
            // shows the optimistic busy dot. Whether the shell treats LF as a
            // continuation or a submit is its line editor's business.
            if k.key == Key::Enter {
                self.write(b"\n");
                return true;
            }
        }
        if m.alt {
            // Option+Left/Right = readline word motion (ESC-b / ESC-f);
            // Option+Up/Down send a plain arrow, not a corrupting modified CSI.
            match k.key {
                Key::Left => {
                    self.write(b"\x1bb");
                    return true;
                }
                Key::Right => {
                    self.write(b"\x1bf");
                    return true;
                }
                Key::Up | Key::Down => {
                    self.before_input();
                    self.term.send_key(&KeyPress {
                        key: k.key,
                        mods: Mods::default(),
                        text: None,
                    });
                    return true;
                }
                _ => {}
            }
        }
        self.before_input();
        self.term.send_key(k);
        // Submitting a command: show busy now; the process-group check catches
        // up within the grace.
        if k.key == Key::Enter {
            cx.request(Request::Busy);
        }
        true
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        match *event {
            // A press only records the anchor; a plain click never highlights.
            MouseEvent::Down { pos: (x, y), .. } => {
                if self.selection.take().is_some() {
                    cx.redraw();
                }
                self.press_cell = self.cell_at(x, y);
            }
            MouseEvent::Drag { pos: (x, y) } => {
                let (Some(anchor), Some(head)) = (self.press_cell, self.cell_at(x, y)) else {
                    return;
                };
                let sel = Some(Selection { anchor, head });
                if self.selection != sel {
                    self.selection = sel;
                    cx.redraw();
                }
            }
            // Finishing a drag copies the selection, as terminals do.
            MouseEvent::Up { dragged, .. } => {
                if dragged {
                    self.copy_selection(cx);
                }
                self.press_cell = None;
            }
            MouseEvent::Move { .. } => {}
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, _pos: (f32, f32), _dx: f32, dy: f32) {
        let ch = cx.ui.cell_h;
        self.scroll_accum += dy;
        let lines = (self.scroll_accum / ch).trunc() as i32;
        self.scroll_accum -= lines as f32 * ch;
        if lines == 0 {
            return;
        }
        // Queue the motion instead of jumping: paint eases it out a bounded step
        // per frame, so each frame only reveals what shaping can finish. Wheel up
        // (positive) reveals older history, a negative viewport delta.
        let delta = -lines;
        self.pending_scroll = if self.pending_scroll != 0 && (self.pending_scroll > 0) != (delta > 0) {
            // A reversal drops the opposing backlog so it responds at once.
            delta
        } else {
            self.pending_scroll.saturating_add(delta)
        }
        .clamp(-MAX_PENDING_SCROLL, MAX_PENDING_SCROLL);
        // The selection is viewport-relative; scrolling invalidates it.
        self.selection = None;
        cx.request_frame();
    }

    fn pump(&mut self, budget: usize) -> Pumped {
        self.term.pump_budgeted(budget)
    }

    fn is_busy(&self) -> bool {
        self.term.is_busy()
    }

    fn write_input(&mut self, bytes: &[u8]) -> bool {
        self.term.write_bytes(bytes);
        true
    }

    fn text(&mut self) -> Option<String> {
        self.term.pump();
        let grid = self.term.snapshot();
        let mut out = String::new();
        for row in 0..grid.size.rows {
            let mut line = String::new();
            for col in 0..grid.size.cols {
                match grid.cell(col, row) {
                    Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                    _ => line.push(' '),
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        Some(out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Absolute pixel rect of cell (col, row) inside `area`.
fn cell_rect(area: Rect, col: u16, row: u16, cell_w: f32, cell_h: f32) -> Rect {
    Rect {
        x: area.x + col as f32 * cell_w,
        y: area.y + row as f32 * cell_h,
        w: cell_w,
        h: cell_h,
    }
}

/// A row's cells grouped into (text, fg-colour) runs of one colour.
fn row_spans(grid: &Grid, row: u16) -> Vec<(String, [u8; 3])> {
    let mut spans: Vec<(String, [u8; 3])> = Vec::new();
    // Stop at the last inked column: trailing blanks draw nothing in the text
    // pass (their background is a separate quad), so shaping them is wasted work.
    for col in 0..row_content_len(grid, row) {
        let (ch, fg) = match grid.cell(col, row) {
            Some(Cell { text, fg, .. }) if !text.is_empty() => (text.clone(), *fg),
            _ => (" ".to_string(), grid.default_fg),
        };
        match spans.last_mut() {
            Some((s, c)) if *c == fg => s.push_str(&ch),
            _ => spans.push((ch, fg)),
        }
    }
    spans
}

/// Columns up to and including the last cell with visible ink on `row`. Both
/// the content key and the shaping stop here, which also lifts the cache hit
/// rate: rows that differ only in trailing blanks share one shaped row.
fn row_content_len(grid: &Grid, row: u16) -> u16 {
    let mut len = 0u16;
    for col in 0..grid.size.cols {
        let inked = grid
            .cell(col, row)
            .map(|c| c.text.chars().any(|ch| !ch.is_whitespace()))
            .unwrap_or(false);
        if inked {
            len = col + 1;
        }
    }
    len
}

/// The next word boundary column on `row` from `col`, moving right (`forward`)
/// or left. A word is a run of non-blank cells.
fn word_col(grid: &Grid, row: u16, col: u16, forward: bool) -> u16 {
    let cols = grid.size.cols;
    if cols == 0 {
        return 0;
    }
    let blank = |c: u16| {
        grid.cell(c, row)
            .map(|cell| cell.text.trim().is_empty())
            .unwrap_or(true)
    };
    let mut c = col;
    if forward {
        // Skip the current word, then the gap, landing on the next word's start.
        while c < cols - 1 && !blank(c) {
            c += 1;
        }
        while c < cols - 1 && blank(c) {
            c += 1;
        }
    } else {
        while c > 0 && blank(c - 1) {
            c -= 1;
        }
        while c > 0 && !blank(c - 1) {
            c -= 1;
        }
    }
    c
}

/// The text of a selection over `grid`, row-major, trailing spaces trimmed per
/// line and rows joined with newlines.
fn selection_text(grid: &Grid, sel: Selection) -> String {
    let ((sc, sr), (ec, er)) = sel.ordered();
    let last_col = grid.size.cols.saturating_sub(1);
    let mut out = String::new();
    for row in sr..=er.min(grid.size.rows.saturating_sub(1)) {
        let first = if row == sr { sc } else { 0 };
        let last = if row == er { ec } else { last_col };
        let mut line = String::new();
        for col in first..=last.min(last_col) {
            match grid.cell(col, row) {
                Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                _ => line.push(' '),
            }
        }
        out.push_str(line.trim_end());
        if row != er {
            out.push('\n');
        }
    }
    out
}

/// The inclusive column span `[first, last]` of a selection on `row`, if the row
/// is within it.
fn selection_row_span(sel: Selection, row: u16, cols: u16) -> Option<(u16, u16)> {
    let ((sc, sr), (ec, er)) = sel.ordered();
    if row < sr || row > er || cols == 0 {
        return None;
    }
    let first = if row == sr { sc } else { 0 };
    let last = if row == er { ec } else { cols - 1 };
    Some((first.min(cols - 1), last.min(cols - 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostrealm_terminal::{CellAttrs, Cursor, GridSize};

    fn grid_of(rows: &[&str]) -> Grid {
        let cols = rows[0].chars().count() as u16;
        let cells = rows
            .iter()
            .flat_map(|r| r.chars())
            .map(|ch| Cell {
                text: if ch == ' ' { String::new() } else { ch.to_string() },
                fg: [200, 200, 200],
                bg: [0, 0, 0],
                attrs: CellAttrs::default(),
                wide: false,
            })
            .collect();
        Grid {
            size: GridSize { cols, rows: rows.len() as u16 },
            cells,
            cursor: Cursor { col: 0, row: 0, visible: false },
            default_fg: [200, 200, 200],
            default_bg: [0, 0, 0],
        }
    }

    #[test]
    fn selection_text_spans_and_trims() {
        let grid = grid_of(&["abc ", "def ", "ghi "]);
        let sel = Selection { anchor: (1, 0), head: (1, 2) };
        assert_eq!(selection_text(&grid, sel), "bc\ndef\ngh");
        let rev = Selection { anchor: (1, 2), head: (1, 0) };
        assert_eq!(selection_text(&grid, rev), "bc\ndef\ngh", "order doesn't matter");
        let one = Selection { anchor: (0, 0), head: (3, 0) };
        assert_eq!(selection_text(&grid, one), "abc", "trailing blanks trimmed");
    }

    #[test]
    fn word_col_finds_word_boundaries() {
        let grid = grid_of(&["ab cd ef"]);
        assert_eq!(word_col(&grid, 0, 0, true), 3, "forward from a -> start of cd");
        assert_eq!(word_col(&grid, 0, 3, true), 6, "forward from cd -> start of ef");
        assert_eq!(word_col(&grid, 0, 4, false), 3, "backward from d -> start of cd");
        assert_eq!(word_col(&grid, 0, 7, false), 6, "backward from f -> start of ef");
    }

    use crate::plugin::testing::{cmd, press, Harness, UI};
    use std::time::{Duration, Instant};

    /// A terminal running `sh -c line`, once `want` shows in its output.
    fn running(line: &str, want: &str) -> TerminalView {
        let mut c = CommandBuilder::new("/bin/sh");
        c.arg("-c");
        c.arg(line);
        let term = ThreadedTerminal::spawn(80, 24, 8, 16, Some(c), None).expect("spawn");
        let mut v = TerminalView::new(term);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.text().unwrap_or_default().contains(want) {
            assert!(Instant::now() < deadline, "no {want:?} from `{line}`");
            std::thread::sleep(Duration::from_millis(20));
        }
        v
    }

    /// The centre of cell (col, row) in a view painted at the origin.
    fn cell(col: u16, row: u16) -> (f32, f32) {
        ((col as f32 + 0.5) * UI.cell_w, (row as f32 + 0.5) * UI.cell_h)
    }

    const RECT: Rect = Rect { x: 0.0, y: 0.0, w: 320.0, h: 160.0 };

    #[test]
    fn paint_sizes_the_pty_to_the_rect_and_draws_every_row() {
        let mut v = running("printf ready; sleep 2", "ready");
        let mut h = Harness::new();
        let rows = h.paint(&mut v, RECT).text.len();
        assert_eq!(v.geom, Some((40, 10, 8, 16)), "40x10 cells of 8x16 px");
        assert_eq!(rows, v.grid.size.rows as usize, "one text item per grid row");
    }

    #[test]
    fn enter_reports_busy_and_unbound_cmd_chords_are_not_typed() {
        let mut v = running("printf ready; sleep 2", "ready");
        let h = Harness::new();
        let (used, out) = h.key(&mut v, press(Key::Enter));
        assert!(used);
        assert_eq!(out.requests, vec![Request::Busy]);
        let (used, out) = h.key(&mut v, cmd(Key::Char('q')));
        assert!(!used && out.requests.is_empty());
    }

    #[test]
    fn dragging_selects_cells() {
        let mut v = running("printf 'hello world'; sleep 2", "hello world");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(0, 0), clicks: 1 });
        assert!(v.selection.is_none(), "a press alone selects nothing");
        let out = h.mouse(&mut v, MouseEvent::Drag { pos: cell(4, 0) });
        assert!(out.redraw);
        assert_eq!(selection_text(&v.grid, v.selection.unwrap()), "hello");
        h.mouse(&mut v, MouseEvent::Up { pos: cell(4, 0), dragged: true });
        // A fresh press clears it.
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 0), clicks: 1 });
        assert!(v.selection.is_none());
    }

    #[test]
    fn wheel_scroll_is_queued_then_eased_out_by_paint() {
        let mut v = running("printf ready; sleep 2", "ready");
        let mut h = Harness::new();
        let out = h.scroll(&mut v, (5.0, 5.0), 3.0 * UI.cell_h);
        assert!(out.frame, "paced through the frame clock");
        assert_eq!(v.pending_scroll, -3, "wheel up reveals older history");
        h.paint(&mut v, RECT);
        assert_eq!(v.pending_scroll, 0, "a small backlog drains in one step");
        // Typing snaps back to the live bottom and drops any backlog.
        h.scroll(&mut v, (5.0, 5.0), 100.0 * UI.cell_h);
        h.key(&mut v, press(Key::Char('x')));
        assert_eq!(v.pending_scroll, 0);
    }

    #[test]
    fn shift_arrows_select_from_the_cursor() {
        let mut v = running("printf 'abc'; sleep 2", "abc");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let mut left = press(Key::Left);
        left.mods.shift = true;
        h.key(&mut v, left);
        let sel = v.selection.expect("shift+left starts a selection");
        assert_eq!(sel.head.0 + 1, sel.anchor.0, "one cell left of the cursor");
    }

    #[test]
    fn row_spans_stop_at_the_last_ink_and_merge_colours() {
        let grid = grid_of(&["a b   "]);
        assert_eq!(row_content_len(&grid, 0), 3);
        assert_eq!(row_spans(&grid, 0), vec![("a b".to_string(), [200, 200, 200])]);
    }
}
