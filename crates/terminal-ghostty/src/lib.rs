//! `libghostty-vt` + `portable-pty` implementation of [`TerminalBackend`].
//!
//! Runs on a single (UI) thread: the VT engine and its render iterators are
//! `!Send`, so they stay here. A dedicated reader thread does the only blocking
//! work (reading the PTY master) and forwards raw bytes over a channel; every
//! call into the VT engine happens in [`GhosttyTerminal::pump`] on this thread.

pub mod threaded;
pub use threaded::{ThreadedTerminal, UiWaker};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::{Context, Result};
use ghostrealm_terminal::{
    Cell, CellAttrs, Cursor, Grid, GridSize, Key, KeyPress, Lifecycle, Mods, MouseAction,
    MouseButton, MouseReport, Pumped, Rgb, Scroll, TerminalBackend,
};
use libghostty_vt::error::Error as VtError;
use libghostty_vt::key as vtkey;
use libghostty_vt::mouse as vtmouse;
use libghostty_vt::paste as vtpaste;
use libghostty_vt::terminal::Mode;
use libghostty_vt::render::{CellIterator, RenderState, RowIterator};
use libghostty_vt::screen::Screen;
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::Terminal;
use portable_pty::{native_pty_system, Child, ChildKiller, MasterPty, PtySize};

/// Re-exported so the app can describe a command without depending on
/// `portable-pty` directly.
pub use portable_pty::CommandBuilder;

/// Called by the reader thread after forwarding child output, so an event loop
/// can wake and redraw instead of polling. `Send` only (it lives on the thread).
pub type PtyWaker = Box<dyn FnMut() + Send + 'static>;

/// Scrollback cap in bytes — Ghostty's own `scrollback-limit` default. The
/// library default is 10 KB, which (pruned at ~400 KB page granularity) keeps
/// only about one page of history.
const SCROLLBACK_MAX_BYTES: usize = 10_000_000;

fn rgb(c: RgbColor) -> Rgb {
    [c.r, c.g, c.b]
}

fn blank_cell(fg: Rgb, bg: Rgb) -> Cell {
    Cell {
        text: String::new(),
        fg,
        bg,
        attrs: CellAttrs::default(),
        wide: false,
    }
}

pub struct GhosttyTerminal {
    term: Terminal<'static, 'static>,
    /// Render scratch objects, kept across snapshots — `update()` reuses them
    /// each frame instead of allocating fresh VT render state and iterators.
    render_state: RenderState<'static>,
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
    /// Kept alive for resize; the reader is cloned from it and the writer taken.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Raw child output, forwarded from the reader thread.
    rx: Receiver<Vec<u8>>,
    /// Bytes the reader thread has put on `rx` so far (applied or not).
    forwarded: Arc<AtomicUsize>,
    /// Bytes the VT engine wants written back to the child (query replies),
    /// pushed by the `on_pty_write` callback during `vt_write`.
    pty_out: Rc<RefCell<VecDeque<u8>>>,
    encoder: vtkey::Encoder<'static>,
    mouse_encoder: vtmouse::Encoder<'static>,
    /// Mouse buttons currently held (a bit per [`button_bit`]), which the
    /// encoder needs to report motion outside the grid during a drag.
    mouse_held: u8,
    /// The cell of the last pointer event reported, to drop motion that stays
    /// inside it. (The encoder's own tracking is cleared by every option sync.)
    mouse_cell: Option<(u32, u32)>,
    cell_w: u32,
    cell_h: u32,
    /// Cached once the child is observed to have exited.
    exited: Option<Lifecycle>,
    /// The signal that ended the child, from the `waitid` siginfo of the thread
    /// that waited for it ([`Self::note_exit_signal`]). Preferred over
    /// [`signal_number`], which only knows signals below 32.
    exit_signal: Option<i32>,
    /// The reader thread signalled EOF (child's PTY closed).
    reader_done: bool,
    /// Grid changed since the last `snapshot()`; lets the app skip re-snapshotting
    /// an idle surface. Set by `pump`/`resize`, cleared by `snapshot`.
    dirty: bool,
}

impl GhosttyTerminal {
    /// Spawn `command` (or the user's default shell) attached to a new PTY.
    pub fn spawn(
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
        command: Option<CommandBuilder>,
    ) -> Result<Self> {
        Self::spawn_with_waker(cols, rows, cell_w, cell_h, command, None)
    }

    /// Like [`spawn`](Self::spawn), plus a `waker` the reader thread invokes each
    /// time it forwards output, so an event loop can redraw on demand rather than
    /// polling every frame.
    pub fn spawn_with_waker(
        cols: u16,
        rows: u16,
        cell_w: u32,
        cell_h: u32,
        command: Option<CommandBuilder>,
        waker: Option<PtyWaker>,
    ) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: (cols as u32 * cell_w) as u16,
                pixel_height: (rows as u32 * cell_h) as u16,
            })
            .context("openpty")?;

        let mut cmd = match command {
            Some(c) => c,
            None => CommandBuilder::new_default_prog(),
        };
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair.slave.spawn_command(cmd).context("spawn shell")?;
        // Slave fd is held by the child now; drop our copy so EOF propagates.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().context("clone reader")?;
        let writer = pair.master.take_writer().context("take writer")?;

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let forwarded = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&forwarded);
        std::thread::Builder::new()
            .name("pty-reader".into())
            .spawn(move || {
                let mut waker = waker;
                let mut wake = || {
                    if let Some(w) = waker.as_mut() {
                        w();
                    }
                };
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                            counter.fetch_add(n, Ordering::SeqCst);
                            wake();
                        }
                        Err(_) => break,
                    }
                }
                // Empty send marks EOF so the UI thread can notice promptly.
                let _ = tx.send(Vec::new());
                wake();
            })
            .context("spawn reader thread")?;

        let pty_out: Rc<RefCell<VecDeque<u8>>> = Rc::new(RefCell::new(VecDeque::new()));
        let mut term = Terminal::new(cols, rows).context("create vt terminal")?;
        term.set_scrollback_max_bytes(Some(SCROLLBACK_MAX_BYTES))
            .context("set scrollback limit")?;
        {
            let sink = Rc::clone(&pty_out);
            term.on_pty_write(move |_term, data| {
                sink.borrow_mut().extend(data.iter().copied());
            })
            .context("register pty_write callback")?;
        }

        let encoder = vtkey::Encoder::new().context("create key encoder")?;
        let mouse_encoder = vtmouse::Encoder::new().context("create mouse encoder")?;
        let render_state = RenderState::new().context("render state")?;
        let row_iter = RowIterator::new().context("row iter")?;
        let cell_iter = CellIterator::new().context("cell iter")?;

        Ok(Self {
            term,
            render_state,
            row_iter,
            cell_iter,
            master: pair.master,
            writer,
            child,
            rx,
            forwarded,
            pty_out,
            encoder,
            mouse_encoder,
            mouse_held: 0,
            mouse_cell: None,
            cell_w,
            cell_h,
            exited: None,
            exit_signal: None,
            reader_done: false,
            dirty: true,
        })
    }

    /// Bytes of child output the reader thread has queued for `pump`
    /// so far, applied or not. Lets a test wait for output to arrive without
    /// applying it.
    pub fn output_forwarded(&self) -> usize {
        self.forwarded.load(Ordering::SeqCst)
    }

    /// Whether the child's side of the PTY has closed (its output ended).
    pub fn output_closed(&self) -> bool {
        self.reader_done
    }

    /// The child's process id (its process group's, too: it leads its own
    /// session).
    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Record the signal that ended the child, as its exit waiter saw it, so
    /// [`Terminal::lifecycle`] reports it whatever its number.
    ///
    /// A command can reach the worker after the child exited but before the
    /// waiter reports, so `lifecycle` may already have stored a plain exit; a
    /// signal then upgrades it.
    pub(crate) fn note_exit_signal(&mut self, signal: Option<i32>) {
        self.exit_signal = signal;
        if let (Some(Lifecycle::Exited(_)), Some(signal)) = (self.exited, signal) {
            self.exited = Some(Lifecycle::Signaled(signal));
        }
    }

    /// The PTY's foreground process group (the running command, else the
    /// shell).
    pub fn foreground_group(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    /// A handle that kills the child from another thread.
    pub fn child_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        self.child.clone_killer()
    }

    /// Write whatever the VT engine queued for the child, then flush.
    fn flush_pty_out(&mut self) {
        if self.pty_out.borrow().is_empty() {
            return;
        }
        let bytes: Vec<u8> = self.pty_out.borrow_mut().drain(..).collect();
        let _ = self.writer.write_all(&bytes);
        let _ = self.writer.flush();
    }

    /// Fill `out` from the current VT state, reusing its cell vector and each
    /// cell's string buffer (no per-cell allocation in steady state).
    fn build_into(&mut self, out: &mut Grid) -> Result<()> {
        let snap = self.render_state.update(&self.term).context("snapshot")?;

        let cols = snap.cols().context("cols")?;
        let rows = snap.rows().context("rows")?;
        let colors = snap.colors().context("colors")?;
        let default_fg = rgb(colors.foreground);
        let default_bg = rgb(colors.background);
        let cursor_vp = snap.cursor_viewport().context("cursor")?;
        let cursor_visible = snap.cursor_visible().unwrap_or(false);

        let want = cols as usize * rows as usize;
        let cells = &mut out.cells;
        if cells.len() < want {
            cells.resize(want, blank_cell(default_fg, default_bg));
        }

        // Reset a cell to blank, keeping its string allocation.
        let blank = |dst: &mut Cell| {
            dst.text.clear();
            dst.fg = default_fg;
            dst.bg = default_bg;
            dst.attrs = CellAttrs::default();
            dst.wide = false;
        };

        let mut rows_it = self.row_iter.update(&snap).context("row update")?;
        let mut produced_rows = 0u16;
        while let Some(row) = rows_it.next() {
            let base = produced_rows as usize * cols as usize;
            let mut cells_it = self.cell_iter.update(row).context("cell update")?;
            let mut col = 0u16;
            while let Some(cell) = cells_it.next() {
                if col >= cols {
                    break;
                }
                let style = cell.style().unwrap_or_default();
                let mut fg = cell.fg_color().ok().flatten().map(rgb).unwrap_or(default_fg);
                let mut bg = cell.bg_color().ok().flatten().map(rgb).unwrap_or(default_bg);
                if style.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let dst = &mut cells[base + col as usize];
                dst.text.clear();
                let _ = cell.graphemes_utf8(&mut dst.text);
                dst.fg = fg;
                dst.bg = bg;
                dst.attrs = CellAttrs {
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                    dim: style.faint,
                };
                dst.wide = false;
                col += 1;
            }
            // Blank the rest of the row the iterator did not fill.
            while col < cols {
                blank(&mut cells[base + col as usize]);
                col += 1;
            }
            produced_rows += 1;
            if produced_rows >= rows {
                break;
            }
        }
        // Blank any rows the iterator did not yield (e.g. an all-blank tail).
        for r in produced_rows..rows {
            let base = r as usize * cols as usize;
            for c in 0..cols as usize {
                blank(&mut cells[base + c]);
            }
        }
        cells.truncate(want);

        out.size = GridSize { cols, rows };
        out.default_fg = default_fg;
        out.default_bg = default_bg;
        out.viewport_active = self.term.viewport_active().unwrap_or(true);
        out.alt_screen = self.term.active_screen().is_ok_and(|s| s == Screen::Alternate);
        out.cursor = match cursor_vp {
            Some(c) => Cursor {
                col: c.x,
                row: c.y,
                visible: cursor_visible,
            },
            None => Cursor {
                col: 0,
                row: 0,
                visible: false,
            },
        };
        Ok(())
    }

    /// [`Self::build_into`], or a blank grid of the VT's size if that fails.
    fn fill(&mut self, out: &mut Grid) {
        if let Err(e) = self.build_into(out) {
            eprintln!("ghostrealm: snapshot failed: {e:#}");
            let cols = self.term.cols().unwrap_or(0);
            let rows = self.term.rows().unwrap_or(0);
            *out = Grid::blank(GridSize { cols, rows }, [220, 220, 220], [0, 0, 0]);
        }
    }

    fn map_key(key: Key) -> vtkey::Key {
        match key {
            Key::Char(_) => vtkey::Key::Unidentified,
            Key::Enter => vtkey::Key::Enter,
            Key::Tab => vtkey::Key::Tab,
            Key::Backspace => vtkey::Key::Backspace,
            Key::Escape => vtkey::Key::Escape,
            Key::Delete => vtkey::Key::Delete,
            Key::Insert => vtkey::Key::Insert,
            Key::Up => vtkey::Key::ArrowUp,
            Key::Down => vtkey::Key::ArrowDown,
            Key::Left => vtkey::Key::ArrowLeft,
            Key::Right => vtkey::Key::ArrowRight,
            Key::Home => vtkey::Key::Home,
            Key::End => vtkey::Key::End,
            Key::PageUp => vtkey::Key::PageUp,
            Key::PageDown => vtkey::Key::PageDown,
            Key::Function(n) => match n {
                1 => vtkey::Key::F1,
                2 => vtkey::Key::F2,
                3 => vtkey::Key::F3,
                4 => vtkey::Key::F4,
                5 => vtkey::Key::F5,
                6 => vtkey::Key::F6,
                7 => vtkey::Key::F7,
                8 => vtkey::Key::F8,
                9 => vtkey::Key::F9,
                10 => vtkey::Key::F10,
                11 => vtkey::Key::F11,
                12 => vtkey::Key::F12,
                _ => vtkey::Key::Unidentified,
            },
        }
    }

    fn map_mods(m: Mods) -> vtkey::Mods {
        let mut out = vtkey::Mods::empty();
        if m.shift {
            out |= vtkey::Mods::SHIFT;
        }
        if m.ctrl {
            out |= vtkey::Mods::CTRL;
        }
        if m.alt {
            out |= vtkey::Mods::ALT;
        }
        if m.super_ {
            out |= vtkey::Mods::SUPER;
        }
        out
    }

    /// Encode a key press to its VT byte sequence (no PTY I/O). Exposed for
    /// unit tests; `send_key` calls this and writes the result.
    pub fn encode_key(&mut self, press: &KeyPress) -> Result<Vec<u8>> {
        let mut event = vtkey::Event::new().context("key event")?;
        event.set_action(vtkey::Action::Press);
        event.set_mods(Self::map_mods(press.mods));
        event.set_key(Self::map_key(press.key));
        if let Key::Char(c) = press.key {
            event.set_unshifted_codepoint(c);
            let text = press.text.clone().unwrap_or_else(|| c.to_string());
            event.set_utf8(Some(text));
        } else if let Some(text) = &press.text {
            event.set_utf8(Some(text.clone()));
        }
        self.encoder.set_options_from_terminal(&self.term);
        let mut out = Vec::new();
        self.encoder
            .encode_to_vec(&event, &mut out)
            .context("encode key")?;
        Ok(out)
    }

    /// The bytes that paste `text` into the program (no PTY I/O): bracketed when
    /// the program enabled mode 2004, else as typed.
    pub fn encode_paste(&self, text: &str) -> Result<Vec<u8>> {
        if self.term.mode(Mode::BRACKETED_PASTE).unwrap_or(false) {
            bracketed_paste(text)
        } else {
            Ok(text.as_bytes().to_vec())
        }
    }

    /// Encode a pointer event in the protocol the program selected (empty when
    /// tracking is off or the mode doesn't report this event). Exposed for
    /// unit tests; `send_mouse` calls this and writes the result.
    pub fn encode_mouse(&mut self, report: &MouseReport) -> Result<Vec<u8>> {
        let (cols, rows) = (self.term.cols().unwrap_or(0) as u32, self.term.rows().unwrap_or(0) as u32);
        let (cw, ch) = (self.cell_w.max(1), self.cell_h.max(1));
        // The pane can extend past the grid (padding strip) and a drag or wheel
        // can arrive from outside it. Pin the position to the last pixel of the
        // grid, as xterm does, so no event is dropped as out-of-bounds while its
        // matching press or release is sent.
        // The legacy (X10/normal) format has one byte per coordinate, so it can
        // express cells 0..=222 only and libghostty drops anything beyond that
        // rather than clamping; xterm pins to 222 there too.
        let legacy = ![Mode::UTF8_MOUSE, Mode::SGR_MOUSE, Mode::URXVT_MOUSE, Mode::SGR_PIXELS_MOUSE]
            .iter()
            .any(|m| self.term.mode(*m).unwrap_or(false));
        let (max_cols, max_rows) = if legacy { (cols.min(223), rows.min(223)) } else { (cols, rows) };
        let at = (
            report.at.0.clamp(0.0, (max_cols as f32 - 1.0 / cw as f32).max(0.0)),
            report.at.1.clamp(0.0, (max_rows as f32 - 1.0 / ch as f32).max(0.0)),
        );
        let bit = report.button.map_or(0, button_bit);
        match report.action {
            MouseAction::Press => self.mouse_held |= bit,
            MouseAction::Release => self.mouse_held &= !bit,
            MouseAction::Motion => {}
        }
        let mut event = vtmouse::Event::new().context("mouse event")?;
        event
            .set_action(match report.action {
                MouseAction::Press => vtmouse::Action::Press,
                MouseAction::Release => vtmouse::Action::Release,
                MouseAction::Motion => vtmouse::Action::Motion,
            })
            .set_button(report.button.map(|b| match b {
                MouseButton::Left => vtmouse::Button::Left,
                MouseButton::Middle => vtmouse::Button::Middle,
                MouseButton::Right => vtmouse::Button::Right,
                MouseButton::WheelUp => vtmouse::Button::Four,
                MouseButton::WheelDown => vtmouse::Button::Five,
            }))
            .set_mods(Self::map_mods(report.mods))
            .set_position(vtmouse::Position {
                x: at.0 * cw as f32,
                y: at.1 * ch as f32,
            });
        self.mouse_encoder
            .set_options_from_terminal(&self.term)
            .set_size(vtmouse::EncoderSize {
                screen_width: cols * cw,
                screen_height: rows * ch,
                cell_width: cw,
                cell_height: ch,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            })
            .set_any_button_pressed(self.mouse_held != 0);
        let cell = (
            (at.0 as u32).min(cols.saturating_sub(1)),
            (at.1 as u32).min(rows.saturating_sub(1)),
        );
        let pixels = self.term.mode(Mode::SGR_PIXELS_MOUSE).unwrap_or(false);
        if report.action == MouseAction::Motion && !pixels && self.mouse_cell == Some(cell) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        self.mouse_encoder
            .encode_to_vec(&event, &mut out)
            .context("encode mouse")?;
        if !out.is_empty() {
            self.mouse_cell = Some(cell);
        }
        Ok(out)
    }
}

/// `text` wrapped in the bracketed-paste markers. Control bytes (ESC, so an
/// embedded end marker included, NUL, DEL, and the terminal-driver line-editing
/// keys) become spaces, as in xterm, so a paste can neither end the bracket
/// early nor act as a keystroke.
pub fn bracketed_paste(text: &str) -> Result<Vec<u8>> {
    let mut data = text.as_bytes().to_vec();
    // Markers add 12 bytes; the encoder reports the size it needs if short.
    let mut buf = vec![0u8; data.len() + 16];
    let written = match vtpaste::encode(&mut data, true, &mut buf) {
        Ok(n) => n,
        Err(VtError::OutOfSpace { required }) => {
            buf.resize(required, 0);
            vtpaste::encode(&mut data, true, &mut buf).context("encode paste")?
        }
        Err(e) => return Err(e).context("encode paste"),
    };
    buf.truncate(written);
    Ok(buf)
}

/// A bit per mouse button for [`GhosttyTerminal::mouse_held`]; wheel notches
/// are never held.
fn button_bit(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 4,
        MouseButton::WheelUp | MouseButton::WheelDown => 0,
    }
}

impl TerminalBackend for GhosttyTerminal {
    fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32) {
        if cols == 0 || rows == 0 {
            return;
        }
        self.cell_w = cell_w_px;
        self.cell_h = cell_h_px;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: (cols as u32 * cell_w_px) as u16,
            pixel_height: (rows as u32 * cell_h_px) as u16,
        });
        let _ = self.term.resize(cols, rows, cell_w_px, cell_h_px);
        self.dirty = true;
    }

    fn send_key(&mut self, press: &KeyPress) {
        if let Ok(bytes) = self.encode_key(press) {
            if !bytes.is_empty() {
                let _ = self.writer.write_all(&bytes);
                let _ = self.writer.flush();
            }
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    fn paste(&mut self, text: &str) {
        // Output queued behind this paste may have toggled the mode.
        self.pump_budgeted(usize::MAX);
        if let Ok(bytes) = self.encode_paste(text) {
            self.write_bytes(&bytes);
        }
    }

    fn mouse_tracking(&self) -> bool {
        self.term.is_mouse_tracking().unwrap_or(false)
    }

    fn viewport_active(&self) -> bool {
        self.term.viewport_active().unwrap_or(true)
    }

    fn send_mouse(&mut self, report: &MouseReport) {
        if let Ok(bytes) = self.encode_mouse(report) {
            if !bytes.is_empty() {
                self.write_bytes(&bytes);
            }
        }
    }

    fn scroll(&mut self, scroll: Scroll) {
        let vp = match scroll {
            Scroll::Delta(n) => ScrollViewport::Delta(n as isize),
            Scroll::Top => ScrollViewport::Top,
            Scroll::Bottom => ScrollViewport::Bottom,
        };
        self.term.scroll_viewport(vp);
        self.dirty = true;
    }

    fn pump(&mut self) -> bool {
        self.pump_budgeted(usize::MAX).changed
    }

    fn pump_budgeted(&mut self, max_bytes: usize) -> Pumped {
        let mut changed = false;
        let mut bytes = 0usize;
        let mut more = false;
        loop {
            if bytes >= max_bytes {
                // Budget spent; anything still queued waits for the next pump.
                more = true;
                break;
            }
            match self.rx.try_recv() {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        self.reader_done = true;
                    } else {
                        bytes += chunk.len();
                        self.term.vt_write(&chunk);
                        changed = true;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reader_done = true;
                    break;
                }
            }
        }
        if changed {
            self.dirty = true;
            self.flush_pty_out();
        }
        Pumped { changed, more }
    }

    fn needs_snapshot(&self) -> bool {
        self.dirty
    }

    fn snapshot(&mut self) -> Grid {
        let mut g = Grid::empty();
        self.fill(&mut g);
        g
    }

    fn snapshot_into(&mut self, out: &mut Grid) {
        self.dirty = false;
        self.fill(out);
    }

    fn title(&self) -> Option<String> {
        self.term
            .title()
            .ok()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    fn is_busy(&self) -> bool {
        // The terminal's foreground process group (tcgetpgrp) differs from the
        // shell's own pid/pgrp exactly when a foreground command is running. Job
        // control (interactive shells) is what makes this authoritative; a shell
        // that doesn't move the foreground group reads as never busy.
        match (self.master.process_group_leader(), self.child.process_id()) {
            (Some(fg), Some(shell)) => fg != shell as i32,
            _ => false,
        }
    }

    fn lifecycle(&mut self) -> Lifecycle {
        if let Some(done) = self.exited {
            return done;
        }
        // Read the signal before `try_wait` reaps the child: the waiter's
        // report may not have arrived yet, and `portable_pty`'s status only
        // knows signals below 32.
        #[cfg(unix)]
        if self.exit_signal.is_none() {
            self.exit_signal = self.child.process_id().and_then(exit_signal_now);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let signal = self.exit_signal.or_else(|| status.signal().and_then(signal_number));
                let done = match signal {
                    Some(signal) => Lifecycle::Signaled(signal),
                    None => Lifecycle::Exited(Some(status.exit_code() as i32)),
                };
                self.exited = Some(done);
                done
            }
            _ => Lifecycle::Running,
        }
    }
}

/// A `waitid` look at the child `pid` that leaves it unreaped: `Ok(None)` if
/// it is still running (`nohang`), else `Ok(Some((si_code, si_status)))`.
#[cfg(unix)]
pub(crate) fn peek_exit(pid: u32, nohang: bool) -> std::io::Result<Option<(i32, i32)>> {
    let flags = libc::WEXITED | libc::WNOWAIT | if nohang { libc::WNOHANG } else { 0 };
    // SAFETY: waitid writes only the zeroed siginfo it is given.
    let (r, info) = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let r = libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags);
        (r, info)
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: waitid filled the siginfo for a WEXITED report.
    let (child, status) = unsafe { (info.si_pid(), info.si_status()) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let (child, status) = (info.si_pid, info.si_status);
    // WNOHANG with nothing to report leaves the siginfo zeroed.
    Ok((child != 0).then_some((info.si_code, status)))
}

/// The signal that killed the child `pid`, if it has exited that way and is
/// not yet reaped.
#[cfg(unix)]
fn exit_signal_now(pid: u32) -> Option<i32> {
    let (code, status) = peek_exit(pid, true).ok()??;
    threaded::killed_by(code, status)
}

/// The number of the signal whose `strsignal` text is `description`: the only
/// form of it `portable_pty`'s exit status keeps. Used when no exit waiter
/// supplied the number (the non-threaded terminal); knows signals below 32.
#[cfg(unix)]
fn signal_number(description: &str) -> Option<i32> {
    (1..32).find(|&n| {
        // SAFETY: strsignal returns a NUL-terminated string (or null) that
        // stays valid until the next strsignal call on this thread.
        let text = unsafe { libc::strsignal(n) };
        !text.is_null() && unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy() == description
    })
}

#[cfg(not(unix))]
fn signal_number(_description: &str) -> Option<i32> {
    None
}
