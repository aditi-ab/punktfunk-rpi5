//! Per-session capture, topology, and encoder decision, resolved once from
//! [`HostConfig`](crate::config) plus handshake-negotiated depth, HDR, chroma, and codec.
//!
//! Capture and topology callers read this artifact instead of re-deriving from config.
//! [`EncoderBackend`] is recorded for logging; `encode::windows_resolved_backend` still
//! opens the encoder. [`SessionPlan::output_format`] is the one-way edge into capture so
//! the capturer never probes the encode backend again.
//!
//! Platform-neutral so it threads through `virtual_stream` / `build_pipeline`. Linux
//! resolves to portal + single-process; Windows is IDD-push + single-process.
//!
//! See `design/windows-host-rewrite.md`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Linux: xdg ScreenCast portal → PipeWire. The only Linux capture path.
    Portal,
    /// Windows: IDD direct-push from the pf-vdisplay driver's shared ring. The
    /// host runs as SYSTEM in the interactive console session, so it captures
    /// the secure desktop too.
    IddPush,
}

impl CaptureBackend {
    /// Shared by [`SessionPlan::resolve`] and the standalone callers (GameStream / spike).
    #[cfg(target_os = "linux")]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }

    #[cfg(target_os = "windows")]
    pub fn resolve() -> Self {
        CaptureBackend::IddPush
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTopology {
    /// One process captures and encodes: Linux portal, or Windows IDD-push in the
    /// host's SYSTEM process in the interactive console session.
    SingleProcess,
}

/// Recorded for logging. The encoder open still goes through
/// `encode::windows_resolved_backend` (config-backed, GPU-vendor cached).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderBackend {
    /// Linux: NVENC vs VAAPI is auto-detected inside `encode::open_video` (not modeled here).
    PlatformAuto,
    Nvenc,
    Amf,
    Qsv,
    Software,
}

impl EncoderBackend {
    /// `PlatformAuto` (Linux NVENC/VAAPI) is always GPU; only `Software` takes CPU staging.
    pub fn is_gpu(self) -> bool {
        !matches!(self, EncoderBackend::Software)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SessionPlan {
    pub capture: CaptureBackend,
    pub topology: SessionTopology,
    pub encoder: EncoderBackend,
    /// 8, or 10 = HEVC Main10 / 10-bit AV1. 10 does not imply HDR — `hdr` carries that.
    pub bit_depth: u8,
    /// Handshake HDR verdict, handed to the capturer (not derived from depth).
    /// Windows IDD-push enables advanced colour; Linux offers 10-bit PQ/BT.2020.
    /// Set only where `capture::capturer_supports_hdr_for` said yes — Linux means
    /// a gamescope output off our `pipewire-hdr` build; other compositors are 8-bit.
    pub hdr: bool,
    /// 4:2:0, or 4:4:4 when client, host, and GPU all support it. `Yuv420` on every backend that declined.
    pub chroma: crate::encode::ChromaFormat,
    /// HEVC by default; H.264 for a GPU-less software host (`resolve_codec` over advertised ∩ host capability).
    pub codec: crate::encode::Codec,
    /// Datagram-aligned encoder chunking: `Some(shard_payload)` on PyroWave, applied
    /// to every encoder this plan opens so AUs stay shard-aligned across rebuilds.
    /// `None` for H.26x.
    pub wire_chunk: Option<usize>,
    /// Encoder composites cursor bitmaps. Set only via [`cursor_blend_for`]: Linux
    /// when the encoder is the compositing stage; Windows always `false` (IDD
    /// composites the pointer). Encoders whose fast path cannot blend stay off
    /// those shapes — see [`Self::output_format`] and `encode::cursor_blend_capable`.
    pub cursor_blend: bool,
    /// Client draws the pointer locally, so `cursor_blend` is off and (on Windows)
    /// the capturer sets the driver's hardware cursor via [`OutputFormat::hw_cursor`](pf_frame::OutputFormat).
    pub cursor_forward: bool,
    /// Gamescope cursor from XFixes, not `SPA_META_Cursor`. Distinct from
    /// `cursor_forward`: stock gamescope neither embeds nor carries the channel,
    /// so the host composites. `false` when gamescope paints the cursor itself
    /// (`pf_vdisplay::gamescope_composites_cursor`) — otherwise a second pointer.
    pub gamescope_cursor: bool,
    /// Encoder slice-count ceiling from [`VIDEO_CAP_MULTI_SLICE`](punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE):
    /// 32 when the bit is set (backend default; no client-side cap), 1 when not
    /// (single-slice frames for TV-SoC decoders). Applied to every encoder this
    /// plan opens so slicing cannot change shape across a rebuild.
    pub max_slices: u32,
}

impl SessionPlan {
    /// `hdr` is the handshake verdict, not derived from depth: 10-bit SDR exists.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        bit_depth: u8,
        hdr: bool,
        chroma: crate::encode::ChromaFormat,
        codec: crate::encode::Codec,
        cursor_blend: bool,
        cursor_forward: bool,
        multi_slice: bool,
    ) -> Self {
        SessionPlan {
            capture: CaptureBackend::resolve(),
            topology: resolve_topology(),
            encoder: resolve_encoder(),
            bit_depth,
            hdr,
            chroma,
            codec,
            wire_chunk: None,
            cursor_blend,
            cursor_forward,
            // Callers that know the compositor overwrite this; default off for everyone else.
            gamescope_cursor: false,
            max_slices: if multi_slice { 32 } else { 1 },
        }
    }

    /// `gpu` from the already-resolved `encoder` (no second probe); `hdr` from the plan.
    pub fn output_format(&self) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        // Linux NVENC 4:4:4: libavcodec `hevc_nvenc` only emits 4:4:4 from a YUV444
        // *input*; RGB-in is always 4:2:0. Zero-copy produces that input on the GPU
        // (`ImportKind::Tiled444`). Without it the encoder swscales CPU RGB → YUV444P,
        // so force GPU capture off here only. (VAAPI 4:4:4 keeps dmabuf; Windows NVENC takes BGRA.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 = self.chroma.is_444()
                && !crate::encode::linux_zero_copy_is_vaapi()
                && !crate::zerocopy::enabled();
            if gpu && force_cpu_for_nvenc_444 {
                // Name the session codec, not the gate. `linux_zero_copy_is_vaapi()` reads
                // the host-global encoder pref, so a PyroWave session on an NVENC/auto host
                // lands here too — and it never touches NVENC or YUV444P; it loses dmabuf
                // passthrough.
                if self.codec == crate::encode::Codec::PyroWave {
                    tracing::warn!(
                        "4:4:4 PyroWave session with PUNKTFUNK_ZEROCOPY off: zero-copy GPU \
                         capture DISABLED — the wavelet encoder loses its raw-dmabuf passthrough \
                         and every frame becomes a full-resolution CPU readback plus an upload \
                         into its own Vulkan device; expect a materially lower fps ceiling (set \
                         PUNKTFUNK_ZEROCOPY=1 to restore the passthrough)"
                    );
                } else {
                    tracing::warn!(
                        "4:4:4 session on the NVENC path without PUNKTFUNK_ZEROCOPY: zero-copy \
                         GPU capture DISABLED — every frame is CPU RGB + swscale RGB→YUV444P; \
                         expect a lower fps ceiling than 4:2:0 at this mode (set \
                         PUNKTFUNK_ZEROCOPY=1 for the GPU 4:4:4 convert)"
                    );
                }
            }
            gpu && !force_cpu_for_nvenc_444
        };
        // PyroWave keeps `gpu = true`: the facade routes to raw-dmabuf passthrough
        // (`ZeroCopyPolicy::pyrowave_session` advertises importable modifiers). The
        // EGL→CUDA importer is skipped — only NVENC consumes those payloads.
        crate::capture::OutputFormat {
            gpu,
            hdr: self.hdr,
            // 10-bit without HDR = 10-bit SDR: Windows expands BGRA 8→10 (`Rgb10a2Sdr`)
            // and does not touch the display's colour state.
            ten_bit_sdr: self.bit_depth == 10 && !self.hdr,
            hw_cursor: self.cursor_forward,
            // 4:4:4 needs a full-chroma source: Windows stays on RGB (not NV12/P010)
            // so NVENC can CSC to 4:4:4.
            chroma_444: self.chroma.is_444(),
            // Windows: IDD-push makes the NV12 out-ring shareable and signals a shared
            // fence for Vulkan import. Linux: facade flips to raw-dmabuf (see above).
            pyrowave: self.codec == crate::encode::Codec::PyroWave,
            // Native NV12 (gamescope) is Linux Vulkan Video only, resolved from the
            // plan's codec so capture never reaches into encode. That path has no CSC
            // to fold the cursor, so any `cursor_blend` session captures RGB instead
            // (compute-CSC / VkSlotBlend). `cursor_blend` subsumes `gamescope_cursor`.
            #[cfg(target_os = "linux")]
            nv12_native: crate::encode::linux_native_nv12_ok(self.codec) && !self.cursor_blend,
            #[cfg(not(target_os = "linux"))]
            nv12_native: false,
        }
    }
}

pub(crate) fn resolve_topology() -> SessionTopology {
    SessionTopology::SingleProcess
}

/// THE rule for [`SessionPlan::cursor_blend`], shared by every resolve caller
/// so they cannot drift.
///
/// * **Linux**: the encoder is the compositing stage. Blend for a cursor-forward
///   session (capture-mouse flip needs the host composite on demand), for
///   gamescope (no pointer in the capture; XFixes must be drawn), and for a
///   no-channel session when the backend can composite. Mutter virtual streams
///   never re-record on cursor-only motion, so compositor-embeds is not a
///   fallback except on backends that cannot blend (libav VAAPI/NVENC, software).
/// * **Everywhere else**: never. Windows IDD composites the pointer itself
///   (`cursor_blend.rs` / DWM); no Windows encode backend reads `frame.cursor`.
///   Gated on Linux because the VAAPI/CUDA prediction and zero-copy switch
///   exist only there.
pub(crate) fn cursor_blend_for(
    cursor_forward: bool,
    gamescope: bool,
    codec: crate::encode::Codec,
    bit_depth: u8,
) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cursor_forward, gamescope, codec, bit_depth);
        false
    }
    #[cfg(target_os = "linux")]
    {
        if gamescope {
            // gamescope capture has no SPA_META_Cursor; skip the blend-capable term or a
            // gamescope that paints its own pointer loses native-NV12 for a blend that
            // never receives an overlay.
            return gamescope_needs_host_cursor(true);
        }
        if cursor_forward {
            return true;
        }
        // Same CUDA-payload prediction as `handshake::cursor_forward`: NVIDIA plus
        // the zero-copy switch, deciding direct-SDK NVENC (blends) vs libav (doesn't).
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        crate::encode::cursor_blend_capable(codec, cuda_planned, bit_depth == 10)
    }
}

/// Gamescope keeps the cursor on a hardware plane and does not paint it into
/// the PipeWire node, so the host reads XFixes and blends. Patch level 2+
/// (`--pipewire-composite-cursor`) puts it in the node — then the host must
/// not blend, or the pointer is drawn twice. A session that composites also
/// forces compute colour-conversion, because the RGB-direct source has no
/// blend stage; cursor-in-node is the zero-copy end-to-end path.
#[cfg(not(target_os = "windows"))]
fn gamescope_needs_host_cursor(gamescope: bool) -> bool {
    gamescope && !pf_vdisplay::gamescope_composites_cursor()
}

/// Kept beside [`cursor_blend_for`] because the two must agree: reader without
/// blend wastes an X11 connection; blend without reader streams no pointer.
pub(crate) fn gamescope_cursor_for(gamescope: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        let _ = gamescope;
        false
    }
    #[cfg(not(target_os = "windows"))]
    {
        gamescope_needs_host_cursor(gamescope)
    }
}

#[cfg(target_os = "windows")]
fn resolve_encoder() -> EncoderBackend {
    match crate::encode::windows_resolved_backend() {
        crate::encode::WindowsBackend::Nvenc => EncoderBackend::Nvenc,
        crate::encode::WindowsBackend::Amf => EncoderBackend::Amf,
        crate::encode::WindowsBackend::Qsv => EncoderBackend::Qsv,
        crate::encode::WindowsBackend::Software => EncoderBackend::Software,
    }
}

#[cfg(not(target_os = "windows"))]
fn resolve_encoder() -> EncoderBackend {
    // `PUNKTFUNK_ENCODER=software` forces GPU-less openh264, which must take
    // CPU-staged capture (`Software.is_gpu() == false`). Everything else stays
    // `PlatformAuto` (NVENC/VAAPI inside `encode::open_video`).
    match pf_host_config::config().encoder_pref.as_str() {
        "software" | "sw" | "openh264" => EncoderBackend::Software,
        _ => EncoderBackend::PlatformAuto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{ChromaFormat, Codec};

    #[test]
    fn resolve_copies_negotiated_fields() {
        let cases = [
            (8, false, ChromaFormat::Yuv420, Codec::H264, false, false),
            (10, true, ChromaFormat::Yuv444, Codec::H265, true, true),
        ];

        for (bit_depth, hdr, chroma, codec, cursor_blend, cursor_forward) in cases {
            let plan = SessionPlan::resolve(
                bit_depth,
                hdr,
                chroma,
                codec,
                cursor_blend,
                cursor_forward,
                true,
            );

            assert_eq!(plan.bit_depth, bit_depth);
            assert_eq!(plan.hdr, hdr);
            assert_eq!(plan.chroma, chroma);
            assert_eq!(plan.codec, codec);
            assert_eq!(plan.cursor_blend, cursor_blend);
            assert_eq!(plan.cursor_forward, cursor_forward);
            assert_eq!(plan.max_slices, 32);
            assert_eq!(plan.wire_chunk, None);
            assert!(!plan.gamescope_cursor);
        }
    }

    #[test]
    fn resolve_limits_single_slice_clients() {
        let plan = SessionPlan::resolve(
            8,
            false,
            ChromaFormat::Yuv420,
            Codec::H264,
            false,
            false,
            false,
        );

        assert_eq!(plan.max_slices, 1);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_uses_idd_push_on_windows() {
        assert_eq!(CaptureBackend::resolve(), CaptureBackend::IddPush);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn resolve_uses_portal_off_windows() {
        assert_eq!(CaptureBackend::resolve(), CaptureBackend::Portal);
    }

    #[test]
    fn topology_is_always_single_process() {
        assert_eq!(resolve_topology(), SessionTopology::SingleProcess);
    }

    #[test]
    fn only_software_encoder_uses_cpu_staging() {
        let cases = [
            (EncoderBackend::PlatformAuto, true),
            (EncoderBackend::Nvenc, true),
            (EncoderBackend::Amf, true),
            (EncoderBackend::Qsv, true),
            (EncoderBackend::Software, false),
        ];

        for (backend, expected) in cases {
            assert_eq!(backend.is_gpu(), expected);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_forward_forces_blend_on_linux() {
        assert!(cursor_blend_for(true, false, Codec::H264, 8));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gamescope_cursor_reader_matches_blend_rule() {
        let cursor_blend = cursor_blend_for(false, true, Codec::H265, 10);
        let gamescope_cursor = gamescope_cursor_for(true);

        assert_eq!(cursor_blend, gamescope_cursor);
        assert_eq!(cursor_blend, gamescope_needs_host_cursor(true));
        assert!(!gamescope_cursor_for(false));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn cursor_rules_are_disabled_off_linux() {
        for cursor_forward in [false, true] {
            for gamescope in [false, true] {
                assert!(!cursor_blend_for(
                    cursor_forward,
                    gamescope,
                    Codec::H265,
                    10
                ));
                assert!(!gamescope_cursor_for(gamescope));
            }
        }
    }
}
