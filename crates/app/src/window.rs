//! wgpu + winit + glyphon window hosting the multiplexer.
//!
//! Renders the active vtab's split tree: each pane's active surface is drawn in
//! its computed rect (background quads + cursor via an instanced-quad pipeline,
//! foreground text via glyphon), all in one wgpu scene. Cmd-chords run app
//! commands through the registry; other keys go to the focused surface.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ghostrealm_core::{
    ArgKind, ArgSpec, Args, Chrome, Config, LineNumbers, PaneId, Rect, Registry, Side, SurfaceId,
    TabStatus, Value, VtabId,
};
use ghostrealm_terminal::{Cell, Grid, Key, KeyPress, Mods, Scroll, TerminalBackend};
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

use crate::app_state::{build_registry, AppState, SETTINGS_VTAB_NAME};
use crate::editor::Motion;
use crate::row_cache::RowCache;
use crate::tap::TapDetector;

/// Divider gap between split panes, in physical pixels.
const DIVIDER: f32 = 6.0;
/// Focused-pane border thickness, in physical pixels.
const BORDER: f32 = 2.0;
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
/// Shape at least this many missed rows per frame regardless of the time budget,
/// so a backlog always makes forward progress even on an already-slow frame.
const MIN_SHAPES_PER_FRAME: usize = 8;
/// Max child-output bytes drained per surface per frame. Bounds VT-parse work so
/// one burst can't stall a frame; the remainder is pumped on following frames.
const PUMP_BUDGET: usize = 512 * 1024;
/// Soft cap on cached shaped rows. Comfortably holds several full screens so
/// scrollback and multiple surfaces stay warm; trimmed after each frame.
const ROW_CACHE_CAP: usize = 4096;
/// Scrollback lines per mouse-wheel notch.
const SCROLL_LINES_PER_NOTCH: f32 = 3.0;
/// Pending-scroll metering (see `apply_pending_scroll`). Each frame advances the
/// viewport by `pending / EASE_DIVISOR`, clamped to `[MIN, MAX]` lines: small
/// scrolls stay slow and coherent (no torn, half-shaped viewport), while a big
/// flick eases out fast enough to cross a large scrollback in a beat. `MAX` is
/// the one knob trading catch-up speed against how much a fast fling can outrun
/// shaping.
const SCROLL_STEP_MIN: i32 = 6;
const SCROLL_STEP_MAX: i32 = 40;
const SCROLL_EASE_DIVISOR: i32 = 3;
/// Cap on queued scroll lines, so flicking hard against the scrollback boundary
/// can't pile up a backlog that then has to unwind before a reverse flick takes
/// effect (and so metering always drains in a bounded number of frames).
const MAX_PENDING_SCROLL: i32 = 600;
/// Editor surface background / foreground (slightly distinct from a terminal).
const EDITOR_BG: [u8; 3] = [26, 26, 32];
const EDITOR_FG: [u8; 3] = [220, 220, 230];
/// Editor line-number gutter: dim, brighter on the cursor's line.
const EDITOR_GUTTER: [u8; 3] = [110, 110, 125];
const EDITOR_GUTTER_CUR: [u8; 3] = [190, 190, 205];
/// Active-line highlight fill behind the cursor row.
const EDITOR_CURSOR_LINE: [u8; 3] = [255, 255, 255];
const EDITOR_CURSOR_LINE_ALPHA: f32 = 0.05;
/// Close-button ('×') glyph colour — dim so it reads as a secondary affordance,
/// brighter under the cursor.
const CLOSE_GLYPH: [u8; 3] = [140, 140, 155];
const CLOSE_GLYPH_HOVER: [u8; 3] = [235, 235, 245];
/// Highlight box drawn behind a clickable button on hover.
const BUTTON_HOVER_BG: [u8; 3] = [255, 255, 255];
const BUTTON_HOVER_ALPHA: f32 = 0.14;
/// Sidebar "+ New workspace" label colour, brighter under the cursor.
const NEW_VTAB_LABEL: [u8; 3] = [150, 150, 165];
const NEW_VTAB_LABEL_HOVER: [u8; 3] = [210, 210, 220];
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

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadInstance {
    /// Top-left in NDC.
    pos: [f32; 2],
    /// Width/height in NDC (height negative to grow downward).
    size: [f32; 2],
    color: [f32; 4],
}

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
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
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
        // Wake for whichever comes first: the next paced PTY frame or a pending
        // inbox auto-read deadline.
        let mut wake: Option<Instant> = state.frame_pending.then_some(state.next_frame);
        if let Some(d) = state.app.next_inbox_deadline() {
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
                let _ = state.app.autosave_all_editors();
                state.window.set_visible(false);
                std::process::exit(0);
            }
            WindowEvent::Focused(false) => {
                // App lost focus: autosave editors (switching to another app), and
                // hot-reload the config if it was one of them.
                if state.app.autosave_all_editors() {
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
                    redraw |= state.update_selection();
                } else if state.cfg.input.focus_follows_mouse && state.focus_pane_under_cursor() {
                    redraw = true;
                }
                // Close-button hover highlight tracks the cursor everywhere.
                redraw |= state.update_button_hover();
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
                // cancelled); other clicks act on press as before.
                if !state.arm_button() {
                    state.on_click();
                    state.begin_selection();
                }
                state.window.request_redraw();
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => {
                state.fire_button();
                state.end_selection();
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

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    /// Shaped terminal rows keyed by content, so scrolling and vtab/pane switching
    /// reuse shaping instead of reshaping from scratch.
    row_cache: RowCache,
    /// Last snapshot per surface, reused for idle surfaces so a still pane costs
    /// no snapshot; refreshed when the surface reports it changed or was resized.
    grid_cache: HashMap<SurfaceId, Grid>,
    /// Grid size we last resized each surface to, so a layout pass resizes a
    /// surface's terminal only when its cell dimensions actually change.
    surface_geom: HashMap<SurfaceId, (u16, u16)>,
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
    /// Row content key drawn at each screen position `(left, top)` last frame.
    /// When this frame's shaping budget defers a row, we redraw its previous
    /// (now slightly stale) content instead of a blank gap, until the new row is
    /// shaped a frame or two later. Rebuilt every frame.
    prev_rows: HashMap<(i32, i32), u64>,

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
    /// Sub-line remainder (physical px) carried between wheel/trackpad events so
    /// slow scrolls accumulate instead of being rounded away, and no motion is
    /// lost. The terminal viewport itself is still line-quantised.
    scroll_accum: f32,
    /// Whole viewport lines still to apply, metered out at `SCROLL_STEP`/frame so
    /// the viewport never advances faster than shaping can keep up (sign matches
    /// `Scroll::Delta`: negative reveals older history).
    pending_scroll: i32,
    /// Surface the pending scroll applies to; retargeting drops any leftover.
    pending_scroll_target: Option<SurfaceId>,
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
    /// Active terminal text selection, if any.
    selection: Option<Selection>,
    /// Left mouse button is held (for drag-selection).
    mouse_down: bool,
    /// Whether the current press has moved enough to count as a drag.
    dragging: bool,
    /// Physical-pixel position where the current press began.
    press_px: (f32, f32),
    /// The cell the press landed on (selection anchor), if it was over a terminal.
    /// A selection is only materialised once a drag actually starts.
    press_cell: Option<(SurfaceId, u16, u16)>,
    /// The editor surface a press landed on, if any — drives editor drag-select
    /// (the anchor/cursor live on the `EditorBuffer`).
    editor_drag: Option<SurfaceId>,
    /// The surface focused as of the last frame, so a focus change can autosave
    /// the editor that just lost focus.
    last_focused_surface: Option<SurfaceId>,
    /// Per-editor soft-wrap override (absent = the config default). Toggled by the
    /// ribbon's wrap button.
    soft_wrap: HashMap<SurfaceId, bool>,
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

/// An active text selection within one surface's viewport (cell coordinates).
#[derive(Clone, Copy)]
struct Selection {
    surface: SurfaceId,
    /// Where the drag began.
    anchor: (u16, u16),
    /// Where it currently ends.
    head: (u16, u16),
}

impl Selection {
    /// (start, end) ordered row-major (start <= end).
    fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.anchor.1, self.anchor.0);
        let h = (self.head.1, self.head.0);
        if a <= h {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }
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
    /// Toggle soft-wrap for an editor surface.
    ToggleSoftWrap(SurfaceId),
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
        if let Some(line) = command_line {
            app = app.with_shell_line(line);
        }
        app = app.with_waker(waker);
        app.set_inbox_config(cfg.inbox);
        app.set_default_dir(cfg.default_dir());
        app.new_vtab().context("open initial tab")?;

        let warm_metrics = Metrics::new(
            cfg.terminal.font_size * scale,
            cfg.terminal.line_height * scale,
        );
        let warm_buffer = Buffer::new(&mut font_system, warm_metrics);
        let close_buffer = Buffer::new(&mut font_system, warm_metrics);

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
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            row_cache: RowCache::new(ROW_CACHE_CAP),
            grid_cache: HashMap::new(),
            surface_geom: HashMap::new(),
            metrics_gen: 0,
            warm_buffer,
            atlas_warmed_gen: None,
            pending_fonts,
            mono_family,
            font_locale: locale,
            prev_rows: HashMap::new(),
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
            scroll_accum: 0.0,
            pending_scroll: 0,
            pending_scroll_target: None,
            sidebar_rows: Vec::new(),
            buttons: Vec::new(),
            button_hover: None,
            armed_button: None,
            close_buffer,
            selection: None,
            mouse_down: false,
            dragging: false,
            editor_drag: None,
            last_focused_surface: None,
            soft_wrap: HashMap::new(),
            press_px: (0.0, 0.0),
            press_cell: None,
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
    /// (invalidating the shaping cache and forcing a reflow).
    fn apply_config(&mut self, cfg: Config) {
        let (cw, ch) = measure_cell(
            &mut self.font_system,
            self.scale,
            cfg.terminal.font_size,
            cfg.terminal.line_height,
        );
        if (cw - self.cell_w).abs() > 0.01 || (ch - self.cell_h).abs() > 0.01 {
            self.cell_w = cw;
            self.cell_h = ch;
            self.metrics_gen = self.metrics_gen.wrapping_add(1);
            self.surface_geom.clear(); // reflow terminals to the new cell size
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
            &mut self.font_system,
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
        self.font_system = FontSystem::new_with_locale_and_db(self.font_locale.clone(), db);
        // Cell metrics should be unchanged (same pinned monospace); re-measure in
        // case the fuller DB resolves the family to a different face.
        let (cw, ch) = measure_cell(
            &mut self.font_system,
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

    /// The row key to draw at screen position `pos` when this frame's own content
    /// there was deferred by the shaping budget: reuse the previous frame's key if
    /// its shaping is still cached (touching it to keep it warm), else fall back to
    /// `fallback` — whose buffer is absent, so the row draws blank. The fallback
    /// only bites on the first-ever reveal of a position, where there is no prior
    /// content to hold. Takes fields explicitly (not `&mut self`) so it can be
    /// called while a `grid` snapshot is borrowed elsewhere in the frame.
    #[allow(clippy::too_many_arguments)]
    fn stale_key(
        prev_rows: &HashMap<(i32, i32), u64>,
        row_cache: &mut RowCache,
        font_system: &mut FontSystem,
        pos: (i32, i32),
        fallback: u64,
        metrics: Metrics,
        width: f32,
        cell_h: f32,
    ) -> u64 {
        match prev_rows.get(&pos).copied() {
            Some(prev) if row_cache.buffer(prev).is_some() => {
                row_cache.ensure(prev, font_system, metrics, width, cell_h, &[]);
                prev
            }
            _ => fallback,
        }
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
                        let title = if s.title.is_empty() {
                            "sh".to_string()
                        } else {
                            s.title.clone()
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
        self.cfg.sidebar.width * self.scale
    }

    /// The sidebar's rect in physical pixels (honours `[sidebar] side`).
    fn sidebar_rect(&self) -> Rect {
        sidebar_rect_for(
            self.cfg.sidebar.side,
            self.sidebar_width(),
            self.config.width as f32,
            self.config.height as f32,
        )
    }

    /// The workspace (panes) rect in physical pixels — everything the sidebar
    /// doesn't occupy.
    fn workspace_rect(&self) -> Rect {
        workspace_rect_for(
            self.cfg.sidebar.side,
            self.sidebar_width(),
            self.config.width as f32,
            self.config.height as f32,
        )
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

    /// The chrome button under the cursor, if any.
    fn button_at_cursor(&self) -> Option<ButtonAction> {
        let (x, y) = self.cursor;
        self.buttons
            .iter()
            .find(|(r, _)| rect_contains(*r, x, y))
            .map(|&(_, t)| t)
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
                let _ = self.app.new_vtab();
            }
            ButtonAction::CloseVtab(id) => self.app.close_vtab(id),
            ButtonAction::CloseSurface(vt, pid, sid) => self.app.close_surface(vt, pid, sid),
            ButtonAction::ToggleSoftWrap(sid) => {
                let now = self.soft_wrap_on(sid);
                self.soft_wrap.insert(sid, !now);
            }
        }
        self.dirty = true;
        true
    }

    /// Whether soft-wrap is on for editor `sid`: its per-pane override, else the
    /// config default.
    fn soft_wrap_on(&self, sid: SurfaceId) -> bool {
        self.soft_wrap
            .get(&sid)
            .copied()
            .unwrap_or(self.cfg.editor.soft_wrap)
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

    fn on_click(&mut self) {
        if self.palette.is_some() {
            self.palette_click();
            return;
        }
        if self.menu.is_some() {
            self.menu_click();
            return;
        }
        let (x, y) = self.cursor;
        self.editor_drag = None;
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
        let active_is_editor = active_sid.is_some_and(|s| self.app.is_editor(s));

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
        } else if active_is_editor {
            // A click in the editor body moves the text cursor there and starts a
            // potential drag-selection (anchor at the click; a selection only
            // materialises once the cursor is dragged away — see update_selection).
            if let Some((sid, row, col)) = self.editor_pos_at(x, y) {
                if let Some(e) = self.app.editor_mut(sid) {
                    e.cursor = (row, col);
                    e.anchor = Some((row, col));
                }
                self.editor_drag = Some(sid);
            }
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
                self.begin_command("tab.rename");
            }
            MenuAction::SetDir => {
                self.app.focus_vtab(target);
                self.begin_command("workspace.set_root");
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

    /// The (surface, col, row) under a physical point, if it's over a terminal
    /// cell (not the sidebar, a tab strip, or outside any pane).
    fn cell_at(&self, x: f32, y: f32) -> Option<(SurfaceId, u16, u16)> {
        if self.in_sidebar(x) {
            return None;
        }
        let workspace = self.workspace_rect();
        let vt = self.app.tree.active_vtab()?;
        let vtab = self.app.tree.vtab(vt)?;
        for (pid, r) in vtab.layout(workspace, DIVIDER) {
            if !(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h) {
                continue;
            }
            let pane = vtab.panes().into_iter().find(|p| p.id == pid)?;
            let sid = pane.active_surface()?.id;
            let strip_h = if self.strip_shown(pane.surfaces.len()) {
                self.strip_height()
            } else {
                0.0
            };
            let term_y = r.y + strip_h;
            if y < term_y {
                return None; // in the tab strip
            }
            let grid = self.grid_cache.get(&sid)?;
            if grid.size.cols == 0 || grid.size.rows == 0 {
                return None;
            }
            let col = (((x - r.x) / self.cell_w).floor().max(0.0) as u16)
                .min(grid.size.cols - 1);
            let row = (((y - term_y) / self.cell_h).floor().max(0.0) as u16)
                .min(grid.size.rows - 1);
            return Some((sid, col, row));
        }
        None
    }

    /// Width (physical px) of the editor's line-number gutter for a buffer of
    /// `total_lines` lines. Zero when line numbers are off.
    fn editor_gutter_w(&self, total_lines: usize) -> f32 {
        if self.cfg.editor.line_numbers == LineNumbers::Off {
            return 0.0;
        }
        let digits = ((total_lines.max(1) as f64).log10().floor() as usize + 1).max(2);
        // one column of left pad + the number + one column of right pad.
        (digits as f32 + 2.0) * self.cell_w
    }

    /// The editor `(surface, row, col)` at point `(x, y)`, or `None` if the point
    /// isn't in an editor body (accounts for scroll, the tab strip, and gutter).
    fn editor_pos_at(&self, x: f32, y: f32) -> Option<(SurfaceId, usize, usize)> {
        if self.in_sidebar(x) {
            return None;
        }
        let workspace = self.workspace_rect();
        let vt = self.app.tree.active_vtab()?;
        let vtab = self.app.tree.vtab(vt)?;
        for (pid, r) in vtab.layout(workspace, DIVIDER) {
            if !(x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h) {
                continue;
            }
            let pane = vtab.panes().into_iter().find(|p| p.id == pid)?;
            let sid = pane.active_surface()?.id;
            let e = self.app.editor(sid)?;
            let strip_h = if self.strip_shown(pane.surfaces.len()) {
                self.strip_height()
            } else {
                0.0
            };
            // A single-tab editor has a filename ribbon in place of the strip.
            let ribbon_h = if strip_h == 0.0 {
                self.strip_height()
            } else {
                0.0
            };
            let body_y = r.y + strip_h + ribbon_h;
            if y < body_y {
                return None; // in the tab strip / ribbon
            }
            let gutter_w = self.editor_gutter_w(e.lines.len());
            let body_x = r.x + gutter_w;
            let body_w = (r.w - gutter_w).max(1.0);
            let row_off = ((y - body_y) / self.cell_h).floor().max(0.0) as usize;
            let col_off = ((x - body_x) / self.cell_w).round().max(0.0) as usize;
            // Walk visual rows from the scroll top to the clicked one, matching the
            // render's wrapping, so a click maps to the right (logical line, col).
            let wrap = self.soft_wrap_on(sid);
            let wrap_cols = if wrap {
                ((body_w / self.cell_w).floor() as usize).max(1)
            } else {
                usize::MAX
            };
            let mut vidx = 0usize;
            let mut found: Option<(usize, usize, usize)> = None; // (line, start, len)
            let mut ln = e.scroll;
            'walk: while ln < e.lines.len() {
                if wrap {
                    for (text, start) in wrap_line(&e.lines[ln], wrap_cols) {
                        if vidx == row_off {
                            found = Some((ln, start, text.chars().count()));
                            break 'walk;
                        }
                        vidx += 1;
                    }
                } else {
                    if vidx == row_off {
                        found = Some((ln, 0, e.line_len(ln)));
                        break 'walk;
                    }
                    vidx += 1;
                }
                ln += 1;
            }
            let (row, col) = match found {
                Some((line, start, len)) => (line, (start + col_off).min(start + len)),
                None => {
                    // Below the last line: land at the end of the last line.
                    let last = e.lines.len().saturating_sub(1);
                    (last, e.line_len(last))
                }
            };
            return Some((sid, row, col));
        }
        None
    }

    /// Save an editor surface if it has unsaved edits and a backing file (autosave
    /// on focus loss). Terminals and pathless scratch buffers are ignored. Saving
    /// the config file also hot-reloads it, like Cmd+S.
    fn autosave_editor(&mut self, sid: SurfaceId) {
        let saved = match self.app.editor_mut(sid) {
            Some(e) if e.modified && e.path.is_some() => {
                let _ = e.save();
                e.path.clone()
            }
            _ => None,
        };
        if saved.is_some() && saved == ghostrealm_core::config::config_path() {
            self.reload_config();
        }
    }

    /// Record a potential drag-selection anchor at the current cursor and clear any
    /// prior selection. A selection is materialised only once a drag starts, so a
    /// plain click never highlights. No-op while an overlay owns input.
    fn begin_selection(&mut self) {
        if self.palette.is_some() || self.menu.is_some() {
            return;
        }
        let had_selection = self.selection.is_some();
        self.selection = None;
        self.dragging = false;
        self.mouse_down = true;
        self.press_px = self.cursor;
        self.press_cell = self.cell_at(self.cursor.0, self.cursor.1);
        if had_selection {
            self.dirty = true; // clear the old highlight
        }
    }

    /// Extend the selection to the cursor during a drag. Returns whether it
    /// changed (so the caller can redraw).
    fn update_selection(&mut self) -> bool {
        if !self.mouse_down {
            return false;
        }
        let (x, y) = self.cursor;
        // Ignore sub-pixel jitter until it's clearly a drag.
        if !self.dragging {
            let (dx, dy) = (x - self.press_px.0, y - self.press_px.1);
            if dx * dx + dy * dy < 9.0 {
                return false;
            }
            self.dragging = true;
        }
        // Editor drag: move the text cursor, keeping the anchor set at the press.
        if let Some(sid) = self.editor_drag {
            if let Some((s, row, col)) = self.editor_pos_at(x, y) {
                if s == sid {
                    if let Some(e) = self.app.editor_mut(sid) {
                        if e.cursor != (row, col) {
                            e.cursor = (row, col);
                            self.dirty = true;
                            return true;
                        }
                    }
                }
            }
            return false;
        }
        let Some((surface, ac, ar)) = self.press_cell else {
            return false;
        };
        let Some((cur_surface, col, row)) = self.cell_at(x, y) else {
            return false;
        };
        if cur_surface != surface {
            return false; // don't select across panes
        }
        if self.selection.map(|s| s.head) == Some((col, row)) {
            return false;
        }
        self.selection = Some(Selection {
            surface,
            anchor: (ac, ar),
            head: (col, row),
        });
        self.dirty = true;
        true
    }

    /// Finish a drag: copy a non-empty terminal selection to the clipboard. For an
    /// editor, a plain click (no drag) clears the anchor so nothing is selected;
    /// a real drag leaves the selection for Cmd+C (editors don't auto-copy).
    fn end_selection(&mut self) {
        self.mouse_down = false;
        if let Some(sid) = self.editor_drag.take() {
            if !self.dragging {
                if let Some(e) = self.app.editor_mut(sid) {
                    e.anchor = None;
                }
            }
            self.dragging = false;
            self.dirty = true;
            return;
        }
        if self.dragging {
            self.copy_selection();
        }
        self.dragging = false;
        self.press_cell = None;
    }

    /// Extend (or start) a keyboard selection over the focused terminal grid by
    /// one cell (or one word) in `dir`. The anchor is the current selection's, or
    /// the terminal cursor if none. Not for editor surfaces.
    fn extend_terminal_selection(&mut self, dir: ArrowDir, by_word: bool) {
        let Some(sid) = self.app.focused_surface() else {
            return;
        };
        if self.app.is_editor(sid) {
            return;
        }
        let Some(grid) = self.grid_cache.get(&sid) else {
            return;
        };
        let (cols, rows) = (grid.size.cols, grid.size.rows);
        if cols == 0 || rows == 0 {
            return;
        }
        let (anchor, mut head) = match self.selection {
            Some(s) if s.surface == sid => (s.anchor, s.head),
            _ => {
                let c = (
                    grid.cursor.col.min(cols - 1),
                    grid.cursor.row.min(rows - 1),
                );
                (c, c)
            }
        };
        match dir {
            ArrowDir::Left => {
                head.0 = if by_word {
                    word_col(grid, head.1, head.0, false)
                } else {
                    head.0.saturating_sub(1)
                };
            }
            ArrowDir::Right => {
                head.0 = if by_word {
                    word_col(grid, head.1, head.0, true)
                } else {
                    (head.0 + 1).min(cols - 1)
                };
            }
            ArrowDir::Up => head.1 = head.1.saturating_sub(1),
            ArrowDir::Down => head.1 = (head.1 + 1).min(rows - 1),
        }
        self.selection = Some(Selection {
            surface: sid,
            anchor,
            head,
        });
        self.dirty = true;
    }

    /// Paste the clipboard into the focused terminal.
    fn paste_to_terminal(&mut self) {
        if let Some(cb) = self.clipboard.as_mut() {
            if let Ok(text) = cb.get_text() {
                if !text.is_empty() {
                    self.app.write_to_focused(text.as_bytes());
                    self.dirty = true;
                }
            }
        }
    }

    /// Copy the current selection's text to the system clipboard.
    fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            return;
        }
        let Some(grid) = self.grid_cache.get(&sel.surface) else {
            return;
        };
        let text = selection_text(grid, sel);
        if !text.is_empty() {
            if let Some(cb) = self.clipboard.as_mut() {
                let _ = cb.set_text(text);
            }
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
        // Over the sidebar, the wheel scrolls the workspace list, not the terminal
        // — and pixel-precise, so it tracks the trackpad exactly.
        if self.in_sidebar(self.cursor.0) {
            let before = self.sidebar_scroll;
            self.sidebar_scroll = (self.sidebar_scroll - px).clamp(0.0, self.sidebar_max_scroll);
            if self.sidebar_scroll != before {
                self.dirty = true;
                self.frame_pending = true;
            }
            return;
        }
        // Accumulate sub-line pixels and carry the remainder, so a slow drag isn't
        // rounded to zero (the old behaviour: motion under half a cell vanished,
        // which felt like a dead zone and a speed threshold) and momentum tails
        // aren't dropped. The viewport moves by whole lines; the leftover fraction
        // rides along to the next event.
        self.scroll_accum += px;
        let lines = (self.scroll_accum / self.cell_h).trunc() as i32;
        self.scroll_accum -= lines as f32 * self.cell_h;
        // An editor under the cursor scrolls its own text buffer: vertically by
        // lines, and (when soft-wrap is off) horizontally by columns.
        let over = self.surface_under_cursor().or_else(|| self.app.focused_surface());
        if let Some(sid) = over {
            if self.app.is_editor(sid) {
                let wrap = self.soft_wrap_on(sid);
                if let Some(e) = self.app.editor_mut(sid) {
                    let mut changed = false;
                    if lines != 0 {
                        let max = e.lines.len().saturating_sub(1);
                        let ns = ((e.scroll as isize - lines as isize).max(0) as usize).min(max);
                        if ns != e.scroll {
                            e.scroll = ns;
                            changed = true;
                        }
                    }
                    if !wrap && px_x != 0.0 {
                        let cols = (px_x / self.cell_w).round() as isize;
                        if cols != 0 {
                            let maxlen =
                                e.lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
                            let nh = ((e.hscroll as isize - cols).max(0) as usize).min(maxlen);
                            if nh != e.hscroll {
                                e.hscroll = nh;
                                changed = true;
                            }
                        }
                    }
                    if changed {
                        self.dirty = true;
                        self.frame_pending = true;
                    }
                }
                return;
            }
        }
        if lines == 0 {
            return;
        }
        // Don't drive the viewport straight to the target: queue the motion and
        // meter it out at SCROLL_STEP lines/frame (see `apply_pending_scroll`), so
        // each frame reveals only what shaping can finish — smooth, coherent, and
        // untorn even on a hard flick. Wheel up (positive delta) reveals older
        // history → negative viewport delta.
        let Some(target) = self.surface_under_cursor().or_else(|| self.app.focused_surface())
        else {
            return;
        };
        let delta = -lines;
        if self.pending_scroll_target != Some(target) {
            // New target: abandon any leftover momentum aimed at the old surface.
            self.pending_scroll = 0;
            self.pending_scroll_target = Some(target);
        }
        if self.pending_scroll != 0 && (self.pending_scroll > 0) != (delta > 0) {
            // Reversal: drop the opposing backlog so the new direction responds at
            // once instead of first unwinding stale momentum.
            self.pending_scroll = delta;
        } else {
            self.pending_scroll = self.pending_scroll.saturating_add(delta);
        }
        self.pending_scroll = self
            .pending_scroll
            .clamp(-MAX_PENDING_SCROLL, MAX_PENDING_SCROLL);
        // The selection is viewport-relative; scrolling invalidates it.
        self.selection = None;
        self.dirty = true;
        // Pace the redraw through the frame clock (coalesces wheel bursts).
        self.frame_pending = true;
    }

    /// Advance the viewport toward a queued scroll by at most `SCROLL_STEP` lines,
    /// so a fast flick's momentum plays out over several coherent frames instead of
    /// snapping the viewport somewhere shaping can't fill in one frame. Returns
    /// whether a step was applied (the caller then treats the frame as dirty).
    fn apply_pending_scroll(&mut self) -> bool {
        if self.pending_scroll == 0 {
            return false;
        }
        let Some(sid) = self.pending_scroll_target else {
            self.pending_scroll = 0;
            return false;
        };
        // Ease-out: drain a fraction of the backlog, bounded to [MIN, MAX] lines
        // and never past what remains. Small scrolls creep coherently; big flicks
        // move fast and settle.
        let mag = self.pending_scroll.abs();
        let step_mag = (mag / SCROLL_EASE_DIVISOR)
            .clamp(SCROLL_STEP_MIN, SCROLL_STEP_MAX)
            .min(mag);
        let step = step_mag * self.pending_scroll.signum();
        let applied = match self.app.terminal(sid) {
            Some(t) => {
                t.scroll(Scroll::Delta(step));
                true
            }
            // Surface went away mid-catch-up: drop the remainder.
            None => {
                self.pending_scroll = 0;
                self.pending_scroll_target = None;
                return false;
            }
        };
        self.pending_scroll -= step;
        if self.pending_scroll != 0 {
            // More to go: keep the frame clock ticking so we step again next frame.
            self.frame_pending = true;
        }
        applied
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
        let bar_w = self.sidebar_width();
        let bar_x = self.sidebar_rect().x;
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
                y: 0.0,
                w: bar_w,
                h: sh,
            },
            sw,
            sh,
            self.chrome.sidebar,
            1.0,
        ));

        // One buffer per workspace plus one for the pinned "+" button.
        while self.sidebar_buffers.len() < n + 1 {
            let b = Buffer::new(&mut self.font_system, metrics);
            self.sidebar_buffers.push(b);
        }

        let mut placements = Vec::new();

        // Pinned "+" new-workspace button at the very top.
        let btn_y = 0.0; // flush against the top edge (no margin above the button)
        let btn = Rect {
            x: bar_x,
            y: btn_y,
            w: bar_w,
            h: row_h,
        };
        let btn_hovered = rect_contains(btn, self.cursor.0, self.cursor.1);
        if btn_hovered {
            // Full-bleed row highlight (edge to edge, full height) — a row button,
            // not a compact icon.
            quads.push(rect_quad(btn, sw, sh, BUTTON_HOVER_BG, BUTTON_HOVER_ALPHA));
        }
        let btn_color = if btn_hovered {
            NEW_VTAB_LABEL_HOVER
        } else {
            NEW_VTAB_LABEL
        };
        {
            let buf = &mut self.sidebar_buffers[n];
            buf.set_metrics(metrics);
            buf.set_size(Some((bar_w - pad * 2.0).max(1.0)), Some(self.cell_h));
            buf.set_rich_text(
                std::iter::once(("+  New workspace", attrs_for(btn_color))),
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
        }
        placements.push(Placement {
            idx: n,
            left: bar_x + pad,
            top: btn_y + (row_h - self.cell_h) * 0.5,
            bounds: TextBounds {
                left: bar_x as i32,
                top: btn_y as i32,
                right: (bar_x + bar_w) as i32,
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
            buf.shape_until_scroll(&mut self.font_system, false);
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
                    BUTTON_HOVER_BG,
                    BUTTON_HOVER_ALPHA,
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
                color: if hovered { CLOSE_GLYPH_HOVER } else { CLOSE_GLYPH },
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
            let b = Buffer::new(&mut self.font_system, metrics);
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
            buf.shape_until_scroll(&mut self.font_system, false);
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
        // Cmd-chords drive the app via the registry; everything else goes to the
        // focused terminal. (Cmd is reserved so app shortcuts never reach a shell.)
        if self.mods.super_ {
            // Editor Cmd combos (clipboard + line/doc nav) take priority.
            if self.app.focused_is_editor() && self.editor_cmd_key(event) {
                return;
            }
            // Cmd+Arrow: line start/end in the focused terminal (Ctrl-A / Ctrl-E).
            if !self.app.focused_is_editor() {
                let bytes: Option<&[u8]> = match &event.logical_key {
                    WKey::Named(NamedKey::ArrowLeft) | WKey::Named(NamedKey::ArrowUp) => {
                        Some(&[0x01])
                    }
                    WKey::Named(NamedKey::ArrowRight) | WKey::Named(NamedKey::ArrowDown) => {
                        Some(&[0x05])
                    }
                    _ => None,
                };
                if let Some(b) = bytes {
                    self.selection = None;
                    self.app.write_to_focused(b);
                    self.dirty = true;
                    return;
                }
            }
            if let Some(c) = match &event.logical_key {
                WKey::Character(s) => s.chars().next(),
                _ => None,
            } {
                // Cmd+C copies an active selection (else it falls through as a
                // reserved Cmd chord).
                if c.eq_ignore_ascii_case(&'c') && self.selection.map(|s| !s.is_empty()).unwrap_or(false)
                {
                    self.copy_selection();
                    return;
                }
                // Cmd+V pastes the clipboard into the focused terminal.
                if c.eq_ignore_ascii_case(&'v') && !self.app.focused_is_editor() {
                    self.paste_to_terminal();
                    return;
                }
                // Cmd+, opens the config file in an editor (settings live in the file).
                if c == ',' {
                    self.open_config_editor();
                    return;
                }
                // Cmd+S saves the focused editor; saving the config hot-reloads it.
                if c.eq_ignore_ascii_case(&'s') && self.app.focused_is_editor() {
                    let saved = self.app.focused_editor_mut().and_then(|e| {
                        let _ = e.save();
                        e.path.clone()
                    });
                    self.dirty = true;
                    if saved.is_some()
                        && saved == ghostrealm_core::config::config_path()
                    {
                        self.reload_config();
                    }
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
                }
            }
            return;
        }

        // Shift + Page/Home/End drives scrollback (terminal only; the editor
        // handles Shift itself for selection).
        if self.mods.shift && !self.app.focused_is_editor() {
            let page = ((self.config.height as f32 / self.cell_h).floor() as i32 - 1).max(1);
            let scroll = match &event.logical_key {
                WKey::Named(NamedKey::PageUp) => Some(Scroll::Delta(-page)),
                WKey::Named(NamedKey::PageDown) => Some(Scroll::Delta(page)),
                WKey::Named(NamedKey::Home) => Some(Scroll::Top),
                WKey::Named(NamedKey::End) => Some(Scroll::Bottom),
                _ => None,
            };
            if let Some(scroll) = scroll {
                self.app.scroll_focused(scroll);
                self.dirty = true;
                return;
            }
            // Shift+Arrow extends a keyboard selection over the terminal grid
            // (word-wise with Alt) instead of corrupting the shell input.
            if !self.app.focused_is_editor() {
                let dir = match &event.logical_key {
                    WKey::Named(NamedKey::ArrowLeft) => Some(ArrowDir::Left),
                    WKey::Named(NamedKey::ArrowRight) => Some(ArrowDir::Right),
                    WKey::Named(NamedKey::ArrowUp) => Some(ArrowDir::Up),
                    WKey::Named(NamedKey::ArrowDown) => Some(ArrowDir::Down),
                    _ => None,
                };
                if let Some(dir) = dir {
                    self.extend_terminal_selection(dir, self.mods.alt);
                    return;
                }
                // Shift+Enter sends a newline (LF) as a raw byte — it does NOT go
                // through the Enter path, so it never triggers the optimistic busy
                // dot. (Whether the shell treats LF as a continuation vs submit is
                // the shell's line-editor config.)
                if matches!(event.logical_key, WKey::Named(NamedKey::Enter)) {
                    self.selection = None;
                    self.app.write_to_focused(b"\n");
                    return;
                }
            }
        }

        // Option+Left/Right = word motion in the terminal (readline ESC-b / ESC-f);
        // Option+Up/Down send a plain arrow (avoid the corrupting modified CSI).
        if self.mods.alt && !self.app.focused_is_editor() {
            match &event.logical_key {
                WKey::Named(NamedKey::ArrowLeft) => {
                    self.selection = None;
                    self.app.write_to_focused(b"\x1bb");
                    return;
                }
                WKey::Named(NamedKey::ArrowRight) => {
                    self.selection = None;
                    self.app.write_to_focused(b"\x1bf");
                    return;
                }
                WKey::Named(NamedKey::ArrowUp) | WKey::Named(NamedKey::ArrowDown) => {
                    let up = matches!(event.logical_key, WKey::Named(NamedKey::ArrowUp));
                    self.selection = None;
                    self.app.send_key_to_focused(&KeyPress {
                        key: if up { Key::Up } else { Key::Down },
                        mods: Mods::default(),
                        text: None,
                    });
                    return;
                }
                _ => {}
            }
        }

        // An editor surface consumes keys itself (no PTY).
        if self.app.focused_is_editor() {
            self.editor_key(event);
            return;
        }

        // Any key that reaches the shell clears a keyboard selection.
        self.selection = None;
        let text = event.text.as_ref().map(|s| s.to_string());
        let key = match &event.logical_key {
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
                _ => return,
            },
            WKey::Character(s) => match s.chars().next() {
                Some(c) => Key::Char(c),
                None => return,
            },
            _ => return,
        };
        self.app.send_key_to_focused(&KeyPress {
            key,
            mods: self.mods,
            text,
        });
    }

    /// Route a key to the focused editor buffer. Shift extends a selection.
    fn editor_key(&mut self, event: &winit::event::KeyEvent) {
        let selecting = self.mods.shift;
        let Some(e) = self.app.focused_editor_mut() else {
            return;
        };
        // Motions (Shift extends the selection, else it clears).
        let motion = match &event.logical_key {
            WKey::Named(NamedKey::ArrowLeft) => Some(Motion::Left),
            WKey::Named(NamedKey::ArrowRight) => Some(Motion::Right),
            WKey::Named(NamedKey::ArrowUp) => Some(Motion::Up),
            WKey::Named(NamedKey::ArrowDown) => Some(Motion::Down),
            WKey::Named(NamedKey::Home) => Some(Motion::Home),
            WKey::Named(NamedKey::End) => Some(Motion::End),
            _ => None,
        };
        if let Some(m) = motion {
            if selecting {
                e.move_cursor_selecting(m);
            } else {
                e.move_cursor(m);
            }
            self.dirty = true;
            return;
        }
        match &event.logical_key {
            WKey::Named(NamedKey::Enter) => e.insert_newline(),
            WKey::Named(NamedKey::Backspace) => e.backspace(),
            WKey::Named(NamedKey::Delete) => e.delete_forward(),
            WKey::Named(NamedKey::Space) => e.insert_char(' '),
            WKey::Named(NamedKey::Tab) => {
                for _ in 0..4 {
                    e.insert_char(' ');
                }
            }
            WKey::Character(s) => {
                for c in s.chars() {
                    e.insert_char(c);
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    /// Cmd combos while an editor is focused: clipboard (C/X/V) and line/document
    /// navigation (arrows). Returns whether the key was consumed.
    fn editor_cmd_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        let selecting = self.mods.shift;
        if let WKey::Character(s) = &event.logical_key {
            match s.chars().next().map(|c| c.to_ascii_lowercase()) {
                Some('c') => {
                    if let Some(t) = self.app.focused_editor_mut().and_then(|e| e.selected_text()) {
                        if let Some(cb) = self.clipboard.as_mut() {
                            let _ = cb.set_text(t);
                        }
                    }
                    return true;
                }
                Some('x') => {
                    let cut = self.app.focused_editor_mut().and_then(|e| {
                        let t = e.selected_text();
                        if t.is_some() {
                            e.delete_selection();
                        }
                        t
                    });
                    if let Some(t) = cut {
                        if let Some(cb) = self.clipboard.as_mut() {
                            let _ = cb.set_text(t);
                        }
                        self.dirty = true;
                    }
                    return true;
                }
                Some('v') => {
                    if let Some(text) = self.clipboard.as_mut().and_then(|c| c.get_text().ok()) {
                        if let Some(e) = self.app.focused_editor_mut() {
                            e.insert_str(&text);
                            self.dirty = true;
                        }
                    }
                    return true;
                }
                _ => {}
            }
        }
        // Cmd+Left/Right = line start/end; Cmd+Up/Down = document start/end.
        let Some(e) = self.app.focused_editor_mut() else {
            return false;
        };
        match &event.logical_key {
            WKey::Named(NamedKey::ArrowLeft) => {
                if selecting {
                    e.move_cursor_selecting(Motion::Home);
                } else {
                    e.move_cursor(Motion::Home);
                }
            }
            WKey::Named(NamedKey::ArrowRight) => {
                if selecting {
                    e.move_cursor_selecting(Motion::End);
                } else {
                    e.move_cursor(Motion::End);
                }
            }
            WKey::Named(NamedKey::ArrowUp) => e.move_document(false, selecting),
            WKey::Named(NamedKey::ArrowDown) => e.move_document(true, selecting),
            _ => return false,
        }
        self.dirty = true;
        true
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
                let b = Buffer::new(&mut self.font_system, metrics);
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
                buf.shape_until_scroll(&mut self.font_system, false);
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
            let b = Buffer::new(&mut self.font_system, metrics);
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
            buf.shape_until_scroll(&mut self.font_system, false);
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
        // Meter out any queued scroll one bounded step, before deciding to render.
        if self.apply_pending_scroll() {
            self.dirty = true;
        }
        // Advance the auto-read dwell; a status change needs a redraw.
        if self.app.tick_inbox() {
            self.dirty = true;
        }
        // Autosave an editor when focus moves off it (workspace/tab/pane switch).
        let focused = self.app.focused_surface();
        if focused != self.last_focused_surface {
            if let Some(prev) = self.last_focused_surface {
                self.autosave_editor(prev);
            }
            self.last_focused_surface = focused;
        }
        if !self.dirty {
            return Ok(());
        }

        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let workspace = self.workspace_rect();
        let panes = self.active_panes(workspace);

        // Resolve each pane's tab-strip height, editor ribbon height, and the body
        // (below-chrome) rect. A single-tab editor shows a filename ribbon instead
        // of a tab strip (multi-tab editors show the filename in their tab).
        let mut resolved: Vec<(PaneRender, f32, f32, Rect)> = Vec::with_capacity(panes.len());
        for pr in panes {
            let strip_h = if self.strip_shown(pr.surfaces.len()) {
                self.strip_height()
            } else {
                0.0
            };
            let active_is_editor = pr
                .surfaces
                .iter()
                .find(|(_, _, a)| *a)
                .is_some_and(|(s, _, _)| self.app.is_editor(*s));
            let ribbon_h = if active_is_editor && strip_h == 0.0 {
                self.strip_height()
            } else {
                0.0
            };
            let chrome_h = strip_h + ribbon_h;
            let term = Rect {
                x: pr.rect.x,
                y: pr.rect.y + chrome_h,
                w: pr.rect.w,
                h: (pr.rect.h - chrome_h).max(1.0),
            };
            resolved.push((pr, strip_h, ribbon_h, term));
        }

        // Resize a pane's terminal only when its cell dimensions actually change;
        // a focus-only change or new content costs no resize. The content-keyed
        // row cache is position-independent, so switching panes/vtabs or scrolling
        // never invalidates it.
        let (cw, ch) = (self.cell_w.round() as u32, self.cell_h.round() as u32);
        for (pr, _, _, term) in &resolved {
            let Some(sid) = pr
                .surfaces
                .iter()
                .find(|(_, _, a)| *a)
                .map(|(id, _, _)| *id)
            else {
                continue;
            };
            let cols = ((term.w / self.cell_w).floor() as u16).max(1);
            let rows = ((term.h / self.cell_h).floor() as u16).max(1);
            if self.surface_geom.get(&sid) != Some(&(cols, rows)) {
                self.app.resize_surface(sid, cols, rows, cw, ch);
                self.surface_geom.insert(sid, (cols, rows));
            }
        }

        self.row_cache.begin_frame(self.metrics_gen);
        let metrics = self.metrics();
        // Bound shaping work per frame: cache hits are free, but cache misses
        // (never-seen rows, e.g. a fast scroll into history) are shaped only
        // while under budget. Overflow rows are deferred to later frames.
        let shape_deadline = Instant::now() + SHAPE_BUDGET;
        let mut shaped = 0usize;
        let mut deferred = false;
        // Row content key drawn at each screen position this frame, so a deferred
        // row can fall back to its previous content instead of a blank gap.
        let mut cur_rows: HashMap<(i32, i32), u64> = HashMap::new();
        let mut bg_quads: Vec<QuadInstance> = Vec::new();
        let mut overlay_quads: Vec<QuadInstance> = Vec::new();
        let mut row_placements: Vec<RowPlacement> = Vec::new();
        let mut strip_placements: Vec<Placement> = Vec::new();
        let mut strip_idx = 0usize;
        let multi_pane = resolved.len() > 1;

        // Close-button chrome: one shared '×' glyph placed at every tab/workspace
        // close button, and the hit rects those buttons occupy (rebuilt each frame).
        self.buttons.clear();
        let mut close_placements: Vec<Placement> = Vec::new();
        let active_vt = self.app.tree.active_vtab();
        self.close_buffer.set_metrics(metrics);
        self.close_buffer
            .set_size(Some(self.cell_w * 2.0), Some(self.cell_h));
        self.close_buffer.set_rich_text(
            std::iter::once(("\u{00d7}", attrs_for(CLOSE_GLYPH))),
            &Attrs::new().family(Family::SansSerif),
            Shaping::Advanced,
            None,
        );
        self.close_buffer
            .shape_until_scroll(&mut self.font_system, false);

        for (pr, strip_h, ribbon_h, term) in &resolved {
            // Horizontal tab strip (autohidden for single-surface panes).
            if *strip_h > 0.0 {
                let n = pr.surfaces.len().max(1);
                let tab_w = pr.rect.w / n as f32;
                bg_quads.push(rect_quad(
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
                        bg_quads.push(rect_quad(
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
                            .push(Buffer::new(&mut self.font_system, metrics));
                    }
                    // Italic when the tab is an editor with unsaved edits.
                    let unsaved = self.app.editor(*sid).is_some_and(|e| e.modified);
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
                    buf.shape_until_scroll(&mut self.font_system, false);
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
                            bg_quads.push(rect_quad(
                                hover_box(hit, close_w),
                                sw,
                                sh,
                                BUTTON_HOVER_BG,
                                BUTTON_HOVER_ALPHA,
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
                            color: if hovered { CLOSE_GLYPH_HOVER } else { CLOSE_GLYPH },
                        });
                        self.buttons.push((hit, ButtonAction::CloseSurface(vt, pr.id, *sid)));
                    }
                }
            }

            let Some((active_sid, _, _)) = pr.surfaces.iter().find(|(_, _, a)| *a) else {
                continue;
            };
            let sid = *active_sid;

            // Editor filename ribbon (a single-tab editor's own line; multi-tab
            // editors show their filename in the tab instead). Filename italic when
            // there are unsaved edits, followed by dim size/line-count metrics.
            if *ribbon_h > 0.0 {
                let ry = pr.rect.y + *strip_h;
                bg_quads.push(rect_quad(
                    Rect {
                        x: pr.rect.x,
                        y: ry,
                        w: pr.rect.w,
                        h: *ribbon_h,
                    },
                    sw,
                    sh,
                    self.chrome.sidebar,
                    1.0,
                ));
                let info = self.app.editor(sid).map(|e| {
                    let bytes =
                        e.lines.iter().map(|l| l.len()).sum::<usize>() + e.lines.len().saturating_sub(1);
                    (e.title(), e.modified, bytes, e.lines.len())
                });
                if let Some((fname, modified, bytes, nlines)) = info {
                    let pad = 8.0 * self.scale;
                    if strip_idx >= self.strip_buffers.len() {
                        self.strip_buffers
                            .push(Buffer::new(&mut self.font_system, metrics));
                    }
                    let mut name_attrs = Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgb(220, 220, 230));
                    if modified {
                        name_attrs = name_attrs.style(Style::Italic);
                    }
                    let meta = format!("{bytes} B   ·   {nlines} lines");
                    let dim = Attrs::new()
                        .family(Family::SansSerif)
                        .color(Color::rgb(140, 140, 155));
                    let top = ry + (*ribbon_h - self.cell_h) * 0.5;
                    // Reserve the far-right for the sticky soft-wrap toggle.
                    let wb_w = self.cell_h;
                    let wb_x = pr.rect.x + pr.rect.w - wb_w - pad;
                    let meta_w = meta.chars().count() as f32 * self.cell_w;
                    let meta_left = wb_x - pad - meta_w;
                    // Filename, left-aligned.
                    if strip_idx >= self.strip_buffers.len() {
                        self.strip_buffers
                            .push(Buffer::new(&mut self.font_system, metrics));
                    }
                    let buf = &mut self.strip_buffers[strip_idx];
                    buf.set_metrics(metrics);
                    buf.set_wrap(Wrap::None);
                    buf.set_size(
                        Some((meta_left - (pr.rect.x + pad)).max(1.0)),
                        Some(self.cell_h),
                    );
                    buf.set_rich_text(
                        std::iter::once((fname.as_str(), name_attrs)),
                        &Attrs::new().family(Family::SansSerif),
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    strip_placements.push(Placement {
                        idx: strip_idx,
                        left: pr.rect.x + pad,
                        top,
                        bounds: TextBounds {
                            left: pr.rect.x as i32,
                            top: ry as i32,
                            right: meta_left as i32,
                            bottom: (ry + *ribbon_h) as i32,
                        },
                        color: [220, 220, 230],
                    });
                    strip_idx += 1;
                    // Metrics, right-aligned.
                    if strip_idx >= self.strip_buffers.len() {
                        self.strip_buffers
                            .push(Buffer::new(&mut self.font_system, metrics));
                    }
                    let buf = &mut self.strip_buffers[strip_idx];
                    buf.set_metrics(metrics);
                    buf.set_wrap(Wrap::None);
                    buf.set_size(Some(meta_w + self.cell_w), Some(self.cell_h));
                    buf.set_rich_text(
                        std::iter::once((meta.as_str(), dim)),
                        &Attrs::new().family(Family::SansSerif),
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    strip_placements.push(Placement {
                        idx: strip_idx,
                        left: meta_left,
                        top,
                        bounds: TextBounds {
                            left: meta_left as i32,
                            top: ry as i32,
                            right: (pr.rect.x + pr.rect.w) as i32,
                            bottom: (ry + *ribbon_h) as i32,
                        },
                        color: [140, 140, 155],
                    });
                    strip_idx += 1;
                    // Sticky soft-wrap toggle button (right end of the ribbon).
                    let wb_on = self.soft_wrap_on(sid);
                    let wb_hit = Rect {
                        x: wb_x,
                        y: ry,
                        w: wb_w,
                        h: *ribbon_h,
                    };
                    let wb_hovered = rect_contains(wb_hit, self.cursor.0, self.cursor.1);
                    if wb_hovered {
                        bg_quads.push(rect_quad(
                            hover_box(wb_hit, wb_w),
                            sw,
                            sh,
                            BUTTON_HOVER_BG,
                            BUTTON_HOVER_ALPHA,
                        ));
                    }
                    let wb_color = if wb_on { self.chrome.accent } else { CLOSE_GLYPH };
                    if strip_idx >= self.strip_buffers.len() {
                        self.strip_buffers
                            .push(Buffer::new(&mut self.font_system, metrics));
                    }
                    let buf = &mut self.strip_buffers[strip_idx];
                    buf.set_metrics(metrics);
                    buf.set_wrap(Wrap::None);
                    buf.set_size(Some(wb_w), Some(self.cell_h));
                    buf.set_rich_text(
                        std::iter::once(("\u{21a9}", attrs_for(wb_color))),
                        &Attrs::new().family(Family::Monospace),
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    strip_placements.push(Placement {
                        idx: strip_idx,
                        left: wb_x + (wb_w - self.cell_w) * 0.5,
                        top,
                        bounds: TextBounds {
                            left: wb_x as i32,
                            top: ry as i32,
                            right: (wb_x + wb_w) as i32,
                            bottom: (ry + *ribbon_h) as i32,
                        },
                        color: wb_color,
                    });
                    strip_idx += 1;
                    self.buttons.push((wb_hit, ButtonAction::ToggleSoftWrap(sid)));
                }
            }

            // Editor surface: render its text buffer instead of a terminal grid.
            if self.app.is_editor(sid) {
                let rows_vis = (term.h / self.cell_h).floor().max(1.0) as usize;
                let wrap = self.soft_wrap_on(sid);
                // Layout dims (the gutter width needs the line count).
                let total_lines = match self.app.editor(sid) {
                    Some(e) => e.lines.len(),
                    None => continue,
                };
                let gutter_w = self.editor_gutter_w(total_lines);
                let body_x = term.x + gutter_w;
                let body_w = (term.w - gutter_w).max(1.0);
                let body_cols = ((body_w / self.cell_w).floor() as usize).max(1);
                // Follow the cursor only when it moved (so manual scrolling sticks);
                // reset horizontal scroll under wrap; keep vertical scroll in bounds.
                if let Some(e) = self.app.editor_mut(sid) {
                    e.follow_cursor(rows_vis, if wrap { None } else { Some(body_cols) });
                    if wrap {
                        e.hscroll = 0;
                    }
                    e.clamp_scroll_bounds();
                }
                #[allow(clippy::type_complexity)]
                let (scroll, hscroll, cursor, sel, visible): (
                    usize,
                    usize,
                    (usize, usize),
                    Option<((usize, usize), (usize, usize))>,
                    Vec<String>,
                ) = match self.app.editor(sid) {
                    Some(e) => (
                        e.scroll,
                        e.hscroll,
                        e.cursor,
                        e.selection(),
                        (0..rows_vis)
                            .map(|i| e.lines.get(e.scroll + i).cloned().unwrap_or_default())
                            .collect(),
                    ),
                    None => continue,
                };
                bg_quads.push(rect_quad(*term, sw, sh, EDITOR_BG, 1.0));

                // Lay out the visible visual rows: (logical_line, text, start_col,
                // y). Soft-wrap breaks long logical lines into several visual rows;
                // otherwise each line is one row, offset left by hscroll and clipped.
                let wrap_cols = if wrap { body_cols } else { usize::MAX };
                let bottom = term.y + term.h;
                let mut vis: Vec<(usize, String, usize, f32)> = Vec::new();
                let mut vy = term.y;
                for (i, line) in visible.iter().enumerate() {
                    if vy >= bottom {
                        break;
                    }
                    let ln = scroll + i;
                    if wrap {
                        for (text, start) in wrap_line(line, wrap_cols) {
                            if vy >= bottom {
                                break;
                            }
                            vis.push((ln, text, start, vy));
                            vy += self.cell_h;
                        }
                    } else {
                        let text: String = line.chars().skip(hscroll).collect();
                        vis.push((ln, text, hscroll, vy));
                        vy += self.cell_h;
                    }
                }

                // Active-line highlight behind every visual row of the cursor line.
                if self.cfg.editor.cursor_line {
                    for (ln, _t, _s, y) in &vis {
                        if *ln == cursor.0 {
                            bg_quads.push(rect_quad(
                                Rect { x: term.x, y: *y, w: term.w, h: self.cell_h },
                                sw,
                                sh,
                                EDITOR_CURSOR_LINE,
                                EDITOR_CURSOR_LINE_ALPHA,
                            ));
                        }
                    }
                }
                // Selection highlight (behind text), intersected with each row.
                if let Some(((sr, sc), (er, ec))) = sel {
                    for (ln, text, start, y) in &vis {
                        let ln = *ln;
                        if ln < sr || ln > er {
                            continue;
                        }
                        let row_end = start + text.chars().count();
                        let first_l = if ln == sr { sc } else { 0 };
                        let last_l = if ln == er { ec } else { usize::MAX };
                        let vfirst = first_l.max(*start);
                        let vlast = last_l.min(row_end);
                        if vlast > vfirst {
                            bg_quads.push(rect_quad(
                                Rect {
                                    x: body_x + (vfirst - start) as f32 * self.cell_w,
                                    y: *y,
                                    w: (vlast - vfirst) as f32 * self.cell_w,
                                    h: self.cell_h,
                                },
                                sw,
                                sh,
                                self.chrome.accent,
                                0.35,
                            ));
                        }
                    }
                }
                // Line-number gutter — only on each logical line's first visual row.
                if gutter_w > 0.0 {
                    for (ln, _text, start, y) in &vis {
                        if *start != 0 || *ln >= total_lines {
                            continue;
                        }
                        let is_cur = *ln == cursor.0;
                        let num = match self.cfg.editor.line_numbers {
                            LineNumbers::Relative if !is_cur => {
                                (cursor.0 as isize - *ln as isize).unsigned_abs()
                            }
                            _ => ln + 1, // absolute (and the cursor line in relative mode)
                        };
                        let s = num.to_string();
                        let color = if is_cur { EDITOR_GUTTER_CUR } else { EDITOR_GUTTER };
                        let key = self.row_cache.row_key(std::iter::once((s.as_str(), color)));
                        if self.row_cache.buffer(key).is_some() {
                            self.row_cache
                                .ensure(key, &mut self.font_system, metrics, gutter_w, self.cell_h, &[]);
                        } else {
                            let spans = vec![(s.clone(), color)];
                            self.row_cache
                                .ensure(key, &mut self.font_system, metrics, gutter_w, self.cell_h, &spans);
                        }
                        let num_w = s.chars().count() as f32 * self.cell_w;
                        row_placements.push(RowPlacement {
                            key,
                            left: body_x - self.cell_w - num_w,
                            top: *y,
                            bounds: TextBounds {
                                left: term.x as i32,
                                top: term.y as i32,
                                right: body_x as i32,
                                bottom: bottom as i32,
                            },
                            color,
                        });
                    }
                }
                // Text for each visual row (keyed by its own content, so wrapped
                // sub-rows are cache-friendly and width-independent).
                for (_ln, text, _start, y) in &vis {
                    let key = self
                        .row_cache
                        .row_key(std::iter::once((text.as_str(), EDITOR_FG)));
                    let pos = (body_x as i32, *y as i32);
                    let place_key = if self.row_cache.buffer(key).is_some() {
                        self.row_cache
                            .ensure(key, &mut self.font_system, metrics, body_w, self.cell_h, &[]);
                        key
                    } else if shaped < MIN_SHAPES_PER_FRAME || Instant::now() < shape_deadline {
                        let spans = vec![(text.clone(), EDITOR_FG)];
                        self.row_cache
                            .ensure(key, &mut self.font_system, metrics, body_w, self.cell_h, &spans);
                        shaped += 1;
                        key
                    } else {
                        deferred = true;
                        Self::stale_key(
                            &self.prev_rows,
                            &mut self.row_cache,
                            &mut self.font_system,
                            pos,
                            key,
                            metrics,
                            body_w,
                            self.cell_h,
                        )
                    };
                    cur_rows.insert(pos, place_key);
                    row_placements.push(RowPlacement {
                        key: place_key,
                        left: body_x,
                        top: *y,
                        bounds: TextBounds {
                            left: body_x as i32,
                            top: term.y as i32,
                            right: (term.x + term.w) as i32,
                            bottom: bottom as i32,
                        },
                        color: EDITOR_FG,
                    });
                }
                // Cursor (thin bar): the cursor-line row with the largest start
                // that the column still falls on.
                if let Some((_, _t, start, y)) = vis
                    .iter()
                    .rev()
                    .find(|(ln, _t, start, _y)| *ln == cursor.0 && *start <= cursor.1)
                {
                    let cx = body_x + (cursor.1 - start) as f32 * self.cell_w;
                    // Clip to the body so a scrolled-off cursor never draws over the
                    // gutter or the neighbouring pane/sidebar.
                    if cx >= body_x && cx < term.x + term.w {
                        overlay_quads.push(rect_quad(
                            Rect {
                                x: cx,
                                y: *y,
                                w: 2.0 * self.scale,
                                h: self.cell_h,
                            },
                            sw,
                            sh,
                            self.chrome.accent,
                            0.9,
                        ));
                    }
                }
                if pr.focused && multi_pane {
                    push_border(&mut overlay_quads, pr.rect, sw, sh, self.chrome.accent);
                }
                continue;
            }

            // Snapshot only when the surface actually changed since we last did;
            // an idle pane reuses its cached grid.
            if self.app.surface_needs_snapshot(sid) || !self.grid_cache.contains_key(&sid) {
                // Reuse the surface's previous grid (its cell strings/vector) as
                // the snapshot target so a steady stream of frames doesn't
                // re-allocate every cell.
                let mut g = self.grid_cache.remove(&sid).unwrap_or_else(Grid::empty);
                if let Some(t) = self.app.terminal(sid) {
                    t.snapshot_into(&mut g);
                }
                self.grid_cache.insert(sid, g);
            }
            let Some(grid) = self.grid_cache.get(&sid) else {
                continue;
            };
            // Pane background fills its terminal rect.
            bg_quads.push(rect_quad(*term, sw, sh, grid.default_bg, 1.0));

            // Selection highlight (behind text) for this surface.
            if let Some(sel) = self.selection {
                if sel.surface == sid {
                    for row in 0..grid.size.rows {
                        if let Some((first, last)) = selection_row_span(sel, row, grid.size.cols) {
                            let x = term.x + first as f32 * self.cell_w;
                            let w = (last - first + 1) as f32 * self.cell_w;
                            bg_quads.push(rect_quad(
                                Rect {
                                    x,
                                    y: term.y + row as f32 * self.cell_h,
                                    w,
                                    h: self.cell_h,
                                },
                                sw,
                                sh,
                                self.chrome.accent,
                                0.35,
                            ));
                        }
                    }
                }
            }

            for row in 0..grid.size.rows {
                for col in 0..grid.size.cols {
                    if let Some(c) = grid.cell(col, row) {
                        if c.bg != grid.default_bg {
                            bg_quads.push(rect_quad(
                                cell_rect(*term, col, row, self.cell_w, self.cell_h),
                                sw,
                                sh,
                                c.bg,
                                1.0,
                            ));
                        }
                    }
                }

                // Cheap content key (no span strings); shape only on a miss.
                // Keyed over the inked prefix only, so trailing blanks neither
                // cost shaping nor split otherwise-identical rows in the cache.
                let content_len = row_content_len(grid, row);
                let key = self.row_cache.row_key((0..content_len).map(|col| {
                    match grid.cell(col, row) {
                        Some(c) => (c.text.as_str(), c.fg),
                        None => ("", grid.default_fg),
                    }
                }));
                let top = term.y + row as f32 * self.cell_h;
                let place_key = if self.row_cache.buffer(key).is_some() {
                    // Cache hit: no shaping, just refresh recency.
                    self.row_cache
                        .ensure(key, &mut self.font_system, metrics, term.w, self.cell_h, &[]);
                    key
                } else if shaped < MIN_SHAPES_PER_FRAME || Instant::now() < shape_deadline {
                    let spans = row_spans(grid, row);
                    self.row_cache
                        .ensure(key, &mut self.font_system, metrics, term.w, self.cell_h, &spans);
                    shaped += 1;
                    key
                } else {
                    // Over budget: finish this row a frame later. Meanwhile redraw
                    // the previous (stale) content at this position if it's still
                    // cached, rather than a blank gap.
                    deferred = true;
                    Self::stale_key(
                        &self.prev_rows,
                        &mut self.row_cache,
                        &mut self.font_system,
                        (term.x as i32, top as i32),
                        key,
                        metrics,
                        term.w,
                        self.cell_h,
                    )
                };
                cur_rows.insert((term.x as i32, top as i32), place_key);
                row_placements.push(RowPlacement {
                    key: place_key,
                    left: term.x,
                    top,
                    bounds: TextBounds {
                        left: term.x as i32,
                        top: term.y as i32,
                        right: (term.x + term.w) as i32,
                        bottom: (term.y + term.h) as i32,
                    },
                    color: grid.default_fg,
                });
            }

            if grid.cursor.visible {
                let cur = grid
                    .cell(grid.cursor.col, grid.cursor.row)
                    .map(|c| c.fg)
                    .unwrap_or(grid.default_fg);
                overlay_quads.push(rect_quad(
                    cell_rect(
                        *term,
                        grid.cursor.col,
                        grid.cursor.row,
                        self.cell_w,
                        self.cell_h,
                    ),
                    sw,
                    sh,
                    cur,
                    0.6,
                ));
            }
            if pr.focused && multi_pane {
                push_border(&mut overlay_quads, pr.rect, sw, sh, self.chrome.accent);
            }
        }
        // Remember what each position drew, so next frame's deferred rows can fall
        // back to it instead of a blank gap.
        self.prev_rows = cur_rows;

        // Palette overlay: dim + panel + selection quads (drawn after terminal
        // text), and its text (drawn last, via a second renderer).
        // Sidebar (bg quads before text; its names join the main text pass).
        let sidebar_placements = self.build_sidebar(sw, sh, &mut bg_quads, &mut close_placements);

        let palette_placements = if self.palette.is_some() {
            self.build_palette(sw, sh, &mut overlay_quads)
        } else {
            Vec::new()
        };
        let menu_placements = if self.menu.is_some() {
            self.build_menu(sw, sh, &mut overlay_quads)
        } else {
            Vec::new()
        };

        let n_bg = bg_quads.len() as u32;
        bg_quads.extend_from_slice(&overlay_quads);
        self.upload_quads(&bg_quads);

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
                .shape_until_scroll(&mut self.font_system, false);
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

        let mut text_areas: Vec<TextArea> = row_placements
            .iter()
            .filter_map(|p| {
                self.row_cache.buffer(p.key).map(|buffer| TextArea {
                    buffer,
                    left: p.left,
                    top: p.top,
                    scale: 1.0,
                    bounds: p.bounds,
                    default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                    custom_glyphs: &[],
                })
            })
            .collect();
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
        text_areas.extend(warm_area);

        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                text_areas,
                &mut self.swash_cache,
            )
            .context("text prepare")?;
        // The row buffers are no longer borrowed; trim the cache to its cap and
        // drop grids/geometry for surfaces that have closed.
        self.row_cache.end_frame();
        self.grid_cache.retain(|sid, _| self.app.has_surface(*sid));
        self.surface_geom.retain(|sid, _| self.app.has_surface(*sid));

        // Palette and menu are mutually exclusive; both draw above everything via
        // the overlay text renderer.
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
        };
        if !overlay_areas.is_empty() {
            self.palette_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    overlay_areas,
                    &mut self.swash_cache,
                )
                .context("overlay prepare")?;
        }

        let frame = match self.surface.get_current_texture() {
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
        let view = frame
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
            let total = (n_bg as usize + overlay_quads.len()) as u32;
            if total > n_bg {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, n_bg..total);
            }
            if !palette_placements.is_empty() || !menu_placements.is_empty() {
                self.palette_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .context("overlay render")?;
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        self.atlas.trim();
        // Rows left unshaped by this frame's budget need another frame to finish;
        // keep the surface dirty and paced so the shaping backlog drains.
        self.dirty = deferred;
        if deferred {
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

/// A terminal row's placement this frame, referring to its shaped buffer in the
/// content-keyed [`RowCache`] rather than a positional pool slot.
struct RowPlacement {
    key: u64,
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

/// Group a row's cells into (text, fg-colour) runs of consecutive same colour.
fn row_spans(grid: &Grid, row: u16) -> Vec<(String, [u8; 3])> {
    let mut spans: Vec<(String, [u8; 3])> = Vec::new();
    // Stop at the last inked column: trailing blanks draw nothing in the text
    // pass (their background is a separate quad), so shaping them is wasted work.
    for col in 0..row_content_len(grid, row) {
        let (ch, fg) = match grid.cell(col, row) {
            Some(Cell { text, fg, .. }) if !text.is_empty() => (text.clone(), *fg),
            _ => (" ".to_string(), grid.default_fg),
        };
        match spans.last_mut() {
            Some((s, c)) if *c == fg => s.push_str(&ch),
            _ => spans.push((ch, fg)),
        }
    }
    spans
}

/// Columns up to and including the last cell with visible ink on `row`.
///
/// Trailing blank cells render nothing in the text pass (their background, if
/// any, is drawn as a separate quad), so both the content key and the shaping
/// can stop here. Besides shaping less, this lifts the cache hit rate: rows that
/// differ only in how many trailing blanks they carry now share one shaped row.
fn row_content_len(grid: &Grid, row: u16) -> u16 {
    let mut len = 0u16;
    for col in 0..grid.size.cols {
        let inked = grid
            .cell(col, row)
            .map(|c| c.text.chars().any(|ch| !ch.is_whitespace()))
            .unwrap_or(false);
        if inked {
            len = col + 1;
        }
    }
    len
}

/// A centred square inside `hit` of side `side` (inset a little), for the close
/// button's hover highlight.
fn hover_box(hit: Rect, side: f32) -> Rect {
    let s = (side - 4.0).max(1.0);
    Rect {
        x: hit.x + (hit.w - s) * 0.5,
        y: hit.y + (hit.h - s) * 0.5,
        w: s,
        h: s,
    }
}

/// Whether point `(x, y)` lies inside `r` (half-open on the far edges).
fn rect_contains(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// A cursor/selection direction.
#[derive(Clone, Copy)]
enum ArrowDir {
    Left,
    Right,
    Up,
    Down,
}

/// The next word boundary column on `row` from `col`, moving right (`forward`) or
/// left. A word is a run of non-blank cells.
fn word_col(grid: &Grid, row: u16, col: u16, forward: bool) -> u16 {
    let cols = grid.size.cols;
    if cols == 0 {
        return 0;
    }
    let blank = |c: u16| {
        grid.cell(c, row)
            .map(|cell| cell.text.trim().is_empty())
            .unwrap_or(true)
    };
    if forward {
        let mut c = col;
        // Skip the current word, then the gap, landing on the next word's start.
        while c < cols - 1 && !blank(c) {
            c += 1;
        }
        while c < cols - 1 && blank(c) {
            c += 1;
        }
        c
    } else {
        let mut c = col;
        while c > 0 && blank(c - 1) {
            c -= 1;
        }
        while c > 0 && !blank(c - 1) {
            c -= 1;
        }
        c
    }
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

/// The text of a selection over `grid`, row-major, trailing spaces trimmed per
/// line and rows joined with newlines.
fn selection_text(grid: &Grid, sel: Selection) -> String {
    let ((sc, sr), (ec, er)) = sel.ordered();
    let mut out = String::new();
    for row in sr..=er.min(grid.size.rows.saturating_sub(1)) {
        let first = if row == sr { sc } else { 0 };
        let last = if row == er {
            ec
        } else {
            grid.size.cols.saturating_sub(1)
        };
        let mut line = String::new();
        for col in first..=last.min(grid.size.cols.saturating_sub(1)) {
            match grid.cell(col, row) {
                Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                _ => line.push(' '),
            }
        }
        out.push_str(line.trim_end());
        if row != er {
            out.push('\n');
        }
    }
    out
}

/// The inclusive column span `[first, last]` of a selection on `row`, if the row
/// is within the selection; used to draw the highlight.
fn selection_row_span(sel: Selection, row: u16, cols: u16) -> Option<(u16, u16)> {
    let ((sc, sr), (ec, er)) = sel.ordered();
    if row < sr || row > er || cols == 0 {
        return None;
    }
    let first = if row == sr { sc } else { 0 };
    let last = if row == er { ec } else { cols - 1 };
    Some((first.min(cols - 1), last.min(cols - 1)))
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

/// Absolute pixel rect of cell (col,row) inside pane `pane`.
fn cell_rect(pane: Rect, col: u16, row: u16, cell_w: f32, cell_h: f32) -> Rect {
    Rect {
        x: pane.x + col as f32 * cell_w,
        y: pane.y + row as f32 * cell_h,
        w: cell_w,
        h: cell_h,
    }
}

fn rect_quad(r: Rect, sw: f32, sh: f32, color: [u8; 3], alpha: f32) -> QuadInstance {
    let ndc_x = r.x / sw * 2.0 - 1.0;
    let ndc_y = 1.0 - r.y / sh * 2.0;
    let ndc_w = r.w / sw * 2.0;
    let ndc_h = -(r.h / sh * 2.0);
    QuadInstance {
        pos: [ndc_x, ndc_y],
        size: [ndc_w, ndc_h],
        color: [
            srgb_to_linear(color[0]) as f32,
            srgb_to_linear(color[1]) as f32,
            srgb_to_linear(color[2]) as f32,
            alpha,
        ],
    }
}

/// Push four thin quads forming a border just inside `r`.
fn push_border(out: &mut Vec<QuadInstance>, r: Rect, sw: f32, sh: f32, color: [u8; 3]) {
    let t = BORDER;
    out.push(rect_quad(
        Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: t,
        },
        sw,
        sh,
        color,
        1.0,
    ));
    out.push(rect_quad(
        Rect {
            x: r.x,
            y: r.y + r.h - t,
            w: r.w,
            h: t,
        },
        sw,
        sh,
        color,
        1.0,
    ));
    out.push(rect_quad(
        Rect {
            x: r.x,
            y: r.y,
            w: t,
            h: r.h,
        },
        sw,
        sh,
        color,
        1.0,
    ));
    out.push(rect_quad(
        Rect {
            x: r.x + r.w - t,
            y: r.y,
            w: t,
            h: r.h,
        },
        sw,
        sh,
        color,
        1.0,
    ));
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

fn srgb_to_linear(c: u8) -> f64 {
    let s = c as f64 / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
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

/// Soft-wrap `line` to `cols` columns, returning each visual row as
/// `(text, start_char_index)`. Greedy word wrap (break at the last space that
/// fits, keeping the space on the current row); a word longer than `cols` is
/// hard-broken. A short line yields a single row.
fn wrap_line(line: &str, cols: usize) -> Vec<(String, usize)> {
    let chars: Vec<char> = line.chars().collect();
    if cols == 0 || chars.len() <= cols {
        return vec![(line.to_string(), 0)];
    }
    let mut rows = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let hard_end = (i + cols).min(chars.len());
        let mut brk = hard_end;
        if hard_end < chars.len() {
            // Prefer breaking after the last space within the window.
            if let Some(pos) = (i..hard_end).rev().find(|&k| chars[k] == ' ') {
                if pos + 1 > i {
                    brk = pos + 1;
                }
            }
        }
        rows.push((chars[i..brk].iter().collect(), i));
        i = brk;
    }
    if rows.is_empty() {
        rows.push((String::new(), 0));
    }
    rows
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
    fn selection_text_spans_and_trims() {
        use super::{selection_text, Selection};
        use ghostrealm_core::SurfaceId;
        use ghostrealm_terminal::{Cell, CellAttrs, Cursor, Grid, GridSize};

        // A 4x3 grid: "abc "/"def "/"ghi " (trailing blank column).
        let rows = ["abc ", "def ", "ghi "];
        let mut cells = Vec::new();
        for r in rows {
            for ch in r.chars() {
                let text = if ch == ' ' { String::new() } else { ch.to_string() };
                cells.push(Cell {
                    text,
                    fg: [200, 200, 200],
                    bg: [0, 0, 0],
                    attrs: CellAttrs::default(),
                    wide: false,
                });
            }
        }
        let grid = Grid {
            size: GridSize { cols: 4, rows: 3 },
            cells,
            cursor: Cursor {
                col: 0,
                row: 0,
                visible: false,
            },
            default_fg: [200, 200, 200],
            default_bg: [0, 0, 0],
        };

        // Multi-row selection from (1,0) to (1,2): "bc" + full "def" + "gh".
        let sel = Selection {
            surface: SurfaceId(1),
            anchor: (1, 0),
            head: (1, 2),
        };
        assert_eq!(selection_text(&grid, sel), "bc\ndef\ngh");

        // Anchor/head order doesn't matter.
        let rev = Selection {
            surface: SurfaceId(1),
            anchor: (1, 2),
            head: (1, 0),
        };
        assert_eq!(selection_text(&grid, rev), "bc\ndef\ngh");

        // Single-row selection trims trailing blanks.
        let one = Selection {
            surface: SurfaceId(1),
            anchor: (0, 0),
            head: (3, 0),
        };
        assert_eq!(selection_text(&grid, one), "abc");
    }

    #[test]
    fn word_col_finds_word_boundaries() {
        use super::word_col;
        use ghostrealm_terminal::{Cell, CellAttrs, Cursor, Grid, GridSize};

        // Row 0: "ab cd ef" (cols 0..8).
        let text = "ab cd ef";
        let cells: Vec<Cell> = text
            .chars()
            .map(|ch| Cell {
                text: if ch == ' ' { String::new() } else { ch.to_string() },
                fg: [0, 0, 0],
                bg: [0, 0, 0],
                attrs: CellAttrs::default(),
                wide: false,
            })
            .collect();
        let grid = Grid {
            size: GridSize {
                cols: text.len() as u16,
                rows: 1,
            },
            cells,
            cursor: Cursor {
                col: 0,
                row: 0,
                visible: false,
            },
            default_fg: [0, 0, 0],
            default_bg: [0, 0, 0],
        };

        assert_eq!(word_col(&grid, 0, 0, true), 3, "forward from a -> start of cd");
        assert_eq!(word_col(&grid, 0, 3, true), 6, "forward from cd -> start of ef");
        assert_eq!(word_col(&grid, 0, 4, false), 3, "backward from d -> start of cd");
        assert_eq!(word_col(&grid, 0, 7, false), 6, "backward from f -> start of ef");
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
    fn wrap_line_word_wraps_with_char_start_offsets() {
        // A short line is a single row.
        assert_eq!(super::wrap_line("hello", 10), vec![("hello".to_string(), 0)]);
        // Greedy word wrap keeps the breaking space and reports char start offsets.
        assert_eq!(
            super::wrap_line("the quick brown", 9),
            vec![
                ("the ".to_string(), 0),
                ("quick ".to_string(), 4),
                ("brown".to_string(), 10),
            ]
        );
        // A word longer than the width is hard-broken.
        assert_eq!(
            super::wrap_line("abcdef", 3),
            vec![("abc".to_string(), 0), ("def".to_string(), 3)]
        );
    }

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

        let iters = 30u32;
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
        let per_frame = start.elapsed() / iters;
        println!("full-screen reshape: {per_frame:?}/frame ({cols}x{rows})");
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
