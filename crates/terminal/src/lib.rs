//! The terminal-backend seam.
//!
//! The app talks to a terminal only through [`TerminalBackend`] and the
//! backend-neutral types here. Concrete backends (e.g. libghostty-vt +
//! portable-pty) live behind this boundary so they can be swapped without the
//! app knowing. Types deliberately avoid any dependency on the VT engine,
//! the PTY layer, or the windowing/render layer.

/// 8-bit-per-channel colour. Resolved to concrete RGB by the backend; the app
/// never deals with palette indices or "default" sentinels.
pub type Rgb = [u8; 3];

/// Per-cell rendition flags, backend-neutral. There is no inverse-video flag:
/// the backend swaps foreground and background when it builds the snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Faint (SGR 2).
    pub dim: bool,
}

/// One rendered grid cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The grapheme cluster to draw. Empty means a blank cell.
    pub text: String,
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: CellAttrs,
    /// Reserved for the lead cell of a double-width grapheme. No backend sets
    /// it yet (every cell reports `false`), so renderers can't rely on it.
    pub wide: bool,
}

impl Cell {
    fn blank(fg: Rgb, bg: Rgb) -> Self {
        Cell {
            text: String::new(),
            fg,
            bg,
            attrs: CellAttrs::default(),
            wide: false,
        }
    }
}

/// Grid dimensions in cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// Cursor position and visibility in viewport cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// An immutable snapshot of the terminal grid for one frame.
#[derive(Clone, Debug)]
pub struct Grid {
    pub size: GridSize,
    /// Row-major, `cols * rows` cells.
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    pub default_fg: Rgb,
    pub default_bg: Rgb,
}

impl Grid {
    /// A fully-blank grid of the given size (used before the first pump and as a
    /// safe fallback if the backend cannot produce a snapshot).
    pub fn blank(size: GridSize, default_fg: Rgb, default_bg: Rgb) -> Self {
        let count = size.cols as usize * size.rows as usize;
        Grid {
            size,
            cells: vec![Cell::blank(default_fg, default_bg); count],
            cursor: Cursor {
                col: 0,
                row: 0,
                visible: false,
            },
            default_fg,
            default_bg,
        }
    }

    /// An empty 0×0 grid. Useful as a reusable target for
    /// [`snapshot_into`](TerminalBackend::snapshot_into), whose fill resizes it.
    pub fn empty() -> Self {
        Grid {
            size: GridSize { cols: 0, rows: 0 },
            cells: Vec::new(),
            cursor: Cursor {
                col: 0,
                row: 0,
                visible: false,
            },
            default_fg: [0, 0, 0],
            default_bg: [0, 0, 0],
        }
    }

    /// Cell at `(col, row)`, or `None` if out of bounds.
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        if col >= self.size.cols || row >= self.size.rows {
            return None;
        }
        self.cells
            .get(row as usize * self.size.cols as usize + col as usize)
    }
}

/// Outcome of a budgeted [`pump`](TerminalBackend::pump_budgeted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pumped {
    /// The grid may have changed and should be re-snapshotted/redrawn.
    pub changed: bool,
    /// The budget was hit with child output still queued; pump again soon.
    pub more: bool,
}

/// A viewport scroll request, backend-neutral.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Move the viewport by `n` lines: negative scrolls up into history (older),
    /// positive scrolls down toward the live bottom.
    Delta(i32),
    /// Jump to the oldest scrollback line.
    Top,
    /// Jump to the live bottom (newest output).
    Bottom,
}

/// Whether the child process is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    /// Exited with the given code, if one was reported.
    Exited(Option<i32>),
    /// Killed by this signal number.
    Signaled(i32),
}

impl Lifecycle {
    /// Whether the child is gone, by exit or by signal.
    pub fn has_exited(self) -> bool {
        self != Lifecycle::Running
    }
}

/// A backend-neutral key. Printable input arrives as [`Key::Char`] (with the
/// resolved text in [`KeyPress::text`]); everything else is a named key the
/// backend maps to the appropriate escape sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Function key F1..=F12.
    Function(u8),
}

/// Active modifier keys at the time of a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_: bool,
}

/// A single key-press event to be encoded and sent to the child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
    /// The composed text this press produces, if any (IME / shifted symbols).
    /// The backend prefers this for [`Key::Char`].
    pub text: Option<String>,
}

/// What a mouse report describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    /// The pointer moved (with `button` set while dragging).
    Motion,
}

/// A mouse button. A wheel notch is a [`Press`](MouseAction::Press) of a wheel
/// "button" with no release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// One pointer event to report to a program that enabled mouse tracking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseReport {
    pub action: MouseAction,
    /// The button involved; `None` for motion with nothing held.
    pub button: Option<MouseButton>,
    pub mods: Mods,
    /// Pointer position in cells from the grid's top-left, fractional within a
    /// cell (`(2.5, 0.5)` is the centre of the third column, first row). The
    /// backend turns it into the cell or pixel coordinates the program asked for.
    pub at: (f32, f32),
}

/// A hosted terminal. Single-threaded: all methods are called on the UI thread.
pub trait TerminalBackend {
    /// Resize the grid. `cell_w_px`/`cell_h_px` are the pixel size of one cell,
    /// forwarded so in-band size reports and image protocols are accurate.
    fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32);

    /// Encode and send a key press to the child.
    fn send_key(&mut self, press: &KeyPress);

    /// Write raw bytes to the child (e.g. bracketed paste payloads).
    fn write_bytes(&mut self, bytes: &[u8]);

    /// Paste `text`: wrapped in the bracketed-paste markers when the program
    /// enabled that mode (with control bytes, including any embedded end marker,
    /// neutralised), else written as typed. The default writes it as typed.
    fn paste(&mut self, text: &str) {
        self.write_bytes(text.as_bytes());
    }

    /// Whether the program enabled mouse tracking (X10, normal, button or any
    /// event), so pointer events belong to it ([`send_mouse`](Self::send_mouse))
    /// rather than to local selection and scrolling.
    fn mouse_tracking(&self) -> bool {
        false
    }

    /// Whether the viewport shows the live screen (not scrolled back into
    /// history). Pointer reports address the live screen, so they are only
    /// meaningful while this holds.
    fn viewport_active(&self) -> bool {
        true
    }

    /// Whether the program switched to the alternate screen, which keeps no
    /// scrollback.
    fn alt_screen(&self) -> bool {
        false
    }

    /// Report a pointer event in whichever protocol the program selected. Does
    /// nothing unless [`mouse_tracking`](Self::mouse_tracking) is on and the
    /// tracking mode reports this kind of event.
    fn send_mouse(&mut self, _report: &MouseReport) {}

    /// Move the scrollback viewport. Marks the grid dirty so the next frame
    /// re-snapshots the scrolled region.
    fn scroll(&mut self, scroll: Scroll);

    /// Drain any pending child output into the terminal state. Returns `true`
    /// if the grid may have changed and should be re-snapshotted/redrawn.
    fn pump(&mut self) -> bool;

    /// Like [`pump`](Self::pump) but stops after roughly `max_bytes` of child
    /// output so one giant burst cannot monopolise a frame. The default drains
    /// everything (ignoring the budget); backends that can bound the work
    /// override this.
    fn pump_budgeted(&mut self, _max_bytes: usize) -> Pumped {
        Pumped {
            changed: self.pump(),
            more: false,
        }
    }

    /// A copy of the current grid. A peek: it leaves
    /// [`needs_snapshot`](Self::needs_snapshot) alone, so an occasional reader
    /// (the agent) can't hide a change from the render path.
    fn snapshot(&mut self) -> Grid;

    /// Snapshot into an existing [`Grid`], reusing its allocations (cell strings,
    /// the cell vector). The hot render path passes the surface's previous grid so
    /// a steady stream of snapshots does not re-allocate every cell each frame.
    /// The default just overwrites `out`; efficient backends override it.
    fn snapshot_into(&mut self, out: &mut Grid) {
        *out = self.snapshot();
    }

    /// Whether the grid changed since the last
    /// [`snapshot_into`](Self::snapshot_into) and so needs re-snapshotting.
    /// Backends that cannot tell return `true` (always re-snapshot).
    fn needs_snapshot(&self) -> bool {
        true
    }

    /// The program-set title (OSC 0/2), if any.
    fn title(&self) -> Option<String>;

    /// Whether a foreground command (a process group other than the shell's own)
    /// is running on the terminal — i.e. the shell is not sitting at its prompt.
    /// Backends that cannot tell return `false`.
    fn is_busy(&self) -> bool {
        false
    }

    /// Current child lifecycle state.
    fn lifecycle(&mut self) -> Lifecycle;
}
