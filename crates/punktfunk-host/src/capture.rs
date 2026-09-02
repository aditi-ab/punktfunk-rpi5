//! Frame-capture facade over `pf-capture`.
//!
//! Re-exports the shared frame types and capturer traits at the historical
//! `crate::capture::*` paths. Host-only entry points — [`open_portal_monitor`],
//! [`capture_virtual_output`] — resolve [`pf_capture::ZeroCopyPolicy`] and, on
//! Windows, the [`pf_capture::FrameChannelSender`] so the capturer never
//! reaches back into encode or vdisplay.

use anyhow::Result;

pub use pf_frame::{CapturedFrame, OutputFormat};
// Named only by the GameStream media path and the Linux pyrowave plumbing below.
// Off both, a native-only Windows build would trip -D warnings.
#[cfg(any(target_os = "linux", feature = "gamestream"))]
pub use pf_frame::PixelFormat;
// `capturer_supports_hdr` is not re-exported: on Linux that name is the platform
// floor and would silently miss the gamescope arm. Use [`capturer_supports_hdr_for`].
pub use pf_capture::{capturer_supports_444, Capturer, SyntheticCapturer};
#[cfg(feature = "gamestream")]
pub use pf_capture::FastSyntheticCapturer;
#[cfg(target_os = "windows")]
pub use pf_capture::{dxgi, synthetic_nv12};

/// Encode-backend facts for a Linux capture session. Resolved here so pf-capture
/// never reaches `crate::encode` (that would recreate the capture→encode cycle).
#[cfg(target_os = "linux")]
fn zero_copy_policy(
    pyrowave_session: bool,
    native_nv12_session: bool,
) -> pf_capture::ZeroCopyPolicy {
    let backend_is_vaapi = crate::encode::linux_zero_copy_is_vaapi();
    // Raw-dmabuf passthrough serves PyroWave on any vendor: the wavelet encoder
    // imports the dmabuf on its own Vulkan device. The `PUNKTFUNK_ENCODER=pyrowave`
    // lab lever also flips `backend_is_vaapi`.
    #[cfg(feature = "pyrowave")]
    let pyrowave_session =
        pyrowave_session || pf_host_config::config().encoder_pref.as_str() == "pyrowave";
    #[cfg(not(feature = "pyrowave"))]
    let pyrowave_session = {
        let _ = pyrowave_session;
        false
    };
    #[cfg(feature = "pyrowave")]
    let pyrowave_modifiers = if pyrowave_session {
        // BGRx is the capture path's canonical packed-RGB; `drm_fourcc(Bgrx)` is always `Some`.
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
        pyrowave_session,
        pyrowave_modifiers,
        native_nv12_session,
        // Only the direct-SDK NVENC backend takes a packed 10-bit PQ CUDA payload.
        // Without it HDR capture stays on the CPU path (libav swscales into P010).
        hdr_cuda_ok: pf_encode::linux_hdr_cuda_ok(),
    }
}

/// Live capturer for a client-sized monitor via the xdg ScreenCast portal.
/// Pass `want_hdr` only when the session negotiated HDR and the mirrored monitor
/// is in HDR mode ([`pf_capture::gnome_hdr_monitor_active`]). Pass
/// `want_metadata_cursor` only when the encode backend composites
/// `CapturedFrame::cursor`; otherwise the portal embeds the pointer and no
/// backend × cursor-mode pair streams cursorless.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    want_hdr: bool,
    want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    // RemoteDesktop-capable desktops (KWin/GNOME) inherit that grant headlessly.
    // wlroots/Sway has no RemoteDesktop portal, so use a plain ScreenCast session.
    let anchored = crate::inject::default_backend() == crate::inject::Backend::Libei;
    // Monitor mirrors never carry the native PyroWave plane (GameStream protocol).
    // Native NV12 stays off: this path does not resolve the codec, and GNOME/KWin
    // do not produce NV12 anyway.
    pf_capture::open_portal_monitor(
        anchored,
        want_hdr,
        want_metadata_cursor,
        zero_copy_policy(false, false),
    )
}

#[cfg(not(target_os = "linux"))]
pub fn open_portal_monitor(
    _want_hdr: bool,
    _want_metadata_cursor: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("portal capture requires Linux (xdg-desktop-portal + PipeWire)")
}

/// Capturer from an already-created [`crate::vdisplay::VirtualOutput`].
/// Explodes the output so pf-capture never depends on the vdisplay type; the
/// capturer takes the keepalive, so dropping it releases the output.
#[cfg(target_os = "linux")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    // The output's compositor rewrites `SPA_META_Cursor` on every buffer (KWin),
    // so id-0 is an authoritative hide. Derived from the backend that created
    // `vout` — a pooled display only ever matches its own backend.
    cursor_id0_hides: bool,
) -> Result<Box<dyn Capturer>> {
    // Portal negotiates its own pixel format, so `want.gpu` gates GPU zero-copy
    // (this path is always the portal; `CaptureBackend` is Windows-only dispatch)
    // and `want.chroma_444` selects planar-YUV444 GPU convert. `gpu = false`
    // forces CPU mmap so the encoder gets CPU-resident RGB for YUV444P.

    // `want.hdr` offers 10-bit PQ/BT.2020. Handshake already resolved it through
    // [`capturer_supports_hdr_for`]; only gamescope off `pipewire-hdr` is HDR.

    // Aim wlr absolute mapping at THIS head. EXTEND backends (Hyprland, sway)
    // sit beside the operator's screen; without this, abs samples never enter
    // the stream. `None` (KWin/Mutter/gamescope) CLEARS a stale name — e.g.
    // Game-Mode switching Hyprland → gamescope, after which `PF-…` is gone.
    crate::inject::set_stream_output(vout.output_name.clone());
    pf_capture::open_virtual_output(
        vout.remote_fd,
        vout.node_id,
        vout.preferred_mode,
        vout.keepalive,
        want.gpu,
        want.chroma_444,
        want.hdr,
        zero_copy_policy(want.pyrowave, want.nv12_native),
        vout.expect_exact_dims,
        cursor_id0_hides,
    )
}

/// Can the native-plane source this session will drive deliver 10-bit PQ/BT.2020?
/// Capture half of the punktfunk/1 bit-depth gate (`native::handshake`).
///
/// Must be truthful before spawn: `bit_depth` is decided before the display
/// exists, and PQ frames to an 8-bit encoder are a hard error (`pf-encode`
/// Linux encoder). `pf_capture::capturer_supports_hdr()` cannot answer this on
/// Linux — it depends on the resolved compositor and the installed gamescope.
///
/// Windows: IDD-push enables advanced colour, so the platform answer.
/// Linux + gamescope: host knob, `packaging/gamescope` 10-bit BT.2020/PQ, a
/// spawned (not attached-foreign) sub-mode, and no earlier virtual-output HDR
/// downgrade latched. Anything else on Linux is 8-bit; GNOME 50+ portal HDR is
/// the GameStream plane (`gamestream::host_hdr_capable` + live monitor probe).
pub fn capturer_supports_hdr_for(compositor: Option<crate::vdisplay::Compositor>) -> bool {
    #[cfg(target_os = "linux")]
    {
        if compositor == Some(crate::vdisplay::Compositor::Gamescope) {
            return pf_host_config::config().gamescope_hdr
                && pf_vdisplay::gamescope_hdr_available()
                && !pf_capture::hdr_capture_failed(pf_capture::HdrSource::VirtualOutput);
        }
    }
    let _ = compositor;
    pf_capture::capturer_supports_hdr()
}

#[cfg(target_os = "windows")]
pub fn capture_virtual_output(
    vout: crate::vdisplay::VirtualOutput,
    want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    // Linux-only (`SPA_META_Cursor`). IDD-push has no such meta; hide is CURSOR_SUPPRESSED.
    _cursor_id0_hides: bool,
) -> Result<Box<dyn Capturer>> {
    let target = vout.win_capture.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay target not yet an active display path (activation failed — see the \
             virtual-display warnings above)"
        )
    })?;
    // Aim the injectors' absolute mapping (pen/touch/abs-mouse) at THIS display: the wire
    // normalizes over the streamed frame, and mapping it over the whole virtual desktop is wrong
    // the moment a physical monitor shares the desktop (Extend topology, or an Exclusive isolate
    // degraded to the keep-physicals fallback) — the pen-offset field bug.
    crate::inject::set_stream_target(Some(pf_win_display::win_display::CcdTargetKey::new(
        target.adapter_luid,
        target.target_id,
    )));
    let pref = vout.preferred_mode;
    let keep = vout.keepalive;
    // Resolve the pf-vdisplay control device once and wrap `send_frame_channel`
    // for the IDD-push capturer. This is the one host reach into `crate::vdisplay`
    // the capturer would otherwise make.
    let control = crate::vdisplay::manager::control_device_handle().ok_or_else(|| {
        anyhow::anyhow!(
            "pf-vdisplay control device not open (monitor not created via the manager?)"
        )
    })?;
    // Each closure clones the `Arc<OwnedHandle>`, so the handle stays open for
    // the closure's life and closes when the manager retires it and the last
    // session drops. An open control handle vetoes the wake-from-sleep PnP cycle.
    let control_frame = control.clone();
    let sender: pf_capture::FrameChannelSender = std::sync::Arc::new(
        move |req: &pf_driver_proto::control::SetFrameChannelRequest| {
            // SAFETY: the captured `control_frame` Arc keeps the control handle open across this
            // call — `send_frame_channel`'s precondition.
            unsafe {
                crate::vdisplay::driver::send_frame_channel(
                    windows::Win32::Foundation::HANDLE(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&*control_frame),
                    ),
                    req,
                )
            }
        },
    );
    // IDD direct-push is the only Windows capture path: frames from the driver's
    // shared ring, in-process. A fresh monitor + ring per session; `want.hdr`
    // enables advanced color. No fallback — open/attach failure fails the session.

    // Presence of this closure opts the session into v5 cursor-channel delivery
    // (capturer creates CursorShm; driver declares the IddCx hardware cursor).
    let control_cursor = control.clone();
    let cursor_sender: Option<pf_capture::CursorChannelSender> = want.hw_cursor.then(|| {
        std::sync::Arc::new(
            move |req: &pf_driver_proto::control::SetCursorChannelRequest| {
                // SAFETY: the captured `control_cursor` Arc keeps the control handle open across
                // this call (`send_cursor_channel`'s precondition).
                unsafe {
                    crate::vdisplay::driver::send_cursor_channel(
                        windows::Win32::Foundation::HANDLE(
                            std::os::windows::io::AsRawHandle::as_raw_handle(&*control_cursor),
                        ),
                        req,
                    )
                }
            },
        ) as pf_capture::CursorChannelSender
    });
    // Secure-desktop actuator (`IOCTL_SET_CURSOR_FORWARD`): drop the hardware
    // cursor declare while UAC/Winlogon is up. Stand-down needs a same-mode
    // re-commit under the vdisplay manager lock, which pf-capture cannot take.

    // Built for every session: a channel-less reuse can still have a live cursor
    // worker from an earlier session. Never-declared targets answer NOT_FOUND,
    // which the capturer logs and ignores.
    let target_id = target.target_id;
    let cursor_forward: Option<pf_capture::CursorForwardSender> = Some({
        std::sync::Arc::new(move |enable: bool| {
            let req = pf_driver_proto::control::SetCursorForwardRequest {
                target_id,
                enable: enable as u32,
            };
            // SAFETY: the captured `control` Arc keeps the control handle open across this call
            // (`send_cursor_forward`'s precondition).
            unsafe {
                crate::vdisplay::driver::send_cursor_forward(
                    windows::Win32::Foundation::HANDLE(
                        std::os::windows::io::AsRawHandle::as_raw_handle(&*control),
                    ),
                    &req,
                )?;
            }
            if !enable {
                crate::vdisplay::manager::force_recommit();
            }
            Ok(())
        }) as pf_capture::CursorForwardSender
    });
    pf_capture::open_idd_push(
        target,
        pref,
        want.hdr,
        want.ten_bit_sdr,
        want.chroma_444,
        want.pyrowave,
        keep,
        sender,
        cursor_sender,
        cursor_forward,
    )
    .map_err(|(e, _keep)| e.context("IDD-push capture open (no fallback)"))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capture_virtual_output(
    _vout: crate::vdisplay::VirtualOutput,
    _want: OutputFormat,
    _capture: crate::session_plan::CaptureBackend,
    _cursor_id0_hides: bool,
) -> Result<Box<dyn Capturer>> {
    anyhow::bail!("virtual-output capture requires Linux or Windows")
}
