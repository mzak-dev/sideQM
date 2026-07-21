//! wgpu rendering: transparent surface, SDF shapes (scrim, tiles, hub, arc),
//! icon quads, glyphon text (always-on tile labels, hub name/subtitle).

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

/// Icon inset and tile corner radius, as ratios of tile half-extent, so they
/// keep their proportions under a configurable tile size instead of drifting.
const ICON_INSET_RATIO: f32 = 10.0 / 32.0;
const TILE_CORNER_RATIO: f32 = 18.0 / 32.0;
/// How far outside the scrim the arc indicator rides.
const ARC_OFFSET: f32 = 6.0;
/// Arc half-width, as a fraction of one slot's angular width.
const ARC_HALF_FRAC: f32 = 0.4;

/// Square window edge length for a given circle radius, tile size, and label
/// font — headroom has to grow with either or a large configured tile/label
/// clips at the window edge.
pub fn window_size(radius_px: u32, tile_half: f32, label_font_px: f32) -> u32 {
    let margin = tile_half + label_font_px * 3.0 + 20.0;
    2 * (radius_px as f32 + margin) as u32
}

/// Angle of slot `k` of `total`, radians, screen coords (y down).
/// Slot 0 sits at 12 o'clock; slots proceed clockwise, evenly spaced.
pub fn slot_angle(k: usize, total: usize) -> f32 {
    k as f32 * TAU / total as f32 - FRAC_PI_2
}

/// Which slot the cursor is over, if any.
///
/// The whole wedge is the target: there's no outer edge where selection stops
/// working (overshoot is fine), only an inner dead zone (`dead_zone_r`, the
/// Hub's own radius) where releasing cancels instead of launching.
pub fn hovered_item(
    cursor: (f64, f64),
    center: (f64, f64),
    total: usize,
    dead_zone_r: f32,
) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let (dx, dy) = ((cursor.0 - center.0) as f32, (cursor.1 - center.1) as f32);
    if dx * dx + dy * dy < dead_zone_r * dead_zone_r {
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
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct ShapeInstance {
    pos: [f32; 2],
    half: [f32; 2],
    corner: f32,
    border: f32,
    fill: [f32; 4],
    border_color: [f32; 4],
    /// 0 = rounded box, 1 = arc stroke.
    kind: f32,
    /// Arc: pointing angle, radians, same atan2 convention as `slot_angle`.
    angle_center: f32,
    /// Arc: half angular width, radians.
    angle_half: f32,
    /// Box only: > 0 = dash count around the border (the meta/"Dodaj" tile).
    dash: f32,
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
    /// Fallback glyph when there's no icon: first letter, or "+" for the meta slot.
    letter: Option<TextBuffer>,
    /// Always-visible caption below the tile; shaped once, recolored per frame.
    label_buf: TextBuffer,
    /// The synthesized "Dodaj" slot at the end, styled with a dashed border.
    is_meta: bool,
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
    /// Hub's main line: lowercased selected name, or "·" while idle.
    hub_label_buf: TextBuffer,
    /// Hub's subtitle, shown only while a slot is selected.
    hub_sub_buf: TextBuffer,

    slots: Vec<Slot>,
    accent: [f32; 3],
    opacity: f32,
    radius: f32,
    tile_half: f32,
    hub_ratio: f32,
    label_font_px: f32,

    anim: Animation,
    phase: Phase,
    tile_springs: Vec<Spring>,
    ring_spring: Spring,
    /// Per-slot selected-tile scale (1.0 <-> hover_scale), independent of the
    /// tile's own entrance/exit spring.
    select_springs: Vec<Spring>,
    arc_rot: Spring,
    arc_alpha: Spring,
    /// Unwrapped rotation target the arc springs toward (can exceed +-TAU so
    /// retargeting always takes the shortest path, never the long way around).
    arc_target: f32,
    /// Whether the arc is currently shown (or fading); false means the next
    /// appearance should snap instead of springing from a stale angle.
    arc_on: bool,
    last_hover: Option<usize>,
    /// Captured at begin_close so the launched tile can pop while everything
    /// else fades, even though `hover` itself is cleared right after.
    closing_launched: Option<usize>,
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
                        6 => Float32, 7 => Float32, 8 => Float32, 9 => Float32,
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
        // Placeholder metrics: set_items (called at the end of this function)
        // recreates both from the configured label_font_px and seeds the idle
        // "." text — that value isn't known this early in construction.
        let hub_label_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 16.9));
        let hub_sub_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));

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
            hub_label_buf,
            hub_sub_buf,
            slots: Vec::new(),
            accent: [0.0; 3],
            opacity: 0.45,
            radius: 280.0,
            tile_half: 32.0,
            hub_ratio: 0.28,
            label_font_px: 13.0,
            anim: Animation::default(),
            phase: Phase::Closed,
            tile_springs: Vec::new(),
            ring_spring: Spring::default(),
            select_springs: Vec::new(),
            arc_rot: Spring::default(),
            arc_alpha: Spring::default(),
            arc_target: 0.0,
            arc_on: false,
            last_hover: None,
            closing_launched: None,
            last_tick: Instant::now(),
        };
        gfx.set_items(cfg);
        gfx
    }

    /// Rebuild slot layout, icon textures, and fallback letters from config.
    pub fn set_items(&mut self, cfg: &Config) {
        self.accent = cfg.appearance.accent_rgb();
        self.opacity = cfg.appearance.opacity();
        self.radius = cfg.appearance.radius_px() as f32;
        self.tile_half = cfg.appearance.tile_half();
        self.hub_ratio = cfg.appearance.hub_ratio();
        self.label_font_px = cfg.appearance.label_font_px();
        self.anim = cfg.animation.clone();

        // Metrics depend on label_font_px, which just changed (or is only now
        // known for the first time), so these get rebuilt from scratch here
        // rather than resized in place.
        self.hub_label_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(self.label_font_px * 1.15, self.label_font_px * 1.15 * 1.3),
        );
        self.hub_label_buf.set_text(
            "\u{b7}",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        self.hub_label_buf.shape_until_scroll(&mut self.font_system, false);
        self.hub_sub_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(self.label_font_px * 0.85, self.label_font_px * 0.85 * 1.3),
        );

        let total = cfg.items.len() + 1;
        self.tile_springs = vec![Spring::default(); total];
        self.select_springs = vec![Spring::default(); total];

        let glyph_px = self.tile_half * 0.9;
        let mut slots = Vec::with_capacity(total);
        for (k, name, icon, is_meta) in cfg
            .items
            .iter()
            .enumerate()
            .map(|(k, it)| (k, it.name.clone(), icons::icon_for(it), false))
            .chain(std::iter::once((cfg.items.len(), "Dodaj".to_string(), None, true)))
        {
            let angle = slot_angle(k, total);
            let bind = icon.map(|ic| self.upload_icon(&ic));
            let letter = if bind.is_none() {
                let ch = if is_meta {
                    "+".to_string()
                } else {
                    name.chars().next().unwrap_or('?').to_uppercase().to_string()
                };
                let mut buf =
                    TextBuffer::new(&mut self.font_system, Metrics::new(glyph_px, glyph_px));
                buf.set_text(&ch, &Attrs::new().family(Family::Monospace), Shaping::Advanced, None);
                buf.shape_until_scroll(&mut self.font_system, false);
                Some(buf)
            } else {
                None
            };
            let mut label_buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(self.label_font_px, self.label_font_px * 1.3),
            );
            label_buf.set_text(&name, &Attrs::new().family(Family::Monospace), Shaping::Advanced, None);
            label_buf.shape_until_scroll(&mut self.font_system, false);
            slots.push(Slot { angle, label: name, icon: bind, letter, label_buf, is_meta });
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
            for s in &mut self.select_springs {
                *s = Spring::default();
            }
            self.ring_spring = Spring::default();
            self.arc_rot = Spring::default();
            self.arc_alpha = Spring::default();
            self.arc_on = false;
            self.last_hover = None;
            self.closing_launched = None;
            self.last_tick = now;
        }
        self.phase = Phase::Shown { opened: now };
    }

    /// Start the collective shrink+fade. Launching already happened; this is
    /// cosmetic. `launched` is the slot that fired, if any, captured here
    /// because `hover` itself gets cleared by the caller right after this call.
    pub fn begin_close(&mut self, launched: Option<usize>) {
        if !matches!(self.phase, Phase::Closed) {
            self.phase = Phase::Closing;
            self.closing_launched = launched;
        }
    }

    /// Advance springs and draw one frame.
    pub fn tick_render(&mut self, hover: Option<usize>) -> Tick {
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

        // --- selection: per-tile select-scale, springing at a different rate
        // toward hover_scale than away from it, plus a bigger "launched" pop ---
        let omega_select = 4.0 / (zeta_open * 0.5); // 500ms, bouncy
        let omega_deselect = 4.0 / 0.4; // 400ms, critically damped
        let pop_scale = self.anim.hover_scale * (1.42 / 1.16); // spec: 1.16 -> 1.42 on launch
        for (k, s) in self.select_springs.iter_mut().enumerate() {
            let target = if matches!(self.phase, Phase::Closing) && self.closing_launched == Some(k)
            {
                pop_scale
            } else if hover == Some(k) {
                self.anim.hover_scale
            } else {
                1.0
            };
            if target > s.x {
                s.tick(target, omega_select, zeta_open, dt);
            } else {
                s.tick(target, omega_deselect, 1.0, dt);
            }
        }

        // --- arc + hub text: snap on first appearance, spring by the
        // shortest angular path after that ---
        if hover != self.last_hover {
            let attrs = Attrs::new().family(Family::Monospace);
            match hover {
                Some(k) => {
                    let target_angle = slot_angle(k, n);
                    if self.arc_on {
                        let delta = ((target_angle - self.arc_target + PI).rem_euclid(TAU)) - PI;
                        self.arc_target += delta;
                    } else {
                        self.arc_target = target_angle;
                        self.arc_rot = Spring { x: target_angle, v: 0.0 };
                        self.arc_on = true;
                    }
                    let name = self.slots[k].label.to_lowercase();
                    self.hub_label_buf.set_text(&name, &attrs, Shaping::Advanced, None);
                    self.hub_label_buf.shape_until_scroll(&mut self.font_system, false);
                    self.hub_sub_buf.set_text(
                        "puść, aby uruchomić",
                        &attrs,
                        Shaping::Advanced,
                        None,
                    );
                    self.hub_sub_buf.shape_until_scroll(&mut self.font_system, false);
                }
                None => {
                    self.arc_on = false;
                    self.hub_label_buf.set_text("\u{b7}", &attrs, Shaping::Advanced, None);
                    self.hub_label_buf.shape_until_scroll(&mut self.font_system, false);
                }
            }
            self.last_hover = hover;
        }
        let omega_arc = 4.0 / (zeta_open * 0.45); // 450ms shortest-path spring
        let omega_arc_fade = 4.0 / 0.15; // 150ms fade, in and out
        self.arc_rot.tick(self.arc_target, omega_arc, zeta_open, dt);
        self.arc_alpha.tick(if hover.is_some() { 1.0 } else { 0.0 }, omega_arc_fade, 1.0, dt);

        self.draw(hover);
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

    fn draw(&mut self, hover: Option<usize>) {
        let center = self.surface_cfg.width as f32 / 2.0;
        let accent = self.accent;
        // Hex sources: scrim #18191F, hub #101216, idle caption #7D8590, hub idle dot #5C6570.
        let scrim_bg = [0.094, 0.102, 0.122];
        let hub_bg = [0.063, 0.071, 0.086];
        let idle_text = [0.490, 0.522, 0.565];
        let hub_dot = [0.361, 0.396, 0.439];
        let white = [1.0, 1.0, 1.0];
        let rgba = |c: [f32; 3], a: f32| {
            TextColor::rgba(
                (c[0] * 255.0) as u8,
                (c[1] * 255.0) as u8,
                (c[2] * 255.0) as u8,
                (a * 255.0) as u8,
            )
        };

        let tile_half = self.tile_half;
        let tile_corner = tile_half * TILE_CORNER_RATIO;
        let icon_inset = tile_half * ICON_INSET_RATIO;
        let label_font_px = self.label_font_px;
        let n = self.slots.len().max(1);
        let ring_r = self.radius;
        let rest_r = ring_r - tile_half - 14.0;
        let hub_r = ring_r * self.hub_ratio;

        // Per-slot animated values: (position, scale, alpha). Tiles always sit
        // at rest_r now — no more cursor-driven outward shift.
        let tiles: Vec<([f32; 2], f32, f32)> = self
            .slots
            .iter()
            .enumerate()
            .map(|(k, slot)| {
                let pos = [center + slot.angle.cos() * rest_r, center + slot.angle.sin() * rest_r];
                let intro = self.tile_springs[k].x.max(0.0);
                let scale = intro * self.select_springs[k].x;
                let alpha = intro.clamp(0.0, 1.0);
                (pos, scale, alpha)
            })
            .collect();

        // --- shapes: scrim, hub, tiles, arc ---
        let ring_s = self.ring_spring.x;
        let ring_scale = 0.85 + 0.15 * ring_s.max(0.0);
        let ring_alpha = ring_s.clamp(0.0, 1.0);
        let mut shapes = vec![
            ShapeInstance {
                pos: [center, center],
                half: [ring_r * ring_scale, ring_r * ring_scale],
                corner: ring_r * ring_scale,
                border: 1.2,
                fill: self.col(scrim_bg, self.opacity * ring_alpha),
                border_color: self.col(white, 0.06 * ring_alpha),
                ..Default::default()
            },
            ShapeInstance {
                pos: [center, center],
                half: [hub_r, hub_r],
                corner: hub_r,
                border: 1.2,
                fill: self.col(hub_bg, ring_alpha),
                border_color: self.col(
                    if hover.is_some() { accent } else { white },
                    (if hover.is_some() { 0.45 } else { 0.10 }) * ring_alpha,
                ),
                ..Default::default()
            },
        ];
        for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
            if scale < 0.01 || alpha < 0.01 {
                continue;
            }
            let hovered = hover == Some(k);
            let (fill_c, fill_a, border_c, border_a) = if hovered {
                (accent, 0.09, accent, 0.9)
            } else {
                (white, 0.06, white, 0.12)
            };
            shapes.push(ShapeInstance {
                pos,
                half: [tile_half * scale, tile_half * scale],
                corner: tile_corner * scale,
                border: 1.1,
                fill: self.col(fill_c, fill_a * alpha),
                border_color: self.col(border_c, border_a * alpha),
                dash: if self.slots[k].is_meta { 10.0 } else { 0.0 },
                ..Default::default()
            });
        }
        let arc_alpha = self.arc_alpha.x.clamp(0.0, 1.0);
        if arc_alpha > 0.01 {
            shapes.push(ShapeInstance {
                pos: [center, center],
                half: [ring_r + ARC_OFFSET, ring_r + ARC_OFFSET],
                border: 1.75, // half of the spec's 3.5px stroke
                fill: self.col(accent, arc_alpha),
                kind: 1.0,
                angle_center: self.arc_rot.x,
                angle_half: ARC_HALF_FRAC * (TAU / n as f32),
                ..Default::default()
            });
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
                half: [(tile_half - icon_inset) * scale, (tile_half - icon_inset) * scale],
                alpha,
            })
            .collect();
        let tex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tex-instances"),
            contents: bytemuck::cast_slice(&tex_instances),
            usage: wgpu::BufferUsages::VERTEX,
        });

        // --- text areas: fallback letters, always-on tile labels, hub text ---
        let mut areas: Vec<TextArea> = Vec::new();
        let full_bounds = TextBounds {
            left: 0,
            top: 0,
            right: self.surface_cfg.width as i32,
            bottom: self.surface_cfg.height as i32,
        };
        for (k, slot) in self.slots.iter().enumerate() {
            let (pos, scale, alpha) = tiles[k];
            if scale < 0.01 || alpha < 0.01 {
                continue;
            }
            if let Some(letter) = &slot.letter {
                // Positioning ratios tuned against the glyph_px used to shape
                // this buffer in set_items (tile_half * 0.9); kept in sync so
                // the glyph stays centered as tile_half changes.
                let glyph_px = tile_half * 0.9;
                areas.push(TextArea {
                    buffer: letter,
                    left: pos[0] - glyph_px * 0.32 * scale,
                    top: pos[1] - glyph_px * 0.5 * scale,
                    scale,
                    bounds: full_bounds,
                    default_color: TextColor::rgba(26, 26, 26, (alpha * 255.0) as u8),
                    custom_glyphs: &[],
                });
            }
            let hovered = hover == Some(k);
            let label_w = slot.label_buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max);
            areas.push(TextArea {
                buffer: &slot.label_buf,
                left: pos[0] - label_w / 2.0,
                top: pos[1] + tile_half * scale + 8.0,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(if hovered { accent } else { idle_text }, alpha),
                custom_glyphs: &[],
            });
        }
        let name_w = self.hub_label_buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max);
        areas.push(TextArea {
            buffer: &self.hub_label_buf,
            left: center - name_w / 2.0,
            top: center - label_font_px * 0.7,
            scale: 1.0,
            bounds: full_bounds,
            default_color: rgba(if hover.is_some() { accent } else { hub_dot }, ring_alpha),
            custom_glyphs: &[],
        });
        if hover.is_some() {
            let sub_w = self.hub_sub_buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max);
            areas.push(TextArea {
                buffer: &self.hub_sub_buf,
                left: center - sub_w / 2.0,
                top: center + label_font_px * 0.55,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(idle_text, ring_alpha),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_zone_cancels() {
        assert_eq!(hovered_item((0.0, 0.0), (0.0, 0.0), 5, 40.0), None);
        assert_eq!(hovered_item((10.0, 10.0), (0.0, 0.0), 5, 40.0), None);
    }

    #[test]
    fn no_outer_cutoff() {
        // Straight up, way past any tile radius: still slot 0, not None.
        assert_eq!(hovered_item((0.0, -10_000.0), (0.0, 0.0), 5, 40.0), Some(0));
    }

    #[test]
    fn slot_zero_is_12_oclock() {
        assert_eq!(hovered_item((0.0, -200.0), (0.0, 0.0), 5, 40.0), Some(0));
    }

    #[test]
    fn picks_nearest_sector_clockwise() {
        // 5 slots => 72 degrees apart, so slot 0's wedge is [-126, -54] degrees
        // (0 = +x/3 o'clock, atan2 convention). Well inside stays slot 0;
        // just past the boundary flips to slot 1.
        let center = (0.0, 0.0);
        let inside = angle_point(-90.0 + 30.0, 200.0);
        assert_eq!(hovered_item(inside, center, 5, 40.0), Some(0));
        let past = angle_point(-90.0 + 40.0, 200.0);
        assert_eq!(hovered_item(past, center, 5, 40.0), Some(1));
    }

    #[test]
    fn wraps_from_last_slot_to_first() {
        // Just past slot 0's wedge in the other direction (below -126 degrees)
        // wraps to the last slot (4), not -1 or a panic.
        let point = angle_point(-90.0 - 40.0, 200.0);
        assert_eq!(hovered_item(point, (0.0, 0.0), 5, 40.0), Some(4));
    }

    #[test]
    fn generic_slot_count() {
        // N != 5 still divides the circle evenly (120 degree slots here).
        assert_eq!(hovered_item(angle_point(-90.0, 200.0), (0.0, 0.0), 3, 40.0), Some(0));
        assert_eq!(hovered_item(angle_point(-90.0 + 120.0, 200.0), (0.0, 0.0), 3, 40.0), Some(1));
    }

    fn angle_point(deg: f32, r: f32) -> (f64, f64) {
        let rad = deg.to_radians();
        (rad.cos() as f64 * r as f64, rad.sin() as f64 * r as f64)
    }
}
