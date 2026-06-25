//! Backend-neutral DXGI adapter selection.
//!
//! The discrete render-GPU LUID picker used to live in the SudoVDA backend (`vdisplay::sudovda`) — a
//! historical accident, since it is display-utility, not SudoVDA-specific. It lives here so the capturers
//! (IDD-push) and the pf-vdisplay backend depend on it as a *peer* instead of reaching into the SudoVDA
//! module — breaking that circular reach-in so SudoVDA can eventually be dropped without losing this
//! helper (audit §9 / Goal 2). This is the plan's `windows/adapter.rs`.

use windows::Win32::Foundation::LUID;

/// Pick the discrete render GPU LUID: the adapter with the most `DedicatedVideoMemory`, skipping
/// WARP / Basic-Render and the SudoVDA software adapter (≈0 VRAM). `PUNKTFUNK_RENDER_ADAPTER=<substring>`
/// forces a match by Description (Apollo's `adapter_name`). Used by the IDD direct-push capturer (to
/// create its shared textures on the same discrete GPU it pins, where NVENC runs) and SET_RENDER_ADAPTER.
///
/// # Safety
/// Creates + enumerates a DXGI factory; the COM calls run in the caller's apartment (the existing callers
/// already satisfy this).
pub(crate) unsafe fn resolve_render_adapter_luid() -> Option<LUID> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let want = std::env::var("PUNKTFUNK_RENDER_ADAPTER")
        .ok()
        .filter(|s| !s.is_empty());
    let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
    let mut best: Option<(LUID, u64, String)> = None;
    let mut i = 0u32;
    while let Ok(a) = factory.EnumAdapters1(i) {
        i += 1;
        let Ok(d) = a.GetDesc1() else { continue };
        let name = String::from_utf16_lossy(&d.Description);
        let name = name.trim_end_matches('\u{0}').to_string();
        let lname = name.to_ascii_lowercase();
        if lname.contains("basic render") || lname.contains("warp") {
            continue; // never pin to the software rasterizer
        }
        if let Some(w) = &want {
            if lname.contains(&w.to_ascii_lowercase()) {
                tracing::info!(
                    adapter = name,
                    "render adapter chosen by PUNKTFUNK_RENDER_ADAPTER"
                );
                return Some(d.AdapterLuid);
            }
            continue;
        }
        let vram = d.DedicatedVideoMemory as u64; // SudoVDA software adapter ≈ 0 → loses to the dGPU
        if best.as_ref().is_none_or(|(_, v, _)| vram > *v) {
            best = Some((d.AdapterLuid, vram, name));
        }
    }
    match best {
        Some((luid, vram, name)) => {
            tracing::info!(
                adapter = name,
                vram_mb = vram / (1024 * 1024),
                "render adapter chosen (max VRAM)"
            );
            Some(luid)
        }
        None => {
            tracing::warn!("no suitable render adapter found for SET_RENDER_ADAPTER");
            None
        }
    }
}
