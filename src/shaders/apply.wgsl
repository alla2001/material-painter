// Writes the current stroke into a paint layer (or mask) using the snapshot
// taken at stroke start as the base, so stroke opacity caps the build-up.
// Paint channels store (value, alpha) pairs; color stores rgb + alpha.

struct ApplyU {
    color: vec4<f32>,   // sRGB-encoded rgb, opacity
    rmh: vec4<f32>,     // roughness, metallic, height, mask value
    ch: vec4<f32>,      // channel enabled flags: color, roughness, metallic, height
    p: vec4<f32>,       // stroke opacity, erase, opacity channel enabled, masks: over the smart mask
    g: vec4<f32>,       // grain strength (0 off), tiling, in uv (else tri-planar world), invert
    e: vec4<f32>,       // sRGB-encoded emission, emission channel enabled
    s: vec4<f32>,       // specular level, specular channel enabled, normal channel enabled (paints flat), unused
};

@group(0) @binding(0) var<uniform> u: ApplyU;
@group(0) @binding(1) var stroke: texture_2d<f32>;
@group(0) @binding(2) var b_color: texture_2d<f32>;
@group(0) @binding(3) var b_rough: texture_2d<f32>;
@group(0) @binding(4) var b_metal: texture_2d<f32>;
@group(0) @binding(5) var b_height: texture_2d<f32>;
@group(0) @binding(6) var pos_map: texture_2d<f32>;
@group(0) @binding(7) var nrm_map: texture_2d<f32>;
@group(0) @binding(8) var grain_tex: texture_2d<f32>;
@group(0) @binding(9) var grain_samp: sampler;
@group(0) @binding(10) var b_opacity: texture_2d<f32>;
@group(0) @binding(11) var b_emission: texture_2d<f32>;
@group(0) @binding(12) var b_normal: texture_2d<f32>;
@group(0) @binding(13) var b_spec: texture_2d<f32>;

fn grain_at(q: vec2<f32>) -> f32 {
    return textureSampleLevel(grain_tex, grain_samp, q, 0.0).r;
}

// Brush grain: a texture fixed to the surface that caps how much paint each
// texel takes, so strokes build up a broken, textured look (scratchy, dusty,
// chipped) instead of a flat one. Tri-planar in world space, or in UV space
// for the UV projection.
fn grain(c: vec2<i32>) -> f32 {
    if (u.g.x <= 0.0) {
        return 1.0;
    }
    var v: f32;
    if (u.g.z > 0.5) {
        let uv = (vec2<f32>(c) + 0.5) / vec2<f32>(textureDimensions(stroke));
        v = grain_at(uv * u.g.y);
    } else {
        let p = textureLoad(pos_map, c, 0).xyz * u.g.y;
        let n = textureLoad(nrm_map, c, 0).xyz * 2.0 - 1.0;
        var w = pow(abs(n), vec3<f32>(4.0));
        w /= max(w.x + w.y + w.z, 1e-4);
        v = grain_at(vec2<f32>(p.z, -p.y)) * w.x + grain_at(p.xz) * w.y + grain_at(vec2<f32>(p.x, -p.y)) * w.z;
    }
    if (u.g.w > 0.5) {
        v = 1.0 - v;
    }
    return mix(1.0, v, u.g.x);
}

fn over(base: vec2<f32>, v: f32, m: f32, erase: bool) -> vec2<f32> {
    if (erase) {
        return vec2<f32>(base.x, base.y * (1.0 - m));
    }
    let a = m + base.y * (1.0 - m);
    if (a < 1e-5) {
        return vec2<f32>(v, 0.0);
    }
    return vec2<f32>((v * m + base.x * base.y * (1.0 - m)) / a, a);
}

struct PaintOut {
    @location(0) color: vec4<f32>,
    @location(1) rough: vec4<f32>,
    @location(2) metal: vec4<f32>,
    @location(3) height: vec4<f32>,
    @location(4) opacity: vec4<f32>,
    @location(5) emission: vec4<f32>,
    @location(6) normal: vec4<f32>,
    @location(7) spec: vec4<f32>,
};

// A color channel's (rgb, coverage) with `v` laid over it by `m` (or erased).
fn over_rgb(b: vec4<f32>, v: vec3<f32>, m: f32, erase: bool) -> vec4<f32> {
    if (erase) {
        return vec4<f32>(b.rgb, b.a * (1.0 - m));
    }
    let a = m + b.a * (1.0 - m);
    if (a > 1e-5) {
        return vec4<f32>((v * m + b.rgb * b.a * (1.0 - m)) / a, a);
    }
    return vec4<f32>(v, a);
}

@fragment
fn fs_paint(in: FsOut) -> PaintOut {
    let c = vec2<i32>(in.pos.xy);
    let m = textureLoad(stroke, c, 0).r * u.p.x * grain(c);
    let erase = u.p.y > 0.5;
    var out: PaintOut;
    if (u.ch.x > 0.5) {
        out.color = over_rgb(textureLoad(b_color, c, 0), u.color.rgb, m, erase);
    }
    if (u.e.w > 0.5) {
        out.emission = over_rgb(textureLoad(b_emission, c, 0), u.e.rgb, m, erase);
    }
    if (u.ch.y > 0.5) {
        out.rough = vec4<f32>(over(textureLoad(b_rough, c, 0).rg, u.rmh.x, m, erase), 0.0, 0.0);
    }
    if (u.ch.z > 0.5) {
        out.metal = vec4<f32>(over(textureLoad(b_metal, c, 0).rg, u.rmh.y, m, erase), 0.0, 0.0);
    }
    if (u.ch.w > 0.5) {
        out.height = vec4<f32>(over(textureLoad(b_height, c, 0).rg, u.rmh.z, m, erase), 0.0, 0.0);
    }
    if (u.p.z > 0.5) {
        out.opacity = vec4<f32>(over(textureLoad(b_opacity, c, 0).rg, u.color.w, m, erase), 0.0, 0.0);
    }
    if (u.s.z > 0.5) {
        // A brush lays flat normals: it smooths away the layer's normal detail.
        out.normal = over_rgb(textureLoad(b_normal, c, 0), vec3<f32>(0.5, 0.5, 1.0), m, erase);
    }
    if (u.s.y > 0.5) {
        out.spec = vec4<f32>(over(textureLoad(b_spec, c, 0).rg, u.s.x, m, erase), 0.0, 0.0);
    }
    return out;
}

// Masks store (r) a multiplier of the layer's smart mask and (g) a value
// added on top: the mask is r * smart mask + g. The app's brush sets r (the
// smart mask shows where it is white) and clears g. Strokes "over" (Painter's
// order, for imports) lay their value over the whole mask so far, smart mask
// included: an affine step on both; erasing them brings back what was below
// the paint (the smart mask, or the mask's starting value).
@fragment
fn fs_mask(in: FsOut) -> @location(0) vec4<f32> {
    let c = vec2<i32>(in.pos.xy);
    let m = textureLoad(stroke, c, 0).r * u.p.x * grain(c);
    let base = textureLoad(b_color, c, 0).rg;
    let erase = u.p.y > 0.5;
    if (u.p.w > 0.5) {
        if (erase) {
            return vec4<f32>(base.x + (1.0 - base.x) * m, base.y * (1.0 - m), 0.0, 1.0);
        }
        return vec4<f32>(base.x * (1.0 - m), base.y * (1.0 - m) + u.rmh.w * m, 0.0, 1.0);
    }
    let target_value = select(u.rmh.w, 0.0, erase);
    return vec4<f32>(mix(base.x, target_value, m), base.y * (1.0 - m), 0.0, 1.0);
}
