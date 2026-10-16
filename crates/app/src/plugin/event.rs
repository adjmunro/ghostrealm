//! Input delivery to views, and the requests they hand back to the app.

use std::path::PathBuf;

use ghostrealm_core::Config;
use ghostrealm_terminal::{KeyPress, Mods};

use super::UiMetrics;

/// A mouse button a press is made with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Mouse input, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseEvent {
    /// A press inside the view. `clicks` counts quick successive presses of
    /// `button` at about the same spot (2 = double-click), per
    /// `[input] double_click_ms`.
    Down { pos: (f32, f32), clicks: u32, button: MouseButton },
    /// `button` is held and the cursor moved past the drag threshold since the
    /// `Down` (delivered for the rest of that press, wherever the cursor goes).
    Drag { pos: (f32, f32), button: MouseButton },
    /// The press of `button` ended. `dragged` says whether any `Drag` preceded
    /// it.
    Up { pos: (f32, f32), dragged: bool, button: MouseButton },
    /// Hover with no button held.
    Move { pos: (f32, f32) },
    /// The pointer, last hovering this view, moved on to something else
    /// (another view, the app's chrome, a modal, or out of the window; never
    /// during a press): drop hover state. `pos` is where it went.
    Leave { pos: (f32, f32) },
}

impl MouseEvent {
    /// The button of a press (`Down`, `Drag`, `Up`); `None` for hover.
    pub fn button(&self) -> Option<MouseButton> {
        match *self {
            Self::Down { button, .. } | Self::Drag { button, .. } | Self::Up { button, .. } => Some(button),
            Self::Move { .. } | Self::Leave { .. } => None,
        }
    }

    /// Whether this is a press of a button other than the left (views that
    /// handle only the left button ignore these).
    pub fn is_other_button(&self) -> bool {
        self.button().is_some_and(|b| b != MouseButton::Left)
    }
}

/// A key event as the platform reported it — presses and releases, with the
/// physical key — for views that forward keys to an engine with its own
/// keyboard handling (a web page). See [`View::raw_key`](super::View::raw_key).
pub struct RawKey<'a> {
    pub event: &'a winit::event::KeyEvent,
    /// The press as the app maps it for [`View::key`](super::View::key); `None`
    /// for a release or a key with no mapping.
    pub press: Option<KeyPress>,
}

/// Something a view asks the app to do once the current call returns.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Open this file beside/in the view's pane, per `[file_browser] open_in`,
    /// with whichever plugin claims the path.
    OpenFile(PathBuf),
    /// The view wrote this file (the app hot-reloads it if it is the config).
    Saved(PathBuf),
    /// A view hosted in the floating picker chose this path: the picker confirms
    /// with it.
    Pick(PathBuf),
    /// Ask where to save the view (the picker), then call [`View::save_as`].
    ///
    /// [`View::save_as`]: super::View::save_as
    SaveAs,
    /// The view's file changed on disk since it loaded or wrote it, so saving
    /// needs an answer first: ask whether to overwrite it or reload (the view
    /// stays as it is until then).
    SaveConflict,
    /// Open the file and folder context menu on `target` at `at` (physical px).
    FileMenu {
        target: crate::file_menu::FileTarget,
        at: (f32, f32),
    },
    /// Open the view's own menu at `at` (physical px). Choosing an item calls
    /// [`View::button`] with its `id`.
    ///
    /// [`View::button`]: super::View::button
    Menu { items: Vec<MenuItem>, at: (f32, f32) },
    /// Close the view's surface, as the Close command would. A terminal whose
    /// shell exited asks this on the next key.
    Close,
    /// The view just started work the user is waiting on (e.g. a submitted shell
    /// command): show its workspace busy now, before `is_busy` catches up.
    Busy,
}

/// An item of a view's menu ([`Request::Menu`]).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    /// The item's text; empty for a [rule](Self::RULE).
    pub label: String,
    /// `Some` for a toggle: whether a check mark shows.
    pub checked: Option<bool>,
    /// Dim text at the right end (a shortcut, a value).
    pub hint: Option<String>,
    /// A disabled item shows dim and can't be chosen.
    pub enabled: bool,
    /// What [`View::button`](super::View::button) hears when it is chosen.
    pub id: u32,
}

impl MenuItem {
    /// A horizontal rule between sections; not clickable.
    pub const RULE: MenuItem = MenuItem {
        label: String::new(),
        checked: None,
        hint: None,
        enabled: false,
        id: u32::MAX,
    };

    /// An enabled plain item.
    pub fn new(id: u32, label: impl Into<String>) -> Self {
        MenuItem { label: label.into(), checked: None, hint: None, enabled: true, id }
    }

    /// A toggle, checked when `on`.
    pub fn check(self, on: bool) -> Self {
        MenuItem { checked: Some(on), ..self }
    }

    pub fn with_hint(self, hint: impl Into<String>) -> Self {
        MenuItem { hint: Some(hint.into()), ..self }
    }

    pub fn enable(self, on: bool) -> Self {
        MenuItem { enabled: on, ..self }
    }

    pub fn is_rule(&self) -> bool {
        self.label.is_empty()
    }
}

/// What an input handler produced, for the app to apply.
#[derive(Debug, Default)]
pub struct Outcome {
    pub requests: Vec<Request>,
    /// Repaint soon.
    pub redraw: bool,
    /// Repaint on the paced frame clock (a burst of scroll/animation steps).
    pub frame: bool,
}

/// What an input handler gets: the config, cell geometry, modifier state, the
/// clipboard, and a place to file requests.
pub struct EventCx<'a> {
    pub cfg: &'a Config,
    pub ui: UiMetrics,
    /// Modifier keys held right now (also on the key itself for key events).
    pub mods: Mods,
    clipboard: Option<&'a mut arboard::Clipboard>,
    out: Outcome,
}

impl<'a> EventCx<'a> {
    pub fn new(
        cfg: &'a Config,
        ui: UiMetrics,
        mods: Mods,
        clipboard: Option<&'a mut arboard::Clipboard>,
    ) -> Self {
        EventCx {
            cfg,
            ui,
            mods,
            clipboard,
            out: Outcome::default(),
        }
    }

    /// Ask the app to do something after this call returns.
    pub fn request(&mut self, r: Request) {
        self.out.requests.push(r);
    }

    /// The view's appearance changed: repaint.
    pub fn redraw(&mut self) {
        self.out.redraw = true;
    }

    /// Repaint on the paced frame clock (for high-frequency changes like
    /// scrolling, so bursts coalesce into one frame per interval).
    pub fn request_frame(&mut self) {
        self.out.redraw = true;
        self.out.frame = true;
    }

    /// The system clipboard's text, if any.
    pub fn clipboard_text(&mut self) -> Option<String> {
        self.clipboard.as_mut()?.get_text().ok()
    }

    /// Put `text` on the system clipboard.
    pub fn set_clipboard_text(&mut self, text: String) {
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
    }

    /// The requests and repaint flags gathered during the call.
    pub fn finish(self) -> Outcome {
        self.out
    }
}
