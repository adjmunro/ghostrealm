//! Off-screen browsers and their CEF handlers. Everything here runs on CEF's UI
//! thread (requests arrive as posted tasks), so state lives in thread-locals.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::raw::c_int;
use std::time::{Duration, Instant};

use cef::*;
use ghostrealm_web::frame::FrameWriter;
use ghostrealm_web::protocol::{
    eval_wrapper, Cursor, EditAction, Event, KeyKind, MouseAction, MouseButton, Request,
    MAX_EVAL_RESULT,
};
use serde_json::{json, Value};

use crate::emit;

#[cfg(target_os = "macos")]
type CursorHandle = *mut u8;
#[cfg(not(target_os = "macos"))]
type CursorHandle = std::os::raw::c_ulong;

/// Render state for one browser, shared by its handlers.
struct View {
    /// Physical size of the area the app shows the page in.
    width: u32,
    height: u32,
    scale: f32,
    frames: FrameWriter,
    /// Last full view paint, kept so a popup (e.g. a `<select>` list) can be
    /// composited over it.
    page: Vec<u8>,
    page_w: u32,
    page_h: u32,
    popup: Option<Popup>,
}

struct Popup {
    /// Position in view (DIP) coordinates.
    rect: Rect,
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

type SharedView = std::rc::Rc<RefCell<View>>;

struct Entry {
    browser: Browser,
    view: SharedView,
    /// Keeps the DevTools observer that answers evals attached.
    _devtools: Option<Registration>,
    eval: EvalState,
}

/// How a browser's DevTools session stands for evals.
#[derive(Default)]
struct EvalState {
    /// `Runtime.enable` succeeded, so context events arrive.
    runtime: bool,
    /// The `uniqueId` of the eval world in the current document.
    world: Option<String>,
    /// Eval worlds announced by `Runtime.executionContextCreated`, newest last:
    /// `(executionContextId, uniqueId)`.
    created: Vec<(i64, String)>,
}

thread_local! {
    static BROWSERS: RefCell<HashMap<u64, Entry>> = RefCell::new(HashMap::new());
    /// Set once the app hung up: quit the message loop when the last browser closes.
    static QUITTING: Cell<bool> = const { Cell::new(false) };
    /// Evals awaiting a DevTools reply, by message id.
    static EVALS: RefCell<HashMap<i32, PendingEval>> = RefCell::new(HashMap::new());
    static NEXT_MESSAGE: Cell<i32> = const { Cell::new(1) };
}

/// The isolated world evals run in. One per document, created on first use:
/// it shares the page's DOM but none of its scripts' globals or prototypes.
const EVAL_WORLD: &str = "ghostrealm-eval";

/// An eval in flight: its DevTools steps run in order, each answered through
/// [`HostDevTools`].
struct PendingEval {
    id: u64,
    req: u64,
    js: String,
    started: Instant,
    step: Step,
    /// The cached world was gone (the page navigated); it was recreated once.
    retried: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `Runtime.enable`, so the world's context (and its `uniqueId`) is reported.
    Enable,
    /// `Page.getFrameTree`, for the main frame's DevTools id.
    FrameTree,
    /// `Page.createIsolatedWorld`.
    World,
    /// `Runtime.evaluate` in the world.
    Evaluate,
}

/// An eval whose promise hasn't settled by now is forgotten (the app gave up
/// long before).
const EVAL_TTL: Duration = Duration::from_secs(60);

/// Eval worlds remembered from context events, per browser.
const CREATED_MAX: usize = 8;

fn with_eval_state<T>(id: u64, f: impl FnOnce(&mut EvalState) -> T) -> Option<T> {
    BROWSERS.with_borrow_mut(|m| m.get_mut(&id).map(|e| f(&mut e.eval)))
}

/// Start evaluating `js` in page `id` for request `req`.
///
/// It runs in an isolated world addressed by its `uniqueId`, which (unlike an
/// `executionContextId`) can't name a context in another renderer process
/// after a navigation, so the script never lands in the page's own world.
fn start_eval(id: u64, req: u64, js: String) {
    EVALS.with_borrow_mut(|m| m.retain(|_, e| e.started.elapsed() < EVAL_TTL));
    let Some((runtime, world)) = with_eval_state(id, |s| (s.runtime, s.world.clone())) else {
        emit(&Event::EvalResult { id, req, ok: false, value: format!("unknown browser {id}") });
        return;
    };
    let e = PendingEval { id, req, js, started: Instant::now(), step: Step::Enable, retried: false };
    match (runtime, world) {
        (_, Some(world)) => evaluate(e, &world),
        (true, None) => frame_tree(e),
        (false, None) => devtools(e, Step::Enable, "Runtime.enable", json!({})),
    }
}

fn frame_tree(e: PendingEval) {
    devtools(e, Step::FrameTree, "Page.getFrameTree", json!({}));
}

fn evaluate(e: PendingEval, world: &str) {
    let expression = eval_wrapper(&e.js);
    devtools(
        e,
        Step::Evaluate,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "uniqueContextId": world,
            "awaitPromise": true,
            "returnByValue": true,
            "silent": true,
        }),
    );
}

/// Send one DevTools method for `e`'s next step.
fn devtools(mut e: PendingEval, step: Step, method: &str, params: Value) {
    let Some(host) = browser(e.id).and_then(|b| b.host()) else {
        return finish(&e, false, "the page closed".into());
    };
    let msg = NEXT_MESSAGE.get();
    NEXT_MESSAGE.set(msg.checked_add(1).unwrap_or(1));
    let line = json!({ "id": msg, "method": method, "params": params }).to_string();
    e.step = step;
    let (id, req) = (e.id, e.req);
    EVALS.with_borrow_mut(|m| m.insert(msg, e));
    if host.send_dev_tools_message(Some(line.as_bytes())) == 0 {
        EVALS.with_borrow_mut(|m| m.remove(&msg));
        emit(&Event::EvalResult { id, req, ok: false, value: "DevTools refused the eval".into() });
    }
}

fn finish(e: &PendingEval, ok: bool, value: String) {
    let (ok, value) = if value.len() > MAX_EVAL_RESULT {
        (false, format!("result too large ({} bytes, max {MAX_EVAL_RESULT})", value.len()))
    } else {
        (ok, value)
    };
    emit(&Event::EvalResult { id: e.id, req: e.req, ok, value });
}

/// The reply to DevTools message `msg` on page `id`: advance its eval.
fn devtools_result(id: u64, msg: i32, success: bool, result: &[u8]) {
    let pending = EVALS.with_borrow_mut(|m| match m.get(&msg) {
        Some(e) if e.id == id => m.remove(&msg),
        _ => None,
    });
    let Some(mut e) = pending else { return };
    let v: Value = serde_json::from_slice(result).unwrap_or(Value::Null);
    if !success {
        if e.step == Step::Evaluate && !e.retried {
            // The world went with its document: make a new one.
            e.retried = true;
            with_eval_state(id, |s| s.world = None);
            return frame_tree(e);
        }
        let message = v["message"].as_str().unwrap_or("DevTools call failed");
        return finish(&e, false, message.to_string());
    }
    match e.step {
        Step::Enable => {
            with_eval_state(id, |s| s.runtime = true);
            frame_tree(e);
        }
        Step::FrameTree => match v["frameTree"]["frame"]["id"].as_str() {
            Some(frame) => {
                let params = json!({ "frameId": frame, "worldName": EVAL_WORLD });
                devtools(e, Step::World, "Page.createIsolatedWorld", params);
            }
            None => finish(&e, false, "the page has no main frame".into()),
        },
        Step::World => {
            let context = v["executionContextId"].as_i64();
            let world = with_eval_state(id, |s| {
                let unique = s
                    .created
                    .iter()
                    .rev()
                    .find(|(c, _)| Some(*c) == context)
                    .map(|(_, u)| u.clone());
                s.world.clone_from(&unique);
                unique
            })
            .flatten();
            match world {
                Some(world) => evaluate(e, &world),
                None => finish(&e, false, "the isolated world did not start".into()),
            }
        }
        Step::Evaluate => {
            if let Some(ex) = v.get("exceptionDetails") {
                let message = ex["exception"]["description"].as_str().or(ex["text"].as_str());
                return finish(&e, false, message.unwrap_or("eval failed").to_string());
            }
            match v["result"]["value"].as_array().map(Vec::as_slice) {
                Some([Value::Bool(ok), Value::String(payload)]) => finish(&e, *ok, payload.clone()),
                _ => finish(&e, false, "eval returned no result".into()),
            }
        }
    }
}

/// A DevTools event on page `id`: track the eval world's context.
fn devtools_event(id: u64, method: &str, params: &[u8]) {
    let context_event = matches!(
        method,
        "Runtime.executionContextCreated"
            | "Runtime.executionContextDestroyed"
            | "Runtime.executionContextsCleared"
    );
    if !context_event {
        return;
    }
    let v: Value = serde_json::from_slice(params).unwrap_or(Value::Null);
    with_eval_state(id, |s| match method {
        "Runtime.executionContextCreated" => {
            let c = &v["context"];
            if let (Some(EVAL_WORLD), Some(ctx), Some(unique)) =
                (c["name"].as_str(), c["id"].as_i64(), c["uniqueId"].as_str())
            {
                if s.created.len() == CREATED_MAX {
                    s.created.remove(0);
                }
                s.created.push((ctx, unique.to_string()));
            }
        }
        "Runtime.executionContextDestroyed" => {
            let gone = v["executionContextUniqueId"].as_str();
            if gone.is_some() && s.world.as_deref() == gone {
                s.world = None;
            }
            s.created.retain(|(_, u)| Some(u.as_str()) != gone);
        }
        _ => {
            s.world = None;
            s.created.clear();
        }
    });
}

/// Run one request (or, for `None`, the app's hang-up) on the UI thread.
pub fn handle(req: Option<Request>) {
    let Some(req) = req else {
        shut_down();
        return;
    };
    match req {
        Request::Create {
            id,
            url,
            width,
            height,
            scale,
        } => create(id, &url, width, height, scale),
        Request::Close { id } => {
            if let Some(host) = browser(id).and_then(|b| b.host()) {
                host.close_browser(1);
            }
        }
        Request::Navigate { id, url } => {
            if let Some(frame) = browser(id).and_then(|b| b.main_frame()) {
                frame.load_url(Some(&CefString::from(url.as_str())));
            }
        }
        Request::Back { id } => {
            if let Some(b) = browser(id) {
                b.go_back();
            }
        }
        Request::Forward { id } => {
            if let Some(b) = browser(id) {
                b.go_forward();
            }
        }
        Request::Reload { id } => {
            if let Some(b) = browser(id) {
                b.reload();
            }
        }
        Request::Resize {
            id,
            width,
            height,
            scale,
        } => resize(id, width, height, scale),
        Request::Focus { id, focused } => {
            if let Some(host) = browser(id).and_then(|b| b.host()) {
                host.set_focus(focused as c_int);
            }
        }
        Request::Visible { id, visible } => {
            if let Some(host) = browser(id).and_then(|b| b.host()) {
                host.was_hidden((!visible) as c_int);
            }
        }
        Request::Mouse {
            id,
            x,
            y,
            mods,
            action,
        } => mouse(id, x, y, mods, action),
        Request::Key { id, key } => {
            let Some(host) = browser(id).and_then(|b| b.host()) else {
                return;
            };
            let type_ = match key.kind {
                KeyKind::Down => KeyEventType::RAWKEYDOWN,
                KeyKind::Up => KeyEventType::KEYUP,
                KeyKind::Char => KeyEventType::CHAR,
            };
            let event = KeyEvent {
                size: std::mem::size_of::<KeyEvent>(),
                type_,
                modifiers: key.mods,
                windows_key_code: key.windows_key_code,
                native_key_code: key.native_key_code,
                character: key.character,
                unmodified_character: key.unmodified_character,
                ..Default::default()
            };
            host.send_key_event(Some(&event));
        }
        Request::Edit { id, action } => {
            let Some(b) = browser(id) else { return };
            let Some(frame) = b.focused_frame().or_else(|| b.main_frame()) else {
                return;
            };
            match action {
                EditAction::Undo => frame.undo(),
                EditAction::Redo => frame.redo(),
                EditAction::Cut => frame.cut(),
                EditAction::Copy => frame.copy(),
                EditAction::Paste => frame.paste(),
                EditAction::SelectAll => frame.select_all(),
            }
        }
        Request::Text { id, req } => match browser(id).and_then(|b| b.main_frame()) {
            Some(frame) => frame.text(Some(&mut TextVisitor::new(id, req))),
            None => emit(&Event::Text {
                id,
                req,
                text: String::new(),
            }),
        },
        Request::Eval { id, req, js } => start_eval(id, req, js),
    }
}

fn browser(id: u64) -> Option<Browser> {
    BROWSERS.with_borrow(|m| m.get(&id).map(|e| e.browser.clone()))
}

fn view(id: u64) -> Option<SharedView> {
    BROWSERS.with_borrow(|m| m.get(&id).map(|e| e.view.clone()))
}

fn create(id: u64, url: &str, width: u32, height: u32, scale: f32) {
    if browser(id).is_some() {
        resize(id, width, height, scale);
        return;
    }
    let view: SharedView = std::rc::Rc::new(RefCell::new(View {
        width: width.max(1),
        height: height.max(1),
        scale: if scale > 0.0 { scale } else { 1.0 },
        frames: FrameWriter::for_page(crate::frame_dir(), id),
        page: Vec::new(),
        page_w: 0,
        page_h: 0,
        popup: None,
    }));
    let mut client = HostClient::new(
        HostRender::new(view.clone(), id),
        HostDisplay::new(id),
        HostLoad::new(id),
        HostLifeSpan::new(id),
        HostContextMenu::new(),
    );
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    };
    let settings = BrowserSettings {
        windowless_frame_rate: 60,
        // Opaque white, so pages without a background don't composite transparent.
        background_color: 0xFFFF_FFFF,
        ..Default::default()
    };
    let created = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
    );
    match created {
        Some(browser) => {
            let devtools = browser
                .host()
                .and_then(|h| h.add_dev_tools_message_observer(Some(&mut HostDevTools::new(id))));
            let entry = Entry { browser, view, _devtools: devtools, eval: EvalState::default() };
            BROWSERS.with_borrow_mut(|m| m.insert(id, entry));
        }
        None => emit(&Event::LoadError {
            id,
            url: url.to_string(),
            error: "failed to create browser".into(),
        }),
    }
}

fn resize(id: u64, width: u32, height: u32, scale: f32) {
    let (Some(view), Some(host)) = (view(id), browser(id).and_then(|b| b.host())) else {
        return;
    };
    let scale_changed = {
        let mut v = view.borrow_mut();
        let scale = if scale > 0.0 { scale } else { v.scale };
        let changed = (scale - v.scale).abs() > f32::EPSILON;
        v.width = width.max(1);
        v.height = height.max(1);
        v.scale = scale;
        changed
    };
    if scale_changed {
        host.notify_screen_info_changed();
    }
    host.was_resized();
}

fn mouse(id: u64, x: i32, y: i32, mods: u32, action: MouseAction) {
    let (Some(view), Some(host)) = (view(id), browser(id).and_then(|b| b.host())) else {
        return;
    };
    let scale = view.borrow().scale;
    let event = MouseEvent {
        x: (x as f32 / scale).round() as c_int,
        y: (y as f32 / scale).round() as c_int,
        modifiers: mods,
    };
    let button = |b: MouseButton| match b {
        MouseButton::Left => MouseButtonType::LEFT,
        MouseButton::Middle => MouseButtonType::MIDDLE,
        MouseButton::Right => MouseButtonType::RIGHT,
    };
    match action {
        MouseAction::Move => host.send_mouse_move_event(Some(&event), 0),
        MouseAction::Leave => host.send_mouse_move_event(Some(&event), 1),
        MouseAction::Down { button: b, count } => {
            host.send_mouse_click_event(Some(&event), button(b), 0, count as c_int)
        }
        MouseAction::Up { button: b, count } => {
            host.send_mouse_click_event(Some(&event), button(b), 1, count as c_int)
        }
        MouseAction::Wheel { dx, dy } => host.send_mouse_wheel_event(
            Some(&event),
            (dx as f32 / scale).round() as c_int,
            (dy as f32 / scale).round() as c_int,
        ),
    }
}

/// The app hung up: close every browser, then quit once they are gone.
fn shut_down() {
    QUITTING.set(true);
    let hosts: Vec<BrowserHost> =
        BROWSERS.with_borrow(|m| m.values().filter_map(|e| e.browser.host()).collect());
    if hosts.is_empty() {
        quit_message_loop();
        return;
    }
    for host in hosts {
        host.close_browser(1);
    }
}

/// Write the current page (plus any visible popup) to the frame file and
/// announce it.
fn present(id: u64, v: &mut View) {
    if v.page.is_empty() {
        return;
    }
    let (w, h) = (v.page_w, v.page_h);
    let written = match &v.popup {
        Some(p) if !p.pixels.is_empty() => {
            let mut out = v.page.clone();
            let px = (p.rect.x as f32 * v.scale).round() as i64;
            let py = (p.rect.y as f32 * v.scale).round() as i64;
            blit(&mut out, w, h, &p.pixels, p.width, p.height, px, py);
            v.frames.write(w, h, &out)
        }
        _ => {
            let View { frames, page, .. } = v;
            frames.write(w, h, page)
        }
    };
    if let Ok(f) = written {
        emit(&Event::Frame {
            id,
            path: f.path,
            slot: f.slot,
            width: w,
            height: h,
            seq: f.seq,
        });
    }
}

/// Copy a `sw x sh` BGRA image into a `dw x dh` one at `(x, y)`, clipped.
#[allow(clippy::too_many_arguments)]
fn blit(dst: &mut [u8], dw: u32, dh: u32, src: &[u8], sw: u32, sh: u32, x: i64, y: i64) {
    for row in 0..sh as i64 {
        let dy = y + row;
        if dy < 0 || dy >= dh as i64 {
            continue;
        }
        let x0 = x.max(0);
        let x1 = (x + sw as i64).min(dw as i64);
        if x1 <= x0 {
            return;
        }
        let s = ((row * sw as i64 + (x0 - x)) * 4) as usize;
        let d = ((dy * dw as i64 + x0) * 4) as usize;
        let n = ((x1 - x0) * 4) as usize;
        dst[d..d + n].copy_from_slice(&src[s..s + n]);
    }
}

fn cursor_kind(t: CursorType) -> Cursor {
    match t {
        CursorType::HAND => Cursor::Pointer,
        CursorType::IBEAM | CursorType::VERTICALTEXT => Cursor::Text,
        CursorType::CROSS | CursorType::CELL => Cursor::Crosshair,
        CursorType::WAIT => Cursor::Wait,
        CursorType::PROGRESS => Cursor::Progress,
        CursorType::MOVE => Cursor::Move,
        CursorType::NOTALLOWED | CursorType::NODROP => Cursor::NotAllowed,
        CursorType::GRAB => Cursor::Grab,
        CursorType::GRABBING => Cursor::Grabbing,
        CursorType::EASTRESIZE
        | CursorType::WESTRESIZE
        | CursorType::EASTWESTRESIZE
        | CursorType::COLUMNRESIZE => Cursor::ResizeEw,
        CursorType::NORTHRESIZE
        | CursorType::SOUTHRESIZE
        | CursorType::NORTHSOUTHRESIZE
        | CursorType::ROWRESIZE => Cursor::ResizeNs,
        CursorType::NORTHEASTRESIZE
        | CursorType::SOUTHWESTRESIZE
        | CursorType::NORTHEASTSOUTHWESTRESIZE => Cursor::ResizeNesw,
        CursorType::NORTHWESTRESIZE
        | CursorType::SOUTHEASTRESIZE
        | CursorType::NORTHWESTSOUTHEASTRESIZE => Cursor::ResizeNwse,
        CursorType::HELP => Cursor::Help,
        CursorType::NONE => Cursor::None,
        _ => Cursor::Default,
    }
}

fn string(s: Option<&CefString>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

wrap_render_handler! {
    struct HostRender {
        view: SharedView,
        id: u64,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let v = self.view.borrow();
            rect.x = 0;
            rect.y = 0;
            rect.width = ((v.width as f32 / v.scale).round() as c_int).max(1);
            rect.height = ((v.height as f32 / v.scale).round() as c_int).max(1);
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> c_int {
            let Some(info) = screen_info else { return 0 };
            info.device_scale_factor = self.view.borrow().scale;
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: c_int) {
            if show == 0 {
                let mut v = self.view.borrow_mut();
                v.popup = None;
                present(self.id, &mut v);
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(rect) = rect else { return };
            let mut v = self.view.borrow_mut();
            match &mut v.popup {
                Some(p) => p.rect = rect.clone(),
                None => {
                    v.popup = Some(Popup { rect: rect.clone(), pixels: Vec::new(), width: 0, height: 0 })
                }
            }
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: c_int,
            height: c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (w, h) = (width as u32, height as u32);
            // SAFETY: CEF hands us a tightly packed BGRA buffer of width*height pixels,
            // valid for the duration of this call.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, w as usize * h as usize * 4) };
            let mut v = self.view.borrow_mut();
            if type_ == PaintElementType::POPUP {
                if let Some(p) = &mut v.popup {
                    p.pixels.clear();
                    p.pixels.extend_from_slice(pixels);
                    p.width = w;
                    p.height = h;
                }
            } else {
                v.page.clear();
                v.page.extend_from_slice(pixels);
                v.page_w = w;
                v.page_h = h;
            }
            present(self.id, &mut v);
        }
    }
}

wrap_display_handler! {
    struct HostDisplay {
        id: u64,
    }

    impl DisplayHandler {
        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() != 0) {
                emit(&Event::Address { id: self.id, url: string(url) });
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            emit(&Event::Title { id: self.id, title: string(title) });
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: CursorHandle,
            type_: CursorType,
            _custom_cursor_info: Option<&CursorInfo>,
        ) -> c_int {
            emit(&Event::Cursor { id: self.id, cursor: cursor_kind(type_) });
            1
        }
    }
}

wrap_load_handler! {
    struct HostLoad {
        id: u64,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: c_int,
            can_go_back: c_int,
            can_go_forward: c_int,
        ) {
            emit(&Event::Loading {
                id: self.id,
                loading: is_loading != 0,
                can_back: can_go_back != 0,
                can_forward: can_go_forward != 0,
            });
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // A cancelled navigation (e.g. superseded by another) is not a failure.
            if error_code == Errorcode::ABORTED || !frame.is_some_and(|f| f.is_main() != 0) {
                return;
            }
            emit(&Event::LoadError { id: self.id, url: string(failed_url), error: string(error_text) });
        }
    }
}

wrap_life_span_handler! {
    struct HostLifeSpan {
        id: u64,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut c_int>,
        ) -> c_int {
            // One page per surface (the pane system provides tabs): open popups and
            // new-window links in place.
            if let (Some(frame), Some(url)) = (browser.and_then(|b| b.main_frame()), target_url) {
                frame.load_url(Some(url));
            }
            1
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            EVALS.with_borrow_mut(|m| m.retain(|_, e| e.id != self.id));
            let empty = BROWSERS.with_borrow_mut(|m| {
                m.remove(&self.id);
                m.is_empty()
            });
            emit(&Event::Closed { id: self.id });
            if empty && QUITTING.get() {
                quit_message_loop();
            }
        }
    }
}

wrap_context_menu_handler! {
    struct HostContextMenu;

    impl ContextMenuHandler {
        fn on_before_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
        ) {
            // Off-screen browsers have no native menu to show it in.
            if let Some(model) = model {
                model.clear();
            }
        }
    }
}

wrap_client! {
    struct HostClient {
        render: RenderHandler,
        display: DisplayHandler,
        load: LoadHandler,
        life_span: LifeSpanHandler,
        context_menu: ContextMenuHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }

        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(self.context_menu.clone())
        }
    }
}

wrap_dev_tools_message_observer! {
    struct HostDevTools {
        id: u64,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: c_int,
            success: c_int,
            result: Option<&[u8]>,
        ) {
            devtools_result(self.id, message_id, success != 0, result.unwrap_or_default());
        }

        fn on_dev_tools_event(&self, _browser: Option<&mut Browser>, method: Option<&CefString>, params: Option<&[u8]>) {
            devtools_event(self.id, &string(method), params.unwrap_or_default());
        }
    }
}

wrap_string_visitor! {
    struct TextVisitor {
        id: u64,
        req: u64,
    }

    impl CefStringVisitor {
        fn visit(&self, string: Option<&CefString>) {
            emit(&Event::Text { id: self.id, req: self.req, text: self::string(string) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::blit;

    #[test]
    fn blit_clips_to_the_destination() {
        // 3x2 destination of zeros; 2x2 source of 9s placed at (2, 1).
        let mut dst = vec![0u8; 3 * 2 * 4];
        blit(&mut dst, 3, 2, &[9u8; 2 * 2 * 4], 2, 2, 2, 1);
        let px = |x: usize, y: usize| dst[(y * 3 + x) * 4];
        assert_eq!(px(2, 1), 9, "in-bounds corner copied");
        assert_eq!(px(1, 1), 0);
        assert_eq!(px(2, 0), 0, "row above untouched");
    }
}
