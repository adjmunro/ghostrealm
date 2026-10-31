//! Input delivery to views, and the requests they hand back to the app.

use std::path::PathBuf;

use ghostrealm_core::Config;
use ghostrealm_terminal::{KeyPress, Mods};

use super::text_edit::Clipboard;
use super::{DocId, UiMetrics};
use crate::toasts::Level;

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
    /// Open this file in a Files surface (the focused one, else the
    /// workspace's last focused one, else a new one), with whichever plugin
    /// claims the path.
    OpenFile(PathBuf),
    /// Open this URL with the system's handler for its scheme (a link to a
    /// web page).
    OpenUrl(String),
    /// Show this folder selected in a Files surface's sidebar (a link to a
    /// folder).
    ShowInSidebar(PathBuf),
    /// The view wrote this file (the app hot-reloads it if it is the config).
    Saved(PathBuf),
    /// The user chose a setting in the view: write `[section] key = value`
    /// into `config.toml` and reload the config.
    SetConfig { section: &'static str, key: &'static str, value: toml::Value },
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
    /// Open the file and folder context menu on `target` at `at` (physical
    /// px), aligned to it per `align`.
    FileMenu {
        target: crate::file_menu::FileTarget,
        at: (f32, f32),
        align: MenuAlign,
    },
    /// Open the view's own menu at `at` (physical px), aligned to it per
    /// `align`. Choosing an item calls [`View::button`] with its `id`.
    ///
    /// [`View::button`]: super::View::button
    Menu { items: Vec<MenuItem>, at: (f32, f32), align: MenuAlign },
    /// Close the view's whole surface, every document in it, asking first
    /// about any with unsaved changes (as the tab's close button does). A
    /// terminal whose shell exited asks this on the next key.
    Close,
    /// Close document `DocId` of the view (one of its [`View::docs`]), asking
    /// first about its unsaved changes, as Close (Cmd+W) does for the active
    /// one: a file tab's close button.
    ///
    /// [`View::docs`]: super::View::docs
    CloseDoc(super::DocId),
    /// The view just started work the user is waiting on (e.g. a submitted shell
    /// command): show its workspace busy now, before `is_busy` catches up.
    Busy,
    /// Tell the user something went wrong (or right) in a toast on the view's
    /// pane. A toast with a `key` replaces the queued one with the same key.
    /// `doc` is the document it concerns; a host fills it in
    /// ([`EventCx::with_doc`]), so a view leaves it `None`.
    Toast {
        level: Level,
        message: String,
        key: Option<String>,
        choices: Vec<ToastChoice>,
        doc: Option<DocId>,
    },
    /// Remove the toast keyed `key`, if any (what it reported is over).
    DismissToast { key: String },
}

/// A button on a view's toast ([`Request::Toast`]). The app runs it, so the
/// view names no button id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastChoice {
    /// "Save As…": ask where to save the document.
    SaveAs,
}

/// How a menu sits against the point it opens at: its top edge is at the
/// point, and its left (`Left`) or right (`Right`) edge too. A menu opened
/// from a button at a view's right end aligns `Right` so it stays over the
/// view. Either way the app keeps it on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuAlign {
    #[default]
    Left,
    Right,
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
    clipboard: Option<&'a mut dyn Clipboard>,
    out: Outcome,
}

impl<'a> EventCx<'a> {
    pub fn new(
        cfg: &'a Config,
        ui: UiMetrics,
        mods: Mods,
        clipboard: Option<&'a mut dyn Clipboard>,
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
        self.clipboard.as_mut()?.text()
    }

    /// Put `text` on the system clipboard.
    pub fn set_clipboard_text(&mut self, text: String) {
        if let Some(cb) = self.clipboard.as_mut() {
            cb.set_text(text);
        }
    }

    /// Run `f` with `base` added to the item ids of the menus it requests
    /// ([`Request::Menu`]), as [`PaintCx::with_id_base`] does for buttons: a
    /// host passes its hosted view's events through this with the same base.
    ///
    /// [`PaintCx::with_id_base`]: super::PaintCx::with_id_base
    pub fn with_id_base<T>(&mut self, base: u32, f: impl FnOnce(&mut Self) -> T) -> T {
        let from = self.out.requests.len();
        let out = f(self);
        for r in &mut self.out.requests[from..] {
            if let Request::Menu { items, .. } = r {
                for item in items.iter_mut().filter(|i| !i.is_rule()) {
                    item.id += base;
                }
            }
        }
        out
    }

    /// Run `f` with `doc` as the document of the toasts it requests
    /// ([`Request::Toast`]) that name none: a host passes each call to a
    /// hosted document through this with that document's id.
    pub fn with_doc<T>(&mut self, doc: DocId, f: impl FnOnce(&mut Self) -> T) -> T {
        let from = self.out.requests.len();
        let out = f(self);
        for r in &mut self.out.requests[from..] {
            if let Request::Toast { doc: d @ None, .. } = r {
                *d = Some(doc);
            }
        }
        out
    }

    /// The requests and repaint flags gathered during the call.
    pub fn finish(self) -> Outcome {
        self.out
    }
}
