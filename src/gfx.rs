//! CPU rendering: tiny-skia rasterizes the Menu into a premultiplied RGBA
//! pixmap, cosmic-text shapes and rasterizes every string into it, and
//! `present::Layered` hands the finished frame to DWM.
//!
//! ADR-0007: nothing in this module touches the GPU. What used to be an SDF
//! shader over three primitive kinds is now three path builders, and what used
//! to be a glyph atlas on the GPU is a per-glyph blit.

use std::f32::consts::TAU;
use std::time::Instant;

use cosmic_text::{
    Attrs, Buffer as TextBuffer, Color as TextColor, Family, FontSystem, Metrics, Shaping,
    SwashCache,
};
use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, LineCap, Paint, Path, PathBuilder, Pixmap,
    PixmapPaint, PremultipliedColorU8, Rect, Stroke, StrokeDash, Transform,
};
use windows::Win32::Foundation::HWND;

use crate::anim::{Animator, FrameModel};
use crate::config::Config;
use crate::geometry::MenuGeometry;
use crate::icons;
use crate::popover::{self, PopoverState};
use crate::present::{self, Layered};

/// Icon inset and tile corner radius, as ratios of tile half-extent, so they
/// keep their proportions under a configurable tile size instead of drifting.
const ICON_INSET_RATIO: f32 = 10.0 / 32.0;
const TILE_CORNER_RATIO: f32 = 18.0 / 32.0;
/// How far outside the scrim the arc indicator rides.
const ARC_OFFSET: f32 = 6.0;
/// Arc half-width, as a fraction of one slot's angular width.
const ARC_HALF_FRAC: f32 = 0.4;
/// Dash periods around the Dodaj tile's border.
const DODAJ_DASHES: f32 = 10.0;
/// Longest chord used when flattening a curve into line segments. Below half a
/// pixel the difference stops being representable after anti-aliasing.
const FLATTEN_CHORD: f32 = 1.2;

struct Slot {
    label: String,
    /// Decoded icon, premultiplied and ready to blit.
    icon: Option<Pixmap>,
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
    /// The frame under construction: premultiplied RGBA, window-sized.
    pixmap: Pixmap,
    /// None only if GDI refused the DIB; the Menu then renders to nothing
    /// rather than taking the process down.
    layered: Option<Layered>,
    hwnd: HWND,

    font_system: FontSystem,
    swash: SwashCache,
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
    /// Popover icon preview, when the current target yields one.
    pop_icon: Option<Pixmap>,

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

fn width_of(buf: &TextBuffer) -> f32 {
    buf.layout_runs().map(|r| r.line_w).fold(0.0f32, f32::max)
}

// --- shapes -------------------------------------------------------------
// Three path builders replacing the three SDF branches the shader used to
// carry. Curves are flattened into line segments rather than approximated
// with cubics: at these radii the chord error is far below the anti-aliased
// coverage the rasterizer produces anyway.

/// A rounded box. Corner radius equal to both half-extents makes a circle,
/// which is how the Scrim, the Hub and the toggle knob get drawn.
fn round_rect(cx: f32, cy: f32, hw: f32, hh: f32, corner: f32) -> Option<Path> {
    let hw = hw.max(0.01);
    let hh = hh.max(0.01);
    let r = corner.clamp(0.0, hw.min(hh));
    let (l, t, right, b) = (cx - hw, cy - hh, cx + hw, cy + hh);
    let mut pb = PathBuilder::new();
    if r <= 0.01 {
        pb.push_rect(Rect::from_ltrb(l, t, right, b)?);
        return pb.finish();
    }
    // 4/3 * (sqrt(2) - 1): the standard cubic approximation of a quarter
    // circle, off by at most 0.02% of the radius.
    const K: f32 = 0.552_284_75;
    let k = r * K;
    pb.move_to(l + r, t);
    pb.line_to(right - r, t);
    pb.cubic_to(right - r + k, t, right, t + r - k, right, t + r);
    pb.line_to(right, b - r);
    pb.cubic_to(right, b - r + k, right - r + k, b, right - r, b);
    pb.line_to(l + r, b);
    pb.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    pb.line_to(l, t + r);
    pb.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    pb.close();
    pb.finish()
}

/// The arc indicator's centerline, to be stroked with round caps — which is
/// what the SDF got for free by clamping the angle.
fn arc_path(cx: f32, cy: f32, r: f32, center: f32, half: f32) -> Option<Path> {
    let sweep = (half * 2.0).abs();
    let steps = ((sweep * r / FLATTEN_CHORD).ceil() as usize).clamp(2, 1024);
    let mut pb = PathBuilder::new();
    for i in 0..=steps {
        let a = center - half + sweep * (i as f32 / steps as f32);
        let (x, y) = (cx + a.cos() * r, cy + a.sin() * r);
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.finish()
}

/// A disc of radius `r` cut by a horizontal chord at `dy` below the center,
/// keeping the part below the chord: the Gear zone and the Done button.
fn segment_path(cx: f32, cy: f32, r: f32, dy: f32) -> Option<Path> {
    if dy >= r {
        return None; // the chord has slid past the disc; nothing is left
    }
    let half_w = (r * r - dy * dy).max(0.0).sqrt();
    // Angles of the two chord intersections, in the same y-down convention the
    // rest of the Menu uses. Sweeping between them passes through the bottom.
    let a0 = dy.atan2(half_w);
    let a1 = std::f32::consts::PI - a0;
    let sweep = a1 - a0;
    let steps = ((sweep * r / FLATTEN_CHORD).ceil() as usize).clamp(2, 1024);
    let mut pb = PathBuilder::new();
    pb.move_to(cx + half_w, cy + dy);
    for i in 1..=steps {
        let a = a0 + sweep * (i as f32 / steps as f32);
        pb.line_to(cx + a.cos() * r, cy + a.sin() * r);
    }
    pb.close();
    pb.finish()
}

fn paint_of(rgb: [f32; 3], alpha: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.blend_mode = BlendMode::SourceOver;
    p.set_color(
        Color::from_rgba(
            rgb[0].clamp(0.0, 1.0),
            rgb[1].clamp(0.0, 1.0),
            rgb[2].clamp(0.0, 1.0),
            alpha.clamp(0.0, 1.0),
        )
        .unwrap_or(Color::TRANSPARENT),
    );
    p
}

/// Fill, then an inset border stroke. Every `kind 0` shape the old shader drew
/// arrives here; the border is inset by half its width so it sits inside the
/// silhouette exactly as the SDF's distance band did.
#[allow(clippy::too_many_arguments)]
fn box_shape(
    dst: &mut Pixmap,
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    corner: f32,
    fill: ([f32; 3], f32),
    border: Option<(f32, [f32; 3], f32)>,
    dashes: f32,
) {
    if fill.1 > 0.002
        && let Some(path) = round_rect(cx, cy, hw, hh, corner)
    {
        dst.fill_path(
            &path,
            &paint_of(fill.0, fill.1),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    let Some((width, rgb, alpha)) = border else {
        return;
    };
    if width <= 0.0 || alpha <= 0.002 {
        return;
    }
    let inset = width / 2.0;
    let (ihw, ihh) = (hw - inset, hh - inset);
    let icorner = (corner - inset).max(0.0);
    let Some(path) = round_rect(cx, cy, ihw, ihh, icorner) else {
        return;
    };
    let mut stroke = Stroke {
        width,
        line_cap: LineCap::Butt,
        ..Default::default()
    };
    if dashes > 0.5 {
        // Arc-length dashes around the actual perimeter. The shader spaced
        // these by angle instead, which bunched them up at the corners.
        let perim = 4.0 * (ihw - icorner).max(0.0) + 4.0 * (ihh - icorner).max(0.0) + TAU * icorner;
        let seg = perim / (dashes * 2.0);
        if seg > 0.05 {
            stroke.dash = StrokeDash::new(vec![seg, seg], 0.0);
        }
    }
    dst.stroke_path(
        &path,
        &paint_of(rgb, alpha),
        &stroke,
        Transform::identity(),
        None,
    );
}

// --- text ---------------------------------------------------------------

/// Source-over one coverage sample into a premultiplied destination pixel.
/// The only hand-written blending in the renderer — every shape goes through
/// tiny-skia, but glyph coverage arrives one pixel at a time from swash.
#[inline]
fn blend_over(dst: &mut PremultipliedColorU8, sr: f32, sg: f32, sb: f32, sa: f32) {
    let inv = 1.0 - sa;
    let to_u8 = |v: f32| (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
    let a = to_u8(sa + (dst.alpha() as f32 / 255.0) * inv);
    // Clamp each channel to alpha: rounding can otherwise push a component one
    // step above it, which is not a representable premultiplied color.
    let r = to_u8(sr + (dst.red() as f32 / 255.0) * inv).min(a);
    let g = to_u8(sg + (dst.green() as f32 / 255.0) * inv).min(a);
    let b = to_u8(sb + (dst.blue() as f32 / 255.0) * inv).min(a);
    *dst = PremultipliedColorU8::from_rgba(r, g, b, a).unwrap_or(*dst);
}

/// Rasterize one shaped buffer into the pixmap.
///
/// Positioning mirrors glyphon's exactly — `line_y` scaled and rounded on its
/// own, everything else folded into the physical glyph — so every offset that
/// `draw` tuned against the old renderer still lands on the same pixel.
#[allow(clippy::too_many_arguments)]
fn blit_text(
    dst: &mut Pixmap,
    fs: &mut FontSystem,
    swash: &mut SwashCache,
    buf: &TextBuffer,
    left: f32,
    top: f32,
    scale: f32,
    rgb: [f32; 3],
    alpha: f32,
    clip: Option<[i32; 4]>,
) {
    let alpha = alpha.clamp(0.0, 1.0);
    if alpha <= 0.004 {
        return;
    }
    let (w, h) = (dst.width() as i32, dst.height() as i32);
    let [cl, ct, cr, cb] = clip.unwrap_or([0, 0, w, h]);
    let (cl, ct, cr, cb) = (cl.max(0), ct.max(0), cr.min(w), cb.min(h));
    if cl >= cr || ct >= cb {
        return;
    }
    let base = TextColor::rgb(
        (rgb[0].clamp(0.0, 1.0) * 255.0) as u8,
        (rgb[1].clamp(0.0, 1.0) * 255.0) as u8,
        (rgb[2].clamp(0.0, 1.0) * 255.0) as u8,
    );
    let stride = dst.width() as usize;
    let pixels = dst.pixels_mut();
    for run in buf.layout_runs() {
        let line_y = (run.line_y * scale).round() as i32;
        for glyph in run.glyphs {
            let pg = glyph.physical((left, top), scale);
            // swash hands back coverage in the alpha channel and leaves the
            // requested alpha unapplied, so it is folded in here.
            swash.with_pixels(fs, pg.cache_key, base, |gx, gy, c| {
                let x = pg.x + gx;
                let y = line_y + pg.y + gy;
                if x < cl || x >= cr || y < ct || y >= cb {
                    return;
                }
                let sa = (c.a() as f32 / 255.0) * alpha;
                if sa <= 0.002 {
                    return;
                }
                let idx = y as usize * stride + x as usize;
                let Some(px) = pixels.get_mut(idx) else {
                    return;
                };
                blend_over(
                    px,
                    (c.r() as f32 / 255.0) * sa,
                    (c.g() as f32 / 255.0) * sa,
                    (c.b() as f32 / 255.0) * sa,
                    sa,
                );
            });
        }
    }
}

/// A decoded icon, premultiplied once here rather than on every blit.
fn to_pixmap(icon: &icons::RgbaIcon) -> Option<Pixmap> {
    let mut pm = Pixmap::new(icon.width, icon.height)?;
    for (dst, src) in pm.pixels_mut().iter_mut().zip(icon.pixels.chunks_exact(4)) {
        let a = src[3] as u32;
        let m = |c: u8| ((c as u32 * a + 127) / 255) as u8;
        *dst = PremultipliedColorU8::from_rgba(m(src[0]), m(src[1]), m(src[2]), src[3])
            .unwrap_or(PremultipliedColorU8::TRANSPARENT);
    }
    Some(pm)
}

/// Draw a square icon centered on `pos` with half-extent `half`.
fn draw_icon(dst: &mut Pixmap, icon: &Pixmap, pos: [f32; 2], half: f32, alpha: f32) {
    if half <= 0.0 || alpha <= 0.004 || icon.width() == 0 {
        return;
    }
    let scale = (half * 2.0) / icon.width() as f32;
    dst.draw_pixmap(
        0,
        0,
        icon.as_ref(),
        &PixmapPaint {
            opacity: alpha.clamp(0.0, 1.0),
            quality: FilterQuality::Bilinear,
            ..Default::default()
        },
        Transform::from_row(scale, 0.0, 0.0, scale, pos[0] - half, pos[1] - half),
        None,
    );
}

impl Gfx {
    /// `hwnd` must already carry `WS_EX_LAYERED` — see `main::set_no_activate`.
    pub fn new(hwnd: HWND, cfg: &Config, geo: MenuGeometry) -> Gfx {
        let size = geo.window_size().max(1);
        let pixmap = Pixmap::new(size, size).expect("frame pixmap");
        let layered = Layered::new(hwnd, size, size);
        if layered.is_none() {
            eprintln!("sideQM: could not create the layered surface; the menu will not draw");
        }

        let mut font_system = FontSystem::new();
        let swash = SwashCache::new();
        // Placeholder metrics: set_items (called at the end of this function)
        // recreates every buffer from the configured label_font_px and seeds
        // the idle "·" — that value isn't known this early in construction.
        let hub_label_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 16.9));
        let hub_sub_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));
        let gear_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 13.0));
        let remove_buf = TextBuffer::new(&mut font_system, Metrics::new(13.0, 13.0));
        let toggle_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));
        let done_buf = TextBuffer::new(&mut font_system, Metrics::new(11.0, 14.3));

        let mut gfx = Gfx {
            pixmap,
            layered,
            hwnd,
            font_system,
            swash,
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

    /// A decode finished: convert it and let the Tile stop drawing its letter.
    pub fn set_slot_icon(&mut self, k: usize, icon: &icons::RgbaIcon) {
        let pm = to_pixmap(icon);
        if let Some(slot) = self.slots.get_mut(k) {
            slot.icon = pm;
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

    /// Icon preview for the Popover's current target: a pixmap when one has
    /// been decoded, else a fallback letter.
    pub fn set_popover_icon(&mut self, icon: Option<&icons::RgbaIcon>, fallback: char) {
        self.pop_icon = icon.and_then(to_pixmap);
        self.set_popover_fallback(fallback);
    }

    /// Just the letter — the name field changed but the icon didn't, so there
    /// is no reason to rebuild the pixmap.
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
        if let Some(layered) = &mut self.layered {
            layered.present(self.pixmap.data());
        }
        // Frame pacing: block until the compositor is ready for the next one,
        // or the redraw loop would spin as fast as the CPU can rasterize.
        present::wait_for_vblank();
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

    /// The window changed size. Legal now that no swapchain is involved —
    /// ADR-0002's ban died with the DirectComposition path.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if self.pixmap.width() == w && self.pixmap.height() == h {
            return;
        }
        match Pixmap::new(w, h) {
            Some(pm) => self.pixmap = pm,
            None => return,
        }
        self.layered = Layered::new(self.hwnd, w, h);
    }

    fn draw(&mut self, frame: &FrameModel, view: &MenuView) {
        self.pixmap.fill(Color::TRANSPARENT);
        let center = self.pixmap.width() as f32 / 2.0;
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
        let dragged_slot = view.drag.as_ref().map(|d| self.geo.slot_of_item(d.from));
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
        // Emission order is the old vertex buffer's order, which is what keeps
        // the alpha compositing identical.
        let scrim_scale = 0.85 + 0.15 * frame.scrim.max(0.0);
        let scrim_alpha = frame.scrim.clamp(0.0, 1.0);
        let scrim_hr = scrim_r * scrim_scale;
        box_shape(
            &mut self.pixmap,
            center,
            center,
            scrim_hr,
            scrim_hr,
            scrim_hr,
            (scrim_bg, opacity * scrim_alpha),
            Some((1.2, white, 0.06 * scrim_alpha)),
            0.0,
        );
        box_shape(
            &mut self.pixmap,
            center,
            center,
            hub_r,
            hub_r,
            hub_r,
            (hub_bg, scrim_alpha),
            Some((
                1.2,
                if hover.is_some() { accent } else { white },
                (if hover.is_some() { 0.45 } else { 0.10 }) * scrim_alpha,
            )),
            0.0,
        );
        // Gear zone: the Hub's bottom segment; release there enters Pinned.
        // Once Pinned, the Hub belongs to the toggle instead.
        if !view.editing
            && let Some(path) = segment_path(center, center, hub_r, self.geo.gear_cut_dy())
        {
            let a = (if view.gear_hover { 0.14 } else { 0.05 }) * scrim_alpha;
            self.pixmap.fill_path(
                &path,
                &paint_of(if view.gear_hover { accent } else { white }, a),
                FillRule::Winding,
                Transform::identity(),
                None,
            );
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
            box_shape(
                &mut self.pixmap,
                pos[0],
                pos[1],
                tile_half * scale,
                tile_half * scale,
                tile_corner * scale,
                (fill_c, fill_a * alpha),
                Some((1.1, border_c, border_a * alpha)),
                if self.slots[k].is_meta {
                    DODAJ_DASHES
                } else {
                    0.0
                },
            );
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
                box_shape(
                    &mut self.pixmap,
                    pos[0] + tile_half * 0.85 * scale,
                    pos[1] - tile_half * 0.85 * scale,
                    remove_r,
                    remove_r,
                    remove_r,
                    (if hovered { danger } else { hub_bg }, 0.95 * alpha),
                    Some((1.1, if hovered { danger } else { white }, 0.35 * alpha)),
                    0.0,
                );
            }
            // Toggle: track + knob, the knob sliding to the "on" end when the
            // Dodaj slot is hidden.
            let (track_w, track_h) = (30.0f32, 16.0f32);
            let track_cy = center + 9.0;
            let on = view.add_hidden;
            box_shape(
                &mut self.pixmap,
                center,
                track_cy,
                track_w / 2.0,
                track_h / 2.0,
                track_h / 2.0,
                (
                    if on { accent } else { white },
                    (if on { 0.75 } else { 0.08 }) * scrim_alpha,
                ),
                Some((
                    1.1,
                    if view.hover_toggle { accent } else { white },
                    (if view.hover_toggle { 0.7 } else { 0.18 }) * scrim_alpha,
                )),
                0.0,
            );
            let knob_r = track_h / 2.0 - 2.5;
            box_shape(
                &mut self.pixmap,
                center + if on { track_w / 4.0 } else { -track_w / 4.0 },
                track_cy,
                knob_r,
                knob_r,
                knob_r,
                (if on { hub_bg } else { white }, 0.9 * scrim_alpha),
                None,
                0.0,
            );
            // Done: the way out, in the same segment the Gear zone (the way in)
            // occupies outside Pinned.
            if let Some(path) = segment_path(center, center, hub_r, self.geo.done_cut_dy()) {
                let a = (if view.hover_done { 0.18 } else { 0.06 }) * scrim_alpha;
                self.pixmap.fill_path(
                    &path,
                    &paint_of(if view.hover_done { accent } else { white }, a),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }

        if let Some((arc_angle, arc_alpha)) = frame.arc {
            let r = scrim_r + ARC_OFFSET;
            let half = ARC_HALF_FRAC * (TAU / n as f32);
            if let Some(path) = arc_path(center, center, r, arc_angle, half) {
                // border 1.75 was half of the spec's 3.5px stroke; a centered
                // stroke wants the whole width, and round caps reproduce what
                // clamping the SDF's angle used to give for free.
                let stroke = Stroke {
                    width: 3.5,
                    line_cap: LineCap::Round,
                    ..Default::default()
                };
                self.pixmap.stroke_path(
                    &path,
                    &paint_of(accent, arc_alpha),
                    &stroke,
                    Transform::identity(),
                    None,
                );
            }
        }

        // --- Popover: panel morphing out of the Dodaj Tile + form widgets ---
        // Content fades in over the last stretch of the expansion.
        let pop_content_a = ((pop_p - 0.6) / 0.4).clamp(0.0, 1.0) * scrim_alpha;
        if pop_active {
            let ps = view.popover.unwrap();
            let lp = |a: f32, b: f32| a + (b - a) * pop_p;
            let panel = ps.layout.panel;
            box_shape(
                &mut self.pixmap,
                center + lp(ps.origin[0], panel.center[0]),
                center + lp(ps.origin[1], panel.center[1]),
                lp(tile_half, panel.half[0]),
                lp(tile_half, panel.half[1]),
                lp(tile_corner, 12.0),
                (hub_bg, 0.97 * scrim_alpha),
                Some((1.1, white, 0.10 * scrim_alpha)),
                0.0,
            );
            if pop_content_a > 0.01 {
                use popover::Element as El;
                let a = pop_content_a;
                let mut field = |r: &popover::Rect, focused: bool| {
                    box_shape(
                        &mut self.pixmap,
                        center + r.center[0],
                        center + r.center[1],
                        r.half[0],
                        r.half[1],
                        8.0,
                        (white, 0.05 * a),
                        Some((
                            1.1,
                            if focused { accent } else { white },
                            (if focused { 0.85 } else { 0.12 }) * a,
                        )),
                        0.0,
                    );
                };
                field(&ps.layout.name_field, ps.focus == El::NameField);
                field(&ps.layout.target_field, ps.focus == El::TargetField);
                let mut button = |r: &popover::Rect, hovered: bool| {
                    box_shape(
                        &mut self.pixmap,
                        center + r.center[0],
                        center + r.center[1],
                        r.half[0],
                        r.half[1],
                        8.0,
                        (white, (if hovered { 0.11 } else { 0.06 }) * a),
                        Some((
                            1.1,
                            if hovered { accent } else { white },
                            (if hovered { 0.55 } else { 0.12 }) * a,
                        )),
                        0.0,
                    );
                };
                button(&ps.layout.browse_btn, ps.hover == Some(El::Browse));
                button(&ps.layout.icon_btn, ps.hover == Some(El::IconBtn));
                button(&ps.layout.cancel_btn, ps.hover == Some(El::Cancel));
                // Commit: accent when valid, muted when the target is empty.
                let valid = ps.valid();
                box_shape(
                    &mut self.pixmap,
                    center + ps.layout.commit_btn.center[0],
                    center + ps.layout.commit_btn.center[1],
                    ps.layout.commit_btn.half[0],
                    ps.layout.commit_btn.half[1],
                    8.0,
                    (
                        if valid { accent } else { white },
                        (if valid { 0.9 } else { 0.05 }) * a,
                    ),
                    Some((
                        1.1,
                        accent,
                        (if valid {
                            1.0
                        } else if ps.hover == Some(El::Commit) {
                            0.35
                        } else {
                            0.0
                        }) * a,
                    )),
                    0.0,
                );
                // Icon preview well.
                box_shape(
                    &mut self.pixmap,
                    center + ps.layout.icon_preview.center[0],
                    center + ps.layout.icon_preview.center[1],
                    ps.layout.icon_preview.half[0],
                    ps.layout.icon_preview.half[1],
                    8.0,
                    (white, 0.05 * a),
                    Some((1.1, white, 0.10 * a)),
                    0.0,
                );
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
                    box_shape(
                        &mut self.pixmap,
                        x,
                        center + rect.center[1],
                        0.75,
                        rect.half[1] - 7.0,
                        0.0,
                        (accent, 0.95 * a),
                        None,
                        0.0,
                    );
                }
            }
        }

        // --- icons ---
        for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
            let Some(icon) = self.slots.get(k).and_then(|s| s.icon.as_ref()) else {
                continue;
            };
            draw_icon(
                &mut self.pixmap,
                icon,
                pos,
                (tile_half - icon_inset) * scale,
                alpha,
            );
        }
        if pop_active
            && pop_content_a > 0.01
            && let Some(icon) = &self.pop_icon
        {
            let r = &view.popover.unwrap().layout.icon_preview;
            draw_icon(
                &mut self.pixmap,
                icon,
                [center + r.center[0], center + r.center[1]],
                r.half[0] - 6.0,
                pop_content_a,
            );
        }

        // --- text: fallback letters, always-on tile labels, hub text ---
        let glyph_px = self.geo.glyph_px();
        for k in 0..self.slots.len() {
            let (pos, scale, alpha) = tiles[k];
            if scale < 0.01 || alpha < 0.01 || (self.slots[k].is_meta && pop_active) {
                continue;
            }
            // The letter is what a Tile shows until its icon has been decoded,
            // and what it keeps forever if there is no icon to be had.
            if self.slots[k].icon.is_none()
                && let Some(letter) = &self.slots[k].letter
            {
                // Positioning ratios tuned against geo.glyph_px(), the same
                // size set_items shaped this buffer with.
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    letter,
                    pos[0] - glyph_px * 0.32 * scale,
                    pos[1] - glyph_px * 0.5 * scale,
                    scale,
                    [0.102, 0.102, 0.102], // #1A1A1A
                    alpha,
                    None,
                );
            }
            let hovered = hover == Some(k);
            let label_w = width_of(&self.slots[k].label_buf);
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.slots[k].label_buf,
                pos[0] - label_w / 2.0,
                pos[1] + tile_half * scale + 8.0,
                1.0,
                if hovered { accent } else { idle_text },
                alpha,
                None,
            );
        }
        // The Hub's idle dot / selected name, and the gear glyph, belong to the
        // press-and-hold Menu. While Pinned the Hub is the toggle's.
        if !view.editing {
            let name_w = width_of(&self.hub_label_buf);
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.hub_label_buf,
                center - name_w / 2.0,
                center - label_font_px * 0.7,
                1.0,
                if hover.is_some() { accent } else { hub_dot },
                scrim_alpha,
                None,
            );
        }
        if hover.is_some() && !view.editing {
            let sub_w = width_of(&self.hub_sub_buf);
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.hub_sub_buf,
                center - sub_w / 2.0,
                center + label_font_px * 0.55,
                1.0,
                idle_text,
                scrim_alpha,
                None,
            );
        }
        // Gear glyph, centered in the Hub's bottom segment.
        if !view.editing {
            let gear_w = width_of(&self.gear_buf);
            let gear_px = self.geo.hub_r() * 0.28;
            let seg_cy = center + (self.geo.gear_cut_dy() + hub_r) / 2.0;
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.gear_buf,
                center - gear_w / 2.0,
                seg_cy - gear_px * 0.62,
                1.0,
                if view.gear_hover { accent } else { hub_dot },
                scrim_alpha,
                None,
            );
        } else if !pop_active {
            // Toggle caption, above the switch.
            let w = width_of(&self.toggle_buf);
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.toggle_buf,
                center - w / 2.0,
                center - TOGGLE_LABEL_PX * 1.5,
                1.0,
                if view.hover_toggle { accent } else { idle_text },
                scrim_alpha,
                None,
            );
            // Done caption, centered in the Hub's bottom segment.
            let done_w = width_of(&self.done_buf);
            let seg_cy = center + (self.geo.done_cut_dy() + hub_r) / 2.0;
            blit_text(
                &mut self.pixmap,
                &mut self.font_system,
                &mut self.swash,
                &self.done_buf,
                center - done_w / 2.0,
                seg_cy - TOGGLE_LABEL_PX * 0.62,
                1.0,
                if view.hover_done { accent } else { idle_text },
                scrim_alpha,
                None,
            );
            // One glyph buffer, drawn once per removable Tile.
            let rm_w = width_of(&self.remove_buf);
            for (k, (pos, scale, alpha)) in tiles.iter().copied().enumerate() {
                if self.slots.get(k).is_none_or(|s| s.is_meta) || scale < 0.01 || alpha < 0.01 {
                    continue;
                }
                let color = if view.hover_remove == Some(k) {
                    white
                } else {
                    idle_text
                };
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    &self.remove_buf,
                    pos[0] + tile_half * 0.85 * scale - rm_w / 2.0,
                    pos[1] - tile_half * 0.85 * scale - remove_r * 0.95,
                    1.0,
                    color,
                    alpha,
                    None,
                );
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
            for (buf, rect) in [
                (&pop.lbl_name, &ps.layout.name_field),
                (&pop.lbl_target, &ps.layout.target_field),
            ] {
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    buf,
                    center + rect.left() + 2.0,
                    center + rect.top() - POP_LABEL_PX * 1.55,
                    1.0,
                    idle_text,
                    a,
                    None,
                );
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
                // Clip to the field, so a long value scrolls under its border
                // instead of spilling across the panel.
                let clip = [
                    (center + rect.left() + 3.0) as i32,
                    (center + rect.top()) as i32,
                    (center + rect.left() + rect.half[0] * 2.0 - 3.0) as i32,
                    (center + rect.top() + rect.half[1] * 2.0) as i32,
                ];
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    buf,
                    center + rect.left() + POP_FIELD_PAD + scroll,
                    center + rect.center[1] - POP_FIELD_PX * 0.62,
                    1.0,
                    text_c,
                    a,
                    Some(clip),
                );
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
                let w = width_of(buf);
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    buf,
                    center + rect.center[0] - w / 2.0,
                    center + rect.center[1] - POP_BTN_PX * 0.62,
                    1.0,
                    color,
                    a,
                    None,
                );
            }
            if self.pop_icon.is_none() {
                let rect = &ps.layout.icon_preview;
                let w = width_of(&pop.fallback);
                blit_text(
                    &mut self.pixmap,
                    &mut self.font_system,
                    &mut self.swash,
                    &pop.fallback,
                    center + rect.center[0] - w / 2.0,
                    center + rect.center[1] - POP_FALLBACK_PX * 0.55,
                    1.0,
                    idle_text,
                    a,
                    None,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three path builders replace the shader's three SDF branches; these
    /// pin down the shapes they are supposed to produce.
    #[test]
    fn round_rect_spans_its_extents_and_degrades_to_a_circle() {
        let p = round_rect(100.0, 50.0, 20.0, 10.0, 4.0).expect("rounded rect");
        let b = p.bounds();
        assert!((b.left() - 80.0).abs() < 0.01, "{b:?}");
        assert!((b.right() - 120.0).abs() < 0.01, "{b:?}");
        assert!((b.top() - 40.0).abs() < 0.01, "{b:?}");
        assert!((b.bottom() - 60.0).abs() < 0.01, "{b:?}");

        // Corner radius is clamped to the smaller half-extent, so the Scrim's
        // corner == half never produces a degenerate path.
        let circle = round_rect(0.0, 0.0, 30.0, 30.0, 999.0).expect("circle");
        let b = circle.bounds();
        assert!(
            (b.left() + 30.0).abs() < 0.01 && (b.right() - 30.0).abs() < 0.01,
            "{b:?}"
        );

        // Zero radius is a plain rect, not an empty path.
        assert!(round_rect(0.0, 0.0, 5.0, 5.0, 0.0).is_some());
    }

    #[test]
    fn segment_keeps_the_part_below_the_chord() {
        let r = 40.0;
        let dy = 24.0;
        let p = segment_path(0.0, 0.0, r, dy).expect("segment");
        let b = p.bounds();
        // Bottom reaches the disc, top stops at the chord.
        assert!((b.bottom() - r).abs() < 0.5, "{b:?}");
        assert!((b.top() - dy).abs() < 0.5, "{b:?}");
        let half_w = (r * r - dy * dy).sqrt();
        assert!((b.right() - half_w).abs() < 0.5, "{b:?}");

        // A chord past the disc leaves nothing to draw rather than a bad path.
        assert!(segment_path(0.0, 0.0, r, r + 1.0).is_none());
    }

    #[test]
    fn arc_rides_its_radius_around_the_pointing_angle() {
        let r = 100.0;
        let p = arc_path(0.0, 0.0, r, 0.0, 0.3).expect("arc");
        // Every flattened point sits on the circle, within the chord error.
        for pt in p.points() {
            let d = (pt.x * pt.x + pt.y * pt.y).sqrt();
            assert!((d - r).abs() < 1.0, "point off the radius: {d}");
        }
        let b = p.bounds();
        assert!(b.right() > 90.0, "arc should point along +x: {b:?}");
    }

    /// Popover fields scroll their text under the field border, so the clip
    /// rect is the only thing keeping a long value from spilling across the
    /// panel. glyphon used to enforce this with `TextBounds`.
    #[test]
    fn text_blit_respects_its_clip_rect() {
        let mut fs = FontSystem::new();
        let mut swash = SwashCache::new();
        let mut buf = TextBuffer::new(&mut fs, Metrics::new(20.0, 26.0));
        buf.set_text(
            "MMMMMMMMMMMM",
            &Attrs::new().family(Family::Monospace),
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(&mut fs, false);

        let ink = |pm: &Pixmap, l: u32, r: u32| {
            let w = pm.width() as usize;
            let px = pm.pixels();
            (0..pm.height() as usize)
                .flat_map(|y| (l as usize..r as usize).map(move |x| y * w + x))
                .filter(|&i| px[i].alpha() > 0)
                .count()
        };

        let mut full = Pixmap::new(240, 40).unwrap();
        blit_text(
            &mut full,
            &mut fs,
            &mut swash,
            &buf,
            5.0,
            5.0,
            1.0,
            [1.0, 1.0, 1.0],
            1.0,
            None,
        );
        assert!(ink(&full, 0, 120) > 0, "no text rendered at all");
        assert!(
            ink(&full, 120, 240) > 0,
            "the sample must be long enough to cross the clip edge"
        );

        let mut clipped = Pixmap::new(240, 40).unwrap();
        blit_text(
            &mut clipped,
            &mut fs,
            &mut swash,
            &buf,
            5.0,
            5.0,
            1.0,
            [1.0, 1.0, 1.0],
            1.0,
            Some([0, 0, 120, 40]),
        );
        assert!(ink(&clipped, 0, 120) > 0, "clipping erased everything");
        assert_eq!(ink(&clipped, 120, 240), 0, "text escaped the clip rect");
    }

    #[test]
    fn blend_over_stays_a_valid_premultiplied_color() {
        // Opaque white over transparent, then half-alpha black over that:
        // the invariant that matters is channels <= alpha, which is what
        // PremultipliedColorU8 refuses to represent otherwise.
        let mut px = PremultipliedColorU8::TRANSPARENT;
        blend_over(&mut px, 1.0, 1.0, 1.0, 1.0);
        assert_eq!((px.red(), px.alpha()), (255, 255));
        blend_over(&mut px, 0.0, 0.0, 0.0, 0.5);
        assert!(px.red() <= px.alpha() && px.green() <= px.alpha() && px.blue() <= px.alpha());
        assert_eq!(px.alpha(), 255);
    }
}
