//! Draws the images views place with [`PaintCx::image`]: one GPU texture per
//! `(surface, id)`, re-uploaded only when the image's version changes, stretched
//! over its rect and clipped to its clip rect. Base-layer images draw after the
//! fills, overlay images after the top quads.
//!
//! [`PaintCx::image`]: crate::plugin::PaintCx::image

use std::collections::HashMap;
use std::ops::Range;

use ghostrealm_core::{Rect, SurfaceId};

use crate::plugin::ImageItem;

/// Image pixels are sRGB-encoded and premultiplied in linear light (see
/// [`ImageSource`](crate::plugin::ImageSource)): sampled as sRGB they read as
/// linear light, which blends correctly and which the target encodes back. Quads
/// and text also write linear light whatever the target, so images match them on
/// a non-sRGB target too.
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

struct Texture {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
    /// Mip levels, full size included.
    levels: u32,
    version: u64,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageQuad {
    /// Top-left in NDC.
    pos: [f32; 2],
    /// Width/height in NDC (height negative to grow downward).
    size: [f32; 2],
    /// 1.0 ignores the image's alpha.
    opaque: f32,
}

/// A prepared draw: texture key, scissor rect, and whether it draws 1:1 (nearest
/// sampling is exact) or scaled (filtered through the mips).
struct Draw {
    key: (SurfaceId, u32),
    scissor: (u32, u32, u32, u32),
    scaled: bool,
}

pub struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    /// Group 1: nearest sampling for 1:1 draws, trilinear for scaled ones.
    nearest: wgpu::BindGroup,
    smooth: wgpu::BindGroup,
    textures: HashMap<(SurfaceId, u32), Texture>,
    instances: wgpu::Buffer,
    capacity: u64,
    /// This frame's draws in instance order: base layer, then overlay.
    draws: Vec<Draw>,
    n_base: usize,
}

impl ImageRenderer {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                },
                count: None,
            }],
        });
        let sampler_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image-sampler"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            }],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image"),
            bind_group_layouts: &[Some(&layout), Some(&sampler_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageQuad>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    // Sources are premultiplied (see `ImageSource`).
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
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
        });
        let sampler_group = |sampler: &wgpu::Sampler| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("image-sampler"),
                layout: &sampler_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(sampler),
                }],
            })
        };
        let nearest = sampler_group(&device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image-nearest"),
            ..Default::default()
        }));
        let smooth = sampler_group(&device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image-smooth"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        }));
        let capacity = 8;
        ImageRenderer {
            pipeline,
            layout,
            nearest,
            smooth,
            textures: HashMap::new(),
            instances: instance_buffer(device, capacity),
            capacity,
            draws: Vec::new(),
            n_base: 0,
        }
    }

    /// Upload changed images and lay out this frame's draws on a `sw`x`sh`
    /// target: `base` for [`draw_base`](Self::draw_base), `overlay` for
    /// [`draw_overlay`](Self::draw_overlay). Textures of images not placed this
    /// frame are dropped.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        base: &[ImageItem],
        overlay: &[ImageItem],
        sw: u32,
        sh: u32,
    ) {
        self.draws.clear();
        self.n_base = 0;
        let mut quads: Vec<ImageQuad> = Vec::new();
        for (i, img) in base.iter().chain(overlay).enumerate() {
            if i == base.len() {
                // The overlay's draws start here.
                self.n_base = self.draws.len();
            }
            let key = (img.owner, img.id);
            let (w, h) = img.source.size();
            if w == 0 || h == 0 {
                continue;
            }
            let levels = 1 + img.source.mips().len() as u32;
            let current = self.textures.get(&key).is_some_and(|t| {
                t.version == img.version && t.width == w && t.height == h && t.levels == levels
            });
            if !current {
                let mut data = Vec::new();
                if let Some(pixels) = img.source.bgra() {
                    data.push((w, h, pixels));
                    data.extend(img.source.mips().iter().map(|(w, h, px)| (*w, *h, px.as_slice())));
                }
                let whole = !data.is_empty()
                    && data.iter().all(|(w, h, px)| px.len() >= *w as usize * *h as usize * 4);
                if whole {
                    self.upload(device, queue, key, img.version, &data);
                } else if !self.textures.contains_key(&key) {
                    continue;
                }
                // Pixels gone (a web page's overwritten frame): draw the last ones.
            }
            let Some(scissor) = scissor(img.clip, sw, sh) else {
                continue;
            };
            let r = img.rect;
            let (fw, fh) = (sw as f32, sh as f32);
            quads.push(ImageQuad {
                pos: [r.x / fw * 2.0 - 1.0, 1.0 - r.y / fh * 2.0],
                size: [r.w / fw * 2.0, -(r.h / fh * 2.0)],
                opaque: if img.source.opaque() { 1.0 } else { 0.0 },
            });
            self.draws.push(Draw {
                key,
                scissor,
                scaled: r.w != w as f32 || r.h != h as f32,
            });
        }
        if overlay.is_empty() {
            self.n_base = self.draws.len();
        }
        let placed: Vec<(SurfaceId, u32)> =
            base.iter().chain(overlay).map(|i| (i.owner, i.id)).collect();
        self.textures.retain(|k, _| placed.contains(k));
        if quads.is_empty() {
            return;
        }
        if quads.len() as u64 > self.capacity {
            self.capacity = (quads.len() as u64).next_power_of_two();
            self.instances = instance_buffer(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&quads));
    }

    /// Draw the base layer's images (after the background quads).
    pub fn draw_base(&self, pass: &mut wgpu::RenderPass<'_>, sw: u32, sh: u32) {
        self.draw(pass, 0..self.n_base, sw, sh);
    }

    /// Draw the overlay layer's images (after the top quads).
    pub fn draw_overlay(&self, pass: &mut wgpu::RenderPass<'_>, sw: u32, sh: u32) {
        self.draw(pass, self.n_base..self.draws.len(), sw, sh);
    }

    /// Draw `range` of the prepared images; restores a full-target scissor
    /// afterwards.
    fn draw(&self, pass: &mut wgpu::RenderPass<'_>, range: Range<usize>, sw: u32, sh: u32) {
        if range.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        for i in range {
            let d = &self.draws[i];
            let Some(t) = self.textures.get(&d.key) else {
                continue;
            };
            let (x, y, w, h) = d.scissor;
            pass.set_bind_group(0, &t.bind_group, &[]);
            pass.set_bind_group(1, if d.scaled { &self.smooth } else { &self.nearest }, &[]);
            pass.set_scissor_rect(x, y, w, h);
            pass.draw(0..4, i as u32..i as u32 + 1);
        }
        pass.set_scissor_rect(0, 0, sw, sh);
    }

    /// Write `levels` (full size first, then each mip) into `key`'s texture,
    /// replacing it when its size or level count differs.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        key: (SurfaceId, u32),
        version: u64,
        levels: &[(u32, u32, &[u8])],
    ) {
        let (width, height, _) = levels[0];
        let count = levels.len() as u32;
        let fits = self
            .textures
            .get(&key)
            .is_some_and(|t| t.width == width && t.height == height && t.levels == count);
        if !fits {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("image"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: count,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEXTURE_FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("image"),
                layout: &self.layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                }],
            });
            self.textures.insert(
                key,
                Texture {
                    texture,
                    bind_group,
                    width,
                    height,
                    levels: count,
                    version,
                },
            );
        }
        let Some(t) = self.textures.get_mut(&key) else {
            return;
        };
        for (level, (w, h, pixels)) in levels.iter().enumerate() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &t.texture,
                    mip_level: level as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels[..*w as usize * *h as usize * 4],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * w),
                    rows_per_image: Some(*h),
                },
                wgpu::Extent3d {
                    width: *w,
                    height: *h,
                    depth_or_array_layers: 1,
                },
            );
        }
        t.version = version;
    }
}

/// `r` as a scissor rect within a `sw`x`sh` target, or `None` if nothing of it
/// is on the target.
fn scissor(r: Rect, sw: u32, sh: u32) -> Option<(u32, u32, u32, u32)> {
    let x0 = r.x.max(0.0) as u32;
    let y0 = r.y.max(0.0) as u32;
    let x1 = ((r.x + r.w).max(0.0) as u32).min(sw);
    let y1 = ((r.y + r.h).max(0.0) as u32).min(sh);
    (x1 > x0 && y1 > y0).then(|| (x0, y0, x1 - x0, y1 - y0))
}

fn instance_buffer(device: &wgpu::Device, capacity: u64) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("image-instances"),
        size: capacity * std::mem::size_of::<ImageQuad>() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

const IMAGE_WGSL: &str = r#"
struct Inst {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) opaque: f32,
};
struct VOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) opaque: f32,
};
@group(0) @binding(0) var img: texture_2d<f32>;
@group(1) @binding(0) var samp: sampler;
@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: Inst) -> VOut {
    var corners = array<vec2<f32>, 4>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
    let c = corners[vi];
    var o: VOut;
    o.clip = vec4<f32>(inst.pos + c * inst.size, 0.0, 1.0);
    o.uv = c;
    o.opaque = inst.opaque;
    return o;
}
@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(img, samp, in.uv);
    return vec4<f32>(c.rgb, max(c.a, in.opaque));
}
"#;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::plugin::{Image, ImageSource};

    struct Solid(Vec<u8>, u32);

    impl ImageSource for Solid {
        fn size(&self) -> (u32, u32) {
            (self.1, self.1)
        }
        fn bgra(&self) -> Option<&[u8]> {
            Some(&self.0)
        }
        fn opaque(&self) -> bool {
            true
        }
    }

    const DIM: u32 = 64;

    fn item(id: u32, source: Arc<dyn ImageSource>, rect: Rect, clip: Rect) -> ImageItem {
        ImageItem {
            owner: SurfaceId(1),
            id,
            version: 1,
            source,
            rect,
            clip,
        }
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    async fn gpu() -> (wgpu::Device, wgpu::Queue) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .expect("no GPU adapter for the headless render test");
        adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .expect("request device")
    }

    /// The kind of target the app draws to (the surface's default format is sRGB).
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    /// Prepare and draw `base` then `overlay` onto a black `DIM`x`DIM` RGBA
    /// target, and read it back.
    fn render(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        r: &mut ImageRenderer,
        base: &[ImageItem],
        overlay: &[ImageItem],
    ) -> Vec<u8> {
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width: DIM,
                height: DIM,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        r.prepare(device, queue, base, overlay, DIM, DIM);

        let bpr = DIM * 4;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (bpr * DIM) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&Default::default());
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
            r.draw_base(&mut pass, DIM, DIM);
            r.draw_overlay(&mut pass, DIM, DIM);
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(DIM),
                },
            },
            wgpu::Extent3d {
                width: DIM,
                height: DIM,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(enc.finish()));
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            let _ = tx.send(res);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .expect("poll");
        rx.recv().expect("map channel").expect("map ok");
        let mut data = Vec::new();
        data.extend_from_slice(&slice.get_mapped_range().expect("map range"));
        data
    }

    fn px(data: &[u8], x: u32, y: u32) -> [u8; 3] {
        let o = ((y * DIM + x) * 4) as usize;
        [data[o], data[o + 1], data[o + 2]]
    }

    /// A BGRA image lands at its position, channels in order, clipped to its
    /// clip rect, and nothing outside. An opaque source's alpha is ignored: its
    /// pixels land exactly as given, whatever is under them.
    #[test]
    fn image_draws_at_its_position_clipped() {
        pollster::block_on(async {
            // A 32x32 blue (BGRA, alpha 0) image at (0, 0), clipped to its left half.
            let blue = Arc::new(Solid([255u8, 0, 0, 0].repeat(32 * 32), 32));
            let it = item(0, blue, rect(0.0, 0.0, 32.0, 32.0), rect(0.0, 0.0, 16.0, 32.0));
            // Over a white square, an opaque frame with half alpha in its pixels.
            let white = Arc::new(Solid([255u8; 4].repeat(16 * 16), 16));
            let under = item(1, white, rect(32.0, 32.0, 16.0, 16.0), rect(0.0, 0.0, 64.0, 64.0));
            let frame = Arc::new(Solid([10u8, 20, 30, 128].repeat(16 * 16), 16));
            let over = item(2, frame, rect(32.0, 32.0, 16.0, 16.0), rect(0.0, 0.0, 64.0, 64.0));
            let (device, queue) = gpu().await;
            let mut r = ImageRenderer::new(&device, FORMAT);
            let data = render(&device, &queue, &mut r, &[it, under, over], &[]);
            assert_eq!(px(&data, 8, 8), [0, 0, 255], "inside: blue, as RGBA");
            assert_eq!(px(&data, 24, 8), [0, 0, 0], "inside the image but outside its clip");
            assert_eq!(px(&data, 56, 8), [0, 0, 0], "outside the image");
            assert_eq!(px(&data, 40, 40), [30, 20, 10], "an opaque source draws its pixels as given");
        });
    }

    /// A source whose pixels are gone.
    struct Gone;

    impl ImageSource for Gone {
        fn size(&self) -> (u32, u32) {
            (8, 8)
        }
        fn bgra(&self) -> Option<&[u8]> {
            None
        }
    }

    /// A new version whose pixels are gone (a page frame the host overwrote)
    /// keeps drawing the last upload.
    #[test]
    fn an_image_whose_pixels_are_gone_keeps_its_last_upload() {
        pollster::block_on(async {
            let full = rect(0.0, 0.0, DIM as f32, DIM as f32);
            let blue = Arc::new(Solid([255u8, 0, 0, 255].repeat(8 * 8), 8));
            let (device, queue) = gpu().await;
            let mut r = ImageRenderer::new(&device, FORMAT);
            render(&device, &queue, &mut r, &[item(0, blue, rect(0.0, 0.0, 8.0, 8.0), full)], &[]);
            let gone = ImageItem {
                version: 2,
                ..item(0, Arc::new(Gone), rect(0.0, 0.0, 8.0, 8.0), full)
            };
            let data = render(&device, &queue, &mut r, &[gone], &[]);
            assert_eq!(px(&data, 4, 4), [0, 0, 255]);
            // Never uploaded: nothing to draw.
            let fresh = item(5, Arc::new(Gone), rect(16.0, 16.0, 8.0, 8.0), full);
            let data = render(&device, &queue, &mut r, &[fresh], &[]);
            assert_eq!(px(&data, 20, 20), [0, 0, 0]);
        });
    }

    /// Magnified, the boundary between an opaque and a transparent pixel fades
    /// out the opaque colour instead of darkening toward the transparent
    /// pixel's (black) colour.
    #[test]
    fn transparent_edges_have_no_dark_fringe() {
        pollster::block_on(async {
            let white = Arc::new(Solid([255u8; 4].repeat(DIM as usize * DIM as usize), DIM));
            let full = rect(0.0, 0.0, DIM as f32, DIM as f32);
            // Opaque white beside transparent black, stretched over the target.
            let edge = Image::from_rgba(2, 1, vec![255, 255, 255, 255, 0, 0, 0, 0]);
            let base = [item(0, white, full, full), item(1, edge, full, full)];
            let (device, queue) = gpu().await;
            let mut r = ImageRenderer::new(&device, FORMAT);
            let data = render(&device, &queue, &mut r, &base, &[]);
            let mid = px(&data, DIM / 2, 8);
            assert!(mid.iter().all(|c| *c >= 250), "white over white stays white, got {mid:?}");
        });
    }

    /// A half-transparent image blends in linear light: 50% white over white
    /// stays white, and 50% white over black emits half the light (188).
    #[test]
    fn partly_transparent_pixels_blend_in_linear_light() {
        pollster::block_on(async {
            let white = Arc::new(Solid([255u8; 4].repeat(16 * 16), 16));
            let half = || Image::from_rgba(1, 1, vec![255, 255, 255, 128]);
            let base = [
                item(0, white, rect(0.0, 0.0, 16.0, 16.0), rect(0.0, 0.0, 64.0, 64.0)),
                item(1, half(), rect(0.0, 0.0, 16.0, 16.0), rect(0.0, 0.0, 64.0, 64.0)),
                item(2, half(), rect(32.0, 0.0, 16.0, 16.0), rect(0.0, 0.0, 64.0, 64.0)),
            ];
            let (device, queue) = gpu().await;
            let mut r = ImageRenderer::new(&device, FORMAT);
            let data = render(&device, &queue, &mut r, &base, &[]);
            let over_white = px(&data, 8, 8);
            assert!(over_white.iter().all(|c| *c >= 254), "over white: {over_white:?}");
            let over_black = px(&data, 40, 8);
            assert!(over_black.iter().all(|c| c.abs_diff(188) <= 1), "over black: {over_black:?}");
        });
    }

    /// A decoded image stretches over its rect (through its mips when shrunk),
    /// blends by its alpha, and an overlay image draws too.
    #[test]
    fn decoded_images_scale_blend_and_draw_in_the_overlay() {
        pollster::block_on(async {
            // 128x8: left half green, right half red; drawn shrunk into 32x2.
            let (iw, ih) = (128u32, 8u32);
            let mut rgba = Vec::new();
            for _ in 0..ih {
                for x in 0..iw {
                    rgba.extend_from_slice(if x < iw / 2 { &[0, 255, 0, 255] } else { &[255, 0, 0, 255] });
                }
            }
            let split = Image::from_rgba(iw, ih, rgba);
            // Fully transparent: draws nothing.
            let clear = Image::from_rgba(4, 4, [255, 255, 255, 0].repeat(4 * 4));
            let full = rect(0.0, 0.0, DIM as f32, DIM as f32);
            let base = [
                item(0, split, rect(0.0, 0.0, 32.0, 2.0), full),
                item(1, clear, rect(0.0, 16.0, 16.0, 16.0), full),
            ];
            let blue = Arc::new(Solid([255u8, 0, 0, 255].repeat(8 * 8), 8));
            let overlay = [item(2, blue, rect(48.0, 48.0, 8.0, 8.0), full)];
            let (device, queue) = gpu().await;
            let mut r = ImageRenderer::new(&device, FORMAT);
            let data = render(&device, &queue, &mut r, &base, &overlay);
            let left = px(&data, 4, 1);
            let right = px(&data, 28, 1);
            assert!(left[1] > 200 && left[0] < 50, "left is green, got {left:?}");
            assert!(right[0] > 200 && right[1] < 50, "right is red, got {right:?}");
            assert_eq!(px(&data, 8, 24), [0, 0, 0], "transparent pixels leave the target");
            assert_eq!(px(&data, 52, 52), [0, 0, 255], "the overlay image draws");
            assert_eq!(r.textures.len(), 3, "one texture per image");
            assert_eq!(r.textures[&(SurfaceId(1), 0)].levels, 8, "128x8 down to 1x1");

            // A frame that places nothing frees every texture.
            r.prepare(&device, &queue, &[], &[], DIM, DIM);
            assert!(r.textures.is_empty(), "freed once a frame stops drawing it");
        });
    }
}
