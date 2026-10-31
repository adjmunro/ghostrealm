//! The terminal plugin: a shell (or command) on a PTY, parsed by the VT engine
//! on a worker thread ([`ThreadedTerminal`]) and drawn from grid snapshots.
//!
//! This is the hot path. Rows go through the shared row cache under the frame's
//! shaping budget; a still pane reuses its last snapshot; the PTY is resized only
//! when the cell grid actually changes; and wheel scrolling is eased over several
//! frames so the viewport never outruns shaping.

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Result;
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Config, Rect, Registry};
use ghostrealm_terminal::{
    Cell, Grid, Key, KeyPress, Lifecycle, Mods, MouseAction, MouseButton as ReportButton, MouseReport, Pumped,
    Scroll, TerminalBackend,
};
use ghostrealm_terminal_ghostty::{CommandBuilder, ThreadedTerminal};

use crate::app_state::AppState;
use crate::plugin::text_edit::{Action, Keymap, Motion};
use crate::plugin::{EventCx, MouseButton, MouseEvent, OpenCx, PaintCx, Plugin, Request, View};
use crate::toasts::Level;

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
/// Most wheel notches reported to a mouse-tracking program for one scroll
/// event, so a fling can't flood its input.
const MAX_WHEEL_REPORTS: i32 = 10;
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
    /// Chosen as a whole (word or line), so a single cell is still a selection.
    explicit: bool,
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
        self.anchor == self.head && !self.explicit
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
    /// The button of the current press when it (and its drag and release) goes
    /// to the program as mouse reports instead of driving the local selection.
    reporting: Option<ReportButton>,
    /// Sub-line wheel remainder (px) carried between events, so a slow drag
    /// isn't rounded to nothing and momentum tails aren't dropped.
    scroll_accum: f32,
    /// Whole lines still to scroll, eased out a step per frame (negative
    /// reveals older history, like `Scroll::Delta`).
    pending_scroll: i32,
    /// How the shell ended, once seen. The pane then shows a banner and the
    /// next key closes it, unless `[terminal] close_on_exit` closed it first.
    exit: Option<Lifecycle>,
    /// The grid has given up the pane's last row to the exit banner. Never
    /// undone, so a reflow that frees a blank row can't flip the size back.
    banner_reserved: bool,
    /// Failed snapshots, reported as a toast once per run.
    failures: SnapshotFailures,
}

/// Tracks runs of failed snapshots so each run is reported once (a failure
/// repeats on every snapshot until the VT recovers), and its toast is
/// dismissed when the run ends. Paint sees the outcome but can't raise
/// requests, so each waits for the view's next tick ([`View::deadline`]).
#[derive(Debug)]
struct SnapshotFailures {
    /// Keys this view's toast, so a later run replaces an unread one.
    key: String,
    /// A failure is in progress (reported or not).
    failing: bool,
    /// The toast was raised and not yet dismissed.
    shown: bool,
    /// The request the next tick sends, and since when it is due.
    due: Option<(Instant, Due)>,
}

#[derive(Debug, PartialEq)]
enum Due {
    /// A run's first failure, and why.
    Report(String),
    /// The run whose failure was reported has ended.
    Dismiss,
}

impl SnapshotFailures {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        SnapshotFailures {
            key: format!("terminal-snapshot:{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            failing: false,
            shown: false,
            due: None,
        }
    }

    /// Note a snapshot's outcome (its [`Grid::error`]). The first failure of
    /// a run becomes due; a good snapshot ends the run, and dismisses its
    /// toast if one was raised.
    fn observe(&mut self, error: Option<&str>, now: Instant) {
        match error {
            Some(e) if !self.failing => {
                self.failing = true;
                self.due = Some((now, Due::Report(e.to_owned())));
            }
            None if self.failing => {
                self.failing = false;
                self.due = self.shown.then_some((now, Due::Dismiss));
            }
            _ => {}
        }
    }

    /// Since when a request is due, if one is.
    fn deadline(&self) -> Option<Instant> {
        self.due.as_ref().map(|(at, _)| *at)
    }

    /// The due request, once: the run's toast, or its dismissal.
    fn take(&mut self) -> Option<Request> {
        let (_, due) = self.due.take()?;
        match due {
            Due::Report(error) => {
                let message = format!("Couldn't draw the terminal: {error}");
                eprintln!("ghostrealm: {message}");
                self.shown = true;
                Some(Request::Toast {
                    level: Level::Error,
                    message,
                    key: Some(self.key.clone()),
                    choices: Vec::new(),
                    doc: None,
                })
            }
            Due::Dismiss => {
                self.shown = false;
                Some(Request::DismissToast { key: self.key.clone() })
            }
        }
    }
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
            reporting: None,
            scroll_accum: 0.0,
            pending_scroll: 0,
            exit: None,
            banner_reserved: false,
            failures: SnapshotFailures::new(),
        }
    }

    /// The child's state (running, or exited with a code).
    pub fn lifecycle(&mut self) -> Lifecycle {
        self.term.lifecycle()
    }

    /// How the shell ended, if it has (as of the last pump).
    pub fn exit(&self) -> Option<Lifecycle> {
        self.exit
    }

    /// Note the shell's end the first time it is seen. Returns whether it was.
    fn observe_exit(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        let state = self.term.lifecycle();
        self.exit = state.has_exited().then_some(state);
        self.exit.is_some()
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

    /// Paste `text`: bracketed if the program asked for it (see
    /// [`TerminalBackend::paste`]).
    fn paste(&mut self, text: &str) {
        self.before_input();
        self.term.paste(text);
    }

    /// Whether pointer input belongs to the program: it enabled mouse tracking
    /// and Shift isn't held (Shift keeps selection and scrolling local).
    fn forwards_mouse(&self, mods: Mods) -> bool {
        // Scrolled back, the pointer addresses history, not the program's live
        // screen: selection and copy stay local and nothing is reported.
        !mods.shift && self.term.mouse_tracking() && self.term.viewport_active()
    }

    /// Report a pointer event at physical-pixel `pos` to the program.
    fn report_mouse(&mut self, action: MouseAction, button: Option<ReportButton>, mods: Mods, pos: (f32, f32)) {
        let (cw, ch) = self.cell;
        self.term.send_mouse(&MouseReport {
            action,
            button,
            mods,
            at: ((pos.0 - self.rect.x) / cw, (pos.1 - self.rect.y) / ch),
        });
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
        self.selection = Some(Selection { anchor, head, explicit: false });
    }

    /// Cmd chords the keymap didn't turn into readline keys: copy and paste.
    fn cmd_key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        match k.key {
            Key::Char(c) if c.eq_ignore_ascii_case(&'c') => self.copy_selection(cx),
            Key::Char(c) if c.eq_ignore_ascii_case(&'v') => match cx.clipboard_text() {
                Some(text) if !text.is_empty() => self.paste(&text),
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
        // Snapshot only when the grid changed, reusing the previous grid's
        // allocations as the target.
        if self.term.needs_snapshot() || !self.snapshotted {
            self.term.snapshot_into(&mut self.grid);
            self.snapshotted = true;
            self.failures.observe(self.grid.error.as_deref(), Instant::now());
        }
        let cols = ((rect.w / cw).floor() as u16).max(1);
        let pane_rows = ((rect.h / ch).floor() as u16).max(1);
        if self.exit.is_some() && !self.banner_reserved && wants_banner_row(&self.grid, cols, pane_rows) {
            self.banner_reserved = true;
            // The shrink scrolls the output up under a viewport-relative
            // selection.
            self.selection = None;
            self.press_cell = None;
        }
        // Resize the PTY only when the cell grid (or cell size) changes.
        let rows = if self.banner_reserved { (pane_rows - 1).max(1) } else { pane_rows };
        let geom = (cols, rows, cw.round() as u32, ch.round() as u32);
        if self.geom != Some(geom) {
            self.term.resize(cols, rows, geom.2, geom.3);
            self.geom = Some(geom);
        }

        let banner = self.exit.map(exit_banner);
        let banner_row = banner.as_ref().and_then(|_| {
            let grid_rows = self.grid.size.rows;
            banner_row(&self.grid).or(if self.banner_reserved && pane_rows > 1 {
                // None until the shrunken grid arrives, rather than covering
                // a row of output.
                Some(grid_rows).filter(|&r| r < pane_rows)
            } else {
                Some(grid_rows.saturating_sub(1))
            })
        });
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
            if Some(row) == banner_row {
                continue;
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
        if let (Some(text), Some(row)) = (banner.as_deref(), banner_row) {
            let top = rect.y + row as f32 * ch;
            let line = Rect { x: rect.x, y: top, w: rect.w, h: ch };
            cx.fill_alpha(line, cx.chrome.accent, 0.35);
            let key = cx.row_key(std::iter::once((text, grid.default_fg)));
            cx.budgeted_row(key, || vec![(text.to_string(), grid.default_fg)], rect.x, top, rect.w, rect, grid.default_fg);
        }
        if grid.cursor.visible && banner.is_none() {
            let fg = grid
                .cell(grid.cursor.col, grid.cursor.row)
                .map(|c| c.fg)
                .unwrap_or(grid.default_fg);
            cx.fill_top(cell_rect(rect, grid.cursor.col, grid.cursor.row, cw, ch), fg, CURSOR_ALPHA);
        }
    }

    fn key(&mut self, cx: &mut EventCx, k: &KeyPress) -> bool {
        let m = k.mods;
        // Nothing is listening any more: a key closes the pane. Cmd chords
        // stay with copy and paste.
        if self.exit.is_some() && !m.super_ {
            cx.request(Request::Close);
            return true;
        }
        let action = Keymap::platform().action(k);
        // The text keymap's word/line motions and deletes become the readline
        // keys shells bind (Shift+Cmd too; Shift+Option selects below). A
        // full-screen program on the alternate screen binds its own keys, so
        // there they keep their normal encoding.
        if !self.grid.alt_screen && (m.super_ || (!m.shift && (m.alt || m.ctrl))) {
            if let Some(bytes) = action.and_then(readline) {
                self.write(bytes);
                cx.redraw();
                return true;
            }
        }
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
            // Shift+Arrow selects over the grid (word-wise with the keymap's word
            // modifier) instead of corrupting the shell input.
            if matches!(k.key, Key::Left | Key::Right | Key::Up | Key::Down) {
                let by_word = matches!(action, Some(Action::Move(Motion::WordLeft | Motion::WordRight)));
                self.extend_selection(k.key, by_word);
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
        // Option+Up/Down send a plain arrow, not a corrupting modified CSI.
        if m.alt && matches!(k.key, Key::Up | Key::Down) {
            self.before_input();
            self.term.send_key(&KeyPress {
                key: k.key,
                mods: Mods::default(),
                text: None,
            });
            return true;
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
        let mods = cx.mods;
        // A release can be lost (the window lost focus mid-drag, say), which
        // would leave the program believing the button is still held. A new
        // press first closes the old one.
        if let MouseEvent::Down { pos, .. } = *event {
            if let Some(held) = self.reporting.take() {
                self.report_mouse(MouseAction::Release, Some(held), mods, pos);
            }
        }
        match *event {
            MouseEvent::Down { pos, button, .. } if self.forwards_mouse(mods) => {
                let button = report_button(button);
                self.reporting = Some(button);
                // Only the left button drives the local selection.
                if button == ReportButton::Left {
                    self.press_cell = None;
                    if self.selection.take().is_some() {
                        cx.redraw();
                    }
                }
                self.report_mouse(MouseAction::Press, Some(button), mods, pos);
            }
            MouseEvent::Drag { pos, .. } if self.reporting.is_some() => {
                self.report_mouse(MouseAction::Motion, self.reporting, mods, pos);
            }
            MouseEvent::Up { pos, .. } if self.reporting.is_some() => {
                let held = self.reporting.take();
                self.report_mouse(MouseAction::Release, held, mods, pos);
            }
            MouseEvent::Move { pos } if self.forwards_mouse(mods) => {
                self.report_mouse(MouseAction::Motion, None, mods, pos);
            }
            // The local selection is the left button's.
            _ if event.is_other_button() => {}
            // A press only records the anchor; a plain click never highlights.
            MouseEvent::Down { pos: (x, y), clicks, .. } => {
                if self.selection.take().is_some() {
                    cx.redraw();
                }
                self.press_cell = self.cell_at(x, y);
                // A double-click selects (and copies) the word under the pointer;
                // a triple-click the row.
                if let Some((col, row)) = self.press_cell.filter(|_| clicks >= 2) {
                    let (start, end) = if clicks == 2 {
                        word_span(&self.grid, row, col)
                    } else {
                        (0, self.grid.size.cols.saturating_sub(1))
                    };
                    self.selection =
                        Some(Selection { anchor: (start, row), head: (end, row), explicit: true });
                    self.copy_selection(cx);
                    cx.redraw();
                }
            }
            MouseEvent::Drag { pos: (x, y), .. } => {
                let (Some(anchor), Some(head)) = (self.press_cell, self.cell_at(x, y)) else {
                    return;
                };
                let sel = Some(Selection { anchor, head, explicit: false });
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
            // A reported drag continues when the pointer leaves the pane: the
            // button is still held, so the program keeps getting motion (pinned
            // to the grid edge by the backend) until the release, as in xterm.
            // Hover is not reported outside the pane, so Leave itself is not
            // forwarded.
            MouseEvent::Leave { .. } => {}
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), _dx: f32, dy: f32) {
        let ch = cx.ui.cell_h;
        self.scroll_accum += dy;
        let lines = (self.scroll_accum / ch).trunc() as i32;
        self.scroll_accum -= lines as f32 * ch;
        if lines == 0 {
            return;
        }
        // A program tracking the mouse gets the wheel as button 4/5 presses.
        if self.forwards_mouse(cx.mods) {
            let button = if lines > 0 { ReportButton::WheelUp } else { ReportButton::WheelDown };
            for _ in 0..lines.abs().min(MAX_WHEEL_REPORTS) {
                self.report_mouse(MouseAction::Press, Some(button), cx.mods, pos);
            }
            // The program redraws in response; a viewport-relative selection
            // would highlight stale text.
            if self.selection.take().is_some() {
                cx.redraw();
            }
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
        let mut pumped = self.term.pump_budgeted(budget);
        // The banner appears on the next draw.
        pumped.changed |= self.observe_exit();
        pumped
    }

    fn is_busy(&self) -> bool {
        self.term.is_busy()
    }

    fn deadline(&self, _cfg: &Config) -> Option<Instant> {
        self.failures.deadline()
    }

    fn toast_keys(&self) -> Vec<String> {
        vec![self.failures.key.clone()]
    }

    fn tick(&mut self, cx: &mut EventCx, _now: Instant) {
        if let Some(toast) = self.failures.take() {
            cx.request(toast);
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> bool {
        self.term.write_bytes(bytes);
        true
    }

    fn text(&mut self) -> Option<String> {
        // A peek: new output and the exit stay news for `pump`, which drives
        // the redraw and the inbox's unread mark.
        let grid = self.term.snapshot();
        let exit = self.exit.or(Some(self.term.lifecycle()).filter(|l| l.has_exited()));
        let banner = exit.map(exit_banner);
        let banner_row = banner_row(&grid);
        let mut out = String::new();
        for row in 0..grid.size.rows {
            if let Some(text) = banner.as_deref().filter(|_| Some(row) == banner_row) {
                out.push_str(text);
                out.push('\n');
                continue;
            }
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
        // No blank row to take: the banner follows the output.
        if let Some(text) = banner.as_deref().filter(|_| banner_row.is_none()) {
            out.push_str(text);
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

/// The button a tracking program hears for a press of `b`.
fn report_button(b: MouseButton) -> ReportButton {
    match b {
        MouseButton::Left => ReportButton::Left,
        MouseButton::Middle => ReportButton::Middle,
        MouseButton::Right => ReportButton::Right,
    }
}

/// Whether an ended shell's `grid` should give up the last of a pane's
/// `cols` x `pane_rows` cells to the exit banner: no blank row is left for it.
/// Judged on the grid's own flags, not the backend's live state, which can
/// be newer than the grid: scrolled back, the rows are history with no
/// cursor. Shrinking drops the top row from a one-row pane or the alternate
/// screen (no scrollback), so those keep the banner over the last row.
fn wants_banner_row(grid: &Grid, cols: u16, pane_rows: u16) -> bool {
    pane_rows > 1
        && (grid.size.cols, grid.size.rows) == (cols, pane_rows)
        && grid.viewport_active
        && !grid.alt_screen
        && banner_row(grid).is_none()
}

/// The first blank row from the cursor's down, which the exit banner takes.
/// With none, the banner goes below the grid, or over its last row where the
/// grid can't give one up.
fn banner_row(grid: &Grid) -> Option<u16> {
    (grid.cursor.row..grid.size.rows).find(|&r| row_content_len(grid, r) == 0)
}

/// The banner shown once the shell has ended.
fn exit_banner(end: Lifecycle) -> String {
    match end {
        Lifecycle::Exited(Some(code)) => format!("exited with code {code} \u{2014} press any key to close"),
        Lifecycle::Signaled(n) => format!("killed by signal {} \u{2014} press any key to close", signal_name(n)),
        _ => "exited \u{2014} press any key to close".to_string(),
    }
}

/// `SIGKILL` for 9, and so on; the number for one without a name here.
fn signal_name(n: i32) -> String {
    #[cfg(unix)]
    for (signal, name) in [
        (libc::SIGHUP, "SIGHUP"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGQUIT, "SIGQUIT"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGTRAP, "SIGTRAP"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGFPE, "SIGFPE"),
        (libc::SIGKILL, "SIGKILL"),
        (libc::SIGUSR1, "SIGUSR1"),
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGUSR2, "SIGUSR2"),
        (libc::SIGPIPE, "SIGPIPE"),
        (libc::SIGALRM, "SIGALRM"),
        (libc::SIGTERM, "SIGTERM"),
    ] {
        if n == signal {
            return name.to_string();
        }
    }
    n.to_string()
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

/// Whether cell `(col, row)` is the empty second half of a double-width
/// grapheme (CJK, emoji) in the cell before it.
fn is_spacer(grid: &Grid, row: u16, col: u16) -> bool {
    col > 0
        && grid.cell(col, row).is_some_and(|c| c.text.is_empty())
        && grid
            .cell(col - 1, row)
            .is_some_and(|c| unicode_width::UnicodeWidthStr::width(c.text.as_str()) == 2)
}

/// The first and last columns of the run of like cells (word characters,
/// blanks, or other punctuation) containing `col` on `row`. A double-width
/// grapheme's spacer cell belongs to the grapheme.
fn word_span(grid: &Grid, row: u16, col: u16) -> (u16, u16) {
    let cols = grid.size.cols;
    if cols == 0 {
        return (col, col);
    }
    let class = |mut c: u16| {
        if is_spacer(grid, row, c) {
            c -= 1;
        }
        match grid.cell(c, row).and_then(|cell| cell.text.chars().next()) {
            Some(ch) if ch.is_alphanumeric() || ch == '_' => 0,
            Some(ch) if !ch.is_whitespace() => 2,
            _ => 1,
        }
    };
    let col = col.min(cols - 1);
    let k = class(col);
    let mut start = col;
    while start > 0 && class(start - 1) == k {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < cols && class(end + 1) == k {
        end += 1;
    }
    (start, end)
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
            if is_spacer(grid, row, col) {
                continue;
            }
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

/// The readline (emacs-mode) keys for a word/line motion or delete, which
/// shells' line editors bind by default. Other actions have none.
fn readline(action: Action) -> Option<&'static [u8]> {
    Some(match action {
        Action::Move(Motion::WordLeft) => b"\x1bb",
        Action::Move(Motion::WordRight) => b"\x1bf",
        Action::Move(Motion::LineStart | Motion::DocStart) => b"\x01",
        Action::Move(Motion::LineEnd | Motion::DocEnd) => b"\x05",
        Action::Delete(Motion::WordLeft) => b"\x1b\x7f",
        Action::Delete(Motion::WordRight) => b"\x1bd",
        Action::Delete(Motion::LineStart) => b"\x15",
        Action::Delete(Motion::LineEnd) => b"\x0b",
        _ => return None,
    })
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
            viewport_active: true,
            alt_screen: false,
            error: None,
        }
    }

    /// A run of failed snapshots reports once, when its first is due; a
    /// good snapshot ends the run and dismisses its toast, so the next
    /// failure reports again.
    #[test]
    fn a_run_of_failed_snapshots_reports_once() {
        let mut f = SnapshotFailures::new();
        let t0 = Instant::now();
        f.observe(None, t0);
        assert_eq!(f.deadline(), None);

        f.observe(Some("render state"), t0);
        f.observe(Some("render state"), t0 + std::time::Duration::from_millis(16));
        assert_eq!(f.deadline(), Some(t0), "due from the run's first failure");
        let Some(Request::Toast { level, message, key, .. }) = f.take() else { panic!("a toast") };
        assert_eq!(level, Level::Error);
        assert!(message.contains("render state"), "{message}");
        assert_eq!(key.as_deref(), Some(f.key.as_str()));
        assert!(f.take().is_none(), "reported once");
        f.observe(Some("render state"), t0);
        assert_eq!(f.deadline(), None, "the run goes on");

        f.observe(None, t0);
        assert_eq!(f.take(), Some(Request::DismissToast { key: f.key.clone() }));
        f.observe(None, t0);
        assert!(f.take().is_none(), "dismissed once");
        f.observe(Some("cols"), t0);
        assert!(matches!(f.take(), Some(Request::Toast { .. })), "a new run reports again");
    }

    /// A run that ends before its toast's tick reports nothing and dismisses
    /// nothing; a new failure after a dismissal goes unsent reports instead.
    #[test]
    fn a_recovered_failure_is_not_reported() {
        let mut f = SnapshotFailures::new();
        f.observe(Some("snapshot"), Instant::now());
        f.observe(None, Instant::now());
        assert!(f.take().is_none());

        f.observe(Some("snapshot"), Instant::now());
        f.take();
        f.observe(None, Instant::now());
        f.observe(Some("again"), Instant::now());
        assert!(matches!(f.take(), Some(Request::Toast { .. })), "its toast replaces the shown one");
    }

    /// Each terminal keys its own toast, so one pane's failure doesn't
    /// replace another's.
    #[test]
    fn each_terminal_keys_its_own_toast() {
        assert_ne!(SnapshotFailures::new().key, SnapshotFailures::new().key);
    }

    #[test]
    fn the_exit_banner_names_the_code_or_the_signal() {
        let tail = "\u{2014} press any key to close";
        assert_eq!(exit_banner(Lifecycle::Exited(Some(2))), format!("exited with code 2 {tail}"));
        assert_eq!(exit_banner(Lifecycle::Exited(None)), format!("exited {tail}"));
        #[cfg(unix)]
        {
            assert_eq!(exit_banner(Lifecycle::Signaled(libc::SIGTERM)), format!("killed by signal SIGTERM {tail}"));
            assert_eq!(exit_banner(Lifecycle::Signaled(libc::SIGSEGV)), format!("killed by signal SIGSEGV {tail}"));
        }
        assert_eq!(exit_banner(Lifecycle::Signaled(99)), format!("killed by signal 99 {tail}"));
    }

    #[test]
    fn the_banner_takes_the_first_blank_row_from_the_cursor_down() {
        let mut g = grid_of(&["out ", "$ x ", "    ", "    "]);
        g.cursor.row = 1;
        assert_eq!(banner_row(&g), Some(2));
        g.cursor.row = 2;
        assert_eq!(banner_row(&g), Some(2));
        let mut full = grid_of(&["out ", "err "]);
        full.cursor.row = 1;
        assert_eq!(banner_row(&full), None);
    }

    #[test]
    fn only_a_full_live_primary_grid_at_the_pane_size_gives_up_a_row() {
        let mut g = grid_of(&["out ", "err "]);
        g.cursor.row = 1;
        assert!(wants_banner_row(&g, 4, 2));
        assert!(!wants_banner_row(&g, 4, 3), "a grid not yet at the pane's size");
        assert!(!wants_banner_row(&grid_of(&["out "]), 4, 1), "a one-row pane");
        let history = Grid { viewport_active: false, ..g.clone() };
        assert!(!wants_banner_row(&history, 4, 2), "a page of scrollback");
        let alt = Grid { alt_screen: true, ..g.clone() };
        assert!(!wants_banner_row(&alt, 4, 2), "the alternate screen");
        assert!(!wants_banner_row(&grid_of(&["out ", "    "]), 4, 2), "a blank row is left");
    }

    #[test]
    fn selection_text_spans_and_trims() {
        let grid = grid_of(&["abc ", "def ", "ghi "]);
        let sel = Selection { anchor: (1, 0), head: (1, 2), explicit: false };
        assert_eq!(selection_text(&grid, sel), "bc\ndef\ngh");
        let rev = Selection { anchor: (1, 2), head: (1, 0), explicit: false };
        assert_eq!(selection_text(&grid, rev), "bc\ndef\ngh", "order doesn't matter");
        let one = Selection { anchor: (0, 0), head: (3, 0), explicit: false };
        assert_eq!(selection_text(&grid, one), "abc", "trailing blanks trimmed");
    }

    #[test]
    fn keymap_motions_become_readline_keys() {
        let key = |key, f: fn(&mut Mods)| {
            let mut mods = Mods::default();
            f(&mut mods);
            KeyPress { key, mods, text: None }
        };
        let mac = |k: KeyPress| Keymap::macos().action(&k).and_then(readline);
        assert_eq!(mac(key(Key::Left, |m| m.alt = true)), Some(&b"\x1bb"[..]));
        assert_eq!(mac(key(Key::Down, |m| m.super_ = true)), Some(&b"\x05"[..]));
        assert_eq!(mac(key(Key::Backspace, |m| m.super_ = true)), Some(&b"\x15"[..]));
        assert_eq!(mac(key(Key::Char('v'), |m| m.super_ = true)), None, "paste isn't readline");
        let pc = |k: KeyPress| Keymap::pc().action(&k).and_then(readline);
        assert_eq!(pc(key(Key::Right, |m| m.ctrl = true)), Some(&b"\x1bf"[..]));
        assert_eq!(pc(key(Key::Char('c'), |m| m.ctrl = true)), None, "Ctrl+C still reaches the shell");
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

    /// Paint a view running `sh -c "printf go; read x; <script>"` into
    /// `rect`, run the script, and paint until it has exited, its output shows
    /// `want` and the grid is `rows` tall.
    fn exited_after(rect: Rect, script: &str, want: &str, rows: u16) -> (TerminalView, Harness) {
        let mut v = running(&format!("printf go; read x; {script}"), "go");
        let mut h = Harness::new();
        h.paint(&mut v, rect);
        v.write_input(b"\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        // The exit can be seen before the last output is read: wait for both.
        while !v.text().unwrap_or_default().contains(want) || v.exit().is_none() {
            assert!(Instant::now() < deadline, "`{script}` never exited with {want:?}");
            v.pump(usize::MAX);
            std::thread::sleep(Duration::from_millis(20));
        }
        loop {
            h.paint(&mut v, rect);
            let shown = (0..v.grid.size.rows).any(|r| selection_text(&v.grid, row_sel(r)).contains(want));
            if v.grid.size.rows == rows && shown {
                break;
            }
            assert!(Instant::now() < deadline, "the grid never settled at {rows} rows");
            std::thread::sleep(Duration::from_millis(20));
        }
        h.paint(&mut v, rect);
        (v, h)
    }

    fn row_sel(row: u16) -> Selection {
        Selection { anchor: (0, row), head: (u16::MAX, row), explicit: true }
    }

    /// The text drawn on each row of the last paint, by row.
    fn drawn(h: &Harness) -> Vec<(u16, String)> {
        h.frame
            .text
            .iter()
            .map(|t| {
                let text = h.text.buffer(t.src).map(|b| b.lines.iter().map(|l| l.text()).collect());
                ((t.top / UI.cell_h) as u16, text.unwrap_or_default())
            })
            .collect()
    }

    fn drawn_on(h: &Harness, row: u16) -> Vec<String> {
        drawn(h).into_iter().filter(|(r, _)| *r == row).map(|(_, t)| t).collect()
    }

    fn banner() -> String {
        exit_banner(Lifecycle::Exited(Some(0)))
    }

    #[test]
    fn reading_the_text_leaves_the_output_and_the_exit_for_pump() {
        let mut v = running("stty -echo; printf ready; read x", "ready");
        assert!(v.pump(usize::MAX).changed, "the output `text` showed is still new to `pump`");
        v.write_input(b"\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.text().unwrap_or_default().contains(&banner()) {
            assert!(Instant::now() < deadline, "the exit never showed");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(v.exit(), None, "`text` left the exit unobserved");
        assert!(v.pump(usize::MAX).changed, "the exit is new to `pump`");
        assert!(v.exit().is_some());
    }

    #[test]
    fn a_full_grid_keeps_its_last_row_above_the_banner() {
        let (mut v, h) = exited_after(RECT, "seq 1 12; printf last-row", "last-row", 9);
        assert_eq!(v.geom.map(|g| g.1), Some(9), "the pane's last row is left for the banner");
        assert_eq!(drawn_on(&h, 8), ["last-row"]);
        assert_eq!(drawn_on(&h, 9), [banner()]);
        let text = v.text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 10, "nine grid rows and the banner: {text:?}");
        assert_eq!(lines[8], "last-row");
        assert_eq!(lines[9], banner());
    }

    #[test]
    fn giving_up_a_row_drops_the_selection() {
        // `stty -echo` runs before `last-row` shows, so the second newline
        // isn't echoed: an echoed one would scroll a blank last row into
        // the grid, which the banner takes without giving up a row.
        let mut v = running("printf go; read x; seq 1 12; stty -echo; printf last-row; read y", "go");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        v.write_input(b"\n");
        let deadline = Instant::now() + Duration::from_secs(5);
        while selection_text(&v.grid, row_sel(9)) != "last-row" {
            assert!(Instant::now() < deadline, "no full grid");
            h.paint(&mut v, RECT);
            std::thread::sleep(Duration::from_millis(20));
        }
        h.mouse(&mut v, MouseEvent::Down { pos: cell(0, 9), clicks: 3, button: MouseButton::Left });
        assert!(v.selection.is_some());
        v.write_input(b"\n");
        while v.grid.size.rows != 9 {
            assert!(Instant::now() < deadline, "the grid never gave up a row");
            v.pump(usize::MAX);
            h.paint(&mut v, RECT);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(v.selection, None, "its row moved");
    }

    /// A snapshot that failed shows as a toast from the next tick after the
    /// paint that saw it; one that recovers dismisses it.
    #[test]
    fn a_failed_snapshot_toasts_from_the_tick_after_paint() {
        // An exited shell publishes no more grids, so the paints below keep
        // the grid set here.
        let (mut v, mut h) = exited_after(RECT, "true", "go", 10);
        let cfg = Config::default();
        assert_eq!(v.deadline(&cfg), None);

        v.grid.error = Some("render state".into());
        v.snapshotted = false;
        h.paint(&mut v, RECT);
        let due = v.deadline(&cfg).expect("a toast is due");
        let out = h.tick(&mut v, due);
        let [Request::Toast { message, key: Some(key), .. }] = &out.requests[..] else {
            panic!("{:?}", out.requests)
        };
        assert!(message.contains("render state"), "{message}");
        let key = key.clone();
        assert_eq!(v.deadline(&cfg), None, "sent once");

        v.grid.error = None;
        v.snapshotted = false;
        h.paint(&mut v, RECT);
        let due = v.deadline(&cfg).expect("a dismissal is due");
        assert_eq!(h.tick(&mut v, due).requests, [Request::DismissToast { key }]);
    }

    #[test]
    fn a_short_output_has_the_banner_on_the_row_below_it() {
        let (mut v, h) = exited_after(RECT, "echo hi", "hi", 10);
        assert_eq!(drawn_on(&h, 0), ["go"]);
        assert_eq!(drawn_on(&h, 1), ["hi"]);
        assert_eq!(drawn_on(&h, 2), [banner()]);
        let text = v.text().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(&lines[..3], ["go", "hi", &banner()]);
    }

    #[test]
    fn output_ending_on_a_blank_last_row_gives_the_banner_that_row() {
        let (v, h) = exited_after(RECT, "seq 1 9", "9", 10);
        assert_eq!(v.geom.map(|g| g.1), Some(10), "no row given up");
        assert_eq!(drawn_on(&h, 0), ["1"], "nothing scrolled away");
        assert_eq!(drawn_on(&h, 8), ["9"]);
        assert_eq!(drawn_on(&h, 9), [banner()]);
    }

    #[test]
    fn scrolled_back_history_does_not_take_the_banner_row() {
        let (mut v, mut h) = exited_after(RECT, "seq 1 30; printf 'end\\n\\n\\n'", "end", 10);
        let live = selection_text(&v.grid, row_sel(0));
        v.term.scroll(Scroll::Delta(-15));
        let deadline = Instant::now() + Duration::from_secs(5);
        while v.term.viewport_active() || selection_text(&v.grid, row_sel(0)) == live {
            assert!(Instant::now() < deadline, "the viewport never scrolled back");
            h.paint(&mut v, RECT);
            std::thread::sleep(Duration::from_millis(20));
        }
        h.paint(&mut v, RECT);
        assert!(!v.banner_reserved, "the live screen has a blank row for the banner");
        assert_eq!(v.geom.map(|g| g.1), Some(10));
    }

    /// Scrolled back then to the bottom, the backend's live state reports the
    /// live screen before the painted grid stops being history.
    #[test]
    fn the_painted_grid_not_the_live_state_decides_the_banner_row() {
        let mut v = running("printf ready; sleep 3", "ready");
        let mut h = Harness::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while v.grid.size != (GridSize { cols: 40, rows: 10 }) || v.term.needs_snapshot() {
            assert!(Instant::now() < deadline, "the grid never settled");
            h.paint(&mut v, RECT);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(v.term.viewport_active());
        let row = "x".repeat(40);
        v.grid = Grid { viewport_active: false, ..grid_of(&[row.as_str(); 10]) };
        v.exit = Some(Lifecycle::Exited(Some(0)));
        h.paint(&mut v, RECT);
        assert!(!v.banner_reserved, "a page of history kept the pane's rows");
        v.grid.viewport_active = true;
        h.paint(&mut v, RECT);
        assert!(v.banner_reserved, "the same rows as the live screen give one up");
    }

    #[test]
    fn a_one_row_pane_shows_the_banner_over_its_row() {
        let one = Rect { h: UI.cell_h, ..RECT };
        let (mut v, h) = exited_after(one, "printf one-row", "one-row", 1);
        assert_eq!(drawn(&h), [(0, banner())]);
        assert_eq!(v.text().unwrap(), format!("one-row\n{}\n", banner()), "the surface text keeps both");
    }

    #[test]
    fn the_alternate_screen_keeps_its_rows_under_the_banner() {
        let script = "printf '\\033[?1049h\\033[H'; seq 1 9; printf last";
        let (v, h) = exited_after(RECT, script, "last", 10);
        assert_eq!(v.geom.map(|g| g.1), Some(10), "no row dropped from a screen without scrollback");
        assert_eq!(drawn_on(&h, 0), ["1"]);
        assert_eq!(drawn_on(&h, 9), [banner()]);
    }

    /// The bytes a word-left chord reaches the program as, in hex, after
    /// `setup` runs: the program reads raw input and prints it with `od`.
    fn word_left_bytes(setup: &str) -> String {
        let script = format!("{setup}stty raw -echo min 0 time 20; printf ready; head -c 64 | od -An -tx1; sleep 3");
        let mut v = running(&script, "ready");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let mut mods = Mods::default();
        if cfg!(target_os = "macos") {
            mods.alt = true;
        } else {
            mods.ctrl = true;
        }
        let (used, _) = h.key(&mut v, KeyPress { key: Key::Left, mods, text: None });
        assert!(used);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = v.text().unwrap_or_default();
            // Raw mode leaves the dump on the line after "ready".
            let dump = text.split_once("ready").and_then(|(_, rest)| rest.lines().next()).unwrap_or("");
            if !dump.trim().is_empty() {
                return dump.split_whitespace().collect::<Vec<_>>().join(" ");
            }
            assert!(Instant::now() < deadline, "no bytes echoed: {text:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn word_left_is_readline_on_the_primary_screen_and_plain_on_the_alternate() {
        assert_eq!(word_left_bytes(""), "1b 62", "ESC b");
        let mods = if cfg!(target_os = "macos") { "33" } else { "35" };
        let plain = format!("1b 5b 31 3b {mods} 44");
        assert_eq!(word_left_bytes("printf '\\033[?1049h'; "), plain, "CSI 1;3D / CSI 1;5D");
    }

    #[test]
    fn dragging_selects_cells() {
        let mut v = running("printf 'hello world'; sleep 2", "hello world");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(0, 0), clicks: 1, button: MouseButton::Left });
        assert!(v.selection.is_none(), "a press alone selects nothing");
        let out = h.mouse(&mut v, MouseEvent::Drag { pos: cell(4, 0), button: MouseButton::Left });
        assert!(out.redraw);
        assert_eq!(selection_text(&v.grid, v.selection.unwrap()), "hello");
        h.mouse(&mut v, MouseEvent::Up { pos: cell(4, 0), dragged: true, button: MouseButton::Left });
        // A fresh press clears it.
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 0), clicks: 1, button: MouseButton::Left });
        assert!(v.selection.is_none());
    }

    #[test]
    fn scrolled_back_the_mouse_selects_locally_instead_of_reporting() {
        let mut v = running("printf '\\033[?1000h'; seq 1 100; sleep 3", "100");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !v.term.mouse_tracking() {
            assert!(Instant::now() < deadline, "tracking never enabled");
            std::thread::sleep(Duration::from_millis(15));
        }
        // Live: the press belongs to the program.
        h.mouse(&mut v, MouseEvent::Down { pos: cell(1, 1), clicks: 1, button: MouseButton::Left });
        assert!(v.reporting.is_some());
        h.mouse(&mut v, MouseEvent::Up { pos: cell(1, 1), dragged: false, button: MouseButton::Left });

        v.term.scroll(Scroll::Delta(-10));
        let deadline = Instant::now() + Duration::from_secs(5);
        while v.term.viewport_active() {
            assert!(Instant::now() < deadline, "never scrolled back");
            std::thread::sleep(Duration::from_millis(15));
        }
        h.mouse(&mut v, MouseEvent::Down { pos: cell(1, 1), clicks: 1, button: MouseButton::Left });
        assert!(v.reporting.is_none(), "nothing is reported while scrolled back");
        h.mouse(&mut v, MouseEvent::Drag { pos: cell(4, 1), button: MouseButton::Left });
        assert!(v.selection.is_some(), "the drag selects locally");
        assert!(v.term.mouse_tracking(), "the program still tracks the mouse");
    }

    #[test]
    fn word_span_stops_at_blanks_and_punctuation() {
        let grid = grid_of(&["ab_1 (cd)"]);
        assert_eq!(word_span(&grid, 0, 1), (0, 3));
        assert_eq!(word_span(&grid, 0, 5), (5, 5), "punctuation");
        assert_eq!(word_span(&grid, 0, 7), (6, 7));
    }

    #[test]
    fn word_span_keeps_a_wide_grapheme_with_its_spacer() {
        let mut grid = grid_of(&["(\u{4e16}  b"]);
        // grid_of puts the wide char in one cell; its spacer is the blank after.
        assert_eq!(word_span(&grid, 0, 1), (1, 2), "the lead cell takes its spacer");
        assert_eq!(word_span(&grid, 0, 2), (1, 2), "the spacer finds its lead");
        let sel = Selection { anchor: (1, 0), head: (2, 0), explicit: true };
        assert_eq!(selection_text(&grid, sel), "\u{4e16}", "no space for the spacer");
        grid.cells[3].text = "x".into();
        assert_eq!(word_span(&grid, 0, 1), (1, 4), "following letters join the run");
    }

    #[test]
    fn a_single_cell_word_is_a_selection_that_copies() {
        let mut v = running("printf 'a b c'; sleep 2", "a b c");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 0), clicks: 2, button: MouseButton::Left });
        let sel = v.selection.unwrap();
        assert!(!sel.is_empty(), "one cell, but chosen as a whole");
        assert_eq!(selection_text(&v.grid, sel), "b");
    }

    #[test]
    fn triple_click_selects_the_row() {
        let mut v = running("printf 'hello world'; sleep 2", "hello world");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 0), clicks: 3, button: MouseButton::Left });
        assert_eq!(selection_text(&v.grid, v.selection.unwrap()), "hello world");
    }

    #[test]
    fn double_click_selects_a_word() {
        let mut v = running("printf 'hello world'; sleep 2", "hello world");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(7, 0), clicks: 2, button: MouseButton::Left });
        assert_eq!(selection_text(&v.grid, v.selection.unwrap()), "world");
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

    /// A terminal whose program switched `modes` on, then echoes its input
    /// through `cat -v` (control bytes show as `^[`, with `stty -echo` hiding
    /// the tty's own echo).
    fn echoing(modes: &str) -> TerminalView {
        let v = running(&format!("stty -echo; printf '{modes}'; echo READY; exec cat -v"), "READY");
        let deadline = Instant::now() + Duration::from_secs(5);
        while modes.contains("1000") && !v.term.mouse_tracking() {
            assert!(Instant::now() < deadline, "tracking never turned on");
            std::thread::sleep(Duration::from_millis(10));
        }
        v
    }

    /// The screen's text, rows joined (the grid wraps long echoes), once it
    /// contains `want`.
    fn echoed(v: &mut TerminalView, want: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let screen = v.text().unwrap_or_default().replace('\n', "");
            if screen.contains(want) {
                return screen;
            }
            assert!(Instant::now() < deadline, "no {want:?} echoed, got {screen:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const SHIFT: Mods = Mods { shift: true, ctrl: false, alt: false, super_: false };

    #[test]
    fn paste_is_bracketed_only_when_the_program_asked() {
        let mut v = echoing("\\033[?2004h");
        v.paste("a\nb");
        v.write(b"\r");
        assert_eq!(echoed(&mut v, "^[[201~"), "READY^[[200~ab^[[201~");

        let mut plain = echoing("");
        plain.paste("raw\n");
        assert_eq!(echoed(&mut plain, "raw"), "READYraw", "no markers without mode 2004");
    }

    #[test]
    fn a_tracking_program_gets_press_drag_release_and_hover() {
        let mut v = echoing("\\033[?1003h\\033[?1006h");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 1), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Drag { pos: cell(5, 1), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: cell(5, 1), dragged: true, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Move { pos: cell(7, 3) });
        v.write(b"\r");
        let screen = echoed(&mut v, "^[[<35;8;4M");
        assert_eq!(screen, "READY^[[<0;3;2M^[[<32;6;2M^[[<0;6;2m^[[<35;8;4M");
        assert!(v.selection.is_none() && v.press_cell.is_none(), "nothing selected locally");
    }

    #[test]
    fn a_tracking_program_gets_right_and_middle_presses_and_drags() {
        let mut v = echoing("\\033[?1003h\\033[?1006h");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let sel = Some(Selection { anchor: (0, 0), head: (3, 0), explicit: false });
        v.selection = sel;
        let (right, middle) = (MouseButton::Right, MouseButton::Middle);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 1), clicks: 1, button: right });
        h.mouse(&mut v, MouseEvent::Drag { pos: cell(5, 1), button: right });
        h.mouse(&mut v, MouseEvent::Up { pos: cell(5, 1), dragged: true, button: right });
        h.mouse(&mut v, MouseEvent::Down { pos: cell(1, 2), clicks: 1, button: middle });
        h.mouse(&mut v, MouseEvent::Drag { pos: cell(3, 2), button: middle });
        h.mouse(&mut v, MouseEvent::Up { pos: cell(3, 2), dragged: true, button: middle });
        assert!(v.reporting.is_none());
        assert_eq!(v.selection, sel, "the selection is the left button's");
        v.write(b"\r");
        let screen = echoed(&mut v, "^[[<1;4;3m");
        assert_eq!(screen, "READY^[[<2;3;2M^[[<34;6;2M^[[<2;6;2m^[[<1;2;3M^[[<33;4;3M^[[<1;4;3m");
    }

    #[test]
    fn right_and_middle_presses_leave_the_local_selection_alone() {
        let mut v = echoing("");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let sel = Some(Selection { anchor: (0, 0), head: (3, 0), explicit: false });
        v.selection = sel;
        for button in [MouseButton::Right, MouseButton::Middle] {
            h.mouse(&mut v, MouseEvent::Down { pos: cell(5, 1), clicks: 2, button });
            h.mouse(&mut v, MouseEvent::Drag { pos: cell(7, 1), button });
            h.mouse(&mut v, MouseEvent::Up { pos: cell(7, 1), dragged: true, button });
        }
        assert_eq!(v.selection, sel);
        assert!(v.press_cell.is_none() && v.reporting.is_none());
    }

    #[test]
    fn the_wheel_goes_to_a_tracking_program_not_the_scrollback() {
        let mut v = echoing("\\033[?1000h\\033[?1006h");
        let h = Harness::new();
        let out = h.scroll(&mut v, cell(1, 2), 2.0 * UI.cell_h);
        assert!(!out.frame && v.pending_scroll == 0, "no local scroll");
        h.scroll(&mut v, cell(1, 2), -UI.cell_h);
        v.write(b"\r");
        assert_eq!(echoed(&mut v, "^[[<65"), "READY^[[<64;2;3M^[[<64;2;3M^[[<65;2;3M");
    }

    #[test]
    fn a_forwarded_wheel_clears_the_selection() {
        let mut v = echoing("\\033[?1000h\\033[?1006h");
        let h = Harness::new();
        v.selection = Some(Selection { anchor: (0, 0), head: (3, 0), explicit: false });
        h.scroll(&mut v, cell(1, 2), UI.cell_h);
        assert!(v.selection.is_none());
    }

    #[test]
    fn a_new_press_releases_one_whose_release_was_lost() {
        let mut v = echoing("\\033[?1000h\\033[?1006h");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: cell(2, 1), clicks: 1, button: MouseButton::Left });
        // No Up arrives.
        h.mouse(&mut v, MouseEvent::Down { pos: cell(4, 1), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: cell(4, 1), dragged: false, button: MouseButton::Left });
        assert!(v.reporting.is_none());
        v.write(b"\r");
        let screen = echoed(&mut v, "^[[<0;5;2m");
        assert_eq!(screen, "READY^[[<0;3;2M^[[<0;5;2m^[[<0;5;2M^[[<0;5;2m");
    }

    #[test]
    fn shift_keeps_the_mouse_local() {
        let mut v = echoing("\\033[?1000h\\033[?1006h");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        h.mouse_with(&mut v, SHIFT, MouseEvent::Down { pos: cell(0, 0), clicks: 1, button: MouseButton::Left });
        assert_eq!(v.press_cell, Some((0, 0)), "a local press");
        assert!(v.reporting.is_none());
        h.mouse_with(&mut v, SHIFT, MouseEvent::Drag { pos: cell(3, 0), button: MouseButton::Left });
        assert!(v.selection.is_some(), "Shift+drag selects");
        h.mouse(&mut v, MouseEvent::Up { pos: cell(3, 0), dragged: true, button: MouseButton::Left });
        let out = h.scroll_with(&mut v, SHIFT, cell(1, 1), 3.0 * UI.cell_h);
        assert!(out.frame && v.pending_scroll == -3, "Shift+wheel scrolls the scrollback");
        // Nothing was reported: only the marker comes back.
        v.write(b"END\r");
        let screen = echoed(&mut v, "END");
        assert!(!screen.contains("^[["), "{screen}");
    }

    #[test]
    fn row_spans_stop_at_the_last_ink_and_merge_colours() {
        let grid = grid_of(&["a b   "]);
        assert_eq!(row_content_len(&grid, 0), 3);
        assert_eq!(row_spans(&grid, 0), vec![("a b".to_string(), [200, 200, 200])]);
    }
}
