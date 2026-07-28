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
use crate::popover::{self, PopoverState};

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
    /// 0 = rounded box, 1 = arc stroke, 2 = circle segment (Gear zone).
    kind: f32,
    /// Arc: pointing angle, radians, same atan2 convention as `slot_angle`.
    /// Segment: chord y-offset from the shape center, px (+down).
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
    /// A remove pop finished: drop that Slot's Item now.
    pub remove_done: Option<usize>,
}

/// A Tile in flight between two Slots.
pub struct DragView {
    /// Item index picked up.
    pub from: usize,
    /// Item index it would land on if dropped now.
    pub to: usize,
    /// Cursor position, Menu-center-relative px.
    pub cursor: [f32; 2],
}

/// Everything main knows that the renderer needs this frame.
pub struct MenuView<'a> {
    pub hover: Option<usize>,
    pub gear_hover: bool,
    pub popover: Option<&'a PopoverState>,
    /// Pinned with no Popover open: Tiles carry remove controls and the Hub
    /// carries the Dodaj slot's toggle.
    pub editing: bool,
    /// Slot whose remove control is under the cursor.
    pub hover_remove: Option<usize>,
    pub hover_toggle: bool,
    /// The Done button (Hub's bottom segment) is under the cursor.
    pub hover_done: bool,
    /// What the toggle currently reads — the Dodaj slot is hidden.
    pub add_hidden: bool,
    /// A Tile is in flight: it follows the cursor instead of its angle, and the
    /// rest have already sprung to the order the drop would produce.
    pub drag: Option<DragView>,
}

/// Popover text buffers, alive only from begin_pin until the next begin_open.
struct PopBufs {
    name: TextBuffer,
    target: TextBuffer,
    lbl_name: TextBuffer,
    lbl_target: TextBuffer,
    browse: TextBuffer,
    icon_btn: TextBuffer,
    commit: TextBuffer,
    cancel: TextBuffer,
    /// Icon-preview fallback letter when there's no extractable icon.
    fallback: TextBuffer,
    /// PopoverState.generation last shaped into name/target.
    generation: u64,
}

/// Popover font sizes, px.
const POP_FIELD_PX: f32 = 14.0;
const POP_LABEL_PX: f32 = 11.0;
const POP_BTN_PX: f32 = 13.0;
const POP_FALLBACK_PX: f32 = 26.0;
/// Horizontal text padding inside a field.
const POP_FIELD_PAD: f32 = 9.0;
/// Caption on the Dodaj slot's toggle, px.
const TOGGLE_LABEL_PX: f32 = 11.0;

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
    /// Gear zone glyph (⚙) in the Hub's bottom segment.
    gear_buf: TextBuffer,
    /// The remove control's glyph. One buffer, drawn once per Tile — the text
    /// is identical, only the position differs.
    remove_buf: TextBuffer,
    /// Caption on the Dodaj slot's toggle, in the Hub while Pinned.
    toggle_buf: TextBuffer,
    /// The Done button's caption, in the Hub segment the gear glyph vacates.
    done_buf: TextBuffer,
    /// Popover text, created lazily when a Popover opens.
    pop: Option<PopBufs>,
    /// Popover icon-preview texture, when the current target yields one.
    pop_icon: Option<wgpu::BindGroup>,

    slots: Vec<Slot>,
    /// The full config, one shape everywhere; visual values read straight off it.
    cfg: Config,
    /// Shared Menu geometry; same-constructor copy of the one App holds.
    geo: MenuGeometry,
    animator: Animator,
    /// Hover whose Item name is currently shaped into the hub text buffers.
    shaped_hover: Option<usize>,
    last_tick: Instant,
    /// Fixed origin for wall-clock effects (caret blink).
    epoch: Instant,
}

/// Caret x-offset in a shaped single-line buffer, from glyph byte ranges.
fn caret_x(buf: &TextBuffer, caret: usize) -> f32 {
    let mut end_x = 0.0;
    for run in buf.layout_runs() {
        for g in run.glyphs {
            if g.start >= caret {
                return g.x;
            }
            end_x = g.x + g.w;
        }
    }
    end_x
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
        viewport.update(
            &queue,
            Resolution {
                width: size.width,
                height: size.height,
            },
        );
        let mut atlas = TextAtlas::new(&device, &queue, &glyph_cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        // Placeholder metrics: set_items (called at the end of this function)
        // recreates both from the configured label_font_px and seeds the idle
        // "." text — that value isn't known this early in construction.
        let hub_label_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 16.9));
        let hub_sub_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));
        let gear_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 13.0));
        let remove_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 13.0));
        let toggle_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));
        let done_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));

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
            gear_buf,
            remove_buf,
            toggle_buf,
            done_buf,
            pop: None,
            pop_icon: None,
            slots: Vec::new(),
            cfg: cfg.clone(),
            geo,
            animator: Animator::new(),
            shaped_hover: None,
            last_tick: Instant::now(),
            epoch: Instant::now(),
        };
        gfx.set_items(cfg, geo);
        gfx
    }

    /// Rebuild slot layout and fallback letters from config. Icons are not
    /// touched here — decoding happens off-thread and arrives later through
    /// `set_slot_icon`, so a Tile shows its letter until then.
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
        self.hub_label_buf
            .shape_until_scroll(&mut self.font_system, false);
        self.hub_sub_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(label_font_px * 0.85, label_font_px * 0.85 * 1.3),
        );
        // Gear zone glyph, sized to the Hub's bottom segment (0.4 * hub_r tall).
        // Named family: U+2699 must come from Segoe UI Symbol, not an emoji font.
        let gear_px = geo.hub_r() * 0.28;
        self.gear_buf = TextBuffer::new(&mut self.font_system, Metrics::new(gear_px, gear_px));
        self.gear_buf.set_text(
            "\u{2699}",
            &Attrs::new().family(Family::Name("Segoe UI Symbol")),
            Shaping::Advanced,
            None,
        );
        self.gear_buf
            .shape_until_scroll(&mut self.font_system, false);

        // Remove control and the Dodaj slot's toggle caption — Pinned-only
        // chrome, sized off the same geometry as everything else.
        let rm_px = geo.remove_r() * 1.5;
        self.remove_buf = TextBuffer::new(&mut self.font_system, Metrics::new(rm_px, rm_px));
        self.remove_buf.set_text(
            "\u{d7}",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        self.remove_buf
            .shape_until_scroll(&mut self.font_system, false);
        self.toggle_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(TOGGLE_LABEL_PX, TOGGLE_LABEL_PX * 1.3),
        );
        self.toggle_buf.set_text(
            "add hidden",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        self.toggle_buf
            .shape_until_scroll(&mut self.font_system, false);
        self.done_buf = TextBuffer::new(
            &mut self.font_system,
            Metrics::new(TOGGLE_LABEL_PX, TOGGLE_LABEL_PX * 1.3),
        );
        self.done_buf.set_text(
            "done",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        self.done_buf
            .shape_until_scroll(&mut self.font_system, false);

        let total = geo.slot_count();
        self.animator.set_slot_count(total);

        let glyph_px = geo.glyph_px();
        let mut slots = Vec::with_capacity(total);
        // Slot order comes from the geometry, not from the Item list: the
        // Dodaj slot can sit first, last, or nowhere at all.
        for k in 0..total {
            let (name, is_meta) = match geo.item_at(k) {
                Some(i) => (cfg.items[i].name.clone(), false),
                None => ("Dodaj".to_string(), true),
            };
            let letter = {
                let ch = if is_meta {
                    "+".to_string()
                } else {
                    name.chars()
                        .next()
                        .unwrap_or('?')
                        .to_uppercase()
                        .to_string()
                };
                let mut buf =
                    TextBuffer::new(&mut self.font_system, Metrics::new(glyph_px, glyph_px));
                buf.set_text(
                    &ch,
                    &Attrs::new().family(Family::Monospace),
                    Shaping::Advanced,
                    None,
                );
                buf.shape_until_scroll(&mut self.font_system, false);
                Some(buf)
            };
            let mut label_buf = TextBuffer::new(
                &mut self.font_system,
                Metrics::new(label_font_px, label_font_px * 1.3),
            );
            label_buf.set_text(
                &name,
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            label_buf.shape_until_scroll(&mut self.font_system, false);
            slots.push(Slot {
                label: name,
                icon: None,
                letter,
                label_buf,
                is_meta,
            });
        }
        self.slots = slots;
    }

    /// A decode finished: upload it and let the Tile stop drawing its letter.
    /// Called from the event loop, which owns the Device and Queue.
    pub fn set_slot_icon(&mut self, k: usize, icon: &icons::RgbaIcon) {
        let bind = self.upload_icon(icon);
        if let Some(slot) = self.slots.get_mut(k) {
            slot.icon = Some(bind);
        }
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

    /// A Popover opened: build its text buffers and start its spring. Pinned
    /// itself needs nothing from here — entering it draws no new resources.
    pub fn open_popover(&mut self, editing: bool) {
        let fs = &mut self.font_system;
        let attrs = Attrs::new().family(Family::Monospace);
        let mk = |fs: &mut FontSystem, px: f32, text: &str| {
            let mut b = TextBuffer::new(fs, Metrics::new(px, px * 1.3));
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(fs, false);
            b
        };
        self.pop = Some(PopBufs {
            name: mk(fs, POP_FIELD_PX, ""),
            target: mk(fs, POP_FIELD_PX, ""),
            lbl_name: mk(fs, POP_LABEL_PX, "nazwa"),
            lbl_target: mk(fs, POP_LABEL_PX, "cel"),
            browse: mk(fs, POP_BTN_PX, "\u{2026}"),
            icon_btn: mk(fs, POP_BTN_PX, "ikona\u{2026}"),
            commit: mk(fs, POP_BTN_PX, if editing { "zapisz" } else { "dodaj" }),
            cancel: mk(fs, POP_BTN_PX, "anuluj"),
            fallback: mk(fs, POP_FALLBACK_PX, "?"),
            generation: u64::MAX, // force the first reshape
        });
        self.pop_icon = None;
        self.animator.open_popover();
    }

    /// The Popover closed: collapse its spring and free its text/icon resources
    /// now, rather than holding them until the next begin_open.
    pub fn close_popover(&mut self) {
        self.animator.close_popover();
        self.pop = None;
        self.pop_icon = None;
    }

    /// Start the remove pop on a Slot. The Item is dropped only once
    /// `Tick::remove_done` reports the pop finished.
    pub fn begin_remove(&mut self, slot: usize) {
        self.animator.begin_remove(slot);
    }

    /// The Slot is going away: take its springs with it, so the Tiles after it
    /// keep their own animation state instead of inheriting the popped one's.
    pub fn drop_slot(&mut self, slot: usize) {
        self.animator.drop_slot(slot);
    }

    /// A drag committed: carry the Tile's springs to its new Slot so it stays
    /// where the drag left it on screen instead of snapping.
    pub fn reorder_slots(&mut self, from: usize, to: usize) {
        self.animator.reorder_slots(from, to);
    }

    /// Icon preview for the Popover's current target: a texture when one has
    /// been decoded, else a fallback letter.
    pub fn set_popover_icon(&mut self, icon: Option<&icons::RgbaIcon>, fallback: char) {
        self.pop_icon = icon.map(|i| self.upload_icon(i));
        self.set_popover_fallback(fallback);
    }

    /// Just the letter — the name field changed but the icon didn't, so there
    /// is no reason to re-upload the texture.
    pub fn set_popover_fallback(&mut self, fallback: char) {
        if let Some(pop) = &mut self.pop {
            let ch: String = fallback.to_uppercase().collect();
            pop.fallback.set_text(
                &ch,
                &Attrs::new().family(Family::Monospace),
                Shaping::Advanced,
                None,
            );
            pop.fallback
                .shape_until_scroll(&mut self.font_system, false);
        }
    }

    /// Advance the animation and draw one frame.
    pub fn tick_render(&mut self, view: &MenuView) -> Tick {
        let now = Instant::now();
        let dt = (now - self.last_tick).as_secs_f32().min(0.05);
        self.last_tick = now;

        let angles = self.target_angles(view.drag.as_ref());
        let frame = self
            .animator
            .tick(dt, view.hover, &self.geo, &self.cfg.animation, &angles);
        if !frame.request_frame {
            return Tick {
                request_frame: false,
                just_closed: frame.just_closed,
                remove_done: frame.remove_done,
            };
        }

        // Popover field text follows the editing state; reshape only on change.
        if let Some(ps) = view.popover
            && let Some(pop) = &mut self.pop
            && pop.generation != ps.generation
        {
            let attrs = Attrs::new().family(Family::Monospace);
            pop.name
                .set_text(&ps.name.text, &attrs, Shaping::Advanced, None);
            pop.name.shape_until_scroll(&mut self.font_system, false);
            pop.target
                .set_text(&ps.target.text, &attrs, Shaping::Advanced, None);
            pop.target.shape_until_scroll(&mut self.font_system, false);
            pop.generation = ps.generation;
        }

        // Hub text follows Hover; shaping needs the FontSystem, so it stays here.
        if frame.hovered != self.shaped_hover {
            let attrs = Attrs::new().family(Family::Monospace);
            match frame.hovered {
                Some(k) => {
                    let name = self.slots[k].label.to_lowercase();
                    self.hub_label_buf
                        .set_text(&name, &attrs, Shaping::Advanced, None);
                    self.hub_label_buf
                        .shape_until_scroll(&mut self.font_system, false);
                    self.hub_sub_buf.set_text(
                        "puść, aby uruchomić",
                        &attrs,
                        Shaping::Advanced,
                        None,
                    );
                    self.hub_sub_buf
                        .shape_until_scroll(&mut self.font_system, false);
                }
                None => {
                    self.hub_label_buf
                        .set_text("\u{b7}", &attrs, Shaping::Advanced, None);
                    self.hub_label_buf
                        .shape_until_scroll(&mut self.font_system, false);
                }
            }
            self.shaped_hover = frame.hovered;
        }

        self.draw(&frame, view);
        Tick {
            request_frame: true,
            just_closed: false,
            remove_done: frame.remove_done,
        }
    }

    /// Where each Tile belongs this frame. The resting angles, unless a drag is
    /// in flight — then every other Tile has already moved to the arrangement
    /// the drop would produce, so the preview cannot disagree with the result.
    fn target_angles(&self, drag: Option<&DragView>) -> Vec<f32> {
        let g = &self.geo;
        let mut out: Vec<f32> = (0..g.slot_count()).map(|k| g.slot_angle(k)).collect();
        if let Some(d) = drag {
            for i in 0..g.item_count() {
                let moved = crate::geometry::moved_index(i, d.from, d.to);
                out[g.slot_of_item(i)] = g.slot_angle(g.slot_of_item(moved));
            }
        }
        out
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
        self.viewport
            .update(&self.queue, Resolution { width, height });
    }

    /// sRGB component -> linear, when the surface format demands linear input.
    fn col(&self, c: [f32; 3], a: f32) -> [f32; 4] {
        if self.srgb {
            let f = |v: f32| {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            [f(c[0]), f(c[1]), f(c[2]), a]
        } else {
            [c[0], c[1], c[2], a]
        }
    }

    fn draw(&mut self, frame: &FrameModel, view: &MenuView) {
        let center = self.surface_cfg.width as f32 / 2.0;
        let hover = frame.hovered;
        // Popover expand progress; the meta Tile morphs into the panel, so it
        // stops drawing as itself the moment the popover exists.
        let pop_p = if view.popover.is_some() {
            frame.popover
        } else {
            0.0
        };
        let pop_active = view.popover.is_some() && frame.popover > 0.001;
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
        // While Pinned the Hub has to hold the Dodaj slot's toggle, so it draws
        // at its floored radius. Outside Pinned this is the plain configured
        // value — the Dead zone and Gear zone must not move (see hub_r_pinned).
        let hub_r = if view.editing {
            self.geo.hub_r_pinned()
        } else {
            self.geo.hub_r()
        };

        // Per-slot animated values: (position, scale, alpha). Tiles sit at
        // rest_r on their sprung angle — except the one being dragged, which
        // rides the cursor.
        let dragged_slot = view
            .drag
            .as_ref()
            .map(|d| self.geo.slot_of_item(d.from));
        let tiles: Vec<([f32; 2], f32, f32)> = frame
            .slots
            .iter()
            .enumerate()
            .map(|(k, sf)| {
                let pos = match (&view.drag, dragged_slot == Some(k)) {
                    (Some(d), true) => [center + d.cursor[0], center + d.cursor[1]],
                    _ => [
                        center + sf.angle.cos() * rest_r,
                        center + sf.angle.sin() * rest_r,
                    ],
                };
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
        // Gear zone: the Hub's bottom segment; release there enters Pinned.
        // Once Pinned, the Hub belongs to the toggle instead.
        if !view.editing {
            shapes.push(ShapeInstance {
                pos: [center, center],
                half: [hub_r, hub_r],
                fill: self.col(
                    if view.gear_hover { accent } else { white },
                    (if view.gear_hover { 0.14 } else { 0.05 }) * scrim_alpha,
                ),
                kind: 2.0,
                angle_center: self.geo.gear_cut_dy(),
                ..Default::default()
            });
        }
        for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
            if scale < 0.01 || alpha < 0.01 || (self.slots[k].is_meta && pop_active) {
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
        // --- Pinned chrome: a remove control per Item Tile, and the Dodaj
        // slot's toggle in the Hub. Both are inert while a Popover is open,
        // and drawing them then would only compete with the panel. ---
        let remove_r = self.geo.remove_r();
        let danger = [0.878, 0.353, 0.353]; // #E05A5A
        if view.editing && !pop_active {
            for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
                if self.slots.get(k).is_none_or(|s| s.is_meta) || scale < 0.01 || alpha < 0.01 {
                    continue;
                }
                let hovered = view.hover_remove == Some(k);
                shapes.push(ShapeInstance {
                    pos: [
                        pos[0] + tile_half * 0.85 * scale,
                        pos[1] - tile_half * 0.85 * scale,
                    ],
                    half: [remove_r, remove_r],
                    corner: remove_r,
                    border: 1.1,
                    fill: self.col(if hovered { danger } else { hub_bg }, 0.95 * alpha),
                    border_color: self.col(if hovered { danger } else { white }, 0.35 * alpha),
                    ..Default::default()
                });
            }
            // Toggle: track + knob, the knob sliding to the "on" end when the
            // Dodaj slot is hidden.
            let (track_w, track_h) = (30.0, 16.0);
            let track_cy = center + 9.0;
            let on = view.add_hidden;
            shapes.push(ShapeInstance {
                pos: [center, track_cy],
                half: [track_w / 2.0, track_h / 2.0],
                corner: track_h / 2.0,
                border: 1.1,
                fill: self.col(
                    if on { accent } else { white },
                    (if on { 0.75 } else { 0.08 }) * scrim_alpha,
                ),
                border_color: self.col(
                    if view.hover_toggle { accent } else { white },
                    (if view.hover_toggle { 0.7 } else { 0.18 }) * scrim_alpha,
                ),
                ..Default::default()
            });
            let knob_r = track_h / 2.0 - 2.5;
            shapes.push(ShapeInstance {
                pos: [
                    center + if on { track_w / 4.0 } else { -track_w / 4.0 },
                    track_cy,
                ],
                half: [knob_r, knob_r],
                corner: knob_r,
                fill: self.col(if on { hub_bg } else { white }, 0.9 * scrim_alpha),
                ..Default::default()
            });
            // Done: the way out, in the same segment the Gear zone (the way in)
            // occupies outside Pinned.
            shapes.push(ShapeInstance {
                pos: [center, center],
                half: [hub_r, hub_r],
                fill: self.col(
                    if view.hover_done { accent } else { white },
                    (if view.hover_done { 0.18 } else { 0.06 }) * scrim_alpha,
                ),
                kind: 2.0,
                angle_center: self.geo.done_cut_dy(),
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

        // --- Popover: panel morphing out of the Dodaj Tile + form widgets ---
        // Content fades in over the last stretch of the expansion.
        let pop_content_a = ((pop_p - 0.6) / 0.4).clamp(0.0, 1.0) * scrim_alpha;
        let to_win = |r: &popover::Rect| [center + r.center[0], center + r.center[1]];
        if pop_active {
            let ps = view.popover.unwrap();
            let lp = |a: f32, b: f32| a + (b - a) * pop_p;
            let panel = ps.layout.panel;
            shapes.push(ShapeInstance {
                pos: [
                    center + lp(ps.origin[0], panel.center[0]),
                    center + lp(ps.origin[1], panel.center[1]),
                ],
                half: [lp(tile_half, panel.half[0]), lp(tile_half, panel.half[1])],
                corner: lp(tile_corner, 12.0),
                border: 1.1,
                fill: self.col(hub_bg, 0.97 * scrim_alpha),
                border_color: self.col(white, 0.10 * scrim_alpha),
                ..Default::default()
            });
            if pop_content_a > 0.01 {
                let field = |r: &popover::Rect, focused: bool| ShapeInstance {
                    pos: to_win(r),
                    half: r.half,
                    corner: 8.0,
                    border: 1.1,
                    fill: self.col(white, 0.05 * pop_content_a),
                    border_color: self.col(
                        if focused { accent } else { white },
                        (if focused { 0.85 } else { 0.12 }) * pop_content_a,
                    ),
                    ..Default::default()
                };
                use popover::Element as El;
                shapes.push(field(&ps.layout.name_field, ps.focus == El::NameField));
                shapes.push(field(&ps.layout.target_field, ps.focus == El::TargetField));
                let button = |r: &popover::Rect, hovered: bool| ShapeInstance {
                    pos: to_win(r),
                    half: r.half,
                    corner: 8.0,
                    border: 1.1,
                    fill: self.col(white, (if hovered { 0.11 } else { 0.06 }) * pop_content_a),
                    border_color: self.col(
                        if hovered { accent } else { white },
                        (if hovered { 0.55 } else { 0.12 }) * pop_content_a,
                    ),
                    ..Default::default()
                };
                shapes.push(button(&ps.layout.browse_btn, ps.hover == Some(El::Browse)));
                shapes.push(button(&ps.layout.icon_btn, ps.hover == Some(El::IconBtn)));
                shapes.push(button(&ps.layout.cancel_btn, ps.hover == Some(El::Cancel)));
                // Commit: accent when valid, muted when the target is empty.
                let valid = ps.valid();
                shapes.push(ShapeInstance {
                    pos: to_win(&ps.layout.commit_btn),
                    half: ps.layout.commit_btn.half,
                    corner: 8.0,
                    border: 1.1,
                    fill: self.col(
                        if valid { accent } else { white },
                        (if valid { 0.9 } else { 0.05 }) * pop_content_a,
                    ),
                    border_color: self.col(
                        accent,
                        (if valid {
                            1.0
                        } else if ps.hover == Some(El::Commit) {
                            0.35
                        } else {
                            0.0
                        }) * pop_content_a,
                    ),
                    ..Default::default()
                });
                // Icon preview well.
                shapes.push(ShapeInstance {
                    pos: to_win(&ps.layout.icon_preview),
                    half: ps.layout.icon_preview.half,
                    corner: 8.0,
                    border: 1.1,
                    fill: self.col(white, 0.05 * pop_content_a),
                    border_color: self.col(white, 0.10 * pop_content_a),
                    ..Default::default()
                });
                // Caret in the focused field, 1s blink cycle.
                let blink_on = self.epoch.elapsed().as_millis() % 1000 < 500;
                if blink_on && let Some(pop) = &self.pop {
                    let (rect, buf, caret) = if ps.focus == El::TargetField {
                        (&ps.layout.target_field, &pop.target, ps.target.caret)
                    } else {
                        (&ps.layout.name_field, &pop.name, ps.name.caret)
                    };
                    let cx = caret_x(buf, caret);
                    let inner_w = rect.half[0] * 2.0 - 2.0 * POP_FIELD_PAD;
                    let scroll = (inner_w - 2.0 - cx).min(0.0);
                    let x = center + rect.left() + POP_FIELD_PAD + scroll + cx;
                    shapes.push(ShapeInstance {
                        pos: [x, center + rect.center[1]],
                        half: [0.75, rect.half[1] - 7.0],
                        fill: self.col(accent, 0.95 * pop_content_a),
                        ..Default::default()
                    });
                }
            }
        }

        let shape_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("shapes"),
                contents: bytemuck::cast_slice(&shapes),
                usage: wgpu::BufferUsages::VERTEX,
            });

        // --- icon quads ---
        let mut tex_instances: Vec<TexInstance> = tiles
            .iter()
            .map(|&(pos, scale, alpha)| TexInstance {
                pos,
                half: [
                    (tile_half - icon_inset) * scale,
                    (tile_half - icon_inset) * scale,
                ],
                alpha,
            })
            .collect();
        // Popover icon preview rides at the end, drawn with its own bind group.
        let pop_icon_instance = (pop_active && pop_content_a > 0.01 && self.pop_icon.is_some())
            .then(|| {
                let r = &view.popover.unwrap().layout.icon_preview;
                tex_instances.push(TexInstance {
                    pos: to_win(r),
                    half: [r.half[0] - 6.0, r.half[1] - 6.0],
                    alpha: pop_content_a,
                });
                tex_instances.len() - 1
            });
        let tex_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
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
            if scale < 0.01 || alpha < 0.01 || (slot.is_meta && pop_active) {
                continue;
            }
            // The letter is what a Tile shows until its icon has been decoded,
            // and what it keeps forever if there is no icon to be had.
            if slot.icon.is_none()
                && let Some(letter) = &slot.letter
            {
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
            let label_w = slot
                .label_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
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
        // The Hub's idle dot / selected name, and the gear glyph, belong to the
        // press-and-hold Menu. While Pinned the Hub is the toggle's.
        if !view.editing {
            let name_w = self
                .hub_label_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
            areas.push(TextArea {
                buffer: &self.hub_label_buf,
                left: center - name_w / 2.0,
                top: center - label_font_px * 0.7,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(if hover.is_some() { accent } else { hub_dot }, scrim_alpha),
                custom_glyphs: &[],
            });
        }
        if hover.is_some() && !view.editing {
            let sub_w = self
                .hub_sub_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
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
        // Gear glyph, centered in the Hub's bottom segment.
        if !view.editing {
            let gear_w = self
                .gear_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
            let gear_px = self.geo.hub_r() * 0.28;
            let seg_cy = center + (self.geo.gear_cut_dy() + hub_r) / 2.0;
            areas.push(TextArea {
                buffer: &self.gear_buf,
                left: center - gear_w / 2.0,
                top: seg_cy - gear_px * 0.62,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(if view.gear_hover { accent } else { hub_dot }, scrim_alpha),
                custom_glyphs: &[],
            });
        } else if !pop_active {
            // Toggle caption, above the switch.
            let w = self
                .toggle_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
            areas.push(TextArea {
                buffer: &self.toggle_buf,
                left: center - w / 2.0,
                top: center - TOGGLE_LABEL_PX * 1.5,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(
                    if view.hover_toggle { accent } else { idle_text },
                    scrim_alpha,
                ),
                custom_glyphs: &[],
            });
            // Done caption, centered in the Hub's bottom segment.
            let done_w = self
                .done_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
            let seg_cy = center + (self.geo.done_cut_dy() + hub_r) / 2.0;
            areas.push(TextArea {
                buffer: &self.done_buf,
                left: center - done_w / 2.0,
                top: seg_cy - TOGGLE_LABEL_PX * 0.62,
                scale: 1.0,
                bounds: full_bounds,
                default_color: rgba(
                    if view.hover_done { accent } else { idle_text },
                    scrim_alpha,
                ),
                custom_glyphs: &[],
            });
            // One glyph buffer, drawn once per removable Tile.
            let rm_w = self
                .remove_buf
                .layout_runs()
                .map(|r| r.line_w)
                .fold(0.0f32, f32::max);
            for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
                if self.slots.get(k).is_none_or(|s| s.is_meta) || scale < 0.01 || alpha < 0.01 {
                    continue;
                }
                areas.push(TextArea {
                    buffer: &self.remove_buf,
                    left: pos[0] + tile_half * 0.85 * scale - rm_w / 2.0,
                    top: pos[1] - tile_half * 0.85 * scale - remove_r * 0.95,
                    scale: 1.0,
                    bounds: full_bounds,
                    default_color: rgba(
                        if view.hover_remove == Some(k) {
                            white
                        } else {
                            idle_text
                        },
                        alpha,
                    ),
                    custom_glyphs: &[],
                });
            }
        }
        // Popover text: labels, field contents (clipped + caret-scrolled),
        // button captions, and the icon-preview fallback letter.
        if pop_active
            && pop_content_a > 0.01
            && let (Some(ps), Some(pop)) = (view.popover, &self.pop)
        {
            use popover::Element as El;
            let a = pop_content_a;
            let text_c = [0.92, 0.94, 0.96];
            let width_of =
                |buf: &TextBuffer| buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max);
            for (buf, rect) in [
                (&pop.lbl_name, &ps.layout.name_field),
                (&pop.lbl_target, &ps.layout.target_field),
            ] {
                areas.push(TextArea {
                    buffer: buf,
                    left: center + rect.left() + 2.0,
                    top: center + rect.top() - POP_LABEL_PX * 1.55,
                    scale: 1.0,
                    bounds: full_bounds,
                    default_color: rgba(idle_text, a),
                    custom_glyphs: &[],
                });
            }
            for (buf, rect, caret, focused) in [
                (
                    &pop.name,
                    &ps.layout.name_field,
                    ps.name.caret,
                    ps.focus == El::NameField,
                ),
                (
                    &pop.target,
                    &ps.layout.target_field,
                    ps.target.caret,
                    ps.focus == El::TargetField,
                ),
            ] {
                let inner_w = rect.half[0] * 2.0 - 2.0 * POP_FIELD_PAD;
                let scroll = if focused {
                    (inner_w - 2.0 - caret_x(buf, caret)).min(0.0)
                } else {
                    0.0
                };
                areas.push(TextArea {
                    buffer: buf,
                    left: center + rect.left() + POP_FIELD_PAD + scroll,
                    top: center + rect.center[1] - POP_FIELD_PX * 0.62,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: (center + rect.left() + 3.0) as i32,
                        top: (center + rect.top()) as i32,
                        right: (center + rect.left() + rect.half[0] * 2.0 - 3.0) as i32,
                        bottom: (center + rect.top() + rect.half[1] * 2.0) as i32,
                    },
                    default_color: rgba(text_c, a),
                    custom_glyphs: &[],
                });
            }
            let valid = ps.valid();
            let buttons: [(&TextBuffer, &popover::Rect, [f32; 3]); 4] = [
                (&pop.browse, &ps.layout.browse_btn, text_c),
                (&pop.icon_btn, &ps.layout.icon_btn, text_c),
                (&pop.cancel, &ps.layout.cancel_btn, idle_text),
                // Dark caption on the accent-filled commit; muted when disabled.
                (
                    &pop.commit,
                    &ps.layout.commit_btn,
                    if valid { [0.06, 0.07, 0.09] } else { idle_text },
                ),
            ];
            for (buf, rect, color) in buttons {
                areas.push(TextArea {
                    buffer: buf,
                    left: center + rect.center[0] - width_of(buf) / 2.0,
                    top: center + rect.center[1] - POP_BTN_PX * 0.62,
                    scale: 1.0,
                    bounds: full_bounds,
                    default_color: rgba(color, a),
                    custom_glyphs: &[],
                });
            }
            if self.pop_icon.is_none() {
                let rect = &ps.layout.icon_preview;
                areas.push(TextArea {
                    buffer: &pop.fallback,
                    left: center + rect.center[0] - width_of(&pop.fallback) / 2.0,
                    top: center + rect.center[1] - POP_FALLBACK_PX * 0.55,
                    scale: 1.0,
                    bounds: full_bounds,
                    default_color: rgba(idle_text, a),
                    custom_glyphs: &[],
                });
            }
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
            if let (Some(i), Some(bind)) = (pop_icon_instance, &self.pop_icon) {
                pass.set_bind_group(1, bind, &[]);
                pass.draw(0..6, i as u32..i as u32 + 1);
            }

            if let Err(e) = self
                .text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
            {
                eprintln!("sideQM: text render failed: {e}");
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        self.atlas.trim();
    }
}
