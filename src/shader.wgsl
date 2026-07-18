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

// ---- shapes: SDF rounded box; a circle is a box with corner radius == half extent ----

struct ShapeIn {
    @location(0) pos: vec2f,          // center, window px
    @location(1) half: vec2f,         // half extents, px
    @location(2) corner: f32,         // corner radius, px
    @location(3) border: f32,         // border thickness, px (0 = none)
    @location(4) fill: vec4f,
    @location(5) border_color: vec4f,
}

struct ShapeOut {
    @builtin(position) clip: vec4f,
    @location(0) local: vec2f,
    @location(1) half: vec2f,
    @location(2) corner: f32,
    @location(3) border: f32,
    @location(4) fill: vec4f,
    @location(5) border_color: vec4f,
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
    return out;
}

fn sd_round_box(p: vec2f, b: vec2f, r: f32) -> f32 {
    let q = abs(p) - b + vec2f(r);
    return length(max(q, vec2f(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_shape(in: ShapeOut) -> @location(0) vec4f {
    let d = sd_round_box(in.local, in.half, in.corner);
    let aa = max(fwidth(d), 0.001);
    let coverage = 1.0 - smoothstep(-aa, aa, d);
    let border_mix = smoothstep(-in.border - aa, -in.border + aa, d);
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
