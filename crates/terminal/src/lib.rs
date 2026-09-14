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

/// Per-cell rendition flags, backend-neutral.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Foreground/background already swapped by the backend when it built the
    /// snapshot, so the renderer does not need to know about inverse video.
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
    /// True for the lead cell of a double-width grapheme (its trailing cell is
    /// emitted as a blank so column indexing stays 1:1 with the grid).
    pub wide: bool,
}

impl Cell {
    fn blank(fg: Rgb, bg: Rgb) -> Self {
        Cell { text: String::new(), fg, bg, attrs: CellAttrs::default(), wide: false }
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
            cursor: Cursor { col: 0, row: 0, visible: false },
            default_fg,
            default_bg,
        }
    }

    /// Cell at `(col, row)`, or `None` if out of bounds.
    pub fn cell(&self, col: u16, row: u16) -> Option<&Cell> {
        if col >= self.size.cols || row >= self.size.rows {
            return None;
        }
        self.cells.get(row as usize * self.size.cols as usize + col as usize)
    }
}

/// Whether the child process is still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Running,
    /// Exited with the given code, if one was reported.
    Exited(Option<i32>),
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
    /// Function key F1..=F24.
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

/// A hosted terminal. Single-threaded: all methods are called on the UI thread.
pub trait TerminalBackend {
    /// Resize the grid. `cell_w_px`/`cell_h_px` are the pixel size of one cell,
    /// forwarded so in-band size reports and image protocols are accurate.
    fn resize(&mut self, cols: u16, rows: u16, cell_w_px: u32, cell_h_px: u32);

    /// Encode and send a key press to the child.
    fn send_key(&mut self, press: &KeyPress);

    /// Write raw bytes to the child (e.g. bracketed paste payloads).
    fn write_bytes(&mut self, bytes: &[u8]);

    /// Drain any pending child output into the terminal state. Returns `true`
    /// if the grid may have changed and should be re-snapshotted/redrawn.
    fn pump(&mut self) -> bool;

    /// Build a snapshot of the current grid for rendering.
    fn snapshot(&mut self) -> Grid;

    /// The program-set title (OSC 0/2), if any.
    fn title(&self) -> Option<String>;

    /// Current child lifecycle state.
    fn lifecycle(&mut self) -> Lifecycle;
}
