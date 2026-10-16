//! wgpu + winit + glyphon window hosting the multiplexer.
//!
//! Renders the active vtab's split tree: each pane's active surface is drawn in
//! its computed rect (background quads + cursor via an instanced-quad pipeline,
//! foreground text via glyphon), all in one wgpu scene. Cmd-chords run app
//! commands through the registry; other keys go to the focused surface.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_core::config::Catalogue;
use ghostrealm_core::{
    ArgKind, ArgSpec, Args, Axis, Chrome, Config, Divider, PaneId, Rect, Registry, Side,
    SurfaceId, TabStatus, Value, VtabId,
};
use ghostrealm_terminal::{Key, KeyPress, Mods};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton as WinitButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use crate::config_watch::{self, ConfigWatch};
use crate::app_state::{build_registry, AppState, Pick, PICKER_SID, SETTINGS_VTAB_NAME};
use crate::file_menu::{FileAction, FileTarget, MenuEntry};
use crate::plugin::paint::{line_quad, push_border, rect_quad, round_quad, srgb_to_linear, QuadInstance};
use crate::plugin::text_edit::{Applied, LineEdit};
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Font, Frame, Layer, MouseButton, MouseEvent, Outcome, PaintCx, RawKey,
    MenuAlign, MenuItem, Request, Shaped, TextItem, TextKit, TextSrc, UiMetrics, View,
};
use crate::clicks::{Click, ClickCounter};
use crate::dividers::{self, Dividers, Press};
use crate::images::ImageRenderer;
use crate::tap::{HeldKey, TapDetector};

/// Divider gap between split panes, in physical pixels.
const DIVIDER: f32 = 6.0;
/// How far from the divider gap, in logical pixels, a pointer still grabs it.
const DIVIDER_SLOP: f32 = 3.0;
/// How many command-palette matches to rank (the visible window scrolls through
/// them).
const PALETTE_SEARCH_CAP: usize = 100;
/// Max result rows shown at once in the palette (the rest scroll into view).
const PALETTE_VISIBLE: usize = 10;
/// Minimum spacing between PTY-driven frames (~120fps). Coalesces a flood of
/// output wakes into at most one redraw per interval so input stays responsive.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);
/// Per-frame wall-clock budget for shaping cache-missed rows. Rows past it draw
/// background-only for this frame and finish on following frames, so a fast
/// scroll into fresh content never stalls a frame on a burst of shaping.
const SHAPE_BUDGET: Duration = Duration::from_millis(5);
/// How long a tooltip's anchor must stay hovered before the tooltip shows.
const TOOLTIP_DELAY: Duration = Duration::from_millis(500);
/// Max child-output bytes drained per surface per frame. Bounds VT-parse work so
/// one burst can't stall a frame; the remainder is pumped on following frames.
const PUMP_BUDGET: usize = 512 * 1024;
/// Soft cap on cached shaped rows. Comfortably holds several full screens so
/// scrollback and multiple surfaces stay warm; trimmed after each frame.
const ROW_CACHE_CAP: usize = 4096;
/// Scrollback lines per mouse-wheel notch.
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
/// Longest entry name the picker's confirm button shows before eliding.
const PATH_LABEL_MAX: usize = 32;
/// Logical height of the custom macOS title-bar strip (points; scaled per-DPI).
const TITLE_BAR_H: f32 = 28.0;
/// Logical width reserved at the top-left for the macOS traffic-light buttons.
const TRAFFIC_LIGHT_W: f32 = 76.0;
/// Empty-workspace screen: dim hint colour.
const EMPTY_HINT: [u8; 3] = [110, 110, 125];

/// Glyphs pre-rasterised into the atlas after a metrics change so the first
/// scroll into fresh content doesn't stall rasterising them: printable ASCII
/// plus the box-drawing/block set common in TUIs.
const ATLAS_WARM_GLYPHS: &str = concat!(
    " !\"#$%&'()*+,-./0123456789:;<=>?@",
    "ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`",
    "abcdefghijklmnopqrstuvwxyz{|}~",
    "─│┌┐└┘├┤┬┴┼═║╔╗╚╝╠╣╦╩╬",
    "█▀▄▌▐░▒▓▔▕■□▪▫●○◆◇•·…←↑→↓",
);

/// Wakes the event loop when a terminal produces output, so we redraw on demand
/// instead of polling (and reshaping) every frame.
#[derive(Debug, Clone, Copy)]
enum UserEvent {
    PtyOutput,
    /// The background font loader finished; the full system-font DB is waiting on
    /// the channel and should be swapped in. Carries no data (the DB isn't `Copy`).
    FontsLoaded,
    /// The system asked the app to terminate (the app menu, the Dock, logout)
    /// and is waiting for `about_to_wait` to answer.
    Terminate,
}

pub fn run(command_line: Option<String>) -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("create event loop")?;
    // Event-driven: sleep until input, resize, or PTY output wakes us.
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let mut app = App {
        state: None,
        command_line,
        proxy,
        wake_pending: Arc::new(AtomicBool::new(false)),
        unsaved: UnsavedFlag::default(),
    };
    event_loop.run_app(&mut app).context("run app")?;
    Ok(())
}

struct App {
    state: Option<State>,
    command_line: Option<String>,
    proxy: EventLoopProxy<UserEvent>,
    /// Set by the reader threads, cleared when the loop consumes a wake. Lets the
    /// waker send at most one pending `PtyOutput` event no matter how many chunks
    /// arrive, so a flood doesn't drown the event queue.
    wake_pending: Arc<AtomicBool>,
    /// Whether quitting would ask first (unsaved changes, a copy or move
    /// still running). The system's quit request reads it: AppKit wants an
    /// answer at once.
    unsaved: UnsavedFlag,
}

/// A cached answer to "would quitting ask first?" (about unsaved changes, or
/// a file copy or move still running), shared with the system's quit request.
/// The exact value is computed once per loop pass (`about_to_wait`); every
/// event handler first assumes the worst, because an event (a keystroke) may
/// edit a buffer and a quit request can arrive in the same pass before the
/// exact value is refreshed. A termination request that
/// meets a `true` is held, and answered once the exact value is known (a wrong
/// `true` is answered yes), so a stale flag never cancels a logout.
#[derive(Clone, Default)]
struct UnsavedFlag {
    unsaved: Rc<Cell<bool>>,
    /// A termination request is waiting for the exact value.
    reply_due: Rc<Cell<bool>>,
}

impl UnsavedFlag {
    fn assume_unsaved(&self) {
        self.unsaved.set(true);
    }

    fn set(&self, unsaved: bool) {
        self.unsaved.set(unsaved);
    }

    fn get(&self) -> bool {
        self.unsaved.get()
    }

    /// Hold a termination request until [`Self::settle`].
    fn hold_termination(&self) {
        self.reply_due.set(true);
    }

    /// With the exact value known, the answer to a held termination request:
    /// `Some(true)` to go ahead, `Some(false)` to stay and ask. `None` when
    /// none is held.
    fn settle(&self, unsaved: bool) -> Option<bool> {
        self.set(unsaved);
        self.reply_due.take().then_some(!unsaved)
    }
}

#[cfg(target_os = "macos")]
use crate::macos_quit::{reply_to_terminate, reply_to_terminate_later};
#[cfg(not(target_os = "macos"))]
fn reply_to_terminate(_go_ahead: bool) {}
#[cfg(not(target_os = "macos"))]
fn reply_to_terminate_later(_go_ahead: bool) {}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("ghostrealm")
            .with_inner_size(LogicalSize::new(900.0, 560.0));
        // On macOS, extend the content view under a transparent title bar and hide
        // the native title so we render our own title-bar strip (title + sidebar
        // toggle) while the traffic lights float over the top-left corner.
        #[cfg(target_os = "macos")]
        let attrs = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true)
        };
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        #[cfg(target_os = "macos")]
        crate::macos_keys::route_reserved_chords(&window);
        match pollster::block_on(State::new(
            window,
            self.command_line.clone(),
            self.proxy.clone(),
            self.wake_pending.clone(),
        )) {
            Ok(s) => {
                // Held while something would be lost: unsaved changes (the app
                // asks), or a file operation still running (cut off half done).
                #[cfg(target_os = "macos")]
                crate::macos_quit::intercept_terminate({
                    let (proxy, unsaved) = (self.proxy.clone(), self.unsaved.clone());
                    let jobs = s.app.file_jobs_pending();
                    move || {
                        if !unsaved.get() && jobs.idle() {
                            return crate::macos_quit::Terminate::Now;
                        }
                        unsaved.hold_termination();
                        let _ = proxy.send_event(UserEvent::Terminate);
                        crate::macos_quit::Terminate::Later
                    }
                });
                s.window.request_redraw();
                self.state = Some(s);
            }
            Err(e) => {
                eprintln!("ghostrealm: failed to init window state: {e:#}");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        self.unsaved.assume_unsaved();
        let Some(state) = &mut self.state else { return };
        match event {
            // A terminal produced output. Let the waker send another wake now, and
            // note that a frame is due; `about_to_wait` paces the actual redraw so
            // a flood of output can't outrun input handling.
            UserEvent::PtyOutput => {
                self.wake_pending.store(false, Ordering::Release);
                state.frame_pending = true;
            }
            // The full font DB is ready: swap it in for complete glyph fallback.
            UserEvent::FontsLoaded => state.upgrade_fonts(),
            // Answered in `about_to_wait`, once the unsaved state is exact.
            UserEvent::Terminate => {}
        }
    }

    /// AppKit is terminating the app (its quit went ahead at once, or after
    /// a held request was answered yes): report the copies and moves the
    /// file worker finished, as an in-window quit does before it exits.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = &mut self.state {
            state.app.poll_file_jobs_on_exit();
            state.print_file_reports();
        }
    }

    /// Pace PTY-driven redraws: at most one per `FRAME_INTERVAL`, and never in a
    /// way that starves queued input (winit delivers input events, waking the
    /// `WaitUntil` sleep, before this schedules the next frame).
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(state) = &mut self.state else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        // Text a command put aside for the system clipboard.
        if let Some(text) = state.app.take_clipboard_text() {
            if let Some(cb) = state.clipboard.as_mut() {
                let _ = cb.set_text(text);
            }
        }
        state.print_file_reports();
        // The copy the quit question was about finished: quit.
        if state.app.continue_busy_quit() {
            state.exit_if_quitting();
        }
        // A copy or move still running asks too: going ahead would leave it
        // half done.
        let ask = state.app.has_unsaved() || !state.app.file_jobs_idle();
        match self.unsaved.settle(ask) {
            // After this handler returns: terminating inside it would re-enter winit.
            Some(true) => reply_to_terminate_later(true),
            // Asked from outside the window (it may be minimised, or the app
            // in the background): bring it forward to show the question.
            Some(false) => {
                reply_to_terminate(false);
                state.request_quit();
                if state.app.confirm().is_some() {
                    state.window.set_minimized(false);
                    state.window.focus_window();
                }
                state.window.request_redraw();
            }
            None => {}
        }
        // Fire view timers (e.g. an editor's idle autosave) that came due while
        // the app sat idle.
        if state.fire_view_timers() {
            state.window.request_redraw();
        }
        // Wake for whichever comes first: the next paced PTY frame, a pending inbox
        // auto-read deadline, or a view's timer.
        let mut wake: Option<Instant> = state.frame_pending.then_some(state.next_frame);
        if let Some(d) = state.app.next_inbox_deadline() {
            wake = Some(wake.map_or(d, |w| w.min(d)));
        }
        if let Some(d) = state.next_view_deadline() {
            wake = Some(wake.map_or(d, |w| w.min(d)));
        }
        if let Some(d) = state.tooltip_due {
            if Instant::now() >= d {
                state.dirty = true;
            }
            wake = Some(wake.map_or(d, |w| w.min(d)));
        }
        if state.window_focused {
            if Instant::now() >= state.config_check_due {
                state.check_files();
            }
            let d = state.config_check_due;
            wake = Some(wake.map_or(d, |w| w.min(d)));
        }
        match wake {
            Some(t) if Instant::now() >= t => {
                state.window.request_redraw();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.unsaved.assume_unsaved();
        let Some(state) = &mut self.state else { return };
        state.drop_stale_menu();
        match event {
            WindowEvent::CloseRequested => {
                state.request_quit();
                state.window.request_redraw();
            }
            WindowEvent::Focused(true) => {
                state.window_focused = true;
                state.check_files();
            }
            WindowEvent::Focused(false) => {
                state.window_focused = false;
                // App lost focus: autosave editors (switching to another app), and
                // hot-reload the config if it was one of them.
                if state.app.autosave_all(&state.cfg) {
                    state.reload_config();
                }
                // The release of a press held across the focus loss may never
                // arrive; left held, hover could never be withdrawn.
                if state.mouse_down {
                    state.end_press(state.press_button);
                    state.window.request_redraw();
                }
                // A button pressed before switching away is cancelled, not
                // fired by a release that may arrive without its press.
                state.armed_button = None;
                // Likewise a selection dragged in the palette's input line.
                state.palette_release();
            }
            WindowEvent::Resized(size) => {
                state.resize(size.width, size.height);
                state.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                state.set_scale(scale_factor as f32);
                state.window.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => {
                let s = m.state();
                state.mods = Mods {
                    shift: s.shift_key(),
                    ctrl: s.control_key(),
                    alt: s.alt_key(),
                    super_: s.super_key(),
                };
            }
            WindowEvent::CursorLeft { .. } => {
                let left = state.hover.left_window(state.mouse_down);
                if state.send_leave(left) {
                    state.window.request_redraw();
                }
            }
            WindowEvent::CursorEntered { .. } => state.hover.entered(),
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor = (position.x as f32, position.y as f32);
                let mut redraw = false;
                if state.palette.is_some() {
                    redraw |= if state.palette_selecting { state.palette_drag() } else { state.palette_hover() };
                } else if state.menu.is_some() {
                    redraw |= state.menu_hover();
                } else if state.dividers.dragging() {
                    redraw |= state.drag_divider();
                } else if state.mouse_down {
                    redraw |= state.update_drag();
                } else if state.cfg.input.focus_follows_mouse && state.focus_pane_under_cursor() {
                    redraw = true;
                }
                // Close-button hover highlight tracks the cursor everywhere.
                redraw |= state.update_button_hover();
                redraw |= state.update_htab_hover();
                redraw |= state.update_divider_hover();
                // Hover reaches the view under the cursor (e.g. the file browser's
                // highlight follows the mouse).
                redraw |= state.hover_view();
                // Leaving the rect a view set a cursor for restores the default.
                if state.cursor_rect.is_some_and(|r| !rect_contains(r, state.cursor.0, state.cursor.1)) {
                    state.cursor_rect = None;
                    state.set_cursor_icon(CursorIcon::Default);
                    redraw = true;
                }
                if redraw {
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: WinitButton::Left,
                ..
            } => {
                state.release_lost_press();
                // A close button arms on press and fires on release (so it can be
                // cancelled); a press on the empty title-bar strip drags the window;
                // other clicks act on press as before.
                if state.arm_button() {
                    // armed; fires on release
                } else if state.cursor.1 < state.title_bar_h() {
                    let _ = state.window.drag_window();
                } else if state.press_divider() {
                    // grabbed a split divider
                } else {
                    state.on_click();
                    state.begin_press(MouseButton::Left);
                }
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: WinitButton::Left,
                ..
            } => {
                state.palette_release();
                if !state.fire_menu() {
                    state.fire_button();
                }
                state.end_press(MouseButton::Left);
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: WinitButton::Right,
                ..
            } => {
                state.release_lost_press();
                state.on_right_click();
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: WinitButton::Middle,
                ..
            } => {
                state.release_lost_press();
                state.press_view(MouseButton::Middle);
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: button @ (WinitButton::Right | WinitButton::Middle),
                ..
            } => {
                let button = if button == WinitButton::Right { MouseButton::Right } else { MouseButton::Middle };
                state.end_press(button);
                state.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Scroll is applied to the VT immediately, but the (expensive)
                // redraw is paced by `about_to_wait` so a trackpad flick coalesces
                // into ~one render per frame instead of one per event.
                state.on_scroll(delta);
            }
            WindowEvent::KeyboardInput { event, .. }
                if state.closing_key.swallows(
                    &event.physical_key,
                    event.state == ElementState::Pressed,
                    event.repeat,
                    is_modifier_key(&event.logical_key),
                ) => {}
            WindowEvent::KeyboardInput { event, .. } => {
                let toggled = state.handle_key_taps(&event);
                if event.state == ElementState::Pressed {
                    state.key_in_flight = Some(event.physical_key);
                    state.on_key(&event);
                    state.key_in_flight = None;
                    state.mark_dirty();
                    state.window.request_redraw();
                } else {
                    // Views that do their own key handling (a web page) need releases.
                    if state.on_key_release(&event) || toggled {
                        state.window.request_redraw();
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = state.render() {
                    eprintln!("ghostrealm: render error: {e:#}");
                }
            }
            _ => {}
        }
    }
}

struct State {
    window: Arc<Window>,
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,

    /// The shared font system, content-keyed row cache, and per-frame scratch
    /// text shared by the chrome and every view.
    text: TextKit,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// Bumped when font metrics change (scale/size); folded into row-cache keys so
    /// stale shaping never survives a metrics change.
    metrics_gen: u64,
    /// Reusable buffer holding the common glyph set, shaped once per metrics
    /// change to pre-warm the atlas (see [`ATLAS_WARM_GLYPHS`]).
    warm_buffer: Buffer,
    /// `metrics_gen` the atlas was last warmed for; `None` forces a warm pass.
    atlas_warmed_gen: Option<u64>,
    /// Full system-font DB being loaded off-thread; taken and swapped in once the
    /// `FontsLoaded` event arrives. `None` when booting on the full DB already, or
    /// after the upgrade has happened.
    pending_fonts: Option<Receiver<glyphon::cosmic_text::fontdb::Database>>,
    /// Monospace family pinned at boot, re-applied to the full DB on upgrade so
    /// normal text doesn't visibly change font when the swap happens.
    mono_family: Option<String>,
    /// Locale for font fallback ordering, reused when rebuilding the FontSystem.
    font_locale: String,

    quad_pipeline: wgpu::RenderPipeline,
    /// Textures for the images views place (web pages, file previews).
    images: ImageRenderer,
    /// The mouse cursor shown now, and the rect a view asked for it over (the
    /// cursor resets to the default when the mouse leaves that rect).
    cursor_icon: CursorIcon,
    cursor_rect: Option<Rect>,
    /// The split divider the pointer is over or dragging.
    dividers: Dividers,
    /// Views painted last frame (on screen), for [`View::shown`].
    shown: std::collections::HashSet<SurfaceId>,
    /// The view last sent hover, told when the pointer leaves it.
    hover: Hover,
    quad_buffer: wgpu::Buffer,
    quad_capacity: u64,

    /// Separate text renderer for the palette so its text draws above the panel
    /// (terminal text and palette text can't share one pass with a quad between).
    palette_renderer: TextRenderer,
    /// Draws the context menu's text, after its quads: a menu can stack over
    /// another overlay (the picker), whose text draws in the palette pass.
    menu_renderer: TextRenderer,
    palette_buffers: Vec<Buffer>,
    palette: Option<Palette>,
    /// Last-rendered palette panel rect (physical px), for click-outside dismissal.
    palette_panel: Rect,
    /// Last-rendered palette result rows: (row rect, result index, command id),
    /// for click/hover hit-testing. Rebuilt each frame the palette is open.
    palette_rows: Vec<(Rect, usize, String)>,
    /// The palette's input line as last drawn: its rect, the x where its text
    /// (prefix included) starts after scrolling, and the prefix's byte length.
    palette_line: Option<(Rect, f32, usize)>,
    /// A left press that began in the palette's input line is selecting text.
    palette_selecting: bool,
    /// Text buffers for the sidebar's vtab names (drawn in the main text pass).
    sidebar_buffers: Vec<Buffer>,
    /// Text buffers for pane tab-strip labels (drawn in the main text pass).
    strip_buffers: Vec<Buffer>,
    /// Sidebar context menu, when open.
    menu: Option<Menu>,
    /// Text buffers for the context-menu item labels (drawn via the overlay pass).
    menu_buffers: Vec<Buffer>,
    /// Vertical scroll offset of the workspace list (physical px).
    sidebar_scroll: f32,
    /// Max sidebar scroll (content height beyond the visible area).
    sidebar_max_scroll: f32,
    /// Workspace rows as last rendered: (rect, vtab id), for click/right-click.
    sidebar_rows: Vec<(Rect, ghostrealm_core::VtabId)>,
    /// Close-button ('×') hit rects as last rendered, for tabs and workspaces.
    buttons: Vec<(Rect, ButtonAction)>,
    /// Index into `buttons` of the close button under the cursor, if any — used
    /// to redraw only when the hovered button changes.
    button_hover: Option<usize>,
    /// The pane tab under the cursor (pane, index): an unsaved one's close
    /// slot shows '×' instead of its dot while hovered.
    htab_hover: Option<(PaneId, usize)>,
    /// A close button pressed but not yet released. It fires on release only if
    /// the cursor is still over the same button, so a press can be cancelled by
    /// moving away before releasing.
    armed_button: Option<ButtonAction>,
    /// Single shaped '×' glyph, placed at each close button (one buffer, many
    /// placements). Reshaped on a metrics change like the other chrome buffers.
    close_buffer: Buffer,
    /// Shaped chevron for the sidebar show/hide toggle, placed via `toggle_place`.
    toggle_buffer: Buffer,
    /// Where to draw the toggle chevron this frame (set by `build_sidebar`).
    toggle_place: Option<Placement>,
    /// Shaped centred title for the custom macOS title-bar strip.
    title_buffer: Buffer,
    /// Where to draw the title-bar label this frame (set by `build_titlebar`).
    title_place: Option<Placement>,
    /// The floating picker's panel rect this frame (for click-outside).
    picker_panel: Option<Rect>,
    /// The tooltip anchor under the cursor and since when, for the show delay.
    tooltip_hover: Option<(Rect, Instant)>,
    /// When a pending tooltip is due to show (a wake-up).
    tooltip_due: Option<Instant>,
    /// Notices edits to the config file made outside the app.
    config_watch: ConfigWatch,
    /// Whether the window has keyboard focus (the config file is polled only
    /// then).
    window_focused: bool,
    /// When the focused window next checks the config file.
    config_check_due: Instant,
    /// Presses, for counting double-clicks.
    clicks: ClickCounter,
    /// The view the current press landed on; it gets the press's drag and
    /// release wherever the cursor goes.
    mouse_capture: Option<SurfaceId>,
    /// The button of the current press: only its release ends the press.
    press_button: MouseButton,
    /// Whether the workspace sidebar is collapsed (session-only, like soft-wrap).
    sidebar_hidden: bool,
    /// A mouse button is held (a press in progress).
    mouse_down: bool,
    /// Whether the current press has moved enough to count as a drag.
    dragging: bool,
    /// Physical-pixel position where the current press began.
    press_px: (f32, f32),
    /// The surface focused as of the last frame, so a focus change reaches the
    /// views that lost and gained it (e.g. an editor autosaves on blur).
    last_focused_surface: Option<SurfaceId>,
    /// System clipboard handle (None if unavailable).
    clipboard: Option<arboard::Clipboard>,
    /// Last cursor position in physical pixels, for click hit-testing.
    cursor: (f32, f32),

    app: AppState,
    registry: Registry<AppState>,
    /// Every config option, for checking the config file on (re)load.
    catalogue: Arc<Catalogue>,
    cfg: Config,
    /// Chrome colours resolved once at startup (explicit `[chrome]`, else inherited
    /// from Ghostty, else defaults).
    chrome: Chrome,
    mods: Mods,
    scale: f32,
    cell_w: f32,
    cell_h: f32,
    dirty: bool,
    /// A frame is due from a high-frequency source that must be rate-limited:
    /// queued PTY output (or a mid-drain budgeted pump) or mouse-wheel scrolling.
    /// `about_to_wait` paces it to at most one redraw per `FRAME_INTERVAL` so a
    /// burst of events can't outrun input handling.
    frame_pending: bool,
    /// Earliest time the next paced frame may run.
    next_frame: Instant,
    /// Double-tap-Shift detector for the palette (IntelliJ "Search Everywhere").
    shift_taps: TapDetector,
    /// The key whose press closed a terminal that had exited: its repeats are
    /// dropped, not typed into the tab that takes over.
    closing_key: HeldKey<winit::keyboard::PhysicalKey>,
    /// The physical key of the press being handled, for `closing_key`.
    key_in_flight: Option<winit::keyboard::PhysicalKey>,
    /// Double-tap-Ctrl detector for "Run Anything".
    ctrl_taps: TapDetector,
}

/// What the overlay input does with its text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaletteMode {
    /// Fuzzy-search the command registry; Enter runs the selected command.
    Commands,
    /// "Run Anything": Enter opens a new vtab running the typed command line.
    Run,
}

/// Collecting arguments for a chosen command before running it.
struct PendingArgs {
    id: String,
    title: String,
    specs: Vec<ArgSpec>,
    /// Values collected so far, one per spec in order.
    values: Vec<Value>,
    /// Arguments already known (the file menu's target), passed along with
    /// the collected ones.
    preset: Args,
}

/// Command-palette / run-anything overlay state.
struct Palette {
    /// The search query, run line, or argument being typed.
    input: LineEdit,
    /// Selected result index into the full (filtered) result list.
    selected: usize,
    /// Index of the first visible result row (scroll offset).
    scroll: usize,
    mode: PaletteMode,
    /// When set, the palette is collecting arguments for a chosen command; the
    /// input line feeds the current argument instead of the search query.
    pending: Option<PendingArgs>,
}

/// An action a sidebar context-menu item performs on its target vtab.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuAction {
    MarkRead,
    MarkUnread,
    Dismiss,
    Rename,
    SetDir,
    Close,
    /// An item of the file menu.
    File(FileAction),
    /// An item of a view's own menu: the view hears it as `View::button(id)`.
    View(u32),
    /// A horizontal rule between sections; not clickable.
    Rule,
}

/// What a context menu's actions apply to.
#[derive(Clone)]
enum MenuTarget {
    /// A workspace in the sidebar.
    Vtab(ghostrealm_core::VtabId),
    /// A file, folder, or empty space in a file browser, or an editor's file.
    File(FileTarget),
    /// The view on this surface, which filed `Request::Menu`.
    View(SurfaceId),
}

/// A context-menu row.
#[derive(Clone)]
struct MenuRow {
    label: String,
    action: MenuAction,
    /// `Some` for a toggle: whether a check mark shows.
    checked: Option<bool>,
    /// Dim text at the right end.
    hint: Option<String>,
    /// A disabled row shows dim and takes no hover or clicks.
    enabled: bool,
}

impl MenuRow {
    fn new(label: impl Into<String>, action: MenuAction) -> Self {
        let enabled = !matches!(action, MenuAction::Rule);
        MenuRow { label: label.into(), action, checked: None, hint: None, enabled }
    }

    /// A view's item; one with an empty label is a rule.
    fn of_view(item: MenuItem) -> Self {
        if item.is_rule() {
            return MenuRow::new("", MenuAction::Rule);
        }
        MenuRow {
            label: item.label,
            action: MenuAction::View(item.id),
            checked: item.checked,
            hint: item.hint,
            enabled: item.enabled,
        }
    }
}

/// A context menu: the workspace or file menu, or a view's own.
struct Menu {
    target: MenuTarget,
    /// Where the menu was opened (physical px); the panel is clamped on-screen.
    anchor: (f32, f32),
    align: MenuAlign,
    items: Vec<MenuRow>,
    /// Item under the cursor, for highlight.
    hover: Option<usize>,
    /// Item the left button went down on; it runs on release over the same item.
    armed: Option<usize>,
    /// Per-item hit rects (physical px) from the last layout; empty for a
    /// rule or a disabled item.
    rows: Vec<Rect>,
    /// The panel rect (physical px), for click-outside dismissal.
    panel: Rect,
    /// The picker was open when the menu opened: the menu belongs to it (or
    /// to what was under it) only while that holds.
    over_picker: bool,
}

/// What a left release on an open menu did.
#[derive(Clone, Copy, Debug, PartialEq)]
enum MenuRelease {
    /// No item was armed: the release isn't the menu's.
    Unarmed,
    /// Released off the armed item: nothing runs, the menu stays.
    Cancelled,
    Chosen(MenuAction),
}

impl Menu {
    fn new(target: MenuTarget, anchor: (f32, f32), align: MenuAlign, items: Vec<MenuRow>, over_picker: bool) -> Self {
        Menu {
            target,
            anchor,
            align,
            items,
            hover: None,
            armed: None,
            rows: Vec::new(),
            panel: Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
            over_picker,
        }
    }

    /// Whether the menu must close: a confirmation is up (the menu would sit
    /// over it and take its clicks), or the picker opened or closed under it
    /// (a `View(PICKER_SID)` menu would act on another picker, or none).
    fn stale(&self, picker_open: bool, confirming: bool) -> bool {
        confirming || self.over_picker != picker_open
    }

    /// Whether any item can be chosen.
    fn choosable(items: &[MenuRow]) -> bool {
        items.iter().any(|i| i.enabled)
    }

    /// Take the hit rects of `layout`, as drawn.
    fn place(&mut self, layout: &MenuLayout) {
        self.rows = layout.rows.iter().map(|r| r.hit).collect();
        self.panel = layout.panel;
    }

    fn item_at(&self, x: f32, y: f32) -> Option<usize> {
        self.rows.iter().position(|r| rect_contains(*r, x, y))
    }

    /// Hover at `(x, y)`; returns whether the highlighted item changed.
    fn hover_at(&mut self, x: f32, y: f32) -> bool {
        let idx = self.item_at(x, y);
        std::mem::replace(&mut self.hover, idx) != idx
    }

    /// A left press at `(x, y)` arms the item under it. Returns `false` when
    /// the press is outside the panel (the menu closes).
    fn press(&mut self, x: f32, y: f32) -> bool {
        self.armed = self.item_at(x, y);
        self.armed.is_some() || rect_contains(self.panel, x, y)
    }

    /// The left button went up at `(x, y)`.
    fn release(&mut self, x: f32, y: f32) -> MenuRelease {
        match self.armed.take() {
            None => MenuRelease::Unarmed,
            Some(i) if self.item_at(x, y) == Some(i) => MenuRelease::Chosen(self.items[i].action),
            Some(_) => MenuRelease::Cancelled,
        }
    }
}

/// Where a menu's parts go (physical px).
struct MenuLayout {
    panel: Rect,
    rows: Vec<RowLayout>,
}

/// One row of a [`MenuLayout`].
struct RowLayout {
    /// The row's band across the panel.
    rect: Rect,
    /// Takes hover and clicks: `rect` for an enabled item, else empty.
    hit: Rect,
    /// Where the label shows (it is clipped to this).
    label: Rect,
    /// Where the hint shows, if it has one and there is room for it.
    hint: Option<Rect>,
    /// The check mark's square, when the item is checked and there is room.
    check: Option<Rect>,
}

/// Fewest label columns a narrow menu keeps before giving up hint space.
const MENU_LABEL_MIN_COLS: f32 = 6.0;

/// Lay out a menu of `items` opened at `anchor` on a `screen`-sized window.
/// `label_w[i]` and `hint_w[i]` are item `i`'s label and hint widths as
/// shaped. The check-mark and hint columns sit at the right end,
/// the hint against the edge and the check mark left of it. On a narrow
/// window the hint is cut, then dropped, before the label shrinks below
/// [`MENU_LABEL_MIN_COLS`].
fn menu_layout(
    items: &[MenuRow],
    label_w: &[f32],
    hint_w: &[f32],
    anchor: (f32, f32),
    align: MenuAlign,
    screen: (f32, f32),
    ui: UiMetrics,
) -> MenuLayout {
    let (sw, sh) = screen;
    let pad = 8.0 * ui.scale;
    let gap = ui.cell_w;
    let row_h = ui.cell_h + 6.0 * ui.scale;
    // A rule takes less height than an item.
    let rule_h = 9.0 * ui.scale;
    let check_w = ui.cell_h * 0.8;
    let right_w = items
        .iter()
        .zip(hint_w)
        .map(|(item, &hw)| match (item.checked.is_some(), item.hint.is_some()) {
            (true, true) => check_w + gap + hw,
            (true, false) => check_w,
            (false, true) => hw,
            (false, false) => 0.0,
        })
        .fold(0.0, f32::max);
    let right_w = if right_w > 0.0 { right_w + gap * 2.0 } else { 0.0 };
    let maxlen = label_w.iter().copied().reduce(f32::max).unwrap_or(6.0 * ui.cell_w);
    let panel_w = (maxlen + pad * 3.0 + right_w).min(sw * 0.6);
    let height_of = |i: &MenuRow| if matches!(i.action, MenuAction::Rule) { rule_h } else { row_h };
    let panel_h = items.iter().map(height_of).sum::<f32>() + pad;
    let x = match align {
        MenuAlign::Left => anchor.0,
        MenuAlign::Right => anchor.0 - panel_w,
    };
    let px = x.min((sw - panel_w).max(0.0)).max(0.0);
    let py = anchor.1.min((sh - panel_h).max(0.0)).max(0.0);
    let panel = Rect { x: px, y: py, w: panel_w, h: panel_h };

    let empty = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
    let (left, right_edge) = (px + pad, (px + panel_w - pad).max(px + pad));
    let mut rows = Vec::with_capacity(items.len());
    let mut y = py + pad * 0.5;
    for ((item, &lw), &hw) in items.iter().zip(label_w).zip(hint_w) {
        let rect = Rect { x: px, y, w: panel_w, h: height_of(item) };
        y += rect.h;
        let band = |x0: f32, x1: f32| Rect { x: x0, y: rect.y, w: (x1 - x0).max(0.0), h: rect.h };
        // What the right end may take, keeping the label its minimum.
        let keep = lw.min(MENU_LABEL_MIN_COLS * ui.cell_w);
        let mut budget = (right_edge - left - keep - gap).max(0.0);
        let mut right = right_edge;
        let reserve_check = item.checked.is_some() && budget >= check_w;
        if reserve_check {
            budget -= check_w + gap;
        }
        // A hint too narrow to read is dropped.
        let hint_space = if item.hint.is_some() { hw.min(budget) } else { 0.0 };
        let hint = (hint_space >= ui.cell_w).then(|| {
            let r = band(right - hint_space, right);
            right -= hint_space + gap;
            r
        });
        let check = reserve_check.then(|| {
            let r = Rect { x: right - check_w, y: rect.y + (rect.h - check_w) * 0.5, w: check_w, h: check_w };
            right -= check_w + gap;
            r
        });
        let check = check.filter(|_| item.checked == Some(true));
        rows.push(RowLayout {
            rect,
            hit: if item.enabled { rect } else { empty },
            label: band(left, right.max(left)),
            hint,
            check,
        });
    }
    MenuLayout { panel, rows }
}

/// Call `f` on surface `sid`'s view (the picker's for [`PICKER_SID`]) with
/// `cx`. Returns whether there was such a view.
fn call_view(app: &mut AppState, sid: SurfaceId, cx: &mut EventCx, f: impl FnOnce(&mut dyn View, &mut EventCx)) -> bool {
    let view: &mut dyn View = if sid == PICKER_SID {
        match app.picker_mut() {
            Some(v) => v,
            None => return false,
        }
    } else {
        match app.view_mut(sid) {
            Some(v) => v,
            None => return false,
        }
    };
    f(view, cx);
    true
}

/// A pane resolved for rendering: its rect, focus, and its surfaces
/// (id, display title, is-active).
struct PaneRender {
    id: PaneId,
    rect: Rect,
    focused: bool,
    surfaces: Vec<(SurfaceId, String, bool)>,
}

/// What a clickable chrome button does. All buttons share hover-highlight and
/// arm-on-press/fire-on-release behaviour (see `arm_button`/`fire_button`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonAction {
    /// Open a new workspace (the sidebar's "+ New workspace").
    NewVtab,
    /// Close a workspace (vtab) from the sidebar.
    CloseVtab(VtabId),
    /// Close one tab (surface) in a pane's horizontal tab strip.
    CloseSurface(VtabId, PaneId, SurfaceId),
    /// Show/hide the workspace sidebar.
    ToggleSidebar,
    /// Open a plugin's content in the given pane (the "nothing open" picker).
    OpenPlugin(VtabId, PaneId, &'static str),
    /// A button a view registered while painting: (owning surface, view-local id).
    View(SurfaceId, u32),
    /// Confirm / cancel the floating picker.
    PickerConfirm,
    PickerCancel,
    /// Accept / cancel the open confirmation.
    ConfirmAccept,
    ConfirmCancel,
    ConfirmDiscard,
}

impl State {
    async fn new(
        window: Arc<Window>,
        command_line: Option<String>,
        proxy: EventLoopProxy<UserEvent>,
        wake_pending: Arc<AtomicBool>,
    ) -> Result<Self> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .context("create surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await
            .context("request adapter")?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .context("request device")?;

        let config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface has no default config")?;
        surface.configure(&device, &config);
        let format = config.format;

        // Fast boot: shape on a curated monospace-only DB loaded from disk, and
        // pull in the full system-font DB (for emoji/CJK/rare-glyph fallback) on a
        // background thread, swapping it in via `FontsLoaded`. Falls back to a
        // synchronous full load if no monospace candidate is found on disk.
        let locale = font_locale();
        let (mut font_system, pending_fonts, mono_family) = match curated_font_db() {
            Some((db, fam)) => {
                let fs = FontSystem::new_with_locale_and_db(locale.clone(), db);
                let (tx, rx) = mpsc::channel();
                let bg_proxy = proxy.clone();
                let spawned = std::thread::Builder::new()
                    .name("font-loader".into())
                    .spawn(move || {
                        let mut db = glyphon::cosmic_text::fontdb::Database::new();
                        db.load_system_fonts();
                        if tx.send(db).is_ok() {
                            let _ = bg_proxy.send_event(UserEvent::FontsLoaded);
                        }
                    })
                    .is_ok();
                (fs, spawned.then_some(rx), Some(fam))
            }
            None => (FontSystem::new(), None, None),
        };
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let palette_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let menu_renderer = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        // The registry and plugins make up the catalogue the config is checked
        // against.
        let registry = build_registry();
        let catalogue = Arc::new(crate::settings::catalogue(&registry));
        let cfg = ghostrealm_core::config::load_or_create(&catalogue).unwrap_or_default();
        let chrome = cfg.resolved_chrome();
        let double_tap_window = Duration::from_millis(cfg.input.double_tap_window_ms as u64);
        let (cell_w, cell_h) = measure_cell(
            &mut font_system,
            scale,
            cfg.terminal.font_size,
            cfg.terminal.line_height,
        );

        // App state: terminals wake the event loop through the proxy. The atomic
        // collapses a burst of chunk-wakes into a single queued event — the loop
        // clears it when it consumes the wake, so we send again only once it has.
        let waker: Arc<dyn Fn() + Send + Sync> = {
            let proxy = proxy.clone();
            Arc::new(move || {
                if !wake_pending.swap(true, Ordering::AcqRel) {
                    let _ = proxy.send_event(UserEvent::PtyOutput);
                }
            })
        };
        let mut app = AppState::new().with_unsaved_guard();
        let has_command = command_line.is_some();
        if let Some(line) = command_line {
            app = app.with_shell_line(line);
        }
        app = app
            .with_waker(waker)
            .with_languages(Arc::new(crate::languages::Languages::new(
                ghostrealm_core::config::config_path(),
                catalogue.clone(),
            )));
        app.set_inbox_config(cfg.inbox);
        app.set_default_dir(cfg.default_dir());
        app.set_close_on_exit(cfg.terminal.close_on_exit);
        // A CLI command spawns a terminal running it; a bare launch opens the empty
        // "nothing open" workspace where the user picks what to open.
        if has_command {
            app.new_vtab().context("open initial tab")?;
        } else {
            app.new_empty_vtab();
        }

        let warm_metrics = Metrics::new(
            cfg.terminal.font_size * scale,
            cfg.terminal.line_height * scale,
        );
        let warm_buffer = Buffer::new(&mut font_system, warm_metrics);
        let close_buffer = Buffer::new(&mut font_system, warm_metrics);
        let toggle_buffer = Buffer::new(&mut font_system, warm_metrics);
        let title_buffer = Buffer::new(&mut font_system, warm_metrics);

        let quad_pipeline = build_quad_pipeline(&device, format);
        let images = ImageRenderer::new(&device, format);
        let quad_capacity = 4096;
        let quad_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-instances"),
            size: quad_capacity * std::mem::size_of::<QuadInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            window,
            instance,
            device,
            queue,
            surface,
            config,
            text: TextKit::new(font_system, ROW_CACHE_CAP),
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            metrics_gen: 0,
            warm_buffer,
            atlas_warmed_gen: None,
            pending_fonts,
            mono_family,
            font_locale: locale,
            palette_renderer,
            menu_renderer,
            palette_buffers: Vec::new(),
            palette: None,
            palette_panel: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            palette_rows: Vec::new(),
            palette_line: None,
            palette_selecting: false,
            sidebar_buffers: Vec::new(),
            strip_buffers: Vec::new(),
            menu: None,
            menu_buffers: Vec::new(),
            sidebar_scroll: 0.0,
            sidebar_max_scroll: 0.0,
            sidebar_rows: Vec::new(),
            buttons: Vec::new(),
            button_hover: None,
            htab_hover: None,
            armed_button: None,
            close_buffer,
            toggle_buffer,
            toggle_place: None,
            title_buffer,
            title_place: None,
            picker_panel: None,
            tooltip_hover: None,
            tooltip_due: None,
            config_watch: ConfigWatch::new(ghostrealm_core::config::config_path()),
            window_focused: true,
            config_check_due: Instant::now() + config_watch::POLL,
            clicks: ClickCounter::default(),
            mouse_capture: None,
            press_button: MouseButton::Left,
            sidebar_hidden: false,
            mouse_down: false,
            dragging: false,
            last_focused_surface: None,
            press_px: (0.0, 0.0),
            clipboard: arboard::Clipboard::new().ok(),
            cursor: (0.0, 0.0),
            quad_pipeline,
            images,
            cursor_icon: CursorIcon::Default,
            cursor_rect: None,
            dividers: Dividers::default(),
            shown: std::collections::HashSet::new(),
            hover: Hover::default(),
            quad_buffer,
            quad_capacity,
            app,
            registry,
            catalogue,
            cfg,
            chrome,
            mods: Mods::default(),
            scale,
            cell_w,
            cell_h,
            dirty: true,
            frame_pending: false,
            next_frame: Instant::now(),
            shift_taps: TapDetector::new(double_tap_window),
            closing_key: HeldKey::default(),
            key_in_flight: None,
            ctrl_taps: TapDetector::new(double_tap_window),
        })
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Re-apply a (possibly changed) config: re-resolve chrome, inbox timings, and
    /// the default directory, and re-measure cell metrics if the font changed
    /// (invalidating the shaping cache; views reflow to the new cell size).
    fn apply_config(&mut self, cfg: Config) {
        let (cw, ch) = measure_cell(
            &mut self.text.font_system,
            self.scale,
            cfg.terminal.font_size,
            cfg.terminal.line_height,
        );
        if (cw - self.cell_w).abs() > 0.01 || (ch - self.cell_h).abs() > 0.01 {
            self.cell_w = cw;
            self.cell_h = ch;
            self.metrics_gen = self.metrics_gen.wrapping_add(1);
        }
        self.chrome = cfg.resolved_chrome();
        self.app.set_inbox_config(cfg.inbox);
        self.app.set_default_dir(cfg.default_dir());
        self.app.set_close_on_exit(cfg.terminal.close_on_exit);
        self.cfg = cfg;
        self.dirty = true;
    }

    /// Reload the config file from disk and apply it (config is the source of
    /// truth). A file that doesn't parse leaves the current config in place.
    fn reload_config(&mut self) {
        self.config_watch.mark_seen();
        if let Some(cfg) = ghostrealm_core::config::load_or_create(&self.catalogue) {
            self.apply_config(cfg);
            self.window.request_redraw();
        }
    }

    /// Reload the config if the file changed outside the app since it was last
    /// loaded, and have open editors check their files: one without unsaved
    /// changes re-reads its file, one with unsaved changes keeps them and is
    /// marked (saving asks first).
    fn check_files(&mut self) {
        self.config_check_due = Instant::now() + config_watch::POLL;
        let catalogue = self.catalogue.clone();
        let mut changed = false;
        let loaded = self.config_watch.poll(|| {
            changed = true;
            ghostrealm_core::config::load_or_create(&catalogue)
        });
        if let Some(cfg) = loaded {
            self.apply_config(cfg);
        }
        if changed | self.app.check_open_files() {
            self.window.request_redraw();
        }
    }

    /// Open the config file in an editor pane (creating it if absent). Saving it
    /// (Cmd+S) hot-reloads the config.
    fn open_config_editor(&mut self) {
        if let Some(path) = ghostrealm_core::config::ensure_file() {
            // Settings live in their own "settings" workspace, not the focused pane.
            if let Err(e) = self.app.open_settings(path.clone()) {
                eprintln!("ghostrealm: open {}: {e:#}", path.display());
            }
            self.dirty = true;
        }
    }

    /// Canonical chord string for the current modifiers + `c` (matches
    /// `ghostrealm_core::config::normalize_chord`'s order).
    fn chord_string(&self, c: char) -> String {
        let mut s = String::new();
        if self.mods.ctrl {
            s.push_str("ctrl+");
        }
        if self.mods.alt {
            s.push_str("alt+");
        }
        if self.mods.shift {
            s.push_str("shift+");
        }
        if self.mods.super_ {
            s.push_str("cmd+");
        }
        s.push(c.to_ascii_lowercase());
        s
    }

    fn metrics(&self) -> Metrics {
        Metrics::new(
            self.cfg.terminal.font_size * self.scale,
            self.cfg.terminal.line_height * self.scale,
        )
    }

    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        let (cw, ch) = measure_cell(
            &mut self.text.font_system,
            scale,
            self.cfg.terminal.font_size,
            self.cfg.terminal.line_height,
        );
        self.cell_w = cw;
        self.cell_h = ch;
        // Metrics changed: existing shaping is stale.
        self.metrics_gen = self.metrics_gen.wrapping_add(1);
        self.dirty = true;
    }

    /// Swap the curated boot font DB for the full system DB once the background
    /// loader delivers it, restoring complete glyph fallback (emoji, CJK, rare
    /// symbols). Re-pins the same monospace family so normal text keeps its
    /// appearance, then invalidates shaping and the warmed atlas so the next
    /// frame reshapes against the fuller DB.
    fn upgrade_fonts(&mut self) {
        let Some(rx) = self.pending_fonts.take() else {
            return;
        };
        let Ok(mut db) = rx.try_recv() else {
            return;
        };
        if let Some(fam) = &self.mono_family {
            db.set_monospace_family(fam.clone());
        }
        self.text.font_system = FontSystem::new_with_locale_and_db(self.font_locale.clone(), db);
        // Cell metrics should be unchanged (same pinned monospace); re-measure in
        // case the fuller DB resolves the family to a different face.
        let (cw, ch) = measure_cell(
            &mut self.text.font_system,
            self.scale,
            self.cfg.terminal.font_size,
            self.cfg.terminal.line_height,
        );
        self.cell_w = cw;
        self.cell_h = ch;
        self.metrics_gen = self.metrics_gen.wrapping_add(1);
        self.atlas_warmed_gen = None;
        self.dirty = true;
        self.window.request_redraw();
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        self.dirty = true;
    }

    /// Height of a pane's horizontal tab strip in physical pixels.
    fn strip_height(&self) -> f32 {
        self.cell_h + 6.0 * self.scale
    }

    /// Whether a pane shows a tab strip: multiple surfaces, or autohide disabled.
    /// (A pane holding one Files surface shows its file in the surface's own
    /// row and header instead.)
    fn strip_shown(&self, n: usize) -> bool {
        n > 1 || !self.cfg.tabs.autohide_single_tab
    }

    /// The active vtab's panes with full surface info, laid out in `workspace`.
    fn active_panes(&self, workspace: Rect) -> Vec<PaneRender> {
        let Some(vt) = self.app.tree.active_vtab() else {
            return Vec::new();
        };
        let Some(vtab) = self.app.tree.vtab(vt) else {
            return Vec::new();
        };
        vtab.layout(workspace, DIVIDER)
            .into_iter()
            .filter_map(|(pid, rect)| {
                let pane = vtab.panes().into_iter().find(|p| p.id == pid)?;
                let surfaces = pane
                    .surfaces
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let title = if !s.title.is_empty() {
                            s.title.clone()
                        } else if self.app.surface_is_empty(s.id) {
                            "New".to_string()
                        } else {
                            "sh".to_string()
                        };
                        (s.id, title, i == pane.active)
                    })
                    .collect();
                Some(PaneRender {
                    id: pid,
                    rect,
                    focused: pid == vtab.focused_pane,
                    surfaces,
                })
            })
            .collect()
    }

    fn sidebar_width(&self) -> f32 {
        if self.sidebar_hidden {
            0.0
        } else {
            self.cfg.sidebar.width * self.scale
        }
    }

    /// Max gap between two clicks for a double-click (from `[input] double_click_ms`).
    fn double_click_window(&self) -> Duration {
        Duration::from_millis(self.cfg.input.double_click_ms as u64)
    }

    /// Open a file view `sid` asked for, in a Files surface.
    fn open_requested_file(&mut self, sid: SurfaceId, path: std::path::PathBuf) {
        if let Err(e) = self.app.open_file_from(sid, &path) {
            eprintln!("ghostrealm: open {}: {e:#}", path.display());
        }
    }

    /// Height of the custom title-bar strip in physical pixels. macOS renders its
    /// own strip (title + sidebar toggle) over a full-size content view; other
    /// platforms keep the native title bar and reserve nothing.
    fn title_bar_h(&self) -> f32 {
        if cfg!(target_os = "macos") {
            TITLE_BAR_H * self.scale
        } else {
            0.0
        }
    }

    /// The sidebar's rect in physical pixels (honours `[sidebar] side`), below the
    /// title-bar strip.
    fn sidebar_rect(&self) -> Rect {
        let top = self.title_bar_h();
        let mut r = sidebar_rect_for(
            self.cfg.sidebar.side,
            self.sidebar_width(),
            self.config.width as f32,
            self.config.height as f32,
        );
        r.y = top;
        r.h = (r.h - top).max(0.0);
        r
    }

    /// The workspace (panes) rect in physical pixels — everything the sidebar and
    /// the title-bar strip don't occupy.
    fn workspace_rect(&self) -> Rect {
        let top = self.title_bar_h();
        let mut r = workspace_rect_for(
            self.cfg.sidebar.side,
            self.sidebar_width(),
            self.config.width as f32,
            self.config.height as f32,
        );
        r.y = top;
        r.h = (r.h - top).max(0.0);
        r
    }

    /// Whether physical x-coordinate `x` falls in the sidebar column.
    fn in_sidebar(&self, x: f32) -> bool {
        let r = self.sidebar_rect();
        x >= r.x && x < r.x + r.w
    }

    /// The workspace vtab under a sidebar point, from the last render's rows.
    fn sidebar_vtab_at(&self, x: f32, y: f32) -> Option<ghostrealm_core::VtabId> {
        self.sidebar_rows
            .iter()
            .find(|(r, _)| rect_contains(*r, x, y))
            .map(|(_, id)| *id)
    }

    /// The chrome button under the cursor, if any. The sidebar toggle wins any
    /// overlap (it can float over a pane's tab strip when the sidebar is hidden);
    /// otherwise the first button in paint order takes the hit.
    fn button_at_cursor(&self) -> Option<ButtonAction> {
        let (x, y) = self.cursor;
        // Overlays are modal: the palette and menu own the pointer, and while the
        // picker is open only its own buttons are live.
        if self.palette.is_some() || self.menu.is_some() {
            return None;
        }
        let picker = self.app.picker_open();
        let confirm = self.app.confirm().is_some();
        let mut hit = None;
        for &(r, action) in &self.buttons {
            let modal = matches!(
                action,
                ButtonAction::PickerConfirm | ButtonAction::PickerCancel | ButtonAction::View(PICKER_SID, _)
            );
            let asking = matches!(
                action,
                ButtonAction::ConfirmAccept | ButtonAction::ConfirmCancel | ButtonAction::ConfirmDiscard
            );
            if (confirm && !asking) || (picker && !confirm && !modal) {
                continue;
            }
            if rect_contains(r, x, y) {
                if action == ButtonAction::ToggleSidebar {
                    return Some(action);
                }
                hit.get_or_insert(action);
            }
        }
        hit
    }

    /// Mouse-down over a button arms it (fires later on release), so a press can
    /// be cancelled by moving away before releasing. Returns whether one was armed
    /// (the caller then suppresses selection/focus for this press).
    fn arm_button(&mut self) -> bool {
        self.armed_button = self.button_at_cursor();
        self.armed_button.is_some()
    }

    /// Mouse-up: run the armed button only if the cursor is still over the same
    /// one. Returns whether it fired.
    fn fire_button(&mut self) -> bool {
        let Some(armed) = self.armed_button.take() else {
            return false;
        };
        if self.button_at_cursor() != Some(armed) {
            return false; // released off the button — cancelled
        }
        match armed {
            ButtonAction::NewVtab => self.app.open_picker(Pick::NewWorkspace),
            ButtonAction::CloseVtab(id) => self.app.close_vtab(id),
            ButtonAction::CloseSurface(vt, pid, sid) => self.app.close_surface(vt, pid, sid),
            ButtonAction::ToggleSidebar => self.sidebar_hidden = !self.sidebar_hidden,
            ButtonAction::OpenPlugin(vt, pane, id) => {
                if let Err(e) = self.app.open_plugin_in_pane(vt, pane, id) {
                    eprintln!("ghostrealm: open {id}: {e:#}");
                }
            }
            ButtonAction::View(sid, id) => {
                self.view_event(sid, |v, cx| v.button(cx, id));
            }
            ButtonAction::PickerConfirm => self.confirm_picker(None),
            ButtonAction::PickerCancel => self.app.close_picker(),
            ButtonAction::ConfirmAccept => self.accept_confirm(),
            ButtonAction::ConfirmCancel => self.app.cancel_confirm(),
            ButtonAction::ConfirmDiscard => self.discard_confirm(),
        }
        self.dirty = true;
        true
    }

    /// Recompute which button is under the cursor; returns whether it changed, so
    /// the caller redraws only when the hover highlight must move.
    fn update_button_hover(&mut self) -> bool {
        let (x, y) = self.cursor;
        let now = self
            .buttons
            .iter()
            .position(|(r, _)| rect_contains(*r, x, y));
        if now != self.button_hover {
            self.button_hover = now;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Deliver hover to the view under the cursor (the picker's, while it is
    /// open). Returns whether it asked for a redraw.
    fn hover_view(&mut self) -> bool {
        if self.mouse_down {
            return false;
        }
        let pos = self.cursor;
        let may_hover = self.may_hover();
        let target = if self.app.picker_open() {
            Some(PICKER_SID)
        } else {
            self.surface_under_cursor()
                .filter(|s| self.app.view(*s).is_some())
        }
        .filter(|s| may_hover(*s));
        let left = self.hover.set(target);
        let left = self.send_leave(left);
        let moved = match target {
            Some(sid) => self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Move { pos })),
            None => false,
        };
        left | moved
    }

    /// Which views may have hover: none while the palette, a menu, or a
    /// confirmation owns the pointer; only the picker's while it is open; and
    /// only views painted last frame (a view on a hidden tab or workspace is
    /// not under the pointer).
    fn may_hover(&self) -> impl Fn(SurfaceId) -> bool {
        let blocked = self.palette.is_some() || self.menu.is_some() || self.app.confirm().is_some();
        let picker = self.app.picker_open();
        let shown = self.shown.clone();
        move |sid| !blocked && (!picker || sid == PICKER_SID) && (sid == PICKER_SID || shown.contains(&sid))
    }

    /// Tell `sid` (a view that lost hover) that the pointer left it. Returns
    /// whether it asked to redraw.
    fn send_leave(&mut self, sid: Option<SurfaceId>) -> bool {
        let Some(sid) = sid else {
            return false;
        };
        let pos = self.cursor;
        self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Leave { pos }))
    }

    /// A key release: offered raw to the focused view when nothing modal owns
    /// the keyboard. Returns whether it asked for a redraw.
    fn on_key_release(&mut self, event: &winit::event::KeyEvent) -> bool {
        if self.app.confirm().is_some() || self.app.picker_open() || self.palette.is_some() {
            return false;
        }
        let Some(sid) = self.app.focused_surface().filter(|s| self.app.view(*s).is_some()) else {
            return false;
        };
        let raw = RawKey { event, press: None };
        self.view_event(sid, |v, cx| {
            v.raw_key(cx, &raw);
        })
    }

    /// Where the dividers are laid out and how small a drag may make a pane.
    fn divider_frame(&self) -> dividers::Frame {
        dividers::Frame {
            workspace: self.workspace_rect(),
            gap: DIVIDER,
            slop: DIVIDER_SLOP * self.scale,
            cell: (self.cell_w, self.cell_h),
            strip_h: self.strip_height(),
            strip_for_one: self.strip_shown(1),
        }
    }

    /// The split divider under the cursor, unless something covers it: an
    /// overlay, the sidebar, the title bar, or a button.
    fn divider_under_cursor(&self) -> Option<(VtabId, Divider)> {
        let (x, y) = self.cursor;
        let covered = self.palette.is_some()
            || self.menu.is_some()
            || self.app.picker_open()
            || self.app.confirm().is_some()
            || self.in_sidebar(x)
            || y < self.title_bar_h()
            || self.button_at_cursor().is_some();
        if covered {
            return None;
        }
        dividers::under(&self.app.tree, &self.divider_frame(), self.cursor)
    }

    /// Track which divider the pointer is over. Returns whether it changed.
    fn update_divider_hover(&mut self) -> bool {
        let over = self.divider_under_cursor();
        if !self.dividers.hover(over.as_ref()) {
            return false;
        }
        if let Some((_, d)) = over {
            self.set_cursor_icon(divider_icon(d.axis));
            self.cursor_rect = Some(d.grab_rect(DIVIDER_SLOP * self.scale));
        }
        self.dirty = true;
        true
    }

    /// A left press: grab the divider under the cursor to drag it, or reset
    /// its split to halves on a double-click. Returns whether it took the press.
    fn press_divider(&mut self) -> bool {
        let Some(hit) = self.divider_under_cursor() else {
            self.dividers.missed();
            return false;
        };
        // Not part of a pane's multi-click; dividers count their own.
        self.clicks.reset();
        let (click, frame) = (self.click(), self.divider_frame());
        if self.dividers.press(&mut self.app.tree, &frame, hit, &click) == Press::Grabbed {
            self.begin_press(MouseButton::Left);
        }
        self.dirty = true;
        true
    }

    /// The pointer moved with a divider grabbed: resize its split. Returns
    /// whether to redraw.
    fn drag_divider(&mut self) -> bool {
        let frame = self.divider_frame();
        self.dirty = true;
        self.dividers.drag(&mut self.app.tree, &frame, self.cursor)
    }

    fn set_cursor_icon(&mut self, icon: CursorIcon) {
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            self.window.set_cursor(icon);
        }
    }

    /// Tell views that came on or went off screen this frame.
    fn update_shown(&mut self, painted: &[SurfaceId]) {
        let now: std::collections::HashSet<SurfaceId> = painted.iter().copied().collect();
        let gone: Vec<SurfaceId> = self.shown.difference(&now).copied().collect();
        let new: Vec<SurfaceId> = now.difference(&self.shown).copied().collect();
        for sid in gone {
            if let Some(v) = self.app.view_mut(sid) {
                v.shown(false);
            }
        }
        for sid in new {
            if let Some(v) = self.app.view_mut(sid) {
                v.shown(true);
            }
        }
        self.shown = now;
    }

    /// Cell geometry and scale for view contexts.
    fn ui(&self) -> UiMetrics {
        UiMetrics {
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            scale: self.scale,
        }
    }

    /// Call `f` on surface `sid`'s view (the picker's for [`PICKER_SID`]) with an
    /// event context, then apply what it asked for. Returns whether it asked for
    /// a redraw (`false` when there is no such view).
    fn view_event(&mut self, sid: SurfaceId, f: impl FnOnce(&mut dyn View, &mut EventCx)) -> bool {
        let ui = self.ui();
        let mut cx = EventCx::new(&self.cfg, ui, self.mods, self.clipboard.as_mut());
        if !call_view(&mut self.app, sid, &mut cx, f) {
            return false;
        }
        let out = cx.finish();
        let redraw = out.redraw;
        self.apply_outcome(sid, out);
        redraw
    }

    /// Apply a view's repaint flags and requests.
    fn apply_outcome(&mut self, sid: SurfaceId, out: Outcome) {
        if out.redraw {
            self.dirty = true;
        }
        if out.frame {
            self.frame_pending = true;
        }
        for req in out.requests {
            match req {
                Request::OpenFile(path) => self.open_requested_file(sid, path),
                Request::Saved(path) => self.saved(path),
                Request::Pick(path) if sid == PICKER_SID => self.confirm_picker(Some(path)),
                Request::Pick(_) => {}
                Request::SaveAs => self.app.open_save_as(sid),
                Request::SaveConflict => self.app.ask_disk_conflict(sid),
                Request::Busy => self.app.mark_busy(sid),
                Request::Close => {
                    if let Some(key) = self.key_in_flight {
                        self.closing_key.hold(key);
                    }
                    self.app.close_surface_id(sid);
                }
                Request::CloseDoc(doc) => self.app.close_doc_of(sid, doc),
                Request::FileMenu { target, at, align } => self.open_file_menu(target, at, align),
                Request::Menu { items, at, align } => self.open_view_menu(sid, items, at, align),
            }
        }
    }

    /// A view wrote `path`: hot-reload it if it is the config.
    fn saved(&mut self, path: std::path::PathBuf) {
        let config = ghostrealm_core::config::config_path();
        if config.is_some_and(|c| crate::languages::same_file(&c, &path)) {
            self.reload_config();
        }
    }

    /// Confirm the picker with `path`, else with its browser's choice.
    fn confirm_picker(&mut self, path: Option<std::path::PathBuf>) {
        let done = match path {
            Some(p) => self.app.confirm_picker_with(p),
            None => self.app.confirm_picker(),
        };
        self.after_pick(done);
    }

    /// Go ahead with the open confirmation's action.
    fn accept_confirm(&mut self) {
        let done = self.app.accept_confirm();
        self.after_confirm(done);
    }

    /// Follow up an answered confirmation: hot-reload a written config, report
    /// a failed save, and exit if it was a quit.
    fn after_confirm(&mut self, done: Result<Vec<std::path::PathBuf>>) {
        match done {
            Ok(written) => written.into_iter().for_each(|p| self.saved(p)),
            Err(e) => eprintln!("ghostrealm: save: {e:#}"),
        }
        self.dirty = true;
        self.exit_if_quitting();
    }

    /// Go ahead with the open confirmation's action without saving.
    fn discard_confirm(&mut self) {
        match self.app.discard_confirm() {
            Ok(written) => written.into_iter().for_each(|p| self.saved(p)),
            Err(e) => eprintln!("ghostrealm: {e:#}"),
        }
        self.dirty = true;
        self.exit_if_quitting();
    }

    /// Ask to quit: straight away when nothing is unsaved, else after the
    /// confirmation. Editors autosave first, as their setting says; the
    /// confirmation covers what that left unsaved.
    fn request_quit(&mut self) {
        if self.app.confirm().is_some() {
            return;
        }
        if self.app.autosave_all(&self.cfg) {
            self.reload_config();
        }
        // Palette and menus swallow the confirmation's buttons.
        self.palette = None;
        self.menu = None;
        self.app.request_quit();
        self.dirty = true;
        self.exit_if_quitting();
    }

    /// Exit the process once a quit has been agreed. Hides the window first
    /// for instant visual feedback, then exits outright: graceful teardown
    /// (the wgpu/Metal device drop) is synchronous and can stall for seconds,
    /// sometimes long enough that macOS marks the app unresponsive. Exiting
    /// lets the OS reclaim threads and fds; the closing PTY masters SIGHUP the
    /// child shells, exactly as closing any terminal does. A copy or move
    /// still running was asked about before the quit was agreed; the copies
    /// and moves the file worker finished are reported first.
    fn exit_if_quitting(&mut self) {
        if self.app.quitting() {
            self.window.set_visible(false);
            self.app.poll_file_jobs_on_exit();
            self.print_file_reports();
            std::process::exit(0);
        }
    }

    /// Print file operations that failed after their command returned, or
    /// went ahead only in part.
    fn print_file_reports(&mut self) {
        for report in self.app.take_file_reports() {
            match report.result {
                Err(e) => eprintln!("ghostrealm: {}: {e}", report.command),
                Ok(m) if report.warning => eprintln!("ghostrealm: {}: {m}", report.command),
                Ok(_) => {}
            }
        }
    }

    /// Follow up a picker action: hot-reload a written config, report failure.
    fn after_pick(&mut self, done: Result<Option<std::path::PathBuf>>) {
        match done {
            Ok(Some(written)) => self.saved(written),
            Ok(None) => {}
            Err(e) => eprintln!("ghostrealm: picker: {e:#}"),
        }
        self.dirty = true;
    }

    /// A left press at the cursor now, for counting multi-clicks
    /// (`[input] double_click_ms`).
    fn click(&self) -> Click {
        Click {
            button: MouseButton::Left,
            at: Instant::now(),
            pos: self.cursor,
            window: self.double_click_window(),
            slop: 4.0 * self.scale,
        }
    }

    /// Count this press of `button` as the next of a multi-click when it
    /// follows the last one quickly at about the same spot.
    fn count_click(&mut self, button: MouseButton) -> u32 {
        let c = Click { button, ..self.click() };
        self.clicks.count(&c)
    }

    /// A release can be lost (the window lost focus mid-drag, say). Before a
    /// new press starts, the view still holding the old one hears its `Up` and
    /// a divider still grabbed is let go, so neither keeps acting on a button
    /// no longer held.
    fn release_lost_press(&mut self) {
        if let Some(sid) = self.mouse_capture.take() {
            let (pos, dragged, button) = (self.cursor, self.dragging, self.press_button);
            self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Up { pos, dragged, button }));
        }
        if self.dividers.release() {
            self.dirty = true;
        }
    }

    fn on_click(&mut self) {
        // A confirmation takes only its buttons (they arm on press).
        if self.app.confirm().is_some() {
            return;
        }
        // A menu takes the click first: one may be open over the picker.
        if self.menu.is_some() {
            self.menu_click();
            return;
        }
        // The picker is modal: a click in its panel goes to its file
        // browser, a click outside cancels. (Its buttons arm on press.)
        if self.app.picker_open() {
            let pos = self.cursor;
            match self.picker_panel {
                Some(panel) if rect_contains(panel, pos.0, pos.1) => {
                    let clicks = self.count_click(MouseButton::Left);
                    self.view_event(PICKER_SID, |v, cx| {
                        v.mouse(cx, &MouseEvent::Down { pos, clicks, button: MouseButton::Left })
                    });
                    self.mouse_capture = Some(PICKER_SID);
                }
                Some(_) => self.app.close_picker(),
                None => {}
            }
            self.dirty = true;
            return;
        }
        if self.palette.is_some() {
            self.palette_click();
            return;
        }
        let (x, y) = self.cursor;
        if self.in_sidebar(x) {
            // The "+ New workspace" button is a chrome button (arm/fire on
            // release); here we only handle selecting a workspace row.
            if let Some(id) = self.sidebar_vtab_at(x, y) {
                self.app.focus_vtab(id);
                self.dirty = true;
            }
            return;
        }
        let Some(PaneHit { vt, pid, active_sid, .. }) = self.pane_under_cursor() else {
            return;
        };

        // A click in a visible tab strip switches the pane's active surface.
        if let Some((_, idx)) = self.htab_under_cursor() {
            if let Some(pane) = self.app.tree.vtab_mut(vt).and_then(|v| v.pane_mut(pid)) {
                pane.active = idx;
            }
        } else if let Some(sid) = active_sid.filter(|s| self.app.view(*s).is_some()) {
            // Focus first (so anything the click opens targets this pane), then
            // hand the press to the view; it owns the rest of the press.
            if let Some(vtab) = self.app.tree.vtab_mut(vt) {
                vtab.focused_pane = pid;
            }
            let clicks = self.count_click(MouseButton::Left);
            let pos = (x, y);
            self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Down { pos, clicks, button: MouseButton::Left }));
            self.mouse_capture = Some(sid);
            self.dirty = true;
            return;
        }
        if let Some(vtab) = self.app.tree.vtab_mut(vt) {
            vtab.focused_pane = pid;
        }
        self.dirty = true;
    }

    /// The pane under the cursor in the active workspace (not over the
    /// sidebar).
    fn pane_under_cursor(&self) -> Option<PaneHit> {
        let (x, y) = self.cursor;
        if self.in_sidebar(x) {
            return None;
        }
        let vt = self.app.tree.active_vtab()?;
        let vtab = self.app.tree.vtab(vt)?;
        let (pid, rect) = vtab
            .layout(self.workspace_rect(), DIVIDER)
            .into_iter()
            .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)?;
        let pane = vtab.panes().into_iter().find(|p| p.id == pid)?;
        Some(PaneHit {
            vt,
            pid,
            rect,
            n: pane.surfaces.len(),
            active_sid: pane.active_surface().map(|s| s.id),
        })
    }

    /// The pane tab under the cursor: its pane and index.
    fn htab_under_cursor(&self) -> Option<(PaneId, usize)> {
        let PaneHit { pid, rect, n, .. } = self.pane_under_cursor()?;
        if !self.in_strip(rect, n, self.cursor.1) {
            return None;
        }
        let tab_w = rect.w / n.max(1) as f32;
        Some((pid, (((self.cursor.0 - rect.x) / tab_w).floor() as usize).min(n - 1)))
    }

    /// Note the pane tab under the cursor; returns whether it changed (its
    /// close slot then repaints).
    fn update_htab_hover(&mut self) -> bool {
        let now = self.htab_under_cursor();
        let changed = now != self.htab_hover;
        self.htab_hover = now;
        changed
    }

    /// Whether `y` is in the tab strip of a pane at `rect` holding `n`
    /// surfaces (false when the pane shows no strip).
    fn in_strip(&self, rect: Rect, n: usize, y: f32) -> bool {
        self.strip_shown(n) && y < rect.y + self.strip_height()
    }

    /// A right or middle press: the view whose content is under the cursor
    /// takes it, its pane focused first, as with a left press. Returns
    /// whether a view took it.
    fn press_view(&mut self, button: MouseButton) -> bool {
        if self.app.confirm().is_some() || self.app.picker_open() || self.palette.is_some() || self.menu.is_some() {
            return false;
        }
        let pos = self.cursor;
        let Some(PaneHit { vt, pid, rect, n, active_sid }) = self.pane_under_cursor() else {
            return false;
        };
        let Some(sid) = active_sid.filter(|s| !self.in_strip(rect, n, pos.1) && self.app.view(*s).is_some()) else {
            return false;
        };
        if let Some(vtab) = self.app.tree.vtab_mut(vt) {
            vtab.focused_pane = pid;
        }
        let clicks = self.count_click(button);
        self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Down { pos, clicks, button }));
        self.mouse_capture = Some(sid);
        self.begin_press(button);
        self.dirty = true;
        true
    }

    /// Right-click opens a context menu over the sidebar vtab under the cursor.
    fn on_right_click(&mut self) {
        // Modal overlays keep the pointer; a context menu would open beneath them.
        if self.app.picker_open() {
            return;
        }
        self.palette = None;
        let (x, y) = self.cursor;
        self.menu = None;
        if !self.in_sidebar(x) {
            // The view hears the press itself (a web page's own menu), then
            // is asked for its context menu.
            self.press_view(MouseButton::Right);
            if let Some(sid) = self.surface_under_cursor().filter(|s| self.app.view(*s).is_some()) {
                self.view_event(sid, |v, cx| v.context_menu(cx, (x, y)));
            }
            self.dirty = true;
            return;
        }
        let Some(target) = self.sidebar_vtab_at(x, y) else {
            self.dirty = true;
            return;
        };
        let status = self.app.tree.vtab(target).map(|v| v.status);
        let mut items = Vec::new();
        match status {
            Some(TabStatus::Unread { .. }) => items.push(MenuRow::new("Mark read", MenuAction::MarkRead)),
            Some(TabStatus::NeedsInput) => items.push(MenuRow::new("Dismiss", MenuAction::Dismiss)),
            _ => items.push(MenuRow::new("Mark unread", MenuAction::MarkUnread)),
        }
        items.push(MenuRow::new("Rename…", MenuAction::Rename));
        items.push(MenuRow::new("Set directory…", MenuAction::SetDir));
        items.push(MenuRow::new("Close workspace", MenuAction::Close));
        let over_picker = self.app.picker_open();
        self.menu = Some(Menu::new(MenuTarget::Vtab(target), (x, y), MenuAlign::Left, items, over_picker));
        self.dirty = true;
    }

    /// Close the menu if what it opened over changed (see [`Menu::stale`]).
    fn drop_stale_menu(&mut self) {
        let (picker, confirming) = (self.app.picker_open(), self.app.confirm().is_some());
        if self.menu.as_ref().is_some_and(|m| m.stale(picker, confirming)) {
            self.menu = None;
            self.dirty = true;
        }
    }

    /// A left press while the context menu is open: arm the item under it (it
    /// runs on release), or close the menu when the press is outside it.
    fn menu_click(&mut self) {
        let (x, y) = self.cursor;
        if self.menu.as_mut().is_some_and(|m| !m.press(x, y)) {
            self.menu = None;
        }
        self.dirty = true;
    }

    /// The left button went up: run the armed menu item if the cursor is still
    /// over it. Returns whether the menu handled the release.
    fn fire_menu(&mut self) -> bool {
        let (x, y) = self.cursor;
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        match menu.release(x, y) {
            MenuRelease::Unarmed => false,
            MenuRelease::Cancelled => true,
            MenuRelease::Chosen(action) => {
                let target = menu.target.clone();
                self.menu = None;
                self.apply_menu_action(action, target);
                self.dirty = true;
                true
            }
        }
    }

    /// Highlight the menu item under the cursor; returns whether it changed.
    fn menu_hover(&mut self) -> bool {
        let (x, y) = self.cursor;
        let changed = self.menu.as_mut().is_some_and(|m| m.hover_at(x, y));
        self.dirty |= changed;
        changed
    }

    fn apply_menu_action(&mut self, action: MenuAction, target: MenuTarget) {
        let target = match (action, target) {
            (MenuAction::File(action), MenuTarget::File(target)) => {
                self.run_file_action(action, &target);
                return;
            }
            (MenuAction::View(id), MenuTarget::View(sid)) => {
                self.view_event(sid, |v, cx| v.button(cx, id));
                return;
            }
            (_, MenuTarget::Vtab(target)) => target,
            _ => return,
        };
        match action {
            MenuAction::MarkRead | MenuAction::Dismiss => {
                self.app.tree.set_status(target, TabStatus::Read)
            }
            MenuAction::MarkUnread => self
                .app
                .tree
                .set_status(target, TabStatus::Unread { success: true }),
            MenuAction::Close => self.app.close_vtab(target),
            // Rename / Set directory act on the active workspace, so focus the
            // target first, then open the argument prompt for the command.
            MenuAction::Rename => {
                self.app.focus_vtab(target);
                self.begin_command("workspace.rename");
            }
            MenuAction::SetDir => {
                self.app.focus_vtab(target);
                self.app.open_picker(Pick::ActiveRoot);
                self.dirty = true;
            }
            MenuAction::File(_) | MenuAction::View(_) | MenuAction::Rule => {}
        }
    }

    /// Open the file menu on `target` at `at`.
    fn open_file_menu(&mut self, target: FileTarget, at: (f32, f32), align: MenuAlign) {
        if self.app.picker_open() {
            return;
        }
        let items = self
            .app
            .file_menu(&target)
            .into_iter()
            .map(|e| match e {
                MenuEntry::Item(a) => MenuRow::new(a.label(), MenuAction::File(a)),
                MenuEntry::Rule => MenuRow::new("", MenuAction::Rule),
            })
            .collect();
        self.palette = None;
        let over_picker = self.app.picker_open();
        self.menu = Some(Menu::new(MenuTarget::File(target), at, align, items, over_picker));
        self.dirty = true;
    }

    /// Open surface `sid`'s own menu of `items` at `at` (the picker's view's
    /// too: the menu draws above the picker).
    fn open_view_menu(&mut self, sid: SurfaceId, items: Vec<MenuItem>, at: (f32, f32), align: MenuAlign) {
        let items: Vec<MenuRow> = items.into_iter().map(MenuRow::of_view).collect();
        if !Menu::choosable(&items) {
            return;
        }
        self.palette = None;
        let over_picker = self.app.picker_open();
        self.menu = Some(Menu::new(MenuTarget::View(sid), at, align, items, over_picker));
        self.dirty = true;
    }

    /// Run a file menu item as its command on `target`; one that needs a name
    /// asks for it in the palette's argument prompt.
    fn run_file_action(&mut self, action: FileAction, target: &FileTarget) {
        let prefill = (action == FileAction::Rename)
            .then(|| target.path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .flatten();
        self.begin_command_with(action.command(), action.args(target), prefill);
    }

    /// A click while the palette is open: run a clicked result, or dismiss when
    /// the click lands outside the panel.
    fn palette_click(&mut self) {
        if self.palette_press() {
            return;
        }
        let (x, y) = self.cursor;
        if let Some(id) = self
            .palette_rows
            .iter()
            .find(|(r, _, _)| rect_contains(*r, x, y))
            .map(|(_, _, id)| id.clone())
        {
            self.palette = None;
            let _ = self.registry.execute(&id, &Args::new(), &mut self.app);
            self.dirty = true;
            return;
        }
        if !rect_contains(self.palette_panel, x, y) {
            self.palette = None;
        }
        self.dirty = true;
    }

    /// The palette's input line across the panel, as laid out this frame.
    fn palette_line_rect(&self) -> Rect {
        let p = self.palette_panel;
        Rect { x: p.x, y: p.y + 10.0 * self.scale, w: p.w, h: self.cell_h }
    }

    /// The caret column nearest `x` on the palette's input line, and the column
    /// of the character under it.
    fn palette_cols(&self, x: f32) -> Option<(usize, usize)> {
        let (_, left, prefix) = self.palette_line?;
        let buf = self.palette_buffers.first()?;
        let input = &self.palette.as_ref()?.input;
        let edge = |col: usize| left + glyph_x(buf, prefix + input.byte_of(col));
        let len = input.text().chars().count();
        Some((nearest_caret(len, x, edge), char_under(len, x, edge)))
    }

    /// A press on the palette's input line selects text there; whether it
    /// began on the line.
    fn palette_press(&mut self) -> bool {
        // A release lost to a focus switch mid-drag never ended the last one.
        self.palette_release();
        let (x, y) = self.cursor;
        let on_line = self.palette_line.is_some_and(|(r, ..)| rect_contains(r, x, y));
        if !on_line {
            return false;
        }
        let clicks = self.count_click(MouseButton::Left);
        if let (Some((caret, over)), Some(p)) = (self.palette_cols(x), self.palette.as_mut()) {
            p.input.press(caret, over, clicks, self.mods.shift);
            self.palette_selecting = true;
        }
        self.dirty = true;
        true
    }

    /// The pointer moved with the button held from the palette's input line.
    fn palette_drag(&mut self) -> bool {
        let Some((caret, _)) = self.palette_cols(self.cursor.0) else { return false };
        match self.palette.as_mut() {
            Some(p) if p.input.cursor() != caret => {
                p.input.drag_to(caret);
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    /// The button went up: end a selection begun on the palette's input line.
    fn palette_release(&mut self) {
        if std::mem::take(&mut self.palette_selecting) {
            if let Some(p) = self.palette.as_mut() {
                p.input.release();
            }
            self.dirty = true;
        }
    }

    /// Hover over a palette result highlights it. Returns whether the selection
    /// moved (so the caller can redraw only on change).
    fn palette_hover(&mut self) -> bool {
        let (x, y) = self.cursor;
        let Some(sel) = self
            .palette_rows
            .iter()
            .find(|(r, _, _)| rect_contains(*r, x, y))
            .map(|(_, idx, _)| *idx)
        else {
            return false;
        };
        match self.palette.as_mut() {
            Some(p) if p.selected != sel => {
                p.selected = sel;
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    /// The active surface of the pane under the cursor, if the cursor is over the
    /// workspace (not the sidebar).
    fn surface_under_cursor(&self) -> Option<SurfaceId> {
        let (x, y) = self.cursor;
        if self.in_sidebar(x) {
            return None;
        }
        let workspace = self.workspace_rect();
        let vt = self.app.tree.active_vtab()?;
        let vtab = self.app.tree.vtab(vt)?;
        for (pid, r) in vtab.layout(workspace, DIVIDER) {
            if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
                let pane = vtab.panes().into_iter().find(|p| p.id == pid)?;
                return pane.active_surface().map(|s| s.id);
            }
        }
        None
    }

    /// Move keyboard focus to the pane under the cursor (focus-follows-mouse).
    /// Returns whether the focused pane changed.
    fn focus_pane_under_cursor(&mut self) -> bool {
        let (x, y) = self.cursor;
        if self.in_sidebar(x) {
            return false;
        }
        let workspace = self.workspace_rect();
        let Some(vt) = self.app.tree.active_vtab() else {
            return false;
        };
        let hit = self.app.tree.vtab(vt).and_then(|vtab| {
            vtab.layout(workspace, DIVIDER)
                .into_iter()
                .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
                .map(|(pid, _)| pid)
        });
        let Some(pid) = hit else { return false };
        match self.app.tree.vtab_mut(vt) {
            Some(vtab) if vtab.focused_pane != pid => {
                vtab.focused_pane = pid;
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    /// Every view with timers: the panes' views and the floating picker's.
    fn timed_views(&self) -> impl Iterator<Item = (SurfaceId, &dyn View)> {
        let picker = self.app.picker().map(|v| (PICKER_SID, v as &dyn View));
        self.app.views().chain(picker)
    }

    /// The earliest timer any view has pending.
    fn next_view_deadline(&self) -> Option<Instant> {
        self.timed_views()
            .filter_map(|(_, v)| v.deadline(&self.cfg))
            .min()
    }

    /// Run the timers of views whose deadline has passed. Returns whether any
    /// fired (the caller redraws).
    fn fire_view_timers(&mut self) -> bool {
        let now = Instant::now();
        let due: Vec<SurfaceId> = self
            .timed_views()
            .filter(|(_, v)| v.deadline(&self.cfg).is_some_and(|d| now >= d))
            .map(|(sid, _)| sid)
            .collect();
        for sid in &due {
            self.view_event(*sid, |v, cx| v.tick(cx, now));
        }
        !due.is_empty()
    }

    /// Start tracking a press of `button` (for the drag threshold and
    /// release). No-op while an overlay owns input.
    fn begin_press(&mut self, button: MouseButton) {
        if self.palette.is_some() || self.menu.is_some() {
            return;
        }
        self.dragging = false;
        self.mouse_down = true;
        self.press_button = button;
        self.press_px = self.cursor;
    }

    /// The cursor moved with the button held: once past the drag threshold, the
    /// view that took the press gets the drag. Returns whether to redraw.
    fn update_drag(&mut self) -> bool {
        if !self.mouse_down {
            return false;
        }
        let pos = self.cursor;
        // Ignore sub-pixel jitter until it's clearly a drag.
        if !self.dragging {
            let (dx, dy) = (pos.0 - self.press_px.0, pos.1 - self.press_px.1);
            if dx * dx + dy * dy < 9.0 {
                return false;
            }
            self.dragging = true;
        }
        let button = self.press_button;
        match self.mouse_capture {
            Some(sid) => self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Drag { pos, button })),
            None => false,
        }
    }

    /// `button` was released: finish the press, if it was that button's (the
    /// view that took it gets the release).
    fn end_press(&mut self, button: MouseButton) {
        if self.mouse_down && button != self.press_button {
            return;
        }
        self.mouse_down = false;
        if self.dividers.release() {
            self.dirty = true;
        }
        let dragged = std::mem::take(&mut self.dragging);
        if let Some(sid) = self.mouse_capture.take() {
            let pos = self.cursor;
            self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Up { pos, dragged, button }));
        }
        let left = self.hover.released();
        self.send_leave(left);
    }

    /// Scroll the scrollback of the pane under the cursor (or the focused pane).
    fn on_scroll(&mut self, delta: MouseScrollDelta) {
        if self.palette.is_some() {
            return;
        }
        // Normalise both event kinds to physical pixels. A line/notch wheel is
        // worth SCROLL_LINES_PER_NOTCH cells; a trackpad reports pixels directly.
        let (px, px_x) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (
                y * SCROLL_LINES_PER_NOTCH * self.cell_h,
                x * SCROLL_LINES_PER_NOTCH * self.cell_w,
            ),
            MouseScrollDelta::PixelDelta(p) => (p.y as f32, p.x as f32),
        };
        if px == 0.0 && px_x == 0.0 {
            return;
        }
        let pos = self.cursor;
        if self.app.confirm().is_some() {
            return;
        }
        // The picker is modal: the wheel scrolls its list.
        if self.app.picker_open() {
            self.view_event(PICKER_SID, |v, cx| v.scroll(cx, pos, px_x, px));
            return;
        }
        // Over the sidebar, the wheel scrolls the workspace list (pixel-precise, so
        // it tracks the trackpad exactly).
        if self.in_sidebar(self.cursor.0) {
            let before = self.sidebar_scroll;
            self.sidebar_scroll = (self.sidebar_scroll - px).clamp(0.0, self.sidebar_max_scroll);
            if self.sidebar_scroll != before {
                self.dirty = true;
                self.frame_pending = true;
            }
            return;
        }
        // A view takes the wheel: the one under the cursor, else the focused one.
        let target = self.surface_under_cursor().or_else(|| self.app.focused_surface());
        if let Some(sid) = target {
            self.view_event(sid, |v, cx| v.scroll(cx, pos, px_x, px));
        }
    }

    /// Shape the sidebar-toggle chevron (at `icon_px`) into `rect`, record its
    /// placement + hit rect, and draw its hover highlight over the whole `rect`.
    /// With `chip_bg`, first paint an opaque sidebar-coloured backing (for a toggle
    /// that floats with nothing behind it). The chevron points toward the sidebar's
    /// outer edge to collapse, toward the workspace to reveal.
    fn place_sidebar_toggle(
        &mut self,
        rect: Rect,
        icon_px: f32,
        sw: f32,
        sh: f32,
        quads: &mut Vec<QuadInstance>,
        chip_bg: bool,
    ) {
        let side = self.cfg.sidebar.side;
        let hidden = self.sidebar_hidden;
        let hovered = rect_contains(rect, self.cursor.0, self.cursor.1);
        if chip_bg {
            quads.push(rect_quad(rect, sw, sh, self.chrome.sidebar, 1.0));
        }
        if hovered {
            quads.push(rect_quad(rect, sw, sh, theme::HOVER_BG, theme::HOVER_ALPHA));
        }
        let glyph = match (side, hidden) {
            (Side::Left, false) | (Side::Right, true) => "\u{2039}", // ‹
            (Side::Left, true) | (Side::Right, false) => "\u{203a}", // ›
        };
        let color = if hovered { theme::LABEL_HOVER } else { theme::LABEL };
        // Tight line box (line height == font size) so centring the box centres the
        // glyph — the terminal metrics' leading would otherwise ride it high.
        self.toggle_buffer
            .set_metrics(Metrics::new(icon_px, icon_px));
        self.toggle_buffer
            .set_size(Some(rect.w.max(1.0)), Some(rect.h.max(1.0)));
        self.toggle_buffer.set_rich_text(
            std::iter::once((glyph, attrs_for(color))),
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.toggle_buffer
            .shape_until_scroll(&mut self.text.font_system, false);
        let glyph_w = self
            .toggle_buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max);
        self.toggle_place = Some(Placement {
            idx: 0,
            left: rect.x + (rect.w - glyph_w) * 0.5,
            top: rect.y + (rect.h - icon_px) * 0.5,
            bounds: TextBounds {
                left: rect.x as i32,
                top: rect.y as i32,
                right: (rect.x + rect.w) as i32,
                bottom: (rect.y + rect.h) as i32,
            },
            color,
        });
        self.buttons.push((rect, ButtonAction::ToggleSidebar));
    }

    /// The sidebar-toggle rect at the sidebar/workspace boundary (or the window
    /// edge when collapsed) — used only when there is no title-bar strip to host
    /// the toggle. Returns the rect so the caller keeps the "+" row clear of it.
    fn build_sidebar_edge_toggle(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Rect {
        let hidden = self.sidebar_hidden;
        let sz = self.cell_h + 8.0 * self.scale;
        let x = if hidden {
            match self.cfg.sidebar.side {
                Side::Left => 0.0,
                Side::Right => (sw - sz).max(0.0),
            }
        } else {
            let bar_x = self.sidebar_rect().x;
            let bar_w = self.sidebar_width();
            match self.cfg.sidebar.side {
                Side::Left => bar_x + bar_w - sz,
                Side::Right => bar_x,
            }
        };
        let rect = Rect { x, y: self.title_bar_h(), w: sz, h: sz };
        self.place_sidebar_toggle(rect, self.cell_h, sw, sh, quads, hidden);
        rect
    }

    /// Draw the custom title-bar strip (macOS): a full-width bar holding the
    /// centred app title and the sidebar toggle, with the top-left kept clear for
    /// the traffic lights. The toggle follows `[sidebar] side`.
    fn build_titlebar(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) {
        let h = self.title_bar_h();
        if h <= 0.0 {
            return;
        }
        quads.push(rect_quad(Rect { x: 0.0, y: 0.0, w: sw, h }, sw, sh, self.chrome.sidebar, 1.0));

        // Toggle: a full-height square on the sidebar's side (so its hover
        // highlight spans the strip), clear of the traffic lights.
        let lights = TRAFFIC_LIGHT_W * self.scale;
        let tog_x = match self.cfg.sidebar.side {
            Side::Left => lights,
            Side::Right => (sw - h).max(0.0),
        };
        let rect = Rect { x: tog_x, y: 0.0, w: h, h };
        // A chevron large enough to read easily within the strip.
        self.place_sidebar_toggle(rect, h * 0.66, sw, sh, quads, false);

        // Title, truly centred on the full window width (looks balanced even though
        // the toggle/lights sit off to the sides). A short title never reaches them.
        self.title_buffer.set_metrics(self.metrics());
        self.title_buffer.set_size(Some(sw), Some(self.cell_h));
        self.title_buffer.set_rich_text(
            std::iter::once(("ghostrealm", attrs_for(theme::TITLE))),
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.title_buffer
            .shape_until_scroll(&mut self.text.font_system, false);
        // Measure the shaped (proportional) width so the title is truly centred.
        let title_w = self
            .title_buffer
            .layout_runs()
            .map(|r| r.line_w)
            .fold(0.0_f32, f32::max);
        self.title_place = Some(Placement {
            idx: 0,
            left: (sw - title_w) * 0.5,
            top: (h - self.cell_h) * 0.5,
            bounds: TextBounds {
                left: 0,
                top: 0,
                right: sw as i32,
                bottom: h as i32,
            },
            color: theme::TITLE,
        });
    }

    /// The "nothing open" picker's options as `(plugin id, title)`, in order
    /// (their 1-based index is the key that opens them).
    fn pickable_plugins(&self) -> Vec<(&'static str, &'static str)> {
        self.app
            .plugins()
            .iter()
            .filter(|p| p.pickable())
            .map(|p| (p.id(), p.title()))
            .collect()
    }

    /// Draw the empty-pane ("nothing open") screen in `rect`: a heading, a
    /// numbered button per pickable plugin, and shortcut hints. Buttons open
    /// that plugin in pane `pane` of workspace `vt`.
    fn build_empty_pane(&mut self, frame: &mut Frame, rect: Rect, vt: Option<VtabId>, pane: PaneId) {
        let options = self.pickable_plugins();
        let mut hints: Vec<String> = [
            ("split.leftright", "splits left/right"),
            ("split.topbottom", "splits top/bottom"),
        ]
        .into_iter()
        .filter_map(|(cmd, what)| {
            let chord = self.cfg.binding_for(cmd)?;
            Some(format!("{}  {what}", pretty_chord(&chord)))
        })
        .collect();
        hints.push(match self.cfg.binding_for("pane.close") {
            Some(c) => format!("{}  closes this workspace", pretty_chord(&c)),
            None => "Close this workspace from the palette".to_string(),
        });
        let ui = self.ui();
        let (scale, ch) = (ui.scale, ui.cell_h);
        // The buttons here are the app's (pushed to `self.buttons`), so the
        // context's view-button owner is never used.
        let mut cx = PaintCx::new(
            &self.cfg,
            &self.chrome,
            ui,
            self.cursor,
            &mut self.text,
            frame,
            Layer::Base,
            PICKER_SID,
        );
        cx.fill(rect, cx.chrome.background);

        let n = options.len() as f32;
        let btn_w = (rect.w * 0.6)
            .clamp(180.0 * scale, 380.0 * scale)
            .min((rect.w - 24.0 * scale).max(1.0));
        let btn_h = ch + 14.0 * scale;
        let gap = 8.0 * scale;
        let heading_gap = 18.0 * scale;
        let hint_gap = 20.0 * scale;
        let total = ch + heading_gap + n * btn_h + (n - 1.0) * gap + hint_gap + hints.len() as f32 * ch;
        let centre = rect.x + rect.w * 0.5;
        let bx = centre - btn_w * 0.5;
        let pad = 12.0 * scale;
        let mut y = rect.y + (rect.h - total).max(0.0) * 0.5;

        let heading = cx.shape("Nothing open", Font::SANS, rect.w);
        cx.place(heading, centre - heading.width * 0.5, y, rect, theme::TITLE);
        y += ch + heading_gap;

        // One button per plugin, numbered (0 for the tenth) so that key opens it.
        for (i, (id, label)) in options.into_iter().enumerate() {
            let hit = Rect { x: bx, y, w: btn_w, h: btn_h };
            let hovered = cx.hovered(hit);
            cx.fill(hit, cx.chrome.sidebar);
            if hovered {
                cx.highlight(hit);
            }
            let color = if hovered { theme::LABEL_HOVER } else { theme::LABEL };
            let text = format!("{}   {label}", (i + 1) % 10);
            cx.label(&text, Font::SANS, bx + pad, y + (btn_h - ch) * 0.5, hit, color);
            if let Some(vt) = vt {
                self.buttons.push((hit, ButtonAction::OpenPlugin(vt, pane, id)));
            }
            y += btn_h + gap;
        }

        y += hint_gap - gap;
        for hint in hints {
            let hint = cx.shape(&hint, Font::SANS, rect.w);
            cx.place(hint, centre - hint.width * 0.5, y, rect, EMPTY_HINT);
            y += ch;
        }
    }

    /// Draw the floating picker overlay: a dim backdrop and a centred
    /// panel hosting a file-browser view (on the overlay layer) above a
    /// confirm/cancel footer.
    fn build_picker(&mut self, frame: &mut Frame) {
        // Overlay text draws above every overlay quad, so a confirmation can't
        // stack over the picker: the picker hides until it is answered.
        if !self.app.picker_open() || self.app.confirm().is_some() {
            return;
        }
        let (sw, sh) = frame.screen;
        let scale = self.scale;
        let ch = self.cell_h;
        let pad = 10.0 * scale;
        frame.top.push(rect_quad(Rect { x: 0.0, y: 0.0, w: sw, h: sh }, sw, sh, [0, 0, 0], 0.5));
        let pw = (sw * 0.6).clamp(360.0 * scale, 760.0 * scale).min((sw - 40.0 * scale).max(1.0));
        let ph = (sh * 0.7).min((sh - 40.0 * scale).max(1.0));
        let (px, py) = ((sw - pw) * 0.5, (sh - ph) * 0.5);
        let panel = Rect { x: px, y: py, w: pw, h: ph };
        self.picker_panel = Some(panel);
        let title_h = ch + 12.0 * scale;
        let footer_h = ch + 16.0 * scale;

        let ui = self.ui();
        let Some(purpose) = self.app.picker_purpose() else {
            return;
        };
        let title_text = match purpose {
            Pick::NewWorkspace => "New workspace directory",
            Pick::ActiveRoot => "Set workspace directory",
            Pick::Open => "Open file",
            Pick::SaveAs(_) => "Save as",
            Pick::SaveCopy(_) => "Save copy as",
        };
        // Under a menu, nothing in the picker hovers.
        let cursor = if self.menu.is_some() { (-1.0, -1.0) } else { self.cursor };
        let Some(view) = self.app.picker_mut() else {
            return;
        };
        let mut cx = PaintCx::new(
            &self.cfg,
            &self.chrome,
            ui,
            cursor,
            &mut self.text,
            frame,
            Layer::Overlay,
            PICKER_SID,
        );
        cx.focused = true;
        cx.fill(panel, cx.chrome.background);
        cx.border(panel, cx.chrome.accent);

        let title_rect = Rect { x: px, y: py, w: pw, h: title_h };
        let title = cx.shape(title_text, Family::SansSerif, pw);
        cx.place(title, px + (pw - title.width) * 0.5, py + (title_h - ch) * 0.5, title_rect, theme::TITLE);

        let body = Rect {
            x: px,
            y: py + title_h,
            w: pw,
            h: (ph - title_h - footer_h).max(1.0),
        };
        view.paint(&mut cx, body);
        // Read after painting: a paint drops a selection that is no longer listed.
        // With nothing chosen yet, the confirm button shows but is inert.
        let choice = view.choice();
        let verb = match purpose {
            Pick::Open => "Open",
            Pick::SaveAs(_) | Pick::SaveCopy(_) if choice.as_ref().is_some_and(|p| p.exists()) => "Replace",
            Pick::SaveAs(_) | Pick::SaveCopy(_) => "Save",
            Pick::NewWorkspace | Pick::ActiveRoot => "Use",
        };
        let confirm = match &choice {
            Some(p) => format!("{verb} {}", path_label(p)),
            None => verb.to_string(),
        };

        // Footer: [Cancel] [Use <dir>], right-aligned.
        let bh = ch + 8.0 * scale;
        let by = py + ph - footer_h + (footer_h - bh) * 0.5;
        let mut right = px + pw - pad;
        for (label, action, primary) in [
            (confirm.as_str(), ButtonAction::PickerConfirm, true),
            ("Cancel", ButtonAction::PickerCancel, false),
        ] {
            let w = label.chars().count() as f32 * ui.cell_w;
            let hit = Rect { x: right - w - pad * 2.0, y: by, w: w + pad * 2.0, h: bh };
            let live = !primary || choice.is_some();
            let hov = live && cx.hovered(hit);
            if primary {
                cx.fill_alpha(hit, cx.chrome.accent, if live { 0.5 } else { 0.15 });
            } else {
                cx.fill(hit, cx.chrome.sidebar);
            }
            if hov {
                cx.highlight(hit);
            }
            let s = cx.shape(label, Family::SansSerif, w + pad);
            let color = match (live, primary || hov) {
                (false, _) => theme::DIM,
                (true, true) => theme::LABEL_HOVER,
                (true, false) => theme::LABEL,
            };
            cx.place(s, hit.x + (hit.w - s.width) * 0.5, by + (bh - ch) * 0.5, hit, color);
            if live {
                self.buttons.push((hit, action));
            }
            right -= w + pad * 3.0;
        }
        let owned = frame.view_buttons.drain(..);
        self.buttons.extend(owned.map(|(r, s, id)| (r, ButtonAction::View(s, id))));
    }

    /// Draw the open confirmation: a dim backdrop and a centred panel with the
    /// question above [<discard>] [Cancel] [<accept>], right-aligned.
    fn build_confirm(&mut self, frame: &mut Frame) {
        let Some(confirm) = self.app.confirm().cloned() else {
            return;
        };
        let (sw, sh) = frame.screen;
        let scale = self.scale;
        let ch = self.cell_h;
        let pad = 12.0 * scale;
        frame.top.push(rect_quad(Rect { x: 0.0, y: 0.0, w: sw, h: sh }, sw, sh, [0, 0, 0], 0.5));
        let bh = ch + 8.0 * scale;
        // Right to left: accept, Cancel, then the discard button. The primary
        // one is what Enter presses.
        let cancels = confirm.enter_cancels();
        let buttons: Vec<(&'static str, ButtonAction, bool)> = [
            confirm.accept.map(|l| (l, ButtonAction::ConfirmAccept, !cancels)),
            Some(("Cancel", ButtonAction::ConfirmCancel, cancels)),
            confirm.discard.map(|l| (l, ButtonAction::ConfirmDiscard, false)),
        ]
        .into_iter()
        .flatten()
        .collect();
        let button_w = |label: &str| label.chars().count() as f32 * self.ui().cell_w + pad * 2.0;
        let buttons_w: f32 = buttons.iter().map(|(l, ..)| button_w(l) + pad).sum::<f32>() - pad;
        let max_w = (sw - 40.0 * scale).max(1.0);
        let pw = (sw * 0.4)
            .clamp(320.0 * scale, 520.0 * scale)
            .max(buttons_w + pad * 2.0)
            .min(max_w);
        let ui = self.ui();
        let mut cx = PaintCx::new(
            &self.cfg,
            &self.chrome,
            ui,
            self.cursor,
            &mut self.text,
            frame,
            Layer::Overlay,
            PICKER_SID,
        );
        // Shaped text doesn't wrap: break the message into lines that fit, and
        // size the panel to them.
        let text_w = pw - pad * 2.0;
        let lines = wrap_words(&confirm.message, text_w, |s| cx.shape(s, Family::SansSerif, f32::MAX).width);
        let ph = ch * lines.len() as f32 + bh + pad * 3.0;
        let (px, py) = ((sw - pw) * 0.5, (sh - ph) * 0.5);
        let panel = Rect { x: px, y: py, w: pw, h: ph };
        cx.fill(panel, cx.chrome.background);
        cx.border(panel, cx.chrome.accent);
        for (i, line) in lines.iter().enumerate() {
            let msg = cx.shape(line, Family::SansSerif, text_w);
            cx.place(msg, px + pad, py + pad + ch * i as f32, panel, theme::TITLE);
        }

        let by = py + ph - pad - bh;
        let mut right = px + pw - pad;
        for (label, action, primary) in buttons {
            let w = label.chars().count() as f32 * ui.cell_w;
            let hit = Rect { x: right - w - pad * 2.0, y: by, w: w + pad * 2.0, h: bh };
            let hov = cx.hovered(hit);
            if primary {
                cx.fill_alpha(hit, cx.chrome.accent, 0.5);
            } else {
                cx.fill(hit, cx.chrome.sidebar);
            }
            if hov {
                cx.highlight(hit);
            }
            let s = cx.shape(label, Family::SansSerif, w + pad);
            let color = if primary || hov { theme::LABEL_HOVER } else { theme::LABEL };
            cx.place(s, hit.x + (hit.w - s.width) * 0.5, by + (bh - ch) * 0.5, hit, color);
            self.buttons.push((hit, action));
            right -= w + pad * 3.0;
        }
    }

    /// Draw the hovered tooltip once its anchor has been hovered for
    /// [`TOOLTIP_DELAY`] (until then, schedule the wake that shows it): a small
    /// panel below the anchor (above, near the bottom edge). A pane's tooltip is
    /// suppressed while a modal overlay is open.
    fn build_tooltip(&mut self, frame: &mut Frame) {
        let modal = self.palette.is_some()
            || self.menu.is_some()
            || self.app.picker_open()
            || self.app.confirm().is_some();
        // A menu covers every layer, the picker's too.
        let tip = frame
            .tooltip
            .take()
            .filter(|(_, _, layer)| self.menu.is_none() && (!modal || *layer == Layer::Overlay));
        let Some((anchor, text, _)) = tip else {
            self.tooltip_hover = None;
            self.tooltip_due = None;
            return;
        };
        let now = Instant::now();
        let since = match self.tooltip_hover {
            Some((r, t)) if r == anchor => t,
            _ => now,
        };
        self.tooltip_hover = Some((anchor, since));
        if now < since + TOOLTIP_DELAY {
            self.tooltip_due = Some(since + TOOLTIP_DELAY);
            return;
        }
        self.tooltip_due = None;

        let (sw, sh) = frame.screen;
        let (scale, ch) = (self.scale, self.cell_h);
        let pad = 6.0 * scale;
        let ui = self.ui();
        let mut cx = PaintCx::new(
            &self.cfg,
            &self.chrome,
            ui,
            self.cursor,
            &mut self.text,
            frame,
            Layer::Overlay,
            PICKER_SID,
        );
        let lines: Vec<Shaped> = text.lines().map(|l| cx.shape(l, Family::SansSerif, sw)).collect();
        let w = lines.iter().map(|s| s.width).fold(0.0, f32::max) + pad * 2.0;
        let h = lines.len() as f32 * ch + pad * 2.0;
        let gap = 4.0 * scale;
        let below = anchor.y + anchor.h + gap;
        let y = if below + h <= sh { below } else { (anchor.y - gap - h).max(0.0) };
        let x = anchor.x.clamp(0.0, (sw - w).max(0.0));
        let panel = Rect { x, y, w, h };
        cx.fill(panel, theme::TOOLTIP_BG);
        cx.border(panel, theme::INPUT_BORDER);
        for (i, line) in lines.into_iter().enumerate() {
            cx.place(line, x + pad, y + pad + i as f32 * ch, panel, theme::TEXT);
        }
    }

    /// Push sidebar quads and shape vtab-name text; returns placements into
    /// `sidebar_buffers`.
    fn build_sidebar(
        &mut self,
        sw: f32,
        sh: f32,
        quads: &mut Vec<QuadInstance>,
        close_placements: &mut Vec<Placement>,
    ) -> Vec<Placement> {
        // With a title-bar strip the toggle lives there; otherwise it sits at the
        // sidebar edge.
        let toggle = if self.title_bar_h() > 0.0 {
            Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 }
        } else {
            self.build_sidebar_edge_toggle(sw, sh, quads)
        };
        if self.sidebar_hidden {
            return Vec::new();
        }
        let bar = self.sidebar_rect();
        let bar_w = self.sidebar_width();
        let bar_x = bar.x;
        let bar_top = bar.y;
        let row_h = self.cell_h + 8.0 * self.scale;
        let pad = 8.0 * self.scale;
        let dot = 6.0 * self.scale;
        let text_x = bar_x + pad + dot + 6.0 * self.scale;
        let metrics = self.metrics();
        let active = self.app.tree.active_vtab();
        #[allow(clippy::type_complexity)]
        let vtabs: Vec<(
            ghostrealm_core::VtabId,
            String,
            bool,
            TabStatus,
            bool,
            Option<std::path::PathBuf>,
        )> = self
            .app
            .tree
            .vtabs()
            .iter()
            .map(|v| {
                (
                    v.id,
                    v.name.clone(),
                    v.user_named,
                    v.status,
                    Some(v.id) == active,
                    self.app.vtab_dir(v.id),
                )
            })
            .collect();
        let n = vtabs.len();

        quads.push(rect_quad(
            Rect {
                x: bar_x,
                y: bar_top,
                w: bar_w,
                h: bar.h,
            },
            sw,
            sh,
            self.chrome.sidebar,
            1.0,
        ));

        // One buffer per workspace plus one for the pinned "+" button.
        while self.sidebar_buffers.len() < n + 1 {
            let b = Buffer::new(&mut self.text.font_system, metrics);
            self.sidebar_buffers.push(b);
        }

        let mut placements = Vec::new();

        // Pinned "+" new-workspace button at the top of the sidebar (below the
        // title-bar strip), kept clear of the edge toggle when there is no strip.
        let btn_y = bar_top;
        let (btn_x, label_x) = match self.cfg.sidebar.side {
            Side::Left => (bar_x, bar_x + pad),
            Side::Right => (bar_x + toggle.w, bar_x + toggle.w + pad),
        };
        let btn_w = (bar_w - toggle.w).max(1.0);
        let btn = Rect {
            x: btn_x,
            y: btn_y,
            w: btn_w,
            h: row_h,
        };
        let btn_hovered = rect_contains(btn, self.cursor.0, self.cursor.1);
        if btn_hovered {
            // Full-bleed row highlight (edge to edge, full height) — a row button,
            // not a compact icon.
            quads.push(rect_quad(btn, sw, sh, theme::HOVER_BG, theme::HOVER_ALPHA));
        }
        let btn_color = if btn_hovered {
            theme::LABEL_HOVER
        } else {
            theme::LABEL
        };
        {
            let buf = &mut self.sidebar_buffers[n];
            buf.set_metrics(metrics);
            buf.set_size(Some((btn_w - pad * 2.0).max(1.0)), Some(self.cell_h));
            buf.set_rich_text(
                std::iter::once(("+  New workspace", attrs_for(btn_color))),
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.text.font_system, false);
        }
        placements.push(Placement {
            idx: n,
            left: label_x,
            top: btn_y + (row_h - self.cell_h) * 0.5,
            bounds: TextBounds {
                left: btn_x as i32,
                top: btn_y as i32,
                right: (btn_x + btn_w) as i32,
                bottom: (btn_y + row_h) as i32,
            },
            color: btn_color,
        });
        self.buttons.push((btn, ButtonAction::NewVtab));

        // Scrollable workspace list below the pinned button. Rows are variable
        // height: an unnamed workspace shows just its (shortened) directory on one
        // line; a renamed one shows the name over a greyed directory on two lines.
        let list_top = btn_y + row_h;
        let visible_h = (sh - list_top).max(0.0);
        let vpad = 8.0 * self.scale;
        let close_w = self.cell_h;
        let close_x = bar_x + bar_w - close_w - pad;
        let text_w_chars = (((close_x - text_x) / self.cell_w).floor() as usize).max(3);

        // Per-row display + height.
        let dir_color = [140, 140, 155];
        let name_color = [220, 220, 230];
        let mut rows: Vec<(bool, Option<String>, String, bool)> = Vec::with_capacity(n);
        let mut heights: Vec<f32> = Vec::with_capacity(n);
        for (_id, name, user_named, _status, _active, dir) in &vtabs {
            let dir_short = dir.as_ref().map(|d| shorten_dir(d, text_w_chars));
            let two_line = *user_named && dir_short.is_some();
            // Line 1 label: the name if named, else the directory shorthand
            // (falling back to the stored name only when there's no directory).
            let label = if *user_named {
                name.clone()
            } else {
                dir_short.clone().unwrap_or_else(|| name.clone())
            };
            heights.push(if two_line { 2.0 } else { 1.0 } * self.cell_h + vpad);
            rows.push((two_line, dir_short, label, name.as_str() == SETTINGS_VTAB_NAME));
        }
        let total: f32 = heights.iter().sum();
        self.sidebar_max_scroll = (total - visible_h).max(0.0);
        self.sidebar_scroll = self.sidebar_scroll.clamp(0.0, self.sidebar_max_scroll);
        let scroll = self.sidebar_scroll;

        self.sidebar_rows.clear();
        let mut y = list_top - scroll;
        for (i, (id, _name, _user_named, status, is_active, _dir)) in vtabs.iter().enumerate() {
            let (two_line, dir_short, label, is_settings) = &rows[i];
            let text_h = if *two_line { 2.0 * self.cell_h } else { self.cell_h };
            let rh = heights[i];
            if y + rh <= list_top || y >= sh {
                y += rh;
                continue; // scrolled out of the list viewport
            }
            let vis_top = y.max(list_top);
            let vis_bot = (y + rh).min(sh);
            self.sidebar_rows.push((
                Rect {
                    x: bar_x,
                    y: vis_top,
                    w: bar_w,
                    h: (vis_bot - vis_top).max(0.0),
                },
                *id,
            ));
            if *is_active {
                quads.push(rect_quad(
                    Rect {
                        x: bar_x,
                        y: vis_top,
                        w: bar_w,
                        h: vis_bot - vis_top,
                    },
                    sw,
                    sh,
                    [40, 44, 60],
                    1.0,
                ));
            }
            let dot_y = y + (rh - dot) * 0.5;
            if dot_y >= list_top && dot_y + dot <= sh {
                quads.push(rect_quad(
                    Rect {
                        x: bar_x + pad,
                        y: dot_y,
                        w: dot,
                        h: dot,
                    },
                    sw,
                    sh,
                    status_color(*status),
                    1.0,
                ));
            }
            let name_attrs = || {
                let a = Attrs::new()
                    .family(Family::SansSerif)
                    .color(Color::rgb(name_color[0], name_color[1], name_color[2]));
                if *is_settings {
                    a.style(Style::Italic)
                } else {
                    a
                }
            };
            let dir_attrs = Attrs::new()
                .family(Family::SansSerif)
                .color(Color::rgb(dir_color[0], dir_color[1], dir_color[2]));
            let buf = &mut self.sidebar_buffers[i];
            buf.set_metrics(metrics);
            buf.set_wrap(Wrap::None);
            buf.set_size(Some((close_x - text_x).max(1.0)), Some(text_h));
            if *two_line {
                let dir_s = dir_short.clone().unwrap_or_default();
                buf.set_rich_text(
                    [
                        (label.as_str(), name_attrs()),
                        ("\n", name_attrs()),
                        (dir_s.as_str(), dir_attrs),
                    ],
                    &Attrs::new().family(Family::SansSerif),
                    Shaping::Advanced,
                    None,
                );
            } else {
                buf.set_rich_text(
                    std::iter::once((label.as_str(), name_attrs())),
                    &Attrs::new().family(Family::SansSerif),
                    Shaping::Advanced,
                    None,
                );
            }
            buf.shape_until_scroll(&mut self.text.font_system, false);
            placements.push(Placement {
                idx: i,
                left: text_x,
                top: y + (rh - text_h) * 0.5,
                bounds: TextBounds {
                    left: bar_x as i32,
                    top: vis_top as i32,
                    right: close_x as i32,
                    bottom: vis_bot as i32,
                },
                color: name_color,
            });
            // Close ('×') button for this workspace (centred vertically in the row).
            let hit = Rect {
                x: close_x,
                y: vis_top,
                w: close_w,
                h: (vis_bot - vis_top).max(0.0),
            };
            let hovered = rect_contains(hit, self.cursor.0, self.cursor.1);
            if hovered {
                quads.push(rect_quad(
                    hover_box(hit, close_w),
                    sw,
                    sh,
                    theme::HOVER_BG,
                    theme::HOVER_ALPHA,
                ));
            }
            close_placements.push(Placement {
                idx: 0,
                left: close_x + (close_w - self.cell_w) * 0.5,
                top: y + (rh - self.cell_h) * 0.5,
                bounds: TextBounds {
                    left: close_x as i32,
                    top: vis_top as i32,
                    right: (close_x + close_w) as i32,
                    bottom: vis_bot as i32,
                },
                color: if hovered { theme::GLYPH_HOVER } else { theme::GLYPH },
            });
            self.buttons.push((hit, ButtonAction::CloseVtab(*id)));
            y += rh;
        }

        placements
    }

    /// Build the context-menu overlay (panel, hover pill, rules, check marks)
    /// into `quads`; shape item labels and hints into `menu_buffers`; record
    /// the hit rects. Returns placements.
    fn build_menu(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Vec<Placement> {
        let (items, anchor, align, hover) = match &self.menu {
            Some(m) => (m.items.clone(), m.anchor, m.align, m.hover),
            None => return Vec::new(),
        };
        let metrics = self.metrics();
        let ui = self.ui();
        let n = items.len();
        while self.menu_buffers.len() < n * 2 {
            let b = Buffer::new(&mut self.text.font_system, metrics);
            self.menu_buffers.push(b);
        }
        // Shaped unbounded, one line each; placements clip them to their
        // layout rects. Returns the shaped width.
        let cell_h = self.cell_h;
        let shape = |buf: &mut Buffer, text: &str, color: [u8; 3], fs: &mut FontSystem| -> f32 {
            buf.set_metrics(metrics);
            buf.set_size(None, Some(cell_h));
            buf.set_rich_text(
                std::iter::once((text, attrs_for(color))),
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(fs, false);
            buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max)
        };
        let (mut label_w, mut hint_w) = (vec![0.0f32; n], vec![0.0f32; n]);
        for (i, item) in items.iter().enumerate() {
            let color = if item.enabled { [230, 230, 240] } else { theme::DIM };
            label_w[i] = shape(&mut self.menu_buffers[i], &item.label, color, &mut self.text.font_system);
            if let Some(hint) = &item.hint {
                hint_w[i] = shape(&mut self.menu_buffers[n + i], hint, theme::TEXT_DIM, &mut self.text.font_system);
            }
        }
        let layout = menu_layout(&items, &label_w, &hint_w, anchor, align, (sw, sh), ui);

        // The hover pill sits `inset` inside the panel; the panel's corners
        // are the pill's grown by it, so the two curves run parallel.
        let inset = 4.0 * self.scale;
        let pill_r = theme::pill_radius(ui);
        let border = (theme::hairline(ui), theme::INPUT_BORDER, 1.0);
        quads.push(round_quad(layout.panel, sw, sh, pill_r + inset, [34, 34, 42], 0.98, Some(border)));
        let bounds_of = |r: Rect| TextBounds {
            left: r.x as i32,
            top: r.y as i32,
            right: (r.x + r.w) as i32,
            bottom: (r.y + r.h) as i32,
        };
        let mut placements = Vec::with_capacity(n * 2);
        for (i, (item, row)) in items.iter().zip(&layout.rows).enumerate() {
            let rect = row.rect;
            if matches!(item.action, MenuAction::Rule) {
                let pad = 8.0 * self.scale;
                let line_h = theme::hairline(ui);
                quads.push(rect_quad(
                    Rect { x: rect.x + pad, y: rect.y + (rect.h - line_h) * 0.5, w: rect.w - pad * 2.0, h: line_h },
                    sw,
                    sh,
                    [90, 90, 105],
                    1.0,
                ));
                continue;
            }
            if hover == Some(i) {
                let pill = Rect { x: rect.x + inset, w: rect.w - inset * 2.0, ..rect };
                quads.push(round_quad(pill, sw, sh, pill_r, theme::HOVER_BG, theme::HOVER_ALPHA, None));
            }
            let top = rect.y + (rect.h - self.cell_h) * 0.5;
            if let Some(h) = row.hint {
                // Right-aligned; a hint cut short keeps its start.
                let left = (h.x + h.w - hint_w[i]).max(h.x);
                placements.push(Placement { idx: n + i, left, top, bounds: bounds_of(h), color: theme::TEXT_DIM });
            }
            if let Some(c) = row.check {
                let w = (1.5 * self.scale).max(1.0);
                let color = if item.enabled { self.chrome.accent } else { theme::DIM };
                let pts = [(0.15, 0.55), (0.4, 0.8), (0.85, 0.25)].map(|(u, v)| (c.x + u * c.w, c.y + v * c.h));
                for seg in pts.windows(2) {
                    quads.push(line_quad(seg[0], seg[1], w, false, sw, sh, color, 1.0));
                }
            }
            let color = if item.enabled { [230, 230, 240] } else { theme::DIM };
            placements.push(Placement { idx: i, left: row.label.x, top, bounds: bounds_of(row.label), color });
        }

        if let Some(m) = self.menu.as_mut() {
            m.place(&layout);
        }
        placements
    }

    /// Run a command by id: execute immediately if it needs no required args, else
    /// open the palette to collect them (the argument prompt).
    fn begin_command(&mut self, id: &str) {
        self.begin_command_with(id, Args::new(), None);
    }

    /// [`Self::begin_command`] with some arguments known (`preset`) and the
    /// prompt starting with `prefill`.
    fn begin_command_with(&mut self, id: &str, preset: Args, prefill: Option<String>) {
        let specs: Vec<ArgSpec> = self
            .registry
            .meta(id)
            .map(|m| m.args.iter().filter(|a| a.required && preset.get(a.name).is_none()).cloned().collect())
            .unwrap_or_default();
        if specs.is_empty() {
            self.run_command(id, &preset);
        } else {
            let title = self
                .registry
                .meta(id)
                .map(|m| m.title.to_string())
                .unwrap_or_default();
            self.menu = None;
            self.palette = Some(Palette {
                input: prefill.map(LineEdit::new).unwrap_or_default(),
                selected: 0,
                scroll: 0,
                mode: PaletteMode::Commands,
                pending: Some(PendingArgs {
                    id: id.to_string(),
                    title,
                    specs,
                    values: Vec::new(),
                    preset,
                }),
            });
        }
        self.dirty = true;
    }

    /// Run command `id`, reporting a failure.
    fn run_command(&mut self, id: &str, args: &Args) {
        if let Err(e) = self.registry.execute(id, args, &mut self.app) {
            eprintln!("ghostrealm: {id}: {e}");
        }
    }

    /// Open the command palette (fresh query, first result selected).
    fn open_palette(&mut self) {
        self.menu = None;
        self.palette = Some(Palette {
            input: LineEdit::default(),
            selected: 0,
            scroll: 0,
            mode: PaletteMode::Commands,
            pending: None,
        });
        self.dirty = true;
    }

    /// Toggle the overlay in `mode`: close it if already open in that mode, else
    /// (re)open it in that mode.
    fn toggle_palette(&mut self, mode: PaletteMode) {
        self.menu = None;
        match &self.palette {
            Some(p) if p.mode == mode => self.palette = None,
            _ => {
                self.palette = Some(Palette {
                    input: LineEdit::default(),
                    selected: 0,
                    scroll: 0,
                    mode,
                    pending: None,
                })
            }
        }
        self.dirty = true;
    }

    /// Feed a key event to the double-tap detectors. Returns `true` if a double-tap
    /// toggled the overlay (so the caller redraws). Shift/Ctrl never reach the
    /// shell as lone taps, so this is safe to run for every key event.
    fn handle_key_taps(&mut self, event: &winit::event::KeyEvent) -> bool {
        let is_shift = matches!(event.logical_key, WKey::Named(NamedKey::Shift));
        let is_ctrl = matches!(event.logical_key, WKey::Named(NamedKey::Control));
        match event.state {
            ElementState::Pressed => {
                if !event.repeat {
                    // A press of one tracked modifier (or any other key) breaks a
                    // lone-tap sequence of the other.
                    if is_shift {
                        self.shift_taps.press();
                        self.ctrl_taps.interrupt();
                    } else if is_ctrl {
                        self.ctrl_taps.press();
                        self.shift_taps.interrupt();
                    } else {
                        self.shift_taps.interrupt();
                        self.ctrl_taps.interrupt();
                    }
                }
                false
            }
            // A confirmation or the picker owns the keyboard; a palette over it
            // would take its keys and leave it unanswerable.
            ElementState::Released if self.app.confirm().is_some() || self.app.picker_open() => {
                self.shift_taps.interrupt();
                self.ctrl_taps.interrupt();
                false
            }
            ElementState::Released => {
                let now = Instant::now();
                if is_shift && self.shift_taps.release(now) {
                    self.toggle_palette(PaletteMode::Commands);
                    return true;
                }
                if is_ctrl && self.ctrl_taps.release(now) {
                    self.toggle_palette(PaletteMode::Run);
                    return true;
                }
                false
            }
        }
    }

    fn on_key(&mut self, event: &winit::event::KeyEvent) {
        // A confirmation owns the keyboard: Enter presses the primary button
        // (the save button, when it has one; Cancel over a file changed on
        // disk), Escape cancels. A held
        // key's repeats are ignored, so the Enter that asked can't also answer
        // (nor one pressed before a background job asked). For a moment after
        // a job asks, keys are ignored, and then Enter cancels: an Enter typed
        // for a terminal must not answer a Delete question that has appeared.
        if let Some(confirm) = self.app.confirm() {
            if event.repeat || !confirm.takes_keys_at(Instant::now()) {
                return;
            }
            match &event.logical_key {
                WKey::Named(NamedKey::Enter) => {
                    let done = self.app.enter_confirm();
                    self.after_confirm(done);
                }
                WKey::Named(NamedKey::Escape) => self.app.cancel_confirm(),
                _ => {}
            }
            self.dirty = true;
            return;
        }
        // Cmd+Q quits (macOS routes it here instead of ending the process, so
        // unsaved changes get asked about).
        if self.mods.super_ && !self.mods.shift && !self.mods.alt && !self.mods.ctrl {
            if let WKey::Character(s) = base_key(event) {
                if s.eq_ignore_ascii_case("q") {
                    self.request_quit();
                    return;
                }
            }
        }
        // A context menu is dismissed by Escape (one over the picker before
        // the picker); other keys pass through.
        if self.menu.is_some() && matches!(event.logical_key, WKey::Named(NamedKey::Escape)) {
            self.menu = None;
            self.dirty = true;
            return;
        }
        // The picker, when open, owns the keyboard: Escape cancels,
        // Cmd+Enter confirms, everything else drives its file browser.
        if self.app.picker_open() {
            match &event.logical_key {
                WKey::Named(NamedKey::Escape) => {
                    self.app.close_picker();
                    self.dirty = true;
                }
                WKey::Named(NamedKey::Enter) if self.mods.super_ => {
                    self.confirm_picker(None);
                }
                _ => {
                    if let Some(press) = winit_key_press(event, self.mods) {
                        self.view_event(PICKER_SID, |v, cx| {
                            v.key(cx, &press);
                        });
                    }
                }
            }
            return;
        }
        // The palette, when open, owns the keyboard.
        if self.palette.is_some() {
            self.palette_key(event);
            return;
        }
        // Cmd chords: the app's own (settings, keybindings) match first; unbound
        // ones fall to the focused view (clipboard, save, navigation). A view
        // never forwards a Cmd chord to a shell.
        if self.mods.super_ {
            // Use the base key (Shift's symbol transform undone), so a binding like
            // `cmd+shift+/` matches even though Shift+/ yields `?`.
            if let WKey::Character(s) = base_key(event) {
                if let Some(c) = s.chars().next() {
                    // Cmd+, opens the config file in an editor (settings live in
                    // the file).
                    if c == ',' {
                        self.open_config_editor();
                        return;
                    }
                    let chord = self.chord_string(c);
                    if let Some(id) = self.cfg.binding(&chord) {
                        if id == "palette.toggle" {
                            self.open_palette();
                        } else {
                            let _ = self.registry.execute(&id, &Args::new(), &mut self.app);
                        }
                        self.dirty = true;
                        return;
                    }
                }
            }
        }

        // The focused view takes the key: raw first (views with their own key
        // handling), else mapped.
        if let Some(sid) = self.app.focused_surface().filter(|s| self.app.view(*s).is_some()) {
            let raw = RawKey { event, press: winit_key_press(event, self.mods) };
            self.view_event(sid, |v, cx| {
                if !v.raw_key(cx, &raw) {
                    if let Some(press) = &raw.press {
                        v.key(cx, press);
                    }
                }
            });
            return;
        }

        // The "nothing open" picker: a number key opens that option.
        if !self.mods.super_ && self.app.focused_shows_picker() {
            if let WKey::Character(s) = &event.logical_key {
                if let Some(d) = s.chars().next().and_then(|c| c.to_digit(10)) {
                    let i = if d == 0 { 9 } else { d as usize - 1 };
                    if let Some(id) = self.pickable_plugins().get(i).map(|(id, _)| *id) {
                        if let Err(e) = self.app.open_plugin_in_focused(id) {
                            eprintln!("ghostrealm: open {id}: {e:#}");
                        }
                        self.dirty = true;
                    }
                }
            }
        }
    }

    /// Ranked, human-visible command results for `query`: `(id, title, chord)`.
    /// Hidden (agent-only) commands are excluded.
    fn palette_results(&self, query: &str) -> Vec<(String, String, Option<String>)> {
        self.registry
            .search(query, PALETTE_SEARCH_CAP)
            .into_iter()
            .filter(|h| !h.meta.hidden)
            .map(|h| {
                (
                    h.meta.id.to_string(),
                    h.meta.title.to_string(),
                    self.cfg.binding_for(h.meta.id),
                )
            })
            .collect()
    }

    fn palette_key(&mut self, event: &winit::event::KeyEvent) {
        // In argument-collection mode the input feeds the current arg, not search.
        if self
            .palette
            .as_ref()
            .map(|p| p.pending.is_some())
            .unwrap_or(false)
        {
            self.palette_arg_key(event);
            return;
        }

        let (query, selected, mode) = match &self.palette {
            Some(p) => (p.input.text().to_string(), p.selected, p.mode),
            None => return,
        };
        let result_count = || {
            if mode == PaletteMode::Commands {
                self.palette_results(&query).len()
            } else {
                0
            }
        };
        match &event.logical_key {
            WKey::Named(NamedKey::Escape) => self.palette = None,
            WKey::Named(NamedKey::Enter) => match mode {
                PaletteMode::Commands => {
                    let id = self
                        .palette_results(&query)
                        .get(selected)
                        .map(|(id, _, _)| id.clone());
                    self.palette = None;
                    if let Some(id) = id {
                        // Executes now, or reopens the palette to collect args.
                        self.begin_command(&id);
                    }
                }
                PaletteMode::Run => {
                    let line = query.trim().to_string();
                    self.palette = None;
                    if !line.is_empty() {
                        if let Err(e) = self.app.new_vtab_running(line) {
                            eprintln!("ghostrealm: run-anything failed: {e:#}");
                        }
                    }
                }
            },
            WKey::Named(NamedKey::ArrowDown) => {
                let n = result_count();
                if let Some(p) = self.palette.as_mut() {
                    if n > 0 {
                        p.selected = (selected + 1) % n;
                    }
                }
            }
            WKey::Named(NamedKey::ArrowUp) => {
                let n = result_count();
                if let Some(p) = self.palette.as_mut() {
                    if n > 0 {
                        p.selected = (selected + n - 1) % n;
                    }
                }
            }
            _ => {
                if self.palette_edit(event) == Applied::Changed {
                    if let Some(p) = self.palette.as_mut() {
                        p.selected = 0;
                        p.scroll = 0;
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Apply a text-editing key to the palette's input.
    fn palette_edit(&mut self, event: &winit::event::KeyEvent) -> Applied {
        match (winit_key_press(event, self.mods), self.palette.as_mut()) {
            (Some(press), Some(p)) => p.input.key(&press, &mut self.clipboard),
            _ => Applied::Ignored,
        }
    }

    /// Key handling while the palette is collecting a command's arguments.
    fn palette_arg_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            WKey::Named(NamedKey::Escape) => self.palette = None,
            WKey::Named(NamedKey::Enter) => {
                let mut run: Option<(String, Args)> = None;
                if let Some(p) = self.palette.as_mut() {
                    let query = p.input.text().to_string();
                    if let Some(pend) = p.pending.as_mut() {
                        let idx = pend.values.len();
                        if let Some(spec) = pend.specs.get(idx) {
                            if let Some(val) = parse_arg_value(&spec.kind, &query) {
                                pend.values.push(val);
                                if pend.values.len() == pend.specs.len() {
                                    let mut args = pend.preset.clone();
                                    for (s, v) in pend.specs.iter().zip(&pend.values) {
                                        args.insert(s.name, v.clone());
                                    }
                                    run = Some((pend.id.clone(), args));
                                } else {
                                    p.input.set_text("");
                                }
                            }
                            // An unparseable value keeps the query for a retry.
                        }
                    }
                }
                if let Some((id, args)) = run {
                    self.palette = None;
                    self.run_command(&id, &args);
                }
            }
            _ => {
                self.palette_edit(event);
            }
        }
        self.dirty = true;
    }

    /// Build the palette overlay: push its dim/panel/selection quads and shape
    /// its text lines, returning their placements (indices into palette_buffers).
    fn build_palette(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Vec<Placement> {
        // Snapshot palette state, including any in-progress argument prompt.
        let (input, mode, pending) = match &self.palette {
            Some(p) => {
                let pend = p.pending.as_ref().map(|pd| {
                    let idx = pd.values.len();
                    let (name, desc, kind_hint) = pd
                        .specs
                        .get(idx)
                        .map(|s| {
                            let kh = match &s.kind {
                                ArgKind::Enum(vs) => format!("one of: {}", vs.join(", ")),
                                k => k.to_string(),
                            };
                            (s.name.to_string(), s.description.to_string(), kh)
                        })
                        .unwrap_or_default();
                    (pd.title.clone(), name, desc, kind_hint, idx, pd.specs.len())
                });
                (p.input.clone(), p.mode, pend)
            }
            None => return Vec::new(),
        };

        let query = input.text().to_string();
        let is_search = pending.is_none() && mode == PaletteMode::Commands;
        // Command mode fuzzy-searches the registry (hidden commands excluded); run
        // mode takes the raw line; arg mode collects an argument.
        let results: Vec<(String, String, Option<String>)> = if is_search {
            self.palette_results(&query)
        } else {
            Vec::new()
        };
        let n = results.len();

        // Clamp selection + scroll so the selected result stays visible.
        let (sel, scroll) = if is_search {
            let p = self.palette.as_mut().unwrap();
            if n == 0 {
                p.selected = 0;
                p.scroll = 0;
            } else {
                p.selected = p.selected.min(n - 1);
                if p.selected < p.scroll {
                    p.scroll = p.selected;
                } else if p.selected >= p.scroll + PALETTE_VISIBLE {
                    p.scroll = p.selected + 1 - PALETTE_VISIBLE;
                }
                p.scroll = p.scroll.min(n.saturating_sub(PALETTE_VISIBLE));
            }
            (p.selected, p.scroll)
        } else {
            (0, 0)
        };

        let metrics = self.metrics();
        let line_h = self.cell_h;
        let pad = 10.0 * self.scale;
        let panel_w = (sw * 0.6).clamp(240.0, 720.0 * self.scale);
        let panel_x = ((sw - panel_w) / 2.0).max(0.0);
        let panel_y = sh * 0.12;
        let body_rows = if is_search && n > 0 {
            n.min(PALETTE_VISIBLE)
        } else {
            1 // a hint / "no matches" / arg-prompt line
        };
        let panel_h = (1 + body_rows) as f32 * line_h + pad * 2.0;

        self.palette_panel = Rect {
            x: panel_x,
            y: panel_y,
            w: panel_w,
            h: panel_h,
        };

        quads.push(rect_quad(
            Rect {
                x: 0.0,
                y: 0.0,
                w: sw,
                h: sh,
            },
            sw,
            sh,
            [0, 0, 0],
            0.45,
        ));
        quads.push(rect_quad(
            Rect {
                x: panel_x,
                y: panel_y,
                w: panel_w,
                h: panel_h,
            },
            sw,
            sh,
            [28, 28, 36],
            0.98,
        ));
        // Selection highlight at the selected result's visible row.
        if is_search && n > 0 {
            let vis = sel - scroll;
            quads.push(rect_quad(
                Rect {
                    x: panel_x + pad * 0.5,
                    y: panel_y + pad + (1 + vis) as f32 * line_h,
                    w: panel_w - pad,
                    h: line_h,
                },
                sw,
                sh,
                self.chrome.accent,
                0.9,
            ));
        }

        // Each line is a list of coloured spans (title bright + right-aligned chord
        // dim). Record the visible rows for click/hover hit-testing.
        let usable_cols = (((panel_w - pad * 2.0) / self.cell_w).floor() as usize).max(1);
        self.palette_rows.clear();
        let mut lines: Vec<Vec<(String, [u8; 3])>> = Vec::new();

        if let Some((title, name, desc, kind_hint, idx, total)) = &pending {
            // Argument-collection view: input the current argument.
            let prefix = format!("\u{203a} {title} — {name}: ");
            lines.push(vec![(format!("{prefix}{query}"), [235, 235, 245])]);
            let more = if idx + 1 < *total {
                "Enter for next"
            } else {
                "Enter to run"
            };
            let desc = if desc.is_empty() {
                kind_hint.clone()
            } else {
                format!("{desc} · {kind_hint}")
            };
            lines.push(vec![(format!("  {desc} · {more}"), [150, 150, 160])]);

            while self.palette_buffers.len() < lines.len() {
                let b = Buffer::new(&mut self.text.font_system, metrics);
                self.palette_buffers.push(b);
            }
            let text_x = panel_x + pad;
            let text_w = (panel_w - pad * 2.0).max(1.0);
            let mut placements = Vec::with_capacity(lines.len());
            for (i, spans) in lines.iter().enumerate() {
                let buf = &mut self.palette_buffers[i];
                buf.set_metrics(metrics);
                buf.set_wrap(if i == 0 { Wrap::None } else { Wrap::WordOrGlyph });
                buf.set_size(Some(text_w), Some(line_h));
                buf.set_rich_text(
                    spans.iter().map(|(t, c)| (t.as_str(), attrs_for(*c))),
                    &Attrs::new().family(Family::Monospace),
                    Shaping::Advanced,
                    None,
                );
                buf.shape_until_scroll(&mut self.text.font_system, false);
                placements.push(Placement {
                    idx: i,
                    left: text_x,
                    top: panel_y + pad + i as f32 * line_h,
                    bounds: TextBounds {
                        left: panel_x as i32,
                        top: panel_y as i32,
                        right: (panel_x + panel_w) as i32,
                        bottom: (panel_y + panel_h) as i32,
                    },
                    color: [235, 235, 245],
                });
            }
            let left = self.scroll_palette_input(&mut placements, text_x, text_w, prefix.len(), &input);
            self.palette_line = Some((self.palette_line_rect(), left, prefix.len()));
            let clip = (text_x, text_x + text_w);
            self.palette_caret(quads, (sw, sh), (left, panel_y + pad), clip, prefix.len(), &input);
            return placements;
        }

        let prefix = match mode {
            PaletteMode::Commands => "\u{203a} ",
            PaletteMode::Run => "\u{2b95} ",
        };
        lines.push(vec![(format!("{prefix}{query}"), [235, 235, 245])]);
        match mode {
            PaletteMode::Run => lines.push(vec![(
                "  (Enter to run in a new tab)".to_string(),
                [150, 150, 160],
            )]),
            PaletteMode::Commands if n == 0 => lines.push(vec![(
                "  (no matching commands)".to_string(),
                [150, 150, 160],
            )]),
            PaletteMode::Commands => {
                let end = (scroll + PALETTE_VISIBLE).min(n);
                for (offset, (id, title, chord)) in results[scroll..end].iter().enumerate() {
                    let i = scroll + offset;
                    let prefix = if i == sel { "\u{25b8} " } else { "  " };
                    let chord = chord.clone().unwrap_or_default();
                    let left = format!("{prefix}{title}");
                    let used = left.chars().count() + chord.chars().count();
                    let gap = usable_cols
                        .saturating_sub(used)
                        .max(if chord.is_empty() { 0 } else { 1 });
                    let mut spans = vec![(format!("{left}{}", " ".repeat(gap)), [235, 235, 245])];
                    if !chord.is_empty() {
                        spans.push((chord, [140, 140, 155]));
                    }
                    lines.push(spans);
                    self.palette_rows.push((
                        Rect {
                            x: panel_x,
                            y: panel_y + pad + (1 + offset) as f32 * line_h,
                            w: panel_w,
                            h: line_h,
                        },
                        i,
                        id.clone(),
                    ));
                }
            }
        }

        while self.palette_buffers.len() < lines.len() {
            let b = Buffer::new(&mut self.text.font_system, metrics);
            self.palette_buffers.push(b);
        }
        let text_x = panel_x + pad;
        let text_w = (panel_w - pad * 2.0).max(1.0);
        let mut placements = Vec::with_capacity(lines.len());
        for (i, spans) in lines.iter().enumerate() {
            let buf = &mut self.palette_buffers[i];
            buf.set_metrics(metrics);
            buf.set_wrap(if i == 0 { Wrap::None } else { Wrap::WordOrGlyph });
            buf.set_size(Some(text_w), Some(line_h));
            buf.set_rich_text(
                spans.iter().map(|(t, c)| (t.as_str(), attrs_for(*c))),
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.text.font_system, false);
            placements.push(Placement {
                idx: i,
                left: text_x,
                top: panel_y + pad + i as f32 * line_h,
                bounds: TextBounds {
                    left: panel_x as i32,
                    top: panel_y as i32,
                    right: (panel_x + panel_w) as i32,
                    bottom: (panel_y + panel_h) as i32,
                },
                color: [235, 235, 245],
            });
        }
        let left = self.scroll_palette_input(&mut placements, text_x, text_w, prefix.len(), &input);
        self.palette_line = Some((self.palette_line_rect(), left, prefix.len()));
        let clip = (text_x, text_x + text_w);
        self.palette_caret(quads, (sw, sh), (left, panel_y + pad), clip, prefix.len(), &input);
        placements
    }

    /// Scroll the palette's input line (placement 0, unwrapped) left so the
    /// caret stays inside the `text_w` wide text area at `text_x`, clipping
    /// what scrolls out. Returns the line's new left edge.
    fn scroll_palette_input(
        &self,
        placements: &mut [Placement],
        text_x: f32,
        text_w: f32,
        prefix: usize,
        input: &LineEdit,
    ) -> f32 {
        let Some(buf) = self.palette_buffers.first() else {
            return text_x;
        };
        let caret = glyph_x(buf, prefix + input.byte_of(input.cursor()));
        let left = text_x - input_shift(caret, 2.0 * self.scale, text_w);
        if let Some(p) = placements.first_mut() {
            p.left = left;
            p.bounds.left = text_x.floor() as i32;
            p.bounds.right = (text_x + text_w).ceil() as i32;
        }
        left
    }

    /// The caret and selection on the palette's input line (buffer 0, shaped,
    /// drawn at `origin`), whose text starts `prefix` bytes in. The selection
    /// is clipped to the text area's `(left, right)`.
    fn palette_caret(
        &self,
        quads: &mut Vec<QuadInstance>,
        (sw, sh): (f32, f32),
        (x, y): (f32, f32),
        (clip_l, clip_r): (f32, f32),
        prefix: usize,
        input: &LineEdit,
    ) {
        let Some(buf) = self.palette_buffers.first() else {
            return;
        };
        let at = |col: usize| x + glyph_x(buf, prefix + input.byte_of(col));
        let band = |x0: f32, w: f32| Rect { x: x0, y, w, h: self.cell_h };
        if let Some((s, e)) = input.selection() {
            let (x0, x1) = (at(s).clamp(clip_l, clip_r), at(e).clamp(clip_l, clip_r));
            quads.push(rect_quad(band(x0, x1 - x0), sw, sh, self.chrome.accent, 0.35));
        }
        let caret = band(at(input.cursor()), 2.0 * self.scale);
        quads.push(rect_quad(caret, sw, sh, self.chrome.accent, 0.9));
    }

    fn render(&mut self) -> Result<()> {
        // Drain a bounded slice of child output; anything past the budget is left
        // for the next frame so a flood can't stretch one frame indefinitely.
        let asking = self.app.confirm().is_some();
        let (changed, more) = self.app.pump_all_budgeted(PUMP_BUDGET);
        if changed {
            self.dirty = true;
        }
        // A file job that finished asked a question (Delete): the palette
        // would swallow its buttons.
        if !asking && self.app.confirm().is_some() {
            self.palette = None;
        }
        self.drop_stale_menu();
        self.frame_pending = more;
        self.next_frame = Instant::now() + FRAME_INTERVAL;
        // Advance the auto-read dwell; a status change needs a redraw.
        if self.app.tick_inbox() {
            self.dirty = true;
        }
        // Tell views when keyboard focus moves between them (workspace/tab/pane
        // switch), e.g. so an editor autosaves on blur.
        let focused = self.app.focused_surface();
        if focused != self.last_focused_surface {
            if let Some(prev) = self.last_focused_surface {
                self.view_event(prev, |v, cx| v.focus_changed(cx, false));
            }
            if let Some(next) = focused {
                self.view_event(next, |v, cx| v.focus_changed(cx, true));
                self.app.note_focus(next);
            }
            self.last_focused_surface = focused;
        }
        self.fire_view_timers();
        // A modal that opened (from the keyboard, say) takes hover from the
        // view under the pointer.
        let may_hover = self.may_hover();
        let left = self.hover.restrict(self.mouse_down, may_hover);
        self.send_leave(left);
        if !self.dirty {
            return Ok(());
        }
        // The layout may have changed since the pointer last moved (a split, a
        // close, a drag let go): light the divider under it now.
        let over = self.divider_under_cursor();
        self.dividers.hover(over.as_ref());

        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let workspace = self.workspace_rect();
        let panes = self.active_panes(workspace);

        // Resolve each pane's tab-strip height and its content rect below it.
        let mut resolved: Vec<(PaneRender, f32, Rect)> = Vec::with_capacity(panes.len());
        for pr in panes {
            let strip_h = if self.strip_shown(pr.surfaces.len()) {
                self.strip_height()
            } else {
                0.0
            };
            let term = Rect {
                x: pr.rect.x,
                y: pr.rect.y + strip_h,
                w: pr.rect.w,
                h: (pr.rect.h - strip_h).max(1.0),
            };
            resolved.push((pr, strip_h, term));
        }

        let metrics = self.metrics();
        // Bound shaping work per frame: cache hits are free, but cache misses
        // (never-seen rows, e.g. a fast scroll into history) are shaped only
        // while under budget. Overflow rows are deferred to later frames.
        self.text.begin_frame(self.metrics_gen, metrics, self.cell_h, SHAPE_BUDGET);
        let mut frame = Frame::new((sw, sh));
        let ui = self.ui();
        let mut strip_placements: Vec<Placement> = Vec::new();
        let mut strip_idx = 0usize;
        let multi_pane = resolved.len() > 1;
        let modal = self.palette.is_some()
            || self.menu.is_some()
            || self.app.picker_open()
            || self.app.confirm().is_some();

        // Close-button chrome: one shared '×' glyph placed at every tab/workspace
        // close button, and the hit rects those buttons occupy (rebuilt each frame).
        self.buttons.clear();
        self.picker_panel = None;
        let mut close_placements: Vec<Placement> = Vec::new();
        let active_vt = self.app.tree.active_vtab();
        self.close_buffer.set_metrics(metrics);
        self.close_buffer
            .set_size(Some(self.cell_w * 2.0), Some(self.cell_h));
        self.close_buffer.set_rich_text(
            std::iter::once(("\u{00d7}", attrs_for(theme::GLYPH))),
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.close_buffer
            .shape_until_scroll(&mut self.text.font_system, false);

        let mut painted: Vec<SurfaceId> = Vec::new();
        for (pr, strip_h, term) in &resolved {
            // Horizontal tab strip (autohidden for single-surface panes).
            if *strip_h > 0.0 {
                let n = pr.surfaces.len().max(1);
                let tab_w = pr.rect.w / n as f32;
                frame.bg.push(rect_quad(
                    Rect {
                        x: pr.rect.x,
                        y: pr.rect.y,
                        w: pr.rect.w,
                        h: *strip_h,
                    },
                    sw,
                    sh,
                    self.chrome.sidebar,
                    1.0,
                ));
                let pad = 6.0 * self.scale;
                let close_w = self.cell_h;
                let text_top = pr.rect.y + (*strip_h - self.cell_h) * 0.5;
                for (i, (sid, title, is_active)) in pr.surfaces.iter().enumerate() {
                    let tab_x = pr.rect.x + i as f32 * tab_w;
                    if *is_active {
                        frame.bg.push(rect_quad(
                            Rect {
                                x: tab_x,
                                y: pr.rect.y,
                                w: tab_w,
                                h: *strip_h,
                            },
                            sw,
                            sh,
                            self.chrome.accent,
                            0.5,
                        ));
                    }
                    let close_x = tab_x + tab_w - close_w - pad;
                    if strip_idx >= self.strip_buffers.len() {
                        self.strip_buffers
                            .push(Buffer::new(&mut self.text.font_system, metrics));
                    }
                    let unsaved = self.app.view(*sid).is_some_and(|v| v.modified());
                    // Amber when the file changed on disk under unsaved edits.
                    let changed = self.app.view(*sid).is_some_and(|v| v.marked_changed_on_disk());
                    let title_color = if changed { theme::WARN } else { [220, 220, 230] };
                    let title_attrs = Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgb(title_color[0], title_color[1], title_color[2]));
                    let buf = &mut self.strip_buffers[strip_idx];
                    buf.set_metrics(metrics);
                    // Leave room on the right for the close button.
                    buf.set_size(Some((close_x - (tab_x + pad)).max(1.0)), Some(self.cell_h));
                    buf.set_rich_text(
                        std::iter::once((title.as_str(), title_attrs)),
                        &Attrs::new().family(Family::SansSerif),
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.text.font_system, false);
                    strip_placements.push(Placement {
                        idx: strip_idx,
                        left: tab_x + pad,
                        top: text_top,
                        bounds: TextBounds {
                            left: tab_x as i32,
                            top: pr.rect.y as i32,
                            right: close_x as i32,
                            bottom: (pr.rect.y + *strip_h) as i32,
                        },
                        color: title_color,
                    });
                    strip_idx += 1;
                    // Close ('×') button for this tab.
                    if let Some(vt) = active_vt {
                        let hit = Rect {
                            x: close_x,
                            y: pr.rect.y,
                            w: close_w,
                            h: *strip_h,
                        };
                        let tab = Rect { x: tab_x, y: pr.rect.y, w: tab_w, h: *strip_h };
                        let hover = |r: Rect| rect_contains(r, self.cursor.0, self.cursor.1);
                        let slot = close_slot(hit, unsaved, hover(tab), hover(hit), self.cell_h, (sw, sh));
                        frame.bg.extend(slot.quads);
                        if let Some(color) = slot.cross {
                            close_placements.push(Placement {
                                idx: 0,
                                left: close_x + (close_w - self.cell_w) * 0.5,
                                top: text_top,
                                bounds: TextBounds {
                                    left: close_x as i32,
                                    top: pr.rect.y as i32,
                                    right: (close_x + close_w) as i32,
                                    bottom: (pr.rect.y + *strip_h) as i32,
                                },
                                color,
                            });
                        }
                        self.buttons.push((hit, ButtonAction::CloseSurface(vt, pr.id, *sid)));
                    }
                }
            }

            // No surface, or an empty tab: draw the "nothing open" screen.
            let active_sid = pr
                .surfaces
                .iter()
                .find(|(_, _, a)| *a)
                .map(|(s, _, _)| *s)
                .filter(|s| !self.app.surface_is_empty(*s));
            let Some(sid) = active_sid else {
                self.build_empty_pane(&mut frame, *term, active_vt, pr.id);
                if pr.focused && multi_pane {
                    push_border(&mut frame.top, pr.rect, sw, sh, self.chrome.accent);
                }
                continue;
            };
            if let Some(view) = self.app.view_mut(sid) {
                // The view paints everything below the strip; it sizes itself
                // (e.g. resizes its PTY) from the rect it gets.
                let mut cx = PaintCx::new(
                    &self.cfg,
                    &self.chrome,
                    ui,
                    self.cursor,
                    &mut self.text,
                    &mut frame,
                    Layer::Base,
                    sid,
                );
                cx.focused = pr.focused;
                cx.modal = modal;
                view.paint(&mut cx, *term);
                painted.push(sid);
                if pr.focused && multi_pane {
                    push_border(&mut frame.top, pr.rect, sw, sh, self.chrome.accent);
                }
                let owned = frame.view_buttons.drain(..);
                self.buttons.extend(owned.map(|(r, s, id)| (r, ButtonAction::View(s, id))));
            }
        }
        // Rows the shaping budget deferred need a follow-up frame.
        let deferred = self.text.finish_paint();
        self.update_shown(&painted);
        // A view that just went off screen (a keyboard tab or workspace switch)
        // no longer has the pointer.
        let may_hover = self.may_hover();
        let left = self.hover.restrict(self.mouse_down, may_hover);
        self.send_leave(left);
        match frame.cursor {
            Some((r, icon)) => {
                self.cursor_rect = Some(r);
                self.set_cursor_icon(icon);
            }
            None => {
                self.cursor_rect = None;
                self.set_cursor_icon(CursorIcon::Default);
            }
        }
        // A divider under the pointer, or being dragged, lights up and sets
        // the cursor (a drag keeps it wherever the pointer goes).
        let highlight = self.dividers.highlight(&self.app.tree, &self.divider_frame());
        if let Some((d, dragging)) = highlight.filter(|_| !modal) {
            let alpha = if dragging { 0.9 } else { 0.6 };
            frame.bg.push(rect_quad(d.strip, sw, sh, self.chrome.accent, alpha));
            self.set_cursor_icon(divider_icon(d.axis));
            self.cursor_rect = (!dragging).then(|| d.grab_rect(DIVIDER_SLOP * self.scale));
        }

        // Palette overlay: dim + panel + selection quads (drawn after terminal
        // text), and its text (drawn last, via a second renderer).
        // Sidebar (bg quads before text; its names join the main text pass).
        let sidebar_placements = self.build_sidebar(sw, sh, &mut frame.bg, &mut close_placements);
        // Custom title-bar strip (macOS); a no-op elsewhere. Drawn after the
        // sidebar so its strip covers the sidebar's top edge cleanly.
        self.title_place = None;
        self.build_titlebar(sw, sh, &mut frame.bg);

        let palette_placements = if self.palette.is_some() {
            self.build_palette(sw, sh, &mut frame.top)
        } else {
            Vec::new()
        };
        // Floating picker and confirmation (modal overlays). Their text joins the
        // overlay pass.
        self.build_picker(&mut frame);
        self.build_confirm(&mut frame);
        self.build_tooltip(&mut frame);
        // The menu draws after every other overlay, quads and text, so one
        // opened from the picker's view shows above the picker.
        let mut menu_quads = Vec::new();
        let menu_placements = if self.menu.is_some() {
            self.build_menu(sw, sh, &mut menu_quads)
        } else {
            Vec::new()
        };

        self.images.prepare(
            &self.device,
            &self.queue,
            &frame.images,
            &frame.overlay_images,
            self.config.width,
            self.config.height,
        );
        let n_bg = frame.bg.len() as u32;
        let n_top = frame.top.len();
        let n_menu = menu_quads.len();
        let mut quads = std::mem::take(&mut frame.bg);
        quads.extend_from_slice(&frame.top);
        quads.extend_from_slice(&menu_quads);
        self.upload_quads(&quads);

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        // One-shot atlas warm after a metrics change: shape the common glyph set
        // and hand it to `prepare` far off-screen (so it rasterises into the
        // atlas but is clipped away by the GPU, never drawn). Unbounded text
        // bounds keep `prepare` from culling the glyphs before they reach the
        // atlas. Cheap: happens once per font-size/scale change, not per frame.
        let warm_area = if self.atlas_warmed_gen != Some(self.metrics_gen) {
            self.warm_buffer.set_metrics(metrics);
            self.warm_buffer.set_size(Some(f32::MAX), Some(self.cell_h));
            self.warm_buffer.set_rich_text(
                std::iter::once((ATLAS_WARM_GLYPHS, attrs_for([220, 220, 230]))),
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            self.warm_buffer
                .shape_until_scroll(&mut self.text.font_system, false);
            self.atlas_warmed_gen = Some(self.metrics_gen);
            Some(TextArea {
                buffer: &self.warm_buffer,
                left: 0.0,
                top: -1.0e6,
                scale: 1.0,
                bounds: TextBounds {
                    left: i32::MIN,
                    top: i32::MIN,
                    right: i32::MAX,
                    bottom: i32::MAX,
                },
                default_color: Color::rgb(220, 220, 230),
                custom_glyphs: &[],
            })
        } else {
            None
        };

        // Frame text references the row cache or the scratch pool; borrow those
        // fields alone so the font system stays free for `prepare`.
        let (row_cache, scratch) = (&self.text.row_cache, &self.text.scratch);
        let area = |p: &TextItem| {
            let buffer = match p.src {
                TextSrc::Row(key) => row_cache.buffer(key),
                TextSrc::Scratch(idx) => scratch.get(idx),
            }?;
            Some(TextArea {
                buffer,
                left: p.left,
                top: p.top,
                scale: 1.0,
                bounds: p.bounds,
                default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                custom_glyphs: &[],
            })
        };
        let mut text_areas: Vec<TextArea> = frame.text.iter().filter_map(area).collect();
        text_areas.extend(sidebar_placements.iter().map(|p| TextArea {
            buffer: &self.sidebar_buffers[p.idx],
            left: p.left,
            top: p.top,
            scale: 1.0,
            bounds: p.bounds,
            default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
            custom_glyphs: &[],
        }));
        text_areas.extend(strip_placements.iter().map(|p| TextArea {
            buffer: &self.strip_buffers[p.idx],
            left: p.left,
            top: p.top,
            scale: 1.0,
            bounds: p.bounds,
            default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
            custom_glyphs: &[],
        }));
        // Every close button shares the one shaped '×' buffer, placed per button.
        text_areas.extend(close_placements.iter().map(|p| TextArea {
            buffer: &self.close_buffer,
            left: p.left,
            top: p.top,
            scale: 1.0,
            bounds: p.bounds,
            default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
            custom_glyphs: &[],
        }));
        // Sidebar show/hide toggle chevron.
        if let Some(p) = &self.toggle_place {
            text_areas.push(TextArea {
                buffer: &self.toggle_buffer,
                left: p.left,
                top: p.top,
                scale: 1.0,
                bounds: p.bounds,
                default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                custom_glyphs: &[],
            });
        }
        // Custom title-bar label (macOS strip).
        if let Some(p) = &self.title_place {
            text_areas.push(TextArea {
                buffer: &self.title_buffer,
                left: p.left,
                top: p.top,
                scale: 1.0,
                bounds: p.bounds,
                default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                custom_glyphs: &[],
            });
        }
        text_areas.extend(warm_area);

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.text.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .context("text prepare")?;

        // The palette and the picker are mutually exclusive; each draws above
        // everything but the menu via the overlay text renderer (each from its
        // own pool).
        let overlay_areas: Vec<TextArea> = if !palette_placements.is_empty() {
            palette_placements
                .iter()
                .map(|p| TextArea {
                    buffer: &self.palette_buffers[p.idx],
                    left: p.left,
                    top: p.top,
                    scale: 1.0,
                    bounds: p.bounds,
                    default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                    custom_glyphs: &[],
                })
                .collect()
        } else {
            frame.overlay_text.iter().filter_map(area).collect()
        };
        let has_overlay_text = !overlay_areas.is_empty();
        if !overlay_areas.is_empty() {
            self.palette_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.text.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    overlay_areas,
                    &mut self.swash_cache,
                )
                .context("overlay prepare")?;
        }
        // The menu's text, in a pass of its own after its quads.
        if !menu_placements.is_empty() {
            let areas = menu_placements.iter().map(|p| TextArea {
                buffer: &self.menu_buffers[p.idx],
                left: p.left,
                top: p.top,
                scale: 1.0,
                bounds: p.bounds,
                default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                custom_glyphs: &[],
            });
            self.menu_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.text.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    areas,
                    &mut self.swash_cache,
                )
                .context("menu prepare")?;
        }
        // No text pass references the row cache any more: trim it to its cap.
        self.text.row_cache.end_frame();

        let target = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(())
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self
                    .instance
                    .create_surface(self.window.clone())
                    .context("recreate surface")?;
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            other => return Err(anyhow::anyhow!("surface acquire failed: {other:?}")),
        };
        let view = target
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        let bg = self.chrome.background;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Divider/background behind panes.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: srgb_to_linear(bg[0]),
                            g: srgb_to_linear(bg[1]),
                            b: srgb_to_linear(bg[2]),
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if n_bg > 0 {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, 0..n_bg);
            }
            self.images.draw_base(&mut pass, self.config.width, self.config.height);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("text render")?;
            let total = (n_bg as usize + n_top) as u32;
            if total > n_bg {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, n_bg..total);
            }
            self.images.draw_overlay(&mut pass, self.config.width, self.config.height);
            if has_overlay_text {
                self.palette_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .context("overlay render")?;
            }
            if n_menu > 0 {
                let (start, end) = ((n_bg as usize + n_top) as u32, (n_bg as usize + n_top + n_menu) as u32);
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, start..end);
            }
            // Only what was prepared this frame: a renderer replays its last
            // prepared text otherwise.
            if !menu_placements.is_empty() {
                self.menu_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .context("menu render")?;
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(target);
        self.atlas.trim();
        // Rows left unshaped by this frame's budget need another frame to finish;
        // keep the surface dirty and paced so the shaping backlog drains.
        let again = deferred || frame.wants_frame;
        self.dirty = again;
        if again {
            self.frame_pending = true;
        }
        Ok(())
    }

    fn upload_quads(&mut self, quads: &[QuadInstance]) {
        if quads.is_empty() {
            return;
        }
        let needed = quads.len() as u64;
        if needed > self.quad_capacity {
            self.quad_capacity = needed.next_power_of_two();
            self.quad_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("quad-instances"),
                size: self.quad_capacity * std::mem::size_of::<QuadInstance>() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue
            .write_buffer(&self.quad_buffer, 0, bytemuck::cast_slice(quads));
    }
}

/// A chrome text buffer's placement for this frame (sidebar / strip / palette),
/// indexing that widget's own buffer pool.
struct Placement {
    idx: usize,
    left: f32,
    top: f32,
    bounds: TextBounds,
    color: [u8; 3],
}

/// Whether `key` is a modifier (or lock) key, which makes no text of its own.
fn is_modifier_key(key: &WKey) -> bool {
    use NamedKey::*;
    matches!(
        key,
        WKey::Named(
            Shift | Control | Alt | AltGraph | Super | Meta | Hyper | CapsLock | NumLock | ScrollLock | Fn | FnLock
        )
    )
}

/// Break `text` at spaces into lines no wider than `max_w` as `width_of`
/// measures them. A word wider than that gets a line of its own.
fn wrap_words(text: &str, max_w: f32, mut width_of: impl FnMut(&str) -> f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ').filter(|w| !w.is_empty()) {
        if line.is_empty() {
            line.push_str(word);
            continue;
        }
        let candidate = format!("{line} {word}");
        if width_of(&candidate) <= max_w {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        }
    }
    lines.push(line);
    lines
}

fn attrs_for<'a>(color: [u8; 3]) -> Attrs<'a> {
    Attrs::new()
        .family(Family::Monospace)
        .color(Color::rgb(color[0], color[1], color[2]))
}

/// Translate a winit key event into the backend-neutral [`KeyPress`] views and
/// terminals take. With Cmd held, a character key is its unshifted base key
/// (so `cmd+shift+/` reads as `/`). `None` for keys with no mapping (a lone
/// modifier, media keys, ...).
fn winit_key_press(event: &winit::event::KeyEvent, mods: Mods) -> Option<KeyPress> {
    let text = event.text.as_ref().map(|s| s.to_string());
    let logical = if mods.super_ {
        base_key(event)
    } else {
        event.logical_key.clone()
    };
    let key = match &logical {
        WKey::Named(named) => match named {
            NamedKey::Enter => Key::Enter,
            NamedKey::Tab => Key::Tab,
            NamedKey::Backspace => Key::Backspace,
            NamedKey::Escape => Key::Escape,
            NamedKey::Delete => Key::Delete,
            NamedKey::Insert => Key::Insert,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::Home => Key::Home,
            NamedKey::End => Key::End,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Space => Key::Char(' '),
            NamedKey::F1 => Key::Function(1),
            NamedKey::F2 => Key::Function(2),
            NamedKey::F3 => Key::Function(3),
            NamedKey::F4 => Key::Function(4),
            NamedKey::F5 => Key::Function(5),
            NamedKey::F6 => Key::Function(6),
            NamedKey::F7 => Key::Function(7),
            NamedKey::F8 => Key::Function(8),
            NamedKey::F9 => Key::Function(9),
            NamedKey::F10 => Key::Function(10),
            NamedKey::F11 => Key::Function(11),
            NamedKey::F12 => Key::Function(12),
            _ => return None,
        },
        WKey::Character(s) => Key::Char(s.chars().next()?),
        _ => return None,
    };
    Some(KeyPress { key, mods, text })
}

/// How far to scroll a single-line input left so a caret `caret_w` wide at
/// `caret_x` (px from the line's start) fits in `width`.
fn input_shift(caret_x: f32, caret_w: f32, width: f32) -> f32 {
    (caret_x + caret_w - width).max(0.0)
}

/// The caret column in `0..=len` whose position `edge(col)` is nearest `x`.
fn nearest_caret(len: usize, x: f32, edge: impl Fn(usize) -> f32) -> usize {
    (0..=len)
        .min_by(|&a, &b| (edge(a) - x).abs().total_cmp(&(edge(b) - x).abs()))
        .unwrap_or(0)
}

/// The column of the character whose cell contains `x` (the nearest one when
/// `x` is off either end); 0 for empty text.
fn char_under(len: usize, x: f32, edge: impl Fn(usize) -> f32) -> usize {
    (0..len).rev().find(|&col| edge(col) <= x).unwrap_or(0)
}

/// The x offset (px) where the glyph at byte `byte` of `buf`'s first line
/// starts, or the line's end past its last glyph.
fn glyph_x(buf: &Buffer, byte: usize) -> f32 {
    let Some(run) = buf.layout_runs().next() else {
        return 0.0;
    };
    run.glyphs
        .iter()
        .find(|g| g.start >= byte)
        .map(|g| g.x)
        .unwrap_or(run.line_w)
}

/// A path's last component for a button label (the full path for a root like `/`),
/// shortened with an ellipsis past [`PATH_LABEL_MAX`] characters.
fn path_label(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    if name.chars().count() <= PATH_LABEL_MAX {
        return name;
    }
    let mut short: String = name.chars().take(PATH_LABEL_MAX - 1).collect();
    short.push('…');
    short
}

/// The key with Shift's symbol transform undone (Shift+/ is `/`, not `?`), so
/// chords match what is printed on the key.
fn base_key(event: &winit::event::KeyEvent) -> WKey {
    #[cfg(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    ))]
    {
        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
        event.key_without_modifiers()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(unix, not(target_os = "macos"))
    )))]
    {
        event.logical_key.clone()
    }
}

/// Format a canonical chord ("cmd+shift+t") as macOS symbols ("⌘⇧T") for display.
fn pretty_chord(chord: &str) -> String {
    let mut mods = String::new();
    let mut key = "";
    for part in chord.split('+') {
        match part {
            "cmd" | "meta" | "win" => mods.push('\u{2318}'),      // ⌘
            "shift" => mods.push('\u{21e7}'),                     // ⇧
            "ctrl" => mods.push('\u{2303}'),                      // ⌃
            "alt" | "opt" | "option" => mods.push('\u{2325}'),   // ⌥
            k => key = k,
        }
    }
    let key = if key.chars().count() == 1 {
        key.to_uppercase()
    } else {
        key.to_string()
    };
    format!("{mods}{key}")
}

/// Parse a palette-entered argument string into a typed [`Value`] per its kind.
/// Returns `None` when the input doesn't fit the kind (the palette re-prompts).
fn parse_arg_value(kind: &ArgKind, s: &str) -> Option<Value> {
    match kind {
        ArgKind::Str => Some(Value::Str(s.to_string())),
        ArgKind::Int => s.trim().parse::<i64>().ok().map(Value::Int),
        ArgKind::Bool => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "1" | "on" => Some(Value::Bool(true)),
            "false" | "no" | "n" | "0" | "off" => Some(Value::Bool(false)),
            _ => None,
        },
        ArgKind::Enum(vs) => {
            let s = s.trim();
            vs.iter()
                .any(|v| v == s)
                .then(|| Value::Str(s.to_string()))
        }
    }
}

/// The sidebar rect for `side` given its width and the surface size (physical px).
fn sidebar_rect_for(side: Side, width: f32, sw: f32, sh: f32) -> Rect {
    let x = match side {
        Side::Left => 0.0,
        Side::Right => (sw - width).max(0.0),
    };
    Rect {
        x,
        y: 0.0,
        w: width,
        h: sh,
    }
}

/// The cursor over a divider between `axis` children.
fn divider_icon(axis: Axis) -> CursorIcon {
    match axis {
        Axis::LeftRight => CursorIcon::ColResize,
        Axis::TopBottom => CursorIcon::RowResize,
    }
}

/// The workspace rect (everything the sidebar doesn't occupy) for `side`.
fn workspace_rect_for(side: Side, width: f32, sw: f32, sh: f32) -> Rect {
    let x = match side {
        Side::Left => width,
        Side::Right => 0.0,
    };
    Rect {
        x,
        y: 0.0,
        w: (sw - width).max(1.0),
        h: sh,
    }
}

fn status_color(status: TabStatus) -> [u8; 3] {
    match status {
        TabStatus::Read => [90, 90, 100],
        TabStatus::Busy => [210, 180, 60],
        TabStatus::Unread { success: true } => theme::ON,
        TabStatus::Unread { success: false } => [200, 80, 80],
        TabStatus::NeedsInput => [210, 120, 40],
    }
}

/// A compact directory label that fits in `max` characters, no wrap/overflow:
/// prefer "parent/current", else "current", else "curr…" truncated with an
/// ellipsis. `~` is substituted for the home directory's own segment.
fn shorten_dir(path: &std::path::Path, max: usize) -> String {
    let seg = |p: &std::path::Path| -> Option<String> {
        p.file_name().and_then(|s| s.to_str()).map(str::to_string)
    };
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let name = if Some(path) == home.as_deref() {
        "~".to_string()
    } else {
        seg(path).unwrap_or_else(|| path.to_string_lossy().into_owned())
    };
    let parent = path.parent().and_then(|p| {
        if Some(p) == home.as_deref() {
            Some("~".to_string())
        } else {
            seg(p)
        }
    });
    if let Some(par) = parent {
        let combined = format!("{par}/{name}");
        if combined.chars().count() <= max {
            return combined;
        }
    }
    if name.chars().count() <= max {
        return name;
    }
    if max <= 1 {
        return "…".to_string();
    }
    let head: String = name.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Locale for cosmic-text font fallback, derived from `$LANG` (e.g.
/// `en_US.UTF-8` → `en-US`), defaulting to `en-US`.
fn font_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|l| l.split('.').next().map(|s| s.replace('_', "-")))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}

/// A minimal font DB holding just the system monospace (and a sans for chrome),
/// loaded straight from known macOS paths so boot doesn't wait on the ~800ms
/// full system-font scan. Returns the DB and the monospace family to pin, or
/// `None` if no monospace candidate was found (caller falls back to a full load).
fn curated_font_db() -> Option<(glyphon::cosmic_text::fontdb::Database, String)> {
    use glyphon::cosmic_text::fontdb;
    let mut db = fontdb::Database::new();
    // Preference order: SF Mono (current macOS default), then long-standing
    // fallbacks that are reliably present.
    let mono_candidates = [
        "/System/Library/Fonts/SFNSMono.ttf",
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Monaco.ttf",
        "/System/Library/Fonts/Courier.ttc",
    ];
    let mut mono_family = None;
    for path in mono_candidates {
        // The DB is empty until the first success, so the loaded face is first.
        if db.load_font_file(path).is_ok() {
            if let Some(face) = db.faces().next() {
                if let Some((name, _)) = face.families.first() {
                    mono_family = Some(name.clone());
                    break;
                }
            }
        }
    }
    let mono_family = mono_family?;
    // A sans-serif for tab-strip titles; best-effort, ignored if absent.
    for path in ["/System/Library/Fonts/Helvetica.ttc", "/Library/Fonts/Arial.ttf"] {
        if db.load_font_file(path).is_ok() {
            break;
        }
    }
    db.set_monospace_family(mono_family.clone());
    Some((db, mono_family))
}

fn measure_cell(
    font_system: &mut FontSystem,
    scale: f32,
    font_size: f32,
    line_height: f32,
) -> (f32, f32) {
    let metrics = Metrics::new(font_size * scale, line_height * scale);
    let mut buf = Buffer::new(font_system, metrics);
    buf.set_text(
        "MMMMMMMMMM",
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    buf.shape_until_scroll(font_system, false);
    let w = buf
        .layout_runs()
        .next()
        .map(|r| r.line_w / 10.0)
        .filter(|w| *w > 0.0)
        .unwrap_or(font_size * scale * 0.6);
    (w, line_height * scale)
}

fn build_quad_pipeline(device: &wgpu::Device, format: wgpu::TextureFormat) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("quad"),
        source: wgpu::ShaderSource::Wgsl(QUAD_WGSL.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quad-layout"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quad-pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: std::mem::size_of::<QuadInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &wgpu::vertex_attr_array![
                    0 => Float32x2, 1 => Float32x2, 2 => Float32x4,
                    3 => Float32x2, 4 => Float32, 5 => Float32, 6 => Float32x4, 7 => Float32x4,
                ],
            })],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// The pane under the cursor, as [`State::pane_under_cursor`] finds it.
struct PaneHit {
    vt: VtabId,
    pid: PaneId,
    rect: Rect,
    /// How many surfaces the pane holds.
    n: usize,
    active_sid: Option<SurfaceId>,
}

/// Which view has the pointer's hover, so the view sent the last `Move` gets
/// one `Leave` when the pointer moves on, a modal takes the pointer, or the
/// pointer leaves the window.
#[derive(Default)]
struct Hover {
    view: Option<SurfaceId>,
    /// The pointer left the window during a press. The press's view still
    /// gets the drag, so it hears `Leave` on release instead.
    outside: bool,
}

impl Hover {
    /// Hover moves to `next` (`None`: no view). Returns the view that lost it.
    fn set(&mut self, next: Option<SurfaceId>) -> Option<SurfaceId> {
        let prev = std::mem::replace(&mut self.view, next);
        prev.filter(|p| Some(*p) != next)
    }

    /// Only views `allowed` may keep hover. Returns the view that lost it.
    /// Never during a `pressed` button: the view that took the press keeps
    /// hover until the release, so it never hears `Leave` mid-press.
    fn restrict(&mut self, pressed: bool, allowed: impl Fn(SurfaceId) -> bool) -> Option<SurfaceId> {
        match self.view {
            Some(v) if !pressed && !allowed(v) => self.set(None),
            _ => None,
        }
    }

    /// The pointer left the window, `pressed` or not. Returns the view that
    /// lost hover.
    fn left_window(&mut self, pressed: bool) -> Option<SurfaceId> {
        if pressed {
            self.outside = true;
            None
        } else {
            self.set(None)
        }
    }

    /// The pointer came back into the window.
    fn entered(&mut self) {
        self.outside = false;
    }

    /// The press ended. Returns the view that lost hover, if the pointer
    /// left the window during the press.
    fn released(&mut self) -> Option<SurfaceId> {
        if std::mem::take(&mut self.outside) {
            self.set(None)
        } else {
            None
        }
    }
}

// A quad is a plain fill unless it has a corner radius, border, or segment
// (see `QuadInstance`); those take a signed distance to their shape in pixels,
// antialiased over one pixel, and clipped to the quad's clip rect. Shaped
// quads grow by `PAD` px per side so the outer half of that pixel, past the
// shape's own rect, is shaded too.
const QUAD_WGSL: &str = r#"
const PAD: f32 = 1.0;
struct Inst {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) px: vec2<f32>,
    @location(4) radius: f32,
    @location(5) border_w: f32,
    @location(6) border_color: vec4<f32>,
    @location(7) clip: vec4<f32>,
};
struct VOut {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    // The border colour, or a segment's endpoints relative to the centre.
    @location(1) @interpolate(flat) extra: vec4<f32>,
    // Pixels from the rect's centre.
    @location(2) local: vec2<f32>,
    @location(3) @interpolate(flat) half: vec2<f32>,
    @location(4) @interpolate(flat) shape: vec2<f32>,
    @location(5) @interpolate(flat) clip: vec4<f32>,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
    let c = corners[vi];
    let px = abs(inst.px);
    let shaped = inst.radius != 0.0 || inst.border_w != 0.0;
    let pad = select(0.0, PAD, shaped);
    let ndc_per_px = inst.size / max(px, vec2<f32>(1e-4));
    let p = inst.pos + c * inst.size + (c * 2.0 - 1.0) * pad * ndc_per_px;
    var o: VOut;
    o.position = vec4<f32>(p, 0.0, 1.0);
    o.color = inst.color;
    o.clip = inst.clip;
    o.local = (c - vec2<f32>(0.5)) * (px + 2.0 * pad);
    o.half = px * 0.5;
    o.shape = vec2<f32>(inst.radius, inst.border_w);
    if (inst.radius < 0.0) {
        let h = vec4<f32>(o.half, o.half);
        o.extra = inst.border_color - h;
    } else {
        o.extra = inst.border_color;
    }
    return o;
}
@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    if (in.shape.x == 0.0 && in.shape.y == 0.0) {
        return in.color;
    }
    // A shaped quad draws only inside its clip (a plain fill's rect is
    // clipped before it gets here).
    let f = in.position.xy;
    if (f.x < in.clip.x || f.y < in.clip.y || f.x >= in.clip.z || f.y >= in.clip.w) {
        discard;
    }
    if (in.shape.x < 0.0) {
        // A segment from `a` to `b`, `hw` either side: round ends, or flat
        // ends exactly at `a` and `b` (an oriented box).
        let hw = -in.shape.x;
        let a = in.extra.xy;
        let b = in.extra.zw;
        let ba = b - a;
        let len = length(ba);
        var d: f32;
        if (in.shape.y > 0.0) {
            let u = select(vec2<f32>(1.0, 0.0), ba / len, len > 0.0);
            let v = vec2<f32>(-u.y, u.x);
            let m = in.local - (a + b) * 0.5;
            let q = vec2<f32>(abs(dot(m, u)) - len * 0.5, abs(dot(m, v)) - hw);
            d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0);
        } else {
            let pa = in.local - a;
            let t = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-6), 0.0, 1.0);
            d = length(pa - ba * t) - hw;
        }
        return vec4<f32>(in.color.rgb, in.color.a * clamp(0.5 - d, 0.0, 1.0));
    }
    let r = min(in.shape.x, min(in.half.x, in.half.y));
    let q = abs(in.local) - in.half + vec2<f32>(r);
    let d = length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
    let cover = clamp(0.5 - d, 0.0, 1.0);
    // How much of the pixel lies inside the border, in the fill.
    let inner = clamp(0.5 - (d + in.shape.y), 0.0, 1.0);
    // Mix border and fill premultiplied, so a translucent fill beside an
    // opaque border doesn't tint the border's edge.
    let a = mix(in.extra.a, in.color.a, inner);
    if (a <= 0.0) {
        return vec4<f32>(0.0);
    }
    let rgb = mix(in.extra.rgb * in.extra.a, in.color.rgb * in.color.a, inner) / a;
    return vec4<f32>(rgb, a * cover);
}
"#;

/// What a pane tab's close slot draws.
struct CloseSlot {
    quads: Vec<QuadInstance>,
    /// The '×' glyph's colour, when it shows.
    cross: Option<[u8; 3]>,
}

/// A pane tab's close slot `hit`: a dot while its surface has unsaved
/// changes, until the pointer is over the tab (`tab_hovered`) and the slot
/// offers to close it with '×'.
fn close_slot(
    hit: Rect,
    unsaved: bool,
    tab_hovered: bool,
    hovered: bool,
    cell_h: f32,
    screen: (f32, f32),
) -> CloseSlot {
    let (sw, sh) = screen;
    if unsaved && !tab_hovered {
        let d = (cell_h * 0.45).round();
        let dot = Rect {
            x: hit.x + ((hit.w - d) * 0.5).round(),
            y: hit.y + ((hit.h - d) * 0.5).round(),
            w: d,
            h: d,
        };
        let quads = vec![round_quad(dot, sw, sh, d * 0.5, theme::TEXT, 1.0, None)];
        return CloseSlot { quads, cross: None };
    }
    let quads = match hovered {
        true => vec![rect_quad(hover_box(hit, hit.h.min(hit.w)), sw, sh, theme::HOVER_BG, theme::HOVER_ALPHA)],
        false => Vec::new(),
    };
    CloseSlot { quads, cross: Some(if hovered { theme::GLYPH_HOVER } else { theme::GLYPH }) }
}

#[cfg(test)]
mod tests {
    use super::{
        build_quad_pipeline, char_under, glyph_x, input_shift, nearest_caret, measure_cell, rect_contains, sidebar_rect_for,
        workspace_rect_for, wrap_words, Hover, QuadInstance, divider_icon, close_slot,
    };
    use crate::plugin::theme;
    use ghostrealm_core::{Axis, Rect, Side, SurfaceId};
    use winit::window::CursorIcon;
    use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, Wrap};
    use std::time::{Duration, Instant};

    mod menus {
        use super::super::{call_view, menu_layout, Menu, MenuAction, MenuRelease, MenuRow, MenuTarget};
        use crate::app_state::{AppState, Pick, PICKER_SID};
        use crate::plugin::{testing::UI, EventCx, MenuAlign, MenuItem};
        use ghostrealm_core::{Rect, SurfaceId};

        fn centre(r: Rect) -> (f32, f32) {
            (r.x + r.w / 2.0, r.y + r.h / 2.0)
        }

        fn rows(items: Vec<MenuItem>) -> Vec<MenuRow> {
            items.into_iter().map(MenuRow::of_view).collect()
        }

        /// A menu as `open_view_menu` makes it, laid out and placed as
        /// `build_menu` does; with each row's band.
        fn open(target: MenuTarget, items: Vec<MenuItem>, hint_w: &[f32], align: MenuAlign) -> (Menu, Vec<Rect>) {
            let mut menu = Menu::new(target, (400.0, 50.0), align, rows(items), false);
            let label_w = vec![80.0; menu.items.len()];
            let layout = menu_layout(&menu.items, &label_w, hint_w, menu.anchor, align, (800.0, 600.0), UI);
            menu.place(&layout);
            (menu, layout.rows.iter().map(|r| r.rect).collect())
        }

        #[test]
        fn only_enabled_items_take_hover_and_clicks_and_fire_on_release_over_the_same_item() {
            let items = vec![
                MenuItem::new(1, "Soft wrap").check(true),
                MenuItem::RULE,
                MenuItem::new(2, "Autosave").check(false).enable(false),
                MenuItem::new(3, "Bytes"),
            ];
            let (mut m, bands) = open(MenuTarget::View(SurfaceId(5)), items, &[0.0; 4], MenuAlign::Left);
            let [wrap, rule, autosave, bytes] = [0, 1, 2, 3].map(|i| centre(bands[i]));

            assert!(m.hover_at(wrap.0, wrap.1));
            assert_eq!(m.hover, Some(0));
            assert!(!m.hover_at(wrap.0, wrap.1 + 1.0), "same item");
            for (x, y) in [rule, autosave] {
                m.hover_at(x, y);
                assert_eq!(m.hover, None, "a rule or a disabled item takes no hover");
                // A press on one stays in the menu but arms nothing.
                assert!(m.press(x, y));
                assert_eq!(m.release(x, y), MenuRelease::Unarmed);
            }
            // Released off the armed item: cancelled.
            assert!(m.press(wrap.0, wrap.1));
            assert_eq!(m.release(bytes.0, bytes.1), MenuRelease::Cancelled);
            // Released over it: chosen.
            assert!(m.press(bytes.0, bytes.1));
            assert_eq!(m.release(bytes.0, bytes.1), MenuRelease::Chosen(MenuAction::View(3)));
            assert!(m.press(wrap.0, wrap.1));
            assert_eq!(m.release(wrap.0, wrap.1), MenuRelease::Chosen(MenuAction::View(1)));
            // Outside the panel: the menu closes.
            assert!(!m.press(m.panel.x - 5.0, wrap.1));
        }

        /// The picker's file browser can open a menu (its toggles live in
        /// `⋯`); the chosen item reaches the picker's view, as `view_event`
        /// delivers it.
        #[test]
        fn a_menu_from_the_pickers_view_dispatches_to_the_picker() {
            const BTN_HIDDEN: u32 = 1;
            let mut app = AppState::new();
            app.open_picker(Pick::ActiveRoot);
            let items = vec![MenuItem::new(BTN_HIDDEN, "Hidden files").check(false), MenuItem::new(99, "Nothing")];
            let (mut m, _) = open(MenuTarget::View(PICKER_SID), items, &[0.0; 2], MenuAlign::Right);
            let cfg = ghostrealm_core::Config::default();
            let mut choose = |m: &mut Menu, i: usize| {
                let (x, y) = centre(m.rows[i]);
                assert!(m.press(x, y));
                let MenuRelease::Chosen(MenuAction::View(id)) = m.release(x, y) else {
                    panic!("item {i} wasn't chosen");
                };
                let MenuTarget::View(sid) = m.target.clone() else { panic!("a view menu") };
                let mut cx = EventCx::new(&cfg, UI, Default::default(), None);
                assert!(call_view(&mut app, sid, &mut cx, |v, cx| v.button(cx, id)), "the picker's view");
                cx.finish().redraw
            };
            assert!(choose(&mut m, 0), "the picker's browser toggled hidden files");
            assert!(!choose(&mut m, 1), "an id it doesn't know does nothing");
        }

        #[test]
        fn a_menu_closes_when_a_confirmation_appears_or_the_picker_changes_under_it() {
            let menu = |over_picker| Menu::new(MenuTarget::View(PICKER_SID), (0.0, 0.0), MenuAlign::Left, Vec::new(), over_picker);
            let (plain, over) = (menu(false), menu(true));
            assert!(!plain.stale(false, false));
            assert!(plain.stale(false, true), "a confirmation appeared");
            assert!(plain.stale(true, false), "the picker opened over it");
            assert!(!over.stale(true, false));
            assert!(over.stale(false, false), "its picker closed");
            assert!(over.stale(true, true));
        }

        #[test]
        fn a_menu_needs_a_choosable_item() {
            let ok = |items| Menu::choosable(&rows(items));
            assert!(!ok(vec![]));
            assert!(!ok(vec![MenuItem::RULE, MenuItem::new(1, "Off").enable(false)]));
            assert!(ok(vec![MenuItem::RULE, MenuItem::new(1, "On")]));
        }

        #[test]
        fn a_wide_label_gets_its_shaped_width() {
            // Wider than its character count in cells (CJK, wide glyphs).
            let items = rows(vec![MenuItem::new(1, "設定").check(true)]);
            let l = menu_layout(&items, &[300.0], &[0.0], (0.0, 0.0), MenuAlign::Left, (800.0, 600.0), UI);
            assert!(l.rows[0].label.w >= 300.0, "{:?}", l.rows[0].label);
        }

        #[test]
        fn a_right_aligned_menu_ends_at_its_anchor_and_stays_on_screen() {
            let items = || rows(vec![MenuItem::new(1, "Soft wrap")]);
            let at = |anchor, align| menu_layout(&items(), &[80.0], &[0.0], anchor, align, (800.0, 600.0), UI).panel;
            let p = at((400.0, 50.0), MenuAlign::Right);
            assert_eq!((p.x + p.w, p.y), (400.0, 50.0));
            let p = at((400.0, 50.0), MenuAlign::Left);
            assert_eq!((p.x, p.y), (400.0, 50.0));
            assert_eq!(at((10.0, 50.0), MenuAlign::Right).x, 0.0, "clamped at the left edge");
            let p = at((790.0, 590.0), MenuAlign::Left);
            assert!(p.x + p.w <= 800.0 && p.y + p.h <= 600.0, "clamped at the right and bottom");
        }

        #[test]
        fn hint_sits_right_with_the_check_left_of_it_and_a_narrow_menu_cuts_the_hint_first() {
            let items = || {
                rows(vec![
                    MenuItem::new(1, "Show file tree").check(true).with_hint("Shift Cmd Y"),
                    MenuItem::new(2, "Preview tabs").check(true),
                    MenuItem::new(3, "Off").check(false),
                ])
            };
            let (label_w, hint_w) = ([112.0, 96.0, 24.0], [90.0, 0.0, 0.0]);
            let wide = menu_layout(&items(), &label_w, &hint_w, (0.0, 0.0), MenuAlign::Left, (800.0, 600.0), UI);
            let (r0, r1, r2) = (&wide.rows[0], &wide.rows[1], &wide.rows[2]);
            let (hint, check) = (r0.hint.unwrap(), r0.check.unwrap());
            let edge = wide.panel.x + wide.panel.w - 8.0;
            assert_eq!(hint.x + hint.w, edge, "the hint is flush with the right padding");
            assert_eq!(hint.w, 90.0, "room for the whole hint");
            assert!(check.x + check.w <= hint.x, "the check mark is left of the hint");
            assert!(r0.label.x + r0.label.w <= check.x, "the label stops before the check");
            assert_eq!(r1.check.unwrap().x + r1.check.unwrap().w, edge, "no hint: the check is at the edge");
            assert!(r2.check.is_none(), "unchecked: no mark");
            assert!(r2.label.x + r2.label.w <= edge - UI.cell_h * 0.8, "but its column stays reserved");

            // Narrow: the hint is cut, then dropped; nothing goes left of the
            // padding and the label keeps its minimum.
            for sw in [300.0, 200.0, 120.0, 60.0, 10.0] {
                let l = menu_layout(&items(), &label_w, &hint_w, (0.0, 0.0), MenuAlign::Left, (sw, 600.0), UI);
                let left = l.panel.x + 8.0;
                for r in &l.rows {
                    assert!(r.label.x >= left && r.label.w >= 0.0);
                    for part in [r.hint, r.check].into_iter().flatten() {
                        assert!(part.x >= left, "at {sw}: {part:?} left of the padding");
                        assert!(part.x >= r.label.x + r.label.w, "at {sw}: overlaps the label");
                    }
                }
                let r0 = &l.rows[0];
                if let Some(h) = r0.hint {
                    assert!(h.w >= UI.cell_w && h.w <= 90.0, "too narrow to read is dropped");
                }
                let (keep, inner) = (6.0 * UI.cell_w, (l.panel.w - 16.0).max(0.0));
                assert!(r0.label.w >= keep.min(inner) - 0.01, "at {sw}: label {}", r0.label.w);
            }
            let narrow = menu_layout(&items(), &label_w, &hint_w, (0.0, 0.0), MenuAlign::Left, (200.0, 600.0), UI);
            let h = narrow.rows[0].hint;
            assert!(h.is_none_or(|h| h.w < 90.0), "the hint is cut or dropped: {h:?}");
        }
    }

    #[test]
    fn an_unsaved_pane_tab_shows_a_dot_in_its_close_slot_until_hovered() {
        let hit = Rect { x: 100.0, y: 0.0, w: 16.0, h: 28.0 };
        let screen = (800.0, 600.0);
        let saved = close_slot(hit, false, false, false, 16.0, screen);
        assert!(saved.quads.is_empty() && saved.cross == Some(theme::GLYPH));

        let unsaved = close_slot(hit, true, false, false, 16.0, screen);
        assert_eq!(unsaved.cross, None, "no cross");
        let [dot] = unsaved.quads.as_slice() else { panic!("one dot") };
        assert_eq!((dot.px, dot.radius), ([7.0, 7.0], 3.5), "a disc");

        let over_tab = close_slot(hit, true, true, false, 16.0, screen);
        assert!(over_tab.quads.is_empty() && over_tab.cross == Some(theme::GLYPH));
        let over_slot = close_slot(hit, true, true, true, 16.0, screen);
        assert_eq!((over_slot.quads.len(), over_slot.cross), (1, Some(theme::GLYPH_HOVER)));
    }

    #[test]
    fn the_resize_cursor_follows_the_divider_axis() {
        assert_eq!(divider_icon(Axis::LeftRight), CursorIcon::ColResize);
        assert_eq!(divider_icon(Axis::TopBottom), CursorIcon::RowResize);
    }

    #[test]
    fn the_palette_pointer_maps_to_carets_and_characters() {
        // Cells 10 px wide starting at x = 100.
        let edge = |col: usize| 100.0 + col as f32 * 10.0;
        assert_eq!(nearest_caret(5, 100.0, edge), 0);
        assert_eq!(nearest_caret(5, 104.0, edge), 0);
        assert_eq!(nearest_caret(5, 106.0, edge), 1);
        assert_eq!(nearest_caret(5, 20.0, edge), 0, "left of the line");
        assert_eq!(nearest_caret(5, 900.0, edge), 5, "right of the text");
        assert_eq!(char_under(5, 106.0, edge), 0);
        assert_eq!(char_under(5, 112.0, edge), 1);
        assert_eq!(char_under(5, 900.0, edge), 4, "the last character");
        assert_eq!(char_under(0, 900.0, edge), 0);
    }

    #[test]
    fn an_event_makes_the_cached_unsaved_flag_pessimistic_until_the_next_refresh() {
        let flag = super::UnsavedFlag::default();
        flag.set(false);
        flag.assume_unsaved();
        assert!(flag.get(), "a terminate request in the same pass must not answer TerminateNow");
        let shared = flag.clone();
        flag.set(false);
        assert!(!shared.get(), "the refresh is seen by the quit hook's clone");
    }

    #[test]
    fn a_held_termination_is_answered_from_the_exact_unsaved_state() {
        let flag = super::UnsavedFlag::default();
        assert_eq!(flag.settle(true), None, "nothing is held");
        // A stale `true`: nothing is unsaved after all, so go ahead.
        flag.assume_unsaved();
        flag.hold_termination();
        assert_eq!(flag.settle(false), Some(true));
        assert_eq!(flag.settle(false), None, "answered once");
        // Really unsaved: stay and ask.
        flag.hold_termination();
        assert_eq!(flag.settle(true), Some(false));
        assert!(flag.get());
    }

    /// A confirmation's message is drawn as lines that each fit the panel, so
    /// none of it (such as the warning that a buffer would be lost) is cut off.
    #[test]
    fn a_message_breaks_into_lines_that_fit() {
        let width = |s: &str| s.chars().count() as f32 * 10.0;
        let msg = "2 buffers have unsaved changes. A buffer with no file can't be saved here and would be lost.";
        let lines = wrap_words(msg, 300.0, width);
        assert!(lines.len() > 1, "{lines:?}");
        assert!(lines.iter().all(|l| width(l) <= 300.0), "{lines:?}");
        assert_eq!(lines.join(" "), msg, "every word, in order");
        // A word too wide for the panel gets its own line rather than an empty one.
        let long = "x".repeat(40);
        assert_eq!(wrap_words(&format!("a {long} b"), 300.0, width), ["a", long.as_str(), "b"]);
        assert_eq!(wrap_words("", 300.0, width), [""]);
    }

    /// Each view that had hover hears `Leave` once: when the pointer moves to
    /// another view, a modal takes the pointer, or the pointer leaves the
    /// window (on release, if a press was held).
    #[test]
    fn hover_ends_once_per_view() {
        let (a, b) = (Some(SurfaceId(1)), Some(SurfaceId(2)));
        let mut h = Hover::default();
        assert_eq!(h.set(a), None);
        assert_eq!(h.set(a), None, "moving within a view keeps its hover");
        assert_eq!(h.set(b), a, "moving on ends the last view's hover");

        assert_eq!(h.restrict(false, |_| true), None);
        assert_eq!(h.restrict(true, |_| false), None, "not mid-press");
        assert_eq!(h.restrict(false, |s| s != SurfaceId(2)), b, "a modal or a hidden view ends it");
        assert_eq!(h.restrict(false, |_| false), None, "once");

        h.set(a);
        assert_eq!(h.left_window(false), a, "leaving the window ends it");
        assert_eq!(h.left_window(false), None);

        h.set(a);
        assert_eq!(h.left_window(true), None, "not mid-press");
        assert_eq!(h.released(), a, "on release, outside the window");
        assert_eq!(h.released(), None);

        h.set(a);
        h.left_window(true);
        h.entered();
        assert_eq!(h.released(), None, "back inside before release");

        // A press whose release was lost keeps hover until it is ended.
        let hidden = |_: SurfaceId| false;
        h.set(a);
        assert_eq!(h.restrict(true, hidden), None);
        assert_eq!(h.released(), None);
        assert_eq!(h.restrict(false, hidden), a, "withdrawn once the press has ended");
    }

    #[test]
    fn sidebar_and_workspace_split_by_side() {
        let (w, sw, sh) = (190.0, 1000.0, 600.0);

        let sb = sidebar_rect_for(Side::Left, w, sw, sh);
        let ws = workspace_rect_for(Side::Left, w, sw, sh);
        assert_eq!((sb.x, sb.w), (0.0, 190.0), "left sidebar hugs x=0");
        assert_eq!((ws.x, ws.w), (190.0, 810.0), "left workspace starts after it");

        let sb = sidebar_rect_for(Side::Right, w, sw, sh);
        let ws = workspace_rect_for(Side::Right, w, sw, sh);
        assert_eq!((sb.x, sb.w), (810.0, 190.0), "right sidebar hugs the right edge");
        assert_eq!((ws.x, ws.w), (0.0, 810.0), "right workspace starts at x=0");

        // The two regions tile the width with no gap or overlap, either side.
        for side in [Side::Left, Side::Right] {
            let sb = sidebar_rect_for(side, w, sw, sh);
            let ws = workspace_rect_for(side, w, sw, sh);
            assert!((sb.w + ws.w - sw).abs() < 0.001, "regions tile the full width");
            assert!(sb.x >= ws.x + ws.w - 0.001 || ws.x >= sb.x + sb.w - 0.001, "no overlap");
        }
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = Rect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 30.0,
        };
        assert!(rect_contains(r, 10.0, 20.0), "top-left corner is inside");
        assert!(rect_contains(r, 60.0, 35.0), "centre is inside");
        assert!(!rect_contains(r, 110.0, 35.0), "right edge is exclusive");
        assert!(!rect_contains(r, 60.0, 50.0), "bottom edge is exclusive");
        assert!(!rect_contains(r, 9.0, 35.0), "left of the rect is outside");
    }

    const FONT_SIZE: f32 = 15.0;
    const LINE_HEIGHT: f32 = 18.0;

    #[test]
    fn curated_font_db_finds_a_monospace() {
        // On macOS the curated fast-boot DB must resolve a monospace family from
        // disk; otherwise boot silently falls back to the ~800ms full scan.
        let (_, family) = super::curated_font_db().expect("a monospace candidate on macOS");
        assert!(!family.is_empty(), "pinned monospace family must be named");
    }

    #[test]
    fn an_unwrapped_input_line_scrolls_to_keep_the_caret_in_view() {
        let mut font_system = FontSystem::new();
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let mut b = Buffer::new(&mut font_system, metrics);
        b.set_wrap(Wrap::None);
        b.set_size(Some(100.0), Some(LINE_HEIGHT));
        let text = "x".repeat(80);
        b.set_text(&text, &Attrs::new().family(Family::Monospace), Shaping::Advanced, None);
        b.shape_until_scroll(&mut font_system, false);
        assert_eq!(b.layout_runs().count(), 1, "one run, however narrow");
        let end = glyph_x(&b, text.len());
        assert!(end > 100.0, "the caret's x runs past the width: {end}");
        assert_eq!(input_shift(end, 2.0, 100.0), end + 2.0 - 100.0);
        assert_eq!(input_shift(50.0, 2.0, 100.0), 0.0, "in view: no scroll");
    }

    // An unoptimised build's shaping cost says nothing about a regression.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "timing: run with --release")]
    fn full_screen_shaping_cost() {
        let mut font_system = FontSystem::new();
        let (_cw, _ch) = measure_cell(&mut font_system, 1.0, FONT_SIZE, LINE_HEIGHT);
        let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
        let rows = 24usize;
        let cols = 80usize;
        let mut buffers: Vec<Buffer> = (0..rows)
            .map(|_| {
                let mut b = Buffer::new(&mut font_system, metrics);
                b.set_size(Some(2000.0), Some(LINE_HEIGHT));
                b
            })
            .collect();
        let line: String = "abcdefghij0123456789".chars().cycle().take(cols).collect();

        // Best of several batches: the whole suite runs shaping in parallel, so a
        // single timing is dominated by CPU contention. The fastest batch reflects
        // uncontended cost, which is what a regression would actually raise.
        let iters = 30u32;
        let batches = 5u32;
        let mut per_frame = Duration::MAX;
        for _ in 0..batches {
            let start = Instant::now();
            for _ in 0..iters {
                for b in &mut buffers {
                    b.set_text(
                        &line,
                        &Attrs::new().family(Family::Monospace),
                        Shaping::Advanced,
                        None,
                    );
                    b.shape_until_scroll(&mut font_system, false);
                }
            }
            per_frame = per_frame.min(start.elapsed() / iters);
        }
        println!("full-screen reshape: {per_frame:?}/frame ({cols}x{rows}, best of {batches})");
        assert!(
            per_frame < Duration::from_millis(50),
            "full-screen reshape unexpectedly slow: {per_frame:?}"
        );
    }

    #[test]
    fn quad_pipeline_fills_with_solid_colour() {
        pollster::block_on(offscreen_red());
    }

    /// The summed coverage of a white-on-black render, in pixels.
    fn ink(data: &[u8]) -> f32 {
        data.chunks(4).map(|p| p[0] as f32 / 255.0).sum()
    }

    const D: f32 = 64.0;

    fn render(quads: &[QuadInstance], clear: f64) -> Vec<u8> {
        let c = wgpu::Color { r: clear, g: clear, b: clear, a: 1.0 };
        pollster::block_on(render_quads(quads, D as u32, c))
    }

    /// A stroke covers its exact area (length x width plus its round ends) at
    /// icon stroke widths, at any angle and sub-pixel position.
    #[test]
    fn strokes_cover_their_exact_area() {
        use crate::plugin::paint::line_quad;
        for w in [1.0, 2.0, 3.0] {
            for (a, b) in [((10.3, 12.1), (40.7, 35.6)), ((8.6, 30.25), (50.2, 30.25)), ((20.5, 50.0), (31.0, 9.7))] {
                let q = line_quad(a, b, w, false, D, D, [255, 255, 255], 1.0);
                let len = (b.0 - a.0).hypot(b.1 - a.1);
                let exact = len * w + std::f32::consts::PI * (w / 2.0).powi(2);
                let got = ink(&render(&[q], 0.0));
                assert!((got - exact).abs() / exact < 0.04, "width {w} {a:?}-{b:?}: {got} vs {exact}");
            }
        }
    }

    /// A flat-ended bar is a rotated rect: a diamond covers its area.
    #[test]
    fn bars_cover_a_rotated_rect() {
        use crate::plugin::paint::line_quad;
        let (x, y, h) = (32.3, 31.8, 14.0);
        let q = line_quad((x - h / 2.0, y - h / 2.0), (x + h / 2.0, y + h / 2.0), h * std::f32::consts::SQRT_2, true, D, D, [255; 3], 1.0);
        let data = render(&[q], 0.0);
        let exact = 2.0 * h * h;
        assert!((ink(&data) - exact).abs() / exact < 0.02, "{} vs {exact}", ink(&data));
        assert_eq!(pixel(&data, 64, 32, 31)[0], 255, "solid in the middle");
        assert_eq!(pixel(&data, 64, 20, 20)[0], 0, "clear past the slanted side");
    }

    /// A long thin bar at 30 degrees covers its area, ends flat at its
    /// endpoints, and is solid along its axis.
    #[test]
    fn an_angled_bar_covers_its_rotated_rect() {
        use crate::plugin::paint::line_quad;
        let (len, w) = (30.0_f32, 4.0_f32);
        let (c, s) = (30f32.to_radians().cos(), 30f32.to_radians().sin());
        let mid = (32.2, 31.7);
        let a = (mid.0 - c * len / 2.0, mid.1 - s * len / 2.0);
        let b = (mid.0 + c * len / 2.0, mid.1 + s * len / 2.0);
        let data = render(&[line_quad(a, b, w, true, D, D, [255; 3], 1.0)], 0.0);
        let exact = len * w;
        assert!((ink(&data) - exact).abs() / exact < 0.02, "{} vs {exact}", ink(&data));
        assert_eq!(pixel(&data, 64, 32, 31)[0], 255, "solid at mid-length");
        // 1.5 px past `b` along the axis: a round end would cover it.
        let past = (b.0 + c * 1.5, b.1 + s * 1.5);
        assert_eq!(pixel(&data, 64, past.0 as u32, past.1 as u32)[0], 0, "flat end");
    }

    /// A plain fill at fractional coordinates covers exactly the pixels whose
    /// centres it contains, at full strength: no antialiasing.
    #[test]
    fn a_fractional_flat_rect_has_hard_edges() {
        let r = Rect { x: 8.3, y: 10.7, w: 12.3, h: 9.6 };
        let data = render(&[crate::plugin::paint::rect_quad(r, D, D, [255; 3], 1.0)], 0.0);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                let inside = cx >= r.x && cx < r.x + r.w && cy >= r.y && cy < r.y + r.h;
                assert_eq!(pixel(&data, 64, x, y)[0], if inside { 255 } else { 0 }, "({x}, {y})");
            }
        }
    }

    /// A rounded rect half outside its clip keeps its corners inside it and
    /// draws nothing outside it.
    #[test]
    fn a_clipped_rounded_rect_keeps_its_shape_inside_the_clip() {
        use crate::plugin::paint::round_quad;
        let r = Rect { x: 10.0, y: 10.0, w: 40.0, h: 30.0 };
        let whole = round_quad(r, D, D, 8.0, [255; 3], 1.0, Some((2.0, [255, 0, 0], 1.0)));
        let mut clipped = whole;
        clipped.clip = [0.0, 0.0, 30.0, 25.0];
        let (full, cut) = (render(&[whole], 0.0), render(&[clipped], 0.0));
        for y in 0..64u32 {
            for x in 0..64u32 {
                let want = if x < 30 && y < 25 { pixel(&full, 64, x, y) } else { [0, 0, 0, 255] };
                assert_eq!(pixel(&cut, 64, x, y), want, "({x}, {y})");
            }
        }
        assert_eq!(pixel(&cut, 64, 10, 10), [0, 0, 0, 255], "the corner inside the clip stays round");
    }

    /// At a fractional clip edge, a plain fill cut on the CPU and a shaped
    /// quad clipped in the shader cover the same pixels: those whose centres
    /// lie inside the clip.
    #[test]
    fn cpu_and_shader_clips_agree_at_a_fractional_edge() {
        use crate::plugin::paint::{intersect, rect_quad, round_quad};
        let r = Rect { x: 8.0, y: 8.0, w: 40.0, h: 40.0 };
        let clip = Rect { x: 12.6, y: 5.0, w: 7.8, h: 30.3 };
        let cut = rect_quad(intersect(r, clip).unwrap(), D, D, [255; 3], 1.0);
        // Square corners and a border in the fill's colour: shaped, but solid.
        let mut shaped = round_quad(r, D, D, 0.0, [255; 3], 1.0, Some((1.0, [255; 3], 1.0)));
        shaped.clip = [clip.x, clip.y, clip.x + clip.w, clip.y + clip.h];
        let (a, b) = (render(&[cut], 0.0), render(&[shaped], 0.0));
        assert_eq!(a, b);
        // The clip spans x 12.6..20.4: centres 13.5 to 19.5 are inside.
        let row: Vec<u8> = (11..22).map(|x| pixel(&a, 64, x, 20)[0]).collect();
        assert_eq!(row, [0, 0, 255, 255, 255, 255, 255, 255, 255, 0, 0]);
    }

    /// A translucent segment is one quad, so it never draws past its own
    /// alpha anywhere along its length. (Segments of a polyline overlap at
    /// their joints; icons draw opaque.)
    #[test]
    fn a_translucent_segment_never_exceeds_its_alpha() {
        use crate::plugin::paint::line_quad;
        let q = line_quad((8.4, 10.2), (55.1, 47.9), 2.0, false, D, D, [255; 3], 0.5);
        let data = render(&[q], 0.0);
        let max = data.chunks(4).map(|p| p[0]).max().unwrap();
        assert!((127..=129).contains(&max), "peak {max}");
    }

    /// A small disc keeps its area: its antialiased rim isn't cut off at the
    /// quad's edge.
    #[test]
    fn discs_keep_their_area() {
        use crate::plugin::paint::round_quad;
        for d in [2.0, 3.0, 5.5] {
            let r = Rect { x: 20.3, y: 20.6, w: d, h: d };
            let q = round_quad(r, D, D, d / 2.0, [255; 3], 1.0, None);
            let exact = std::f32::consts::PI * (d / 2.0).powi(2);
            let got = ink(&render(&[q], 0.0));
            // The one-pixel ramp runs a little over on a curve this tight.
            assert!((got - exact).abs() / exact < 0.1, "disc {d}: {got} vs {exact}");
        }
    }

    /// A shaped quad's edge at a fractional position shades the pixel it
    /// half-covers, even where that pixel's centre is outside the rect.
    #[test]
    fn a_fractional_shaped_edge_is_antialiased() {
        let r = Rect { x: 8.7, y: 8.0, w: 20.0, h: 32.0 };
        let q = crate::plugin::paint::round_quad(r, D, D, 2.0, [255; 3], 1.0, None);
        let data = render(&[q], 0.0);
        let edge = pixel(&data, 64, 8, 20)[0];
        assert!((60..=95).contains(&edge), "pixel 8 is 30% covered, got {edge}");
        assert_eq!(pixel(&data, 64, 7, 20)[0], 0);
        assert_eq!(pixel(&data, 64, 9, 20)[0], 255);
    }

    /// Border and translucent fill mix premultiplied: the fill's colour adds
    /// no more than its own alpha's worth anywhere, the border's edge included.
    #[test]
    fn a_translucent_fill_does_not_tint_its_border_edge() {
        let r = Rect { x: 8.0, y: 8.0, w: 48.0, h: 48.0 };
        let q = crate::plugin::paint::round_quad(r, D, D, 6.0, [255, 0, 0], 0.2, Some((2.5, [0, 0, 255], 1.0)));
        let data = render(&[q], 0.0);
        let red = data.chunks(4).map(|p| p[0]).max().unwrap();
        assert!(red <= 52, "red reaches {red}, past the fill's 0.2 alpha");
        assert!(pixel(&data, 64, 10, 32)[0] > 0 && pixel(&data, 64, 10, 32)[2] > 0, "the inner edge mixes both");
    }

    /// An outline (zero alpha fill) and a ring leave their interiors as they were.
    #[test]
    fn outlines_and_rings_leave_their_interior_untouched() {
        use crate::plugin::paint::round_quad;
        let outline = round_quad(Rect { x: 8.0, y: 8.0, w: 48.0, h: 30.0 }, D, D, 5.0, [255; 3], 0.0, Some((2.0, [255; 3], 1.0)));
        let ring = round_quad(Rect { x: 20.5, y: 40.5, w: 20.0, h: 20.0 }, D, D, 10.0, [255; 3], 0.0, Some((1.5, [255; 3], 1.0)));
        let data = render(&[outline, ring], 0.25);
        let bg = pixel(&data, 64, 1, 1);
        assert_eq!(pixel(&data, 64, 32, 22), bg, "outline interior");
        assert_eq!(pixel(&data, 64, 30, 50), bg, "ring interior");
        assert!(pixel(&data, 64, 32, 9)[0] > bg[0], "the outline draws");
        assert!(pixel(&data, 64, 30, 41)[0] > bg[0], "the ring draws");
    }

    /// The RGBA8 pixel at `(x, y)` of a `dim`-wide render.
    fn pixel(data: &[u8], dim: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * dim + x) * 4) as usize;
        [data[o], data[o + 1], data[o + 2], data[o + 3]]
    }

    /// A flat quad keeps hard, pixel-aligned edges: no antialiasing leaks
    /// outside it or softens its border pixels.
    #[test]
    fn flat_quads_fill_exactly_their_pixels() {
        // 64 px rows meet the copy's 256-byte row alignment.
        const DIM: u32 = 64;
        let r = Rect { x: 8.0, y: 8.0, w: 16.0, h: 16.0 };
        let q = crate::plugin::paint::rect_quad(r, DIM as f32, DIM as f32, [0, 255, 0], 1.0);
        let data = pollster::block_on(render_quads(&[q], DIM, wgpu::Color::BLACK));
        for y in 0..DIM {
            for x in 0..DIM {
                let inside = (8..24).contains(&x) && (8..24).contains(&y);
                let want = if inside { [0, 255, 0, 255] } else { [0, 0, 0, 255] };
                assert_eq!(pixel(&data, DIM, x, y), want, "pixel ({x}, {y})");
            }
        }
    }

    /// A rounded, bordered quad: clear past the rounded corner, the border
    /// colour along its edge, the fill in the middle, and an antialiased
    /// (partly covered) pixel on the curve.
    #[test]
    fn rounded_bordered_quads_draw_corner_border_and_fill() {
        const DIM: u32 = 64;
        let r = Rect { x: 8.0, y: 8.0, w: 48.0, h: 32.0 };
        let q = crate::plugin::paint::round_quad(
            r, DIM as f32, DIM as f32, 10.0, [255, 0, 0], 1.0, Some((3.0, [0, 0, 255], 1.0)),
        );
        let data = pollster::block_on(render_quads(&[q], DIM, wgpu::Color::BLACK));
        let px = |x, y| pixel(&data, DIM, x, y);
        assert_eq!(px(8, 8), [0, 0, 0, 255], "the corner pixel is clear");
        assert_eq!(px(55, 39), [0, 0, 0, 255], "the opposite corner is clear");
        assert_eq!(px(5, 20), [0, 0, 0, 255], "outside the rect is clear");
        for (x, y) in [(32, 9), (32, 38), (9, 24), (54, 24)] {
            let p = px(x, y);
            assert!(p[2] > 240 && p[0] < 15, "border at ({x}, {y}), got {p:?}");
        }
        let c = px(32, 24);
        assert!(c[0] > 240 && c[2] < 15, "fill in the middle, got {c:?}");
        // The top-left corner's arc is centred on (18, 18) with radius 10; pixel
        // (12, 9)'s centre lies 0.12 px outside it, so it is part covered.
        let edge = px(12, 9);
        assert!(edge[2] > 10 && edge[2] < 245, "antialiased curve, got {edge:?}");
    }

    /// Paint one 192x24 monospace row of `text` in `style` and rasterise it: the
    /// frame's under-text quads on the GPU, then the shaped glyphs composited on
    /// the CPU. RGBA8 pixels.
    fn draw_row(text: &str, style: crate::row_cache::RunStyle) -> Vec<u8> {
        use crate::plugin::testing::{Harness, UI};
        use crate::plugin::{Layer, PaintCx, RowStyles, TextSrc};
        use glyphon::cosmic_text::{SwashCache, SwashContent};

        const DIM: u32 = 192;
        let mut h = Harness::new();
        h.text.begin_frame(0, Metrics::new(14.0, UI.cell_h), UI.cell_h, Duration::from_millis(50));
        let clip = Rect { x: 0.0, y: 0.0, w: DIM as f32, h: DIM as f32 };
        let fg = [220, 220, 230];
        let runs = [style];
        let styles = if style.is_plain() { RowStyles::NONE } else { RowStyles { runs: &runs, cell_w: UI.cell_w } };
        {
            let mut cx = PaintCx::new(
                &h.cfg, &h.chrome, UI, (-1.0, -1.0), &mut h.text, &mut h.frame, Layer::Base, SurfaceId(1),
            );
            let key = cx.row_key_styled(std::iter::once((text, fg)), styles);
            let spans = vec![(text.to_string(), fg)];
            cx.budgeted_row_styled(key, || spans, styles, 8.0, 8.0, 180.0, clip, fg);
        }
        // The frame is laid out for an 800x600 surface: re-express its quads for the texture.
        let (sw, sh) = h.frame.screen;
        let quads: Vec<QuadInstance> = h
            .frame
            .bg
            .iter()
            .map(|q| {
                let px = |ndc: f32, span: f32| (ndc + 1.0) / 2.0 * span / DIM as f32 * 2.0 - 1.0;
                QuadInstance {
                    pos: [px(q.pos[0], sw), -px(-q.pos[1], sh)],
                    size: [q.size[0] * sw / DIM as f32, q.size[1] * sh / DIM as f32],
                    ..*q
                }
            })
            .collect();
        let bg = wgpu::Color { r: 0.02, g: 0.02, b: 0.03, a: 1.0 };
        let mut px = pollster::block_on(render_quads(&quads, DIM, bg));
        let mut swash = SwashCache::new();
        for item in &h.frame.text {
            let TextSrc::Row(key) = item.src else { continue };
            let buffer = h.text.row_cache.buffer(key).expect("shaped row");
            for run in buffer.layout_runs() {
                for g in run.glyphs {
                    let p = g.physical((item.left, item.top), 1.0);
                    let Some(img) = swash.get_image(&mut h.text.font_system, p.cache_key).as_ref() else {
                        continue;
                    };
                    if img.content != SwashContent::Mask {
                        continue;
                    }
                    let c = g.color_opt.map_or(fg, |c| [c.r(), c.g(), c.b()]);
                    for (i, a) in img.data.iter().enumerate() {
                        let x = p.x + img.placement.left + (i as u32 % img.placement.width) as i32;
                        let y = (run.line_y as i32) + p.y - img.placement.top + (i as u32 / img.placement.width) as i32;
                        if x < 0 || y < 0 || x >= DIM as i32 || y >= DIM as i32 {
                            continue;
                        }
                        let o = ((y as u32 * DIM + x as u32) * 4) as usize;
                        for k in 0..3 {
                            let old = px[o + k] as u32;
                            px[o + k] = ((c[k] as u32 * *a as u32 + old * (255 - *a as u32)) / 255) as u8;
                        }
                    }
                }
            }
        }
        px
    }

    /// Bold, italic, underline, and a code box each change the pixels of a row; a
    /// plain row draws the same every time.
    #[test]
    fn styled_rows_render_differently_from_plain() {
        use crate::row_cache::RunStyle;
        let text = "Hello world, mix";
        let plain = draw_row(text, RunStyle::PLAIN);
        assert_eq!(plain, draw_row(text, RunStyle::PLAIN), "rendering is deterministic");
        assert!(plain.chunks(4).any(|p| p[0] > 100), "the plain row drew glyphs");
        for (name, style) in [
            ("bold", RunStyle::BOLD),
            ("italic", RunStyle::ITALIC),
            ("underline", RunStyle::UNDERLINE),
            ("box", RunStyle::BOX),
        ] {
            assert_ne!(draw_row(text, style), plain, "{name} looks like plain text");
        }
        assert_ne!(draw_row(text, RunStyle::BOLD), draw_row(text, RunStyle::ITALIC));
    }

    /// Render `quads` over `clear` on a `dim`x`dim` offscreen texture; RGBA8 pixels.
    async fn render_quads(quads: &[QuadInstance], dim: u32, clear: wgpu::Color) -> Vec<u8> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect(
                "no GPU adapter for the headless render test. Next steps: run on a machine with a \
                 Metal/Vulkan/GL adapter, or gate this test behind a feature when none is present.",
            );
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("request device");

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("offscreen"),
            size: wgpu::Extent3d {
                width: dim,
                height: dim,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let pipeline = build_quad_pipeline(&device, format);

        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-quad"),
            size: (std::mem::size_of::<QuadInstance>() * quads.len().max(1)) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(quads));

        let bytes_per_row = dim * 4;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (bytes_per_row * dim) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..4, 0..quads.len() as u32);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(dim),
                },
            },
            wgpu::Extent3d {
                width: dim,
                height: dim,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(enc.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("poll");
        rx.recv().expect("map channel").expect("map ok");
        let data = slice.get_mapped_range().expect("map range");
        let out = data.to_vec();
        drop(data);
        out
    }

    async fn offscreen_red() {
        let dim = 64u32;
        let full = Rect { x: 0.0, y: 0.0, w: dim as f32, h: dim as f32 };
        let inst = crate::plugin::paint::rect_quad(full, dim as f32, dim as f32, [255, 0, 0], 1.0);
        let data = render_quads(&[inst], dim, wgpu::Color::BLACK).await;
        let bytes_per_row = dim * 4;
        let off = ((dim / 2) * bytes_per_row + (dim / 2) * 4) as usize;
        let px = [data[off], data[off + 1], data[off + 2], data[off + 3]];
        assert!(
            px[0] > 200 && px[1] < 50 && px[2] < 50 && px[3] > 200,
            "expected an opaque red centre pixel from the quad pipeline, got {px:?}."
        );
    }
}
