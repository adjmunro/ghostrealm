//! Input delivery to views, and the requests they hand back to the app.

use std::path::PathBuf;

use ghostrealm_core::Config;
use ghostrealm_terminal::Mods;

use super::UiMetrics;

/// Left-button mouse input, in physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseEvent {
    /// A press inside the view. `clicks` counts quick successive presses at about
    /// the same spot (2 = double-click), per `[input] double_click_ms`.
    Down { pos: (f32, f32), clicks: u32 },
    /// The button is held and the cursor moved past the drag threshold since the
    /// `Down` (delivered for the rest of that press, wherever the cursor goes).
    Drag { pos: (f32, f32) },
    /// The press ended. `dragged` says whether any `Drag` preceded it.
    Up { pos: (f32, f32), dragged: bool },
    /// Hover with no button held.
    Move { pos: (f32, f32) },
}

/// Something a view asks the app to do once the current call returns.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// The view just started work the user is waiting on (e.g. a submitted shell
    /// command): show its workspace busy now, before `is_busy` catches up.
    Busy,
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
