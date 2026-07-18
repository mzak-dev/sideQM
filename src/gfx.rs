//! wgpu rendering: transparent surface, SDF shapes (circle, tiles, label box),
//! icon quads, glyphon text (hover label + fallback letters).

use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::Arc;

use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color as TextColor, Family, FontSystem,
    Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::config::Config;
use crate::icons;

/// Tile half-extent in px (tiles are 64x64, centered on the circle rim).
pub const TILE_HALF: f32 = 32.0;
/// Headroom around the circle for tiles poking out and the hover label.
pub const PAD: f32 = 60.0;
const ICON_INSET: f32 = 10.0;
const LABEL_FONT_PX: f32 = 16.0;

/// Square window edge length for a given circle radius.
pub fn window_size(radius_px: u32) -> u32 {
    2 * (radius_px + TILE_HALF as u32 + PAD as u32)
}

/// Angle of slot `k` of `total`, radians, screen coords (y down).
/// The last slot (the edit item) is pinned to the bottom (6 o'clock);
/// everything else spreads evenly from there.
pub fn slot_angle(k: usize, total: usize) -> f32 {
    FRAC_PI_2 + (k as f32 - (total - 1) as f32) * TAU / total as f32
}

/// Slot center position, px, relative to the window center.
pub fn slot_offset(k: usize, total: usize, radius: f32) -> (f32, f32) {
    let a = slot_angle(k, total);
    (radius * a.cos(), radius * a.sin())
}

/// Which slot the cursor is over, if any.
///
/// LEARNING CONTRIBUTION POINT: this function is the feel of the whole menu —
/// the inner dead-zone (release-to-cancel area), the outer reach, and whether
/// hover snaps to the nearest sector or requires touching the tile itself are
/// all design choices. This implementation: generous sector snapping between
/// 35% of the radius and just past the tiles. Rewrite to taste.
pub fn hovered_item(
    cursor: (f64, f64),
    center: (f64, f64),
    total: usize,
    radius: f32,
) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let (dx, dy) = ((cursor.0 - center.0) as f32, (cursor.1 - center.1) as f32);
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < radius * 0.35 || dist > radius + 2.2 * TILE_HALF {
        return None;
    }
    let angle = dy.atan2(dx);
    (0..total).min_by_key(|&k| {
        // shortest angular distance to the slot, scaled to an integer key
        let mut d = (angle - slot_angle(k, total)).rem_euclid(TAU);
        if d > TAU / 2.0 {
            d = TAU - d;
        }
        (d * 10_000.0) as u32
    })
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ShapeInstance {
    pos: [f32; 2],
    half: [f32; 2],
    corner: f32,
    border: f32,
    fill: [f32; 4],
    border_color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TexInstance {
    pos: [f32; 2],
    half: [f32; 2],
}

struct Slot {
    /// Tile center relative to window top-left, px.
    pos: [f32; 2],
    label: String,
    icon: Option<wgpu::BindGroup>,
    /// Fallback letter when there's no icon.
    letter: Option<TextBuffer>,
}

pub struct Gfx {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_cfg: wgpu::SurfaceConfiguration,
    srgb: bool,

    shape_pipeline: wgpu::RenderPipeline,
    tex_pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    icon_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,

    font_system: FontSystem,
    swash: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    label_buf: TextBuffer,

    slots: Vec<Slot>,
    accent: [f32; 3],
    opacity: f32,
    radius: f32,
}

impl Gfx {
    pub fn new(window: Arc<Window>, cfg: &Config) -> Gfx {
        let size = window.inner_size();
        let mut instance_desc = wgpu::InstanceDescriptor::new_without_display_handle();
        // ponytail: DX12 hardcoded — wgpu 30's Vulkan path access-violates on this
        // AMD driver ~2s after the first present to a transparent window
        // (STATUS_ACCESS_VIOLATION, diagnosed 2026-07). Revisit if wgpu/driver update.
        instance_desc.backends = match std::env::var("SIDEQM_BACKEND").as_deref() {
            Ok("vulkan") => wgpu::Backends::VULKAN,
            Ok("gl") => wgpu::Backends::GL,
            _ => wgpu::Backends::DX12,
        };
        // DirectComposition presentation: the only DX12 path with per-pixel window alpha.
        instance_desc.backend_options.dx12.presentation_system =
            wgpu::Dx12SwapchainKind::DxgiFromVisual;
        let instance = wgpu::Instance::new(instance_desc);
        let surface = instance.create_surface(window).expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("no adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("no device");

        let caps = surface.get_capabilities(&adapter);
        // Transparency needs a non-opaque alpha mode; prefer premultiplied.
        let alpha_mode = [
            wgpu::CompositeAlphaMode::PreMultiplied,
            wgpu::CompositeAlphaMode::PostMultiplied,
            wgpu::CompositeAlphaMode::Inherit,
        ]
        .into_iter()
        .find(|m| caps.alpha_modes.contains(m))
        .unwrap_or_else(|| {
            eprintln!(
                "sideQM: no transparent alpha mode on {} ({:?}); menu will have an opaque backdrop",
                adapter.get_info().name,
                caps.alpha_modes
            );
            caps.alpha_modes[0]
        });
        eprintln!(
            "sideQM: adapter {} / {:?}, alpha {:?}",
            adapter.get_info().name,
            adapter.get_info().backend,
            alpha_mode
        );
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let srgb = format.is_srgb();

        let surface_cfg = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &surface_cfg);

        let shader = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));

        let globals_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("globals"),
            contents: bytemuck::cast_slice(&[size.width as f32, size.height as f32, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        let icon_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("icon"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let premul_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let target = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(premul_blend),
            write_mask: wgpu::ColorWrites::ALL,
        })];

        let shape_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shapes"),
            bind_group_layouts: &[Some(&globals_layout)],
            immediate_size: 0,
        });
        let shape_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shapes"),
            layout: Some(&shape_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_shape"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ShapeInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, 1 => Float32x2, 2 => Float32,
                        3 => Float32, 4 => Float32x4, 5 => Float32x4,
                    ],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_shape"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let tex_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tex"),
            bind_group_layouts: &[Some(&globals_layout), Some(&icon_layout)],
            immediate_size: 0,
        });
        let tex_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tex"),
            layout: Some(&tex_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_tex"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TexInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_tex"),
                compilation_options: Default::default(),
                targets: &target,
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let mut font_system = FontSystem::new();
        let swash = SwashCache::new();
        let glyph_cache = GlyphCache::new(&device);
        let mut viewport = Viewport::new(&device, &glyph_cache);
        viewport.update(&queue, Resolution { width: size.width, height: size.height });
        let mut atlas = TextAtlas::new(&device, &queue, &glyph_cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let label_buf = TextBuffer::new(
            &mut font_system,
            Metrics::new(LABEL_FONT_PX, LABEL_FONT_PX * 1.3),
        );

        let mut gfx = Gfx {
            surface,
            device,
            queue,
            surface_cfg,
            srgb,
            shape_pipeline,
            tex_pipeline,
            globals_buf,
            globals_bind,
            icon_layout,
            sampler,
            font_system,
            swash,
            viewport,
            atlas,
            text_renderer,
            label_buf,
            slots: Vec::new(),
            accent: [0.0; 3],
            opacity: 0.45,
            radius: 280.0,
        };
        gfx.set_items(cfg);
        gfx
    }

    /// Rebuild slot layout, icon textures, and fallback letters from config.
    pub fn set_items(&mut self, cfg: &Config) {
        self.accent = cfg.appearance.accent_rgb();
        self.opacity = cfg.appearance.opacity.clamp(0.0, 1.0);
        self.radius = cfg.appearance.radius_px as f32;
        let total = cfg.items.len() + 1;
        let center = window_size(cfg.appearance.radius_px) as f32 / 2.0;

        let mut slots = Vec::with_capacity(total);
        for (k, name, icon) in cfg
            .items
            .iter()
            .enumerate()
            .map(|(k, it)| (k, it.name.clone(), icons::icon_for(it)))
            .chain(std::iter::once((cfg.items.len(), "Edit".to_string(), None)))
        {
            let (ox, oy) = slot_offset(k, total, self.radius);
            let pos = [center + ox, center + oy];
            let bind = icon.map(|ic| self.upload_icon(&ic));
            let letter = if bind.is_none() {
                let ch = name.chars().next().unwrap_or('?').to_uppercase().to_string();
                let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(28.0, 28.0));
                buf.set_text(&ch, &Attrs::new().family(Family::Monospace), Shaping::Advanced, None);
                buf.shape_until_scroll(&mut self.font_system, false);
                Some(buf)
            } else {
                None
            };
            slots.push(Slot { pos, label: name, icon: bind, letter });
        }
        self.slots = slots;
    }

    fn upload_icon(&self, icon: &icons::RgbaIcon) -> wgpu::BindGroup {
        let format = if self.srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let texture = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: Some("icon"),
                size: wgpu::Extent3d {
                    width: icon.width,
                    height: icon.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &icon.pixels,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("icon"),
            layout: &self.icon_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.surface_cfg.width = width.max(1);
        self.surface_cfg.height = height.max(1);
        self.surface.configure(&self.device, &self.surface_cfg);
        self.queue.write_buffer(
            &self.globals_buf,
            0,
            bytemuck::cast_slice(&[width as f32, height as f32, 0.0, 0.0]),
        );
        self.viewport.update(&self.queue, Resolution { width, height });
    }

    /// sRGB component -> linear, when the surface format demands linear input.
    fn col(&self, c: [f32; 3], a: f32) -> [f32; 4] {
        if self.srgb {
            let f = |v: f32| {
                if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            [f(c[0]), f(c[1]), f(c[2]), a]
        } else {
            [c[0], c[1], c[2], a]
        }
    }

    pub fn render(&mut self, hover: Option<usize>) {
        let center = self.surface_cfg.width as f32 / 2.0;
        let accent = self.accent;
        let dark = [0.10, 0.10, 0.10];
        let white = [0.96, 0.96, 0.94];
        let gray = [0.45, 0.45, 0.45];

        // --- shapes ---
        let mut shapes = vec![ShapeInstance {
            pos: [center, center],
            half: [self.radius, self.radius],
            corner: self.radius,
            border: 2.0,
            fill: self.col(gray, self.opacity),
            border_color: self.col(accent, 0.9),
        }];
        for (k, slot) in self.slots.iter().enumerate() {
            let hovered = hover == Some(k);
            shapes.push(ShapeInstance {
                pos: slot.pos,
                half: [TILE_HALF, TILE_HALF],
                corner: 4.0,
                border: 2.0,
                fill: self.col(if hovered { accent } else { white }, 1.0),
                border_color: self.col(dark, 1.0),
            });
        }

        // --- hover label geometry (needs text measured first) ---
        let mut label_area: Option<(f32, f32, f32, f32)> = None; // left, top, w, h
        if let Some(k) = hover {
            if let Some(slot) = self.slots.get(k) {
                let text = slot.label.to_uppercase();
                self.label_buf.set_text(
                    &text,
                    &Attrs::new().family(Family::Monospace),
                    Shaping::Advanced,
                    None,
                );
                self.label_buf.shape_until_scroll(&mut self.font_system, false);
                let text_w = self
                    .label_buf
                    .layout_runs()
                    .map(|r| r.line_w)
                    .fold(0.0f32, f32::max);
                let box_h = LABEL_FONT_PX * 1.3 + 8.0;
                let box_w = text_w + 20.0;
                let bx = slot.pos[0];
                let by = slot.pos[1] - TILE_HALF - box_h / 2.0 - 6.0;
                shapes.push(ShapeInstance {
                    pos: [bx, by],
                    half: [box_w / 2.0, box_h / 2.0],
                    corner: 2.0,
                    border: 0.0,
                    fill: self.col(dark, 0.95),
                    border_color: self.col(dark, 0.95),
                });
                label_area = Some((bx - text_w / 2.0, by - LABEL_FONT_PX * 0.65, box_w, box_h));
            }
        }

        let shape_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("shapes"),
            contents: bytemuck::cast_slice(&shapes),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // --- icon quads ---
        let tex_instances: Vec<TexInstance> = self
            .slots
            .iter()
            .map(|s| TexInstance {
                pos: s.pos,
                half: [TILE_HALF - ICON_INSET, TILE_HALF - ICON_INSET],
            })
            .collect();
        let tex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tex-instances"),
            contents: bytemuck::cast_slice(&tex_instances),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // --- text areas ---
        let mut areas: Vec<TextArea> = Vec::new();
        let full_bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.surface_cfg.width as i32,
            bottom: self.surface_cfg.height as i32,
        };
        for (k, slot) in self.slots.iter().enumerate() {
            if let Some(letter) = &slot.letter {
                let hovered = hover == Some(k);
                areas.push(TextArea {
                    buffer: letter,
                    left: slot.pos[0] - 9.0,
                    top: slot.pos[1] - 14.0,
                    scale: 1.0,
                    bounds: full_bounds,
                    default_color: if hovered {
                        TextColor::rgb(20, 20, 20)
                    } else {
                        TextColor::rgb(26, 26, 26)
                    },
                    custom_glyphs: &[],
                });
            }
        }
        if let Some((left, top, _, _)) = label_area {
            areas.push(TextArea {
                buffer: &self.label_buf,
                left,
                top,
                scale: 1.0,
                bounds: full_bounds,
                default_color: TextColor::rgb(245, 245, 240),
                custom_glyphs: &[],
            });
        }
        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash,
        ) {
            eprintln!("sideQM: text prepare failed: {e}");
        }

        // --- pass ---
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            Cst::Outdated | Cst::Lost => {
                self.surface.configure(&self.device, &self.surface_cfg);
                match self.surface.get_current_texture() {
                    Cst::Success(f) | Cst::Suboptimal(f) => f,
                    other => {
                        eprintln!("sideQM: surface unavailable after reconfigure: {other:?}");
                        return;
                    }
                }
            }
            other => {
                eprintln!("sideQM: skipping frame: {other:?}");
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("menu"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.shape_pipeline);
            pass.set_bind_group(0, &self.globals_bind, &[]);
            pass.set_vertex_buffer(0, shape_buf.slice(..));
            pass.draw(0..6, 0..shapes.len() as u32);

            pass.set_pipeline(&self.tex_pipeline);
            pass.set_vertex_buffer(0, tex_buf.slice(..));
            for (k, slot) in self.slots.iter().enumerate() {
                if let Some(bind) = &slot.icon {
                    pass.set_bind_group(1, bind, &[]);
                    pass.draw(0..6, k as u32..k as u32 + 1);
                }
            }

            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                eprintln!("sideQM: text render failed: {e}");
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        self.atlas.trim();
    }
}
