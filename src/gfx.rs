//! wgpu rendering: transparent surface, SDF shapes (scrim, tiles, hub, arc),
//! icon quads, glyphon text (always-on tile labels, hub name/subtitle).

use std::f32::consts::TAU;
use std::sync::Arc;
use std::time::Instant;

use glyphon::{
    Attrs, Buffer as TextBuffer, Cache as GlyphCache, Color as TextColor, Family, FontSystem,
    Metrics, Resolution, Shaping, SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer,
    Viewport,
};
use wgpu::util::DeviceExt;
use winit::window::Window;

use crate::anim::{Animator, FrameModel};
use crate::config::Config;
use crate::geometry::MenuGeometry;
use crate::icons;

/// Icon inset and tile corner radius, as ratios of tile half-extent, so they
/// keep their proportions under a configurable tile size instead of drifting.
const ICON_INSET_RATIO: f32 = 10.0 / 32.0;
const TILE_CORNER_RATIO: f32 = 18.0 / 32.0;
/// How far outside the scrim the arc indicator rides.
const ARC_OFFSET: f32 = 6.0;
/// Arc half-width, as a fraction of one slot's angular width.
const ARC_HALF_FRAC: f32 = 0.4;

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
    /// The full config, one shape everywhere; visual values read straight off it.
    cfg: Config,
    /// Shared Menu geometry; same-constructor copy of the one App holds.
    geo: MenuGeometry,
    animator: Animator,
    /// Hover whose Item name is currently shaped into the hub text buffers.
    shaped_hover: Option<usize>,
    last_tick: Instant,
}

impl Gfx {
    pub fn new(window: Arc<Window>, cfg: &Config, geo: MenuGeometry) -> Gfx {
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
            cfg: cfg.clone(),
            geo,
            animator: Animator::new(),
            shaped_hover: None,
            last_tick: Instant::now(),
        };
        gfx.set_items(cfg, geo);
        gfx
    }

    /// Rebuild slot layout, icon textures, and fallback letters from config.
    pub fn set_items(&mut self, cfg: &Config, geo: MenuGeometry) {
        self.cfg = cfg.clone();
        self.geo = geo;
        let label_font_px = geo.label_font_px();

        // Metrics depend on label_font_px, which just changed (or is only now
        // known for the first time), so these get rebuilt from scratch here
        // rather than resized in place.
        self.hub_label_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(label_font_px * 1.15, label_font_px * 1.15 * 1.3),
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
            Metrics::new(label_font_px * 0.85, label_font_px * 0.85 * 1.3),
        );

        let total = geo.slot_count();
        self.animator.set_slot_count(total);

        let glyph_px = geo.glyph_px();
        let mut slots = Vec::with_capacity(total);
        for (k, name, icon, is_meta) in cfg
            .items
            .iter()
            .enumerate()
            .map(|(k, it)| (k, it.name.clone(), icons::icon_for(it), false))
            .chain(std::iter::once((cfg.items.len(), "Dodaj".to_string(), None, true)))
        {
            let angle = geo.slot_angle(k);
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
                Metrics::new(label_font_px, label_font_px * 1.3),
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
        self.animator.begin_open();
        self.last_tick = Instant::now();
    }

    /// Start the collective shrink+fade. Launching already happened; this is
    /// cosmetic. `launched` is the slot that fired, if any.
    pub fn begin_close(&mut self, launched: Option<usize>) {
        self.animator.begin_close(launched);
    }

    /// Advance the animation and draw one frame.
    pub fn tick_render(&mut self, hover: Option<usize>) -> Tick {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;

        let frame = self.animator.tick(dt, hover, &self.geo, &self.cfg.animation);
        if !frame.request_frame {
            return Tick { request_frame: false, just_closed: frame.just_closed };
        }

        // Hub text follows Hover; shaping needs the FontSystem, so it stays here.
        if frame.hovered != self.shaped_hover {
            let attrs = Attrs::new().family(Family::Monospace);
            match frame.hovered {
                Some(k) => {
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
                    self.hub_label_buf.set_text("\u{b7}", &attrs, Shaping::Advanced, None);
                    self.hub_label_buf.shape_until_scroll(&mut self.font_system, false);
                }
            }
            self.shaped_hover = frame.hovered;
        }

        self.draw(&frame);
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

    fn draw(&mut self, frame: &FrameModel) {
        let center = self.surface_cfg.width as f32 / 2.0;
        let hover = frame.hovered;
        let accent = self.cfg.appearance.accent_rgb();
        let opacity = self.cfg.appearance.opacity();
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

        let tile_half = self.geo.tile_half();
        let tile_corner = tile_half * TILE_CORNER_RATIO;
        let icon_inset = tile_half * ICON_INSET_RATIO;
        let label_font_px = self.geo.label_font_px();
        let n = self.geo.slot_count().max(1);
        let scrim_r = self.geo.scrim_r();
        let rest_r = self.geo.rest_r();
        let hub_r = self.geo.hub_r();

        // Per-slot animated values: (position, scale, alpha). Tiles always sit
        // at rest_r now — no more cursor-driven outward shift.
        let tiles: Vec<([f32; 2], f32, f32)> = self
            .slots
            .iter()
            .zip(&frame.slots)
            .map(|(slot, sf)| {
                let pos = [center + slot.angle.cos() * rest_r, center + slot.angle.sin() * rest_r];
                (pos, sf.scale, sf.alpha)
            })
            .collect();

        // --- shapes: scrim, hub, tiles, arc ---
        let scrim_scale = 0.85 + 0.15 * frame.scrim.max(0.0);
        let scrim_alpha = frame.scrim.clamp(0.0, 1.0);
        let mut shapes = vec![
            ShapeInstance {
                pos: [center, center],
                half: [scrim_r * scrim_scale, scrim_r * scrim_scale],
                corner: scrim_r * scrim_scale,
                border: 1.2,
                fill: self.col(scrim_bg, opacity * scrim_alpha),
                border_color: self.col(white, 0.06 * scrim_alpha),
                ..Default::default()
            },
            ShapeInstance {
                pos: [center, center],
                half: [hub_r, hub_r],
                corner: hub_r,
                border: 1.2,
                fill: self.col(hub_bg, scrim_alpha),
                border_color: self.col(
                    if hover.is_some() { accent } else { white },
                    (if hover.is_some() { 0.45 } else { 0.10 }) * scrim_alpha,
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
        if let Some((arc_angle, arc_alpha)) = frame.arc {
            shapes.push(ShapeInstance {
                pos: [center, center],
                half: [scrim_r + ARC_OFFSET, scrim_r + ARC_OFFSET],
                border: 1.75, // half of the spec's 3.5px stroke
                fill: self.col(accent, arc_alpha),
                kind: 1.0,
                angle_center: arc_angle,
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
                // Positioning ratios tuned against geo.glyph_px(), the same
                // size set_items shaped this buffer with.
                let glyph_px = self.geo.glyph_px();
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
            default_color: rgba(if hover.is_some() { accent } else { hub_dot }, scrim_alpha),
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
                default_color: rgba(idle_text, scrim_alpha),
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
