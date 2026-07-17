// Cursor-overlay blend kernels for the CUDA/NVENC path (cursor-as-metadata). The cursor bitmap is
// straight-alpha RGBA, row-packed (stride = curW*4). Blended into the encoder-OWNED NVENC input
// surface — never the compositor's dmabuf. One thread per cursor pixel (ARGB / YUV444) or per 2x2
// chroma block (NV12). Coefficients are BT.709 limited, matching rgb2yuv.comp so the cursor colour
// matches the rest of the frame regardless of which zero-copy backend encodes it.
//
// Build (regenerate cursor_blend.ptx after editing):
//   nvcc -ptx -arch=compute_75 cursor_blend.cu -o cursor_blend.ptx
// PTX is JIT'd by the driver forward to the actual GPU, so a compute_75 (Turing) baseline runs on
// every Turing-or-newer NVENC GPU. (CUDA 13's nvcc no longer targets pre-Turing archs.)

typedef unsigned char u8;

__device__ __forceinline__ u8 blend8(int dst, int src, int a) {
    return (u8)((src * a + dst * (255 - a)) / 255);
}

// Packed 4-byte surface. NVENC's ARGB format stores bytes B,G,R,A in memory; the cursor is R,G,B,A.
extern "C" __global__ void blend_argb(
    u8* surf, int pitch, int surfW, int surfH,
    const u8* cur, int curW, int curH, int ox, int oy)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;
    int cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx >= curW || cy >= curH) return;
    int px = ox + cx, py = oy + cy;
    if (px < 0 || py < 0 || px >= surfW || py >= surfH) return;
    const u8* s = cur + (size_t)(cy * curW + cx) * 4;
    int a = s[3];
    if (a == 0) return;
    u8* d = surf + (size_t)py * pitch + (size_t)px * 4;
    d[0] = blend8(d[0], s[2], a); // B <- cursor B
    d[1] = blend8(d[1], s[1], a); // G <- cursor G
    d[2] = blend8(d[2], s[0], a); // R <- cursor R
}

// Planar YUV444: three full-res planes stacked at base, base+plane, base+2*plane (plane=pitch*surfH).
extern "C" __global__ void blend_yuv444(
    u8* base, int pitch, int surfW, int surfH,
    const u8* cur, int curW, int curH, int ox, int oy)
{
    int cx = blockIdx.x * blockDim.x + threadIdx.x;
    int cy = blockIdx.y * blockDim.y + threadIdx.y;
    if (cx >= curW || cy >= curH) return;
    int px = ox + cx, py = oy + cy;
    if (px < 0 || py < 0 || px >= surfW || py >= surfH) return;
    const u8* s = cur + (size_t)(cy * curW + cx) * 4;
    int a = s[3];
    if (a == 0) return;
    float R = s[0], G = s[1], B = s[2];
    int Y = (int)(16.0f + 0.1826f * R + 0.6142f * G + 0.0620f * B + 0.5f);
    int U = (int)(128.0f - 0.1006f * R - 0.3386f * G + 0.4392f * B + 0.5f);
    int V = (int)(128.0f + 0.4392f * R - 0.3989f * G - 0.0403f * B + 0.5f);
    size_t plane = (size_t)pitch * surfH;
    u8* yp = base + (size_t)py * pitch + px;
    u8* up = base + plane + (size_t)py * pitch + px;
    u8* vp = base + 2 * plane + (size_t)py * pitch + px;
    *yp = blend8(*yp, Y, a);
    *up = blend8(*up, U, a);
    *vp = blend8(*vp, V, a);
}

// NV12: full-res Y plane + interleaved half-res UV plane. One thread per 2x2 luma block; each blends
// up to four Y samples and one (alpha-weighted) UV sample.
extern "C" __global__ void blend_nv12(
    u8* yb, int yPitch, u8* uvb, int uvPitch, int surfW, int surfH,
    const u8* cur, int curW, int curH, int ox, int oy)
{
    int bx = blockIdx.x * blockDim.x + threadIdx.x;
    int by = blockIdx.y * blockDim.y + threadIdx.y;
    int base_cx = bx * 2, base_cy = by * 2;
    if (base_cx >= curW || base_cy >= curH) return;
    float ua = 0.0f, va = 0.0f, wa = 0.0f;
    int cnt = 0;
    for (int j = 0; j < 2; j++) {
        for (int i = 0; i < 2; i++) {
            int cx = base_cx + i, cy = base_cy + j;
            if (cx >= curW || cy >= curH) continue;
            int px = ox + cx, py = oy + cy;
            if (px < 0 || py < 0 || px >= surfW || py >= surfH) continue;
            const u8* s = cur + (size_t)(cy * curW + cx) * 4;
            int a = s[3];
            if (a == 0) continue;
            float R = s[0], G = s[1], B = s[2];
            int Y = (int)(16.0f + 0.1826f * R + 0.6142f * G + 0.0620f * B + 0.5f);
            u8* yp = yb + (size_t)py * yPitch + px;
            *yp = blend8(*yp, Y, a);
            ua += (128.0f - 0.1006f * R - 0.3386f * G + 0.4392f * B) * a;
            va += (128.0f + 0.4392f * R - 0.3989f * G - 0.0403f * B) * a;
            wa += a;
            cnt++;
        }
    }
    if (wa <= 0.0f || cnt == 0) return;
    // The chroma sample covering this block's top-left surface pixel.
    int uvx = (ox + base_cx) / 2;
    int uvy = (oy + base_cy) / 2;
    if (uvx < 0 || uvy < 0 || uvx * 2 >= surfW || uvy * 2 >= surfH) return;
    int U = (int)(ua / wa + 0.5f);
    int V = (int)(va / wa + 0.5f);
    int amean = (int)(wa / cnt + 0.5f);
    u8* uv = uvb + (size_t)uvy * uvPitch + (size_t)uvx * 2;
    uv[0] = blend8(uv[0], U, amean);
    uv[1] = blend8(uv[1], V, amean);
}
