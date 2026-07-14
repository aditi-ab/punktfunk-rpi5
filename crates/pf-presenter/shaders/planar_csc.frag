// Planar 3-plane YCbCr → RGBA — the PyroWave variant of nv12_csc.frag (separate Cb and
// Cr R8 planes instead of an interleaved CbCr plane; design/pyrowave-codec-plan.md §4.5).
// Same push-constant contract (csc_rows precomputes the matrix + range expansion), same
// output modes — though PyroWave itself is 8-bit SDR BT.709 limited, keeping parity means
// one less divergence if the codec ever signals more. 4:4:4 needs no shader change: the
// chroma planes arrive full-res and the siting correction self-disables.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_y;
layout(set = 0, binding = 1) uniform sampler2D u_cb;
layout(set = 0, binding = 2) uniform sampler2D u_cr;

layout(push_constant) uniform Csc {
    vec4 r0;
    vec4 r1;
    vec4 r2;
    vec4 params; // x: mode, y: tonemap peak, z/w: reserved
} pc;

// SMPTE ST.2084 (PQ) EOTF: code value → display-referred linear, normalized to 1.0 =
// 10000 nits.
vec3 pq_eotf(vec3 e) {
    const float m1 = 0.1593017578125;  // 2610/16384
    const float m2 = 78.84375;         // 2523/4096 * 128
    const float c1 = 0.8359375;        // 3424/4096
    const float c2 = 18.8515625;       // 2413/4096 * 32
    const float c3 = 18.6875;          // 2392/4096 * 32
    vec3 p = pow(max(e, vec3(0.0)), vec3(1.0 / m2));
    return pow(max(p - c1, vec3(0.0)) / (c2 - c3 * p), vec3(1.0 / m1));
}

// BT.2020 → BT.709 primaries (linear light).
vec3 bt2020_to_709(vec3 c) {
    return mat3(
         1.6605, -0.1246, -0.0182,
        -0.5876,  1.1329, -0.1006,
        -0.0728, -0.0083,  1.1187
    ) * c;
}

// Linear → sRGB OETF.
vec3 srgb_oetf(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    bvec3 lo = lessThanEqual(c, vec3(0.0031308));
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(hi, c * 12.92, vec3(lo));
}

void main() {
    // Left-cosited 4:2:0 chroma sampled at luma UV assumes CENTER siting — offset +0.25
    // chroma texels to re-align (same correction as nv12_csc.frag; self-disables when the
    // chroma plane is full-res).
    vec2 cuv = v_uv;
    int cw = textureSize(u_cb, 0).x;
    if (cw < textureSize(u_y, 0).x) {
        cuv.x += 0.25 / float(cw);
    }
    vec3 yuv = vec3(texture(u_y, v_uv).r, texture(u_cb, cuv).r, texture(u_cr, cuv).r);
    vec3 rgb = vec3(
        dot(pc.r0.xyz, yuv) + pc.r0.w,
        dot(pc.r1.xyz, yuv) + pc.r1.w,
        dot(pc.r2.xyz, yuv) + pc.r2.w
    );

    if (pc.params.x > 0.5) {
        vec3 lin = pq_eotf(clamp(rgb, 0.0, 1.0)) * (10000.0 / 203.0);
        lin = max(bt2020_to_709(lin), vec3(0.0));
        float peak = max(pc.params.y, 1.0001);
        float l = max(lin.r, max(lin.g, lin.b));
        if (l > 1.0) {
            float mapped = 1.0 + (l - 1.0) / (1.0 + (l - 1.0) / (peak - 1.0));
            lin *= mapped / l;
        }
        rgb = srgb_oetf(lin);
    } else {
        rgb = clamp(rgb, 0.0, 1.0);
    }
    frag = vec4(rgb, 1.0);
}
