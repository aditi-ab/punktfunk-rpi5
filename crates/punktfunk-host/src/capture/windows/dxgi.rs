//! Windows capture GPU mechanics — the win32u GPU-preference hook, HLSL shader compilation, HDR
//! FP16→P010 conversion ([`HdrP010Converter`]), video-engine colour conversion ([`VideoConverter`]),
//! and the P010 self-test. Consumed by [`super::idd_push`].
//!
//! The shared IDD-push capture IDENTITY — [`WinCaptureTarget`], [`D3d11Frame`], [`pack_luid`], and
//! [`make_device`] (the D3D11 device factory + GPU scheduling-priority hardening) — moved into the
//! `pf-frame` leaf crate so capture, encode, and pf-vdisplay share one identity type without a
//! capture↔encode↔vdisplay cycle (plan §W6); this module re-exports it so every existing
//! `crate::capture::dxgi::*` path keeps resolving. DXGI Desktop Duplication has been removed; this
//! module contains no capturer.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

pub use pf_frame::dxgi::{make_device, pack_luid, D3d11Frame, WinCaptureTarget};

use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D,
    ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER, D3D11_CPU_ACCESS_READ,
    D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_POINT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE_DISCARD,
    D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
    D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV,
    D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT, D3D11_USAGE_DYNAMIC,
    D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC,
};

/// How many times DXGI has actually called our hooked `NtGdiDdDDIGetCachedHybridQueryValue`. If this
/// stays 0 while DDA churns with ACCESS_LOST, the hook is NOT on DXGI's GPU-preference path on this
/// build (so reparenting can't be the cause — look at composition/independent-flip instead). >0 with
/// continuing churn means the hook fires but reparenting isn't the trigger here.
static HYBRID_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

// kernel32 — declared directly so we don't pull the whole Win32_System_Diagnostics_Debug feature for
// one call. FlushInstructionCache serializes the i-cache after the inline patch: the patch is written
// on the main thread but DXGI runs the hooked export from the encode/worker thread (possibly a
// different core), so the "same-thread, no flush needed" assumption was wrong.
#[link(name = "kernel32")]
extern "system" {
    fn FlushInstructionCache(h: *mut c_void, base: *const c_void, size: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}
/// Replacement for `win32u.dll!NtGdiDdDDIGetCachedHybridQueryValue`: always report
/// `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` (3). We fully replace the function (never call the
/// original), so no trampoline is needed. (Independent reimplementation of the same technique Apollo
/// uses: Apollo installs its hook via the MinHook library; this is an original inline byte-patch and
/// copies no Apollo/GPL source.)
unsafe extern "system" fn hybrid_query_hook(gpu_preference: *mut u32) -> i32 {
    HYBRID_HOOK_HITS.fetch_add(1, Ordering::Relaxed);
    if gpu_preference.is_null() {
        return 0xC000_000Du32 as i32; // STATUS_INVALID_PARAMETER
    }
    *gpu_preference = 3; // D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED
    0 // STATUS_SUCCESS
}

/// The win32u GPU-preference hook (the same technique Apollo applies, reimplemented here from the
/// documented DDI — no GPL source copied). On a HYBRID-GPU box DXGI resolves a GPU preference
/// (registry + power settings + the hybrid-adapter DDI) and REPARENTS outputs onto the chosen render
/// GPU — which constantly invalidates Desktop Duplication (DXGI_ERROR_ACCESS_LOST 0x887A0026, the
/// freeze/churn observed on the RTX 4090 + AMD iGPU box; `SET_RENDER_ADAPTER` is ignored there). Faking
/// a cached preference of UNSPECIFIED makes DXGI skip the resolution, so the output is NOT reparented
/// and DDA stays stable on one adapter (this is what makes Apollo's DDA work on this hardware).
/// Installed once, before the first DXGI factory/enumeration; lasts the process lifetime (like Apollo).
pub(crate) fn install_gpu_pref_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    // SAFETY: this one-time hook install only touches a region it has just validated.
    // `LoadLibraryA("win32u.dll")` + `GetProcAddress("NtGdiDdDDIGetCachedHybridQueryValue")` yield the
    // live base of the real exported function, so `target` is a valid executable code pointer to at
    // least the 12 bytes the patch overwrites (an x64 prologue). The two
    // `ptr::copy_nonoverlapping`s each move exactly 12 bytes between the 12-byte stack arrays
    // (`patch`/`readback`) and `target`, which `VirtualProtect(target, 12, PAGE_EXECUTE_READWRITE, …)`
    // has just made writable (and is restored to `old` after) — source and dest never overlap (stack
    // vs. loaded module image), so every access stays in mapped, in-bounds memory.
    // `FlushInstructionCache` gets the current-process pseudo-handle + that same range. The DPI calls
    // take by-value context handles / fill the live local `&mut old`/`&mut restore` for the duration of
    // each synchronous call. Runs once via `Once::call_once`, before any DXGI use.
    HOOK.call_once(|| unsafe {
        use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
        use windows::Win32::System::Memory::{
            VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
        };
        use windows::Win32::UI::HiDpi::{
            GetAwarenessFromDpiAwarenessContext, GetThreadDpiAwarenessContext,
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        // Per-monitor-v2 DPI awareness — REQUIRED for IDXGIOutput5::DuplicateOutput1 (without it the
        // call returns E_ACCESSDENIED forever, forcing the legacy DuplicateOutput path). Matches
        // Apollo's startup. SetProcessDpiAwarenessContext fails with E_ACCESS_DENIED if awareness was
        // already set (manifest / earlier call) — log the outcome AND the effective awareness so a
        // 100% DuplicateOutput1 E_ACCESSDENIED is diagnosable instead of silent.
        match SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            Ok(()) => tracing::info!("DPI awareness set: PER_MONITOR_AWARE_V2"),
            Err(e) => tracing::warn!(error = ?e,
                "SetProcessDpiAwarenessContext failed (already set?) — DuplicateOutput1 may E_ACCESSDENIED"),
        }
        // 0=UNAWARE 1=SYSTEM 2=PER_MONITOR(_V2). DuplicateOutput1 needs 2.
        let awareness = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()).0;
        tracing::info!(awareness, "effective DPI awareness (need 2=PER_MONITOR for DuplicateOutput1)");
        let Ok(lib) = LoadLibraryA(s!("win32u.dll")) else {
            tracing::warn!("GPU-pref hook: win32u.dll not loadable — skipping (DDA may churn on hybrid GPUs)");
            return;
        };
        let Some(target) = GetProcAddress(lib, s!("NtGdiDdDDIGetCachedHybridQueryValue")) else {
            tracing::warn!("GPU-pref hook: NtGdiDdDDIGetCachedHybridQueryValue not exported — skipping");
            return;
        };
        let target = target as usize as *mut u8;
        // x64 absolute jump to our replacement: `mov rax, imm64 ; jmp rax` (12 bytes). We never call the
        // original, so no trampoline/relocation (hence no detour crate / C length-disassembler dep).
        let hook = hybrid_query_hook as *const () as usize;
        let mut patch = [0u8; 12];
        patch[0] = 0x48;
        patch[1] = 0xB8; // mov rax, imm64
        patch[2..10].copy_from_slice(&hook.to_le_bytes());
        patch[10] = 0xFF;
        patch[11] = 0xE0; // jmp rax
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if VirtualProtect(target as *const c_void, 12, PAGE_EXECUTE_READWRITE, &mut old).is_err() {
            tracing::warn!("GPU-pref hook: VirtualProtect failed — skipping");
            return;
        }
        std::ptr::copy_nonoverlapping(patch.as_ptr(), target, 12);
        let mut restore = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(target as *const c_void, 12, old, &mut restore);
        // Serialize the i-cache: the patch is written here (main thread) but DXGI calls the export from
        // the capture/encode worker thread — possibly a different core with a stale i-cache, in which
        // case it would keep running the ORIGINAL function and DXGI would still reparent. (Apollo's
        // MinHook does this flush internally; our hand-rolled patch must do it explicitly.)
        let _ = FlushInstructionCache(GetCurrentProcess(), target as *const c_void, 12);
        // VERIFY the patch actually landed (CFG/hotpatch/short-stub could silently reject it). Read it
        // back; an error! (not a cheery "installed") makes a dead hook obvious in the logs.
        let mut readback = [0u8; 12];
        std::ptr::copy_nonoverlapping(target, readback.as_mut_ptr(), 12);
        if readback == patch {
            tracing::info!(
                "GPU-pref hook installed + verified (win32u hybrid-query -> UNSPECIFIED): reparenting disabled"
            );
        } else {
            tracing::error!(
                want = %format!("{patch:02x?}"), got = %format!("{readback:02x?}"),
                "GPU-pref hook patch did NOT land — hook is DEAD (DXGI will still reparent → ACCESS_LOST churn)"
            );
        }
    });
}

unsafe fn compile_shader(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    let mut blob: Option<ID3DBlob> = None;
    let mut errs: Option<ID3DBlob> = None;
    let r = D3DCompile(
        src.as_ptr() as *const c_void,
        src.len(),
        PCSTR::null(),
        None,
        None,
        entry,
        target,
        0,
        0,
        &mut blob,
        Some(&mut errs),
    );
    if r.is_err() {
        let msg = errs
            .as_ref()
            .map(|e| {
                let p = e.GetBufferPointer() as *const u8;
                String::from_utf8_lossy(std::slice::from_raw_parts(p, e.GetBufferSize()))
                    .to_string()
            })
            .unwrap_or_default();
        bail!("D3DCompile failed: {msg}");
    }
    let blob = blob.context("no shader blob")?;
    let p = blob.GetBufferPointer() as *const u8;
    Ok(std::slice::from_raw_parts(p, blob.GetBufferSize()).to_vec())
}

/// Fullscreen-triangle vertex shader for the HDR conversion pass (3 verts, no input layout).
const HDR_VS: &str = r"
struct VOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VOut main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VOut o;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}
";

/// P010 **luma** pixel shader: scRGB FP16 desktop (linear, Rec.709 primaries, 1.0 = 80 nits) →
/// BT.2020 PQ → BT.2020 non-constant-luminance limited-range Y′, written as a 10-bit code in the high
/// 10 bits of an R16_UNORM render-target view of the P010 plane-0 (luma). The colour pipeline
/// (scRGB→nits→BT.2020-linear→PQ) is IDENTICAL to the R10 HDR path; only the final RGB→Y + studio-range
/// quantization differs. The shared HLSL is factored into [`HDR_P010_COMMON`].
const HDR_P010_COMMON: &str = r"
Texture2D<float4> tx : register(t0);
SamplerState sm : register(s0);
// Rec.709 → Rec.2020 primaries (linear). Same matrix as the R10 HdrConverter (mul(M, v)).
static const float3x3 BT709_TO_BT2020 = {
    0.627403914, 0.329283038, 0.043313048,
    0.069097292, 0.919540405, 0.011362303,
    0.016391439, 0.088013308, 0.895595253
};
float3 pq_oetf(float3 L) {
    // L normalized so 1.0 = 10000 nits. ST 2084. (Identical to HdrConverter.)
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 Lp = pow(saturate(L), m1);
    return pow((c1 + c2 * Lp) / (1.0 + c3 * Lp), m2);
}
// scRGB FP16 sample -> PQ-encoded BT.2020 RGB in [0,1] (the SAME pixels the R10 path would store,
// before quantization). Used by both the luma and chroma passes so they agree bit-for-bit with the
// existing HdrConverter colour math + the Rust reference.
float3 scrgb_to_pq2020(float2 uv) {
    float3 scrgb = max(tx.Sample(sm, uv).rgb, 0.0); // scRGB can be negative (wide gamut); clamp
    float3 nits = scrgb * 80.0;                      // scRGB 1.0 = 80 nits
    float3 lin2020 = mul(BT709_TO_BT2020, nits);     // primaries conversion (linear)
    return pq_oetf(lin2020 / 10000.0);               // normalize to 10k nits, encode PQ -> [0,1]
}
// BT.2020 non-constant-luminance, on the PQ-encoded (gamma) RGB. Kr/Kg/Kb per Rec.2020.
static const float KR = 0.2627;
static const float KG = 0.6780;
static const float KB = 0.0593;
// 10-bit studio (limited) range codes. Y'  -> [64, 940]; Cb/Cr -> [64, 960] (512 ± 448).
float studio_y_code(float3 rgb_pq) {
    float y = KR * rgb_pq.r + KG * rgb_pq.g + KB * rgb_pq.b;     // [0,1]
    float code = 64.0 + 876.0 * y;                              // [64, 940]
    return clamp(code, 64.0, 940.0);
}
float2 studio_cbcr_code(float3 rgb_pq) {
    float y = KR * rgb_pq.r + KG * rgb_pq.g + KB * rgb_pq.b;
    float cb = (rgb_pq.b - y) / 1.8814;                          // ~[-0.5, 0.5]
    float cr = (rgb_pq.r - y) / 1.4746;
    float cbc = 512.0 + 896.0 * cb;                             // [64, 960]
    float crc = 512.0 + 896.0 * cr;
    return float2(clamp(cbc, 64.0, 960.0), clamp(crc, 64.0, 960.0));
}
// P010 stores the 10-bit code in the HIGH 10 bits of each 16-bit sample (code10 << 6). As an
// R16_UNORM / R16G16_UNORM render target the UNORM float that maps to that stored u16 is
// code10*64 / 65535.0. (Verified in hdr_p010_selftest against the readback.)
float code10_to_unorm(float code10) { return (code10 * 64.0) / 65535.0; }
";

/// P010 LUMA pass PS — full-res, writes Y′ to plane 0 (R16_UNORM RTV).
const HDR_P010_Y_PS: &str = r"
#include_common
float main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 pq = scrgb_to_pq2020(uv);
    float yc = studio_y_code(pq);
    return code10_to_unorm(yc);
}
";

/// P010 CHROMA pass PS — half-res, writes interleaved (Cb,Cr) to plane 1 (R16G16_UNORM RTV).
/// **Left-cosited** (H.273 chroma_loc type 0 — the default every decoder infers when
/// chroma_loc_info is unsignaled, and what the clients' sampling corrections assume): the chroma
/// sample sits ON the even luma column, vertically centered between its two rows — so the filter
/// is the 2-row average of that ONE column, IN scRGB-linear space before the PQ encode, then
/// Cb/Cr from the averaged-then-PQ-encoded RGB. (The old 2×2 box was CENTER-sited — a
/// half-luma-pixel chroma shift against what decoders reconstruct; the narrow column decimation
/// also keeps desktop text/edge chroma crisp, and block-uniform inputs stay exact for
/// `hdr_p010_selftest`.) `inv_src` = (1/srcW, 1/srcH).
const HDR_P010_UV_PS: &str = r"
#include_common
cbuffer C : register(b0) { float2 inv_src; float2 pad; };
float2 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    // `uv` is the chroma RT texel centre = the middle of the 2x2 luma block; the left-cosited
    // target is the block's LEFT column, whose two texel centres sit at uv + (-h.x, ±h.y).
    float2 h = inv_src * 0.5;
    float3 a = max(tx.Sample(sm, uv + float2(-h.x, -h.y)).rgb, 0.0);
    float3 b = max(tx.Sample(sm, uv + float2(-h.x,  h.y)).rgb, 0.0);
    float3 scrgb = (a + b) * 0.5;
    float3 nits = scrgb * 80.0;
    float3 lin2020 = mul(BT709_TO_BT2020, nits);
    float3 pq = pq_oetf(lin2020 / 10000.0);
    float2 cc = studio_cbcr_code(pq);
    return float2(code10_to_unorm(cc.x), code10_to_unorm(cc.y));
}
";

/// scRGB FP16 → **P010** (BT.2020 PQ, 10-bit limited/studio range) conversion, in OUR OWN shader (two
/// passes: full-res luma + half-res chroma). NVIDIA's D3D11 VideoProcessor cannot do RGB→P010 (renders
/// green), so we quantize to studio-range 10-bit YUV directly and feed NVENC native P010 — skipping
/// NVENC's internal RGB→YUV CSC (which runs on the contended SM). One per capture device (rebuilt on
/// device recreate).
///
/// Plane writes use per-plane render-target views of the single P010 texture: an `R16_UNORM` RTV
/// selects plane 0 (luma, full WxH), an `R16G16_UNORM` RTV selects plane 1 (chroma, W/2 x H/2). This
/// planar-RTV mechanism needs a D3D11.3+ runtime + driver support; [`HdrP010Converter::convert`]
/// surfaces a clear error if `CreateRenderTargetView` rejects the plane format so the caller can fall
/// back to the existing R10 path.
pub(crate) struct HdrP010Converter {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// Constant buffer for the chroma pass (inv_src texel size). 16 bytes.
    cbuf: ID3D11Buffer,
}

impl HdrP010Converter {
    pub(crate) unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        // Inline the shared HLSL (D3DCompile has no include handler wired here). The two PS sources
        // carry a `#include_common` marker we substitute before compiling.
        let y_src = HDR_P010_Y_PS.replace("#include_common", HDR_P010_COMMON);
        let uv_src = HDR_P010_UV_PS.replace("#include_common", HDR_P010_COMMON);
        let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
        let yb = compile_shader(&y_src, s!("main"), s!("ps_5_0"))?;
        let uvb = compile_shader(&uv_src, s!("main"), s!("ps_5_0"))?;
        let mut vs = None;
        device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
        let mut ps_y = None;
        device.CreatePixelShader(&yb, None, Some(&mut ps_y))?;
        let mut ps_uv = None;
        device.CreatePixelShader(&uvb, None, Some(&mut ps_uv))?;
        let sd = D3D11_SAMPLER_DESC {
            // POINT: the Y pass samples a single texel centre exactly, and the UV pass does its OWN
            // 2x2 box average via 4 explicit taps at texel centres (offset half a texel). Point
            // sampling keeps each tap exact; the averaging is in the shader, not the sampler.
            Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        device.CreateSamplerState(&sd, Some(&mut sampler))?;
        let cbd = D3D11_BUFFER_DESC {
            ByteWidth: 16, // float2 inv_src + float2 pad
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cbuf = None;
        device.CreateBuffer(&cbd, None, Some(&mut cbuf))?;
        Ok(Self {
            vs: vs.context("p010 vs")?,
            ps_y: ps_y.context("p010 y ps")?,
            ps_uv: ps_uv.context("p010 uv ps")?,
            sampler: sampler.context("p010 sampler")?,
            cbuf: cbuf.context("p010 cbuf")?,
        })
    }

    /// Create a per-plane RTV of the P010 texture `dst` with the given single-plane `format`
    /// (`R16_UNORM` for plane 0 luma, `R16G16_UNORM` for plane 1 chroma). The plane is selected by the
    /// view format (planar-RTV semantics); MipSlice 0.
    unsafe fn plane_rtv(
        device: &ID3D11Device,
        dst: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11RenderTargetView> {
        let desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
            },
        };
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        device
            .CreateRenderTargetView(
                dst,
                Some(&desc as *const D3D11_RENDER_TARGET_VIEW_DESC),
                Some(&mut rtv),
            )
            .with_context(|| {
                format!("CreateRenderTargetView(P010 plane, format={format:?}) — driver may not support planar RTVs")
            })?;
        rtv.context("p010 plane rtv null")
    }

    /// Convert `src_srv` (FP16 scRGB, WxH) into `dst` (a `DXGI_FORMAT_P010` texture with
    /// `BIND_RENDER_TARGET`). Two opaque passes: full-res luma → plane 0, half-res chroma → plane 1.
    /// `w`/`h` are the full luma dimensions (must be even). Returns `Err` if a plane RTV can't be
    /// created (driver) so the caller can fall back to the R10 path.
    pub(crate) unsafe fn convert(
        &self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        dst: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Result<()> {
        let y_rtv = Self::plane_rtv(device, dst, DXGI_FORMAT_R16_UNORM)?;
        let uv_rtv = Self::plane_rtv(device, dst, DXGI_FORMAT_R16G16_UNORM)?;

        // Update the chroma constant buffer (inverse source texel size).
        let cb: [f32; 4] = [1.0 / w as f32, 1.0 / h as f32, 0.0, 0.0];
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if ctx
            .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .is_ok()
        {
            std::ptr::copy_nonoverlapping(cb.as_ptr(), mapped.pData as *mut f32, cb.len());
            ctx.Unmap(&self.cbuf, 0);
        }

        // Shared pipeline state.
        ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
        ctx.VSSetShader(&self.vs, None);
        ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
        ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        ctx.IASetInputLayout(None);
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

        // --- LUMA pass: full-res, plane 0 ---
        let vp_y = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        ctx.RSSetViewports(Some(&[vp_y]));
        ctx.OMSetRenderTargets(Some(&[Some(y_rtv.clone())]), None);
        ctx.PSSetShader(&self.ps_y, None);
        ctx.Draw(3, 0);
        ctx.OMSetRenderTargets(Some(&[None]), None);

        // --- CHROMA pass: half-res, plane 1 ---
        let vp_uv = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: (w / 2) as f32,
            Height: (h / 2) as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        ctx.RSSetViewports(Some(&[vp_uv]));
        ctx.OMSetRenderTargets(Some(&[Some(uv_rtv.clone())]), None);
        ctx.PSSetShader(&self.ps_uv, None);
        ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
        ctx.Draw(3, 0);

        // Unbind for the next frame's re-RTV / NVENC read.
        ctx.OMSetRenderTargets(Some(&[None]), None);
        ctx.PSSetShaderResources(0, Some(&[None]));
        Ok(())
    }
}

/// f64 reference for the P010 colour math — the EXACT analogue of the HLSL in [`HDR_P010_COMMON`].
/// Input is one scRGB pixel (linear, Rec.709 primaries, 1.0 = 80 nits, may be >1 for HDR). Output is
/// the 10-bit studio-range (Y, Cb, Cr) codes the shader should produce for a flat (constant) block.
/// Used by [`hdr_p010_selftest`].
#[cfg(target_os = "windows")]
fn p010_reference(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    fn pq_oetf(l: f64) -> f64 {
        let l = l.clamp(0.0, 1.0);
        let m1 = 0.1593017578125;
        let m2 = 78.84375;
        let c1 = 0.8359375;
        let c2 = 18.8515625;
        let c3 = 18.6875;
        let lp = l.powf(m1);
        ((c1 + c2 * lp) / (1.0 + c3 * lp)).powf(m2)
    }
    // scRGB -> nits -> BT.2020 linear (row-major matrix, mul(M, v)).
    let (r, g, b) = (r.max(0.0) * 80.0, g.max(0.0) * 80.0, b.max(0.0) * 80.0);
    let m = [
        [0.627403914, 0.329283038, 0.043313048],
        [0.069097292, 0.919540405, 0.011362303],
        [0.016391439, 0.088013308, 0.895595253],
    ];
    let lr = m[0][0] * r + m[0][1] * g + m[0][2] * b;
    let lg = m[1][0] * r + m[1][1] * g + m[1][2] * b;
    let lb = m[2][0] * r + m[2][1] * g + m[2][2] * b;
    // PQ encode (normalize to 10k nits).
    let pr = pq_oetf(lr / 10000.0);
    let pg = pq_oetf(lg / 10000.0);
    let pb = pq_oetf(lb / 10000.0);
    // BT.2020 non-constant-luminance, limited 10-bit.
    let (kr, kg, kb) = (0.2627, 0.6780, 0.0593);
    let y = kr * pr + kg * pg + kb * pb;
    let cb = (pb - y) / 1.8814;
    let cr = (pr - y) / 1.4746;
    let yc = (64.0 + 876.0 * y).clamp(64.0, 940.0);
    let cbc = (512.0 + 896.0 * cb).clamp(64.0, 960.0);
    let crc = (512.0 + 896.0 * cr).clamp(64.0, 960.0);
    (yc, cbc, crc)
}

/// Colour self-test for [`HdrP010Converter`] (the `hdr-p010-selftest` subcommand): create a hardware
/// D3D11 device, upload a known scRGB FP16 pattern, run the P010 shader passes, read the Y (plane 0)
/// and UV (plane 1) planes back from a staging copy, and compare against the [`p010_reference`] f64
/// math. The ONLY validation we have without green-screening a live HDR stream. PASS if max abs error
/// Y ≤ 4 codes, U/V ≤ 5 codes (rounding + chroma averaging). Prints a per-colour table + PASS/FAIL.
#[cfg(target_os = "windows")]
pub fn hdr_p010_selftest() -> Result<()> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    // 64x64, even dims. A 4x4 grid of 16x16 flat scRGB blocks (each 2x2 chroma footprint uniform →
    // exact chroma comparison) covering pure R/G/B/white/black/gray at plausible HDR nit levels, plus
    // a couple of bright (>1.0 scRGB) colours, then the rest is a gradient (compared on Y only).
    const W: u32 = 64;
    const H: u32 = 64;
    const BLK: u32 = 16;
    // (name, r, g, b) scRGB linear (1.0 = 80 nits). Mix of SDR-ish and HDR (>1.0) values.
    let named: [(&str, f32, f32, f32); 8] = [
        ("red1.0", 1.0, 0.0, 0.0),
        ("green0.5", 0.0, 0.5, 0.0),
        ("blue4.0", 0.0, 0.0, 4.0),
        ("white1.0", 1.0, 1.0, 1.0),
        ("black", 0.0, 0.0, 0.0),
        ("gray0.5", 0.5, 0.5, 0.5),
        ("white4.0", 4.0, 4.0, 4.0),
        ("amber2.0", 2.0, 1.0, 0.0),
    ];

    let grid_cols = W / BLK; // 4
    let pixel_rgb = |x: u32, y: u32| -> (f32, f32, f32, bool) {
        let idx = ((y / BLK) * grid_cols + (x / BLK)) as usize;
        if idx < named.len() {
            let (_, r, g, b) = named[idx];
            (r, g, b, true)
        } else {
            // Gradient (distinct per pixel; Y-only compare), within HDR scRGB range.
            let r = (x as f32 / W as f32) * 3.0;
            let g = (y as f32 / H as f32) * 3.0;
            let b = ((x + y) as f32 / (W + H) as f32) * 3.0;
            (r, g, b, false)
        }
    };

    // Build the scRGB FP16 (R16G16B16A16_FLOAT) source as f16 bits.
    let mut fp16 = vec![0u16; (W * H * 4) as usize];
    let mut flat = vec![false; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, is_flat) = pixel_rgb(x, y);
            let i = ((y * W + x) * 4) as usize;
            fp16[i] = f32_to_f16(r);
            fp16[i + 1] = f32_to_f16(g);
            fp16[i + 2] = f32_to_f16(b);
            fp16[i + 3] = f32_to_f16(1.0);
            flat[(y * W + x) as usize] = is_flat;
        }
    }

    // SAFETY: this self-test creates its own D3D11 device + immediate context (`D3D11CreateDevice`,
    // both checked non-null) and uses ONLY that device for the rest of the block: every
    // `CreateTexture2D`/`CreateShaderResourceView`/`HdrP010Converter::{new,convert}`/`CopyResource`/
    // `Map` is invoked on that device or its context, so all resources share one device and run on this
    // single thread. The source texture's `D3D11_SUBRESOURCE_DATA` points at `fp16`, a live
    // `Vec<u16>` of `W*H*4` samples with `SysMemPitch = W*8`, matching the W×H R16G16B16A16 texture;
    // `fp16` outlives the synchronous `CreateTexture2D` that reads it. The mapped-pointer reads are
    // proven individually at the `read_u16` closure below.
    unsafe {
        // Hardware D3D11 device (no adapter pin — the default GPU is fine for the self-test).
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice(hardware) for hdr-p010-selftest")?;
        let device = device.context("null device")?;
        let context = context.context("null context")?;

        // Source FP16 texture (initialized) + SRV.
        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: fp16.as_ptr() as *const c_void,
            SysMemPitch: W * 8, // 4 channels * 2 bytes
            SysMemSlicePitch: 0,
        };
        let mut src_tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex))
            .context("CreateTexture2D(fp16 src)")?;
        let src_tex = src_tex.context("null src tex")?;
        let mut src_srv: Option<ID3D11ShaderResourceView> = None;
        device
            .CreateShaderResourceView(&src_tex, None, Some(&mut src_srv))
            .context("CreateShaderResourceView(fp16 src)")?;
        let src_srv = src_srv.context("null src srv")?;

        // P010 destination texture (render-target bindable).
        let p010_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut p010: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&p010_desc, None, Some(&mut p010))
            .context("CreateTexture2D(P010 dst)")?;
        let p010 = p010.context("null p010 tex")?;

        let conv = HdrP010Converter::new(&device)?;
        conv.convert(&device, &context, &src_srv, &p010, W, H)?;

        // Staging copy of the whole P010 texture (both planes), MAP_READ.
        let stage_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&stage_desc, None, Some(&mut staging))
            .context("CreateTexture2D(P010 staging)")?;
        let staging = staging.context("null staging")?;
        context.CopyResource(&staging, &p010);

        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
            .context("Map(P010 staging)")?;
        let row_pitch = map.RowPitch as usize; // bytes per luma row (in 16-bit samples: /2)
        let base = map.pData as *const u8;
        // DIAGNOSTIC (the uncertain layout spot — verify on the box if chroma is wrong): the mapped
        // P010 plane offsets. Plane 0 (luma): H rows of W u16. Plane 1 (chroma): H/2 rows of W/2
        // *interleaved* (Cb,Cr) u16 pairs. P010 packs plane 1 after plane 0 at the SAME row pitch; the
        // chroma plane begins at byte offset RowPitch * (luma height). For a STAGING texture that
        // height is the created H (no inter-plane alignment). DepthPitch (total mapped size) lets us
        // sanity-check: it should be ~ RowPitch * H * 3/2. If chroma reads garbage on the box, print
        // these and adjust `chroma_base` (e.g. an aligned luma height).
        tracing::info!(
            row_pitch,
            depth_pitch = map.DepthPitch,
            expected_chroma_base = row_pitch * H as usize,
            expected_total = row_pitch * H as usize * 3 / 2,
            "hdr-p010-selftest: mapped P010 layout (verify chroma plane offset here if chroma is wrong)"
        );
        // Plane 0 (luma): H rows of W u16. Plane 1 (chroma): H/2 rows of W/2 *interleaved* (Cb,Cr)
        // u16 pairs, i.e. W u16 per chroma row. P010 packs plane 1 immediately after plane 0 at the
        // SAME row pitch; per spec the chroma plane begins at an allocation offset of
        // RowPitch * Height (luma rows). We read it from there. (DepthPitch is the full surface size;
        // not all drivers report the chroma offset, so RowPitch*Height is the portable choice.)
        let read_u16 = |byte_off: usize| -> u16 {
            // SAFETY: `base` is the mapped staging pointer; all offsets are within the P010 surface
            // (luma H*RowPitch + chroma (H/2)*RowPitch ≤ DepthPitch). Already in the fn's unsafe scope.
            let p = base.add(byte_off) as *const u16;
            p.read_unaligned()
        };
        // Luma codes: stored u16 in the high 10 bits -> code10 = stored >> 6.
        let mut y_codes = vec![0u16; (W * H) as usize];
        for y in 0..H {
            for x in 0..W {
                let off = (y as usize) * row_pitch + (x as usize) * 2;
                y_codes[(y * W + x) as usize] = read_u16(off) >> 6;
            }
        }
        let cw = W / 2;
        let ch = H / 2;
        let chroma_base = row_pitch * H as usize; // plane 1 offset
        let mut cb_codes = vec![0u16; (cw * ch) as usize];
        let mut cr_codes = vec![0u16; (cw * ch) as usize];
        for cy in 0..ch {
            for cx in 0..cw {
                // Interleaved (Cb, Cr) per chroma sample → 2 u16 = 4 bytes per sample.
                let off = chroma_base + (cy as usize) * row_pitch + (cx as usize) * 4;
                cb_codes[(cy * cw + cx) as usize] = read_u16(off) >> 6;
                cr_codes[(cy * cw + cx) as usize] = read_u16(off + 2) >> 6;
            }
        }
        context.Unmap(&staging, 0);

        // Compare Y over every pixel.
        let mut max_y_err = 0.0f64;
        for y in 0..H {
            for x in 0..W {
                let (r, g, b, _) = pixel_rgb(x, y);
                let (ry, _, _) = p010_reference(r as f64, g as f64, b as f64);
                let got = y_codes[(y * W + x) as usize] as f64;
                max_y_err = max_y_err.max((got - ry).abs());
            }
        }
        // Compare Cb/Cr over flat blocks only (uniform 2x2 footprint → exact reference).
        let mut max_u_err = 0.0f64;
        let mut max_v_err = 0.0f64;
        for cy in 0..ch {
            for cx in 0..cw {
                let (sx, sy) = (cx * 2, cy * 2);
                let all_flat =
                    (0..2).all(|dy| (0..2).all(|dx| flat[((sy + dy) * W + (sx + dx)) as usize]));
                if !all_flat {
                    continue;
                }
                let (r, g, b, _) = pixel_rgb(sx, sy);
                let (_, rcb, rcr) = p010_reference(r as f64, g as f64, b as f64);
                let gu = cb_codes[(cy * cw + cx) as usize] as f64;
                let gv = cr_codes[(cy * cw + cx) as usize] as f64;
                max_u_err = max_u_err.max((gu - rcb).abs());
                max_v_err = max_v_err.max((gv - rcr).abs());
            }
        }

        // Per-colour table.
        println!("HDR P010 self-test ({W}x{H}, BT.2020 PQ, 10-bit limited range)");
        println!(
            "  {:<10} {:>14} {:>14} {:>14}",
            "color", "Y exp/got", "Cb exp/got", "Cr exp/got"
        );
        for (idx, (name, r, g, b)) in named.iter().enumerate() {
            let bx = (idx as u32 % grid_cols) * BLK + BLK / 2;
            let by = (idx as u32 / grid_cols) * BLK + BLK / 2;
            let (ey, ecb, ecr) = p010_reference(*r as f64, *g as f64, *b as f64);
            let gy = y_codes[(by * W + bx) as usize] as f64;
            let (ccx, ccy) = (bx / 2, by / 2);
            let gu = cb_codes[(ccy * cw + ccx) as usize] as f64;
            let gv = cr_codes[(ccy * cw + ccx) as usize] as f64;
            println!(
                "  {:<10} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0}",
                name, ey, gy, ecb, gu, ecr, gv
            );
        }
        println!(
            "  max abs error:  Y={max_y_err:.2} (≤4)   Cb={max_u_err:.2} (≤5)   Cr={max_v_err:.2} (≤5)"
        );

        if max_y_err <= 4.0 && max_u_err <= 5.0 && max_v_err <= 5.0 {
            println!("PASS");
            Ok(())
        } else {
            println!("FAIL");
            bail!(
                "HDR P010 self-test FAILED (Y={max_y_err:.2} Cb={max_u_err:.2} Cr={max_v_err:.2})"
            );
        }
    }
}

/// Minimal f32 → IEEE-754 half (f16) bit pattern, for uploading the FP16 scRGB self-test pattern. Not
/// on any hot path; handles normals, subnormals, and the 1.0/0.0 constants we feed. (round-to-nearest)
#[cfg(target_os = "windows")]
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x007f_ffff;
    if exp <= 0 {
        // Subnormal / zero in half precision.
        if exp < -10 {
            return sign; // too small → ±0
        }
        let mant = mant | 0x0080_0000; // implicit 1
        let shift = (14 - exp) as u32;
        let half_mant = (mant >> shift) as u16;
        // Round to nearest.
        let round = ((mant >> (shift - 1)) & 1) as u16;
        sign | (half_mant + round)
    } else if exp >= 0x1f {
        sign | 0x7c00 // Inf/NaN → Inf (our inputs never hit this)
    } else {
        let half_exp = (exp as u16) << 10;
        let half_mant = (mant >> 13) as u16;
        let round = ((mant >> 12) & 1) as u16;
        sign | half_exp | (half_mant + round)
    }
}

use windows::Win32::Graphics::Direct3D11::{
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
    DXGI_RATIONAL,
};

/// D3D11 **Video Processor** colour/format converter — runs on the GPU's dedicated VIDEO engine, NOT
/// the 3D engine, so the per-frame RGB→YUV conversion does not contend with a GPU-saturating game (the
/// HDR pixel-shader path and NVENC's internal RGB→YUV both use the 3D/compute engine, which an AAA
/// title pins at ~100%). Output is NV12 (SDR, BT.709 studio-range) or P010 (HDR, BT.2020 PQ
/// studio-range) — NVENC's native YUV inputs, so it encodes them with no further conversion.
pub(crate) struct VideoConverter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
}

impl VideoConverter {
    pub(crate) unsafe fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        hdr: bool,
    ) -> Result<Self> {
        let vdev: ID3D11VideoDevice = device.cast().context("device -> ID3D11VideoDevice")?;
        let vctx: ID3D11VideoContext1 = context.cast().context("context -> ID3D11VideoContext1")?;
        let rate = DXGI_RATIONAL {
            Numerator: 240,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumr = vdev
            .CreateVideoProcessorEnumerator(&desc)
            .context("CreateVideoProcessorEnumerator")?;
        let vp = vdev
            .CreateVideoProcessor(&enumr, 0)
            .context("CreateVideoProcessor")?;

        // Full-range RGB in → studio-range YUV out. HDR: scRGB linear (G10) → BT.2020 PQ (G2084).
        // SDR: sRGB (G22) → BT.709 (G22).
        let (in_cs, out_cs) = if hdr {
            (
                DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
            )
        } else {
            (
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
            )
        };
        vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, in_cs);
        vctx.VideoProcessorSetOutputColorSpace1(&vp, out_cs);
        // One frame in, one frame out — no interpolation/auto-processing.
        vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);

        Ok(Self {
            vdev,
            vctx,
            enumr,
            vp,
        })
    }

    /// Convert `input` (BGRA or scRGB FP16) → `output` (NV12 or P010) on the video engine. Views are
    /// created per call (cheap relative to the Blt) so the input texture can vary frame to frame.
    pub(crate) unsafe fn convert(
        &self,
        input: &ID3D11Texture2D,
        output: &ID3D11Texture2D,
    ) -> Result<()> {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        self.vdev
            .CreateVideoProcessorInputView(input, &self.enumr, &in_desc, Some(&mut in_view))
            .context("CreateVideoProcessorInputView")?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
        self.vdev
            .CreateVideoProcessorOutputView(output, &self.enumr, &out_desc, Some(&mut out_view))
            .context("CreateVideoProcessorOutputView")?;
        let out_view = out_view.context("null output view")?;

        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(in_view),
            ..Default::default()
        };
        let blt =
            self.vctx
                .VideoProcessorBlt(&self.vp, &out_view, 0, std::slice::from_ref(&stream));
        // COM in-params never transfer ownership: the Blt only borrowed the input view, and the
        // struct's `ManuallyDrop` field suppressed its release — drop it by hand, success or not.
        // (Skipping this leaked one view + its UMD allocation PER CONVERTED FRAME — the SDR hot
        // path; D3D11 defers the actual destruction until the GPU is done with the blit.)
        drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
        blt.context("VideoProcessorBlt")
    }
}
