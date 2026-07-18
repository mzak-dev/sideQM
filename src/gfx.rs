//! wgpu rendering: transparent surface, SDF shapes (circle, tiles, label box),
//! icon quads, glyphon text (hover label + fallback letters).

use std::f32::consts::{FRAC_PI_2, PI, TAU};
use std::sync::Arc;
use std::time::Instant;

use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color as TextColor, Family, FontSystem,
    Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::config::{Animation, Config};
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
    alpha: f32,
}

struct Slot {
    /// Slot angle, radians, screen coords.
    angle: f32,
    label: String,
    icon: Option<wgpu::BindGroup>,
    /// Fallback letter when there's no icon.
    letter: Option<TextBuffer>,
}

/// Damped spring toward a target; the whole animation system is these.
#[derive(Clone, Copy, Default)]
struct Spring {
    x: f32,
    v: f32,
}

impl Spring {
    fn tick(&mut self, target: f32, omega: f32, zeta: f32, dt: f32) {
        let accel = -2.0 * zeta * omega * self.v - omega * omega * (self.x - target);
        self.v += accel * dt;
        self.x += self.v * dt;
    }

    fn settled(&self, target: f32) -> bool {
        (self.x - target).abs() < 0.005 && self.v.abs() < 0.05
    }
}

enum Phase {
    Closed,
    /// Opening and open are one phase; the springs decide when motion stops.
    Shown { opened: Instant },
    Closing,
}

pub struct Tick {
    /// Keep the redraw loop running.
    pub request_frame: bool,
    /// The close animation just finished; hide the window now.
    pub just_closed: bool,
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

    anim: Animation,
    phase: Phase,
    tile_springs: Vec<Spring>,
    ring_spring: Spring,
    last_tick: Instant,
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
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32],
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
            anim: Animation::default(),
            phase: Phase::Closed,
            tile_springs: Vec::new(),
            ring_spring: Spring::default(),
            last_tick: Instant::now(),
        };
        gfx.set_items(cfg);
        gfx
    }

    /// Rebuild slot layout, icon textures, and fallback letters from config.
    pub fn set_items(&mut self, cfg: &Config) {
        self.accent = cfg.appearance.accent_rgb();
        self.opacity = cfg.appearance.opacity.clamp(0.0, 1.0);
        self.radius = cfg.appearance.radius_px as f32;
        self.anim = cfg.animation.clone();
        let total = cfg.items.len() + 1;
        self.tile_springs = vec![Spring::default(); total];

        let mut slots = Vec::with_capacity(total);
        for (k, name, icon) in cfg
            .items
            .iter()
            .enumerate()
            .map(|(k, it)| (k, it.name.clone(), icons::icon_for(it)))
            .chain(std::iter::once((cfg.items.len(), "Edit".to_string(), None)))
        {
            let angle = slot_angle(k, total);
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
            slots.push(Slot { angle, label: name, icon: bind, letter });
        }
        self.slots = slots;
    }

    /// Start (or restart) the entrance. Reopening mid-close keeps the current
    /// spring state so the menu springs back instead of popping.
    pub fn begin_open(&mut self) {
        let now = Instant::now();
        if matches!(self.phase, Phase::Closed) {
            for s in &mut self.tile_springs {
                *s = Spring::default();
            }
            self.ring_spring = Spring::default();
            self.last_tick = now;
        }
        self.phase = Phase::Shown { opened: now };
    }

    /// Start the collective shrink+fade. Launching already happened; this is cosmetic.
    pub fn begin_close(&mut self) {
        if !matches!(self.phase, Phase::Closed) {
            self.phase = Phase::Closing;
        }
    }

    /// Advance springs and draw one frame. `cursor` is relative to the window center.
    pub fn tick_render(&mut self, cursor: (f32, f32), hover: Option<usize>) -> Tick {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;
        let n = self.slots.len().max(1);

        let zeta_open = (1.0 - 0.6 * self.anim.bounciness.clamp(0.0, 1.0)).max(0.15);
        let omega_open = 4.0 / (zeta_open * (self.anim.open_ms.max(1) as f32 / 1000.0));
        let omega_close = 4.0 / (self.anim.close_ms.max(1) as f32 / 1000.0);
        let stagger = self.anim.stagger_ms as f32 / 1000.0;

        match self.phase {
            Phase::Closed => return Tick { request_frame: false, just_closed: false },
            Phase::Shown { opened } => {
                let elapsed = (now - opened).as_secs_f32();
                for (k, s) in self.tile_springs.iter_mut().enumerate() {
                    if self.anim.open_ms == 0 {
                        *s = Spring { x: 1.0, v: 0.0 };
                    } else if elapsed >= k as f32 * stagger {
                        s.tick(1.0, omega_open, zeta_open, dt);
                    }
                }
                // Ring starts once ~60% of the tiles have begun landing.
                if self.anim.open_ms == 0 {
                    self.ring_spring = Spring { x: 1.0, v: 0.0 };
                } else if elapsed >= 0.6 * n as f32 * stagger {
                    self.ring_spring.tick(1.0, omega_open, zeta_open, dt);
                }
            }
            Phase::Closing => {
                let mut all_settled = true;
                for s in &mut self.tile_springs {
                    if self.anim.close_ms == 0 {
                        *s = Spring::default();
                    } else {
                        s.tick(0.0, omega_close, 1.0, dt);
                    }
                    all_settled &= s.settled(0.0);
                }
                if self.anim.close_ms == 0 {
                    self.ring_spring = Spring::default();
                } else {
                    self.ring_spring.tick(0.0, omega_close, 1.0, dt);
                }
                all_settled &= self.ring_spring.settled(0.0);
                if all_settled {
                    self.phase = Phase::Closed;
                    return Tick { request_frame: false, just_closed: true };
                }
            }
        }

        self.draw(cursor, hover);
        Tick { request_frame: true, just_closed: false }
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

    fn draw(&mut self, cursor: (f32, f32), hover: Option<usize>) {
        let center = self.surface_cfg.width as f32 / 2.0;
        let accent = self.accent;
        let dark = [0.10, 0.10, 0.10];
        let white = [0.96, 0.96, 0.94];
        let gray = [0.45, 0.45, 0.45];

        let n = self.slots.len().max(1);
        let ring_r = self.radius;
        let rest_r = ring_r - TILE_HALF - 14.0;

        // Bulge: cursor-angle-centered cosine falloff, gated by radial distance
        // so the ring rests while the cursor sits in the dead zone.
        let cdist = (cursor.0 * cursor.0 + cursor.1 * cursor.1).sqrt();
        let cangle = cursor.1.atan2(cursor.0);
        let dead_r = 0.35 * ring_r;
        let gate = ((cdist - dead_r) / (rest_r - dead_r)).clamp(0.0, 1.0);
        let half_win = 1.2 * TAU / n as f32;
        let hover_gain = (self.anim.hover_scale - 1.0).max(0.0);

        // Per-slot animated values: (position, scale, alpha)
        let tiles: Vec<([f32; 2], f32, f32)> = self
            .slots
            .iter()
            .enumerate()
            .map(|(k, slot)| {
                let mut ad = (cangle - slot.angle).rem_euclid(TAU);
                if ad > PI {
                    ad = TAU - ad;
                }
                let w = if ad < half_win {
                    gate * 0.5 * (1.0 + (PI * ad / half_win).cos())
                } else {
                    0.0
                };
                let intro = self.tile_springs[k].x;
                let scale = intro.max(0.0) * (1.0 + hover_gain * w);
                let alpha = intro.clamp(0.0, 1.0);
                let r = rest_r + (ring_r - rest_r) * w;
                let pos = [center + slot.angle.cos() * r, center + slot.angle.sin() * r];
                (pos, scale, alpha)
            })
            .collect();

        // --- shapes ---
        let ring_s = self.ring_spring.x;
        let ring_scale = 0.85 + 0.15 * ring_s.max(0.0);
        let ring_alpha = ring_s.clamp(0.0, 1.0);
        let mut shapes = vec![ShapeInstance {
            pos: [center, center],
            half: [ring_r * ring_scale, ring_r * ring_scale],
            corner: ring_r * ring_scale,
            border: 2.0,
            fill: self.col(gray, self.opacity * ring_alpha),
            border_color: self.col(accent, 0.9 * ring_alpha),
        }];
        for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
            if scale < 0.01 || alpha < 0.01 {
                continue;
            }
            let hovered = hover == Some(k);
            shapes.push(ShapeInstance {
                pos,
                half: [TILE_HALF * scale, TILE_HALF * scale],
                corner: 4.0 * scale,
                border: 2.0,
                fill: self.col(if hovered { accent } else { white }, alpha),
                border_color: self.col(dark, alpha),
            });
        }

        // --- hover label geometry (needs text measured first) ---
        let mut label_area: Option<(f32, f32, f32)> = None; // left, top, alpha
        if let Some(k) = hover {
            if let (Some(slot), Some(&(pos, scale, alpha))) = (self.slots.get(k), tiles.get(k)) {
                if alpha > 0.01 {
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
                    let bx = pos[0];
                    let by = pos[1] - TILE_HALF * scale - box_h / 2.0 - 6.0;
                    shapes.push(ShapeInstance {
                        pos: [bx, by],
                        half: [box_w / 2.0, box_h / 2.0],
                        corner: 2.0,
                        border: 0.0,
                        fill: self.col(dark, 0.95 * alpha),
                        border_color: self.col(dark, 0.95 * alpha),
                    });
                    label_area = Some((bx - text_w / 2.0, by - LABEL_FONT_PX * 0.65, alpha));
                }
            }
        }

        let shape_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("shapes"),
            contents: bytemuck::cast_slice(&shapes),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // --- icon quads ---
        let tex_instances: Vec<TexInstance> = tiles
            .iter()
            .map(|&(pos, scale, alpha)| TexInstance {
                pos,
                half: [(TILE_HALF - ICON_INSET) * scale, (TILE_HALF - ICON_INSET) * scale],
                alpha,
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
                let (pos, scale, alpha) = tiles[k];
                if scale < 0.01 || alpha < 0.01 {
                    continue;
                }
                areas.push(TextArea {
                    buffer: letter,
                    left: pos[0] - 9.0 * scale,
                    top: pos[1] - 14.0 * scale,
                    scale,
                    bounds: full_bounds,
                    default_color: TextColor::rgba(26, 26, 26, (alpha * 255.0) as u8),
                    custom_glyphs: &[],
                });
            }
        }
        if let Some((left, top, alpha)) = label_area {
            areas.push(TextArea {
                buffer: &self.label_buf,
                left,
                top,
                scale: 1.0,
                bounds: full_bounds,
                default_color: TextColor::rgba(245, 245, 240, (alpha * 255.0) as u8),
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
