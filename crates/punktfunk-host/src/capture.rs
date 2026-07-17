//! Frame capture facade (plan §7 / §W6). The capturers themselves live in the `pf-capture`
//! subsystem crate; this host module is the thin BRIDGE that (a) re-exports the shared frame
//! vocabulary + the capturer types so every `crate::capture::*` path is unchanged, and (b) keeps
//! the orchestration entry points — [`open_portal_monitor`] / [`capture_virtual_output`] — which
//! know about `crate::{vdisplay, session_plan, inject, encode}` and hand pf-capture the pre-resolved
//! facts it needs (the [`pf_capture::ZeroCopyPolicy`] and, on Windows, the
//! [`pf_capture::FrameChannelSender`]) so the capturer never reaches back into the orchestrator.

use anyhow::Result;

// The shared frame vocabulary lives in `pf-frame`; re-export the pieces host modules still name via
// `crate::capture::*` (the capture mechanics that used the rest moved into pf-capture).
pub use pf_frame::{CapturedFrame, OutputFormat, PixelFormat};
// The capturer types + trait + synthetics live in `pf-capture`; re-export them at the old paths.
pub use pf_capture::{capturer_supports_444, Capturer, FastSyntheticCapturer, SyntheticCapturer};
// `crate::capture::dxgi::{install_gpu_pref_hook, hdr_p010_selftest}` (main.rs subcommands) and
// `crate::capture::synthetic_nv12` resolve through pf-capture's Windows modules.
#[cfg(target_os = "windows")]
pub use pf_capture::{dxgi, synthetic_nv12};

/// Resolve the [`pf_capture::ZeroCopyPolicy`] for a Linux capture session from the encode backend —
/// the one reach into `crate::encode` the capturer must NOT make itself (it would recreate the
/// capture→encode cycle). Resolved here (the host facade) and threaded in, so the edge stays one-way
/// (plan §2.4 / §W6).
#[cfg(target_os = "linux")]
fn zero_copy_policy() -> pf_capture::ZeroCopyPolicy {
    let backend_is_vaapi = crate::encode::linux_zero_copy_is_vaapi();
    #[cfg(feature = "pyrowave")]
    let pyrowave_modifiers =
        if backend_is_vaapi && pf_host_config::config().encoder_pref.as_str() == "pyrowave" {
            // BGRx is the capture path's canonical packed-RGB format (the modifier advertisement keys
            // on it). `drm_fourcc(Bgrx)` is always `Some`.
            pf_frame::drm_fourcc(PixelFormat::Bgrx)
                .map(crate::encode::pyrowave_capture_modifiers)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_modifiers = Vec::new();
    pf_capture::ZeroCopyPolicy {
        backend_is_vaapi,
        backend_is_gpu: crate::encode::resolved_backend_is_gpu(),
        pyrowave_modifiers,
    }
}

/// Open a live capturer for a client-sized monitor via the xdg ScreenCast portal.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    // On RemoteDesktop-capable desktops (KWin/GNOME) anchor ScreenCast to a RemoteDesktop
    // session so it inherits that grant headlessly; wlroots/Sway has no RemoteDesktop portal,
    // so use a plain ScreenCast session there.
    let anchored = crate::inject::default_backend() == crate::inject::Backend::Libei;
    pf_capture::open_portal_monitor(anchored, zero_copy_policy())
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor() -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// Build a capturer from an already-created virtual output ([`crate::vdisplay::VirtualOutput`]).
/// Explodes the output into the primitives pf-capture needs (so the capturer never depends on the
/// vdisplay type); the capturer takes the keepalive, so dropping it releases the output.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    // The Linux host stays 8-bit (HDR is blocked upstream) and the portal negotiates its own pixel
    // format, so `want.gpu` gates GPU zero-copy capture (the capture backend is always the portal —
    // the `CaptureBackend` arg is a Windows-only dispatch) and `want.chroma_444` selects the
    // worker's planar-YUV444 GPU convert. `gpu = false` (4:4:4 without zero-copy) forces the CPU
    // mmap path so the encoder gets CPU-resident RGB to swscale into YUV444P.
    pf_capture::open_virtual_output(
        vout.remote_fd,
        vout.node_id,
        vout.preferred_mode,
        vout.keepalive,
        want.gpu,
        want.chroma_444,
        zero_copy_policy(),
    )
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay target not yet an active display path (activation failed — see the \
             virtual-display warnings above)"
        )
    })?;
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // The sealed-channel delivery seam: resolve the pf-vdisplay control device ONCE (it is
    // process-global — a dead one is retired, kept alive — so the raw value is stable for the
    // process) and wrap `send_frame_channel` in a `Send + Sync` closure the IDD-push capturer calls
    // at ring attach. This is the ONE reach into `crate::vdisplay` the capturer would otherwise make;
    // building it here keeps the capture→vdisplay dependency out of pf-capture (plan §W6).
    let control = crate::vdisplay::manager::control_device_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay control device not open (monitor not created via the manager?)"
        )
    })?;
    // `HANDLE` is not `Send`; capture the raw value and rebuild it inside the closure (the control
    // device is never closed for the process lifetime, so the value stays valid).
    let control_raw = control.0 as isize;
    let sender: pf_capture::FrameChannelSender = std::sync::Arc::new(
        move |req: &pf_driver_proto::control::SetFrameChannelRequest| {
            // SAFETY: `control_raw` is the pf-vdisplay control handle resolved above; it is never
            // closed for the process lifetime, so reconstructing the `HANDLE` and issuing the
            // `IOCTL_SET_FRAME_CHANNEL` is sound (`send_frame_channel`'s precondition).
            unsafe {
                crate::vdisplay::pf_vdisplay::send_frame_channel(
                    windows::Win32::Foundation::HANDLE(control_raw as *mut core::ffi::c_void),
                    req,
                )
            }
        },
    );
    // IDD direct-push is the sole Windows capture path: consume frames straight from the pf-vdisplay
    // driver's shared ring (in-process, Session 0 — it captures the secure desktop too; no Desktop
    // Duplication, no WGC helper). A FRESH monitor + ring is created per session. `want.hdr`
    // proactively enables advanced color and selects the per-frame conversion. There is NO fallback:
    // if it can't open or the driver doesn't attach, the session fails cleanly and the client
    // reconnects.
    pf_capture::open_idd_push(target, pref, want.hdr, want.chroma_444, keep, sender)
        .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    _want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}
