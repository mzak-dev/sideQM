struct Globals {
    resolution: vec2f,
    _pad: vec2f,
}
@group(0) @binding(0) var<uniform> globals: Globals;

// Six vertices of a unit quad as two triangles.
fn corner(vi: u32) -> vec2f {
    var corners = array<vec2f, 6>(
        vec2f(-1.0, -1.0), vec2f(1.0, -1.0), vec2f(-1.0, 1.0),
        vec2f(-1.0, 1.0), vec2f(1.0, -1.0), vec2f(1.0, 1.0),
    );
    return corners[vi];
}

fn to_ndc(p: vec2f) -> vec4f {
    return vec4f(p.x / globals.resolution.x * 2.0 - 1.0,
                 1.0 - p.y / globals.resolution.y * 2.0, 0.0, 1.0);
}

// ---- shapes: SDF rounded box (kind 0) or an arc stroke (kind 1) ----

struct ShapeIn {
    @location(0) pos: vec2f,          // center, window px
    @location(1) half: vec2f,         // half extents, px (arc: radius in .x)
    @location(2) corner: f32,         // corner radius, px
    @location(3) border: f32,         // border thickness, px (0 = none; arc: stroke half-width)
    @location(4) fill: vec4f,
    @location(5) border_color: vec4f,
    @location(6) kind: f32,           // 0 = rounded box, 1 = arc stroke
    @location(7) angle_center: f32,   // arc: pointing angle, radians (atan2 screen convention)
    @location(8) angle_half: f32,     // arc: half angular width, radians
    @location(9) dash: f32,           // box only: > 0 = dash count around the border
}

struct ShapeOut {
    @builtin(position) clip: vec4f,
    @location(0) local: vec2f,
    @location(1) half: vec2f,
    @location(2) corner: f32,
    @location(3) border: f32,
    @location(4) fill: vec4f,
    @location(5) border_color: vec4f,
    @location(6) kind: f32,
    @location(7) angle_center: f32,
    @location(8) angle_half: f32,
    @location(9) dash: f32,
}

@vertex
fn vs_shape(@builtin(vertex_index) vi: u32, in: ShapeIn) -> ShapeOut {
    let c = corner(vi);
    var out: ShapeOut;
    out.clip = to_ndc(in.pos + c * in.half);
    out.local = c * in.half;
    out.half = in.half;
    out.corner = in.corner;
    out.border = in.border;
    out.fill = in.fill;
    out.border_color = in.border_color;
    out.kind = in.kind;
    out.angle_center = in.angle_center;
    out.angle_half = in.angle_half;
    out.dash = in.dash;
    return out;
}

fn sd_round_box(p: vec2f, b: vec2f, r: f32) -> f32 {
    let q = abs(p) - b + vec2f(r);
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Distance to a circular arc's centerline: rotate into the arc's local frame
// (angle 0 = its pointing direction), clamp the angle to the arc's half-width,
// then measure to that point on the circle. Clamping the angle (rather than
// leaving it unbounded) gives rounded end-caps for free.
fn sd_arc(p: vec2f, angle_center: f32, angle_half: f32, radius: f32) -> f32 {
    let ca = cos(-angle_center);
    let sa = sin(-angle_center);
    let q = vec2f(p.x * ca - p.y * sa, p.x * sa + p.y * ca);
    let a = clamp(atan2(q.y, q.x), -angle_half, angle_half);
    let closest = radius * vec2f(cos(a), sin(a));
    return length(q - closest);
}

@fragment
fn fs_shape(in: ShapeOut) -> @location(0) vec4f {
    if (in.kind > 0.5) {
        let d = sd_arc(in.local, in.angle_center, in.angle_half, in.half.x) - in.border;
        let aa = max(fwidth(d), 0.001);
        let coverage = 1.0 - smoothstep(-aa, aa, d);
        let a = in.fill.a * coverage;
        return vec4f(in.fill.rgb * a, a); // premultiplied
    }
    let d = sd_round_box(in.local, in.half, in.corner);
    let aa = max(fwidth(d), 0.001);
    let coverage = 1.0 - smoothstep(-aa, aa, d);
    var border_mix = smoothstep(-in.border - aa, -in.border + aa, d);
    if (in.dash > 0.5) {
        // Angle-based dash: uneven along straight edges vs. corners at this
        // tile size, but not perceptibly so, and far simpler than tracing
        // arc-length around a rounded rect's actual perimeter.
        let pi = 3.14159265359;
        let period = 2.0 * pi / in.dash;
        let phase = (atan2(in.local.y, in.local.x) + pi) % period;
        border_mix *= step(phase, period * 0.5);
    }
    var col = mix(in.fill, in.border_color, select(0.0, border_mix, in.border > 0.0));
    let a = col.a * coverage;
    return vec4f(col.rgb * a, a); // premultiplied
}

// ---- textured quads (icons) ----

struct TexIn {
    @location(0) pos: vec2f,
    @location(1) half: vec2f,
    @location(2) alpha: f32,
}

struct TexOut {
    @builtin(position) clip: vec4f,
    @location(0) uv: vec2f,
    @location(1) alpha: f32,
}

@group(1) @binding(0) var icon_tex: texture_2d<f32>;
@group(1) @binding(1) var icon_samp: sampler;

@vertex
fn vs_tex(@builtin(vertex_index) vi: u32, in: TexIn) -> TexOut {
    let c = corner(vi);
    var out: TexOut;
    out.clip = to_ndc(in.pos + c * in.half);
    out.uv = c * 0.5 + vec2f(0.5);
    out.alpha = in.alpha;
    return out;
}

@fragment
fn fs_tex(in: TexOut) -> @location(0) vec4f {
    let c = textureSample(icon_tex, icon_samp, in.uv);
    let a = c.a * in.alpha;
    return vec4f(c.rgb * a, a); // premultiplied
}
