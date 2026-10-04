//! wgpu + winit + glyphon window hosting the multiplexer.
//!
//! Renders the active vtab's split tree: each pane's active surface is drawn in
//! its computed rect (background quads + cursor via an instanced-quad pipeline,
//! foreground text via glyphon), all in one wgpu scene. Cmd-chords run app
//! commands through the registry; other keys go to the focused surface.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_core::{
    ArgKind, ArgSpec, Args, Axis, Chrome, Config, PaneId, Rect, Registry, Side,
    SurfaceId, TabStatus, Value, VtabId,
};
use ghostrealm_terminal::{Key, KeyPress, Mods};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Wrap,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{Window, WindowId};

use crate::app_state::{build_registry, AppState, PICKER_SID, SETTINGS_VTAB_NAME};
use crate::plugin::paint::{push_border, rect_quad, srgb_to_linear, QuadInstance};
use crate::plugin::{
    hover_box, rect_contains, theme, EventCx, Font, Frame, Layer, MouseEvent, Outcome, PaintCx, Request, TextItem,
    TextKit, TextSrc, UiMetrics, View,
};
use crate::tap::TapDetector;

/// Divider gap between split panes, in physical pixels.
const DIVIDER: f32 = 6.0;
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
/// Max child-output bytes drained per surface per frame. Bounds VT-parse work so
/// one burst can't stall a frame; the remainder is pumped on following frames.
const PUMP_BUDGET: usize = 512 * 1024;
/// Soft cap on cached shaped rows. Comfortably holds several full screens so
/// scrollback and multiple surfaces stay warm; trimmed after each frame.
const ROW_CACHE_CAP: usize = 4096;
/// Scrollback lines per mouse-wheel notch.
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
/// Logical height of the custom macOS title-bar strip (points; scaled per-DPI).
const TITLE_BAR_H: f32 = 28.0;
/// Logical width reserved at the top-left for the macOS traffic-light buttons.
const TRAFFIC_LIGHT_W: f32 = 76.0;
/// Empty-workspace screen: dim hint colour.
const EMPTY_HINT: [u8; 3] = [110, 110, 125];
/// Below this focused-pane width (logical px), the file browser opens files as tabs even
/// when `[file_browser] open_in = "split"` (a split would be too cramped).
const FILE_BROWSER_SPLIT_MIN_W: f32 = 560.0;

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
}

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
        crate::macos_keys::route_help_chord(&window);
        match pollster::block_on(State::new(
            window,
            self.command_line.clone(),
            self.proxy.clone(),
            self.wake_pending.clone(),
        )) {
            Ok(s) => {
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
        let Some(state) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => {
                // Snappy quit: hide the window for instant visual feedback, then
                // exit the process outright. Graceful teardown (joining VT-worker
                // threads, wgpu/Metal device drop) is synchronous and can stall
                // for seconds — sometimes long enough that macOS marks the app
                // unresponsive. Exiting lets the OS reclaim threads and fds; the
                // closing PTY masters SIGHUP the child shells, exactly as closing
                // any terminal does. Flush unsaved editors first (autosave).
                if state.cfg.editor.autosave_on_unfocus {
                    let _ = state.app.autosave_all();
                }
                state.window.set_visible(false);
                std::process::exit(0);
            }
            WindowEvent::Focused(false) => {
                // App lost focus: autosave editors (switching to another app), and
                // hot-reload the config if it was one of them.
                if state.cfg.editor.autosave_on_unfocus && state.app.autosave_all() {
                    state.reload_config();
                }
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
            WindowEvent::CursorMoved { position, .. } => {
                state.cursor = (position.x as f32, position.y as f32);
                let mut redraw = false;
                if state.palette.is_some() {
                    redraw |= state.palette_hover();
                } else if state.menu.is_some() {
                    redraw |= state.menu_hover();
                } else if state.mouse_down {
                    redraw |= state.update_drag();
                } else if state.cfg.input.focus_follows_mouse && state.focus_pane_under_cursor() {
                    redraw = true;
                }
                // Close-button hover highlight tracks the cursor everywhere.
                redraw |= state.update_button_hover();
                // Hover reaches the view under the cursor (e.g. the file browser's
                // highlight follows the mouse).
                redraw |= state.hover_view();
                if redraw {
                    state.window.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                // A close button arms on press and fires on release (so it can be
                // cancelled); a press on the empty title-bar strip drags the window;
                // other clicks act on press as before.
                if state.arm_button() {
                    // armed; fires on release
                } else if state.cursor.1 < state.title_bar_h() {
                    let _ = state.window.drag_window();
                } else {
                    state.on_click();
                    state.begin_press();
                }
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                state.fire_button();
                state.end_press();
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                state.on_right_click();
                state.window.request_redraw();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Scroll is applied to the VT immediately, but the (expensive)
                // redraw is paced by `about_to_wait` so a trackpad flick coalesces
                // into ~one render per frame instead of one per event.
                state.on_scroll(delta);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let toggled = state.handle_key_taps(&event);
                if event.state == ElementState::Pressed {
                    state.on_key(&event);
                    state.mark_dirty();
                    state.window.request_redraw();
                } else if toggled {
                    state.window.request_redraw();
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
    quad_buffer: wgpu::Buffer,
    quad_capacity: u64,

    /// Separate text renderer for the palette so its text draws above the panel
    /// (terminal text and palette text can't share one pass with a quad between).
    palette_renderer: TextRenderer,
    palette_buffers: Vec<Buffer>,
    palette: Option<Palette>,
    /// Last-rendered palette panel rect (physical px), for click-outside dismissal.
    palette_panel: Rect,
    /// Last-rendered palette result rows: (row rect, result index, command id),
    /// for click/hover hit-testing. Rebuilt each frame the palette is open.
    palette_rows: Vec<(Rect, usize, String)>,
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
    /// The floating directory picker's panel rect this frame (for click-outside).
    picker_panel: Option<Rect>,
    /// The last left press (time, position, click count), for counting
    /// double-clicks.
    last_press: Option<(Instant, (f32, f32), u32)>,
    /// The view the current left press landed on; it gets the press's drag and
    /// release wherever the cursor goes.
    mouse_capture: Option<SurfaceId>,
    /// Whether the workspace sidebar is collapsed (session-only, like soft-wrap).
    sidebar_hidden: bool,
    /// Left mouse button is held (a press in progress).
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
}

/// Command-palette / run-anything overlay state.
struct Palette {
    query: String,
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
#[derive(Clone, Copy)]
enum MenuAction {
    MarkRead,
    MarkUnread,
    Dismiss,
    Rename,
    SetDir,
    Close,
}

/// A right-click context menu over a sidebar vtab.
struct Menu {
    /// The vtab the actions apply to.
    target: ghostrealm_core::VtabId,
    /// Where the menu was opened (physical px); the panel is clamped on-screen.
    anchor: (f32, f32),
    items: Vec<(String, MenuAction)>,
    /// Item under the cursor, for highlight.
    hover: Option<usize>,
    /// Per-item rects (physical px), filled at render for hit-testing.
    rows: Vec<Rect>,
    /// The panel rect (physical px), for click-outside dismissal.
    panel: Rect,
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
    /// Open a plugin's content in the focused pane (the "nothing open" picker).
    OpenPlugin(&'static str),
    /// A button a view registered while painting: (owning surface, view-local id).
    View(SurfaceId, u32),
    /// Confirm / cancel the floating directory picker.
    PickerConfirm,
    PickerCancel,
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

        // Build the registry first so a freshly created config can list every
        // command in its `[keybindings]` block.
        let registry = build_registry();
        let cfg = ghostrealm_core::config::load_or_create_with(&keybindable_commands(&registry));
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
        let mut app = AppState::new();
        let has_command = command_line.is_some();
        if let Some(line) = command_line {
            app = app.with_shell_line(line);
        }
        app = app.with_waker(waker);
        app.set_inbox_config(cfg.inbox);
        app.set_default_dir(cfg.default_dir());
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
            palette_buffers: Vec::new(),
            palette: None,
            palette_panel: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
            palette_rows: Vec::new(),
            sidebar_buffers: Vec::new(),
            strip_buffers: Vec::new(),
            menu: None,
            menu_buffers: Vec::new(),
            sidebar_scroll: 0.0,
            sidebar_max_scroll: 0.0,
            sidebar_rows: Vec::new(),
            buttons: Vec::new(),
            button_hover: None,
            armed_button: None,
            close_buffer,
            toggle_buffer,
            toggle_place: None,
            title_buffer,
            title_place: None,
            picker_panel: None,
            last_press: None,
            mouse_capture: None,
            sidebar_hidden: false,
            mouse_down: false,
            dragging: false,
            last_focused_surface: None,
            press_px: (0.0, 0.0),
            clipboard: arboard::Clipboard::new().ok(),
            cursor: (0.0, 0.0),
            quad_pipeline,
            quad_buffer,
            quad_capacity,
            app,
            registry,
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
        self.cfg = cfg;
        self.dirty = true;
    }

    /// Reload the config file from disk and apply it (config is the source of truth).
    fn reload_config(&mut self) {
        let cfg =
            ghostrealm_core::config::load_or_create_with(&keybindable_commands(&self.registry));
        self.apply_config(cfg);
        self.window.request_redraw();
    }

    /// Open the config file in an editor pane (creating it with defaults if absent).
    /// Saving it (Cmd+S) hot-reloads the config.
    fn open_config_editor(&mut self) {
        if let Some(path) = ghostrealm_core::config::config_path() {
            let commands = keybindable_commands(&self.registry);
            // Ensure it exists, seeded with the full keybindings block, then bring
            // an out-of-date file up to date (append missing sections + the
            // keybindings block) without disturbing the user's set values.
            let _ = ghostrealm_core::config::load_or_create_with(&commands);
            ghostrealm_core::config::backfill_config(&commands);
            // Settings live in their own "settings" workspace, not the focused pane.
            self.app.open_settings(path);
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
    /// (A single-tab editor's filename goes in its own ribbon, not a tab, so it
    /// doesn't carry a close button.)
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

    /// The focused pane's width in physical pixels (for the split-vs-tab decision).
    fn focused_pane_width(&self) -> f32 {
        let ws = self.workspace_rect();
        self.app
            .tree
            .active_vtab()
            .and_then(|vt| self.app.tree.vtab(vt))
            .and_then(|v| {
                let fp = v.focused_pane;
                v.layout(ws, DIVIDER).into_iter().find(|(id, _)| *id == fp).map(|(_, r)| r.w)
            })
            .unwrap_or(ws.w)
    }

    /// Open a file a view asked for: a horizontal split beside it when
    /// `[file_browser] open_in = "split"` and the pane is wide enough, else a tab.
    fn open_requested_file(&mut self, path: std::path::PathBuf) {
        use ghostrealm_core::config::OpenIn;
        let split = self.cfg.file_browser.open_in == OpenIn::Split
            && self.focused_pane_width() >= FILE_BROWSER_SPLIT_MIN_W * self.scale;
        let opened = if split {
            self.app.open_path_split(&path, Axis::LeftRight)
        } else {
            self.app.open_path_in_focused(&path)
        };
        if let Err(e) = opened {
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
        // directory picker is open only its own buttons are live.
        if self.palette.is_some() || self.menu.is_some() {
            return None;
        }
        let picker = self.app.dir_picker_open();
        let mut hit = None;
        for &(r, action) in &self.buttons {
            let modal = matches!(
                action,
                ButtonAction::PickerConfirm | ButtonAction::PickerCancel | ButtonAction::View(PICKER_SID, _)
            );
            if picker && !modal {
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
            ButtonAction::NewVtab => {
                self.app.new_empty_vtab();
            }
            ButtonAction::CloseVtab(id) => self.app.close_vtab(id),
            ButtonAction::CloseSurface(vt, pid, sid) => self.app.close_surface(vt, pid, sid),
            ButtonAction::ToggleSidebar => self.sidebar_hidden = !self.sidebar_hidden,
            ButtonAction::OpenPlugin(id) => {
                if let Err(e) = self.app.open_plugin_in_focused(id) {
                    eprintln!("ghostrealm: open {id}: {e:#}");
                }
            }
            ButtonAction::View(sid, id) => {
                self.view_event(sid, |v, cx| v.button(cx, id));
            }
            ButtonAction::PickerConfirm => self.app.confirm_dir_picker(),
            ButtonAction::PickerCancel => self.app.close_dir_picker(),
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
        if self.mouse_down || self.palette.is_some() || self.menu.is_some() {
            return false;
        }
        let pos = self.cursor;
        let target = if self.app.dir_picker_open() {
            Some(PICKER_SID)
        } else {
            self.surface_under_cursor()
                .filter(|s| self.app.view(*s).is_some())
        };
        match target {
            Some(sid) => self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Move { pos })),
            None => false,
        }
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
        let view: &mut dyn View = if sid == PICKER_SID {
            match self.app.dir_picker_mut() {
                Some(v) => v,
                None => return false,
            }
        } else {
            match self.app.view_mut(sid) {
                Some(v) => v,
                None => return false,
            }
        };
        let mut cx = EventCx::new(&self.cfg, ui, self.mods, self.clipboard.as_mut());
        f(view, &mut cx);
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
                Request::OpenFile(path) => self.open_requested_file(path),
                Request::Saved(path) => {
                    if Some(path) == ghostrealm_core::config::config_path() {
                        self.reload_config();
                    }
                }
                Request::Busy => self.app.mark_busy(sid),
            }
        }
    }

    /// Count this left press as the next of a multi-click when it follows the
    /// last one quickly (`[input] double_click_ms`) at about the same spot.
    fn count_click(&mut self) -> u32 {
        let now = Instant::now();
        let (x, y) = self.cursor;
        let slop = 4.0 * self.scale;
        let clicks = match self.last_press {
            Some((t, (px, py), n))
                if now.duration_since(t) < self.double_click_window()
                    && (x - px).abs() <= slop
                    && (y - py).abs() <= slop =>
            {
                n + 1
            }
            _ => 1,
        };
        self.last_press = Some((now, (x, y), clicks));
        clicks
    }

    fn on_click(&mut self) {
        self.mouse_capture = None;
        // The directory picker is modal: a click in its panel goes to its file
        // browser, a click outside cancels. (Its buttons arm on press.)
        if self.app.dir_picker_open() {
            let pos = self.cursor;
            match self.picker_panel {
                Some(panel) if rect_contains(panel, pos.0, pos.1) => {
                    let clicks = self.count_click();
                    self.view_event(PICKER_SID, |v, cx| {
                        v.mouse(cx, &MouseEvent::Down { pos, clicks })
                    });
                    self.mouse_capture = Some(PICKER_SID);
                }
                Some(_) => self.app.close_dir_picker(),
                None => {}
            }
            self.dirty = true;
            return;
        }
        if self.palette.is_some() {
            self.palette_click();
            return;
        }
        if self.menu.is_some() {
            self.menu_click();
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
        let workspace = self.workspace_rect();
        let Some(vt) = self.app.tree.active_vtab() else {
            return;
        };
        // (pane id, rect, surface count) of the pane under the cursor.
        let hit = self.app.tree.vtab(vt).and_then(|vtab| {
            vtab.layout(workspace, DIVIDER)
                .into_iter()
                .find_map(|(pid, r)| {
                    if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
                        let n = vtab
                            .panes()
                            .into_iter()
                            .find(|p| p.id == pid)
                            .map(|p| p.surfaces.len());
                        n.map(|n| (pid, r, n))
                    } else {
                        None
                    }
                })
        });
        let Some((pid, rect, n)) = hit else { return };

        let active_sid = self
            .app
            .tree
            .vtab(vt)
            .and_then(|v| v.panes().into_iter().find(|p| p.id == pid))
            .and_then(|p| p.active_surface().map(|s| s.id));

        // A click in a visible tab strip switches the pane's active surface.
        let strip_h = if self.strip_shown(n) {
            self.strip_height()
        } else {
            0.0
        };
        if strip_h > 0.0 && y < rect.y + strip_h {
            let tab_w = rect.w / n.max(1) as f32;
            let idx = (((x - rect.x) / tab_w).floor() as usize).min(n - 1);
            if let Some(pane) = self.app.tree.vtab_mut(vt).and_then(|v| v.pane_mut(pid)) {
                pane.active = idx;
            }
        } else if let Some(sid) = active_sid.filter(|s| self.app.view(*s).is_some()) {
            // Focus first (so anything the click opens targets this pane), then
            // hand the press to the view; it owns the rest of the press.
            if let Some(vtab) = self.app.tree.vtab_mut(vt) {
                vtab.focused_pane = pid;
            }
            let clicks = self.count_click();
            let pos = (x, y);
            self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Down { pos, clicks }));
            self.mouse_capture = Some(sid);
            self.dirty = true;
            return;
        }
        if let Some(vtab) = self.app.tree.vtab_mut(vt) {
            vtab.focused_pane = pid;
        }
        self.dirty = true;
    }

    /// Right-click opens a context menu over the sidebar vtab under the cursor.
    fn on_right_click(&mut self) {
        self.palette = None;
        let (x, y) = self.cursor;
        self.menu = None;
        if !self.in_sidebar(x) {
            self.dirty = true;
            return;
        }
        let Some(target) = self.sidebar_vtab_at(x, y) else {
            self.dirty = true;
            return;
        };
        let status = self.app.tree.vtab(target).map(|v| v.status);
        let mut items: Vec<(String, MenuAction)> = Vec::new();
        match status {
            Some(TabStatus::Unread { .. }) => {
                items.push(("Mark read".into(), MenuAction::MarkRead))
            }
            Some(TabStatus::NeedsInput) => items.push(("Dismiss".into(), MenuAction::Dismiss)),
            _ => items.push(("Mark unread".into(), MenuAction::MarkUnread)),
        }
        items.push(("Rename…".into(), MenuAction::Rename));
        items.push(("Set directory…".into(), MenuAction::SetDir));
        items.push(("Close workspace".into(), MenuAction::Close));
        self.menu = Some(Menu {
            target,
            anchor: (x, y),
            items,
            hover: None,
            rows: Vec::new(),
            panel: Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
            },
        });
        self.dirty = true;
    }

    /// A left click while the context menu is open: run a clicked item, else close.
    fn menu_click(&mut self) {
        let (x, y) = self.cursor;
        let hit = self.menu.as_ref().and_then(|m| {
            m.rows
                .iter()
                .position(|r| rect_contains(*r, x, y))
                .map(|i| (m.items[i].1, m.target))
        });
        self.menu = None;
        if let Some((action, target)) = hit {
            self.apply_menu_action(action, target);
        }
        self.dirty = true;
    }

    /// Highlight the menu item under the cursor; returns whether it changed.
    fn menu_hover(&mut self) -> bool {
        let (x, y) = self.cursor;
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        let idx = menu.rows.iter().position(|r| rect_contains(*r, x, y));
        if menu.hover != idx {
            menu.hover = idx;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    fn apply_menu_action(&mut self, action: MenuAction, target: ghostrealm_core::VtabId) {
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
                self.app.open_dir_picker();
                self.dirty = true;
            }
        }
    }

    /// A click while the palette is open: run a clicked result, or dismiss when
    /// the click lands outside the panel.
    fn palette_click(&mut self) {
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

    /// The earliest timer any view has pending.
    fn next_view_deadline(&self) -> Option<Instant> {
        self.app
            .views()
            .filter_map(|(_, v)| v.deadline(&self.cfg))
            .min()
    }

    /// Run the timers of views whose deadline has passed. Returns whether any
    /// fired (the caller redraws).
    fn fire_view_timers(&mut self) -> bool {
        let now = Instant::now();
        let due: Vec<SurfaceId> = self
            .app
            .views()
            .filter(|(_, v)| v.deadline(&self.cfg).is_some_and(|d| now >= d))
            .map(|(sid, _)| sid)
            .collect();
        for sid in &due {
            self.view_event(*sid, |v, cx| v.tick(cx, now));
        }
        !due.is_empty()
    }

    /// Start tracking a left press (for the drag threshold and release). No-op
    /// while an overlay owns input.
    fn begin_press(&mut self) {
        if self.palette.is_some() || self.menu.is_some() {
            return;
        }
        self.dragging = false;
        self.mouse_down = true;
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
        match self.mouse_capture {
            Some(sid) => self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Drag { pos })),
            None => false,
        }
    }

    /// Finish a press: the view that took it gets the release.
    fn end_press(&mut self) {
        self.mouse_down = false;
        let dragged = std::mem::take(&mut self.dragging);
        if let Some(sid) = self.mouse_capture.take() {
            let pos = self.cursor;
            self.view_event(sid, |v, cx| v.mouse(cx, &MouseEvent::Up { pos, dragged }));
        }
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
        // The directory picker is modal: the wheel scrolls its list.
        if self.app.dir_picker_open() {
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

    /// Draw the empty-workspace ("nothing open") screen in `rect`: a heading, a
    /// numbered button per pickable plugin, and a close hint. Buttons open that
    /// plugin in the focused (empty) pane.
    fn build_empty_pane(&mut self, frame: &mut Frame, rect: Rect) {
        let options = self.pickable_plugins();
        let hint = match self.cfg.binding_for("pane.close") {
            Some(c) => format!("{}  closes this workspace", pretty_chord(&c)),
            None => "Close this workspace from the palette".to_string(),
        };
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
        let total = ch + heading_gap + n * btn_h + (n - 1.0) * gap + hint_gap + ch;
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
            self.buttons.push((hit, ButtonAction::OpenPlugin(id)));
            y += btn_h + gap;
        }

        y += hint_gap - gap;
        let hint = cx.shape(&hint, Font::SANS, rect.w);
        cx.place(hint, centre - hint.width * 0.5, y, rect, EMPTY_HINT);
    }

    /// Draw the floating directory picker overlay: a dim backdrop and a centred
    /// panel hosting a file-browser view (on the overlay layer) above a
    /// confirm/cancel footer.
    fn build_dir_picker(&mut self, frame: &mut Frame) {
        if !self.app.dir_picker_open() {
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
        let Some(view) = self.app.dir_picker_mut() else {
            return;
        };
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
        cx.focused = true;
        cx.fill(panel, cx.chrome.background);
        cx.border(panel, cx.chrome.accent);

        let title_rect = Rect { x: px, y: py, w: pw, h: title_h };
        let title = cx.shape("Set workspace directory", Family::SansSerif, pw);
        cx.place(title, px + (pw - title.width) * 0.5, py + (title_h - ch) * 0.5, title_rect, theme::TITLE);

        let body = Rect {
            x: px,
            y: py + title_h,
            w: pw,
            h: (ph - title_h - footer_h).max(1.0),
        };
        view.paint(&mut cx, body);

        // Footer: [Cancel] [Use this folder], right-aligned.
        let bh = ch + 8.0 * scale;
        let by = py + ph - footer_h + (footer_h - bh) * 0.5;
        let mut right = px + pw - pad;
        for (label, action, primary) in [
            ("Use this folder", ButtonAction::PickerConfirm, true),
            ("Cancel", ButtonAction::PickerCancel, false),
        ] {
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
        let owned = frame.view_buttons.drain(..);
        self.buttons.extend(owned.map(|(r, s, id)| (r, ButtonAction::View(s, id))));
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

    /// Build the context-menu overlay (panel + hover quads); shape item labels into
    /// `menu_buffers`; record row/panel rects for hit-testing. Returns placements.
    fn build_menu(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Vec<Placement> {
        let (items, anchor, hover) = match &self.menu {
            Some(m) => (m.items.clone(), m.anchor, m.hover),
            None => return Vec::new(),
        };
        let metrics = self.metrics();
        let pad = 8.0 * self.scale;
        let row_h = self.cell_h + 6.0 * self.scale;
        let maxlen = items
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(6) as f32;
        let panel_w = (maxlen * self.cell_w + pad * 3.0).min(sw * 0.6);
        let panel_h = items.len() as f32 * row_h + pad;
        let px = anchor.0.min((sw - panel_w).max(0.0)).max(0.0);
        let py = anchor.1.min((sh - panel_h).max(0.0)).max(0.0);

        quads.push(rect_quad(
            Rect {
                x: px,
                y: py,
                w: panel_w,
                h: panel_h,
            },
            sw,
            sh,
            [34, 34, 42],
            0.98,
        ));

        while self.menu_buffers.len() < items.len() {
            let b = Buffer::new(&mut self.text.font_system, metrics);
            self.menu_buffers.push(b);
        }

        let mut rows = Vec::with_capacity(items.len());
        let mut placements = Vec::with_capacity(items.len());
        for (i, (label, _)) in items.iter().enumerate() {
            let ry = py + pad * 0.5 + i as f32 * row_h;
            let rrect = Rect {
                x: px,
                y: ry,
                w: panel_w,
                h: row_h,
            };
            if hover == Some(i) {
                quads.push(rect_quad(rrect, sw, sh, self.chrome.accent, 0.5));
            }
            let buf = &mut self.menu_buffers[i];
            buf.set_metrics(metrics);
            buf.set_size(Some((panel_w - pad * 2.0).max(1.0)), Some(self.cell_h));
            buf.set_rich_text(
                std::iter::once((label.as_str(), attrs_for([230, 230, 240]))),
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.text.font_system, false);
            placements.push(Placement {
                idx: i,
                left: px + pad,
                top: ry + (row_h - self.cell_h) * 0.5,
                bounds: TextBounds {
                    left: px as i32,
                    top: ry as i32,
                    right: (px + panel_w) as i32,
                    bottom: (ry + row_h) as i32,
                },
                color: [230, 230, 240],
            });
            rows.push(rrect);
        }

        if let Some(m) = self.menu.as_mut() {
            m.rows = rows;
            m.panel = Rect {
                x: px,
                y: py,
                w: panel_w,
                h: panel_h,
            };
        }
        placements
    }

    /// Run a command by id: execute immediately if it needs no required args, else
    /// open the palette to collect them (the argument prompt).
    fn begin_command(&mut self, id: &str) {
        let specs: Vec<ArgSpec> = self
            .registry
            .meta(id)
            .map(|m| m.args.iter().filter(|a| a.required).cloned().collect())
            .unwrap_or_default();
        if specs.is_empty() {
            let _ = self.registry.execute(id, &Args::new(), &mut self.app);
        } else {
            let title = self
                .registry
                .meta(id)
                .map(|m| m.title.to_string())
                .unwrap_or_default();
            self.menu = None;
            self.palette = Some(Palette {
                query: String::new(),
                selected: 0,
                scroll: 0,
                mode: PaletteMode::Commands,
                pending: Some(PendingArgs {
                    id: id.to_string(),
                    title,
                    specs,
                    values: Vec::new(),
                }),
            });
        }
        self.dirty = true;
    }

    /// Open the command palette (fresh query, first result selected).
    fn open_palette(&mut self) {
        self.menu = None;
        self.palette = Some(Palette {
            query: String::new(),
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
                    query: String::new(),
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
        // The directory picker, when open, owns the keyboard: Escape cancels,
        // Cmd+Enter confirms, everything else drives its file browser.
        if self.app.dir_picker_open() {
            match &event.logical_key {
                WKey::Named(NamedKey::Escape) => {
                    self.app.close_dir_picker();
                    self.dirty = true;
                }
                WKey::Named(NamedKey::Enter) if self.mods.super_ => {
                    self.app.confirm_dir_picker();
                    self.dirty = true;
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
        // A context menu is dismissed by Escape; other keys pass through.
        if self.menu.is_some() && matches!(event.logical_key, WKey::Named(NamedKey::Escape)) {
            self.menu = None;
            self.dirty = true;
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

        // The focused view takes the key.
        if let Some(sid) = self.app.focused_surface().filter(|s| self.app.view(*s).is_some()) {
            if let Some(press) = winit_key_press(event, self.mods) {
                self.view_event(sid, |v, cx| {
                    v.key(cx, &press);
                });
            }
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
            Some(p) => (p.query.clone(), p.selected, p.mode),
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
            WKey::Named(NamedKey::Backspace) => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                    p.selected = 0;
                    p.scroll = 0;
                }
            }
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
            WKey::Named(NamedKey::Space) => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.push(' ');
                    p.selected = 0;
                    p.scroll = 0;
                }
            }
            WKey::Character(s) => {
                if let Some(c) = s.chars().next() {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.push(c);
                        p.selected = 0;
                        p.scroll = 0;
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// Key handling while the palette is collecting a command's arguments.
    fn palette_arg_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            WKey::Named(NamedKey::Escape) => self.palette = None,
            WKey::Named(NamedKey::Enter) => {
                let mut run: Option<(String, Args)> = None;
                if let Some(p) = self.palette.as_mut() {
                    let query = p.query.clone();
                    if let Some(pend) = p.pending.as_mut() {
                        let idx = pend.values.len();
                        if let Some(spec) = pend.specs.get(idx) {
                            if let Some(val) = parse_arg_value(&spec.kind, &query) {
                                pend.values.push(val);
                                if pend.values.len() == pend.specs.len() {
                                    let mut args = Args::new();
                                    for (s, v) in pend.specs.iter().zip(&pend.values) {
                                        args.insert(s.name, v.clone());
                                    }
                                    run = Some((pend.id.clone(), args));
                                } else {
                                    p.query.clear();
                                }
                            }
                            // An unparseable value keeps the query for a retry.
                        }
                    }
                }
                if let Some((id, args)) = run {
                    self.palette = None;
                    let _ = self.registry.execute(&id, &args, &mut self.app);
                }
            }
            WKey::Named(NamedKey::Backspace) => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                }
            }
            WKey::Named(NamedKey::Space) => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.push(' ');
                }
            }
            WKey::Character(s) => {
                if let Some(c) = s.chars().next() {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.push(c);
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// Build the palette overlay: push its dim/panel/selection quads and shape
    /// its text lines, returning their placements (indices into palette_buffers).
    fn build_palette(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Vec<Placement> {
        // Snapshot palette state, including any in-progress argument prompt.
        let (query, mode, pending) = match &self.palette {
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
                (p.query.clone(), p.mode, pend)
            }
            None => return Vec::new(),
        };

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
            lines.push(vec![(
                format!("\u{203a} {title} — {name}: {query}"),
                [235, 235, 245],
            )]);
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
            return placements;
        }

        let prompt = match mode {
            PaletteMode::Commands => format!("\u{203a} {}", query),
            PaletteMode::Run => format!("\u{2b95} {}", query),
        };
        lines.push(vec![(prompt, [235, 235, 245])]);
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
        placements
    }

    fn render(&mut self) -> Result<()> {
        // Drain a bounded slice of child output; anything past the budget is left
        // for the next frame so a flood can't stretch one frame indefinitely.
        let (changed, more) = self.app.pump_all_budgeted(PUMP_BUDGET);
        if changed {
            self.dirty = true;
        }
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
            }
            self.last_focused_surface = focused;
        }
        self.fire_view_timers();
        if !self.dirty {
            return Ok(());
        }

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
                    // Italic when the tab's view has unsaved changes.
                    let unsaved = self.app.view(*sid).is_some_and(|v| v.modified());
                    let mut title_attrs = Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgb(220, 220, 230));
                    if unsaved {
                        title_attrs = title_attrs.style(Style::Italic);
                    }
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
                        color: [220, 220, 230],
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
                        let hovered = rect_contains(hit, self.cursor.0, self.cursor.1);
                        if hovered {
                            frame.bg.push(rect_quad(
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
                            top: text_top,
                            bounds: TextBounds {
                                left: close_x as i32,
                                top: pr.rect.y as i32,
                                right: (close_x + close_w) as i32,
                                bottom: (pr.rect.y + *strip_h) as i32,
                            },
                            color: if hovered { theme::GLYPH_HOVER } else { theme::GLYPH },
                        });
                        self.buttons.push((hit, ButtonAction::CloseSurface(vt, pr.id, *sid)));
                    }
                }
            }

            let Some((active_sid, _, _)) = pr.surfaces.iter().find(|(_, _, a)| *a) else {
                // No surface in this pane: draw the "nothing open" screen.
                self.build_empty_pane(&mut frame, *term);
                continue;
            };
            let sid = *active_sid;

            // An empty tab shows the picker; a file browser tab shows the file browser.
            if self.app.surface_is_empty(sid) {
                self.build_empty_pane(&mut frame, *term);
                continue;
            }
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
                cx.tab_strip = *strip_h > 0.0;
                view.paint(&mut cx, *term);
                if pr.focused && multi_pane {
                    push_border(&mut frame.top, pr.rect, sw, sh, self.chrome.accent);
                }
                let owned = frame.view_buttons.drain(..);
                self.buttons.extend(owned.map(|(r, s, id)| (r, ButtonAction::View(s, id))));
            }
        }
        // Rows the shaping budget deferred need a follow-up frame.
        let deferred = self.text.finish_paint();

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
        let menu_placements = if self.menu.is_some() {
            self.build_menu(sw, sh, &mut frame.top)
        } else {
            Vec::new()
        };
        // Floating directory picker (modal overlay). Its text joins the overlay pass.
        self.build_dir_picker(&mut frame);

        let n_bg = frame.bg.len() as u32;
        let n_top = frame.top.len();
        let mut quads = std::mem::take(&mut frame.bg);
        quads.extend_from_slice(&frame.top);
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

        // Palette, menu and the directory picker are mutually exclusive; each draws
        // above everything via the overlay text renderer (each from its own pool).
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
        } else if !menu_placements.is_empty() {
            menu_placements
                .iter()
                .map(|p| TextArea {
                    buffer: &self.menu_buffers[p.idx],
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
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .context("text render")?;
            let total = (n_bg as usize + n_top) as u32;
            if total > n_bg {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, n_bg..total);
            }
            if !palette_placements.is_empty()
                || !menu_placements.is_empty()
                || !frame.overlay_text.is_empty()
            {
                self.palette_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .context("overlay render")?;
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
        TabStatus::Unread { success: true } => [80, 180, 90],
        TabStatus::Unread { success: false } => [200, 80, 80],
        TabStatus::NeedsInput => [210, 120, 40],
    }
}

/// UI actions that are keybindable but aren't registry commands (they act on
/// window state, not `AppState`), so they must be listed for the config block
/// alongside the registry's commands. Dispatched specially in [`State::on_key`].
const SPECIAL_COMMANDS: &[(&str, &str)] = &[("palette.toggle", "Command Palette")];

/// Every keybindable command as `(id, title)` — the non-hidden registry commands
/// plus the special UI actions — used to seed the config's authoritative
/// `[keybindings]` block so it always lists every available action.
fn keybindable_commands(registry: &Registry<AppState>) -> Vec<(&'static str, &'static str)> {
    let mut cmds: Vec<(&'static str, &'static str)> = registry
        .metas()
        .filter(|m| !m.hidden)
        .map(|m| (m.id, m.title))
        .collect();
    cmds.extend_from_slice(SPECIAL_COMMANDS);
    cmds
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
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4],
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

const QUAD_WGSL: &str = r#"
struct Inst {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
    let c = corners[vi];
    let p = inst.pos + c * inst.size;
    var o: VOut;
    o.clip = vec4<f32>(p, 0.0, 1.0);
    o.color = inst.color;
    return o;
}
@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

#[cfg(test)]
mod tests {
    use super::{
        build_quad_pipeline, measure_cell, rect_contains, sidebar_rect_for, workspace_rect_for,
        QuadInstance,
    };
    use ghostrealm_core::{Rect, Side};
    use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};
    use std::time::{Duration, Instant};

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
    fn every_default_binding_is_a_listed_command() {
        // Deterministic guard: the generated keybindings block lists these
        // commands, so a default binding must never point at one that's absent
        // (registry command or special UI action).
        let reg = crate::app_state::build_registry();
        let listed: std::collections::HashSet<&str> = super::keybindable_commands(&reg)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        for id in ghostrealm_core::config::default_binding_ids() {
            assert!(
                listed.contains(id),
                "default binding for `{id}` is missing from keybindable_commands"
            );
        }
    }

    #[test]
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

    async fn offscreen_red() {
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
        let dim = 64u32;
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

        let inst = QuadInstance {
            pos: [-1.0, 1.0],
            size: [2.0, -2.0],
            color: [1.0, 0.0, 0.0, 1.0],
        };
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-quad"),
            size: std::mem::size_of::<QuadInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(&[inst]));

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
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
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
            pass.draw(0..4, 0..1);
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

        let off = ((dim / 2) * bytes_per_row + (dim / 2) * 4) as usize;
        let px = [data[off], data[off + 1], data[off + 2], data[off + 3]];
        assert!(
            px[0] > 200 && px[1] < 50 && px[2] < 50 && px[3] > 200,
            "expected an opaque red centre pixel from the quad pipeline, got {px:?}."
        );
    }
}
