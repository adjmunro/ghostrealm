//! wgpu + winit + glyphon window hosting the multiplexer.
//!
//! Renders the active vtab's split tree: each pane's active surface is drawn in
//! its computed rect (background quads + cursor via an instanced-quad pipeline,
//! foreground text via glyphon), all in one wgpu scene. Cmd-chords run app
//! commands through the registry; other keys go to the focused surface.

use std::sync::Arc;

use anyhow::{Context, Result};
use ghostrealm_core::{Args, Rect, Registry, SurfaceId, TabStatus};
use ghostrealm_terminal::{Cell, Grid, Key, KeyPress, Mods, TerminalBackend};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{Window, WindowId};

use crate::app_state::{build_registry, AppState};

const FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT: f32 = 18.0;
/// Divider gap between split panes, in physical pixels.
const DIVIDER: f32 = 6.0;
/// Focused-pane border thickness, in physical pixels.
const BORDER: f32 = 2.0;
/// Max command-palette results shown at once.
const PALETTE_MAX: usize = 12;
/// Sidebar width in logical pixels (scaled at runtime).
const SIDEBAR_W: f32 = 190.0;

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
    };
    event_loop.run_app(&mut app).context("run app")?;
    Ok(())
}

struct App {
    state: Option<State>,
    command_line: Option<String>,
    proxy: EventLoopProxy<UserEvent>,
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

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: UserEvent) {
        if let Some(state) = &mut self.state {
            state.mark_dirty();
            state.window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                state.on_click();
                state.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    state.on_key(&event);
                    state.mark_dirty();
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
    /// Pool of text buffers, one per visible grid row across all panes; reused
    /// across frames. `prev_row_hash[i]` is the last shaped content of pool i.
    row_buffers: Vec<Buffer>,
    prev_row_hash: Vec<Option<u64>>,

    quad_pipeline: wgpu::RenderPipeline,
    quad_buffer: wgpu::Buffer,
    quad_capacity: u64,

    /// Separate text renderer for the palette so its text draws above the panel
    /// (terminal text and palette text can't share one pass with a quad between).
    palette_renderer: TextRenderer,
    palette_buffers: Vec<Buffer>,
    palette: Option<Palette>,
    /// Text buffers for the sidebar's vtab names (drawn in the main text pass).
    sidebar_buffers: Vec<Buffer>,
    /// Last cursor position in physical pixels, for click hit-testing.
    cursor: (f32, f32),

    app: AppState,
    registry: Registry<AppState>,
    mods: Mods,
    scale: f32,
    cell_w: f32,
    cell_h: f32,
    dirty: bool,
    /// Last frame's pane layout (surface, rect, focused). A change triggers a
    /// terminal resize pass and invalidates the row cache.
    prev_layout: Vec<(SurfaceId, Rect, bool)>,
}

/// Command-palette UI state.
struct Palette {
    query: String,
    selected: usize,
}

impl State {
    async fn new(
        window: Arc<Window>,
        command_line: Option<String>,
        proxy: EventLoopProxy<UserEvent>,
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

        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let palette_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        let (cell_w, cell_h) = measure_cell(&mut font_system, scale);

        // App state: terminals wake the event loop through the proxy.
        let waker: Arc<dyn Fn() + Send + Sync> = {
            let proxy = proxy.clone();
            Arc::new(move || {
                let _ = proxy.send_event(UserEvent::PtyOutput);
            })
        };
        let mut app = AppState::new();
        if let Some(line) = command_line {
            app = app.with_shell_line(line);
        }
        app = app.with_waker(waker);
        app.new_vtab().context("open initial tab")?;
        let registry = build_registry();

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
            row_buffers: Vec::new(),
            prev_row_hash: Vec::new(),
            palette_renderer,
            palette_buffers: Vec::new(),
            palette: None,
            sidebar_buffers: Vec::new(),
            cursor: (0.0, 0.0),
            quad_pipeline,
            quad_buffer,
            quad_capacity,
            app,
            registry,
            mods: Mods::default(),
            scale,
            cell_w,
            cell_h,
            dirty: true,
            prev_layout: Vec::new(),
        })
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        let (cw, ch) = measure_cell(&mut self.font_system, scale);
        self.cell_w = cw;
        self.cell_h = ch;
        self.dirty = true;
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

    /// The active vtab's panes as (active surface, pixel rect, focused).
    fn active_layout(&self, workspace: Rect) -> Vec<(SurfaceId, Rect, bool)> {
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
                let sid = pane.active_surface()?.id;
                Some((sid, rect, pid == vtab.focused_pane))
            })
            .collect()
    }

    fn sidebar_width(&self) -> f32 {
        SIDEBAR_W * self.scale
    }

    /// Handle a left click: switch vtab (sidebar) or focus a pane (workspace).
    fn on_click(&mut self) {
        if self.palette.is_some() {
            return;
        }
        let (x, y) = self.cursor;
        let sidebar_w = self.sidebar_width();
        if x < sidebar_w {
            let row_h = self.cell_h + 8.0 * self.scale;
            let pad = 8.0 * self.scale;
            if y < pad {
                return;
            }
            let idx = ((y - pad) / row_h).floor() as usize;
            if let Some(v) = self.app.tree.vtabs().get(idx) {
                let id = v.id;
                self.app.tree.focus_vtab(id);
                self.dirty = true;
            }
            return;
        }
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let workspace = Rect {
            x: sidebar_w,
            y: 0.0,
            w: sw - sidebar_w,
            h: sh,
        };
        if let Some(vt) = self.app.tree.active_vtab() {
            let hit = self.app.tree.vtab(vt).and_then(|vtab| {
                vtab.layout(workspace, DIVIDER)
                    .into_iter()
                    .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
                    .map(|(pid, _)| pid)
            });
            if let Some(pid) = hit {
                if let Some(vtab) = self.app.tree.vtab_mut(vt) {
                    vtab.focused_pane = pid;
                    self.dirty = true;
                }
            }
        }
    }

    /// Push sidebar quads and shape vtab-name text; returns placements into
    /// `sidebar_buffers`.
    fn build_sidebar(&mut self, sw: f32, sh: f32, quads: &mut Vec<QuadInstance>) -> Vec<Placement> {
        let bar_w = self.sidebar_width();
        let row_h = self.cell_h + 8.0 * self.scale;
        let pad = 8.0 * self.scale;
        let metrics = Metrics::new(FONT_SIZE * self.scale, LINE_HEIGHT * self.scale);
        let active = self.app.tree.active_vtab();
        let vtabs: Vec<(String, TabStatus, bool)> = self
            .app
            .tree
            .vtabs()
            .iter()
            .map(|v| (v.name.clone(), v.status, Some(v.id) == active))
            .collect();

        quads.push(rect_quad(
            Rect {
                x: 0.0,
                y: 0.0,
                w: bar_w,
                h: sh,
            },
            sw,
            sh,
            [24, 24, 30],
            1.0,
        ));

        while self.sidebar_buffers.len() < vtabs.len() {
            let b = Buffer::new(&mut self.font_system, metrics);
            self.sidebar_buffers.push(b);
        }

        let dot = 6.0 * self.scale;
        let text_x = pad + dot + 6.0 * self.scale;
        let mut placements = Vec::with_capacity(vtabs.len());
        for (i, (name, status, is_active)) in vtabs.iter().enumerate() {
            let y = pad + i as f32 * row_h;
            if *is_active {
                quads.push(rect_quad(
                    Rect {
                        x: 0.0,
                        y,
                        w: bar_w,
                        h: row_h,
                    },
                    sw,
                    sh,
                    [40, 44, 60],
                    1.0,
                ));
            }
            quads.push(rect_quad(
                Rect {
                    x: pad,
                    y: y + (row_h - dot) * 0.5,
                    w: dot,
                    h: dot,
                },
                sw,
                sh,
                status_color(*status),
                1.0,
            ));
            let buf = &mut self.sidebar_buffers[i];
            buf.set_metrics(metrics);
            buf.set_size(Some((bar_w - text_x - pad).max(1.0)), Some(self.cell_h));
            buf.set_rich_text(
                std::iter::once((name.as_str(), attrs_for([220, 220, 230]))),
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            placements.push(Placement {
                idx: i,
                left: text_x,
                top: y + (row_h - self.cell_h) * 0.5,
                bounds: TextBounds {
                    left: 0,
                    top: y as i32,
                    right: bar_w as i32,
                    bottom: (y + row_h) as i32,
                },
                color: [220, 220, 230],
            });
        }
        placements
    }

    fn on_key(&mut self, event: &winit::event::KeyEvent) {
        // The palette, when open, owns the keyboard.
        if self.palette.is_some() {
            self.palette_key(event);
            return;
        }
        // Cmd-chords drive the app via the registry; everything else goes to the
        // focused terminal. (Cmd is reserved so app shortcuts never reach a shell.)
        if self.mods.super_ {
            let ch = match &event.logical_key {
                WKey::Character(s) => s.chars().next(),
                _ => None,
            };
            if let Some(c) = ch {
                if c.to_ascii_lowercase() == 'k' {
                    self.palette = Some(Palette {
                        query: String::new(),
                        selected: 0,
                    });
                    self.dirty = true;
                    return;
                }
                let id = match (c.to_ascii_lowercase(), self.mods.shift) {
                    ('t', _) => Some("tab.new"),
                    ('d', false) => Some("split.leftright"),
                    ('d', true) => Some("split.topbottom"),
                    ('w', _) => Some("pane.close"),
                    (']', _) => Some("pane.focus_next"),
                    ('n', _) => Some("surface.new"),
                    _ => None,
                };
                if let Some(id) = id {
                    let _ = self.registry.execute(id, &Args::new(), &mut self.app);
                    self.dirty = true;
                }
            }
            return;
        }

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

    fn palette_key(&mut self, event: &winit::event::KeyEvent) {
        let (query, selected) = match &self.palette {
            Some(p) => (p.query.clone(), p.selected),
            None => return,
        };
        match &event.logical_key {
            WKey::Named(NamedKey::Escape) => self.palette = None,
            WKey::Named(NamedKey::Enter) => {
                let id = self
                    .registry
                    .search(&query, PALETTE_MAX)
                    .get(selected)
                    .map(|h| h.meta.id.to_string());
                self.palette = None;
                if let Some(id) = id {
                    let _ = self.registry.execute(&id, &Args::new(), &mut self.app);
                }
            }
            WKey::Named(NamedKey::Backspace) => {
                if let Some(p) = self.palette.as_mut() {
                    p.query.pop();
                    p.selected = 0;
                }
            }
            WKey::Named(NamedKey::ArrowDown) => {
                let n = self.registry.search(&query, PALETTE_MAX).len();
                if let Some(p) = self.palette.as_mut() {
                    if n > 0 {
                        p.selected = (selected + 1) % n;
                    }
                }
            }
            WKey::Named(NamedKey::ArrowUp) => {
                let n = self.registry.search(&query, PALETTE_MAX).len();
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
                }
            }
            WKey::Character(s) => {
                if let Some(c) = s.chars().next() {
                    if let Some(p) = self.palette.as_mut() {
                        p.query.push(c);
                        p.selected = 0;
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
        let (query, selected) = match &self.palette {
            Some(p) => (p.query.clone(), p.selected),
            None => return Vec::new(),
        };
        let hits: Vec<String> = self
            .registry
            .search(&query, PALETTE_MAX)
            .into_iter()
            .map(|h| h.meta.title.to_string())
            .collect();

        let metrics = Metrics::new(FONT_SIZE * self.scale, LINE_HEIGHT * self.scale);
        let line_h = self.cell_h;
        let pad = 10.0 * self.scale;
        let panel_w = (sw * 0.6).clamp(240.0, 720.0 * self.scale);
        let panel_x = ((sw - panel_w) / 2.0).max(0.0);
        let panel_y = sh * 0.12;
        let line_count = 1 + hits.len().max(1); // query line + results (or "none")
        let panel_h = line_count as f32 * line_h + pad * 2.0;

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
        if !hits.is_empty() {
            let sel = selected.min(hits.len() - 1);
            let sel_y = panel_y + pad + (1 + sel) as f32 * line_h;
            quads.push(rect_quad(
                Rect {
                    x: panel_x + pad * 0.5,
                    y: sel_y,
                    w: panel_w - pad,
                    h: line_h,
                },
                sw,
                sh,
                [60, 90, 150],
                0.9,
            ));
        }

        let mut lines: Vec<(String, [u8; 3])> = Vec::new();
        lines.push((format!("\u{203a} {}", query), [235, 235, 245]));
        if hits.is_empty() {
            lines.push(("  (no matching commands)".to_string(), [150, 150, 160]));
        } else {
            for (i, title) in hits.iter().enumerate() {
                let prefix = if i == selected.min(hits.len() - 1) {
                    "\u{25b8} "
                } else {
                    "  "
                };
                lines.push((format!("{prefix}{title}"), [235, 235, 245]));
            }
        }

        while self.palette_buffers.len() < lines.len() {
            let b = Buffer::new(&mut self.font_system, metrics);
            self.palette_buffers.push(b);
        }
        let text_x = panel_x + pad;
        let text_w = (panel_w - pad * 2.0).max(1.0);
        let mut placements = Vec::with_capacity(lines.len());
        for (i, (text, color)) in lines.iter().enumerate() {
            let buf = &mut self.palette_buffers[i];
            buf.set_metrics(metrics);
            buf.set_size(Some(text_w), Some(line_h));
            buf.set_rich_text(
                std::iter::once((text.as_str(), attrs_for(*color))),
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
                color: *color,
            });
        }
        placements
    }

    fn render(&mut self) -> Result<()> {
        if self.app.pump_all() {
            self.dirty = true;
        }
        if !self.dirty {
            return Ok(());
        }

        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        let sidebar_w = self.sidebar_width();
        let workspace = Rect {
            x: sidebar_w,
            y: 0.0,
            w: (sw - sidebar_w).max(1.0),
            h: sh,
        };
        let layout = self.active_layout(workspace);

        // Layout changed → resize each pane's terminal and invalidate row cache.
        if layout != self.prev_layout {
            let (cw, ch) = (self.cell_w.round() as u32, self.cell_h.round() as u32);
            for (sid, rect, _) in &layout {
                let cols = ((rect.w / self.cell_w).floor() as u16).max(1);
                let rows = ((rect.h / self.cell_h).floor() as u16).max(1);
                self.app.resize_surface(*sid, cols, rows, cw, ch);
            }
            self.prev_row_hash.clear();
            self.prev_layout = layout.clone();
        }

        let metrics = Metrics::new(FONT_SIZE * self.scale, LINE_HEIGHT * self.scale);
        let mut bg_quads: Vec<QuadInstance> = Vec::new();
        let mut overlay_quads: Vec<QuadInstance> = Vec::new();
        let mut placements: Vec<Placement> = Vec::new();
        let mut pool_idx = 0usize;

        for (sid, rect, focused) in &layout {
            let grid = match self.app.terminal(*sid) {
                Some(t) => t.snapshot(),
                None => continue,
            };
            // Pane background fills its rect (dividers show the clear colour).
            bg_quads.push(rect_quad(*rect, sw, sh, grid.default_bg, 1.0));

            for row in 0..grid.size.rows {
                for col in 0..grid.size.cols {
                    if let Some(c) = grid.cell(col, row) {
                        if c.bg != grid.default_bg {
                            bg_quads.push(rect_quad(
                                cell_rect(*rect, col, row, self.cell_w, self.cell_h),
                                sw,
                                sh,
                                c.bg,
                                1.0,
                            ));
                        }
                    }
                }

                let spans = row_spans(&grid, row);
                let hash = hash_spans(&spans);
                if pool_idx >= self.row_buffers.len() {
                    self.row_buffers
                        .push(Buffer::new(&mut self.font_system, metrics));
                    self.prev_row_hash.push(None);
                }
                let unchanged = self.prev_row_hash.get(pool_idx).copied().flatten() == Some(hash);
                let buf = &mut self.row_buffers[pool_idx];
                buf.set_size(Some(rect.w.max(1.0)), Some(self.cell_h));
                if !unchanged {
                    buf.set_metrics(metrics);
                    buf.set_rich_text(
                        spans.iter().map(|(t, c)| (t.as_str(), attrs_for(*c))),
                        &Attrs::new().family(Family::Monospace),
                        Shaping::Advanced,
                        None,
                    );
                    buf.shape_until_scroll(&mut self.font_system, false);
                    self.prev_row_hash[pool_idx] = Some(hash);
                }
                placements.push(Placement {
                    idx: pool_idx,
                    left: rect.x,
                    top: rect.y + row as f32 * self.cell_h,
                    bounds: TextBounds {
                        left: rect.x as i32,
                        top: rect.y as i32,
                        right: (rect.x + rect.w) as i32,
                        bottom: (rect.y + rect.h) as i32,
                    },
                    color: grid.default_fg,
                });
                pool_idx += 1;
            }

            if grid.cursor.visible {
                let cur = grid
                    .cell(grid.cursor.col, grid.cursor.row)
                    .map(|c| c.fg)
                    .unwrap_or(grid.default_fg);
                overlay_quads.push(rect_quad(
                    cell_rect(
                        *rect,
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
            if *focused && layout.len() > 1 {
                push_border(&mut overlay_quads, *rect, sw, sh, [90, 140, 220]);
            }
        }

        // Palette overlay: dim + panel + selection quads (drawn after terminal
        // text), and its text (drawn last, via a second renderer).
        // Sidebar (bg quads before text; its names join the main text pass).
        let sidebar_placements = self.build_sidebar(sw, sh, &mut bg_quads);

        let palette_placements = if self.palette.is_some() {
            self.build_palette(sw, sh, &mut overlay_quads)
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

        let mut text_areas: Vec<TextArea> = placements
            .iter()
            .map(|p| TextArea {
                buffer: &self.row_buffers[p.idx],
                left: p.left,
                top: p.top,
                scale: 1.0,
                bounds: p.bounds,
                default_color: Color::rgb(p.color[0], p.color[1], p.color[2]),
                custom_glyphs: &[],
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

        if !palette_placements.is_empty() {
            let areas: Vec<TextArea> = palette_placements
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
                .collect();
            self.palette_renderer
                .prepare(
                    &self.device,
                    &self.queue,
                    &mut self.font_system,
                    &mut self.atlas,
                    &self.viewport,
                    areas,
                    &mut self.swash_cache,
                )
                .context("palette prepare")?;
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
                            r: srgb_to_linear(20),
                            g: srgb_to_linear(20),
                            b: srgb_to_linear(24),
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
            if !palette_placements.is_empty() {
                self.palette_renderer
                    .render(&self.atlas, &self.viewport, &mut pass)
                    .context("palette render")?;
            }
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        self.atlas.trim();
        self.dirty = false;
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

/// A shaped row buffer's placement for this frame.
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

/// Group a row's cells into (text, fg-colour) runs of consecutive same colour.
fn row_spans(grid: &Grid, row: u16) -> Vec<(String, [u8; 3])> {
    let mut spans: Vec<(String, [u8; 3])> = Vec::new();
    for col in 0..grid.size.cols {
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

fn hash_spans(spans: &[(String, [u8; 3])]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    spans.hash(&mut h);
    h.finish()
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

fn measure_cell(font_system: &mut FontSystem, scale: f32) -> (f32, f32) {
    let metrics = Metrics::new(FONT_SIZE * scale, LINE_HEIGHT * scale);
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
        .unwrap_or(FONT_SIZE * scale * 0.6);
    (w, LINE_HEIGHT * scale)
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
    use super::{build_quad_pipeline, measure_cell, QuadInstance, FONT_SIZE, LINE_HEIGHT};
    use glyphon::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};
    use std::time::{Duration, Instant};

    #[test]
    fn full_screen_shaping_cost() {
        let mut font_system = FontSystem::new();
        let (_cw, _ch) = measure_cell(&mut font_system, 1.0);
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
