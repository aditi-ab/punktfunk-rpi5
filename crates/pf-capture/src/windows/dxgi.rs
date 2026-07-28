//! Windows capture GPU mechanics — the win32u GPU-preference hook, HLSL shader compilation, HDR
//! FP16→P010 conversion ([`HdrP010Converter`]), video-engine colour conversion ([`VideoConverter`]),
//! and the P010 self-test. Consumed by [`super::idd_push`].
//!
//! The shared IDD-push capture IDENTITY — [`WinCaptureTarget`], [`D3d11Frame`], [`pack_luid`], and
//! [`make_device`] (the D3D11 device factory + GPU scheduling-priority hardening) — moved into the
//! `pf-frame` leaf crate so capture, encode, and pf-vdisplay share one identity type without a
//! capture↔encode↔vdisplay cycle (plan §W6); this module re-exports it so every existing
//! `crate::dxgi::*` path keeps resolving. DXGI Desktop Duplication has been removed; this
//! module contains no capturer.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

pub use pf_frame::dxgi::{make_device, pack_luid, D3d11Frame, PyroFrameShare, WinCaptureTarget};

// The P010 colour self-test (sweep Phase 5.5) — the `hdr-p010-selftest` subcommand, its f64
// reference math and the f16 uploader. None of it runs in a session; re-exported at the old paths
// (`main.rs` drives the subcommand, `pf-encode`'s qsv live e2e uses the bars helper).
// Explicit `#[path]`, like `idd_push`'s children: this file is itself reached through a `#[path]`
// from `lib.rs`, so a bare `mod selftest;` would resolve to `windows/selftest.rs`.
#[path = "dxgi/selftest.rs"]
mod selftest;
pub use selftest::{hdr_p010_convert_bars_on_luid, hdr_p010_selftest_at};

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
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_IMMUTABLE, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC,
};

/// How many times DXGI has actually called our hooked `NtGdiDdDDIGetCachedHybridQueryValue`.
/// Reported by [`hybrid_hook_hits`] on every IDD-push open, which is the first point at which DXGI
/// has been exercised (factory → `EnumAdapterByLuid` → device). The patch-readback check in
/// [`install_gpu_pref_hook`] proves the BYTES landed; only this counter proves DXGI actually
/// reaches the export on this build — so `0` here means the hook is inert and a
/// reparenting-flavoured symptom (see [`install_gpu_pref_hook`]) has some other cause.
static HYBRID_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

/// See [`HYBRID_HOOK_HITS`] — surfaced in the IDD-push open log so a dead hook is visible in the
/// field instead of being a silent write-only counter.
pub(crate) fn hybrid_hook_hits() -> u64 {
    HYBRID_HOOK_HITS.load(Ordering::Relaxed)
}

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
    // SAFETY: win32u's contract for this export — the caller (DXGI) passes a writable `*mut u32`
    // out-param — and the null case has just been rejected above, so this is an in-bounds,
    // 4-aligned single-word store into the caller's live local.
    unsafe { *gpu_preference = 3 }; // D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED
    0 // STATUS_SUCCESS
}

/// The win32u GPU-preference hook (the same technique Apollo applies, reimplemented here from the
/// documented DDI — no GPL source copied). On a HYBRID-GPU box DXGI resolves a GPU preference
/// (registry + power settings + the hybrid-adapter DDI) and REPARENTS outputs onto the chosen render
/// GPU, ignoring `SET_RENDER_ADAPTER` (observed on the RTX 4090 + AMD iGPU box). Faking a cached
/// preference of UNSPECIFIED makes DXGI skip that resolution, so an output is NOT reparented and
/// stays on one adapter.
///
/// **Why it is still installed now that DXGI Desktop Duplication is gone:** the IDD-push ring and
/// the driver's swap-chain must live on the SAME adapter, and the host pins that with
/// `SET_RENDER_ADAPTER` at monitor ADD (see `idd_push::open_inner`). A DXGI reparent moves the
/// virtual output off the pinned GPU behind the host's back — which surfaces as the driver's
/// `DRV_STATUS_TEX_FAIL` ("could not open our textures — render-adapter mismatch") and costs a
/// ring rebind. So the hook's job changed from "keep DDA on one adapter" to "keep the VIRTUAL
/// DISPLAY on the adapter we pinned"; the mechanism is unchanged. Installed once from
/// `main.rs`, before the virtual-display setup creates the first DXGI factory; lasts the process
/// lifetime. [`hybrid_hook_hits`] reports whether DXGI ever actually calls it.
pub fn install_gpu_pref_hook() {
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
        // Per-monitor-v2 DPI awareness. It was originally set here because
        // `IDXGIOutput5::DuplicateOutput1` returns E_ACCESSDENIED without it; DDA is gone, but the
        // awareness still matters — an UNAWARE/SYSTEM-aware process gets DPI-VIRTUALIZED window and
        // cursor coordinates, while every geometry the host computes comes from CCD in PHYSICAL
        // pixels (`source_desktop_rect`, `desktop_bounds`). Mixing the two mis-aims the compose
        // kick's `SetCursorPos` and the cursor-blend placement on any scaled display. Set here
        // because this is the earliest process-wide hook point. `SetProcessDpiAwarenessContext`
        // fails with E_ACCESS_DENIED if awareness was already set (manifest / earlier call) — log
        // the outcome AND the effective awareness so a mis-scaled pointer is diagnosable.
        match SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            Ok(()) => tracing::info!("DPI awareness set: PER_MONITOR_AWARE_V2"),
            Err(e) => tracing::warn!(error = ?e,
                "SetProcessDpiAwarenessContext failed (already set?) — cursor/desktop coordinates \
                 may be DPI-virtualized against the host's physical-pixel CCD geometry"),
        }
        // 0=UNAWARE 1=SYSTEM 2=PER_MONITOR(_V2). Physical-pixel coordinates need 2.
        let awareness = GetAwarenessFromDpiAwarenessContext(GetThreadDpiAwarenessContext()).0;
        tracing::info!(
            awareness,
            "effective DPI awareness (need 2=PER_MONITOR for physical-pixel coordinates)"
        );
        let Ok(lib) = LoadLibraryA(s!("win32u.dll")) else {
            tracing::warn!(
                "GPU-pref hook: win32u.dll not loadable — skipping (on a hybrid-GPU box DXGI may \
                 reparent the virtual display off the pinned render adapter → TEX_FAIL rebinds)"
            );
            return;
        };
        let Some(target) = GetProcAddress(lib, s!("NtGdiDdDDIGetCachedHybridQueryValue")) else {
            tracing::warn!(
                "GPU-pref hook: NtGdiDdDDIGetCachedHybridQueryValue not exported — skipping"
            );
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
        if VirtualProtect(
            target as *const c_void,
            12,
            PAGE_EXECUTE_READWRITE,
            &mut old,
        )
        .is_err()
        {
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
                "GPU-pref hook installed + verified (win32u hybrid-query -> UNSPECIFIED): DXGI \
                 output reparenting disabled. Whether DXGI actually CALLS it shows up as \
                 hybrid_hook_hits on the IDD-push open line."
            );
        } else {
            tracing::error!(
                want = %format!("{patch:02x?}"), got = %format!("{readback:02x?}"),
                "GPU-pref hook patch did NOT land — hook is DEAD (on a hybrid-GPU box DXGI can \
                 still reparent the virtual display off the pinned render adapter)"
            );
        }
    });
}

/// Compile one HLSL entry point. Returns the bytecode blob's bytes.
///
/// # Safety
/// `entry` and `target` must be valid NUL-terminated ASCII pointers (an `s!()` literal at every
/// call site); `src` is a plain Rust `&str`, read only for the duration of the call.
pub(crate) unsafe fn compile_shader(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    // SAFETY: `D3DCompile` reads `src.as_ptr()` for exactly `src.len()` bytes (a live `&str` that
    // outlives the synchronous call) plus the two caller-supplied NUL-terminated `PCSTR`s (per the
    // contract above); `&mut blob` / `Some(&mut errs)` are live out-params. Both
    // `slice::from_raw_parts` calls pair a blob's OWN `GetBufferPointer` with its OWN
    // `GetBufferSize` while that blob is still alive, and the slice is copied
    // (`to_string` / `to_vec`) before it goes out of scope.
    unsafe {
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
}

/// Fullscreen-triangle vertex shader for the HDR conversion pass (3 verts, no input layout).
pub(crate) const HDR_VS: &str = r"
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
/// surfaces a clear error if `CreateRenderTargetView` rejects the plane format. (There is no runtime
/// fallback — the error propagates through `try_consume` and ends the session; the "R10 path" the
/// original design referenced was never kept.)
pub(crate) struct HdrP010Converter {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// Constant buffer for the chroma pass: `inv_src` = (1/srcW, 1/srcH), 16 bytes. IMMUTABLE and
    /// filled at construction — it depends only on the source size, and the converter is already
    /// rebuilt whenever the mode changes. It used to be a DYNAMIC buffer that `convert` Mapped,
    /// wrote and Unmapped on EVERY HDR frame, inside the ring slot's keyed-mutex hold, with the
    /// `Map`'s failure silently ignored (leaving the pass reading a stale/garbage texel size).
    cbuf: ID3D11Buffer,
}

// The three converters' methods below are SAFE fns. They were `unsafe fn` because their bodies are
// D3D11 FFI, which is not the same thing as having a caller contract: every parameter is a borrowed
// windows-rs COM wrapper (`&ID3D11Device`, `&ID3D11DeviceContext`, `&ID3D11Texture2D`, the views) or
// a plain `u32`/`bool`, each body builds its own descriptors from those, and every created interface
// owns its reference. There is nothing a caller can pass that makes them unsound, and the proofs the
// markers carried said exactly that — "`?`-checked D3D11 methods on the live `device` borrow" — which
// is a description of the body, not an obligation. `unsafe` now marks the FFI inside them, where the
// blocks and their proofs already were. `compile_shader` KEEPS its marker: it takes `PCSTR`, a raw
// pointer the caller must guarantee is a NUL-terminated literal.
impl HdrP010Converter {
    /// `w`/`h` are the SOURCE dimensions this converter's chroma pass will sample, baked into the
    /// immutable constant buffer. Rebuild the converter if they change (the IDD capturer's
    /// `recreate_ring` already drops it).
    pub(crate) fn new(device: &ID3D11Device, w: u32, h: u32) -> Result<Self> {
        // SAFETY: every call is a `?`-checked D3D11 method on the live `device` borrow, over
        // fully-initialized stack descriptors and live `Option` out-params; `compile_shader` receives
        // `s!()` literals (its contract). Each created COM interface owns its own reference, and no
        // raw pointer outlives the call that produced it.
        unsafe {
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
            // `inv_src` never changes for a given source size — build the buffer IMMUTABLE with its
            // contents, so the chroma pass needs no per-frame Map (and has no unchecked failure).
            let inv_src: [f32; 4] = [1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32, 0.0, 0.0];
            let cbd = D3D11_BUFFER_DESC {
                ByteWidth: 16, // float2 inv_src + float2 pad
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: inv_src.as_ptr().cast(),
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut cbuf = None;
            device.CreateBuffer(&cbd, Some(&init), Some(&mut cbuf))?;
            Ok(Self {
                vs: vs.context("p010 vs")?,
                ps_y: ps_y.context("p010 y ps")?,
                ps_uv: ps_uv.context("p010 uv ps")?,
                sampler: sampler.context("p010 sampler")?,
                cbuf: cbuf.context("p010 cbuf")?,
            })
        }
    }

    /// Create a per-plane RTV of the P010 texture `dst` with the given single-plane `format`
    /// (`R16_UNORM` for plane 0 luma, `R16G16_UNORM` for plane 1 chroma). The plane is selected by the
    /// view format (planar-RTV semantics); MipSlice 0.
    ///
    /// Called ONCE PER OUT-RING SLOT by the owner of the P010 textures, not per frame — see
    /// [`Self::convert`]. Fails when the driver rejects a planar RTV, which is the one hard
    /// requirement of this whole path (a D3D11.3+ runtime plus driver support).
    pub(crate) fn plane_rtv(
        device: &ID3D11Device,
        dst: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11RenderTargetView> {
        // SAFETY: one `?`-checked `CreateRenderTargetView` on the live `device` borrow, with a
        // fully-initialized `D3D11_RENDER_TARGET_VIEW_DESC` local whose address is taken only for the
        // duration of the synchronous call, plus a live `Option` out-param.
        unsafe {
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
    }

    /// Convert `src_srv` (FP16 scRGB, WxH) into a `DXGI_FORMAT_P010` texture through the two plane
    /// RTVs the CALLER built for it ([`Self::plane_rtv`]): full-res luma → `y_rtv` (plane 0),
    /// half-res chroma → `uv_rtv` (plane 1). `w`/`h` are the full luma dimensions (must be even) and
    /// must match the `w`/`h` this converter was constructed with.
    ///
    /// Takes the views rather than creating them: two `CreateRenderTargetView`s per frame — plus a
    /// Map/Unmap of a never-changing 16-byte constant buffer — was pure per-frame cost inside the
    /// ring slot's keyed-mutex hold, i.e. time the DRIVER spent blocked on that slot. Both are
    /// lifetime-of-mode facts: the views belong to the out-ring slot (built in `ensure_out_ring`),
    /// the buffer to this converter, which is already rebuilt on every mode change.
    pub(crate) fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        y_rtv: &ID3D11RenderTargetView,
        uv_rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        // SAFETY: all D3D11 work runs on the caller's live `ctx` borrow (the owning capture thread's
        // immediate context) over borrowed slices of fully-initialized locals (the viewports) and
        // clones of the caller's live SRV/RTVs. No raw pointers and no mapping on this path.
        unsafe {
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
}

/// PyroWave LUMA pass PS — full-res, writes Y′ to a separate `R8_UNORM` texture. BT.709 limited from
/// the 8-bit sRGB (gamma) BGRA slot, BYTE-IDENTICAL to the Linux `rgb2yuv.comp` `lumaY` (so the
/// wavelet client — whose golden fixtures come from that shader — decodes the same colours). `Load`
/// (texelFetch) reads the exact source texel: RTV pixel (x,y) → source texel (x,y).
const PYRO_Y_PS: &str = r"
Texture2D<float4> tx : register(t0);
float main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 c = tx.Load(int3(int2(pos.xy), 0)).rgb;
    return 16.0/255.0 + 0.1826*c.r + 0.6142*c.g + 0.0620*c.b;
}
";

/// PyroWave CHROMA pass PS — half-res, writes interleaved (Cb,Cr) to a separate `R8G8_UNORM` texture.
/// **2×2 box average** (centre-sited) of the four luma-block RGB texels, then BT.709 limited Cb/Cr —
/// BYTE-IDENTICAL to `rgb2yuv.comp` (which averages `(c00+c10+c01+c11)*0.25` then U/V), so the chroma
/// siting matches the client's decoder. Even dimensions guarantee the 2×2 block is in-bounds.
const PYRO_UV_PS: &str = r"
Texture2D<float4> tx : register(t0);
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    int2 p = int2(pos.xy) * 2;
    float3 c00 = tx.Load(int3(p,             0)).rgb;
    float3 c10 = tx.Load(int3(p + int2(1,0), 0)).rgb;
    float3 c01 = tx.Load(int3(p + int2(0,1), 0)).rgb;
    float3 c11 = tx.Load(int3(p + int2(1,1), 0)).rgb;
    float3 a = (c00 + c10 + c01 + c11) * 0.25;
    float u = 128.0/255.0 - 0.1006*a.r - 0.3386*a.g + 0.4392*a.b;
    float v = 128.0/255.0 + 0.4392*a.r - 0.3989*a.g - 0.0403*a.b;
    return float2(u, v);
}
";

/// PyroWave 4:4:4 CHROMA pass PS — FULL-res, per-pixel (no box filter, no siting), the Windows twin
/// of the Linux `rgb2yuv444.comp` chroma math.
const PYRO_UV444_PS: &str = r"
Texture2D<float4> tx : register(t0);
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 c = tx.Load(int3(int2(pos.xy), 0)).rgb;
    float u = 128.0/255.0 - 0.1006*c.r - 0.3386*c.g + 0.4392*c.b;
    float v = 128.0/255.0 + 0.4392*c.r - 0.3989*c.g - 0.0403*c.b;
    return float2(u, v);
}
";

/// Shared HLSL for the PyroWave **HDR** passes: scRGB FP16 → PQ-encoded BT.2020 → 10-bit studio
/// codes MSB-packed into 16-bit UNORM — the SAME colour math as [`HDR_P010_COMMON`] (verified by
/// `hdr_p010_selftest`), restated over `Load`ed texels so the pyrowave passes stay texel-exact like
/// their SDR twins. The wavelet client decodes these planes with the same CSC rows as the P010 path.
const PYRO_HDR_COMMON: &str = r"
Texture2D<float4> tx : register(t0);
static const float3x3 BT709_TO_BT2020 = {
    0.627403914, 0.329283038, 0.043313048,
    0.069097292, 0.919540405, 0.011362303,
    0.016391439, 0.088013308, 0.895595253
};
float3 pq_oetf(float3 L) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 Lp = pow(saturate(L), m1);
    return pow((c1 + c2 * Lp) / (1.0 + c3 * Lp), m2);
}
float3 scrgb_to_pq2020_rgb(float3 scrgb) {
    float3 nits = max(scrgb, 0.0) * 80.0;
    return pq_oetf(mul(BT709_TO_BT2020, nits) / 10000.0);
}
static const float KR = 0.2627;
static const float KG = 0.6780;
static const float KB = 0.0593;
float y_unorm(float3 pq) {
    float y = KR * pq.r + KG * pq.g + KB * pq.b;
    float code = clamp(64.0 + 876.0 * y, 64.0, 940.0);
    return (code * 64.0) / 65535.0;
}
float2 cbcr_unorm(float3 pq) {
    float y = KR * pq.r + KG * pq.g + KB * pq.b;
    float cbc = clamp(512.0 + 896.0 * (pq.b - y) / 1.8814, 64.0, 960.0);
    float crc = clamp(512.0 + 896.0 * (pq.r - y) / 1.4746, 64.0, 960.0);
    return float2((cbc * 64.0) / 65535.0, (crc * 64.0) / 65535.0);
}
";

/// PyroWave HDR LUMA pass PS — full-res, writes PQ Y′ studio codes to an `R16_UNORM` texture.
const PYRO_HDR_Y_PS: &str = r"
#include_common
float main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 pq = scrgb_to_pq2020_rgb(tx.Load(int3(int2(pos.xy), 0)).rgb);
    return y_unorm(pq);
}
";

/// PyroWave HDR 4:2:0 CHROMA pass PS — half-res, centre-sited 2×2 box in scRGB-LINEAR space (the
/// pyrowave family's siting, matching the SDR pass + `rgb2yuv.comp`, NOT the P010 path's
/// left-cositing), then PQ + studio Cb/Cr into an `R16G16_UNORM` texture.
const PYRO_HDR_UV_PS: &str = r"
#include_common
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    int2 p = int2(pos.xy) * 2;
    float3 a = max(tx.Load(int3(p,             0)).rgb, 0.0);
    float3 b = max(tx.Load(int3(p + int2(1,0), 0)).rgb, 0.0);
    float3 c = max(tx.Load(int3(p + int2(0,1), 0)).rgb, 0.0);
    float3 d = max(tx.Load(int3(p + int2(1,1), 0)).rgb, 0.0);
    float3 pq = scrgb_to_pq2020_rgb((a + b + c + d) * 0.25);
    return cbcr_unorm(pq);
}
";

/// PyroWave HDR 4:4:4 CHROMA pass PS — full-res, per-pixel.
const PYRO_HDR_UV444_PS: &str = r"
#include_common
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 pq = scrgb_to_pq2020_rgb(tx.Load(int3(int2(pos.xy), 0)).rgb);
    return cbcr_unorm(pq);
}
";

/// scRGB/BGRA → **separate** YUV planes for the PyroWave wavelet encoder: a full-res Y texture + a
/// (half- or full-res) interleaved CbCr texture (design/pyrowave-windows-host-zerocopy.md +
/// design/pyrowave-444-hdr.md). SDR mode reads the BGRA slot and writes BT.709-limited 8-bit planes
/// (`R8_UNORM`/`R8G8_UNORM`), byte-identical to the Linux `rgb2yuv(444).comp`; HDR mode reads the
/// scRGB FP16 slot and writes P010-style 10-bit studio codes MSB-packed into 16-bit planes
/// (`R16_UNORM`/`R16G16_UNORM`), colour math identical to [`HdrP010Converter`]. The wavelet encoder
/// imports the two SEPARATE textures into its own Vulkan device — the NVIDIA D3D11→Vulkan import of
/// a single *planar* NV12 texture is unreliable at arbitrary sizes, whereas simple single/
/// two-component textures import reliably. The caller owns the two textures + their RTVs (shareable,
/// per out-ring slot); this only records the passes.
pub(crate) struct BgraToYuvPlanes {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    /// Full-res chroma pass (4:4:4) — the chroma viewport skips the /2.
    chroma444: bool,
}

impl BgraToYuvPlanes {
    pub(crate) fn new(device: &ID3D11Device, hdr: bool, chroma444: bool) -> Result<Self> {
        // SAFETY: as `HdrP010Converter::new` — `?`-checked D3D11 shader creation on the live
        // `device` borrow, with `s!()` literals into `compile_shader` and live out-params.
        unsafe {
            let (y_src, uv_src) = match (hdr, chroma444) {
                (false, false) => (PYRO_Y_PS.to_string(), PYRO_UV_PS.to_string()),
                (false, true) => (PYRO_Y_PS.to_string(), PYRO_UV444_PS.to_string()),
                (true, false) => (
                    PYRO_HDR_Y_PS.replace("#include_common", PYRO_HDR_COMMON),
                    PYRO_HDR_UV_PS.replace("#include_common", PYRO_HDR_COMMON),
                ),
                (true, true) => (
                    PYRO_HDR_Y_PS.replace("#include_common", PYRO_HDR_COMMON),
                    PYRO_HDR_UV444_PS.replace("#include_common", PYRO_HDR_COMMON),
                ),
            };
            let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
            let yb = compile_shader(&y_src, s!("main"), s!("ps_5_0"))?;
            let uvb = compile_shader(&uv_src, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps_y = None;
            device.CreatePixelShader(&yb, None, Some(&mut ps_y))?;
            let mut ps_uv = None;
            device.CreatePixelShader(&uvb, None, Some(&mut ps_uv))?;
            Ok(Self {
                vs: vs.context("pyro vs")?,
                ps_y: ps_y.context("pyro y ps")?,
                ps_uv: ps_uv.context("pyro uv ps")?,
                chroma444,
            })
        }
    }

    /// Convert `src_srv` (BGRA slot for SDR / scRGB FP16 slot for HDR, WxH) → `y_rtv` (full-res Y
    /// texture) + `cbcr_rtv` (half- or full-res CbCr texture per the constructed mode). Two opaque
    /// passes; `w`/`h` are the full luma dims (even for 4:2:0).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        y_rtv: &ID3D11RenderTargetView,
        cbcr_rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        // SAFETY: D3D11 state-setting plus two `Draw`s on the caller's live immediate-context
        // borrow, over borrowed slices of fully-initialized locals (the viewports) and clones of the
        // caller's live SRV/RTVs. No raw pointers and no mapping on this path.
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

            // LUMA pass: full-res → the R8 Y texture.
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.OMSetRenderTargets(Some(&[Some(y_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(Some(&[None]), None);

            // CHROMA pass: half-res (4:2:0) or full-res (4:4:4) → the CbCr texture.
            let (cw, ch) = if self.chroma444 {
                (w, h)
            } else {
                (w / 2, h / 2)
            };
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: cw as f32,
                Height: ch as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.OMSetRenderTargets(Some(&[Some(cbcr_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            Ok(())
        }
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
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_RATIONAL,
};

/// D3D11 **Video Processor** colour/format converter — runs on the GPU's dedicated VIDEO engine, NOT
/// the 3D engine, so the per-frame RGB→YUV conversion does not contend with a GPU-saturating game (the
/// HDR pixel-shader path and NVENC's internal RGB→YUV both use the 3D/compute engine, which an AAA
/// title pins at ~100%). Output is **always NV12, BT.709 studio-range** — one of NVENC's native YUV
/// inputs, so it encodes with no further conversion.
///
/// It does NOT produce P010/BT.2020 PQ: [`VideoConverter::new`] pins the output colour space to
/// `YCBCR_STUDIO_G22_LEFT_P709` unconditionally, and NVIDIA's video processor cannot do RGB→P010 at
/// all (it renders green) — the HDR path is [`HdrP010Converter`]'s shader instead. The `scrgb_input`
/// arm of `new` is likewise the only part of the HDR story here (an FP16 desktop tone-mapped DOWN to
/// 8-bit BT.709), and it currently has no caller: `idd_push::ensure_converter` builds this converter
/// only on the SDR/BGRA path and always passes `false`.
pub(crate) struct VideoConverter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
}

impl VideoConverter {
    /// A BGRA/FP16-RGB → **NV12 (BT.709 limited SDR)** video-engine converter. `scrgb_input` picks
    /// the input colour space: `false` = 8-bit sRGB `BGRA` (the SDR ring); `true` = FP16 scRGB
    /// linear (the HDR ring, used by a PyroWave session that tone-maps the HDR desktop down to the
    /// 8-bit wavelet stream). The output is always studio-range BT.709 NV12 — the P010/BT.2020 HDR
    /// path is [`HdrP010Converter`]'s job, never this one.
    pub(crate) fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        scrgb_input: bool,
    ) -> Result<Self> {
        // SAFETY: the `cast()`s and the `?`-checked video-device factory calls run on the caller's
        // live `device`/`context` borrows; `&desc` is a fully-initialized stack
        // `D3D11_VIDEO_PROCESSOR_CONTENT_DESC` read only for the duration of the call, and the
        // colour-space/frame-format setters take the just-created processor by borrow plus plain
        // enum values.
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().context("device -> ID3D11VideoDevice")?;
            let vctx: ID3D11VideoContext1 =
                context.cast().context("context -> ID3D11VideoContext1")?;
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

            // Full-range RGB in → studio-range BT.709 NV12 out. Input gamma follows the ring format:
            // scRGB linear (G10) for the FP16 HDR ring, sRGB (G22) for the 8-bit BGRA SDR ring. The
            // output is always BT.709 SDR (the video processor tone-maps the scRGB case).
            let in_cs = if scrgb_input {
                DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            let out_cs = DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709;
            vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, in_cs);
            vctx.VideoProcessorSetOutputColorSpace1(&vp, out_cs);
            // Progressive: one frame in, one frame out — no deinterlace, no frame-rate conversion.
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // …and no vendor "auto processing" either. Its documented DEFAULT is ENABLED, so until
            // now the comment above claimed something only this call delivers: denoise, edge
            // enhancement and whatever else the driver folds in were free to run inside every SDR
            // `VideoProcessorBlt` — on the desktop-capture hot path, altering the pixels we encode
            // and costing video-engine time nobody asked for.
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            Ok(Self {
                vdev,
                vctx,
                enumr,
                vp,
            })
        }
    }

    /// Convert `input` (BGRA, or scRGB FP16 for a converter built with `scrgb_input`) → `output`
    /// (NV12, BT.709 studio-range — see the type doc: never P010) on the video engine. Views are
    /// created per call (cheap relative to the Blt) so the input texture can vary frame to frame.
    pub(crate) fn convert(&self, input: &ID3D11Texture2D, output: &ID3D11Texture2D) -> Result<()> {
        // SAFETY: both view creations are `?`-checked calls on `self.vdev` with fully-initialized
        // stack descriptors and live out-params. `stream.pInputSurface` is a `ManuallyDrop` of the
        // input view just created: `VideoProcessorBlt` only BORROWS it (a COM in-param never transfers
        // ownership), and the explicit `into_inner` drop below releases that reference exactly once on
        // both the success and the failure path. `slice::from_ref(&stream)` borrows the live local.
        unsafe {
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
}
