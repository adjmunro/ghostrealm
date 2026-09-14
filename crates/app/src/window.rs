//! wgpu + winit + glyphon window that hosts a single terminal surface.
//!
//! Phase 1 spike: prove the render/input/resize loop against a real PTY. The
//! terminal grid is drawn in our own wgpu scene (background quads + cursor via a
//! small instanced-quad pipeline, foreground text via glyphon), which is the
//! unified-compositing model the whole app will use.

use std::sync::Arc;

use anyhow::{Context, Result};
use ghostrealm_terminal::{Cell, Grid, Key, KeyPress, Mods, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key as WKey, NamedKey};
use winit::window::{Window, WindowId};

const FONT_SIZE: f32 = 15.0;
const LINE_HEIGHT: f32 = 18.0;

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
    row_buffers: Vec<Buffer>,

    quad_pipeline: wgpu::RenderPipeline,
    quad_buffer: wgpu::Buffer,
    quad_capacity: u64,

    terminal: GhosttyTerminal,
    mods: Mods,
    scale: f32,
    cell_w: f32,
    cell_h: f32,
    cols: u16,
    rows: u16,
    /// Whether the grid changed since the last draw. When false, a redraw skips
    /// the expensive snapshot + text reshape entirely.
    dirty: bool,
    /// Per-row hash of the last shaped content, so only changed rows re-shape.
    prev_row_hash: Vec<Option<u64>>,
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

        let (cell_w, cell_h) = measure_cell(&mut font_system, scale);
        let cols = ((config.width as f32 / cell_w).floor() as u16).max(1);
        let rows = ((config.height as f32 / cell_h).floor() as u16).max(1);

        let cmd = match command_line {
            Some(line) => {
                let mut c = CommandBuilder::new("/bin/sh");
                c.arg("-c");
                c.arg(line);
                c
            }
            None => CommandBuilder::new_default_prog(),
        };
        let waker: ghostrealm_terminal_ghostty::PtyWaker = {
            let proxy = proxy.clone();
            Box::new(move || {
                let _ = proxy.send_event(UserEvent::PtyOutput);
            })
        };
        let terminal = GhosttyTerminal::spawn_with_waker(
            cols,
            rows,
            cell_w.round() as u32,
            cell_h.round() as u32,
            Some(cmd),
            Some(waker),
        )
        .context("spawn terminal")?;

        let quad_pipeline = build_quad_pipeline(&device, format);
        let quad_capacity = 1024;
        let quad_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-instances"),
            size: quad_capacity * std::mem::size_of::<QuadInstance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut state = Self {
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
            quad_pipeline,
            quad_buffer,
            quad_capacity,
            terminal,
            mods: Mods::default(),
            scale,
            cell_w,
            cell_h,
            cols,
            rows,
            dirty: true,
            prev_row_hash: Vec::new(),
        };
        state.rebuild_row_buffers();
        Ok(state)
    }

    fn rebuild_row_buffers(&mut self) {
        let metrics = Metrics::new(FONT_SIZE * self.scale, LINE_HEIGHT * self.scale);
        let width = self.config.width as f32;
        let cell_h = self.cell_h;
        self.row_buffers = (0..self.rows)
            .map(|_| {
                let mut b = Buffer::new(&mut self.font_system, metrics);
                b.set_size(Some(width), Some(cell_h));
                b
            })
            .collect();
        // Fresh buffers have no shaped content; force every row to reshape once.
        self.prev_row_hash = vec![None; self.rows as usize];
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        let (cw, ch) = measure_cell(&mut self.font_system, scale);
        self.cell_w = cw;
        self.cell_h = ch;
        self.recompute_grid();
        self.dirty = true;
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        self.recompute_grid();
        self.dirty = true;
    }

    fn recompute_grid(&mut self) {
        let cols = ((self.config.width as f32 / self.cell_w).floor() as u16).max(1);
        let rows = ((self.config.height as f32 / self.cell_h).floor() as u16).max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.terminal.resize(
            cols,
            rows,
            self.cell_w.round() as u32,
            self.cell_h.round() as u32,
        );
        self.rebuild_row_buffers();
    }

    fn on_key(&mut self, event: &winit::event::KeyEvent) {
        // Reserve Super (Cmd) chords for future app keybindings; don't send.
        if self.mods.super_ {
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
        self.terminal.send_key(&KeyPress {
            key,
            mods: self.mods,
            text,
        });
    }

    fn render(&mut self) -> Result<()> {
        if self.terminal.pump() {
            self.dirty = true;
        }
        // Nothing changed since the last draw: skip the snapshot + reshape.
        if !self.dirty {
            return Ok(());
        }
        let grid = self.terminal.snapshot();

        // Foreground text: one rich-text buffer per row, coloured per cell run.
        let metrics = Metrics::new(FONT_SIZE * self.scale, LINE_HEIGHT * self.scale);
        for row in 0..grid.size.rows as usize {
            if row >= self.row_buffers.len() {
                break;
            }
            let spans = row_spans(&grid, row as u16);
            let hash = hash_spans(&spans);
            // Only re-shape rows whose content actually changed.
            if self.prev_row_hash.get(row).copied().flatten() == Some(hash) {
                continue;
            }
            let buf = &mut self.row_buffers[row];
            buf.set_metrics(metrics);
            buf.set_rich_text(
                spans.iter().map(|(t, c)| (t.as_str(), attrs_for(*c))),
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            buf.shape_until_scroll(&mut self.font_system, false);
            if let Some(slot) = self.prev_row_hash.get_mut(row) {
                *slot = Some(hash);
            }
        }

        // Background + cursor quads.
        let mut quads: Vec<QuadInstance> = Vec::new();
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        for row in 0..grid.size.rows {
            for col in 0..grid.size.cols {
                if let Some(cell) = grid.cell(col, row) {
                    if cell.bg != grid.default_bg {
                        quads.push(cell_quad(
                            col,
                            row,
                            self.cell_w,
                            self.cell_h,
                            sw,
                            sh,
                            cell.bg,
                            1.0,
                        ));
                    }
                }
            }
        }
        let n_bg = quads.len() as u32;
        if grid.cursor.visible {
            let cur = grid
                .cell(grid.cursor.col, grid.cursor.row)
                .map(|c| c.fg)
                .unwrap_or(grid.default_fg);
            quads.push(cell_quad(
                grid.cursor.col,
                grid.cursor.row,
                self.cell_w,
                self.cell_h,
                sw,
                sh,
                cur,
                0.6,
            ));
        }
        self.upload_quads(&quads);

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        let text_areas: Vec<TextArea> = self
            .row_buffers
            .iter()
            .take(grid.size.rows as usize)
            .enumerate()
            .map(|(row, buf)| TextArea {
                buffer: buf,
                left: 0.0,
                top: row as f32 * self.cell_h,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: self.config.width as i32,
                    bottom: self.config.height as i32,
                },
                default_color: Color::rgb(
                    grid.default_fg[0],
                    grid.default_fg[1],
                    grid.default_fg[2],
                ),
                custom_glyphs: &[],
            })
            .collect();

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
            other => {
                return Err(anyhow::anyhow!("surface acquire failed: {other:?}"));
            }
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
            let bg = grid.default_bg;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
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
            if grid.cursor.visible {
                pass.set_pipeline(&self.quad_pipeline);
                pass.set_vertex_buffer(0, self.quad_buffer.slice(..));
                pass.draw(0..4, n_bg..n_bg + 1);
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

#[allow(clippy::too_many_arguments)]
fn cell_quad(
    col: u16,
    row: u16,
    cell_w: f32,
    cell_h: f32,
    sw: f32,
    sh: f32,
    color: [u8; 3],
    alpha: f32,
) -> QuadInstance {
    let px = col as f32 * cell_w;
    let py = row as f32 * cell_h;
    let ndc_x = px / sw * 2.0 - 1.0;
    let ndc_y = 1.0 - py / sh * 2.0;
    let ndc_w = cell_w / sw * 2.0;
    let ndc_h = -(cell_h / sh * 2.0);
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

    /// Cost of re-shaping a full 80x24 screen — the work the old render loop did
    /// on every frame. Guards the "only reshape on change" optimisation.
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
        // At 60fps a frame budget is ~16ms; reshaping every frame near/over that
        // is the lag. This is why we only reshape on change now.
        assert!(
            per_frame < Duration::from_millis(50),
            "full-screen reshape unexpectedly slow: {per_frame:?}"
        );
    }

    /// Headless proof the quad pipeline produces pixels: render a full-target red
    /// quad to an offscreen texture and read the centre pixel back. Needs a GPU
    /// adapter (Metal/Vulkan/GL); it is the display-free stand-in for eyeballing
    /// the window.
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
        let dim = 64u32; // 64*4 = 256-byte rows, already copy-aligned.
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
            "expected an opaque red centre pixel from the quad pipeline, got {px:?}.\n\
             Next steps: verify QuadInstance NDC mapping (pos/size), the triangle-strip corner \
             order in QUAD_WGSL, and the bytemuck instance upload."
        );
    }
}
