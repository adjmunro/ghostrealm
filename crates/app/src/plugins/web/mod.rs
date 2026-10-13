//! The web plugin: one web page per view (the pane system provides tabs), with
//! a back / forward / reload toolbar and an editable URL bar.
//!
//! Pages render in the out-of-process CEF host (`ghostrealm-web-host`, see
//! [`ghostrealm_web`]): one host serves every web view and is spawned on first
//! use. The view shows the host's latest frame with [`PaintCx::image`] and
//! forwards mouse and raw key events. Without a window (the agent channel) a
//! page is created at a default size the first time it is used.

mod keys;
mod url;

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_core::config::{Kind, OptionSpec, SectionSpec};
use ghostrealm_core::{
    ArgKind, ArgSpec, Args, CmdError, CmdOutcome, CommandMeta, Rect, Registry, SurfaceId,
};
use ghostrealm_terminal::{Key, KeyPress};
use ghostrealm_web::protocol::mods;
use ghostrealm_web::{
    Cursor, EditAction, MouseAction, MouseButton, Request as HostRequest, SharedFrame, ViewState,
    WebHost,
};
use glyphon::Family;
use winit::window::CursorIcon;

use crate::app_state::AppState;
use crate::plugin::text_edit::{Edit, LineEdit};
use crate::plugin::{
    rect_contains, theme, widgets, EventCx, ImageSource, MouseEvent, OpenCx, PaintCx, Plugin,
    RawKey, View, Waker,
};

pub use url::normalize_url;

pub const ID: &str = "web";

/// Toolbar button ids (see [`View::button`]).
const BTN_BACK: u32 = 0;
const BTN_FORWARD: u32 = 1;
const BTN_RELOAD: u32 = 2;

/// Size a page is created at before it is painted (and stays at without a window).
const DEFAULT_SIZE: (u32, u32) = (1024, 768);
/// Agent reads wait this long for a page to finish loading, and for the host to
/// answer (page text, eval).
const LOAD_WAIT: Duration = Duration::from_secs(15);
const REPLY_WAIT: Duration = Duration::from_secs(5);
/// Behind a page whose frame hasn't arrived (or lags a resize).
const PAGE_BG: [u8; 3] = [255, 255, 255];
/// After the host exits, the next spawn waits this long, doubling with each
/// exit in a row up to [`RETRY_MAX`] (Reload retries at once).
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);

/// The host process, shared by every web view.
#[derive(Default)]
struct HostSlot {
    host: Option<WebHost>,
    /// Bumped each time a host is spawned; a view whose page was created in an
    /// older host recreates it.
    generation: u64,
    /// Why there is no (ready) host, shown in every web view. Cleared once a
    /// host reports ready.
    error: Option<String>,
    /// Host exits since one was last ready.
    failures: u32,
    /// No host is spawned before this (after one exited).
    retry_at: Option<Instant>,
    /// The host to run instead of discovering one.
    #[cfg(test)]
    exe: Option<std::path::PathBuf>,
    /// `[web]`, refreshed from the live config on every paint.
    cfg: ghostrealm_core::Web,
    waker: Option<Waker>,
    next_page: u64,
}

impl HostSlot {
    /// The running host, spawning it if needed (but not while backing off
    /// after an exit).
    fn host(&mut self) -> Option<&mut WebHost> {
        if let Some(h) = &self.host {
            let ready = h.is_ready();
            if !h.is_alive() {
                let error = match h.failure() {
                    Some(e) => e,
                    None if ready => "the web host exited".into(),
                    None => "the web host exited while starting".into(),
                };
                self.host = None;
                self.failed(error);
            } else if ready {
                self.failures = 0;
                self.error = None;
            }
        }
        if self.host.is_none() && self.retry_at.is_none_or(|t| Instant::now() >= t) {
            self.retry_at = None;
            match self.find_host() {
                None => {
                    self.error = Some(
                        "web host not found: build it with `mise run web`, or set [web] host"
                            .into(),
                    );
                }
                Some(exe) => match WebHost::spawn(&exe, self.waker.clone()) {
                    Ok(h) => {
                        self.host = Some(h);
                        self.generation += 1;
                    }
                    Err(e) => self.failed(format!("{e:#}")),
                },
            }
        }
        self.host.as_mut()
    }

    fn find_host(&self) -> Option<std::path::PathBuf> {
        #[cfg(test)]
        if let Some(exe) = &self.exe {
            return Some(exe.clone());
        }
        let explicit = self
            .cfg
            .host
            .as_deref()
            .and_then(ghostrealm_core::config::expand_tilde);
        ghostrealm_web::find_host(explicit.as_deref())
    }

    /// The host exited or couldn't start: show why, and back off.
    fn failed(&mut self, error: String) {
        let wait = RETRY_FIRST.saturating_mul(1 << self.failures.min(6)).min(RETRY_MAX);
        self.failures += 1;
        self.error = Some(error);
        self.retry_at = Some(Instant::now() + wait);
    }

    /// Spawn on the next use, without waiting out the back-off.
    fn retry_now(&mut self) {
        self.retry_at = None;
    }

    /// When a host can next be spawned, while there is none.
    fn retry_at(&self) -> Option<Instant> {
        self.retry_at.filter(|_| self.host.is_none())
    }
}

pub struct WebPlugin {
    slot: Rc<RefCell<HostSlot>>,
}

impl WebPlugin {
    pub fn new() -> Self {
        WebPlugin {
            slot: Rc::default(),
        }
    }
}

impl Default for WebPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for WebPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "Web browser"
    }

    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        let page = {
            let mut slot = self.slot.borrow_mut();
            if slot.waker.is_none() {
                slot.waker = cx.waker.clone();
            }
            slot.next_page += 1;
            slot.next_page
        };
        Ok(Box::new(WebView::new(self.slot.clone(), page)))
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        let id_arg = || {
            ArgSpec::optional(
                "id",
                ArgKind::Int,
                "web surface id (default: the focused one)",
            )
        };
        r.register(
            CommandMeta::new(
                "web.new",
                "New Web Browser",
                "Open a web page in the focused pane",
            )
            .arg(ArgSpec::optional(
                "url",
                ArgKind::Str,
                "address or search terms (default: [web] home)",
            )),
            Box::new(|s: &mut AppState, a| {
                s.open_plugin_in_focused(ID)
                    .map_err(|e| CmdError::Failed(format!("{e:#}")))?;
                let sid = s
                    .focused_surface()
                    .ok_or_else(|| CmdError::Failed("no pane to open in".into()))?;
                let page = target(s, &Args::new())?;
                if let Some(u) = a.opt_str("url") {
                    page.start = Some(u.to_string());
                }
                // Start the page now, so a host that can't run is reported here.
                if !page.send(None) {
                    return Err(CmdError::Failed(format!(
                        "opened web surface {}, but {}",
                        sid.0,
                        page.no_host()
                    )));
                }
                Ok(CmdOutcome::msg(format!("opened web surface {}", sid.0)))
            }),
        );
        r.register(
            CommandMeta::new(
                "web.navigate",
                "Web: Go to URL",
                "Load an address or search in a web surface",
            )
            .arg(ArgSpec::required(
                "url",
                ArgKind::Str,
                "address or search terms",
            ))
            .arg(id_arg()),
            Box::new(|s: &mut AppState, a| {
                let input = a.get_str("url")?.to_string();
                target(s, a)?.navigate(&input).map_err(CmdError::Failed)?;
                Ok(CmdOutcome::ok())
            }),
        );
        for (cmd, title, desc) in [
            (
                "web.back",
                "Web: Back",
                "Go back in a web surface's history",
            ),
            (
                "web.forward",
                "Web: Forward",
                "Go forward in a web surface's history",
            ),
            ("web.reload", "Web: Reload", "Reload a web surface's page"),
        ] {
            r.register(
                CommandMeta::new(cmd, title, desc).arg(id_arg()),
                Box::new(move |s: &mut AppState, a| {
                    let page = target(s, a)?;
                    match cmd {
                        "web.back" => page.back(),
                        "web.forward" => page.forward(),
                        _ => page.reload(),
                    }
                    .map_err(CmdError::Failed)?;
                    Ok(CmdOutcome::ok())
                }),
            );
        }
        r.register(
            CommandMeta::new(
                "web.focus_url",
                "Web: Edit URL",
                "Focus the focused web page's URL bar",
            ),
            Box::new(|s: &mut AppState, a| {
                target(s, a)?.edit_url();
                Ok(CmdOutcome::ok())
            }),
        );
        r.register(
            CommandMeta::new(
                "web.eval",
                "Web: Evaluate JavaScript",
                "Run JavaScript in a web page (after it loads) and return its JSON-encoded \
                 result. It runs in the page's own context, so the page can change the result",
            )
            .arg(ArgSpec::required(
                "js",
                ArgKind::Str,
                "expression or statements; a promise is awaited",
            ))
            .arg(id_arg())
            .hidden(),
            Box::new(|s: &mut AppState, a| {
                let js = a.get_str("js")?.to_string();
                target(s, a)?
                    .eval(&js)
                    .map(CmdOutcome::msg)
                    .map_err(CmdError::Failed)
            }),
        );
    }

    fn config_sections(&self) -> Vec<SectionSpec> {
        vec![SectionSpec::options(
            "web",
            "Web browser views.",
            vec![
                OptionSpec::new(
                    "home",
                    Kind::Str,
                    Some("\"https://duckduckgo.com\""),
                    "The page a new web view opens.",
                ),
                OptionSpec::new(
                    "search",
                    Kind::Str,
                    Some("\"https://duckduckgo.com/?q={}\""),
                    "Where URL-bar input that isn't an address goes; `{}` is the query.",
                ),
                OptionSpec::new(
                    "host",
                    Kind::Path,
                    None,
                    "The web host executable. Unset means beside the ghostrealm binary \
                     (build it with `mise run web`).",
                ),
            ],
        )]
    }
}

/// The web view a command targets: surface `id` when given, else the focused one.
fn target<'a>(s: &'a mut AppState, a: &Args) -> Result<&'a mut WebView, CmdError> {
    let sid = match a.get_int("id") {
        Ok(id) => SurfaceId(id as u64),
        Err(_) => s
            .focused_surface()
            .ok_or_else(|| CmdError::Failed("no web surface focused; pass `id`".into()))?,
    };
    s.view_mut(sid)
        .and_then(|v| v.downcast_mut::<WebView>())
        .ok_or_else(|| match a.get_int("id") {
            Ok(id) => CmdError::Failed(format!("surface {id} is not a web surface")),
            Err(_) => CmdError::Failed("no web surface focused; pass `id`".into()),
        })
}

/// A page frame as an image source. Its pixels are copied out of the shared
/// frame when first asked for (the upload), so the host can't change them
/// mid-read.
struct PageImage {
    frame: SharedFrame,
    pixels: std::sync::OnceLock<Option<Vec<u8>>>,
}

impl ImageSource for PageImage {
    fn size(&self) -> (u32, u32) {
        (self.frame.width, self.frame.height)
    }

    fn bgra(&self) -> Option<&[u8]> {
        self.pixels.get_or_init(|| self.frame.read()).as_deref()
    }

    /// A page frame covers its rect: its alpha channel is not used.
    fn opaque(&self) -> bool {
        true
    }
}

pub struct WebView {
    slot: Rc<RefCell<HostSlot>>,
    /// This page's id in the host.
    page: u64,
    /// Host generation the page was created in (0 = not yet).
    created_in: u64,
    /// URL-bar input to open first (else `[web] home`); then the last address
    /// seen, to reopen the page if the host restarts.
    start: Option<String>,
    last_url: String,
    /// Size last sent: physical `(width, height)` and scale.
    sent_size: Option<(u32, u32, f32)>,
    sent_focus: bool,
    sent_visible: bool,
    /// The URL bar while it is being edited.
    url_edit: Option<LineEdit>,
    /// Page state as of the last [`View::refresh`].
    state: ViewState,
    /// Layout from the last paint, for mouse hit-testing.
    url_box: Rect,
    body: Rect,
    /// A left press on the page is held (drags go to the page).
    pressed: Option<u32>,
    hovering: bool,
}

impl WebView {
    fn new(slot: Rc<RefCell<HostSlot>>, page: u64) -> Self {
        let zero = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        WebView {
            slot,
            page,
            created_in: 0,
            start: None,
            last_url: String::new(),
            sent_size: None,
            sent_focus: false,
            sent_visible: true,
            url_edit: None,
            state: ViewState::default(),
            url_box: zero,
            body: zero,
            pressed: None,
            hovering: false,
        }
    }

    /// Make sure the page exists in a running host (creating it at `size`, or
    /// the last/default size), then send `req` if given. Returns false when there
    /// is no host.
    fn send(&mut self, req: Option<HostRequest>) -> bool {
        let mut slot = self.slot.borrow_mut();
        if slot.host().is_none() {
            return false;
        }
        let (search, home, generation) = (
            slot.cfg.search.clone(),
            slot.cfg.home.clone(),
            slot.generation,
        );
        let Some(host) = slot.host.as_mut() else {
            return false;
        };
        if self.created_in != generation {
            let url = match self.start.take() {
                Some(input) => normalize_url(&input, &search),
                None if !self.last_url.is_empty() => self.last_url.clone(),
                None => home,
            };
            let (width, height, scale) =
                self.sent_size
                    .unwrap_or((DEFAULT_SIZE.0, DEFAULT_SIZE.1, 1.0));
            if !host.send(&HostRequest::Create {
                id: self.page,
                url,
                width,
                height,
                scale,
            }) {
                return false;
            }
            self.created_in = generation;
            self.sent_focus = false;
            self.sent_visible = true;
        }
        match req {
            Some(r) => host.send(&r),
            None => true,
        }
    }

    /// Run `f` with the host (after making sure the page exists).
    fn with_host<T>(&mut self, f: impl FnOnce(&mut WebHost, u64) -> T) -> Option<T> {
        if !self.send(None) {
            return None;
        }
        let mut slot = self.slot.borrow_mut();
        slot.host.as_mut().map(|h| f(h, self.page))
    }

    fn no_host(&self) -> String {
        self.slot
            .borrow()
            .error
            .clone()
            .unwrap_or_else(|| "web host not running".into())
    }

    /// Load an address or search terms.
    pub fn navigate(&mut self, input: &str) -> Result<(), String> {
        let search = self.slot.borrow().cfg.search.clone();
        let url = normalize_url(input, &search);
        self.navigation(HostRequest::Navigate { id: self.page, url })
    }

    pub fn back(&mut self) -> Result<(), String> {
        self.refresh_state();
        if !self.state.can_back {
            return Err("no previous page".into());
        }
        self.navigation(HostRequest::Back { id: self.page })
    }

    pub fn forward(&mut self) -> Result<(), String> {
        self.refresh_state();
        if !self.state.can_forward {
            return Err("no next page".into());
        }
        self.navigation(HostRequest::Forward { id: self.page })
    }

    /// Reload the page; retries a host that exited without waiting.
    pub fn reload(&mut self) -> Result<(), String> {
        self.slot.borrow_mut().retry_now();
        self.navigation(HostRequest::Reload { id: self.page })
    }

    /// Send a navigation and arm the next agent read to wait for its load.
    fn navigation(&mut self, req: HostRequest) -> Result<(), String> {
        let page = self.page;
        self.with_host(|h, _| h.expect_load(page));
        if self.send(Some(req)) {
            Ok(())
        } else {
            Err(self.no_host())
        }
    }

    /// Start editing the URL bar with the address selected; while already
    /// editing, select what has been typed.
    pub fn edit_url(&mut self) {
        let e = self
            .url_edit
            .get_or_insert_with(|| LineEdit::new(self.state.url.clone()));
        let _ = e.apply(Edit::SelectAll, &mut None::<String>);
        self.sync_focus(true);
    }

    /// Evaluate `js` once the page has loaded; `Ok` is the JSON-encoded value.
    pub fn eval(&mut self, js: &str) -> Result<String, String> {
        match self.with_host(|h, id| {
            h.wait_loaded(id, LOAD_WAIT);
            h.eval(id, js, REPLY_WAIT)
        }) {
            Some(r) => r,
            None => Err(self.no_host()),
        }
    }

    fn refresh_state(&mut self) -> bool {
        let slot = self.slot.borrow();
        let Some(v) = slot.host.as_ref().and_then(|h| h.view(self.page)) else {
            return false;
        };
        let changed = v.version != self.state.version;
        if !v.url.is_empty() {
            self.last_url = v.url.clone();
        }
        self.state = v;
        changed
    }

    /// Tell the host whether the page has keyboard focus (not while the URL bar
    /// is being edited).
    fn sync_focus(&mut self, view_focused: bool) {
        let want = view_focused && self.url_edit.is_none();
        if want != self.sent_focus
            && self.send(Some(HostRequest::Focus {
                id: self.page,
                focused: want,
            }))
        {
            self.sent_focus = want;
        }
    }

    fn mouse_to_page(&mut self, pos: (f32, f32), extra: u32, mods_now: u32, action: MouseAction) {
        let req = HostRequest::Mouse {
            id: self.page,
            x: (pos.0 - self.body.x).round() as i32,
            y: (pos.1 - self.body.y).round() as i32,
            mods: mods_now | extra,
            action,
        };
        self.send(Some(req));
    }

    /// A key while the URL bar is being edited. Enter loads, Escape cancels;
    /// everything else edits the field.
    fn url_key(&mut self, cx: &mut EventCx, press: &KeyPress) -> bool {
        let Some(edit) = self.url_edit.as_mut() else {
            return false;
        };
        match press.key {
            // Cmd+L when no `[keybindings]` entry takes it first.
            Key::Char(c) if press.mods.super_ && c.eq_ignore_ascii_case(&'l') => self.edit_url(),
            Key::Enter if !press.mods.super_ => {
                let input = edit.text().to_string();
                self.url_edit = None;
                let _ = self.navigate(&input);
                self.sync_focus(true);
            }
            Key::Escape => {
                self.url_edit = None;
                self.sync_focus(true);
            }
            _ => {
                if !edit.key(press, cx).used() {
                    return false;
                }
            }
        }
        cx.redraw();
        true
    }

    /// A Cmd chord on the page: clipboard and editing commands, reload, and
    /// Cmd+L (when no `[keybindings]` entry takes it first).
    fn page_chord(&mut self, c: char, shift: bool) -> bool {
        let action = match c.to_ascii_lowercase() {
            'l' => {
                self.edit_url();
                return true;
            }
            'c' => EditAction::Copy,
            'x' => EditAction::Cut,
            'v' => EditAction::Paste,
            'a' => EditAction::SelectAll,
            'z' if shift => EditAction::Redo,
            'z' => EditAction::Undo,
            'r' => return self.reload().is_ok(),
            _ => return false,
        };
        self.send(Some(HostRequest::Edit {
            id: self.page,
            action,
        }))
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        if self.created_in == 0 {
            return;
        }
        if let Ok(mut slot) = self.slot.try_borrow_mut() {
            if slot.generation == self.created_in {
                if let Some(h) = slot.host.as_mut() {
                    h.send(&HostRequest::Close { id: self.page });
                    h.forget(self.page);
                }
            }
        }
    }
}

fn cursor_icon(c: Cursor) -> CursorIcon {
    match c {
        Cursor::Default | Cursor::None => CursorIcon::Default,
        Cursor::Pointer => CursorIcon::Pointer,
        Cursor::Text => CursorIcon::Text,
        Cursor::Crosshair => CursorIcon::Crosshair,
        Cursor::Wait => CursorIcon::Wait,
        Cursor::Progress => CursorIcon::Progress,
        Cursor::Move => CursorIcon::Move,
        Cursor::NotAllowed => CursorIcon::NotAllowed,
        Cursor::Grab => CursorIcon::Grab,
        Cursor::Grabbing => CursorIcon::Grabbing,
        Cursor::ResizeEw => CursorIcon::EwResize,
        Cursor::ResizeNs => CursorIcon::NsResize,
        Cursor::ResizeNesw => CursorIcon::NeswResize,
        Cursor::ResizeNwse => CursorIcon::NwseResize,
        Cursor::Help => CursorIcon::Help,
    }
}

impl View for WebView {
    fn plugin(&self) -> &'static str {
        ID
    }

    fn title(&self) -> Option<String> {
        let s = &self.state;
        let t = if !s.title.is_empty() {
            &s.title
        } else {
            &s.url
        };
        Some(if t.is_empty() {
            "Web".to_string()
        } else {
            t.clone()
        })
    }

    fn paint(&mut self, cx: &mut PaintCx, rect: Rect) {
        self.slot.borrow_mut().cfg = cx.cfg.web.clone();
        let scale = cx.ui.scale;
        let ch = cx.ui.cell_h;
        let pad = 8.0 * scale;
        let header_h = ch + 10.0 * scale;
        let header = Rect {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: header_h,
        };
        let body = Rect {
            x: rect.x,
            y: rect.y + header_h,
            w: rect.w,
            h: (rect.h - header_h).max(1.0),
        };
        self.body = body;
        cx.fill(header, cx.chrome.sidebar);

        // The page's size follows the body (created there on first paint).
        let size = (body.w.round() as u32, body.h.round() as u32, scale);
        if self.sent_size != Some(size) {
            let created = self.created_in != 0 && self.created_in == self.slot.borrow().generation;
            self.sent_size = Some(size);
            if created {
                self.send(Some(HostRequest::Resize {
                    id: self.page,
                    width: size.0,
                    height: size.1,
                    scale,
                }));
            }
        }
        let alive = self.send(None);
        // Reload also retries a host that couldn't start.
        let reloadable = alive || self.slot.borrow().error.is_some();
        self.refresh_state();
        self.sync_focus(cx.focused);

        // Back / forward / reload.
        let hy = rect.y + (header_h - ch) * 0.5;
        let btn_w = cx.ui.cell_w * 3.0;
        let mut x = rect.x + pad * 0.5;
        for (glyph, enabled, id, tip) in [
            ("\u{2190}", self.state.can_back, BTN_BACK, "Back"),
            ("\u{2192}", self.state.can_forward, BTN_FORWARD, "Forward"),
            ("\u{21bb}", reloadable, BTN_RELOAD, "Reload"),
        ] {
            let hit = Rect {
                x,
                y: rect.y,
                w: btn_w,
                h: header_h,
            };
            let hov = enabled && cx.button(hit, id);
            if hov {
                cx.highlight(hit);
            }
            cx.tooltip(hit, tip);
            let color = match (enabled, hov) {
                (false, _) => theme::DIM,
                (true, true) => theme::GLYPH_HOVER,
                (true, false) => theme::GLYPH,
            };
            let shaped = cx.shape(glyph, Family::Monospace, btn_w);
            cx.place(shaped, x + (btn_w - shaped.width) * 0.5, hy, hit, color);
            x += btn_w;
        }

        // URL bar: the address, or the text being edited.
        let inset = 4.0 * scale;
        let box_x = x + pad * 0.5;
        let url_box = Rect {
            x: box_x,
            y: rect.y + inset,
            w: (rect.x + rect.w - pad - box_x).max(1.0),
            h: header_h - inset * 2.0,
        };
        self.url_box = url_box;
        match &self.url_edit {
            Some(e) => widgets::text_field(cx, url_box, e, "", cx.focused),
            None => {
                let shown = LineEdit::new(self.state.url.clone());
                widgets::text_field(cx, url_box, &shown, "Enter an address", false);
            }
        }
        cx.cursor(url_box, CursorIcon::Text);
        if self.state.loading {
            cx.fill(
                Rect {
                    x: rect.x,
                    y: rect.y + header_h - 2.0 * scale,
                    w: rect.w,
                    h: 2.0 * scale,
                },
                cx.chrome.accent,
            );
        }

        // The page.
        let error = self.slot.borrow().error.clone();
        match error {
            Some(err) => {
                cx.fill(body, cx.chrome.background);
                let s = cx.shape(&err, Family::SansSerif, (body.w - pad * 2.0).max(1.0));
                let left = body.x + ((body.w - s.width) * 0.5).max(pad);
                cx.place(s, left, body.y + body.h * 0.4, body, theme::DIM);
            }
            None => {
                cx.fill(body, PAGE_BG);
                let frame = self
                    .slot
                    .borrow_mut()
                    .host
                    .as_mut()
                    .and_then(|h| h.frame(self.page));
                if let Some(f) = frame {
                    let version = f.version();
                    let rect = Rect { x: body.x, y: body.y, w: f.width as f32, h: f.height as f32 };
                    let image = PageImage {
                        frame: f,
                        pixels: Default::default(),
                    };
                    cx.image(0, version, Arc::new(image), rect, body);
                }
                cx.cursor(body, cursor_icon(self.state.cursor));
            }
        }
    }

    fn raw_key(&mut self, cx: &mut EventCx, key: &RawKey) -> bool {
        let pressed = key.event.state == winit::event::ElementState::Pressed;
        if self.url_edit.is_some() {
            return match (&key.press, pressed) {
                (Some(press), true) => self.url_key(cx, press),
                _ => true,
            };
        }
        let flags = keys::modifier_flags(cx.mods);
        let base = keys::base_char(key.event);
        if cx.mods.super_ {
            // Cmd chords are commands (the page gets no raw Cmd keys).
            return match base {
                Some(c) if pressed => {
                    let used = self.page_chord(c, cx.mods.shift);
                    if used {
                        cx.redraw();
                    }
                    used
                }
                _ => false,
            };
        }
        for k in keys::translate(key.event, base, flags) {
            self.send(Some(HostRequest::Key {
                id: self.page,
                key: k,
            }));
        }
        true
    }

    fn key(&mut self, cx: &mut EventCx, press: &KeyPress) -> bool {
        self.url_key(cx, press)
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        let flags = keys::modifier_flags(cx.mods);
        match *event {
            MouseEvent::Down { pos, clicks } => {
                if rect_contains(self.url_box, pos.0, pos.1) {
                    if self.url_edit.is_none() {
                        self.edit_url();
                    }
                    cx.redraw();
                    return;
                }
                if !rect_contains(self.body, pos.0, pos.1) {
                    return;
                }
                if self.url_edit.take().is_some() {
                    cx.redraw();
                }
                self.sync_focus(true);
                self.pressed = Some(clicks);
                let action = MouseAction::Down {
                    button: MouseButton::Left,
                    count: clicks,
                };
                self.mouse_to_page(pos, mods::LEFT_MOUSE, flags, action);
            }
            MouseEvent::Drag { pos } => {
                if self.pressed.is_some() {
                    self.mouse_to_page(pos, mods::LEFT_MOUSE, flags, MouseAction::Move);
                }
            }
            MouseEvent::Up { pos, .. } => {
                if let Some(count) = self.pressed.take() {
                    let action = MouseAction::Up {
                        button: MouseButton::Left,
                        count,
                    };
                    self.mouse_to_page(pos, 0, flags, action);
                }
            }
            MouseEvent::Move { pos } => {
                let inside = rect_contains(self.body, pos.0, pos.1);
                if inside {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Move);
                } else if self.hovering {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Leave);
                }
                self.hovering = inside;
            }
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), dx: f32, dy: f32) {
        if !rect_contains(self.body, pos.0, pos.1) {
            return;
        }
        let action = MouseAction::Wheel {
            dx: dx.round() as i32,
            dy: dy.round() as i32,
        };
        self.mouse_to_page(pos, 0, keys::modifier_flags(cx.mods), action);
    }

    fn button(&mut self, cx: &mut EventCx, id: u32) {
        let _ = match id {
            BTN_BACK => self.back(),
            BTN_FORWARD => self.forward(),
            BTN_RELOAD => self.reload(),
            _ => Ok(()),
        };
        cx.redraw();
    }

    fn focus_changed(&mut self, _cx: &mut EventCx, focused: bool) {
        if !focused {
            self.url_edit = None;
        }
        self.sync_focus(focused);
    }

    fn shown(&mut self, visible: bool) {
        if visible != self.sent_visible
            && self.created_in != 0
            && self.send(Some(HostRequest::Visible {
                id: self.page,
                visible,
            }))
        {
            self.sent_visible = visible;
        }
    }

    fn refresh(&mut self) -> bool {
        self.refresh_state()
    }

    /// While the host is backing off after an exit: when to try again.
    fn deadline(&self, _cfg: &ghostrealm_core::Config) -> Option<Instant> {
        self.slot.borrow().retry_at()
    }

    fn tick(&mut self, cx: &mut EventCx, now: Instant) {
        if self.slot.borrow().retry_at().is_some_and(|t| now >= t) {
            self.send(None);
            cx.redraw();
        }
    }

    /// The page's address, title, and visible text, once it has loaded.
    fn text(&mut self) -> Option<String> {
        let read = self.with_host(|h, id| {
            h.wait_loaded(id, LOAD_WAIT);
            // Read the state after the text: the host answers in event order, so
            // updates that trail a load (a restored page's title) have landed.
            let text = h.text(id, REPLY_WAIT).unwrap_or_default();
            (h.view(id).unwrap_or_default(), text)
        });
        let Some((v, text)) = read else {
            return Some(format!("error: {}\n", self.no_host()));
        };
        let mut out = format!(
            "url: {}\ntitle: {}\nloading: {}\n",
            v.url, v.title, v.loading
        );
        if let Some(err) = &v.error {
            out.push_str(&format!("error: {err}\n"));
        }
        out.push('\n');
        out.push_str(&text);
        out.push('\n');
        Some(out)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::{cmd, press, Harness};

    /// A view whose host can't be found (tests don't bundle one).
    fn hostless() -> WebView {
        std::env::set_var(
            ghostrealm_web::client::HOST_ENV,
            "/nonexistent/ghostrealm-web-host",
        );
        let plugin = WebPlugin::new();
        let view = plugin.open(&OpenCx::default()).unwrap();
        *(view as Box<dyn Any>).downcast::<WebView>().unwrap()
    }

    /// A view whose host is `script` (a shell script standing in for the real
    /// one), and the file the script appends a line to each time it starts.
    fn stub_host(tag: &str, script: &str) -> (WebView, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::plugin::testing::tmpdir(tag);
        let exe = dir.join("host.sh");
        let runs = dir.join("runs");
        std::fs::write(
            &exe,
            format!("#!/bin/sh\necho run >> '{}'\n{script}\n", runs.display()),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plugin = WebPlugin::new();
        plugin.slot.borrow_mut().exe = Some(exe);
        let view = plugin.open(&OpenCx::default()).unwrap();
        (*(view as Box<dyn Any>).downcast::<WebView>().unwrap(), runs)
    }

    fn runs(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |s| s.lines().count())
    }

    /// Wait for the stub host to exit.
    fn wait_exit(v: &WebView) {
        let until = Instant::now() + Duration::from_secs(10);
        while v.slot.borrow().host.as_ref().is_some_and(|h| h.is_alive()) {
            assert!(Instant::now() < until, "stub host never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_host_that_dies_is_not_respawned_until_reload_or_the_backoff() {
        let (mut v, runs_file) = stub_host(
            "web-respawn",
            r#"echo '{"ev":"failed","error":"the web profile is in use"}'; exit 1"#,
        );
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        wait_exit(&v);
        for _ in 0..5 {
            h.paint(&mut v, RECT);
            h.mouse(&mut v, MouseEvent::Move { pos: (50.0, 200.0) });
        }
        assert_eq!(runs(&runs_file), 1, "painting and input don't respawn");
        assert_eq!(
            v.slot.borrow().error.as_deref(),
            Some("the web profile is in use"),
            "the host's reason is shown"
        );
        let retry = v.deadline(&Default::default()).expect("a retry is scheduled");
        assert!(retry > Instant::now());

        // Reload retries at once; a second failure backs off longer.
        let _ = v.reload();
        wait_exit(&v);
        h.paint(&mut v, RECT);
        assert_eq!(runs(&runs_file), 2, "Reload respawns");
        let next = v.deadline(&Default::default()).expect("still backing off");
        assert!(next - Instant::now() > RETRY_FIRST, "backs off further");
    }

    #[test]
    fn a_host_that_exits_before_ready_keeps_its_error_shown() {
        let (mut v, _) = stub_host("web-early-exit", "exit 0");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        wait_exit(&v);
        h.paint(&mut v, RECT);
        assert_eq!(
            v.slot.borrow().error.as_deref(),
            Some("the web host exited while starting")
        );
        assert!(h.frame.images.is_empty());
    }

    const RECT: Rect = Rect {
        x: 0.0,
        y: 0.0,
        w: 400.0,
        h: 300.0,
    };

    #[test]
    fn a_missing_host_is_explained_in_the_page() {
        let mut v = hostless();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(
            v.slot.borrow().error.is_some(),
            "spawning a missing host reports an error"
        );
        assert!(h.frame.images.is_empty(), "no page image without a host");
        assert_eq!(v.title().as_deref(), Some("Web"));
        assert!(v.text().unwrap().starts_with("error:"));
    }

    #[test]
    fn toolbar_registers_reload_to_retry_but_not_disabled_history_buttons() {
        let mut v = hostless();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let ids: Vec<u32> = h.frame.view_buttons.iter().map(|(_, _, id)| *id).collect();
        assert!(
            !ids.contains(&BTN_BACK) && !ids.contains(&BTN_FORWARD),
            "{ids:?}"
        );
        assert!(
            ids.contains(&BTN_RELOAD),
            "reload retries a host that couldn't start: {ids:?}"
        );
    }

    #[test]
    fn clicking_the_url_bar_edits_it_and_escape_cancels() {
        let mut v = hostless();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let b = v.url_box;
        h.mouse(
            &mut v,
            MouseEvent::Down {
                pos: (b.x + 4.0, b.y + 4.0),
                clicks: 1,
            },
        );
        assert!(
            v.url_edit.is_some(),
            "a click in the URL bar starts editing"
        );
        for c in "rust".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        assert_eq!(v.url_edit.as_ref().unwrap().text(), "rust");
        h.key(&mut v, cmd(Key::Char('a')));
        assert!(
            v.url_edit.as_ref().unwrap().selection().is_some(),
            "Cmd+A selects the field"
        );
        h.key(&mut v, press(Key::Escape));
        assert!(v.url_edit.is_none(), "Escape cancels editing");
    }

    #[test]
    fn cmd_l_edits_the_url_and_reselects_what_was_typed() {
        let mut v = hostless();
        assert!(v.page_chord('l', false), "Cmd+L without a binding still edits the URL");
        assert!(v.url_edit.is_some());
        let h = Harness::new();
        for c in "rust".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        // The binding (web.focus_url) and the fallback both keep the typing.
        v.edit_url();
        h.key(&mut v, cmd(Key::Char('l')));
        let e = v.url_edit.as_ref().unwrap();
        assert_eq!(e.text(), "rust");
        assert!(e.selection().is_some(), "selected for retyping");
    }

    #[test]
    fn web_new_reports_a_host_that_cant_start() {
        let _ = hostless();
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let r = crate::app_state::build_registry().execute("web.new", &Args::new(), &mut s);
        let Err(CmdError::Failed(msg)) = r else {
            panic!("web.new succeeded without a host");
        };
        assert!(msg.contains("opened web surface"), "{msg}");
    }

    #[test]
    fn enter_in_the_url_bar_stops_editing() {
        let mut v = hostless();
        v.edit_url();
        for c in "example.com".chars() {
            Harness::new().key(&mut v, press(Key::Char(c)));
        }
        Harness::new().key(&mut v, press(Key::Enter));
        assert!(v.url_edit.is_none());
    }
}
