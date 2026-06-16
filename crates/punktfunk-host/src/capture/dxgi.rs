//! DXGI Desktop Duplication capture (Windows) — the analogue of the PipeWire portal capturer.
//! Creates a D3D11 device on the SudoVDA adapter (by LUID), finds the matching output (by GDI
//! name), duplicates it, and on each `AcquireNextFrame` copies the desktop image into a CPU-readable
//! staging texture → tightly-packed BGRA (the GPU-less path that feeds the software encoder). A
//! future zero-copy path returns `FramePayload::D3d11` for NVENC.
//!
//! Validates only with a real GPU + an *activated* SudoVDA monitor (`DuplicateOutput` needs a live
//! WDDM output). Compiles on the GPU-less VM; the pure helpers are unit-tested there.

use super::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11BlendState, ID3D11Buffer, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_FLAG,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BLEND_DESC,
    D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_ALPHA, D3D11_BLEND_ONE, D3D11_BLEND_OP_ADD,
    D3D11_BLEND_SRC_ALPHA, D3D11_BUFFER_DESC, D3D11_COLOR_WRITE_ENABLE_ALL, D3D11_COMPARISON_NEVER,
    D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
    D3D11_MAP_WRITE_DISCARD, D3D11_RENDER_TARGET_BLEND_DESC, D3D11_SAMPLER_DESC, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_DYNAMIC, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_MODE_CHANGE_IN_PROGRESS,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::SetCursorPos;

/// The Windows capture identity carried out of the SudoVDA backend in
/// [`crate::vdisplay::VirtualOutput`]: which adapter + which GDI output to duplicate.
#[derive(Clone, Debug)]
pub struct WinCaptureTarget {
    /// Packed DXGI adapter LUID (`(HighPart << 32) | (LowPart & 0xffff_ffff)`).
    pub adapter_luid: i64,
    /// The output's GDI device name, e.g. `\\.\DISPLAY3`. Can CHANGE across a secure-desktop switch.
    pub gdi_name: String,
    /// Stable SudoVDA target id — re-resolved to the current GDI name on every recovery.
    pub target_id: u32,
}

/// A GPU-resident captured texture (future NVENC-D3D11 zero-copy path).
pub struct D3d11Frame {
    pub texture: ID3D11Texture2D,
    pub device: ID3D11Device,
}
// COM pointers, used only from the single owning thread.
unsafe impl Send for D3d11Frame {}

pub fn pack_luid(luid: LUID) -> i64 {
    ((luid.HighPart as i64) << 32) | (luid.LowPart as i64 & 0xffff_ffff)
}

/// Does a fixed-size UTF-16 GDI device name (NUL-padded, e.g. `DXGI_OUTPUT_DESC::DeviceName`)
/// equal `target`?
fn gdi_name_matches(name16: &[u16], target: &str) -> bool {
    let s = String::from_utf16_lossy(name16);
    s.trim_end_matches('\u{0}') == target
}

/// Copy a row-padded BGRA surface (`pitch` >= `w*4`) into a tightly-packed `w*4*h` buffer.
fn depad_bgra(src: &[u8], pitch: usize, w: usize, h: usize) -> Vec<u8> {
    let row = w * 4;
    let mut out = vec![0u8; row * h];
    for y in 0..h {
        out[y * row..y * row + row].copy_from_slice(&src[y * pitch..y * pitch + row]);
    }
    out
}

/// Re-find the live `IDXGIOutput1` for a GDI name across all adapters (the SudoVDA monitor is
/// enumerated under the rendering GPU). Used to recover after ACCESS_LOST, where the cached handle
/// may be stale.
pub(crate) unsafe fn find_output(gdi_name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
    let mut i = 0u32;
    while let Ok(a) = factory.EnumAdapters1(i) {
        let mut j = 0u32;
        while let Ok(o) = a.EnumOutputs(j) {
            let od = o.GetDesc()?;
            if gdi_name_matches(&od.DeviceName, gdi_name) {
                // Diagnostic: which ADAPTER does this output sit under, and at what LUID? If this LUID
                // BOUNCES across an ACCESS_LOST storm, the output is being reparented between adapters
                // (the multi-GPU/IDD case Apollo's win32u hook + SET_RENDER_ADAPTER fix). If it's STABLE,
                // the storm is something else (e.g. HDR independent-flip DDA can't capture).
                if let Ok(ad) = a.GetDesc1() {
                    let name = String::from_utf16_lossy(&ad.Description);
                    tracing::info!(
                        output = gdi_name,
                        adapter = name.trim_end_matches('\u{0}'),
                        luid = format!(
                            "{:08x}:{:08x}",
                            ad.AdapterLuid.HighPart, ad.AdapterLuid.LowPart
                        ),
                        "find_output: output resolved under adapter"
                    );
                }
                return Ok((a.clone(), o.cast::<IDXGIOutput1>()?));
            }
            j += 1;
        }
        i += 1;
    }
    bail!("no DXGI output named {gdi_name} (gone after ACCESS_LOST?)")
}

/// Create a fresh D3D11 device + context on a specific adapter (driver_type UNKNOWN with an explicit
/// adapter). Used at open and on every ACCESS_LOST: a device created on one desktop cannot sustain a
/// duplication on a *different* desktop (perpetual ACCESS_LOST), so the secure-desktop switch needs a
/// device made while the thread is attached to that desktop.
pub(crate) unsafe fn make_device(
    adapter: &IDXGIAdapter1,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    D3D11CreateDevice(
        adapter,
        D3D_DRIVER_TYPE_UNKNOWN,
        HMODULE::default(),
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        Some(&[D3D_FEATURE_LEVEL_11_0]),
        D3D11_SDK_VERSION,
        Some(&mut device),
        None,
        Some(&mut context),
    )
    .context("D3D11CreateDevice")?;
    Ok((
        device.context("null D3D11 device")?,
        context.context("null D3D11 context")?,
    ))
}

/// Re-find the output, make a fresh device on its adapter, and duplicate it. Used by the ACCESS_LOST
/// recovery to rebuild the whole capture on the current (possibly secure) input desktop.
unsafe fn reopen_duplication(
    gdi_name: &str,
) -> Result<(
    ID3D11Device,
    ID3D11DeviceContext,
    IDXGIOutput1,
    IDXGIOutputDuplication,
)> {
    let (adapter, out) = find_output(gdi_name)?;
    let (dev, ctx) = make_device(&adapter)?;
    let dupl = duplicate_output(&out, &dev).context("re-DuplicateOutput after ACCESS_LOST")?;
    Ok((dev, ctx, out, dupl))
}

/// Create the output duplication. Prefer `IDXGIOutput5::DuplicateOutput1` with an explicit
/// encoder-format list (FP16 first, then BGRA8) — Apollo's path. It hands us the desktop's real
/// scanout format (HDR FP16 or SDR BGRA8) and is far more robust to overlay/format changes than
/// legacy `DuplicateOutput` (which always tone-maps to 8-bit BGRA — the source of much of the
/// ACCESS_LOST churn). Requires the process be per-monitor-v2 DPI aware (set at startup in
/// [`install_gpu_pref_hook`]). Falls back to legacy `DuplicateOutput` if Output5 is unavailable or
/// `DuplicateOutput1` fails.
unsafe fn duplicate_output(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> Result<IDXGIOutputDuplication> {
    if let Ok(output5) = output.cast::<IDXGIOutput5>() {
        // BGRA8 only for now (SDR). NOTE: DuplicateOutput1 returns the FIRST format it can provide and
        // DXGI will CONVERT to it — so listing FP16 first would hand back FP16 even on an SDR desktop,
        // wrongly tripping the HDR path. Real HDR capture (FP16 first + IDXGIOutput6 colorspace
        // detection to pick the path) is the follow-up once the churn is settled.
        let formats = [DXGI_FORMAT_B8G8R8A8_UNORM];
        // RETRY DuplicateOutput1. The caller releases the OLD duplication (self.dupl = None) immediately
        // before calling us, and the kernel-side teardown of that duplication is ASYNC — the FIRST
        // DuplicateOutput1 right after can race it and return E_ACCESSDENIED ("output still duplicated")
        // even though we dropped our only reference. A few short retries let the teardown finish so the
        // ROBUST DuplicateOutput1 dup succeeds, instead of falling through to legacy DuplicateOutput,
        // which "succeeds" into a fragile dup that churns ACCESS_LOST/MODE_CHANGE every few ms on this
        // cross-GPU IDD. (This is why DuplicateOutput1 failed but the legacy call a beat later
        // succeeded — pure timing. Apollo retries DuplicateOutput1 2x/200ms for the same reason.)
        // Apollo waits 200 ms between DuplicateOutput1 attempts — the kernel-side teardown of the
        // just-released duplication takes that long, so short (ms) waits aren't enough. Env-tunable so
        // we can dial it without a rebuild: PUNKTFUNK_DUP_RETRY_MS (per-wait, default 200) ×
        // PUNKTFUNK_DUP_RETRY_N (attempts, default 6) → ~1 s worst case before the legacy fallback.
        let retry_ms: u64 = std::env::var("PUNKTFUNK_DUP_RETRY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        // Default 1 (no retry → immediate legacy fallback). On the secure desktop DuplicateOutput1
        // ALWAYS refuses (only LOGON_UI may use it), so retrying there just blocks the capture thread;
        // and on the normal desktop the release-before-reduplicate + gentle recovery already keep the
        // legacy dup stable. Raise PUNKTFUNK_DUP_RETRY_N only on a box where DuplicateOutput1 can win
        // the old-dup-teardown race (then PUNKTFUNK_DUP_RETRY_MS sets the per-wait, default 200).
        let attempts: u64 = std::env::var("PUNKTFUNK_DUP_RETRY_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
            .max(1);
        let mut last_err = None;
        for attempt in 0..attempts {
            match output5.DuplicateOutput1(device, 0, &formats) {
                Ok(d) => {
                    if attempt > 0 {
                        tracing::info!(attempt, "DuplicateOutput1 succeeded on retry (rode out old-dup teardown race)");
                    }
                    return Ok(d);
                }
                Err(e) => {
                    last_err = Some(e);
                    if attempt + 1 < attempts {
                        std::thread::sleep(Duration::from_millis(retry_ms));
                    }
                }
            }
        }
        if let Some(e) = last_err {
            // Expected on the secure (Winlogon) desktop (DuplicateOutput1 is LOGON_UI-only) and fires
            // once per gentle recovery there — throttle so a lock dwell doesn't flood the log. The
            // legacy fallback below handles it; gentle recovery keeps it from churning.
            static FALLBACKS: AtomicU64 = AtomicU64::new(0);
            if FALLBACKS.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
                tracing::warn!(
                    error = %format!("{e:?}"),
                    "DuplicateOutput1 unavailable — using legacy DuplicateOutput (expected on the secure desktop)"
                );
            }
        }
    }
    output.DuplicateOutput(device).context("DuplicateOutput")
}

/// Park the cursor on a duplicated output. A blank virtual display emits NO Desktop Duplication
/// frames until something changes; a pointer move IS a DDA "change", so this kicks the very first
/// `AcquireNextFrame` loose — and lands the cursor on the display the client is viewing. Two moves
/// to distinct points guarantee an actual move even if the cursor already sat at the center.
/// Re-sync the calling (capture) thread to the CURRENT input desktop. MUST be called on EVERY recovery
/// — symmetrically for ENTERING and LEAVING the Winlogon (secure: lock/login/UAC) desktop. Gating it on
/// is_secure_desktop() (the old bug) re-attached only on the way IN, so on the way OUT the capture
/// thread stayed stuck on the gone Winlogon desktop and every rebuild failed → no frames → client
/// timeout → "display disconnected". Apollo calls its equivalent (syncThreadDesktop) before every
/// duplicate. Opening the secure desktop requires SYSTEM (the host relaunches itself as SYSTEM).
/// Matches Apollo by closing the handle right after SetThreadDesktop — the thread keeps the desktop via
/// an internal reference, so this does NOT leak even when called on every recovery.
unsafe fn attach_input_desktop() {
    match OpenInputDesktop(
        DESKTOP_CONTROL_FLAGS(0),
        false,
        DESKTOP_ACCESS_FLAGS(0x1000_0000), // GENERIC_ALL
    ) {
        Ok(desk) => {
            if let Err(e) = SetThreadDesktop(desk) {
                tracing::warn!(error = %format!("{e:?}"), "attach_input_desktop: SetThreadDesktop FAILED");
            }
            let _ = CloseDesktop(desk);
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:?}"), "attach_input_desktop: OpenInputDesktop FAILED")
        }
    }
}

pub(crate) unsafe fn nudge_cursor_onto(output: &IDXGIOutput1) {
    if let Ok(od) = output.GetDesc() {
        let r = od.DesktopCoordinates;
        let _ = SetCursorPos(r.left + 8, r.top + 8);
        let _ = SetCursorPos((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    }
}

/// How many times DXGI has actually called our hooked `NtGdiDdDDIGetCachedHybridQueryValue`. If this
/// stays 0 while DDA churns with ACCESS_LOST, the hook is NOT on DXGI's GPU-preference path on this
/// build (so reparenting can't be the cause — look at composition/independent-flip instead). >0 with
/// continuing churn means the hook fires but reparenting isn't the trigger here.
static HYBRID_HOOK_HITS: AtomicU64 = AtomicU64::new(0);

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
    fn SetThreadExecutionState(es_flags: u32) -> u32;
}
const ES_CONTINUOUS: u32 = 0x8000_0000;
const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;

/// Replacement for `win32u.dll!NtGdiDdDDIGetCachedHybridQueryValue`: always report
/// `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED` (3). We fully replace the function (never call the
/// original), so no trampoline is needed. (Ported verbatim from Apollo's MinHook hook.)
unsafe extern "system" fn hybrid_query_hook(gpu_preference: *mut u32) -> i32 {
    HYBRID_HOOK_HITS.fetch_add(1, Ordering::Relaxed);
    if gpu_preference.is_null() {
        return 0xC000_000Du32 as i32; // STATUS_INVALID_PARAMETER
    }
    *gpu_preference = 3; // D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED
    0 // STATUS_SUCCESS
}

/// Apollo's win32u GPU-preference hook, ported. On a HYBRID-GPU box DXGI resolves a GPU preference
/// (registry + power settings + the hybrid-adapter DDI) and REPARENTS outputs onto the chosen render
/// GPU — which constantly invalidates Desktop Duplication (DXGI_ERROR_ACCESS_LOST 0x887A0026, the
/// freeze/churn observed on the RTX 4090 + AMD iGPU box; `SET_RENDER_ADAPTER` is ignored there). Faking
/// a cached preference of UNSPECIFIED makes DXGI skip the resolution, so the output is NOT reparented
/// and DDA stays stable on one adapter (this is what makes Apollo's DDA work on this hardware).
/// Installed once, before the first DXGI factory/enumeration; lasts the process lifetime (like Apollo).
pub(crate) fn install_gpu_pref_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
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
            Err(e) => tracing::warn!(error = %format!("{e:?}"),
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

// DXGI Desktop Duplication deliberately EXCLUDES the hardware cursor from the captured surface (the
// OS composites it separately). We capture the cursor shape/position from the frame info and blend it
// back in — on the GPU for the zero-copy path (a CPU readback would stall the 240 fps pipeline).

const CURSOR_VS: &str = r"
cbuffer Rect : register(b0) { float4 r; };
struct VOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VOut main(uint vid : SV_VertexID) {
    float2 uv = float2((vid == 1 || vid == 3) ? 1.0 : 0.0, (vid >= 2) ? 1.0 : 0.0);
    VOut o;
    o.pos = float4(lerp(r.x, r.z, uv.x), lerp(r.y, r.w, uv.y), 0.0, 1.0);
    o.uv = uv;
    return o;
}
";

const CURSOR_PS: &str = r"
Texture2D tx : register(t0);
SamplerState sm : register(s0);
// b0 is shared with the VS: float4 rect, then the HDR cursor params. For SDR white_mul=1 / decode=0
// so this is a no-op (returns the raw sampled BGRA, blended in the display's native sRGB space). For
// HDR the cursor is composited onto a LINEAR scRGB FP16 surface where 1.0 = 80 nits, so we sRGB→
// linear decode (correct alpha blending + no dark edge fringe) and scale to HDR graphics white
// (~203 nits → white_mul = 203/80) so the cursor isn't ~2.5x too dim vs the HDR desktop.
cbuffer C : register(b0) { float4 rect; float white_mul; float decode; float2 pad; };
float3 srgb_to_linear(float3 c) {
    return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float4 s = tx.Sample(sm, uv);
    float3 rgb = s.rgb;
    if (decode > 0.5) { rgb = srgb_to_linear(rgb); }
    rgb *= white_mul;
    return float4(rgb, s.a);
}
";

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

/// A DXGI cursor shape decomposed into up to two BGRA layers. A single shape can require BOTH a
/// normal alpha-blended layer AND a screen-inverting (XOR) layer at once — e.g. a masked-color text
/// I-beam (opaque pixels + invert pixels) or a monochrome cursor mixing opaque and invert pixels.
/// Each layer is composited with its own blend; a single image + single blend (the old approach)
/// renders such mixed shapes wrong (wrong color, or a black box where the screen should invert).
#[derive(Clone, Default)]
struct CursorShape {
    w: u32,
    h: u32,
    /// Layer composited with src-over alpha (transparent where a==0). `None` if it has no pixels.
    alpha: Option<Vec<u8>>,
    /// Layer composited with the inversion blend (white opaque → invert the screen underneath).
    /// `None` if it has no pixels.
    xor: Option<Vec<u8>>,
}

/// GPU cursor overlay: a tiny shader pipeline that blends the cursor texture(s) onto the captured
/// frame. Tied to one D3D11 device; rebuilt when the capturer recreates its device on a desktop switch.
struct CursorCompositor {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    cbuf: ID3D11Buffer,
    blend: ID3D11BlendState,
    /// Inversion blend for masked-color (XOR) cursors like the text I-beam: result = white*(1-dest),
    /// i.e. it inverts the screen under the cursor so it's visible on any background.
    blend_invert: ID3D11BlendState,
    sampler: ID3D11SamplerState,
    /// Alpha-blended layer (normal cursor pixels). srv + width + height.
    tex_alpha: Option<(ID3D11ShaderResourceView, u32, u32)>,
    /// Inversion-blended layer (screen-inverting pixels: masked-color I-beam bar, monochrome invert).
    tex_xor: Option<(ID3D11ShaderResourceView, u32, u32)>,
}

impl CursorCompositor {
    unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        let vsb = compile_shader(CURSOR_VS, s!("main"), s!("vs_5_0"))?;
        let psb = compile_shader(CURSOR_PS, s!("main"), s!("ps_5_0"))?;
        let mut vs = None;
        device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
        let mut ps = None;
        device.CreatePixelShader(&psb, None, Some(&mut ps))?;

        let cbd = D3D11_BUFFER_DESC {
            ByteWidth: 32, // float4 rect + (white_mul, decode, pad, pad) for the HDR cursor PS
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };
        let mut cbuf = None;
        device.CreateBuffer(&cbd, None, Some(&mut cbuf))?;

        let mut bd = D3D11_BLEND_DESC::default();
        bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend = None;
        device.CreateBlendState(&bd, Some(&mut blend))?;

        // Inversion blend: result.rgb = src*(1-dest) + dest*(1-src.a). A white opaque cursor pixel
        // (src=1,a=1) -> 1-dest (inverted); a transparent pixel (src=0,a=0) -> dest (unchanged).
        let mut bdi = D3D11_BLEND_DESC::default();
        bdi.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: true.into(),
            SrcBlend: D3D11_BLEND_INV_DEST_COLOR,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        let mut blend_invert = None;
        device.CreateBlendState(&bdi, Some(&mut blend_invert))?;

        let sd = D3D11_SAMPLER_DESC {
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

        Ok(Self {
            vs: vs.context("vs")?,
            ps: ps.context("ps")?,
            cbuf: cbuf.context("cbuf")?,
            blend: blend.context("blend")?,
            blend_invert: blend_invert.context("blend_invert")?,
            sampler: sampler.context("sampler")?,
            tex_alpha: None,
            tex_xor: None,
        })
    }

    /// Upload one BGRA layer as an immutable shader-resource texture and return its SRV.
    unsafe fn upload_layer(
        device: &ID3D11Device,
        bgra: &[u8],
        w: u32,
        h: u32,
    ) -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bgra.as_ptr() as *const c_void,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, Some(&init), Some(&mut tex))?;
        let tex = tex.context("cursor tex")?;
        let mut srv = None;
        device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        srv.context("cursor srv")
    }

    /// (Re)upload the decomposed cursor layers; either layer may be absent (→ that pass is skipped).
    unsafe fn set_shapes(&mut self, device: &ID3D11Device, shape: &CursorShape) -> Result<()> {
        self.tex_alpha = match &shape.alpha {
            Some(b) => Some((
                Self::upload_layer(device, b, shape.w, shape.h)?,
                shape.w,
                shape.h,
            )),
            None => None,
        };
        self.tex_xor = match &shape.xor {
            Some(b) => Some((
                Self::upload_layer(device, b, shape.w, shape.h)?,
                shape.w,
                shape.h,
            )),
            None => None,
        };
        Ok(())
    }

    /// Blend ONE cursor layer onto `rtv` (a render-target view of the captured frame) at frame pixel
    /// (cx,cy). `invert` selects the inversion blend (screen-inverting pixels); otherwise normal
    /// src-over alpha. A shape with both an alpha and an XOR layer is drawn by calling this twice.
    #[allow(clippy::too_many_arguments)]
    unsafe fn draw_layer(
        &self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        fw: u32,
        fh: u32,
        cx: i32,
        cy: i32,
        srv: &ID3D11ShaderResourceView,
        cw: u32,
        ch: u32,
        invert: bool,
        // HDR (decode=true): sRGB→linear decode + scale the cursor to `white_mul` × 80 nits, so a
        // white cursor hits HDR graphics white (~203 nits) not 80. SDR passes white_mul=1.0,
        // decode=false → the PS returns the raw sample (blended in the display's native sRGB space).
        // The inversion (masked-color / I-beam) blend operates on the framebuffer reference, so the
        // caller passes white_mul=1.0/decode=false for the XOR layer even in HDR.
        white_mul: f32,
        decode: bool,
    ) {
        let x0 = (cx as f32 / fw as f32) * 2.0 - 1.0;
        let x1 = ((cx + cw as i32) as f32 / fw as f32) * 2.0 - 1.0;
        let y0 = 1.0 - (cy as f32 / fh as f32) * 2.0;
        let y1 = 1.0 - ((cy + ch as i32) as f32 / fh as f32) * 2.0;
        let (mul, dec) = if invert {
            (1.0_f32, 0.0_f32)
        } else {
            (white_mul, if decode { 1.0 } else { 0.0 })
        };
        // cbuf layout: [rect.x, rect.y, rect.z, rect.w, white_mul, decode, pad, pad] (32 bytes).
        let cb = [x0, y0, x1, y1, mul, dec, 0.0, 0.0];
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if ctx
            .Map(&self.cbuf, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
            .is_ok()
        {
            std::ptr::copy_nonoverlapping(cb.as_ptr(), mapped.pData as *mut f32, cb.len());
            ctx.Unmap(&self.cbuf, 0);
        }
        let vp = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: fw as f32,
            Height: fh as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        ctx.RSSetViewports(Some(&[vp]));
        ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        let blend = if invert {
            &self.blend_invert
        } else {
            &self.blend
        };
        ctx.OMSetBlendState(blend, Some(&[0.0; 4]), 0xffff_ffff);
        ctx.VSSetShader(&self.vs, None);
        ctx.PSSetShader(&self.ps, None);
        ctx.VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
        ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())])); // white_mul/decode for the PS
        ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
        ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        ctx.IASetInputLayout(None);
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
        ctx.Draw(4, 0);
        // Unbind the render target so the next frame's CopyResource into this texture is unobstructed.
        ctx.OMSetRenderTargets(Some(&[None]), None);
    }
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

/// HDR conversion pixel shader: scRGB FP16 desktop (linear, Rec.709 primaries, 1.0 = 80 nits) →
/// BT.2020 primaries → SMPTE ST 2084 (PQ) → written to a 10-bit R10G10B10A2 target for NVENC
/// (HEVC Main10 / HDR10). This is the standard Windows-HDR capture conversion (matches OBS/Sunshine).
const HDR_PS: &str = r"
Texture2D<float4> tx : register(t0);
SamplerState sm : register(s0);
// Rec.709 → Rec.2020 primaries (linear). Column-major rows as written, used with mul(M, v).
static const float3x3 BT709_TO_BT2020 = {
    0.627403914, 0.329283038, 0.043313048,
    0.069097292, 0.919540405, 0.011362303,
    0.016391439, 0.088013308, 0.895595253
};
float3 pq_oetf(float3 L) {
    // L normalized so 1.0 = 10000 nits. ST 2084.
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 Lp = pow(saturate(L), m1);
    return pow((c1 + c2 * Lp) / (1.0 + c3 * Lp), m2);
}
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 scrgb = max(tx.Sample(sm, uv).rgb, 0.0); // scRGB can be negative (wide gamut); clamp
    float3 nits = scrgb * 80.0;                      // scRGB 1.0 = 80 nits → absolute luminance
    float3 lin2020 = mul(BT709_TO_BT2020, nits);     // primaries conversion (linear)
    float3 pq = pq_oetf(lin2020 / 10000.0);          // normalize to 10k nits, encode PQ
    return float4(pq, 1.0);
}
";

/// scRGB FP16 → BT.2020 PQ 10-bit conversion pass. One per capture device (rebuilt on device
/// recreate, like [`CursorCompositor`]). A single fullscreen draw samples the FP16 source SRV and
/// writes PQ-encoded BT.2020 to the bound R10G10B10A2 render target.
pub(crate) struct HdrConverter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}

impl HdrConverter {
    pub(crate) unsafe fn new(device: &ID3D11Device) -> Result<Self> {
        let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
        let psb = compile_shader(HDR_PS, s!("main"), s!("ps_5_0"))?;
        let mut vs = None;
        device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
        let mut ps = None;
        device.CreatePixelShader(&psb, None, Some(&mut ps))?;
        let sd = D3D11_SAMPLER_DESC {
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
        Ok(Self {
            vs: vs.context("hdr vs")?,
            ps: ps.context("hdr ps")?,
            sampler: sampler.context("hdr sampler")?,
        })
    }

    /// Convert `src_srv` (FP16 scRGB) into `dst_rtv` (R10G10B10A2 PQ BT.2020). Opaque pass, no blend.
    pub(crate) unsafe fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        dst_rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) {
        let vp = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        ctx.RSSetViewports(Some(&[vp]));
        ctx.OMSetRenderTargets(Some(&[Some(dst_rtv.clone())]), None);
        ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
        ctx.VSSetShader(&self.vs, None);
        ctx.PSSetShader(&self.ps, None);
        ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
        ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        ctx.IASetInputLayout(None);
        ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        ctx.Draw(3, 0);
        // Unbind so the next frame can CopyResource into the source and re-RTV the destination.
        ctx.OMSetRenderTargets(Some(&[None]), None);
        ctx.PSSetShaderResources(0, Some(&[None]));
    }
}

/// Convert a DXGI pointer shape (color / masked-color / monochrome) into top-down BGRA.
fn convert_pointer_shape(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorShape> {
    let w = si.Width as usize;
    let pitch = si.Pitch as usize;
    if w == 0 || pitch == 0 {
        return None;
    }
    // Type is a u32 (newtype constants compared via .0).
    if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 as u32 {
        // Straight 32bpp BGRA with a real alpha channel → one alpha-blended layer, no XOR layer.
        let h = si.Height as usize;
        if buf.len() < pitch * h {
            return None;
        }
        let mut alpha = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let s = y * pitch + x * 4;
                let d = (y * w + x) * 4;
                alpha[d] = buf[s];
                alpha[d + 1] = buf[s + 1];
                alpha[d + 2] = buf[s + 2];
                alpha[d + 3] = buf[s + 3];
            }
        }
        Some(CursorShape {
            w: w as u32,
            h: h as u32,
            alpha: Some(alpha),
            xor: None,
        })
    } else if si.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 {
        // 32bpp where the alpha byte is a MASK selector (0x00 or 0xFF), not an alpha. A single shape
        // can mix opaque and screen-inverting pixels (the text I-beam: opaque hot-spot dot + an
        // inverting bar), so we split it into BOTH layers:
        //   mask 0x00            -> opaque RGB                 → ALPHA layer
        //   mask 0xFF, RGB != 0  -> invert the screen (white)  → XOR layer
        //   mask 0xFF, RGB == 0  -> XOR with black = no-op      → transparent in both
        let h = si.Height as usize;
        if buf.len() < pitch * h {
            return None;
        }
        let mut alpha = vec![0u8; w * h * 4];
        let mut xor = vec![0u8; w * h * 4];
        let (mut any_alpha, mut any_xor) = (false, false);
        for y in 0..h {
            for x in 0..w {
                let s = y * pitch + x * 4;
                let d = (y * w + x) * 4;
                let (b, g, r, mask) = (buf[s], buf[s + 1], buf[s + 2], buf[s + 3]);
                if mask == 0 {
                    alpha[d] = b;
                    alpha[d + 1] = g;
                    alpha[d + 2] = r;
                    alpha[d + 3] = 255;
                    any_alpha = true;
                } else if b != 0 || g != 0 || r != 0 {
                    // inverting pixel → white opaque; the inversion blend turns this into 1-dest
                    xor[d] = 255;
                    xor[d + 1] = 255;
                    xor[d + 2] = 255;
                    xor[d + 3] = 255;
                    any_xor = true;
                }
            }
        }
        Some(CursorShape {
            w: w as u32,
            h: h as u32,
            alpha: any_alpha.then_some(alpha),
            xor: any_xor.then_some(xor),
        })
    } else {
        // Monochrome: top half = AND mask, bottom half = XOR mask, 1 bpp. Per-pixel (AND,XOR):
        //   (0,0) opaque black      → ALPHA layer
        //   (0,1) opaque white      → ALPHA layer
        //   (1,0) transparent       → neither layer
        //   (1,1) invert the screen → XOR layer (white opaque) — was previously approximated as
        //                             solid black, which is the bug this split fixes.
        let h = (si.Height / 2) as usize;
        if buf.len() < pitch * h * 2 {
            return None;
        }
        let bit = |row: usize, x: usize| (buf[row * pitch + x / 8] >> (7 - (x % 8))) & 1;
        let mut alpha = vec![0u8; w * h * 4];
        let mut xor = vec![0u8; w * h * 4];
        let (mut any_alpha, mut any_xor) = (false, false);
        for y in 0..h {
            for x in 0..w {
                let and_bit = bit(y, x);
                let xor_bit = bit(y + h, x);
                let d = (y * w + x) * 4;
                match (and_bit, xor_bit) {
                    (0, 0) => {
                        // opaque black: BGR already 0, just mark opaque
                        alpha[d + 3] = 255;
                        any_alpha = true;
                    }
                    (0, 1) => {
                        alpha[d] = 255;
                        alpha[d + 1] = 255;
                        alpha[d + 2] = 255;
                        alpha[d + 3] = 255;
                        any_alpha = true;
                    }
                    (1, 0) => {} // transparent
                    _ => {
                        // (1,1) invert screen → white opaque into the XOR layer
                        xor[d] = 255;
                        xor[d + 1] = 255;
                        xor[d + 2] = 255;
                        xor[d + 3] = 255;
                        any_xor = true;
                    }
                }
            }
        }
        Some(CursorShape {
            w: w as u32,
            h: h as u32,
            alpha: any_alpha.then_some(alpha),
            xor: any_xor.then_some(xor),
        })
    }
}

/// CPU src-over alpha blend of a BGRA cursor into a BGRA frame buffer (software-encode path). When
/// `invert` is set (masked-color / XOR cursor), a covered pixel inverts the frame instead (true XOR).
#[allow(clippy::too_many_arguments)]
fn blend_cursor_cpu(
    frame: &mut [u8],
    fw: u32,
    fh: u32,
    cur: &[u8],
    cw: u32,
    ch: u32,
    cx: i32,
    cy: i32,
    invert: bool,
) {
    let (fw, fh, cw, ch) = (fw as i32, fh as i32, cw as i32, ch as i32);
    for y in 0..ch {
        let fy = cy + y;
        if fy < 0 || fy >= fh {
            continue;
        }
        for x in 0..cw {
            let fx = cx + x;
            if fx < 0 || fx >= fw {
                continue;
            }
            let s = ((y * cw + x) * 4) as usize;
            let a = cur[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let d = ((fy * fw + fx) * 4) as usize;
            if invert {
                for k in 0..3 {
                    frame[d + k] = 255 - frame[d + k];
                }
            } else {
                for k in 0..3 {
                    frame[d + k] =
                        ((cur[s + k] as u32 * a + frame[d + k] as u32 * (255 - a)) / 255) as u8;
                }
            }
        }
    }
}

pub struct DuplCapturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// The output duplication. `Option` so recovery can RELEASE it (set `None`) BEFORE re-duplicating:
    /// DXGI permits only ONE `IDXGIOutputDuplication` per output, and a stale one (incl. an ACCESS_LOST
    /// one) keeps holding the output, so a re-`DuplicateOutput1` returns E_ACCESSDENIED and legacy
    /// `DuplicateOutput` returns a BORN-LOST dup — the storm. Apollo releases before re-duplicating; so
    /// do we now. `None` only transiently during recovery (acquire routes None → recovery).
    dupl: Option<IDXGIOutputDuplication>,
    /// The output's GDI name — re-resolved on ACCESS_LOST (a mode change can stale the cached handle).
    gdi_name: String,
    /// Stable SudoVDA target id, used to re-resolve `gdi_name` during recovery.
    target_id: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    staging: Option<ID3D11Texture2D>,
    holding_frame: bool,
    active: AtomicBool,
    timeout_ms: u32,
    /// The first AcquireNextFrame after a (re)DuplicateOutput gets a generous timeout — the initial
    /// desktop snapshot of a large surface can take longer than the per-frame budget.
    first_frame: bool,
    dbg_timeouts: u32,
    dbg_lost: u32,
    dbg_black_seeds: u32,
    last: Option<Vec<u8>>,
    /// GPU-output mode (zero-copy → NVENC): produce `FramePayload::D3d11` instead of CPU BGRA.
    /// Selected by `PUNKTFUNK_ENCODER=nvenc` so the capturer's output matches the encoder's input.
    gpu_mode: bool,
    /// Reused owned texture the duplication frame is copied into for the D3D11 path (the duplication
    /// surface is transient and released each frame).
    gpu_copy: Option<ID3D11Texture2D>,
    /// The most recently produced presentable GPU texture + its pixel format, repeated by
    /// `next_frame` when AcquireNextFrame reports no change (static desktop) or during a rebuild.
    /// Format-tagged because the SDR path presents BGRA `gpu_copy` while the HDR path presents the
    /// 10-bit `hdr10_out` — the encoder needs the right format on every frame.
    last_present: Option<(ID3D11Texture2D, PixelFormat)>,
    /// HDR (scRGB FP16) capture state. Set when the duplication surface is `R16G16B16A16_FLOAT`
    /// (the desktop has HDR on). The frame can't be `CopyResource`d into a BGRA target, so the HDR
    /// path copies it into an FP16 SRV texture, composites the cursor, then runs [`HdrConverter`] to
    /// produce a BT.2020 PQ 10-bit (`R10G10B10A2`) frame for NVENC. Toggling HDR fires ACCESS_LOST →
    /// `recreate_dupl` re-detects the format, so this tracks the *current* duplication.
    hdr_fp16: bool,
    /// FP16 copy of the duplication surface (RT|SRV): the cursor composites onto it and the converter
    /// samples it. Reallocated on device/size change.
    fp16_src: Option<ID3D11Texture2D>,
    fp16_srv: Option<ID3D11ShaderResourceView>,
    /// 10-bit `R10G10B10A2` PQ output of the HDR conversion — the texture handed to NVENC.
    hdr10_out: Option<ID3D11Texture2D>,
    /// scRGB→PQ conversion pass; rebuilt on device recreate.
    hdr_conv: Option<HdrConverter>,
    /// Last time a duplication rebuild was attempted, to throttle retries during an outage (e.g. a
    /// secure-desktop dwell where the output is gone) so we don't block the encode loop or hammer
    /// DuplicateOutput — between attempts the last good frame is repeated. `None` = never attempted.
    last_rebuild: Option<Instant>,
    /// Throttle for ALL ACCESS_LOST recovery attempts (cheap re-duplicate + full rebuild). A
    /// constantly-invalidated duplication (HDR overlay/MPO churn) would otherwise spin recovery and
    /// starve the encode thread; cap attempts to ~one per 5 ms and repeat the last frame between them.
    last_recover: Option<Instant>,
    /// True once at least one real frame has been produced. After that, a frame drought (e.g. a long
    /// secure-desktop dwell with nothing rendering to the virtual output) must never fatally end the
    /// session — `next_frame` keeps repeating the last/seeded frame instead of erroring on its
    /// deadline. The deadline stays fatal only *before* the first frame (a genuine startup misconfig).
    ever_got_frame: bool,
    /// Consecutive rebuilds that produced a BORN-LOST duplication (created OK, but its first
    /// AcquireNextFrame instantly returned ACCESS_LOST). On the NORMAL desktop this is the hybrid
    /// reparent/flip storm — once it persists, `acquire` returns Err so the m3 loop cold-rebuilds the
    /// whole pipeline (new device/output) instead of spinning on a dead dup forever (the bug where the
    /// stream froze on the last frame). Reset to 0 by any real frame. NOT armed on the secure
    /// (Winlogon) desktop, where a long static dwell is legitimate and must never end the session.
    consecutive_born_lost: u32,
    /// GPU cursor overlay (rebuilt on device recreate). `None` until the first composite.
    cursor: Option<CursorCompositor>,
    /// Last cursor shape, decomposed into alpha + XOR layers (kept device-independent so it survives
    /// a device recreate).
    cursor_shape: Option<CursorShape>,
    cursor_pos: (i32, i32),
    cursor_visible: bool,
    /// Cursor shape changed → re-upload to the GPU texture(s) before the next composite.
    cursor_dirty: bool,
    dbg_cursor: u64,
    _keepalive: Box<dyn Send>,
}
// COM objects used only from the one thread that owns the capturer (the encode thread).
unsafe impl Send for DuplCapturer {}

impl DuplCapturer {
    pub fn open(
        target: WinCaptureTarget,
        preferred: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
    ) -> Result<Self> {
        unsafe {
            // Stop DXGI hybrid-GPU output reparenting BEFORE we create the factory / enumerate outputs
            // (the cause of the 0x887A0026 ACCESS_LOST churn on this hybrid box: RTX 4090 + AMD iGPU).
            install_gpu_pref_hook();
            // Force PER-MONITOR-AWARE-V2 on THIS (capture) thread. IDXGIOutput5::DuplicateOutput1
            // REQUIRES V2 — without it the call returns E_ACCESSDENIED forever (the 4370x failures
            // measured live), forcing the legacy DuplicateOutput fallback which yields a BORN-LOST
            // duplication on this box → the ACCESS_LOST storm. SetProcessDpiAwarenessContext failed at
            // startup ("already set" — a manifest/runtime locked the process to a LOWER awareness, and
            // GetAwarenessFromDpiAwarenessContext can't tell V1 from V2: it reports 2 for both). The
            // per-THREAD override works regardless of the process default, so DuplicateOutput1 can
            // succeed (the working dup Apollo gets). Must run on the capture thread before any DXGI use.
            {
                use windows::Win32::UI::HiDpi::{
                    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
                    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                };
                let prev = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                let is_v2 = AreDpiAwarenessContextsEqual(
                    GetThreadDpiAwarenessContext(),
                    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                )
                .as_bool();
                tracing::info!(
                    set_ok = !prev.0.is_null(),
                    thread_is_v2 = is_v2,
                    "capture thread DPI awareness -> PER_MONITOR_AWARE_V2 (required for DuplicateOutput1)"
                );
            }
            // Keep the IDD (SudoVDA) virtual display awake for the capture lifetime: an idle indirect
            // display can be power-gated, which invalidates the duplication (a contributor to the
            // "freezes randomly while streaming" loss). Restored to ES_CONTINUOUS on Drop. (Apollo does
            // this too.) Must run on the capture thread (this one owns the capturer).
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            // 1) Find the output (monitor) whose GDI DeviceName matches, across ALL adapters. On a
            // real-GPU box the SudoVDA virtual monitor's DXGI output is enumerated under the GPU that
            // *renders* it (the discrete/integrated GPU), NOT under the SudoVDA "adapter" LUID that
            // SudoVDA reports — so we can't restrict the search to `target.adapter_luid`. The output
            // also appears a beat after the display is created, so settle-retry for up to ~2 s.
            // `target.adapter_luid` is kept only as a tie-break preference (matched adapter first).
            let _ = target.adapter_luid;
            let deadline = Instant::now() + Duration::from_millis(2000);
            let (adapter, output): (IDXGIAdapter1, IDXGIOutput1) = loop {
                let mut hit = None;
                let mut i = 0u32;
                while let Ok(a) = factory.EnumAdapters1(i) {
                    let ad = a.GetDesc1()?;
                    let aname = String::from_utf16_lossy(&ad.Description);
                    let aname = aname.trim_end_matches('\u{0}');
                    let mut j = 0u32;
                    while let Ok(o) = a.EnumOutputs(j) {
                        let od = o.GetDesc()?;
                        let oname = String::from_utf16_lossy(&od.DeviceName);
                        let oname = oname.trim_end_matches('\u{0}').to_string();
                        tracing::debug!(
                            adapter = aname,
                            luid = format!("{:#x}", pack_luid(ad.AdapterLuid)),
                            output = oname,
                            want = target.gdi_name,
                            "DXGI output seen"
                        );
                        if gdi_name_matches(&od.DeviceName, &target.gdi_name) {
                            tracing::info!(
                                adapter = aname,
                                luid = format!("{:#x}", pack_luid(ad.AdapterLuid)),
                                output = oname,
                                "capturing the SudoVDA output on this adapter"
                            );
                            hit = Some((a.clone(), o.cast::<IDXGIOutput1>()?));
                            break;
                        }
                        j += 1;
                    }
                    if hit.is_some() {
                        break;
                    }
                    i += 1;
                }
                if let Some(h) = hit {
                    break h;
                }
                if Instant::now() >= deadline {
                    let mut topo = Vec::new();
                    let mut i = 0u32;
                    while let Ok(a) = factory.EnumAdapters1(i) {
                        let ad = a.GetDesc1()?;
                        let an = String::from_utf16_lossy(&ad.Description);
                        let mut outs = Vec::new();
                        let mut j = 0u32;
                        while let Ok(o) = a.EnumOutputs(j) {
                            let od = o.GetDesc()?;
                            outs.push(
                                String::from_utf16_lossy(&od.DeviceName)
                                    .trim_end_matches('\u{0}')
                                    .to_string(),
                            );
                            j += 1;
                        }
                        topo.push(format!(
                            "{} [{:#x}]: {:?}",
                            an.trim_end_matches('\u{0}'),
                            pack_luid(ad.AdapterLuid),
                            outs
                        ));
                        i += 1;
                    }
                    bail!(
                        "no DXGI adapter exposes output {} (topology: {})",
                        target.gdi_name,
                        topo.join(" | ")
                    );
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            // 2) D3D11 device ON the adapter that exposes the output (driver_type MUST be UNKNOWN with
            // an explicit adapter). NVENC binds to this same device for zero-copy encode.
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let device = device.context("null D3D11 device")?;
            let context = context.context("null D3D11 context")?;
            // 3) duplicate the output. Attach to the current input desktop first (as SYSTEM this can
            // be the Winlogon secure desktop) so a session that starts at the lock/login screen works,
            // and re-assert display isolation at OPEN time (not just in recovery): a lock/UAC switch can
            // re-attach a physical monitor and route the secure desktop THERE, leaving our virtual
            // output perpetually idle/lost — re-isolating forces the secure desktop back onto it. Cheap
            // + idempotent (a no-op when nothing else is attached).
            attach_input_desktop();
            crate::vdisplay::sudovda::reassert_isolation(&target.gdi_name);
            let dupl = duplicate_output(&output, &device)
                .context("DuplicateOutput (already duplicated by another app?)")?;
            // Did DXGI actually call our win32u GPU-pref hook during factory/device/dupl creation? hits==0
            // here means the hook is NOT on DXGI's reparenting path on this build → reparenting can't be
            // the churn cause (look at independent-flip/composition instead).
            tracing::info!(hook_hits = hybrid_hook_hits(), "win32u GPU-pref hook call count after open");
            // Kick the first frame loose: a blank virtual display is otherwise change-less.
            nudge_cursor_onto(&output);
            let dd: DXGI_OUTDUPL_DESC = dupl.GetDesc();
            let (width, height) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let refresh_hz = preferred
                .map(|(_, _, hz)| hz)
                .filter(|&hz| hz > 0)
                .unwrap_or_else(|| {
                    let r = dd.ModeDesc.RefreshRate;
                    r.Numerator
                        .checked_div(r.Denominator)
                        .map_or(60, |hz| hz.max(1))
                });
            let timeout_ms = std::env::var("PUNKTFUNK_CAPTURE_TIMEOUT_MS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or((2000 / refresh_hz.max(1)).max(100));
            let gpu_mode = std::env::var("PUNKTFUNK_ENCODER")
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "nvenc" | "hw" | "nvidia"))
                .unwrap_or(false);
            tracing::info!(
                "DXGI duplication: {}x{}@{} on {} ({}) dxgi_format={} (87=BGRA8 24=R10G10B10A2 10=R16G16B16A16_FLOAT)",
                width,
                height,
                refresh_hz,
                target.gdi_name,
                if gpu_mode {
                    "D3D11 zero-copy"
                } else {
                    "CPU staging"
                },
                dd.ModeDesc.Format.0,
            );
            Ok(Self {
                device,
                context,
                output,
                dupl: Some(dupl),
                target_id: target.target_id,
                gdi_name: target.gdi_name,
                width,
                height,
                refresh_hz,
                staging: None,
                holding_frame: false,
                active: AtomicBool::new(false),
                timeout_ms,
                first_frame: true,
                dbg_timeouts: 0,
                dbg_lost: 0,
                dbg_black_seeds: 0,
                last: None,
                gpu_mode,
                gpu_copy: None,
                last_present: None,
                hdr_fp16: dd.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT,
                fp16_src: None,
                fp16_srv: None,
                hdr10_out: None,
                hdr_conv: None,
                last_rebuild: None,
                last_recover: None,
                ever_got_frame: false,
                consecutive_born_lost: 0,
                cursor: None,
                cursor_shape: None,
                cursor_pos: (0, 0),
                cursor_visible: false,
                cursor_dirty: false,
                dbg_cursor: 0,
                _keepalive: keepalive,
            })
        }
    }

    unsafe fn ensure_staging(&mut self) -> Result<()> {
        if self.staging.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: D3D11_BIND_FLAG(0).0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(staging)")?;
        self.staging = t;
        Ok(())
    }

    unsafe fn ensure_gpu_copy(&mut self) -> Result<()> {
        if self.gpu_copy.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(gpu copy)")?;
        self.gpu_copy = t;
        Ok(())
    }

    /// FP16 (`R16G16B16A16_FLOAT`) copy of the HDR duplication surface (RT for the cursor composite +
    /// SRV for the converter). Reallocated when absent (device/size change drops it).
    unsafe fn ensure_fp16_src(&mut self) -> Result<()> {
        if self.fp16_src.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(fp16 src)")?;
        let t = t.context("fp16 src tex")?;
        let mut srv = None;
        self.device
            .CreateShaderResourceView(&t, None, Some(&mut srv))?;
        self.fp16_srv = Some(srv.context("fp16 srv")?);
        self.fp16_src = Some(t);
        Ok(())
    }

    /// 10-bit `R10G10B10A2_UNORM` PQ output of the HDR conversion — the texture NVENC encodes.
    unsafe fn ensure_hdr10_out(&mut self) -> Result<()> {
        if self.hdr10_out.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.width,
            Height: self.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut t))
            .context("CreateTexture2D(hdr10 out)")?;
        self.hdr10_out = t;
        Ok(())
    }

    /// Allocate a presentable GPU texture on the *current* device, clear it to black, and record it
    /// as `last_present`. Called after a desktop-switch recovery so `next_frame` always has a D3D11
    /// frame to repeat even while the (secure) desktop renders nothing to the virtual output — this
    /// is what keeps the session alive across a lock/login/UAC transition instead of dropping it. In
    /// HDR mode it seeds the 10-bit output (black = PQ 0); otherwise the BGRA copy. One-shot: the next
    /// real frame overwrites the texture in place.
    unsafe fn seed_black_gpu_frame(&mut self) -> Result<()> {
        // Instrumentation: a BLACK seed means we have no real desktop frame to show — if the client
        // streams black, this is why. On the secure (Winlogon) desktop this fires when the duplication
        // came back born-lost / idle. Counted + logged (throttled) so a real-lock repro shows the mode.
        self.dbg_black_seeds += 1;
        if self.dbg_black_seeds % 32 == 1 {
            tracing::warn!(
                black_seeds = self.dbg_black_seeds,
                "DDA: seeding BLACK frame — no real desktop frame available (secure desktop idle/born-lost?)"
            );
        }
        if self.hdr_fp16 {
            self.ensure_hdr10_out()?;
            let out = self.hdr10_out.clone().context("hdr10 out texture")?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&out, None, Some(&mut rtv))?;
            self.context
                .ClearRenderTargetView(&rtv.context("null RTV (hdr seed)")?, &[0.0, 0.0, 0.0, 1.0]);
            self.last_present = Some((out, PixelFormat::Rgb10a2));
        } else {
            self.ensure_gpu_copy()?;
            let gpu = self.gpu_copy.clone().context("gpu copy texture")?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&gpu, None, Some(&mut rtv))?;
            self.context
                .ClearRenderTargetView(&rtv.context("null RTV (sdr seed)")?, &[0.0, 0.0, 0.0, 1.0]);
            self.last_present = Some((gpu, PixelFormat::Bgra));
        }
        Ok(())
    }

    /// Pull cursor position/visibility/shape out of the frame info (the HW cursor is NOT in the frame).
    unsafe fn update_cursor(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.cursor_pos = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );
            self.cursor_visible = info.PointerPosition.Visible.as_bool();
        }
        if info.PointerShapeBufferSize > 0 {
            let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
            let mut required = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            if self
                .dupl
                .as_ref()
                .is_some_and(|d| {
                    d.GetFramePointerShape(
                        info.PointerShapeBufferSize,
                        buf.as_mut_ptr() as *mut c_void,
                        &mut required,
                        &mut si,
                    )
                    .is_ok()
                })
            {
                if let Some(shape) = convert_pointer_shape(&buf, &si) {
                    tracing::info!(
                        shape_type = si.Type,
                        size = format!("{}x{}", shape.w, shape.h),
                        alpha = shape.alpha.is_some(),
                        xor = shape.xor.is_some(),
                        "cursor shape captured"
                    );
                    self.cursor_shape = Some(shape);
                    self.cursor_dirty = true;
                }
            }
        }
    }

    /// Composite the cursor onto the GPU frame texture (zero-copy path). `hdr` = the target is the
    /// linear scRGB FP16 surface (HDR path) — the cursor is then sRGB→linear decoded and scaled to
    /// HDR graphics white (PUNKTFUNK_HDR_CURSOR_NITS, default 203, per BT.2408) so it isn't ~2.5×
    /// too dim; SDR composites the raw cursor in the display's native sRGB space.
    unsafe fn composite_cursor_gpu(&mut self, gpu: &ID3D11Texture2D, hdr: bool) -> Result<()> {
        // Diagnostic kill-switch: skip the GPU cursor composite entirely (PUNKTFUNK_NO_CURSOR=1) to
        // isolate its cost on the 3D engine. The per-frame render-target view + draw to the 5K target
        // is the suspect for the high 3D usage under heavy desktop change.
        if std::env::var_os("PUNKTFUNK_NO_CURSOR").is_some() {
            return Ok(());
        }
        self.dbg_cursor += 1;
        if self.dbg_cursor % 240 == 1 {
            tracing::debug!(
                visible = self.cursor_visible,
                pos = format!("{:?}", self.cursor_pos),
                shape = self
                    .cursor_shape
                    .as_ref()
                    .map(|s| format!("{}x{}", s.w, s.h)),
                "cursor state"
            );
        }
        if !self.cursor_visible || self.cursor_shape.is_none() {
            return Ok(());
        }
        if self.cursor.is_none() {
            self.cursor = Some(CursorCompositor::new(&self.device)?);
            self.cursor_dirty = true; // fresh device → must (re)upload the shape texture
        }
        if self.cursor_dirty {
            if let Some(shape) = &self.cursor_shape {
                self.cursor
                    .as_mut()
                    .unwrap()
                    .set_shapes(&self.device, shape)?;
            }
            self.cursor_dirty = false;
        }
        let mut rtv: Option<ID3D11RenderTargetView> = None;
        self.device
            .CreateRenderTargetView(gpu, None, Some(&mut rtv))?;
        let rtv = rtv.context("cursor rtv")?;
        let (cx, cy) = self.cursor_pos;
        // HDR graphics-white target in nits → scRGB multiplier (scRGB 1.0 = 80 nits). Default 203
        // (BT.2408); PUNKTFUNK_HDR_CURSOR_NITS overrides without a rebuild. SDR → 1.0, no decode.
        let white_mul = if hdr {
            let nits = std::env::var("PUNKTFUNK_HDR_CURSOR_NITS")
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|n| n.is_finite() && *n > 0.0)
                .unwrap_or(203.0);
            nits / 80.0
        } else {
            1.0
        };
        let (w, h) = (self.width, self.height);
        let comp = self.cursor.as_ref().unwrap();
        // Alpha-blended layer (normal cursor pixels); HDR brightness scale applies here.
        if let Some((srv, cw, ch)) = &comp.tex_alpha {
            comp.draw_layer(
                &self.context,
                &rtv,
                w,
                h,
                cx,
                cy,
                srv,
                *cw,
                *ch,
                false,
                white_mul,
                hdr, // decode sRGB→linear only on the HDR (linear FP16) target
            );
        }
        // Inversion layer (masked-color I-beam bar / monochrome invert): operates on the framebuffer
        // reference, so it is never HDR-scaled or sRGB-decoded.
        if let Some((srv, cw, ch)) = &comp.tex_xor {
            comp.draw_layer(
                &self.context,
                &rtv,
                w,
                h,
                cx,
                cy,
                srv,
                *cw,
                *ch,
                true,
                1.0,
                false,
            );
        }
        Ok(())
    }

    /// CHEAP recovery for the ACCESS_LOST *churn*: re-`DuplicateOutput` on the EXISTING device +
    /// output. No new device/factory, so the encoder is NOT re-initialized and no black is seeded —
    /// the existing `gpu_copy`/HDR textures/`last_present` are kept and frames resume immediately. This
    /// is the right recovery for the HDR overlay-flip churn (the duplication is invalidated but the
    /// output is still live). Returns false when the output can't be re-duplicated (desktop switch /
    /// output gone) so the caller falls back to the full [`recreate_dupl`]. Probes the new duplication
    /// (like recreate_dupl) so a born-lost one is rejected rather than adopted.
    unsafe fn try_reduplicate(&mut self) -> bool {
        if self.holding_frame {
            let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            self.holding_frame = false;
        }
        // RELEASE the old duplication FIRST (drop it → frees the output) before re-duplicating. DXGI
        // allows one duplication per output; leaving the stale one alive is exactly why DuplicateOutput1
        // returned E_ACCESSDENIED and the legacy fallback produced a born-lost dup.
        self.dupl = None;
        let dupl = match duplicate_output(&self.output, &self.device) {
            Ok(d) => d,
            Err(_) => return false,
        };
        // Adopt first (SAME device → existing gpu_copy/HDR textures/last_present stay valid), then probe
        // + CAPTURE the frame: a born-lost duplication returns ACCESS_LOST immediately; alive-but-idle
        // waits the full 16ms. On a real frame we present it (so a static desktop keeps a real
        // last_present instead of the discarded one); idle keeps the existing last_present.
        self.dupl = Some(dupl);
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        match self.dupl.as_ref().unwrap().AcquireNextFrame(16, &mut info, &mut res) {
            Ok(()) => {
                self.update_cursor(&info);
                if let Some(r) = res {
                    let _ = self.present_acquired(r);
                }
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
            Err(_) => return false, // born-lost on the same output → need the full rebuild
        }
        true
    }

    /// ONE rebuild attempt — deliberately non-blocking. ACCESS_LOST fires on desktop switches
    /// (normal ↔ Winlogon secure: lock/login/UAC) and on the mode change we issue at create. We
    /// re-attach to the now-current input desktop and recreate the D3D11 device + duplication on it
    /// (a device made on the previous desktop can't sustain a duplication on the new one). CRUCIAL:
    /// no internal multi-second retry loop — during a secure-desktop dwell the SudoVDA output is
    /// *gone* (`no DXGI output named …`), and a blocking retry here would starve the encode/send
    /// loop of frames for seconds, so the client times out and disconnects (the bug this fixes).
    /// Instead a single attempt returns immediately; the caller ([`acquire`]) repeats the last good
    /// frame and retries on a throttle, so the session survives an arbitrarily long secure visit.
    unsafe fn recreate_dupl(&mut self) -> Result<()> {
        if self.holding_frame {
            let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            self.holding_frame = false;
        }
        // The SudoVDA output's GDI name can CHANGE across a secure-desktop topology rebuild —
        // re-resolve from the STABLE target id so we find it under its current name.
        if let Some(n) = crate::vdisplay::sudovda::resolve_gdi_name(self.target_id) {
            self.gdi_name = n;
        }
        // Heavy topology work — re-attach the thread to the input desktop AND re-isolate the virtual
        // output — ONLY on the actual secure (Winlogon) desktop. Entering it can re-attach a physical
        // monitor and move the secure desktop off our virtual output, which re-isolation fixes. But on
        // the NORMAL desktop this is just routine ACCESS_LOST churn (HDR overlay / MPO / periodic IddCx
        // invalidation), and re-isolating there is a DISPLAY-TOPOLOGY CHANGE that itself invalidates the
        // freshly-rebuilt duplication → a self-feeding ACCESS_LOST storm (200 rebuilds/session observed).
        // Apollo isolates once at startup and its recovery just re-duplicates; match that off the secure
        // desktop. (The lock screen / post-login are NOT Winlogon, so they take this light path too.)
        // Re-sync the capture thread to the CURRENT input desktop on EVERY rebuild — symmetric for
        // ENTERING and LEAVING the secure (Winlogon) desktop. This is the fix for "UAC/lock appears
        // fine but breaks the instant you click out of it": leaving secure used to skip this (it was
        // gated on is_secure_desktop()), stranding the thread on the gone Winlogon desktop. Cheap +
        // leak-free now (attach_input_desktop closes its handle). reassert_isolation stays secure-only
        // (it's a CCD topology mutation that would self-feed a storm on the normal desktop).
        attach_input_desktop();
        if crate::capture::desktop_watch::is_secure_desktop() {
            crate::vdisplay::sudovda::reassert_isolation(&self.gdi_name);
        }
        // RELEASE the old duplication FIRST (frees the output). reopen_duplication creates a NEW device
        // and re-DuplicateOutputs the output; if the stale duplication is still alive it holds the output
        // and the new one is born-lost / E_ACCESSDENIED. (On reopen failure self.dupl stays None and
        // acquire's None-guard re-drives recovery.)
        self.dupl = None;
        let (dev, ctx, out, dupl) = reopen_duplication(&self.gdi_name)?; // Err → caller repeats + retries

        // (The born-lost guard is now the capture-acquire at the end: we adopt, then grab the current
        // frame; ACCESS_LOST there means born-lost, and we seed black + let the throttled caller retry.)
        // A desktop switch can come back at a different size (e.g. the user session applies its own
        // resolution on login). Adopt it: update dimensions and drop the staging/gpu copies so they
        // reallocate. NVENC re-inits at the new size when it sees the frame.
        let dd: DXGI_OUTDUPL_DESC = dupl.GetDesc();
        let (nw, nh) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
        tracing::info!(
            dxgi_format = dd.ModeDesc.Format.0,
            "DXGI duplication rebuilt (format: 87=BGRA8 24=R10G10B10A2 10=R16G16B16A16_FLOAT)"
        );
        if nw != self.width || nh != self.height {
            tracing::info!(
                old = format!("{}x{}", self.width, self.height),
                new = format!("{nw}x{nh}"),
                "DXGI duplication size changed across switch"
            );
            self.width = nw;
            self.height = nh;
            self.staging = None;
        }
        self.device = dev;
        self.context = ctx;
        self.output = out;
        self.dupl = Some(dupl);
        self.gpu_copy = None; // stale: belonged to the old device
        self.cursor = None; // shaders/textures belonged to the old device; rebuilt on demand
        self.last_present = None; // belonged to the old device; reseeded below
                                  // Re-detect HDR and drop the HDR textures/converter (old device). Toggling HDR on or
                                  // off is exactly this path: the duplication comes back as FP16 (HDR) or BGRA8.
        self.hdr_fp16 = dd.ModeDesc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        self.fp16_src = None;
        self.fp16_srv = None;
        self.hdr10_out = None;
        self.hdr_conv = None;
        self.first_frame = true;
        // Capture the CURRENT desktop frame as `last_present` (instead of seeding black). The secure
        // (lock/login/UAC) desktop is STATIC, so DDA only emits a frame on change — if we seeded black
        // we'd stream black until the user pressed a key (the reported bug). A freshly-created
        // duplication's first AcquireNextFrame returns the full current desktop; grab it and present it,
        // so the client shows the real (frozen-until-it-changes) secure desktop. Born-lost (ACCESS_LOST
        // here) or no-initial-frame (timeout) → seed black as a fallback and let the throttled caller
        // retry — a brief black flash during the unsettled switch, then real content.
        nudge_cursor_onto(&self.output); // kick a change so a static desktop yields its first frame
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let captured = match self.dupl.as_ref().unwrap().AcquireNextFrame(120, &mut info, &mut res) {
            Ok(()) => {
                self.update_cursor(&info);
                match res {
                    Some(r) => match self.present_acquired(r) {
                        Ok(_) => {
                            self.first_frame = false;
                            tracing::info!("DXGI recovery: captured real secure-desktop frame");
                            true
                        }
                        Err(e) => {
                            tracing::warn!(error = %format!("{e:#}"), "recovery: present_acquired failed");
                            false
                        }
                    },
                    None => false,
                }
            }
            Err(e) => {
                tracing::warn!(
                    code = format!("{:#x}", e.code().0),
                    "DXGI recovery: no initial frame (born-lost/idle) — seeding black, will retry"
                );
                false
            }
        };
        if !captured && self.gpu_mode {
            if let Err(e) = self.seed_black_gpu_frame() {
                tracing::warn!(error = %format!("{e:#}"), "seed black frame after recovery failed");
            }
        }
        // Track the born-lost storm: a rebuild that grabbed a real frame clears it; one that came back
        // born-lost (created OK, first AcquireNextFrame == ACCESS_LOST) advances it. `acquire` uses this
        // to escape to a full pipeline cold-rebuild on the normal desktop instead of spinning forever.
        if captured {
            self.consecutive_born_lost = 0;
        } else {
            self.consecutive_born_lost = self.consecutive_born_lost.saturating_add(1);
        }
        Ok(())
    }

    /// Acquire one frame: `Some` on a fresh image, `None` on timeout (no change → caller reuses last).
    unsafe fn acquire(&mut self) -> Result<Option<CapturedFrame>> {
        if self.holding_frame {
            let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            self.holding_frame = false;
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        let timeout = if self.first_frame {
            2000
        } else {
            self.timeout_ms
        };
        // If a prior recovery released the old duplication but couldn't create a new one yet (output
        // gone during a secure dwell, etc.), self.dupl is None — synthesize ACCESS_LOST so we flow into
        // the recovery path below instead of panicking.
        let acq = match self.dupl.as_ref() {
            Some(d) => d.AcquireNextFrame(timeout, &mut info, &mut res),
            None => Err(windows::core::Error::from_hresult(DXGI_ERROR_ACCESS_LOST)),
        };
        match acq {
            Ok(()) => {
                if self.first_frame {
                    tracing::info!(w = self.width, h = self.height, "DXGI first frame acquired");
                    self.first_frame = false;
                }
                self.consecutive_born_lost = 0; // a real frame breaks the born-lost storm
                self.update_cursor(&info);
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                self.dbg_timeouts += 1;
                if self.dbg_timeouts % 40 == 1 {
                    tracing::warn!(
                        timeouts = self.dbg_timeouts,
                        first_frame = self.first_frame,
                        "DXGI AcquireNextFrame timeout (no desktop change yet)"
                    );
                }
                return Ok(None);
            }
            // MODE_CHANGE_IN_PROGRESS (0x887A0025) is TRANSIENT by design ("the call may succeed at a
            // later attempt") — the display topology is mid-settle (e.g. just after the IDD's mode is
            // applied). Do NOT recover/rebuild: a rebuild re-issues create()→set_active_mode, re-touching
            // the topology and PERPETUATING the change (the storm we measured). Just repeat the last frame
            // and wait it out, like a timeout. Throttled log so a genuinely stuck change stays visible.
            Err(e) if e.code() == DXGI_ERROR_MODE_CHANGE_IN_PROGRESS => {
                self.dbg_timeouts += 1;
                if self.dbg_timeouts % 120 == 1 {
                    tracing::warn!(
                        "DXGI mode change in progress (0x887A0025) — waiting for topology to settle"
                    );
                }
                return Ok(None);
            }
            // Recoverable losses, ALL handled by rebuilding the duplication (device + re-DuplicateOutput):
            //   ACCESS_LOST   — desktop switch (normal <-> Winlogon secure: lock/login/UAC) or mode change
            //   INVALID_CALL  — the secure->user-desktop switch (post-login) leaves the duplication in a
            //                   state where AcquireNextFrame returns 0x887A0001; recreating recovers it.
            //                   Previously fatal -> the stream dropped the instant the user logged in.
            //   DEVICE_REMOVED/RESET — GPU TDR / driver reset.
            Err(e)
                if e.code() == DXGI_ERROR_ACCESS_LOST
                    || e.code() == DXGI_ERROR_INVALID_CALL
                    || e.code() == DXGI_ERROR_DEVICE_REMOVED
                    || e.code() == DXGI_ERROR_DEVICE_RESET =>
            {
                self.dbg_lost += 1;
                // TIERED recovery. The HDR path produces a constant ACCESS_LOST *churn*: the
                // duplication keeps getting invalidated (overlay/MPO flips that HDR makes aggressive)
                // but the OUTPUT stays valid — a probe passes, the dup lives briefly, dies, repeats.
                // For that, the cheap fix is a fresh DuplicateOutput on the SAME device+output: no new
                // device/factory → NO encoder re-init, NO black seed → frames stay near-continuous
                // (this is what makes HDR animations smooth). Only a genuine output loss (secure-desktop
                // switch, where DISPLAY10 is gone) or a dead device needs the full rebuild — and THAT
                // is throttled so a long secure dwell doesn't hammer DuplicateOutput / starve the
                // client (between attempts we repeat the last frame).
                let device_dead =
                    e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET;
                if self.dbg_lost % 64 == 1 {
                    tracing::warn!(
                        lost = self.dbg_lost,
                        code = format!("{:#x}", e.code().0),
                        "DXGI capture lost — recovering (cheap re-duplicate, full rebuild if output gone)"
                    );
                }
                // GENTLE recovery. On the secure (Winlogon) desktop the duplication dies on EVERY
                // independent-flip; a tight re-duplicate loop tears the duplication down + brings it up
                // hundreds of times/sec — that release/recreate cycle is the real kernel stress (and it
                // stalls the send thread long enough that the client times out → "display disconnected").
                // So instead of fighting it: cap recovery HARD and just repeat the last frame in between
                // (no busy-spin, no per-flip teardown). The session stays alive across a secure dwell; the
                // lock/UAC screen is frozen/laggy, then capture resumes cleanly when the desktop returns.
                // Tunable: PUNKTFUNK_RECOVER_MS (cheap re-duplicate cadence, default 250) and
                // PUNKTFUNK_REBUILD_MS (heavy new-device rebuild cadence, default 1500).
                let recover_ms = std::env::var("PUNKTFUNK_RECOVER_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(250u64);
                let now = Instant::now();
                if self
                    .last_recover
                    .is_some_and(|t| now.duration_since(t) < Duration::from_millis(recover_ms))
                {
                    return Ok(None); // repeat the last frame; do NOT tear down/recreate yet
                }
                self.last_recover = Some(now);
                if !device_dead && self.try_reduplicate() {
                    // Cheap recovery succeeded (same device, no teardown of the device/monitor).
                    self.first_frame = true;
                    return Ok(None);
                }
                // Heavy full rebuild (new device) — the costliest teardown/recreate, so throttle it the
                // hardest. Only when the cheap re-duplicate keeps failing (genuine output/device loss).
                let rebuild_ms = std::env::var("PUNKTFUNK_REBUILD_MS")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1500u64);
                let now = Instant::now();
                let due = self
                    .last_rebuild
                    .map_or(true, |t| now.duration_since(t) >= Duration::from_millis(rebuild_ms));
                if due {
                    self.last_rebuild = Some(now);
                    if self.recreate_dupl().is_ok() {
                        self.first_frame = true;
                    }
                }
                // Born-lost rebuilds (created OK, instant ACCESS_LOST) used to escalate to a full pipeline
                // cold-rebuild here — but that re-issued vd.create()→set_active_mode (an audible PnP
                // add/remove chime + a fresh topology mode change), which never converged and amplified
                // the storm. With the topology fix (set_active_mode no longer promotes the IDD to PRIMARY
                // by default) the born-lost storm is gone at its source; if one ever recurs, just keep
                // repeating the last frame in-process — never tear the IDD down mid-session (Apollo never
                // does). Throttled visibility only.
                if self.consecutive_born_lost > 0 && self.consecutive_born_lost % 40 == 1 {
                    tracing::warn!(
                        consecutive = self.consecutive_born_lost,
                        "DDA born-lost rebuilds — repeating last frame in-process (no teardown)"
                    );
                }
                return Ok(None);
            }
            Err(e) => return Err(e).context("AcquireNextFrame"),
        }
        let res = res.context("AcquireNextFrame: null resource")?;
        // Detect a mode/format change on the hot path. The desktop can flip HDR<->SDR (FP16<->BGRA —
        // e.g. the SudoVDA output dropping out of HDR for the secure desktop) or change resolution
        // WITHOUT raising ACCESS_LOST; `hdr_fp16`/`width`/`height` would then be stale and
        // `present_acquired` would CopyResource into a mismatched-format/size target — corruption, or
        // the secure-desktop "works once, then HDR breaks" bug. Re-read the acquired texture's desc
        // every frame (Apollo does this) and rebuild on a real change instead of presenting a
        // mismatched frame. Throttled like the ACCESS_LOST path so a flapping toggle can't hammer
        // DuplicateOutput.
        if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            tex.GetDesc(&mut d);
            // Only a real SIZE change is reliably detectable here. Format/HDR is NOT: legacy
            // DuplicateOutput always hands back an 8-bit BGRA surface regardless of the output's FP16
            // scanout mode, so comparing the acquired-texture format against `hdr_fp16` (derived from
            // the OUTDUPL ModeDesc) self-fires every frame → a rebuild storm. A genuine resolution
            // change is caught here; a real HDR↔SDR toggle arrives as ACCESS_LOST → recreate_dupl
            // re-detects it. (Genuine FP16 capture is a separate change: DuplicateOutput1.)
            if d.Width != self.width || d.Height != self.height {
                tracing::info!(
                    old = format!("{}x{}", self.width, self.height),
                    new = format!("{}x{}", d.Width, d.Height),
                    "DXGI capture size changed mid-stream — rebuilding"
                );
                let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
                let now = Instant::now();
                let due = self
                    .last_rebuild
                    .map_or(true, |t| now.duration_since(t) >= Duration::from_millis(250));
                if due {
                    self.last_rebuild = Some(now);
                    if self.recreate_dupl().is_ok() {
                        self.first_frame = true;
                    }
                }
                return Ok(None);
            }
        }
        Ok(Some(self.present_acquired(res)?))
    }

    /// Turn a freshly-acquired duplication resource into a `CapturedFrame` and record it as
    /// `last_present`. Factored out of [`acquire`] so the recovery path ([`recreate_dupl`]) can grab
    /// the CURRENT desktop frame instead of seeding black: the secure (lock/login/UAC) desktop is
    /// static, so DDA emits no change-frame to replace a black seed — the cause of the black-screen-
    /// until-you-press-a-key bug. The caller has already `AcquireNextFrame`d; this releases it.
    unsafe fn present_acquired(&mut self, res: IDXGIResource) -> Result<CapturedFrame> {
        self.holding_frame = true;
        let tex: ID3D11Texture2D = res.cast().context("resource -> Texture2D")?;
        if self.gpu_mode && self.hdr_fp16 {
            // HDR zero-copy path: the duplication surface is scRGB FP16 (R16G16B16A16_FLOAT) — it can't
            // be CopyResource'd into a BGRA target (that was the freeze + cursor-trail bug). Copy it into
            // an FP16 SRV texture (same format → valid), composite the cursor onto it (the cursor lands
            // at ~SDR-white brightness, then goes through the PQ curve correctly), then convert scRGB →
            // BT.2020 PQ 10-bit into hdr10_out and hand THAT to NVENC (HEVC Main10 / HDR10).
            self.ensure_fp16_src()?;
            let src = self.fp16_src.clone().context("fp16 src texture")?;
            self.context.CopyResource(&src, &tex);
            let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            self.holding_frame = false;
            self.composite_cursor_gpu(&src, true)?; // onto the FP16 surface (HDR: decode + nits scale)
            self.ensure_hdr10_out()?;
            let out = self.hdr10_out.clone().context("hdr10 out texture")?;
            if self.hdr_conv.is_none() {
                self.hdr_conv = Some(HdrConverter::new(&self.device)?);
            }
            let srv = self.fp16_srv.clone().context("fp16 srv")?;
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            self.device
                .CreateRenderTargetView(&out, None, Some(&mut rtv))?;
            let rtv = rtv.context("hdr10 rtv")?;
            self.hdr_conv.as_ref().unwrap().convert(
                &self.context,
                &srv,
                &rtv,
                self.width,
                self.height,
            );
            self.last_present = Some((out.clone(), PixelFormat::Rgb10a2));
            return Ok(CapturedFrame {
                width: self.width,
                height: self.height,
                pts_ns: now_ns(),
                format: PixelFormat::Rgb10a2,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: out,
                    device: self.device.clone(),
                }),
            });
        }
        if self.gpu_mode {
            // Zero-copy path: keep the frame on the GPU for NVENC. Copy the transient duplication
            // surface into a reused owned texture, release the duplication frame, hand off the texture.
            self.ensure_gpu_copy()?;
            let gpu = self.gpu_copy.clone().context("gpu copy texture")?;
            self.context.CopyResource(&gpu, &tex);
            let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            self.holding_frame = false;
            self.composite_cursor_gpu(&gpu, false)?;
            self.last_present = Some((gpu.clone(), PixelFormat::Bgra));
            return Ok(CapturedFrame {
                width: self.width,
                height: self.height,
                pts_ns: now_ns(),
                format: PixelFormat::Bgra,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: gpu,
                    device: self.device.clone(),
                }),
            });
        }
        self.ensure_staging()?;
        let staging = self.staging.clone().context("staging texture")?;
        self.context.CopyResource(&staging, &tex);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
            .context("Map staging")?;
        let (w, h) = (self.width as usize, self.height as usize);
        let pitch = map.RowPitch as usize;
        let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * h);
        let mut tight = depad_bgra(src, pitch, w, h);
        self.context.Unmap(&staging, 0);
        let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
        self.holding_frame = false;
        if self.cursor_visible {
            if let Some(shape) = &self.cursor_shape {
                let (cx, cy) = self.cursor_pos;
                if let Some(bgra) = &shape.alpha {
                    blend_cursor_cpu(
                        &mut tight,
                        self.width,
                        self.height,
                        bgra,
                        shape.w,
                        shape.h,
                        cx,
                        cy,
                        false,
                    );
                }
                if let Some(bgra) = &shape.xor {
                    blend_cursor_cpu(
                        &mut tight,
                        self.width,
                        self.height,
                        bgra,
                        shape.w,
                        shape.h,
                        cx,
                        cy,
                        true,
                    );
                }
            }
        }
        self.last = Some(tight.clone());
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: PixelFormat::Bgra,
            payload: FramePayload::Cpu(tight),
        })
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

impl Capturer for DuplCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        // Generous: a secure-desktop switch can take several seconds to settle (re-resolve + recreate
        // the duplication up to 12 s). Better a few seconds of frozen-last-frame than dropping the stream.
        let mut deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(f) = unsafe { self.acquire() }? {
                self.ever_got_frame = true;
                return Ok(f);
            }
            if self.gpu_mode {
                if let Some((tex, fmt)) = &self.last_present {
                    // Repeat the last presented GPU frame (SDR BGRA or HDR 10-bit), keeping the encoder
                    // on a matching format through a static desktop or a mid-rebuild gap.
                    return Ok(CapturedFrame {
                        width: self.width,
                        height: self.height,
                        pts_ns: now_ns(),
                        format: *fmt,
                        payload: FramePayload::D3d11(D3d11Frame {
                            texture: tex.clone(),
                            device: self.device.clone(),
                        }),
                    });
                }
            }
            if let Some(b) = &self.last {
                return Ok(CapturedFrame {
                    width: self.width,
                    height: self.height,
                    pts_ns: now_ns(),
                    format: PixelFormat::Bgra,
                    payload: FramePayload::Cpu(b.clone()),
                });
            }
            if Instant::now() > deadline {
                // After we've streamed at least once, never fatally drop on a frame drought: a long
                // secure-desktop dwell (or a slow rebuild) just means no NEW frame yet. Reset the
                // deadline and keep repeating the last/seeded frame so the session stays alive. The
                // deadline stays fatal only before the first frame — a genuine "monitor never lit up".
                if self.ever_got_frame {
                    deadline = Instant::now() + Duration::from_secs(20);
                    continue;
                }
                return Err(anyhow!(
                    "no DXGI frame within 20s (SudoVDA monitor not activated by a WDDM GPU?)"
                ));
            }
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        unsafe { self.acquire() }
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

impl Drop for DuplCapturer {
    fn drop(&mut self) {
        if self.holding_frame {
            unsafe {
                let _ = self.dupl.as_ref().map(|d| d.ReleaseFrame());
            }
        }
        // Release the display/system-required execution state we took at open().
        unsafe {
            SetThreadExecutionState(ES_CONTINUOUS);
        }
        // _keepalive drops after, REMOVEing the SudoVDA monitor.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_luid_roundtrip() {
        let l = LUID {
            LowPart: 0x1234_5678,
            HighPart: 0x0000_0009,
        };
        assert_eq!(pack_luid(l), (0x9i64 << 32) | 0x1234_5678);
    }

    #[test]
    fn gdi_name_match() {
        let mut buf = [0u16; 32];
        for (i, c) in r"\\.\DISPLAY3".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert!(gdi_name_matches(&buf, r"\\.\DISPLAY3"));
        assert!(!gdi_name_matches(&buf, r"\\.\DISPLAY1"));
    }

    #[test]
    fn depad_removes_row_padding() {
        // 2x2 BGRA, pitch = 12 (row=8 + 4 pad bytes).
        let pitch = 12;
        let mut src = vec![0u8; pitch * 2];
        for y in 0..2 {
            for x in 0..8 {
                src[y * pitch + x] = (y * 8 + x) as u8;
            }
        }
        let out = depad_bgra(&src, pitch, 2, 2);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(&out[8..16], &[8, 9, 10, 11, 12, 13, 14, 15]);
    }
}
