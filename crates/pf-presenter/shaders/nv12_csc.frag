// NV12 → RGBA with the stream's CICP signaling — the Vulkan port of the GL presenter's
// fragment shader (video_gl.rs). The YUV→RGB matrix + range expansion arrive as three
// push-constant rows precomputed on the CPU (csc.rs `csc_rows`): rgb[i] = dot(r_i.xyz,
// yuv) + r_i.w — one shader for BT.601/709/2020, full/limited. The chroma plane is
// half-res; the linear sampler interpolates, same as the GL path.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_y;
layout(set = 0, binding = 1) uniform sampler2D u_c;

layout(push_constant) uniform Csc {
    vec4 r0;
    vec4 r1;
    vec4 r2;
} pc;

void main() {
    vec3 yuv = vec3(texture(u_y, v_uv).r, texture(u_c, v_uv).rg);
    frag = vec4(
        clamp(
            vec3(
                dot(pc.r0.xyz, yuv) + pc.r0.w,
                dot(pc.r1.xyz, yuv) + pc.r1.w,
                dot(pc.r2.xyz, yuv) + pc.r2.w
            ),
            0.0,
            1.0
        ),
        1.0
    );
}
