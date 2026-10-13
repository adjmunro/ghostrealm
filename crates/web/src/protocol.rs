//! Messages between the app and the web host process: one JSON object per line,
//! [`Request`]s on the host's stdin and [`Event`]s on its stdout. Page pixels do
//! not travel over the pipe — see [`crate::frame`].
//!
//! Coordinates and sizes are in physical pixels; the host divides by `scale` to
//! get Chromium's view (DIP) coordinates.

use serde::{Deserialize, Serialize};

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
    /// Ask for the main frame's visible text; answered by [`Event::Text`].
    Text {
        id: u64,
        req: u64,
    },
    /// Evaluate JavaScript in the main frame; answered by [`Event::EvalResult`].
    /// The expression may return a promise; its settled value is reported.
    Eval {
        id: u64,
        req: u64,
        js: String,
    },
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
    /// CEF initialised; requests are now served.
    Ready,
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
    Closed {
        id: u64,
    },
    /// The host can't serve requests and is about to exit.
    Failed {
        error: String,
    },
}

/// The console-message prefix the host's eval wrapper logs results under,
/// followed by the request's nonce. The host swallows these; pages never see them
/// in their console.
pub const EVAL_MARKER: &str = "\u{1}ghostrealm-eval\u{1}";

/// The longest eval result the host passes on; longer ones fail.
pub const MAX_EVAL_RESULT: usize = 1 << 20;

/// Wrap `js` so its (awaited) value is reported back as a console message:
/// `EVAL_MARKER <nonce> <ok:0|1> <payload>`. `nonce` must be unguessable and
/// used once: it is what tells the host's own result from a page's log line.
///
/// The wrapper runs in the page's own JavaScript world, so the page can shape
/// the result (by replacing `console.log`, `eval` or `JSON.stringify`): treat it
/// as page-controlled data.
pub fn eval_wrapper(nonce: &str, js: &str) -> String {
    // The source is embedded as a JSON string and run through indirect eval so
    // both expressions and statement lists work.
    let src = serde_json::to_string(js).unwrap_or_else(|_| "\"\"".into());
    let marker = serde_json::to_string(&format!("{EVAL_MARKER}{nonce}")).unwrap_or_default();
    format!(
        "(async () => {{ const m = {marker}; try {{ const v = await (0, eval)({src}); \
         let s; try {{ s = JSON.stringify(v); }} catch (e) {{ s = String(v); }} \
         console.log(m + ' 1 ' + (s === undefined ? 'null' : s)); }} \
         catch (e) {{ console.log(m + ' 0 ' + String(e && e.stack || e)); }} }})();"
    )
}

/// Parse a console message produced by [`eval_wrapper`] into
/// `(nonce, ok, payload)`. `None` for anything else.
pub fn parse_eval_message(msg: &str) -> Option<(&str, bool, &str)> {
    let rest = msg.strip_prefix(EVAL_MARKER)?;
    let (nonce, rest) = rest.split_once(' ')?;
    let (ok, payload) = rest.split_once(' ').unwrap_or((rest, ""));
    Some((nonce, ok == "1", payload))
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
    fn eval_message_parses_and_rejects_page_logs() {
        let msg = format!("{EVAL_MARKER}n0nce 1 {{\"a\":1}}");
        assert_eq!(parse_eval_message(&msg), Some(("n0nce", true, "{\"a\":1}")));
        assert_eq!(parse_eval_message("n0nce 1 hello"), None);
        let err = format!("{EVAL_MARKER}abc 0 ReferenceError: x");
        assert_eq!(
            parse_eval_message(&err),
            Some(("abc", false, "ReferenceError: x"))
        );
    }

    #[test]
    fn eval_wrapper_embeds_source_and_nonce_as_string_literals() {
        let w = eval_wrapper("f00d", "document.title = \"x\"; 1 + 1");
        assert!(w.contains(r#""document.title = \"x\"; 1 + 1""#), "{w}");
        assert!(w.contains("ghostrealm-eval\\u0001f00d\""), "{w}");
    }
}
