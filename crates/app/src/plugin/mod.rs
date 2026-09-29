//! The plugin API: how kinds of content (terminal, editor, file browser, ...)
//! plug into the app.
//!
//! A [`Plugin`] is a kind of content, registered once at startup. It opens
//! [`View`]s — one per surface (a tab in a pane) showing that kind. The app owns
//! the window, GPU, fonts, layout, focus, and input routing; each frame a view is
//! handed its rect and paints it immediate-mode through a [`PaintCx`], and it
//! receives input through an [`EventCx`]. First-party kinds are built on exactly
//! this API (see `crate::plugins`), so they obey the same rules as any other:
//!
//! - **Paint only through the context.** Quads and text go through [`PaintCx`];
//!   text is shaped by the app's one `FontSystem` into the shared glyph atlas,
//!   which lives on the render thread. A view never owns a `FontSystem`.
//! - **Keep heavy work off the UI thread.** Parsing, decoding, and large IO run
//!   on a worker that publishes snapshots and wakes the UI through
//!   [`OpenCx::waker`] (the terminal's `ThreadedTerminal` is the model). Anything
//!   slow in `paint`, `pump`, or an input handler stalls every pane.
//! - **App keybindings win.** A key reaches the focused view only after the app's
//!   own chords (and the palette/overlays) passed on it. A key the view doesn't
//!   handle is dropped; it never falls through to another view.
//! - **Effects go through requests.** A view can't touch the workspace tree while
//!   it is being called; it files a [`Request`] (open a file, report a save, mark
//!   its workspace busy) that the app applies after the call returns.
//! - **Buttons follow the app convention.** Register clickable rects with
//!   [`PaintCx::button`]: the app highlights them on hover and fires
//!   [`View::button`] on mouse-up over the same button (a press can be cancelled
//!   by moving off).

pub mod event;
pub mod paint;
#[cfg(test)]
pub mod testing;
pub mod theme;
pub mod widgets;

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use ghostrealm_core::{Config, Rect, Registry};
use ghostrealm_terminal::{KeyPress, Pumped};

use crate::app_state::AppState;

pub use event::{EventCx, MouseEvent, Outcome, Request};
pub use paint::{Font, Frame, Layer, PaintCx, Shaped, TextItem, TextKit, TextSrc};

/// Called from any thread to wake the UI (it schedules a paced redraw).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// Cell geometry and DPI scale, in physical pixels. Views that lay out on a
/// character grid (all first-party ones) size rows and columns from this.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiMetrics {
    /// Monospace advance width.
    pub cell_w: f32,
    /// Row height.
    pub cell_h: f32,
    /// Physical pixels per logical pixel.
    pub scale: f32,
}

/// What a plugin gets when opening a new view.
#[derive(Clone, Default)]
pub struct OpenCx {
    /// The directory the view should start in (the workspace's root, else the
    /// app default). `None` leaves it to the plugin (e.g. the shell's default).
    pub cwd: Option<PathBuf>,
    /// A command line the app was launched with; the terminal runs it instead of
    /// the login shell. Other kinds ignore it.
    pub command: Option<String>,
    /// Wakes the UI after background work publishes something new.
    pub waker: Option<Waker>,
}

/// A kind of content a surface can hold. Registered once; opens [`View`]s.
pub trait Plugin {
    /// Stable id (snake_case): the prefix of the plugin's commands
    /// (`<id>.new`) and the name its views report from [`View::plugin`].
    fn id(&self) -> &'static str;
    /// Display name (the "nothing open" picker, command titles).
    fn title(&self) -> &'static str;
    /// Whether the "nothing open" picker offers this kind.
    fn pickable(&self) -> bool {
        true
    }
    /// A fresh view of this kind (a new shell, an empty buffer, ...).
    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>>;
    /// Whether this plugin can show `path`. When a file is opened (from the file
    /// browser or a command), the first registered plugin that claims it wins.
    fn opens_path(&self, _path: &Path) -> bool {
        false
    }
    /// A view showing `path`. Only called when [`opens_path`](Self::opens_path)
    /// returned true.
    fn open_path(&self, _path: &Path, cx: &OpenCx) -> Result<Box<dyn View>> {
        self.open(cx)
    }
    /// Add this plugin's commands to the registry (palette, keybindings, agent).
    fn register_commands(&self, _registry: &mut Registry<AppState>) {}
}

/// One surface's content: a live instance of a [`Plugin`]'s kind.
///
/// Every method runs on the UI thread. Only [`paint`](Self::paint) is required;
/// the rest default to "not interested".
pub trait View: Any {
    /// The id of the plugin that opened this view.
    fn plugin(&self) -> &'static str;

    /// The tab title. `None` keeps whatever the tab shows now.
    fn title(&self) -> Option<String> {
        None
    }

    /// Unsaved changes (the tab title shows italic).
    fn modified(&self) -> bool {
        false
    }

    /// Paint the view into `rect` (its whole content area, below the pane's tab
    /// strip). Called only on frames that redraw, and only while the view is its
    /// pane's active tab. A change of `rect` between calls is the resize signal.
    fn paint(&mut self, cx: &mut PaintCx, rect: Rect);

    /// A key press while this view is focused. Returns whether it was used.
    fn key(&mut self, _cx: &mut EventCx, _key: &KeyPress) -> bool {
        false
    }

    /// Left-button mouse input. `Down` arrives for a press inside the view (after
    /// the app focused its pane); `Drag` and `Up` follow for that same press even
    /// if the cursor leaves the view. `Move` is hover with no button held.
    fn mouse(&mut self, _cx: &mut EventCx, _event: &MouseEvent) {}

    /// Wheel/trackpad scroll over the view, in physical pixels. Positive `dy`
    /// means the content should move down (reveal what is above).
    fn scroll(&mut self, _cx: &mut EventCx, _pos: (f32, f32), _dx: f32, _dy: f32) {}

    /// A button registered by the last [`paint`](Self::paint) was clicked.
    fn button(&mut self, _cx: &mut EventCx, _id: u32) {}

    /// Keyboard focus moved onto (`true`) or away from (`false`) this view.
    fn focus_changed(&mut self, _cx: &mut EventCx, _focused: bool) {}

    /// Drain background output into the view's state, bounded to about
    /// `budget` bytes. `changed` marks the view's workspace unread when it isn't
    /// the active one; `more` asks for another pump soon.
    fn pump(&mut self, _budget: usize) -> Pumped {
        Pumped {
            changed: false,
            more: false,
        }
    }

    /// Whether the view is doing work the user is waiting on (a running
    /// command). Drives its workspace's busy status.
    fn is_busy(&self) -> bool {
        false
    }

    /// The next instant [`tick`](Self::tick) should run, if any (a timer).
    fn deadline(&self, _cfg: &Config) -> Option<Instant> {
        None
    }

    /// Run timers that are due at `now`.
    fn tick(&mut self, _cx: &mut EventCx, _now: Instant) {}

    /// Persist unsaved state if it has a home (the app is losing focus or
    /// quitting). Returns the file written, if any.
    fn autosave(&mut self) -> Option<PathBuf> {
        None
    }

    /// Raw input from the agent channel. Returns whether the view accepts it.
    fn write_input(&mut self, _bytes: &[u8]) -> bool {
        false
    }

    /// A plain-text rendering of the view's content, for the agent channel.
    fn text(&mut self) -> Option<String> {
        None
    }

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl dyn View {
    /// The concrete view, if it is a `T`.
    pub fn downcast_ref<T: View>(&self) -> Option<&T> {
        self.as_any().downcast_ref()
    }

    /// The concrete view, if it is a `T`.
    pub fn downcast_mut<T: View>(&mut self) -> Option<&mut T> {
        self.as_any_mut().downcast_mut()
    }
}

/// A centred square of side `side` (inset a little) inside `hit`: the hover
/// highlight of an icon button, smaller than its full hit rect.
pub fn hover_box(hit: Rect, side: f32) -> Rect {
    let s = (side - 4.0).max(1.0);
    Rect {
        x: hit.x + (hit.w - s) * 0.5,
        y: hit.y + (hit.h - s) * 0.5,
        w: s,
        h: s,
    }
}

/// Whether point `(x, y)` lies inside `r` (half-open on the far edges).
pub fn rect_contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}
