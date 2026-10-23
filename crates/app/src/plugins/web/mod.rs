//! The web plugin: one web page per view (the pane system provides tabs), with
//! a back / forward / reload toolbar and an editable URL bar.
//!
//! A pop-up is a page of its own, so it keeps `window.opener`. One a person
//! opened (a press or key reached the page) as a pop-up window
//! (`window.open` with a size) floats over its opener's page with a close
//! button, the opener still live beside it; as a tab (`target=_blank`) it
//! opens as a tab beside its opener. One an agent opened (through a gesture
//! eval's activation) stays hidden: the app keeps it as a hidden surface the
//! agent drives. The app takes tabs and hidden ones from
//! [`WebView::take_opened`]. Each goes when it closes itself.
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
    Cursor, EditAction, MouseAction, MouseButton as PageButton, PopupInfo, PopupKind, Request as HostRequest,
    SharedFrame, ViewState, WebHost,
};
use glyphon::Family;
use winit::window::CursorIcon;

use crate::app_state::AppState;
use crate::plugin::icons::Icon;
use crate::plugin::text_edit::{Edit, LineEdit};
use crate::plugin::{
    rect_contains, theme, widgets, widgets::FieldMouse, EventCx, ImageSource, MouseButton, MouseEvent, OpenCx, PaintCx,
    Plugin,
    RawKey, View, Waker,
};

pub use url::normalize_url;

pub const ID: &str = "web";

/// Toolbar button ids (see [`View::button`]).
const BTN_BACK: u32 = 0;
const BTN_FORWARD: u32 = 1;
const BTN_RELOAD: u32 = 2;
/// A pop-up's close button.
const BTN_CLOSE: u32 = 3;
/// Id base of the floating pop-up's buttons and image.
const POPUP_BASE: u32 = 1000;
/// Space kept between a floating pop-up and its opener's page edges, in
/// logical pixels.
const POPUP_MARGIN: f32 = 24.0;

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
    /// The host bundle speaks another protocol version: it is not respawned
    /// until Reload, `web.new`, or a change to the bundle (its mtime differs
    /// from the one recorded here).
    refused: Option<Option<std::time::SystemTime>>,
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
            // A host that failed is exiting, or can't be spoken to (another
            // protocol version): drop it, which asks it to exit.
            if !h.is_alive() || h.failure().is_some() {
                let mismatch = h.protocol_mismatch();
                let error = match h.failure() {
                    Some(e) => e,
                    None if ready => "the web host exited".into(),
                    None => "the web host exited while starting".into(),
                };
                self.host = None;
                if mismatch {
                    // Another spawn of the same bundle would be refused again.
                    self.error = Some(error);
                    self.retry_at = None;
                    self.refused = Some(self.find_host().and_then(|p| bundle_stamp(&p)));
                } else {
                    self.failed(error);
                }
            } else if ready {
                self.failures = 0;
                self.error = None;
            }
        }
        if let Some(stamp) = self.refused {
            if self.find_host().and_then(|p| bundle_stamp(&p)) != stamp {
                self.refused = None;
            }
        }
        if self.host.is_none()
            && self.refused.is_none()
            && self.retry_at.is_none_or(|t| Instant::now() >= t)
        {
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
        if let Some(exe) = self.exe.clone().or_else(|| tests::TEST_HOST.with_borrow(Clone::clone)) {
            return Some(exe);
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
        self.refused = None;
    }

    /// When a host can next be spawned, while there is none.
    fn retry_at(&self) -> Option<Instant> {
        self.retry_at.filter(|_| self.host.is_none())
    }
}

/// When the host bundle at `exe` last changed.
fn bundle_stamp(exe: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(exe).and_then(|m| m.modified()).ok()
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

    /// What hands the app the tabs a person opened whose opener went before
    /// the app placed them: no view is left to hand them over.
    pub fn orphan_tabs(&self) -> OrphanTabs {
        OrphanTabs(self.slot.clone())
    }
}

/// See [`WebPlugin::orphan_tabs`].
pub struct OrphanTabs(Rc<RefCell<HostSlot>>);

impl OrphanTabs {
    /// Views of those tabs since last asked, oldest first, each with whether
    /// it asked to be shown.
    pub fn take(&self) -> Vec<(WebView, bool)> {
        let tabs = match self.0.borrow().host.as_ref() {
            Some(h) => h.take_orphan_tabs(),
            None => return Vec::new(),
        };
        tabs.iter()
            .map(|p| (WebView::for_popup(self.0.clone(), p, false), p.kind != PopupKind::BackgroundTab))
            .collect()
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
                page.slot.borrow_mut().retry_now();
                if let Err(e) = page.start() {
                    return Err(CmdError::Failed(format!(
                        "opened web surface {}, but {e}",
                        sid.0
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
                "web.close",
                "Web: Close",
                "Close a web surface: a hidden pop-up (see `get tree`), or a tab",
            )
            .arg(id_arg()),
            Box::new(|s: &mut AppState, a| {
                target(s, a)?;
                let sid = match a.get_int("id") {
                    Ok(id) => SurfaceId(id as u64),
                    Err(_) => s.focused_surface().ok_or_else(|| CmdError::Failed("no web surface focused".into()))?,
                };
                s.close_surface_id(sid);
                Ok(CmdOutcome::ok())
            }),
        );
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
                 result. It runs in an isolated world: it sees the page's DOM but not its \
                 scripts' globals, and the page's scripts can't tamper with how it runs or how \
                 its value is encoded. What it reads from the DOM is still page content",
            )
            .arg(ArgSpec::required(
                "js",
                ArgKind::Str,
                "expression or statements; a promise is awaited",
            ))
            .arg(ArgSpec::optional(
                "gesture",
                ArgKind::Bool,
                "run as a user gesture, to do what a click can (open a pop-up). It hands the \
                 page a user activation its own scripts can use too (write the clipboard, open \
                 pop-ups) and can see. Default false",
            ))
            .arg(id_arg())
            .hidden(),
            Box::new(|s: &mut AppState, a| {
                let js = a.get_str("js")?.to_string();
                let gesture = a.get_bool("gesture").unwrap_or(false);
                target(s, a)?
                    .eval(&js, gesture)
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
    /// The view is on screen (painted, and not since hidden). A page starts
    /// hidden, so one opened in the background doesn't render.
    on_screen: bool,
    sent_visible: bool,
    /// The URL bar while it is being edited.
    url_edit: Option<LineEdit>,
    /// The mouse in the URL bar while it is being edited.
    url_field: FieldMouse,
    /// Page state as of the last [`View::refresh`].
    state: ViewState,
    /// Layout from the last paint, for mouse hit-testing.
    url_box: Rect,
    body: Rect,
    /// A press on the page is held (drags go to the page): its button and
    /// click count.
    pressed: Option<(PageButton, u32)>,
    hovering: bool,
    /// The page was made by the host (a pop-up): it is never created again,
    /// so it goes with the host.
    host_made: bool,
    /// Its header has a close button instead of navigation (a floating or
    /// hidden pop-up).
    popup: bool,
    /// The size a pop-up asked for (physical pixels), for floating it.
    popup_size: (u32, u32),
    /// Its close button was pressed.
    dismissed: bool,
    /// Pop-ups a person opened, floating over the page; the last shows.
    floating: Vec<WebView>,
    /// Where the shown floating pop-up was painted.
    float_rect: Rect,
    /// The floating pop-up has the keyboard (else the page has it).
    float_keys: bool,
    /// The press held is the floating pop-up's.
    float_press: bool,
    /// The pointer is over the floating pop-up.
    float_hover: bool,
    /// Whether the view has the keyboard, as last told.
    focused: bool,
    /// Pop-ups to open as tabs or hidden surfaces, until the app takes them.
    opened: Vec<Opened>,
}

/// A pop-up the app opens as a surface of its own.
pub struct Opened {
    pub view: WebView,
    pub place: Place,
    /// A pop-up floating over the page opened it, not the page itself.
    pub from_floating: bool,
}

/// Where an [`Opened`] pop-up goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A hidden surface: an agent opened it.
    Hidden,
    /// A tab beside its opener's, shown when `focus`: a person opened it.
    Tab { focus: bool },
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
            on_screen: false,
            sent_visible: true,
            url_edit: None,
            url_field: FieldMouse::default(),
            state: ViewState::default(),
            url_box: zero,
            body: zero,
            pressed: None,
            hovering: false,
            host_made: false,
            popup: false,
            popup_size: DEFAULT_SIZE,
            dismissed: false,
            floating: Vec::new(),
            float_rect: zero,
            float_keys: false,
            float_press: false,
            float_hover: false,
            focused: false,
            opened: Vec::new(),
        }
    }

    /// A view of pop-up `p`, which the host made (and keeps hidden) in the
    /// running host. A `popup` one has a close button for a header.
    fn for_popup(slot: Rc<RefCell<HostSlot>>, p: &PopupInfo, popup: bool) -> Self {
        let generation = slot.borrow().generation;
        let mut v = WebView::new(slot, p.id);
        v.created_in = generation;
        v.sent_visible = false;
        v.last_url.clone_from(&p.url);
        v.host_made = true;
        v.popup = popup;
        v.popup_size = (p.width, p.height);
        v
    }

    /// The page is gone: it closed itself, or a pop-up closed (by its close
    /// button or its host exiting). A page the app creates outlives its
    /// host: it is made again in the next.
    pub fn is_closed(&self) -> bool {
        let slot = self.slot.borrow();
        let current = slot.generation == self.created_in;
        if !self.host_made {
            return current && slot.host.as_ref().is_some_and(|h| h.is_closed(self.page));
        }
        self.dismissed || !current || slot.host.as_ref().is_none_or(|h| h.is_closed(self.page))
    }

    /// This pop-up tab's page, as opened by `opener`, for another view to
    /// show: this view no longer closes it.
    fn release(mut self, opener: u64, focus: bool) -> PopupInfo {
        self.created_in = 0;
        PopupInfo {
            id: self.page,
            opener,
            url: self.last_url.clone(),
            from_input: true,
            kind: if focus { PopupKind::Tab } else { PopupKind::BackgroundTab },
            width: self.popup_size.0,
            height: self.popup_size.1,
        }
    }

    /// Take the pop-ups the page opened from the host: a person's pop-up
    /// window floats over it (and takes the keyboard); a person's tab and an
    /// agent's pop-up wait in `opened`. Drop floating ones that closed.
    /// Returns whether the floating ones changed.
    fn pull_popups(&mut self) -> bool {
        let popups = {
            let slot = self.slot.borrow();
            match slot.host.as_ref() {
                // A page made in an older host is not this host's page.
                Some(h) if self.created_in == slot.generation => h.take_popups(self.page),
                _ => Vec::new(),
            }
        };
        let mut changed = false;
        for p in &popups {
            let place = match (p.from_input, p.kind) {
                (false, _) => Place::Hidden,
                (true, PopupKind::Popup) => {
                    self.floating.push(WebView::for_popup(self.slot.clone(), p, true));
                    self.float_keys = true;
                    changed = true;
                    continue;
                }
                (true, PopupKind::BackgroundTab) => Place::Tab { focus: false },
                (true, PopupKind::Tab | PopupKind::Window) => Place::Tab { focus: true },
            };
            let view = WebView::for_popup(self.slot.clone(), p, place == Place::Hidden);
            self.opened.push(Opened { view, place, from_floating: false });
        }
        for f in &mut self.floating {
            changed |= f.pull_popups();
            self.opened.extend(f.opened.drain(..).map(|o| Opened { from_floating: true, ..o }));
        }
        if self.drop_closed_floating() {
            return true;
        }
        if changed {
            let focused = self.focused;
            self.sync_focus(focused);
        }
        changed
    }

    /// The pop-ups this page (or a pop-up floating over it) opened as tabs or
    /// hidden surfaces since last asked, for the app to place.
    pub fn take_opened(&mut self) -> Vec<Opened> {
        self.pull_popups();
        std::mem::take(&mut self.opened)
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
        if self.created_in != generation && self.host_made {
            // It went with the host that made it.
            return false;
        }
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
            // Pages are created visible.
            self.sent_visible = true;
            if !self.on_screen
                && host.send(&HostRequest::Visible {
                    id: self.page,
                    visible: false,
                })
            {
                self.sent_visible = false;
            }
        }
        match req {
            Some(r) => host.send(&r),
            None => true,
        }
    }

    /// Create the page in the host. Without a waker (the agent channel) no
    /// repaint would show a failure that comes later, so wait for the host to
    /// be ready and return why it isn't; with one (the GUI) return at once and
    /// let the pane show it.
    fn start(&mut self) -> Result<(), String> {
        if !self.send(None) {
            return Err(self.no_host());
        }
        if self.slot.borrow().waker.is_some() {
            return Ok(());
        }
        if self.with_host(|h, _| h.wait_ready(LOAD_WAIT)) == Some(true) {
            return Ok(());
        }
        // Picks up the reason of a host that exited.
        if self.slot.borrow_mut().host().is_some() {
            return Err(format!("the web host was not ready within {}s", LOAD_WAIT.as_secs()));
        }
        Err(self.no_host())
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
        if self.popup {
            return;
        }
        // Typing goes to the bar, not a floating pop-up.
        self.keys_to_float(false);
        let e = self
            .url_edit
            .get_or_insert_with(|| LineEdit::new(self.state.url.clone()).with_plain_brackets());
        let _ = e.apply(Edit::SelectAll, &mut None::<String>);
        self.sync_focus(true);
    }

    /// Evaluate `js` once the page has loaded, as a user gesture when
    /// `gesture`; `Ok` is the JSON-encoded value.
    pub fn eval(&mut self, js: &str, gesture: bool) -> Result<String, String> {
        match self.with_host(|h, id| {
            h.wait_loaded(id, LOAD_WAIT);
            h.eval_as(id, js, gesture, REPLY_WAIT)
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

    /// Tell the host whether the page is on screen.
    fn sync_visible(&mut self) {
        let visible = self.on_screen;
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

    /// Tell the host whether the page has keyboard focus (not while the URL bar
    /// is being edited).
    fn sync_focus(&mut self, view_focused: bool) {
        self.focused = view_focused;
        let to_float = self.float_keys && !self.floating.is_empty();
        let top = self.floating.len().saturating_sub(1);
        for (i, f) in self.floating.iter_mut().enumerate() {
            f.sync_focus(view_focused && to_float && i == top);
        }
        let want = view_focused && self.url_edit.is_none() && !to_float;
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

impl WebView {
    /// Float the newest pop-up a person opened over the page, centred in
    /// `body`; the ones under it stop rendering.
    fn paint_floating(&mut self, cx: &mut PaintCx, body: Rect) {
        let n = self.floating.len();
        for f in self.floating.iter_mut().take(n.saturating_sub(1)) {
            f.shown(false);
        }
        let Some(top) = self.floating.last_mut() else {
            self.float_rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
            return;
        };
        let header_h = cx.ui.cell_h + 10.0 * cx.ui.scale;
        let r = float_rect(body, top.popup_size, header_h, POPUP_MARGIN * cx.ui.scale);
        self.float_rect = r;
        let keys = cx.focused && self.float_keys;
        cx.above(|cx| {
            let edge = theme::hairline(cx.ui);
            let shadow = 6.0 * cx.ui.scale;
            cx.fill_alpha(
                Rect { x: r.x - shadow, y: r.y - shadow * 0.5, w: r.w + shadow * 2.0, h: r.h + shadow * 2.0 },
                [0, 0, 0],
                0.3,
            );
            cx.fill(Rect { x: r.x - edge, y: r.y - edge, w: r.w + edge * 2.0, h: r.h + edge * 2.0 }, theme::INPUT_BORDER);
            let focused = std::mem::replace(&mut cx.focused, keys);
            cx.with_id_base(POPUP_BASE, |cx| top.paint(cx, r));
            cx.focused = focused;
        });
    }

    /// Route `event` to the floating pop-up when it is over it or holds the
    /// press. A press on it gives it the keyboard; a press on the page takes
    /// the keyboard back. Returns whether the pop-up took the event.
    fn float_mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) -> bool {
        if self.floating.is_empty() {
            return false;
        }
        let flags = keys::modifier_flags(cx.mods);
        let to_float = match *event {
            MouseEvent::Down { pos, .. } => {
                let on = self.float_at(pos);
                self.float_press = on;
                self.keys_to_float(on);
                if on && std::mem::take(&mut self.hovering) {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Leave);
                }
                on
            }
            MouseEvent::Drag { .. } => self.float_press,
            MouseEvent::Up { .. } => std::mem::take(&mut self.float_press),
            MouseEvent::Move { pos } => {
                let on = self.float_at(pos);
                if on && std::mem::take(&mut self.hovering) {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Leave);
                }
                if !on && std::mem::take(&mut self.float_hover) {
                    if let Some(f) = self.floating.last_mut() {
                        f.mouse(cx, &MouseEvent::Leave { pos });
                    }
                }
                self.float_hover = on;
                on
            }
            MouseEvent::Leave { .. } => {
                let held = std::mem::take(&mut self.float_hover);
                if held {
                    if let Some(f) = self.floating.last_mut() {
                        f.mouse(cx, event);
                    }
                }
                // The page hears it too.
                false
            }
        };
        if to_float {
            if let Some(f) = self.floating.last_mut() {
                f.mouse(cx, event);
            }
            cx.redraw();
        }
        to_float
    }

    /// The floating pop-up shown, if `pos` is over it.
    fn float_at(&self, pos: (f32, f32)) -> bool {
        !self.floating.is_empty() && rect_contains(self.float_rect, pos.0, pos.1)
    }

    /// Close the floating pop-up shown if it has the keyboard (Close, Cmd+W,
    /// acts on it rather than the tab). Returns whether it did.
    pub fn dismiss_floating(&mut self) -> bool {
        if !self.float_keys {
            return false;
        }
        let Some(top) = self.floating.last_mut() else {
            return false;
        };
        top.dismissed = true;
        self.drop_closed_floating()
    }

    /// Give the keyboard to the floating pop-up (`true`) or the page.
    fn keys_to_float(&mut self, to_float: bool) {
        if self.float_keys != to_float {
            self.float_keys = to_float;
            let focused = self.focused;
            self.sync_focus(focused);
        }
    }

    /// Drop floating pop-ups that were dismissed or closed; the page gets
    /// the keyboard back once none floats.
    fn drop_closed_floating(&mut self) -> bool {
        let before = self.floating.len();
        self.floating.retain(|f| !f.is_closed());
        if self.floating.len() == before {
            return false;
        }
        // The keyboard stays with the floating pop-ups while one is left.
        self.float_keys &= !self.floating.is_empty();
        self.float_press = false;
        self.float_hover = false;
        let focused = self.focused;
        self.sync_focus(focused);
        true
    }
}

/// Where a pop-up of page size `want` (physical pixels) with a header
/// `header_h` tall floats: centred in `body`, at most `margin` from its edges.
fn float_rect(body: Rect, want: (u32, u32), header_h: f32, margin: f32) -> Rect {
    let w = (want.0 as f32).min(body.w - margin * 2.0).max(1.0).round();
    let h = (want.1 as f32 + header_h).min(body.h - margin * 2.0).max(1.0).round();
    Rect {
        x: (body.x + (body.w - w) * 0.5).round(),
        y: (body.y + (body.h - h) * 0.5).round(),
        w,
        h,
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
                    // A person's tab taken but not yet placed outlives this
                    // page: the app places it. The rest close with it, once
                    // the slot is free.
                    let (tabs, rest): (Vec<Opened>, Vec<Opened>) =
                        std::mem::take(&mut self.opened).into_iter().partition(|o| matches!(o.place, Place::Tab { .. }));
                    self.opened = rest;
                    for o in tabs {
                        let Place::Tab { focus } = o.place else { continue };
                        h.orphan_tab(o.view.release(self.page, focus));
                    }
                    h.send(&HostRequest::Close { id: self.page });
                    h.forget(self.page);
                }
            }
        }
    }
}

fn page_button(b: MouseButton) -> PageButton {
    match b {
        MouseButton::Left => PageButton::Left,
        MouseButton::Middle => PageButton::Middle,
        MouseButton::Right => PageButton::Right,
    }
}

/// The modifier flag telling the page `b` is held.
fn held_flag(b: PageButton) -> u32 {
    match b {
        PageButton::Left => mods::LEFT_MOUSE,
        PageButton::Middle => mods::MIDDLE_MOUSE,
        PageButton::Right => mods::RIGHT_MOUSE,
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
        self.on_screen = true;
        let alive = self.send(None);
        self.sync_visible();
        // Reload also retries a host that couldn't start.
        let reloadable = alive || self.slot.borrow().error.is_some();
        self.refresh_state();
        self.pull_popups();
        self.sync_focus(cx.focused);

        let hy = rect.y + (header_h - ch) * 0.5;
        let btn_w = cx.ui.cell_w * 3.0;
        let mut x = rect.x + pad * 0.5;
        let mut right = rect.x + rect.w - pad;
        if self.popup {
            // A pop-up closes; it doesn't navigate.
            let hit = Rect {
                x: rect.x + rect.w - btn_w,
                y: rect.y,
                w: btn_w,
                h: header_h,
            };
            let hov = cx.button(hit, BTN_CLOSE);
            if hov {
                cx.highlight(hit);
            }
            cx.tooltip(hit, "Close");
            let icon = Rect {
                x: hit.x + (btn_w - ch) * 0.5,
                y: hy,
                w: ch,
                h: ch,
            };
            Icon::Close.paint(cx, icon, if hov { theme::GLYPH_HOVER } else { theme::GLYPH });
            right = hit.x;
        } else {
            // Back / forward / reload.
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
            x += pad * 0.5;
        }

        // URL bar: the address, or the text being edited (a pop-up's only
        // shows where it is).
        let inset = 4.0 * scale;
        let url_box = Rect {
            x,
            y: rect.y + inset,
            w: (right - x).max(1.0),
            h: header_h - inset * 2.0,
        };
        self.url_box = url_box;
        match &self.url_edit {
            Some(e) => {
                let geom = widgets::text_field(cx, url_box, e, "", cx.focused);
                self.url_field.painted(cx, geom);
            }
            None if self.popup => {
                let shown = LineEdit::new(self.state.url.clone());
                widgets::text_field(cx, url_box, &shown, "", false);
                self.url_field.clear();
            }
            None => {
                let shown = LineEdit::new(self.state.url.clone());
                let geom = widgets::text_field(cx, url_box, &shown, "Enter an address", false);
                // A click starts editing, so it takes the mouse already; the
                // geometry maps a drag in that press to the address.
                self.url_field.painted(cx, geom);
            }
        }
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
        let error = self.slot.borrow().error.clone().or_else(|| {
            self.state.gone.as_ref().map(|gone| format!("{gone}. Reload to restart it."))
        });
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
        self.paint_floating(cx, body);
    }

    fn raw_key(&mut self, cx: &mut EventCx, key: &RawKey) -> bool {
        if self.float_keys {
            if let Some(f) = self.floating.last_mut() {
                return cx.with_id_base(POPUP_BASE, |cx| f.raw_key(cx, key));
            }
        }
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
        if self.float_keys {
            if let Some(f) = self.floating.last_mut() {
                return cx.with_id_base(POPUP_BASE, |cx| f.key(cx, press));
            }
        }
        self.url_key(cx, press)
    }

    fn mouse(&mut self, cx: &mut EventCx, event: &MouseEvent) {
        if self.float_mouse(cx, event) {
            return;
        }
        if let Some(edit) = self.url_edit.as_mut() {
            if self.url_field.event(edit, event, cx.mods.shift) {
                cx.redraw();
                return;
            }
        }
        let flags = keys::modifier_flags(cx.mods);
        match *event {
            MouseEvent::Down { pos, clicks, button } => {
                if rect_contains(self.url_box, pos.0, pos.1) {
                    if button != MouseButton::Left || self.popup {
                        return;
                    }
                    if self.url_edit.is_none() {
                        self.edit_url();
                        self.url_field.hold(pos);
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
                let button = page_button(button);
                self.pressed = Some((button, clicks));
                let action = MouseAction::Down { button, count: clicks };
                self.mouse_to_page(pos, held_flag(button), flags, action);
            }
            MouseEvent::Drag { pos, .. } => {
                if let Some((button, _)) = self.pressed {
                    self.mouse_to_page(pos, held_flag(button), flags, MouseAction::Move);
                }
            }
            MouseEvent::Up { pos, .. } => {
                if let Some((button, count)) = self.pressed.take() {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Up { button, count });
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
            MouseEvent::Leave { pos } => {
                if std::mem::take(&mut self.hovering) {
                    self.mouse_to_page(pos, 0, flags, MouseAction::Leave);
                }
            }
        }
    }

    fn scroll(&mut self, cx: &mut EventCx, pos: (f32, f32), dx: f32, dy: f32) {
        if self.float_at(pos) {
            if let Some(f) = self.floating.last_mut() {
                f.scroll(cx, pos, dx, dy);
            }
            return;
        }
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
        if id >= POPUP_BASE {
            if let Some(f) = self.floating.last_mut() {
                cx.with_id_base(POPUP_BASE, |cx| f.button(cx, id - POPUP_BASE));
            }
            self.drop_closed_floating();
            cx.redraw();
            return;
        }
        let _ = match id {
            BTN_BACK => self.back(),
            BTN_FORWARD => self.forward(),
            BTN_RELOAD => self.reload(),
            BTN_CLOSE => {
                self.dismissed = true;
                Ok(())
            }
            _ => Ok(()),
        };
        cx.redraw();
    }

    fn focus_changed(&mut self, _cx: &mut EventCx, focused: bool) {
        if !focused {
            self.url_edit = None;
            for f in &mut self.floating {
                f.url_edit = None;
            }
        }
        self.sync_focus(focused);
    }

    fn shown(&mut self, visible: bool) {
        self.on_screen = visible;
        self.sync_visible();
        if !visible {
            for f in &mut self.floating {
                f.shown(false);
            }
        }
    }

    fn refresh(&mut self) -> bool {
        let floating = self.floating.iter_mut().fold(false, |c, f| f.refresh_state() | c);
        self.refresh_state() | self.pull_popups() | floating
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
        if let Some(gone) = &v.gone {
            out.push_str(&format!("error: {gone}; reload to restart it\n"));
        }
        if v.popups_refused > 0 {
            let max = ghostrealm_web::protocol::MAX_POPUPS;
            out.push_str(&format!("pop-ups refused: {} (a page keeps at most {max} open)\n", v.popups_refused));
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
pub(super) mod tests {
    use super::*;
    use crate::plugin::testing::{cmd, press, Harness};
    use ghostrealm_core::Value;
    use ghostrealm_web::protocol::{POPUP_IDS, PROTOCOL_VERSION};

    thread_local! {
        /// The host a test's web views run (on its own thread) when no view
        /// names one: for views an `AppState` opens.
        pub(in crate::plugins::web) static TEST_HOST: RefCell<Option<std::path::PathBuf>> = const { RefCell::new(None) };
    }

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

    /// The requests the stub host has read, once `ok` holds for them. The
    /// wait is long: the whole suite (real hosts included) loads the machine.
    fn requests(path: &std::path::Path, ok: impl Fn(&str) -> bool) -> String {
        let until = Instant::now() + Duration::from_secs(20);
        loop {
            let got = std::fs::read_to_string(path).unwrap_or_default();
            if ok(&got) {
                return got;
            }
            assert!(Instant::now() < until, "requests so far: {got}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn starting_from_the_agent_reports_a_host_that_fails_after_spawning() {
        let (mut v, _) = stub_host(
            "web-start-fail",
            r#"echo '{"ev":"failed","error":"the web profile is in use"}'; exit 1"#,
        );
        assert_eq!(v.start(), Err("the web profile is in use".into()));

        let ready = format!(r#"echo '{{"ev":"ready","protocol":{PROTOCOL_VERSION}}}'; cat > /dev/null"#);
        let (mut v, _) = stub_host("web-start-ok", &ready);
        assert_eq!(v.start(), Ok(()));
    }

    #[test]
    fn a_host_bundle_from_another_protocol_version_is_reported() {
        let (mut v, _) = stub_host("web-old-host", r#"echo '{"ev":"ready"}'; cat > /dev/null"#);
        let err = v.start().unwrap_err();
        assert!(err.contains("out of date") && err.contains("mise run web"), "{err}");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        assert!(v.slot.borrow().error.as_deref().is_some_and(|e| e.contains("out of date")));
        assert!(v.slot.borrow().host.is_none(), "the host is let go");
    }

    #[test]
    fn a_protocol_mismatch_is_not_retried_until_reload_or_a_new_bundle() {
        let (mut v, runs_file) =
            stub_host("web-mismatch", r#"echo '{"ev":"ready"}'; cat > /dev/null"#);
        let mut h = Harness::new();
        // Painting spawns the host, which reports the wrong protocol; poll
        // until the slot has let go of it.
        let until = Instant::now() + Duration::from_secs(10);
        while v.slot.borrow().refused.is_none() {
            h.paint(&mut v, RECT);
            assert!(Instant::now() < until, "the mismatch was never noticed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(runs(&runs_file), 1);
        assert!(v.deadline(&Default::default()).is_none(), "no timed retry");
        // Even once any back-off would have expired.
        v.slot.borrow_mut().retry_at = Some(Instant::now());
        for _ in 0..5 {
            h.paint(&mut v, RECT);
            h.tick(&mut v, Instant::now());
        }
        assert_eq!(runs(&runs_file), 1, "not respawned");

        // A rebuilt bundle (new mtime) is tried again.
        let exe = v.slot.borrow().exe.clone().unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(60);
        std::fs::File::options().write(true).open(&exe).unwrap().set_modified(later).unwrap();
        h.paint(&mut v, RECT);
        let until = Instant::now() + Duration::from_secs(10);
        while runs(&runs_file) < 2 {
            assert!(Instant::now() < until, "a changed bundle respawns");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn starting_from_the_gui_does_not_wait_for_the_host() {
        let (mut v, _) = stub_host("web-start-gui", "cat > /dev/null");
        v.slot.borrow_mut().waker = Some(Arc::new(|| {}));
        let t = Instant::now();
        assert_eq!(v.start(), Ok(()));
        assert!(t.elapsed() < Duration::from_secs(1), "waited {:?}", t.elapsed());
    }

    #[test]
    fn a_page_starts_hidden_until_painted() {
        let (mut v, runs) = stub_host("web-hidden", "cat > \"$0.requests\"");
        let log = runs.with_file_name("host.sh.requests");
        // Created without being painted (as `web.new` from the agent does).
        assert!(v.send(None));
        let got = requests(&log, |s| s.contains(r#""op":"visible""#));
        assert!(got.contains(r#""op":"create""#), "{got}");
        assert!(got.contains(r#""visible":false"#), "{got}");
        assert!(!v.sent_visible);

        Harness::new().paint(&mut v, RECT);
        requests(&log, |s| s.contains(r#""visible":true"#));
        v.shown(false);
        requests(&log, |s| s.matches(r#""visible":false"#).count() == 2);
    }

    /// The mouse requests in the stub host's log.
    fn mouse_requests(log: &str) -> Vec<(u32, MouseAction)> {
        log.lines()
            .filter_map(|l| match serde_json::from_str(l) {
                Ok(HostRequest::Mouse { mods, action, .. }) => Some((mods, action)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn right_and_middle_presses_reach_the_page_with_their_button_held() {
        let (mut v, runs) = stub_host("web-buttons", "cat > \"$0.requests\"");
        let log = runs.with_file_name("host.sh.requests");
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let (a, b) = ((v.body.x + 10.0, v.body.y + 10.0), (v.body.x + 30.0, v.body.y + 10.0));
        let url = (v.url_box.x + 4.0, v.url_box.y + 4.0);
        let (right, middle) = (MouseButton::Right, MouseButton::Middle);
        h.mouse(&mut v, MouseEvent::Down { pos: url, clicks: 1, button: right });
        h.mouse(&mut v, MouseEvent::Up { pos: url, dragged: false, button: right });
        assert!(v.url_edit.is_none(), "a right press doesn't edit the URL");
        h.mouse(&mut v, MouseEvent::Down { pos: a, clicks: 2, button: right });
        h.mouse(&mut v, MouseEvent::Drag { pos: b, button: right });
        h.mouse(&mut v, MouseEvent::Up { pos: b, dragged: true, button: right });
        h.mouse(&mut v, MouseEvent::Down { pos: a, clicks: 1, button: middle });
        h.mouse(&mut v, MouseEvent::Up { pos: a, dragged: false, button: middle });

        let got = requests(&log, |s| mouse_requests(s).len() == 5);
        let (r, m) = (PageButton::Right, PageButton::Middle);
        assert_eq!(
            mouse_requests(&got),
            [
                (mods::RIGHT_MOUSE, MouseAction::Down { button: r, count: 2 }),
                (mods::RIGHT_MOUSE, MouseAction::Move),
                (0, MouseAction::Up { button: r, count: 2 }),
                (mods::MIDDLE_MOUSE, MouseAction::Down { button: m, count: 1 }),
                (0, MouseAction::Up { button: m, count: 1 }),
            ]
        );
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


    /// The host's `popup` event for pop-up window `id`, opened by page `opener`.
    fn popup_event(id: u64, opener: u64, from_input: bool) -> String {
        popup_event_of(id, opener, from_input, "popup")
    }

    /// [`popup_event`] of `kind`.
    fn popup_event_of(id: u64, opener: u64, from_input: bool, kind: &str) -> String {
        format!(
            r#"{{"ev":"popup","id":{id},"opener":{opener},"url":"https://pop.test/","from_input":{from_input},"kind":"{kind}","width":200,"height":100}}"#
        )
    }

    /// The host requests in `log`.
    fn host_requests(log: &str) -> Vec<HostRequest> {
        log.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    fn focus(id: u64, focused: bool) -> HostRequest {
        HostRequest::Focus { id, focused }
    }

    #[test]
    fn a_popup_a_person_opened_floats_over_the_page_with_the_keyboard() {
        let script = format!(
            "echo '{{\"ev\":\"ready\",\"protocol\":{PROTOCOL_VERSION}}}'\necho '{}'\ncat > \"$0.requests\"",
            popup_event(POPUP_IDS, 1, true)
        );
        let (mut v, runs) = stub_host("web-float", &script);
        let log = runs.with_file_name("host.sh.requests");
        let mut h = Harness::new();
        let until = Instant::now() + Duration::from_secs(10);
        while v.floating.is_empty() {
            assert!(Instant::now() < until, "the pop-up never floated");
            h.paint(&mut v, RECT);
            std::thread::sleep(Duration::from_millis(10));
        }
        h.paint(&mut v, RECT);
        let r = v.float_rect;
        assert_eq!(r.w, 200.0, "as wide as it asked");
        assert!(r.h > 100.0, "its page and a header");
        let mid = |a: f32, b: f32| (a - b).abs() <= 1.0;
        assert!(mid(r.x + r.w * 0.5, v.body.x + v.body.w * 0.5) && mid(r.y + r.h * 0.5, v.body.y + v.body.h * 0.5));
        let close = POPUP_BASE + BTN_CLOSE;
        assert!(h.frame.view_buttons.iter().any(|(_, _, id)| *id == close), "a close button");
        requests(&log, |s| {
            let got = host_requests(s);
            got.contains(&focus(POPUP_IDS, true)) && got.contains(&focus(1, false))
        });

        // A press on the page takes the keyboard; one on the pop-up gives it back.
        let page = (v.body.x + 2.0, v.body.y + 2.0);
        let popup = (r.x + r.w * 0.5, r.y + r.h - 4.0);
        let left = MouseButton::Left;
        for pos in [page, popup] {
            h.mouse(&mut v, MouseEvent::Down { pos, clicks: 1, button: left });
            h.mouse(&mut v, MouseEvent::Up { pos, dragged: false, button: left });
        }
        // A press sends its focus change before its mouse down: wait for the
        // pop-up's down, the last request.
        let pressed = |got: &[HostRequest], id| {
            got.iter().any(|r| matches!(r, HostRequest::Mouse { id: i, action: MouseAction::Down { .. }, .. } if *i == id))
        };
        let got = requests(&log, |s| pressed(&host_requests(s), POPUP_IDS));
        let got = host_requests(&got);
        assert!(pressed(&got, 1), "each press reached its own page: {got:?}");
        assert_eq!(got.iter().filter(|r| **r == focus(POPUP_IDS, true)).count(), 2, "{got:?}");
        assert!(got.contains(&focus(1, true)));

        // Its close button closes it and gives the page the keyboard back.
        h.button(&mut v, close);
        assert!(v.floating.is_empty());
        requests(&log, |s| {
            let got = host_requests(s);
            let last_focus = got.iter().rev().find(|r| matches!(r, HostRequest::Focus { id: 1, .. }));
            got.contains(&HostRequest::Close { id: POPUP_IDS }) && last_focus == Some(&focus(1, true))
        });
    }

    /// A stub host at `dir/host.sh` running `script` after reporting ready
    /// and `events`; the host for views an `AppState` opens on this thread.
    fn stub_app_host(dir: &std::path::Path, events: &[String], script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join("host.sh");
        let echo: String = events.iter().map(|e| format!("echo '{e}'\n")).collect();
        let ready = format!("echo '{{\"ev\":\"ready\",\"protocol\":{PROTOCOL_VERSION}}}'\n");
        std::fs::write(&exe, format!("#!/bin/sh\n{ready}{echo}{script}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        TEST_HOST.with_borrow_mut(|h| *h = Some(exe));
    }

    #[test]
    fn a_tab_a_person_opened_opens_beside_its_opener() {
        let dir = crate::plugin::testing::tmpdir("web-popup-tab");
        let log = dir.join("requests");
        let events = [
            popup_event_of(POPUP_IDS, 1, true, "background_tab"),
            popup_event_of(POPUP_IDS + 1, 1, true, "tab"),
            popup_event_of(POPUP_IDS + 2, 1, false, "tab"),
        ];
        // The foreground tab's page closes itself when told to navigate.
        let script = format!(
            "while read l; do echo \"$l\" >> '{}'; case \"$l\" in *'\"op\":\"navigate\"'*) \
             echo '{{\"ev\":\"closed\",\"id\":{}}}';; esac; done",
            log.display(),
            POPUP_IDS + 1
        );
        stub_app_host(&dir, &events, &script);
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        let surfaces = |s: &AppState| -> Vec<String> {
            s.describe().lines().filter(|l| l.contains("surface ")).map(|l| l.trim().to_string()).collect()
        };
        wait("the pop-ups are placed", || {
            s.pump_all();
            surfaces(&s).len() == 4
        });
        let got = surfaces(&s);
        // The opener, the foreground tab (shown), the background tab, and the
        // agent's hidden one.
        let id = |i: usize| got[i].split(' ').nth(1).unwrap().parse::<u64>().unwrap();
        assert!(got[0].starts_with(&format!("surface {} ", opener.0)), "{got:?}");
        assert!(got[1].ends_with("(active)"), "the foreground tab shows: {got:?}");
        assert!(!got[2].ends_with("(active)"), "{got:?}");
        assert!(got[3].starts_with("hidden surface"), "{got:?}");
        let foreground = SurfaceId(id(1));
        assert_eq!(s.focused_surface(), Some(foreground));
        let page = |s: &mut AppState, sid| s.view_mut(sid).unwrap().downcast_mut::<WebView>().unwrap().page;
        assert_eq!(page(&mut s, foreground), POPUP_IDS + 1);
        assert_eq!(page(&mut s, SurfaceId(id(2))), POPUP_IDS);

        // A tab whose page closes itself closes.
        registry.execute("web.navigate", &Args::new().with("url", Value::Str("example.com".into())), &mut s).unwrap();
        wait("the tab closes", || {
            s.pump_all();
            s.view(foreground).is_none()
        });
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    #[test]
    fn a_hidden_popup_outlives_other_surfaces_closing_but_not_its_opener() {
        let dir = crate::plugin::testing::tmpdir("web-hidden-keep");
        stub_app_host(&dir, &[popup_event(POPUP_IDS, 1, false)], "cat > /dev/null");
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        let hidden = |s: &AppState| s.describe().lines().any(|l| l.starts_with("hidden surface "));
        wait("the hidden pop-up", || {
            s.pump_all();
            hidden(&s)
        });
        // Another surface opens and closes.
        registry.execute("surface.new", &Args::new(), &mut s).unwrap();
        assert_ne!(s.focused_surface(), Some(opener));
        registry.execute("pane.close", &Args::new(), &mut s).unwrap();
        s.pump_all();
        assert!(hidden(&s), "closing another surface keeps it: {}", s.describe());
        // Its opener closes: it goes.
        s.close_surface_id(opener);
        s.pump_all();
        assert!(!hidden(&s), "{}", s.describe());
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    #[test]
    fn a_tab_opened_by_a_tab_that_closed_itself_still_opens() {
        let dir = crate::plugin::testing::tmpdir("web-popup-tab-chain");
        let (a, b) = (POPUP_IDS, POPUP_IDS + 1);
        let events = [
            popup_event_of(a, 1, true, "tab"),
            popup_event_of(b, a, true, "tab"),
            format!(r#"{{"ev":"closed","id":{a}}}"#),
        ];
        stub_app_host(&dir, &events, "cat > /dev/null");
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        wait("the second tab opens and the first closes", || {
            s.pump_all();
            pages(&mut s) == [1, b]
        });
        assert!(s.view(opener).is_some());
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    /// The pages of the web tabs in the workspaces, in tree order.
    fn pages(s: &mut AppState) -> Vec<u64> {
        let ids: Vec<SurfaceId> = s
            .describe()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("surface ")?.split(' ').next()?.parse().ok())
            .map(SurfaceId)
            .collect();
        ids.into_iter().filter_map(|sid| Some(s.view_mut(sid)?.downcast_mut::<WebView>()?.page)).collect()
    }

    /// Page `page`'s tab (the surface whose view shows it).
    fn tab_of(s: &mut AppState, page: u64) -> Option<SurfaceId> {
        let ids: Vec<SurfaceId> = s
            .describe()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("surface ")?.split(' ').next()?.parse().ok())
            .map(SurfaceId)
            .collect();
        ids.into_iter().find(|sid| s.view_mut(*sid).and_then(|v| v.downcast_mut::<WebView>()).is_some_and(|w| w.page == page))
    }

    /// Whether the host client behind surface `sid`'s web view has heard of
    /// page `page` (without the app taking anything from it).
    fn host_knows(s: &mut AppState, sid: SurfaceId, page: u64) -> bool {
        let v = s.view_mut(sid).unwrap().downcast_mut::<WebView>().unwrap();
        let slot = v.slot.borrow();
        slot.host.as_ref().is_some_and(|h| h.view(page).is_some())
    }

    /// Given a web tab (page 1) whose person's click opened a tab (A) the app
    /// has not placed yet, and a host that reports a second person's tab (B,
    /// in the background) from page 1 only once told to close page 1;
    /// when the person closes page 1's tab (Close, Cmd+W);
    /// then A and B open as tabs in that pane, A shown, and neither is closed.
    #[test]
    fn a_tab_a_person_opened_outlives_its_opener_closing_before_it_is_placed() {
        opener_closes_before_its_tabs_are_placed("web-popup-tab-orphan", false);
    }

    /// Given a web tab (page 1) whose view has taken a person's tab (A) from
    /// the host (as painting does) but the app has not placed it yet, and a
    /// host that reports a second person's tab (B, in the background) from
    /// page 1 only once told to close page 1;
    /// when the person closes page 1's tab (Close, Cmd+W);
    /// then A and B open as tabs in that pane, A shown, and neither is closed.
    #[test]
    fn a_tab_its_opener_took_outlives_the_opener_closing_before_it_is_placed() {
        opener_closes_before_its_tabs_are_placed("web-popup-tab-taken-orphan", true);
    }

    /// Page 1 opens tab A, then is closed before the app places A, with A
    /// taken by page 1's view first when `taken`; B arrives after the close.
    fn opener_closes_before_its_tabs_are_placed(tag: &str, taken: bool) {
        let dir = crate::plugin::testing::tmpdir(tag);
        let log = dir.join("requests");
        let (a, b) = (POPUP_IDS, POPUP_IDS + 1);
        let script = format!(
            "while read l; do echo \"$l\" >> '{}'; case \"$l\" in *'\"op\":\"close\",\"id\":1}}'*) \
             echo '{}'; echo '{{\"ev\":\"closed\",\"id\":1}}';; esac; done",
            log.display(),
            popup_event_of(b, 1, true, "background_tab"),
        );
        stub_app_host(&dir, &[popup_event_of(a, 1, true, "tab")], &script);
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        // Nothing pumps the app until the close, so A stays unplaced.
        wait("A reaches the app", || host_knows(&mut s, opener, a));
        if taken {
            let v = s.view_mut(opener).unwrap().downcast_mut::<WebView>().unwrap();
            v.pull_popups();
            assert_eq!(v.opened.len(), 1, "the view holds A");
        }
        registry.execute("pane.close", &Args::new(), &mut s).unwrap();
        assert!(s.view(opener).is_none(), "the opener closed");

        wait("both tabs open", || {
            s.pump_all();
            pages(&mut s) == [a, b]
        });
        assert_eq!(s.focused_surface(), tab_of(&mut s, a), "A shows");
        let got = requests(&log, |l| host_requests(l).contains(&HostRequest::Close { id: 1 }));
        let got = host_requests(&got);
        assert!(!got.iter().any(|r| matches!(r, HostRequest::Close { id } if *id != 1)), "{got:?}");
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    /// Given a page (1) with a person's pop-up window (P) floating over it, a
    /// person's tab (A) from P the app has not placed yet, and a host that
    /// reports a second person's tab (B) from P only once told to close P;
    /// when the person presses P's close button;
    /// then A opens as a tab right after page 1's and B right after A, and
    /// neither is closed.
    #[test]
    fn a_tab_opened_from_a_floating_popup_outlives_its_close_button() {
        let dir = crate::plugin::testing::tmpdir("web-float-tab-orphan");
        let log = dir.join("requests");
        let (p, a, b) = (POPUP_IDS, POPUP_IDS + 1, POPUP_IDS + 2);
        // A comes once page 1 is told to navigate (after P floats), and no
        // pump follows to take it.
        let script = format!(
            "while read l; do echo \"$l\" >> '{}'; case \"$l\" in \
             *'\"op\":\"navigate\",\"id\":1,'*) echo '{}';; \
             *'\"op\":\"close\",\"id\":{p}}}'*) echo '{}'; echo '{{\"ev\":\"closed\",\"id\":{p}}}';; esac; done",
            log.display(),
            popup_event_of(a, p, true, "tab"),
            popup_event_of(b, p, true, "tab"),
        );
        stub_app_host(&dir, &[popup_event(p, 1, true)], &script);
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let sid = s.focused_surface().unwrap();
        wait("P floats", || {
            s.pump_all();
            !s.view_mut(sid).unwrap().downcast_mut::<WebView>().unwrap().floating.is_empty()
        });
        registry.execute("web.navigate", &Args::new().with("url", Value::Str("example.com".into())), &mut s).unwrap();
        wait("A reaches the app", || host_knows(&mut s, sid, a));
        let h = Harness::new();
        h.button(s.view_mut(sid).unwrap(), POPUP_BASE + BTN_CLOSE);
        assert!(s.view_mut(sid).unwrap().downcast_mut::<WebView>().unwrap().floating.is_empty());

        wait("both tabs open", || {
            s.pump_all();
            pages(&mut s) == [1, a, b]
        });
        let got = requests(&log, |l| host_requests(l).contains(&HostRequest::Close { id: p }));
        let got = host_requests(&got);
        assert!(!got.iter().any(|r| matches!(r, HostRequest::Close { id } if *id != p)), "{got:?}");
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    /// Given a web tab the app created (page 1) and a host where, told to
    /// navigate, page 1 opens a person's tab (A) and then closes itself;
    /// when the page is navigated;
    /// then A opens as a tab and page 1's tab closes.
    #[test]
    fn a_tab_whose_page_closes_itself_closes_once_its_tabs_are_placed() {
        let dir = crate::plugin::testing::tmpdir("web-app-page-closes");
        let a = POPUP_IDS;
        let script = format!(
            "while read l; do case \"$l\" in *'\"op\":\"navigate\"'*) \
             echo '{}'; echo '{{\"ev\":\"closed\",\"id\":1}}';; esac; done",
            popup_event_of(a, 1, true, "tab"),
        );
        let title = r#"{"ev":"title","id":1,"title":"Page"}"#.to_string();
        stub_app_host(&dir, &[title], &script);
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        registry.execute("web.navigate", &Args::new().with("url", Value::Str("example.com".into())), &mut s).unwrap();
        wait("A opens and page 1's tab closes", || {
            s.pump_all();
            pages(&mut s) == [a]
        });
        assert!(s.view(opener).is_none());
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    /// Given a web tab (page 1) that opened a person's tab (A) and an agent's
    /// pop-up (B), both of which closed themselves before the app placed
    /// them;
    /// when the app pumps once;
    /// then only page 1's tab is left, still focused, and no hidden surface.
    #[test]
    fn a_popup_that_closed_before_it_was_placed_goes_in_the_same_pass() {
        let dir = crate::plugin::testing::tmpdir("web-popup-closed-unplaced");
        let (a, b) = (POPUP_IDS, POPUP_IDS + 1);
        let events = [
            popup_event_of(a, 1, true, "tab"),
            popup_event_of(b, 1, false, "tab"),
            format!(r#"{{"ev":"closed","id":{a}}}"#),
            format!(r#"{{"ev":"closed","id":{b}}}"#),
        ];
        stub_app_host(&dir, &events, "cat > /dev/null");
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        wait("both close in the host", || {
            let v = s.view_mut(opener).unwrap().downcast_mut::<WebView>().unwrap();
            let slot = v.slot.borrow();
            slot.host.as_ref().is_some_and(|h| h.is_closed(a) && h.is_closed(b))
        });
        s.pump_all();
        assert_eq!(pages(&mut s), [1], "{}", s.describe());
        assert_eq!(s.focused_surface(), Some(opener));
        assert!(!s.describe().contains("hidden surface"), "{}", s.describe());
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    /// Given a web tab (page 1) whose agent's pop-up (B) opened an agent's
    /// pop-up of its own (C) and then closed itself, before the app placed
    /// either;
    /// when the app pumps once;
    /// then neither is left as a hidden surface.
    #[test]
    fn a_hidden_popup_of_a_popup_that_closed_before_it_was_placed_goes_with_it() {
        let dir = crate::plugin::testing::tmpdir("web-popup-chain-closed-unplaced");
        let (b, c) = (POPUP_IDS, POPUP_IDS + 1);
        let events = [
            popup_event_of(b, 1, false, "tab"),
            popup_event_of(c, b, false, "tab"),
            format!(r#"{{"ev":"closed","id":{b}}}"#),
        ];
        stub_app_host(&dir, &events, "cat > /dev/null");
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        wait("B closes in the host", || {
            let v = s.view_mut(opener).unwrap().downcast_mut::<WebView>().unwrap();
            let slot = v.slot.borrow();
            slot.host.as_ref().is_some_and(|h| h.is_closed(b) && h.view(c).is_some())
        });
        s.pump_all();
        assert!(!s.describe().contains("hidden surface"), "{}", s.describe());
        assert_eq!(pages(&mut s), [1]);
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    #[test]
    fn close_and_edit_url_act_on_a_floating_popup_with_the_keyboard() {
        let dir = crate::plugin::testing::tmpdir("web-float-close");
        let log = dir.join("requests");
        let events = [popup_event(POPUP_IDS, 1, true), popup_event(POPUP_IDS + 1, 1, true)];
        stub_app_host(&dir, &events, &format!("cat > '{}'", log.display()));
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let sid = s.focused_surface().unwrap();
        fn web(s: &mut AppState, sid: SurfaceId) -> &mut WebView {
            s.view_mut(sid).unwrap().downcast_mut::<WebView>().unwrap()
        }
        wait("the pop-ups float", || {
            s.pump_all();
            web(&mut s, sid).floating.len() == 2
        });
        assert!(web(&mut s, sid).float_keys);

        // Editing the URL takes the keyboard back to the page's bar.
        registry.execute("web.focus_url", &Args::new(), &mut s).unwrap();
        let v = web(&mut s, sid);
        assert!(!v.float_keys && v.url_edit.is_some());
        assert!(!v.dismiss_floating(), "Close is the tab's while the page has the keyboard");
        v.url_edit = None;
        v.keys_to_float(true);

        // Close (Cmd+W) closes the top pop-up, not the tab; the keyboard
        // goes to the next one, then to the page.
        registry.execute("pane.close", &Args::new(), &mut s).unwrap();
        let v = web(&mut s, sid);
        assert!(v.floating.len() == 1 && v.float_keys);
        registry.execute("pane.close", &Args::new(), &mut s).unwrap();
        assert_eq!(s.focused_surface(), Some(sid), "the tab stays");
        let v = web(&mut s, sid);
        assert!(v.floating.is_empty() && !v.float_keys);
        requests(&log, |l| host_requests(l).contains(&HostRequest::Close { id: POPUP_IDS }));
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }

    #[test]
    fn a_popup_an_agent_opened_is_a_hidden_surface_it_can_drive() {
        let dir = crate::plugin::testing::tmpdir("web-hidden-popup");
        let log = dir.join("requests");
        let loaded = |id: u64| format!(r#"{{"ev":"loading","id":{id},"loading":false,"can_back":false,"can_forward":false}}"#);
        let events = [popup_event(POPUP_IDS, 1, false), loaded(1), loaded(POPUP_IDS)];
        // It answers text requests, and the pop-up closes itself when told
        // to navigate.
        let script = format!(
            "while read l; do echo \"$l\" >> '{}'; case \"$l\" in \
             *'\"op\":\"navigate\"'*) echo '{{\"ev\":\"closed\",\"id\":{POPUP_IDS}}}';; \
             *'\"op\":\"text\"'*) echo \"$l\" | sed 's/.*\"op\":\"text\",\"id\":\\([0-9]*\\),\"req\":\\([0-9]*\\).*/{{\"ev\":\"text\",\"id\":\\1,\"req\":\\2,\"text\":\"\"}}/';; \
             esac; done",
            log.display()
        );
        stub_app_host(&dir, &events, &script);

        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        registry.execute("web.new", &Args::new(), &mut s).unwrap();
        let opener = s.focused_surface().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let hidden = loop {
            s.pump_all();
            let tree = s.describe();
            if let Some(line) = tree.lines().find(|l| l.starts_with("hidden surface ")) {
                assert!(line.ends_with(&format!("(pop-up of surface {})", opener.0)), "{line}");
                break SurfaceId(line.split(' ').nth(2).unwrap().parse().unwrap());
            }
            assert!(Instant::now() < until, "no hidden surface: {tree}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(s.focused_surface(), Some(opener), "it took no focus");
        assert!(!s.describe().contains(&format!("    surface {}", hidden.0)), "it is in no pane");
        let text = s.surface_text(opener).unwrap();
        assert!(text.contains(&format!("pop-up: surface {} (hidden)\n", hidden.0)), "{text}");

        // `web.*` commands reach it.
        let args = Args::new().with("url", Value::Str("example.com".into())).with("id", Value::Int(hidden.0 as i64));
        registry.execute("web.navigate", &args, &mut s).unwrap();
        requests(&log, |l| host_requests(l).iter().any(|r| matches!(r, HostRequest::Navigate { id: POPUP_IDS, .. })));

        // It closed itself: it goes.
        let until = Instant::now() + Duration::from_secs(10);
        while s.describe().contains("hidden surface") {
            assert!(Instant::now() < until, "the closed pop-up stayed");
            s.pump_all();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(s.view(hidden).is_none());
        TEST_HOST.with_borrow_mut(|h| *h = None);
    }


    /// The bundled host (`mise run web`) beside the test binary's profile
    /// directory, if built.
    fn real_host() -> Option<std::path::PathBuf> {
        let dir = std::env::current_exe().ok()?.parent()?.parent()?.to_path_buf();
        let name = ghostrealm_web::client::HOST_NAME;
        let mac = dir.join(format!("{name}.app/Contents/MacOS/{name}"));
        let linux = dir.join(name).join(name);
        [mac, linux].into_iter().find(|p| p.is_file())
    }

    /// A local HTTP server: `/opener` opens `/popup` on a click, counting its
    /// clicks in `data-clicks`, and logs the messages it gets in `data-got`;
    /// `/popup` posts to its opener and closes itself a second later; `/stay`
    /// is a page; `/link` is a link to `/stay` over the whole page, counting
    /// its clicks (middle ones too) in `data-clicks`.
    fn serve() -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let body = match line.split(' ').nth(1).unwrap_or("/") {
                    "/opener" => "<title>Opener</title><body style='margin:0'><button \
                        style='position:fixed;inset:0;border:0;background:rgb(255,0,0)' \
                        onclick=\"document.body.dataset.clicks = +(document.body.dataset.clicks || 0) + 1; \
                        window.open('/popup', 'p', 'width=200,height=100')\">open</button>\
                        <script>addEventListener('message', e => document.body.dataset.got = \
                        (document.body.dataset.got || '') + e.data + ';')</script>",
                    "/popup" => "<title>Popup</title><script>window.opener.postMessage('from the popup', '*'); \
                        setTimeout(() => window.close(), 1000)</script>",
                    "/stay" => "<title>Stay</title><p>stay text</p>",
                    "/link" => "<title>Link</title><body style='margin:0'><a href='/stay' \
                        style='position:fixed;inset:0;background:rgb(255,0,0)'>x</a><script>\
                        for (const t of ['click', 'auxclick']) addEventListener(t, () => \
                        document.body.dataset.clicks = +(document.body.dataset.clicks || 0) + 1, true)</script>",
                    _ => "",
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        base
    }

    fn wait(what: &str, mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < until, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// End to end with the real host (skipped when it isn't built).
    /// Given a page that opens a pop-up window on a click;
    /// when a person clicks it, and an agent's gesture eval opens a page;
    /// then the click's pop-up floats with the keyboard and goes when it
    /// closes itself, and the eval's is a hidden surface the agent drives and
    /// closes; both talk to their opener.
    /// Given a page covered by a link;
    /// when a person clicks it with Cmd (Ctrl off macOS), the middle button,
    /// or Shift;
    /// then each opens the link as a tab for the app to place beside the
    /// page (in the background, the background, and shown), and the page
    /// stays where it was.
    #[test]
    fn popups_with_the_real_host() {
        let Some(exe) = real_host() else {
            eprintln!("skipping: web host not built (run `mise run web`)");
            return;
        };
        let base = serve();
        let profile = crate::plugin::testing::tmpdir("web-popups-profile");
        // No other test here spawns a real host, which reads this at spawn.
        std::env::set_var("GHOSTREALM_WEB_CACHE", profile.join("person"));

        // A person's click.
        let plugin = WebPlugin::new();
        plugin.slot.borrow_mut().exe = Some(exe.clone());
        let view = plugin.open(&OpenCx::default()).unwrap();
        let mut v = *(view as Box<dyn Any>).downcast::<WebView>().unwrap();
        v.start = Some(format!("{base}/opener"));
        v.start().unwrap();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        wait("the opener paints", || {
            v.with_host(|h, id| h.frame(id).and_then(|f| f.read()).is_some_and(|px| px[..3] == [0, 0, 255]))
                .unwrap_or(false)
        });
        // A page that has just painted may not hit-test input yet: click
        // again if the page heard no click within 2 s. A click it heard
        // opens a pop-up, however slowly, so it is not repeated.
        let at = (v.body.x + 10.0, v.body.y + 10.0);
        let mut clicks = 0;
        let mut clicked = Instant::now() - Duration::from_secs(2);
        wait("the pop-up floats", || {
            if v.floating.is_empty()
                && clicked.elapsed() >= Duration::from_secs(2)
                && clicks < 5
                && v.eval("document.body.dataset.clicks || ''", false) == Ok("\"\"".into())
            {
                clicks += 1;
                clicked = Instant::now();
                h.mouse(&mut v, MouseEvent::Move { pos: at });
                h.mouse(&mut v, MouseEvent::Down { pos: at, clicks: 1, button: MouseButton::Left });
                h.mouse(&mut v, MouseEvent::Up { pos: at, dragged: false, button: MouseButton::Left });
            }
            h.paint(&mut v, RECT);
            !v.floating.is_empty()
        });
        assert!(v.float_keys, "it has the keyboard");
        assert!(v.take_opened().is_empty(), "it isn't hidden");
        wait("the pop-up closes itself", || {
            v.refresh();
            v.floating.is_empty()
        });
        assert!(!v.float_keys);
        // A click still on its way when the page was asked can make a second
        // pop-up (or load the first again): each posts the same message.
        let got = v.eval("document.body.dataset.got", false).unwrap();
        let posts: Vec<&str> = got.trim_matches('"').split_terminator(';').collect();
        assert!(!posts.is_empty() && posts.iter().all(|m| *m == "from the popup"), "{got}");
        drop(v);

        // A person's modified clicks on a link.
        let view = plugin.open(&OpenCx::default()).unwrap();
        let mut v = *(view as Box<dyn Any>).downcast::<WebView>().unwrap();
        v.start = Some(format!("{base}/link"));
        v.start().unwrap();
        h.paint(&mut v, RECT);
        wait("the link paints", || {
            v.with_host(|h, id| h.frame(id).and_then(|f| f.read()).is_some_and(|px| px[..3] == [0, 0, 255]))
                .unwrap_or(false)
        });
        use ghostrealm_terminal::Mods;
        let new_tab = Mods { super_: cfg!(target_os = "macos"), ctrl: !cfg!(target_os = "macos"), ..Mods::default() };
        let shift = Mods { shift: true, ..Mods::default() };
        let cases = [
            (new_tab, MouseButton::Left, false),
            (Mods::default(), MouseButton::Middle, false),
            (shift, MouseButton::Left, true),
        ];
        for (n, (mods, button, focus)) in (1..).zip(cases) {
            // As above: click again only while the page heard no click.
            let mut clicks = 0;
            let mut clicked = Instant::now() - Duration::from_secs(2);
            let mut opened = Vec::new();
            wait("the link opens a tab", || {
                if clicked.elapsed() >= Duration::from_secs(2)
                    && clicks < 5
                    && v.eval("+(document.body.dataset.clicks || 0)", false) == Ok((n - 1).to_string())
                {
                    clicks += 1;
                    clicked = Instant::now();
                    h.mouse_with(&mut v, mods, MouseEvent::Move { pos: at });
                    h.mouse_with(&mut v, mods, MouseEvent::Down { pos: at, clicks: 1, button });
                    h.mouse_with(&mut v, mods, MouseEvent::Up { pos: at, dragged: false, button });
                }
                h.paint(&mut v, RECT);
                opened.extend(v.take_opened());
                !opened.is_empty()
            });
            let case = format!("{mods:?} {button:?}");
            assert_eq!(opened.len(), 1, "{case}");
            assert_eq!(opened[0].place, Place::Tab { focus }, "{case}");
            assert!(v.floating.is_empty(), "{case}");
            let tab = opened[0].view.page;
            let title = v.with_host(|h, _| h.eval(tab, "document.title", Duration::from_secs(5)));
            assert_eq!(title, Some(Ok("\"Stay\"".into())), "{case}");
            assert_eq!(v.eval("document.title", false), Ok("\"Link\"".into()), "{case}");
        }
        drop(v);
        drop(plugin);

        // An agent's eval.
        std::env::set_var("GHOSTREALM_WEB_CACHE", profile.join("agent"));
        TEST_HOST.with_borrow_mut(|h| *h = Some(exe));
        let mut s = AppState::new().with_shell_line("sleep 1");
        s.new_empty_vtab();
        let mut registry = crate::app_state::build_registry();
        let mut run = |s: &mut AppState, id: &str, args: Vec<(&str, Value)>| {
            let a = args.into_iter().fold(Args::new(), |a, (k, v)| a.with(k, v));
            registry.execute(id, &a, s).map(|o| o.message.unwrap_or_default())
        };
        let eval = |id: u64, js: &str| vec![("id", Value::Int(id as i64)), ("js", Value::Str(js.into()))];
        let gesture = |id: u64, js: &str| {
            let mut a = eval(id, js);
            a.push(("gesture", Value::Bool(true)));
            a
        };
        run(&mut s, "web.new", vec![("url", Value::Str(format!("{base}/opener")))]).unwrap();
        let opener = s.focused_surface().unwrap();
        let blocked = run(&mut s, "web.eval", eval(opener.0, "window.open('/stay') !== null"));
        assert_eq!(blocked, Ok("false".into()), "an eval is no user gesture by default");
        let opened = run(&mut s, "web.eval", gesture(opener.0, "window.open('/stay') !== null"));
        assert_eq!(opened, Ok("true".into()));
        s.pump_all();
        let tree = s.describe();
        let line = tree.lines().find(|l| l.starts_with("hidden surface ")).unwrap_or_else(|| panic!("{tree}"));
        let hidden: u64 = line.split(' ').nth(2).unwrap().parse().unwrap();
        assert_eq!(s.focused_surface(), Some(opener), "it took no focus");
        assert_eq!(run(&mut s, "web.eval", eval(hidden, "document.title")), Ok("\"Stay\"".into()));
        let text = s.surface_text(SurfaceId(hidden)).unwrap();
        assert!(text.contains("stay text") && text.starts_with(&format!("pop-up of surface {}", opener.0)), "{text}");
        let posted = run(&mut s, "web.eval", eval(hidden, "window.opener.postMessage('from the agent', '*'); 1"));
        assert_eq!(posted, Ok("1".into()));
        wait("the opener hears it", || {
            run(&mut s, "web.eval", eval(opener.0, "document.body.dataset.got || ''")) == Ok("\"from the agent;\"".into())
        });
        let _ = run(&mut s, "web.eval", eval(hidden, "window.close()"));
        wait("the hidden pop-up goes", || {
            s.pump_all();
            s.view(SurfaceId(hidden)).is_none()
        });

        // `web.close` closes one.
        assert_eq!(run(&mut s, "web.eval", gesture(opener.0, "window.open('/stay') !== null")), Ok("true".into()));
        s.pump_all();
        let tree = s.describe();
        let line = tree.lines().find(|l| l.starts_with("hidden surface ")).unwrap_or_else(|| panic!("{tree}"));
        let hidden: u64 = line.split(' ').nth(2).unwrap().parse().unwrap();
        run(&mut s, "web.close", vec![("id", Value::Int(hidden as i64))]).unwrap();
        assert!(s.view(SurfaceId(hidden)).is_none());
        assert!(!s.describe().contains("hidden surface"));
        assert_eq!(s.view(opener).map(|v| v.plugin()), Some(ID), "the opener stays");
        TEST_HOST.with_borrow_mut(|h| *h = None);
        drop(s);
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_floating_popup_fits_the_page_centred() {
        let body = Rect { x: 10.0, y: 20.0, w: 400.0, h: 300.0 };
        assert_eq!(float_rect(body, (200, 100), 20.0, 10.0), Rect { x: 110.0, y: 110.0, w: 200.0, h: 120.0 });
        assert_eq!(float_rect(body, (1000, 1000), 20.0, 10.0), Rect { x: 20.0, y: 30.0, w: 380.0, h: 280.0 });
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
                button: MouseButton::Left,
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
    fn the_url_bar_takes_the_mouse_once_editing() {
        let mut v = hostless();
        let mut h = Harness::new();
        v.edit_url();
        let e = v.url_edit.as_mut().unwrap();
        e.insert("rust lang");
        h.paint(&mut v, RECT);
        let g = v.url_field.geom().expect("live while editing");
        let y = g.rect.y + g.rect.h * 0.5;
        let cell = |col: usize| g.left + (col as f32 + 0.5) * g.cell_w;
        let edge = |col: usize| g.left + col as f32 * g.cell_w;
        h.mouse(&mut v, MouseEvent::Down { pos: (edge(1), y), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Drag { pos: (edge(3), y), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: (edge(3), y), dragged: true, button: MouseButton::Left });
        assert!(v.url_edit.is_some(), "still editing");
        assert_eq!(v.url_edit.as_ref().unwrap().selection(), Some((1, 3)));
        h.mouse(&mut v, MouseEvent::Down { pos: (cell(7), y), clicks: 2, button: MouseButton::Left });
        assert_eq!(v.url_edit.as_ref().unwrap().selection(), Some((5, 9)), "`lang`");
    }

    #[test]
    fn a_click_on_the_idle_url_bar_selects_all_and_a_drag_in_that_press_selects_from_the_press() {
        let mut v = hostless();
        v.state.url = "https://rust-lang.org".into();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let g = v.url_field.geom().expect("an idle bar takes the mouse too");
        let y = g.rect.y + g.rect.h * 0.5;
        let edge = |col: usize| g.left + col as f32 * g.cell_w;
        // A plain click selects the whole address.
        h.mouse(&mut v, MouseEvent::Down { pos: (edge(2), y), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: (edge(2), y), dragged: false, button: MouseButton::Left });
        assert_eq!(v.url_edit.as_ref().unwrap().selection(), Some((0, 21)));
        // A drag from a press on the idle bar selects what it covers.
        v.url_edit = None;
        h.paint(&mut v, RECT);
        h.mouse(&mut v, MouseEvent::Down { pos: (edge(2), y), clicks: 1, button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Drag { pos: (edge(6), y), button: MouseButton::Left });
        h.mouse(&mut v, MouseEvent::Up { pos: (edge(6), y), dragged: true, button: MouseButton::Left });
        assert_eq!(v.url_edit.as_ref().unwrap().selection(), Some((2, 6)));
    }

    #[test]
    fn the_url_bar_shows_the_text_cursor_idle_and_editing() {
        let mut v = hostless();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let r = v.url_box;
        h.cursor = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        for editing in [false, true] {
            if editing {
                v.edit_url();
            }
            let frame = h.paint(&mut v, RECT);
            assert_eq!(frame.cursor.map(|(_, i)| i), Some(winit::window::CursorIcon::Text), "editing: {editing}");
        }
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
    fn leaving_the_view_ends_the_page_hover() {
        let mut v = hostless();
        let mut h = Harness::new();
        h.paint(&mut v, RECT);
        let inside = (v.body.x + 10.0, v.body.y + 10.0);
        h.mouse(&mut v, MouseEvent::Move { pos: inside });
        assert!(v.hovering);
        h.mouse(&mut v, MouseEvent::Leave { pos: (900.0, 10.0) });
        assert!(!v.hovering, "the page is told the pointer left");
    }

    #[test]
    fn a_bracket_typed_over_the_selected_url_replaces_it() {
        let mut v = hostless();
        v.state.url = "https://example.com/".into();
        v.edit_url();
        let h = Harness::new();
        for c in "[::1]".chars() {
            h.key(&mut v, press(Key::Char(c)));
        }
        assert_eq!(v.url_edit.as_ref().unwrap().text(), "[::1]");
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
