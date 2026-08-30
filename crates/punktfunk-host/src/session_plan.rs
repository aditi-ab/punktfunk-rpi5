//! `SessionPlan` — the per-session capture / topology / encoder decision, resolved **once** from
//! [`HostConfig`](crate::config) (+ the handshake-negotiated bit depth) into a typed, logged value.
//!
//! **Goal-1 stage 3** (`design/windows-host-rewrite.md` §2.2): before this, the Windows session decision was
//! re-derived at three call sites — the capture backend inside `capture::capture_virtual_output`, the
//! process topology in `native::should_use_helper`, and the encode backend in
//! `encode::windows_resolved_backend` — each reading [`config`](crate::config) independently, with no
//! single owner (the latent "capture and encode disagree on the backend" hazard, plan §2.4). `SessionPlan`
//! resolves them together, once, so the deployed path reads one typed artifact.
//!
//! Stage 3 routes the **capture** and **topology** decisions through the plan (see
//! `capture::capture_virtual_output` taking [`CaptureBackend`] in, and `virtual_stream` reading
//! [`SessionTopology`]). The **encoder** is resolved by `encode::windows_resolved_backend` (config-backed
//! and GPU-vendor cached since stage 2, so already a single source) and *recorded* here as
//! [`EncoderBackend`]. Threading `encoder`/`input_format` into the encoder + capturer opens — which
//! removes the `capture → encode::windows_resolved_backend()` back-reference recomputed in `dxgi.rs` —
//! is **stage 5**.
//!
//! The type is platform-neutral so it threads through the shared `virtual_stream`/`build_pipeline`
//! signatures; on Linux it resolves to the single portal/single-process path (the 3-way dispatch is a
//! Windows-only concern).

/// Where a session's frames come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureBackend {
    /// Linux: the xdg ScreenCast portal → PipeWire (the only Linux capture path).
    Portal,
    /// Windows: IDD direct-push — frames pulled straight from the pf-vdisplay driver's shared ring
    /// (in-process; the host runs as SYSTEM in the interactive console session, so it captures the
    /// secure desktop too). The sole Windows capture path —
    /// DXGI Desktop Duplication (DDA) and the WGC two-process relay were removed.
    IddPush,
}

impl CaptureBackend {
    /// Resolve the capture backend from [`config`](crate::config). This is the single resolver shared by
    /// [`SessionPlan::resolve`] and the standalone callers (GameStream / spike), so they can't drift.
    #[cfg(target_os = "linux")]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }

    /// Windows: IDD direct-push is the sole capture path (DDA + the WGC two-process relay were removed).
    #[cfg(target_os = "windows")]
    pub fn resolve() -> Self {
        CaptureBackend::IddPush
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    pub fn resolve() -> Self {
        CaptureBackend::Portal
    }
}

/// How a session is structured across processes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTopology {
    /// One process captures + encodes. The only topology: Linux (portal) and Windows (in-process
    /// IDD-push, in the host's SYSTEM process in the interactive console session). The SYSTEM-host
    /// + user-session WGC relay was removed with DDA/WGC.
    SingleProcess,
}

/// The resolved encode backend (recorded for logging / stages 4–5; the per-session encoder open still
/// resolves via `encode::windows_resolved_backend`, which is config-backed + GPU-vendor cached).
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
    /// True if this backend encodes on the GPU (so the capturer should produce GPU-resident frames). Only
    /// the software encoder takes CPU staging; `PlatformAuto` (Linux NVENC/VAAPI) is always GPU.
    pub fn is_gpu(self) -> bool {
        !matches!(self, EncoderBackend::Software)
    }
}

/// The per-session decision, resolved once. `Copy` so it threads through the capture/encode chain
/// without ceremony (stage 4 folds it, with the rest of the arg soup, into a `SessionContext`).
#[derive(Clone, Copy, Debug)]
pub struct SessionPlan {
    pub capture: CaptureBackend,
    pub topology: SessionTopology,
    pub encoder: EncoderBackend,
    /// Handshake-negotiated encode bit depth (8, or 10 = HEVC Main10 / 10-bit AV1). Since the
    /// 10-bit SDR path, 10 does NOT imply HDR — `hdr` below carries that separately.
    pub bit_depth: u8,
    /// The want-HDR flag handed to the capturer (the handshake's HDR verdict — the Welcome's
    /// colour label, no longer derived from the depth): on Windows the IDD-push
    /// capturer proactively enables advanced colour on the virtual display; on Linux it runs the
    /// 10-bit PQ/BT.2020 PipeWire offer. It is only ever set where the handshake's source-aware
    /// gate said yes (`capture::capturer_supports_hdr_for`) — on Linux that means a gamescope
    /// output off our `pipewire-hdr` build, since Mutter's/KWin's/wlroots' virtual outputs are
    /// 8-bit upstream (GNOME 50's HDR is monitor-mirror only, which is the GameStream portal
    /// path's business).
    pub hdr: bool,
    /// Handshake-negotiated chroma subsampling (4:2:0, or full-chroma 4:4:4 when the client + host +
    /// GPU all support it). Resolved before the Welcome; `Yuv420` on every backend that declined it.
    pub chroma: crate::encode::ChromaFormat,
    /// Handshake-negotiated video codec the encoder emits — HEVC by default, H.264 for a GPU-less
    /// software host (`resolve_codec` over the client's advertised codecs ∩ the host's capability).
    pub codec: crate::encode::Codec,
    /// Datagram-aligned wire chunking for the encoder (plan §4.4): `Some(shard_payload)` on a
    /// PyroWave session — applied to EVERY encoder this plan opens (initial + all rebuilds) so
    /// AUs stay shard-aligned across mode/bitrate/stall rebuilds. `None` for the H.26x codecs.
    pub wire_chunk: Option<usize>,
    /// The session may hand the encoder cursor bitmaps to composite (cursor-as-metadata
    /// captures). Set via [`cursor_blend_for`] — the single platform rule — so it is `true` only
    /// where the ENCODER is the compositing stage (Linux: cursor-forward sessions, gamescope,
    /// AND no-channel sessions on a blend-capable backend — the compositor-EMBEDS fallback is
    /// broken on Mutter virtual streams, see [`cursor_blend_for`]);
    /// Windows is always `false` (the IDD capturer composites the pointer itself). Encoders
    /// whose fast path cannot blend (the Vulkan EFC RGB-direct source, native NV12) stay off
    /// those shapes when this is set — see [`Self::output_format`] and
    /// `encode::cursor_blend_capable`, the pre-open mirror that gates the cursor channel — so
    /// the pointer never silently vanishes from the stream.
    pub cursor_blend: bool,
    /// The session negotiated the cursor-forward channel (M2/M2c): the client draws the pointer
    /// locally, so `cursor_blend` is off AND (on Windows) the capturer sets the driver's
    /// hardware cursor up via [`OutputFormat::hw_cursor`](pf_frame::OutputFormat).
    pub cursor_forward: bool,
    /// This gamescope session's cursor comes from the XFixes source, NOT the (absent)
    /// `SPA_META_Cursor` (remote-desktop-sweep Phase C). Distinct from `cursor_forward`: a stock
    /// gamescope can neither embed the pointer nor carry the channel for a plain capture-mode
    /// client, so the host composites the XFixes-sourced cursor into the video (`cursor_blend` is
    /// set too). `build_pipeline` reads this to attach the XFixes reader to the capturer.
    ///
    /// **`false` when the spawned gamescope paints the cursor into its node itself** (our patch
    /// level 2+ — `pf_vdisplay::gamescope_composites_cursor`): the XFixes reader would then be
    /// redundant work producing a SECOND pointer. Resolved by [`cursor_blend_for`]'s sibling so
    /// the two answers cannot disagree.
    pub gamescope_cursor: bool,
    /// Ceiling on the encoder's per-frame slice count, from the client's
    /// [`VIDEO_CAP_MULTI_SLICE`](punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE): 32 (= no
    /// client-side limit, the backend picks its own multi-slice default, §7 LN1) when the bit is
    /// set, 1 (single-slice frames — the pre-0.17 wire shape TV-SoC decoders like Amlogic
    /// require) when it isn't. Applied to EVERY encoder this plan opens (initial + all rebuilds)
    /// so the slicing can never change shape across a mode/bitrate/stall rebuild.
    pub max_slices: u32,
}

impl SessionPlan {
    /// Resolve the whole plan once from [`config`](crate::config) + the negotiated `bit_depth`,
    /// `hdr`, `chroma`, and `codec`. `hdr` is passed IN (the handshake's verdict — the Welcome's
    /// colour label) rather than derived from the depth: since 10-bit SDR exists, `bit_depth ==
    /// 10` no longer implies BT.2020 PQ, and deriving it here re-welded the two.
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
            // Set by the resolve callers (they know the compositor); default off keeps every
            // non-gamescope plan unchanged.
            gamescope_cursor: false,
            max_slices: if multi_slice { 32 } else { 1 },
        }
    }

    /// The capturer's target output format (Goal-1 stage 5): `gpu` from the already-resolved `encoder`
    /// (no second backend probe), `hdr` from the plan. Handed into `capture::capture_virtual_output` so the
    /// capturer never re-derives the encode backend.
    pub fn output_format(&self) -> crate::capture::OutputFormat {
        let gpu = self.encoder.is_gpu();
        // Linux NVENC 4:4:4: libavcodec `hevc_nvenc` only emits 4:4:4 from a YUV444 *input* frame —
        // RGB-in is always subsampled to 4:2:0 (verified on the RTX 5070 Ti). With zero-copy
        // enabled the import worker produces that input ON the GPU (`ImportKind::Tiled444` — the
        // planar-YUV444 convert), so the session stays fully zero-copy at full chroma. Without
        // zero-copy the encoder swscales CPU RGB → YUV444P, which needs CPU-resident frames —
        // force the GPU capture off for that case only. (VAAPI 4:4:4, where the hardware supports
        // it, keeps its dmabuf path via `scale_vaapi`; Windows NVENC ingests BGRA directly.)
        #[cfg(target_os = "linux")]
        let gpu = {
            let force_cpu_for_nvenc_444 = self.chroma.is_444()
                && !crate::encode::linux_zero_copy_is_vaapi()
                && !crate::zerocopy::enabled();
            if gpu && force_cpu_for_nvenc_444 {
                // Surface the trade loudly: this is the single biggest per-frame cost a 4:4:4
                // session adds (full-res CPU readback + swscale RGB→YUV444P every frame), and
                // it looks like an unexplained fps ceiling if you don't know it happened.
                //
                // Name the SESSION's codec, not the backend the gate is named after. The gate
                // keys on `linux_zero_copy_is_vaapi()`, which reads the host-global encoder pref
                // — so a per-session PyroWave negotiation on an NVENC/auto host lands here and
                // was told it was "on the NVENC path", which is false in every particular: the
                // wavelet encoder never touches NVENC, never swscales to YUV444P, and what it
                // actually loses is the raw-dmabuf passthrough its whole design assumes.
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
        // PyroWave on Linux keeps `gpu = true`: the capture facade sees `pyrowave` below and
        // routes the session onto the raw-dmabuf passthrough (the wavelet encoder's own Vulkan
        // device imports the compositor's dmabuf on ANY vendor — `ZeroCopyPolicy::pyrowave_session`
        // advertises its importable modifiers, so Mutter+NVIDIA negotiates tiled zero-copy instead
        // of the old forced CPU-RGB readback). The EGL→CUDA importer is skipped there — its
        // payloads only NVENC consumes.
        crate::capture::OutputFormat {
            gpu,
            hdr: self.hdr,
            // 10-bit depth WITHOUT the HDR label = the 10-bit SDR session: the Windows
            // capturer expands BGRA 8→10 (`Rgb10a2Sdr`) and the display's colour state is
            // never touched.
            ten_bit_sdr: self.bit_depth == 10 && !self.hdr,
            hw_cursor: self.cursor_forward,
            // 4:4:4 needs a full-chroma source: on Windows this keeps the capturer on RGB (not the
            // default NV12/P010 video-engine output) so NVENC can CSC to 4:4:4.
            chroma_444: self.chroma.is_444(),
            // PyroWave: on Windows the IDD-push capturer makes its NV12 out-ring shareable + signals
            // a shared fence so the wavelet encoder can zero-copy-import the texture into its own
            // Vulkan device; on Linux the capture facade flips the zero-copy policy to the
            // raw-dmabuf passthrough (see above).
            pyrowave: self.codec == crate::encode::Codec::PyroWave,
            // Producer-native NV12 (gamescope) is consumable only by the Linux Vulkan Video
            // backend — resolved HERE from the plan's codec so the capturer never reaches back
            // into encode (the same one-way edge as `gpu` above). BUT the native-NV12 encode path
            // has no CSC stage to fold the cursor into — so ANY cursor-compositing session
            // (gamescope Phase C, whose XFixes pointer is absent from the PipeWire node, AND a
            // cursor-forward session, whose capture-mouse flip needs the host composite on
            // demand) must capture RGB instead, routing to the compute-CSC / VkSlotBlend blend
            // that draws `frame.cursor`. Costs the RGB→NV12 CSC we'd otherwise skip; the
            // native-NV12 cursor blend is the perf-preserving follow-up. (`cursor_blend`
            // subsumes `gamescope_cursor` — see [`cursor_blend_for`].)
            #[cfg(target_os = "linux")]
            nv12_native: crate::encode::linux_native_nv12_ok(self.codec) && !self.cursor_blend,
            #[cfg(not(target_os = "linux"))]
            nv12_native: false,
        }
    }
}

/// Process topology. Single-process is the only topology now: Linux (portal) and Windows (in-process
/// IDD-push, in the host's SYSTEM process in the interactive console session). The Windows
/// SYSTEM-host + user-session WGC relay was removed with DDA/WGC.
pub(crate) fn resolve_topology() -> SessionTopology {
    SessionTopology::SingleProcess
}

/// THE rule for [`SessionPlan::cursor_blend`], shared by every resolve caller (initial plan and
/// the mid-stream compositor re-gate) so they can't drift:
/// * **Linux**: the encoder is the compositing stage — blend for a cursor-forward session (the
///   capture-mouse flip needs the host composite on demand), for gamescope (its capture
///   carries no pointer at all; the XFixes-sourced cursor must be drawn into the video), AND
///   for a no-channel session whenever the resolved backend can composite. The pre-channel
///   "compositor EMBEDS the pointer" fallback is a fiction on a Mutter virtual stream:
///   cursor-only motion never re-records the stream (probed on-glass, Mutter 50.3 — frames
///   froze the instant motion went relative while `SPA_META_Cursor` kept updating), so a
///   capture-latched client (which never advertises `CLIENT_CAP_CURSOR`, `console.rs`
///   `latched_mouse`) streamed cursorless. Metadata + host blend is the path that was
///   verified end-to-end; embedded remains only the can't-blend fallback (libav
///   VAAPI/NVENC, software).
/// * **Everywhere else**: never. On Windows the IDD capturer composites the pointer itself
///   (`cursor_blend.rs` / DWM), and no Windows encode backend reads `frame.cursor`; asking the
///   encoder anyway made `open_video`'s blends-cursor backstop fire spuriously on every
///   cursor-channel session. The rule is gated on Linux rather than against Windows because the
///   terms it reads — the VAAPI/CUDA prediction and the zero-copy switch — exist only there.
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
            // gamescope's capture carries no SPA_META_Cursor; the blend-capable term below
            // must not apply, or a patch-2+ gamescope (composites its own pointer) would lose
            // its native-NV12 zero-copy shape for a blend that can never receive an overlay.
            return gamescope_needs_host_cursor(true);
        }
        if cursor_forward {
            return true;
        }
        // No cursor channel: the same CUDA-payload prediction `handshake::cursor_forward` and
        // the GameStream monitor mirror make — the NVIDIA resolution plus the zero-copy master
        // switch — deciding direct-SDK NVENC (blends) vs libav NVENC (doesn't).
        let cuda_planned = !crate::encode::linux_zero_copy_is_vaapi() && crate::zerocopy::enabled();
        crate::encode::cursor_blend_capable(codec, cuda_planned, bit_depth == 10)
    }
}

/// Does a gamescope session still need the HOST to composite its pointer?
///
/// It always did: gamescope keeps the cursor on a hardware plane for scanout and never painted it
/// into its PipeWire node, so the host read it from XFixes and blended it into every frame. Our
/// carried patch (level 2+, `--pipewire-composite-cursor`) puts it in the node instead — and then
/// the host must NOT blend, or the pointer is drawn twice.
///
/// This is worth more than saving a blend. A session that composites forces the encoder onto its
/// compute colour-conversion arm, because the zero-copy RGB-direct source hands the captured
/// buffer to a fixed-function front end that has no blend stage. So a gamescope session with the
/// cursor in the node is the first one that can be genuinely zero-copy end to end.
#[cfg(not(target_os = "windows"))]
fn gamescope_needs_host_cursor(gamescope: bool) -> bool {
    gamescope && !pf_vdisplay::gamescope_composites_cursor()
}

/// Should this session attach the XFixes cursor reader — i.e. is this a gamescope session whose
/// pointer the host still has to source and composite itself? The `SessionPlan::gamescope_cursor`
/// resolver, kept beside [`cursor_blend_for`] because the two must give the same answer: attaching
/// the reader without the blend wastes an X11 connection, and blending without it streams no
/// pointer at all.
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
    // `PUNKTFUNK_ENCODER=software` forces the GPU-less openh264 path — which must take CPU-staged
    // capture (`EncoderBackend::Software.is_gpu() == false` → `output_format().gpu = false`), so the
    // portal capturer delivers CPU RGB. Everything else stays `PlatformAuto` (NVENC/VAAPI resolved
    // inside `encode::open_video`).
    match pf_host_config::config().encoder_pref.as_str() {
        "software" | "sw" | "openh264" => EncoderBackend::Software,
        _ => EncoderBackend::PlatformAuto,
    }
}
