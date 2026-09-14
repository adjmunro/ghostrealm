//! `libghostty-vt` + `portable-pty` implementation of [`TerminalBackend`].
//!
//! Runs on a single (UI) thread: the VT engine and its render iterators are
//! `!Send`, so they stay here. A dedicated reader thread does the only blocking
//! work (reading the PTY master) and forwards raw bytes over a channel; every
//! call into the VT engine happens in [`GhosttyTerminal::pump`] on this thread.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::{Context, Result};
use ghostrealm_terminal::{
    Cell, CellAttrs, Cursor, Grid, GridSize, Key, KeyPress, Lifecycle, Mods, Pumped, Rgb, Scroll,
    TerminalBackend,
};
use libghostty_vt::key as vtkey;
use libghostty_vt::render::{CellIterator, RenderState, RowIterator};
use libghostty_vt::style::{RgbColor, Underline};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::Terminal;
use portable_pty::{native_pty_system, Child, MasterPty, PtySize};

/// Re-exported so the app can describe a command without depending on
/// `portable-pty` directly.
pub use portable_pty::CommandBuilder;

/// Called by the reader thread after forwarding child output, so an event loop
/// can wake and redraw instead of polling. `Send` only (it lives on the thread).
pub type PtyWaker = Box<dyn FnMut() + Send + 'static>;

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
    /// Bytes the VT engine wants written back to the child (query replies),
    /// pushed by the `on_pty_write` callback during `vt_write`.
    pty_out: Rc<RefCell<VecDeque<u8>>>,
    encoder: vtkey::Encoder<'static>,
    cell_w: u32,
    cell_h: u32,
    /// Cached once the child is observed to have exited.
    exited: Option<Option<i32>>,
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
        {
            let sink = Rc::clone(&pty_out);
            term.on_pty_write(move |_term, data| {
                sink.borrow_mut().extend(data.iter().copied());
            })
            .context("register pty_write callback")?;
        }

        let encoder = vtkey::Encoder::new().context("create key encoder")?;
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
            pty_out,
            encoder,
            cell_w,
            cell_h,
            exited: None,
            reader_done: false,
            dirty: true,
        })
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
        self.snapshot_into(&mut g);
        g
    }

    fn snapshot_into(&mut self, out: &mut Grid) {
        self.dirty = false;
        if let Err(e) = self.build_into(out) {
            eprintln!("ghostrealm: snapshot failed: {e:#}");
            let cols = self.term.cols().unwrap_or(0);
            let rows = self.term.rows().unwrap_or(0);
            *out = Grid::blank(GridSize { cols, rows }, [220, 220, 220], [0, 0, 0]);
        }
    }

    fn title(&self) -> Option<String> {
        self.term
            .title()
            .ok()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    }

    fn lifecycle(&mut self) -> Lifecycle {
        if let Some(code) = self.exited {
            return Lifecycle::Exited(code);
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                let code = Some(status.exit_code() as i32);
                self.exited = Some(code);
                Lifecycle::Exited(code)
            }
            _ => Lifecycle::Running,
        }
    }
}
