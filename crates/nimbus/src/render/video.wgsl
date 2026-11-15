// YUV (NV12 / I420) → RGB with optional contrast-adaptive sharpening on luma.

struct Params {
    texel: vec2<f32>,      // 1 / luma texture size
    sharpness: f32,        // 0..1
    plane_layout: u32,     // 0 = NV12, 1 = I420
    full_range: u32,
    matrix: u32,           // 0 = BT.601, 1 = BT.709
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var tex_y: texture_2d<f32>;
@group(0) @binding(3) var tex_u: texture_2d<f32>;
@group(0) @binding(4) var tex_v: texture_2d<f32>;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // Full-screen triangle.
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn luma(uv: vec2<f32>) -> f32 {
    return textureSampleLevel(tex_y, samp, uv, 0.0).r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    var y = luma(in.uv);

    if (p.sharpness > 0.0) {
        // AMD FidelityFX CAS-style: sharpen less where local contrast is
        // already high (avoids halos on the compression edges).
        let n = luma(in.uv + vec2<f32>(0.0, -p.texel.y));
        let s = luma(in.uv + vec2<f32>(0.0, p.texel.y));
        let e = luma(in.uv + vec2<f32>(p.texel.x, 0.0));
        let w = luma(in.uv + vec2<f32>(-p.texel.x, 0.0));
        let mn = min(y, min(min(n, s), min(e, w)));
        let mx = max(y, max(max(n, s), max(e, w)));
        let amp = sqrt(clamp(min(mn, 1.0 - mx) / max(mx, 0.0001), 0.0, 1.0));
        let peak = -1.0 / mix(8.0, 5.0, clamp(p.sharpness, 0.0, 1.0));
        let wgt = amp * peak;
        y = clamp((y + (n + s + e + w) * wgt) / (1.0 + 4.0 * wgt), 0.0, 1.0);
    }

    var cb: f32;
    var cr: f32;
    if (p.plane_layout == 0u) {
        let c = textureSampleLevel(tex_u, samp, in.uv, 0.0).rg;
        cb = c.x;
        cr = c.y;
    } else {
        cb = textureSampleLevel(tex_u, samp, in.uv, 0.0).r;
        cr = textureSampleLevel(tex_v, samp, in.uv, 0.0).r;
    }

    var yy: f32;
    if (p.full_range == 1u) {
        yy = y;
        cb = cb - 0.5;
        cr = cr - 0.5;
    } else {
        yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (cb - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (cr - 128.0 / 255.0) * (255.0 / 224.0);
    }

    var rgb: vec3<f32>;
    if (p.matrix == 1u) {
        rgb = vec3<f32>(
            yy + 1.5748 * cr,
            yy - 0.18733 * cb - 0.46813 * cr,
            yy + 1.8556 * cb,
        );
    } else {
        rgb = vec3<f32>(
            yy + 1.402 * cr,
            yy - 0.344136 * cb - 0.714136 * cr,
            yy + 1.772 * cb,
        );
    }
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
