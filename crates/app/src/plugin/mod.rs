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
pub mod icons;
pub mod paint;
#[cfg(test)]
pub mod testing;
pub mod text_edit;
pub mod theme;
pub mod widgets;

use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use ghostrealm_core::config::SectionSpec;
use ghostrealm_core::{Config, Rect, Registry};
use ghostrealm_terminal::{KeyPress, Pumped};

use crate::app_state::AppState;

pub use event::{EventCx, MenuAlign, MenuItem, MouseButton, MouseEvent, Outcome, RawKey, Request};
pub use paint::{
    Font, Frame, Image, ImageItem, ImageSource, Layer, PaintCx, RowStyles, Shaped, TextItem, TextKit,
    TextSrc,
};
pub use crate::row_cache::RunStyle;

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

impl Default for UiMetrics {
    /// A nominal 8x16 cell at 1x, for views driven without a window.
    fn default() -> Self {
        UiMetrics {
            cell_w: 8.0,
            cell_h: 16.0,
            scale: 1.0,
        }
    }
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
    /// Starts the language service for a file, if it has one.
    pub languages: Option<Arc<crate::languages::Languages>>,
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
    /// The config sections this plugin reads, for the option catalogue (hover
    /// docs and checking in the config file).
    fn config_sections(&self) -> Vec<SectionSpec> {
        Vec::new()
    }
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

    /// Every key press and release while this view is focused, as the platform
    /// reported it, before [`key`](Self::key). Return true to take it (`key`
    /// then isn't called). For views embedding an engine that does its own key
    /// handling and needs releases and physical keys (a web page). The app's
    /// own chords still win.
    fn raw_key(&mut self, _cx: &mut EventCx, _key: &RawKey) -> bool {
        false
    }

    /// Mouse input. `Down` arrives for a press inside the view (after the app
    /// focused its pane); `Drag` and `Up` follow for that same press even if the
    /// cursor leaves the view. Each carries the press's `button` (left, middle,
    /// or right; a right press comes before [`View::context_menu`]); a view
    /// that handles only the left button returns early on
    /// [`MouseEvent::is_other_button`]. `Move` is hover with no button held.
    fn mouse(&mut self, _cx: &mut EventCx, _event: &MouseEvent) {}

    /// A right-click at `pos` inside the view. A view with a context menu
    /// files [`Request::FileMenu`].
    fn context_menu(&mut self, _cx: &mut EventCx, _pos: (f32, f32)) {}

    /// Wheel/trackpad scroll over the view, in physical pixels. Positive `dy`
    /// means the content should move down (reveal what is above).
    fn scroll(&mut self, _cx: &mut EventCx, _pos: (f32, f32), _dx: f32, _dy: f32) {}

    /// A button registered by the last [`paint`](Self::paint) was clicked, or
    /// an item of the view's [`Request::Menu`] was chosen.
    fn button(&mut self, _cx: &mut EventCx, _id: u32) {}

    /// Keyboard focus moved onto (`true`) or away from (`false`) this view.
    fn focus_changed(&mut self, _cx: &mut EventCx, _focused: bool) {}

    /// The view came on screen (`true`: it is painted, as its pane's active tab
    /// in the shown workspace) or left it. A hidden view can pause rendering
    /// work. Views start hidden until first painted.
    fn shown(&mut self, _visible: bool) {}

    /// Drain background output into the view's state, bounded to about
    /// `budget` bytes. `changed` marks the view's workspace unread when it isn't
    /// the active one; `more` asks for another pump soon.
    fn pump(&mut self, _budget: usize) -> Pumped {
        Pumped {
            changed: false,
            more: false,
        }
    }

    /// Apply results background work posted that aren't output (e.g. a
    /// language service's diagnostics). Returns whether to repaint. Unlike
    /// [`pump`](Self::pump), it never marks the workspace unread.
    fn refresh(&mut self) -> bool {
        false
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

    /// Persist unsaved state if it has a home and the view autosaves (the app is
    /// losing focus or quitting). Returns the file written, if any.
    fn autosave(&mut self, _cfg: &Config) -> Option<PathBuf> {
        None
    }

    /// The file the view shows, if any (Save As starts beside it).
    fn path(&self) -> Option<PathBuf> {
        None
    }

    /// The file or folder the view is about, for the file commands that act on
    /// "the focused file": its [`path`](Self::path), unless it knows better
    /// (the file browser's selected entry).
    fn subject(&self) -> Option<PathBuf> {
        self.path()
    }

    /// Write the view to `path` and make it the view's file (Save As).
    fn save_as(&mut self, _path: PathBuf) -> Result<()> {
        anyhow::bail!("{} views can't be saved", self.plugin())
    }

    /// Write the view to `path` and leave it showing its own file (Save Copy As).
    fn save_copy(&mut self, _path: PathBuf) -> Result<()> {
        anyhow::bail!("{} views can't be saved", self.plugin())
    }

    /// Files changed on disk by the app itself (new, moved, renamed): `moved`
    /// lists each `(from, to)`. A listing re-reads; a view showing a moved file
    /// follows it.
    fn files_changed(&mut self, _moved: &[(PathBuf, PathBuf)]) {}

    /// The file at `path` may have been written by another program. A view
    /// showing it checks, and without unsaved changes re-reads it, so a later
    /// save doesn't write stale text over the new one; with unsaved changes it
    /// keeps them and marks itself. Returns whether what it shows changed.
    fn changed_on_disk(&mut self, _path: &Path) -> bool {
        false
    }

    /// The view is marked as differing from its file on disk (the tab title
    /// shows amber).
    fn marked_changed_on_disk(&self) -> bool {
        false
    }

    /// Whether writing the view's file now would overwrite a version it has
    /// not seen: another program wrote the file since the view loaded or
    /// wrote it.
    fn disk_conflict(&self) -> bool {
        false
    }

    /// Replace the view's content with its file as it is on disk, discarding
    /// unsaved changes.
    fn reload(&mut self) -> Result<()> {
        Ok(())
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
