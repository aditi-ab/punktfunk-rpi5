//! Backend-neutral DXGI adapter selection.
//!
//! The discrete render-GPU LUID picker used to live in the SudoVDA backend (`vdisplay::sudovda`) — a
//! historical accident, since it is display-utility, not SudoVDA-specific. It lives here so the capturers
//! (IDD-push) and the pf-vdisplay backend depend on it as a *peer* instead of reaching into the SudoVDA
//! module — breaking that circular reach-in, which let the SudoVDA backend be dropped without losing this
//! helper (audit §9 / Goal 2 — done). This is the plan's `windows/adapter.rs`.
//!
//! The selection logic itself now lives in [`crate::gpu`] (shared with the mgmt API's GPU
//! endpoints): **operator preference (web console) > `PUNKTFUNK_RENDER_ADAPTER` substring > max
//! `DedicatedVideoMemory`**, WARP/Basic-Render always excluded. This wrapper is the LUID-shaped
//! view of it, plus the per-decision logging (call sites are per-session, never per-frame).

use windows::Win32::Foundation::LUID;

/// Pick the render GPU LUID the pipeline is created on: the IDD-push capturer's shared-texture
/// ring, the IddCx SET_RENDER_ADAPTER pin, and (via the captured frame's device) NVENC/AMF/QSV all
/// follow this one decision — see [`crate::gpu::selected_gpu`] for the precedence. A configured
/// preference that doesn't match a present GPU falls back to auto selection (with a warning)
/// rather than returning `None`, so a stale preference never stops the host from streaming.
pub(crate) fn resolve_render_adapter_luid() -> Option<LUID> {
    match crate::gpu::selected_gpu() {
        Some(sel) => {
            tracing::info!(
                adapter = sel.info.name,
                vram_mb = sel.info.vram_bytes / (1024 * 1024),
                source = sel.source.tag(),
                "render adapter selected"
            );
            if sel.source == crate::gpu::PickSource::PreferenceMissing {
                tracing::warn!(
                    "the preferred GPU is not present — auto-selected the adapter above \
                     (fix or clear the preference in the web console)"
                );
            }
            Some(sel.info.luid())
        }
        None => {
            tracing::warn!("no suitable render adapter found for SET_RENDER_ADAPTER");
            None
        }
    }
}
