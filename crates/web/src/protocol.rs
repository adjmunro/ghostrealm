//! Messages between the app and the web host process: one JSON object per line,
//! [`Request`]s on the host's stdin and [`Event`]s on its stdout. Page pixels do
//! not travel over the pipe — see [`crate::frame`].
//!
//! Coordinates and sizes are in physical pixels; the host divides by `scale` to
//! get Chromium's view (DIP) coordinates.

use serde::{Deserialize, Serialize};

/// The version of this protocol, frame files included. Bump it whenever a
/// request, an event, or the frame file layout changes incompatibly: the app
/// refuses a host that reports another one.
pub const PROTOCOL_VERSION: u32 = 3;

/// Ids at or above this are pages the host made for pop-ups ([`Event::Popup`]);
/// the app numbers the pages it creates below it.
pub const POPUP_IDS: u64 = 1 << 40;

/// Pop-ups one page may have open (made or being made); the host refuses
/// more ([`Event::PopupRefused`]).
pub const MAX_POPUPS: usize = 4;

/// Modifier bits, numerically identical to CEF's `cef_event_flags_t` so the host
/// passes them through unchanged.
pub mod mods {
    pub const CAPS_LOCK: u32 = 1 << 0;
    pub const SHIFT: u32 = 1 << 1;
    pub const CONTROL: u32 = 1 << 2;
    pub const ALT: u32 = 1 << 3;
    pub const LEFT_MOUSE: u32 = 1 << 4;
    pub const MIDDLE_MOUSE: u32 = 1 << 5;
    pub const RIGHT_MOUSE: u32 = 1 << 6;
    pub const COMMAND: u32 = 1 << 7;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MouseAction {
    Move,
    Leave,
    Down {
        button: MouseButton,
        count: u32,
    },
    Up {
        button: MouseButton,
        count: u32,
    },
    /// Scroll by a pixel delta (positive `dy` scrolls content down, like CEF).
    Wheel {
        dx: i32,
        dy: i32,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Down,
    Up,
    /// A character produced by a key press (sent after its `Down`).
    Char,
}

/// A key event, in the fields CEF's `cef_key_event_t` needs. On macOS Chromium
/// decodes `native_key_code` (the virtual keycode) plus the characters; elsewhere
/// it uses `windows_key_code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub kind: KeyKind,
    pub windows_key_code: i32,
    pub native_key_code: i32,
    pub character: u16,
    pub unmodified_character: u16,
    pub mods: u32,
}

/// Clipboard/editing commands applied to the focused frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditAction {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// App → host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Create {
        id: u64,
        url: String,
        width: u32,
        height: u32,
        scale: f32,
    },
    Close {
        id: u64,
    },
    Navigate {
        id: u64,
        url: String,
    },
    Back {
        id: u64,
    },
    Forward {
        id: u64,
    },
    Reload {
        id: u64,
    },
    Resize {
        id: u64,
        width: u32,
        height: u32,
        scale: f32,
    },
    Focus {
        id: u64,
        focused: bool,
    },
    /// A page not on screen stops painting (and waking the app).
    Visible {
        id: u64,
        visible: bool,
    },
    Mouse {
        id: u64,
        x: i32,
        y: i32,
        mods: u32,
        action: MouseAction,
    },
    Key {
        id: u64,
        key: Key,
    },
    Edit {
        id: u64,
        action: EditAction,
    },
    /// Pop-up `id` no longer closes with its opener (the app shows it as a
    /// tab of its own).
    Detach {
        id: u64,
    },
    /// Ask for the main frame's visible text; answered by [`Event::Text`].
    Text {
        id: u64,
        req: u64,
    },
    /// Evaluate JavaScript in the main frame, in a world isolated from the
    /// page's scripts (see [`eval_wrapper`]); answered by [`Event::EvalResult`].
    /// The expression may return a promise; its settled value is reported.
    Eval {
        id: u64,
        req: u64,
        js: String,
        /// Run it as a user gesture, so it can do what a click can (open a
        /// pop-up). The activation is the frame's: the page's own scripts
        /// can spend it too (write the clipboard, open pop-ups) and can
        /// tell it came.
        #[serde(default)]
        gesture: bool,
    },
}

/// How the page asked for a pop-up to open (Chromium's window disposition).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupKind {
    /// A pop-up window: `window.open` with a size or position.
    #[default]
    Popup,
    /// A new foreground tab: a `target=_blank` link, `window.open` with no
    /// features.
    Tab,
    /// A new background tab (a Cmd- or middle-click on a link).
    BackgroundTab,
    /// A new window (a Shift-click on a link).
    Window,
}

/// Mouse cursor shapes the page asks for (the subset we map to the OS cursor).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cursor {
    #[default]
    Default,
    Pointer,
    Text,
    Crosshair,
    Wait,
    Progress,
    Move,
    NotAllowed,
    Grab,
    Grabbing,
    ResizeEw,
    ResizeNs,
    ResizeNesw,
    ResizeNwse,
    Help,
    None,
}

/// Host → app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    /// CEF initialised; requests are now served. `protocol` is the host's
    /// [`PROTOCOL_VERSION`] (0 from a host older than the field).
    Ready {
        #[serde(default)]
        protocol: u32,
    },
    /// A new frame for `id` is in `path`, slot `slot` (see [`crate::frame`]).
    Frame {
        id: u64,
        path: String,
        slot: u8,
        width: u32,
        height: u32,
        seq: u64,
    },
    Address {
        id: u64,
        url: String,
    },
    Title {
        id: u64,
        title: String,
    },
    Loading {
        id: u64,
        loading: bool,
        can_back: bool,
        can_forward: bool,
    },
    Cursor {
        id: u64,
        cursor: Cursor,
    },
    Text {
        id: u64,
        req: u64,
        text: String,
    },
    EvalResult {
        id: u64,
        req: u64,
        ok: bool,
        /// JSON-encoded value on success, the error message on failure.
        value: String,
    },
    LoadError {
        id: u64,
        url: String,
        error: String,
    },
    /// The page's renderer process ended (crashed, killed, out of memory): the
    /// page stays on its last frame until it is reloaded.
    RendererGone {
        id: u64,
        /// What happened, e.g. "the page's renderer crashed".
        error: String,
    },
    /// Page `opener` opened a pop-up (`window.open`, a link to a new window):
    /// page `id` (from [`POPUP_IDS`] up), made by the host and hidden until
    /// shown. It keeps `window.opener`. Its requests are served at once.
    Popup {
        id: u64,
        opener: u64,
        url: String,
        /// A `Mouse` press or a `Key` reached the opener within the last 5 s
        /// (as long as Chromium's user activation lasts), so it answers that
        /// input. Otherwise the activation that opened it came from a
        /// gesture eval: the eval opened it, or the page's own script did
        /// with that eval's activation.
        from_input: bool,
        kind: PopupKind,
        /// Its initial physical size: what the page asked for, at most the
        /// opener's.
        width: u32,
        height: u32,
    },
    /// Page `opener` already has [`MAX_POPUPS`] pop-ups open, so the one to
    /// `url` was not made (`window.open` returned `null`).
    PopupRefused {
        opener: u64,
        url: String,
    },
    /// The page closed: by a `Close` request, `window.close()`, its opener
    /// closing (a pop-up), or a pop-up that was never made.
    Closed {
        id: u64,
    },
    /// The host can't serve requests and is about to exit.
    Failed {
        error: String,
    },
}

/// The longest eval result the host passes on; longer ones fail.
pub const MAX_EVAL_RESULT: usize = 1 << 20;

/// The expression the host evaluates for `js`. It settles to `[ok, payload]`:
/// `[true, <JSON of the awaited value>]` (`null` for a value JSON can't
/// encode as text), or `[false, <error message>]`. A payload over
/// [`MAX_EVAL_RESULT`] UTF-16 code units is replaced by an error in the page,
/// so a huge value is never sent to the host (which also caps the result at
/// [`MAX_EVAL_RESULT`] bytes).
///
/// The host runs it in an isolated JavaScript world: it shares the page's DOM
/// but not its globals or prototypes, so the page's scripts can't replace what
/// it calls (`eval`, `JSON.stringify`, `Promise`, ...) or how the value
/// encodes. What it reads from the DOM is still page content.
pub fn eval_wrapper(js: &str) -> String {
    // The source is embedded as a JSON string and run through indirect eval so
    // both expressions and statement lists work.
    let src = serde_json::to_string(js).unwrap_or_else(|_| "\"\"".into());
    let max = MAX_EVAL_RESULT;
    format!(
        "(async () => {{ const out = (ok, s) => s.length > {max} \
         ? [false, 'result too large (over {max} UTF-16 code units)'] : [ok, s]; \
         try {{ const v = await (0, eval)({src}); \
         let s; try {{ s = JSON.stringify(v); }} catch (e) {{ s = String(v); }} \
         return out(true, s === undefined ? 'null' : s); }} \
         catch (e) {{ return out(false, String(e && e.stack || e)); }} }})()"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let r = Request::Mouse {
            id: 3,
            x: 10,
            y: 20,
            mods: mods::SHIFT,
            action: MouseAction::Down {
                button: MouseButton::Left,
                count: 2,
            },
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""op":"mouse""#), "{s}");
        assert_eq!(serde_json::from_str::<Request>(&s).unwrap(), r);
    }

    #[test]
    fn events_round_trip_as_tagged_json() {
        let e = Event::Loading {
            id: 1,
            loading: true,
            can_back: false,
            can_forward: true,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(s.contains(r#""ev":"loading""#), "{s}");
        assert_eq!(serde_json::from_str::<Event>(&s).unwrap(), e);
    }

    #[test]
    fn ready_carries_the_protocol_version() {
        let s = serde_json::to_string(&Event::Ready { protocol: PROTOCOL_VERSION }).unwrap();
        assert_eq!(s, format!(r#"{{"ev":"ready","protocol":{PROTOCOL_VERSION}}}"#));
        let old = serde_json::from_str::<Event>(r#"{"ev":"ready"}"#).unwrap();
        assert_eq!(old, Event::Ready { protocol: 0 }, "a host without the field is version 0");
    }

    #[test]
    fn eval_wrapper_embeds_source_as_a_string_literal() {
        let w = eval_wrapper("document.title = \"x\"; 1 + 1");
        assert!(w.contains(r#"(0, eval)("document.title = \"x\"; 1 + 1")"#), "{w}");
        assert!(w.contains(&format!("s.length > {MAX_EVAL_RESULT}")), "{w}");
    }
}
