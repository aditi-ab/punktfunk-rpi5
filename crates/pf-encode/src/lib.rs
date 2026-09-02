//! Hardware video encode. Binds FFmpeg and vendor SDKs; never rewrites codecs.
//! Low-latency preset, B-frames off.
//!
//! One [`Encoder`] trait, selected in [`open_video`]. Per-GPU backends: NVENC
//! (NVIDIA; GPU RGB→YUV, no host CSC), VAAPI (AMD/Intel; CPU RGB→NV12 or
//! dmabuf into a VA surface), plus optional Vulkan Video, direct-SDK NVENC,
//! AMF, QSV, PyroWave, and software openh264.
//!
//! Capture→encode is one-way: this crate depends on `pf-frame` and
//! `pf-zerocopy`, never on capture. Pin with `PUNKTFUNK_ENCODER`.
//! Evidence: `design/linux-direct-nvenc.md`, `design/linux-vulkan-video-encode.md`,
//! `design/native-amf-encoder.md`, `design/native-qsv-encoder.md`.

use anyhow::Result;
use pf_frame::{CapturedFrame, PixelFormat};

#[path = "enc/codec.rs"]
mod codec;
pub use codec::*;

impl Codec {
    /// `quic::CODEC_*` bits this host can emit on the native path, given the
    /// resolved backend. Fed to [`punktfunk_core::quic::resolve_codec`].
    ///
    /// Software is H.264 only. Probed backends advertise what the GPU encodes
    /// ([`vaapi_codec_support`] / [`windows_codec_support`]); NVENC falls back to
    /// the GameStream superset when the probe cannot answer. An empty probe
    /// means the GPU was unusable at probe time, not that it encodes nothing —
    /// fall back to the superset so auto clients still land on HEVC.
    pub fn host_wire_caps() -> u8 {
        // PyroWave ORs onto the H.26x set; `resolve_codec` ignores the bit unless
        // the client prefers it. Advertised whenever a Vulkan GPU could open;
        // software/GPU-less keeps it off. Resolve the backend once — this path
        // is polled, and the auto arm samples live GPU-preference state.
        #[cfg(target_os = "linux")]
        let backend = linux_resolved_backend();
        #[cfg(all(target_os = "linux", feature = "pyrowave"))]
        let pyro = if backend != LinuxBackend::Software {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0u8
        };
        // Own Vulkan device by render-GPU id; the H.26x backend is irrelevant.
        // Software/GPU-less keeps the bit off. Interop is confirmed at encoder
        // open (`pyrowave_device_confirm_interop_support`); a failed open
        // renegotiates to HEVC.
        #[cfg(all(target_os = "windows", feature = "pyrowave"))]
        let pyro = if windows_resolved_backend() != WindowsBackend::Software {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0u8
        };
        #[cfg(not(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave")))]
        let pyro = 0u8;
        let base = (|| {
            /// GameStream `SERVER_CODEC_MODE_SUPPORT` for an unprobed backend.
            const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
                | punktfunk_core::quic::CODEC_HEVC
                | punktfunk_core::quic::CODEC_AV1;
            #[cfg(target_os = "linux")]
            {
                if backend == LinuxBackend::Software {
                    return punktfunk_core::quic::CODEC_H264;
                }
                // Forced-vulkan pref is a ceiling, never a replacement: the arm
                // encodes HEVC/AV1 only (H.264 dies at open). A static HEVC|AV1
                // would add AV1 on GPUs whose probe withholds it. No
                // `vulkan-encode` feature → advertise nothing.
                let pref_ceiling: u8 = match backend {
                    // Resolver knows the pref is vulkan; only this cfg! knows
                    // the build can open it. Else: advertise-then-die-at-open.
                    LinuxBackend::Vulkan => {
                        if cfg!(feature = "vulkan-encode") {
                            punktfunk_core::quic::CODEC_HEVC | punktfunk_core::quic::CODEC_AV1
                        } else {
                            0
                        }
                    }
                    _ => GPU_SUPERSET,
                };
                if linux_zero_copy_is_vaapi_for(backend) {
                    if let Some(m) = vaapi_codec_support().wire_mask() {
                        return m & pref_ceiling;
                    }
                }
                // Driver GUID list, like the VAAPI arm. Fail-open: `None` leaves
                // the historical superset, so this can only narrow.
                #[cfg(feature = "nvenc")]
                if backend == LinuxBackend::Nvenc {
                    if let Some(m) = nvenc_codec_support().wire_mask() {
                        return m & pref_ceiling;
                    }
                }
                GPU_SUPERSET & pref_ceiling
            }
            #[cfg(target_os = "windows")]
            {
                if windows_resolved_backend() == WindowsBackend::Software {
                    return punktfunk_core::quic::CODEC_H264;
                }
                if windows_backend_is_probed() {
                    if let Some(m) = windows_codec_support().wire_mask() {
                        return m;
                    }
                }
                GPU_SUPERSET
            }
            // No GPU encode backend on this target — keep the unprobed advertisement.
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                let _ = GPU_SUPERSET;
                match pf_host_config::config().encoder_pref.as_str() {
                    "software" | "sw" | "openh264" => punktfunk_core::quic::CODEC_H264,
                    _ => punktfunk_core::quic::CODEC_HEVC,
                }
            }
        })();
        base | pyro
    }
}

/// Open a hardware encoder for `format` and mode. NVENC on NVIDIA, VAAPI on
/// AMD/Intel. `cuda` is GPU frames (`AV_PIX_FMT_CUDA`) from the NVIDIA
/// zero-copy path; otherwise packed RGB/BGR CPU frames. The caller derives
/// `cuda` from the first captured frame. Linux auto-detects; override with
/// `PUNKTFUNK_ENCODER=auto|nvenc|vaapi`.
#[allow(clippy::too_many_arguments)]
pub fn open_video(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    // Backends whose fast path can't blend (Vulkan EFC) key off `cursor_blend`.
    cursor_blend: bool,
    // Client decoder slice ceiling. 1 = single-slice (some TVs wedge on
    // multi-slice AUs); 32 = no client limit. `PUNKTFUNK_NVENC_SLICES` overrides.
    max_slices: u32,
) -> Result<Box<dyn Encoder>> {
    let (inner, backend) = open_video_backend(
        codec,
        format,
        width,
        height,
        fps,
        bitrate_bps,
        cuda,
        bit_depth,
        chroma,
        cursor_blend,
        max_slices,
    )?;
    // Backend label from the branch that opened, not re-derived — Vulkan Video
    // falls back to VAAPI, and a dispatch mirror would report the wrong one.
    // GPU identity is [`pf_gpu::selected_gpu`]. Drop ends the live-session record.
    let gpu = if backend == "software" {
        pf_gpu::ActiveGpu {
            id: String::new(),
            name: "CPU (openh264)".into(),
            vendor_id: 0,
            backend,
        }
    } else {
        match pf_gpu::selected_gpu() {
            Some(sel) => pf_gpu::ActiveGpu {
                id: sel.info.id,
                name: sel.info.name,
                vendor_id: sel.info.vendor_id,
                backend,
            },
            None => pf_gpu::ActiveGpu {
                id: String::new(),
                name: "GPU".into(),
                vendor_id: 0,
                backend,
            },
        }
    };
    // Open-time fallback (Vulkan→VAAPI) and gamescope (no embedded cursor) still
    // reach here. `open_video` cannot re-plan capture, so a warning is all it does.
    if cursor_blend && !inner.caps().blends_cursor {
        tracing::warn!(
            backend,
            "session negotiated a composited cursor but this encode backend does not blend \
             CapturedFrame::cursor — the pointer will be MISSING from the stream unless the \
             capturer composites it"
        );
    }
    Ok(Box::new(TrackedEncoder {
        inner,
        _session: pf_gpu::session_begin(gpu),
    }))
}

/// Ties the `pf_gpu` live-session record to the encoder's lifetime; pure delegation
/// otherwise.
struct TrackedEncoder {
    inner: Box<dyn Encoder>,
    _session: pf_gpu::ActiveSession,
}

impl Encoder for TrackedEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        self.inner.submit(frame)
    }
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.inner.submit_indexed(frame, wire_index)
    }
    fn caps(&self) -> EncoderCaps {
        self.inner.caps()
    }
    fn request_keyframe(&mut self) {
        self.inner.request_keyframe()
    }
    fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.inner.set_hdr_meta(meta)
    }
    fn invalidate_ref_frames(&mut self, first_frame: i64, last_frame: i64) -> bool {
        self.inner.invalidate_ref_frames(first_frame, last_frame)
    }
    fn distrust_references(&mut self) {
        self.inner.distrust_references()
    }
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.inner.set_pipelined(on)
    }
    fn set_wire_chunking(&mut self, shard_payload: usize) {
        self.inner.set_wire_chunking(shard_payload)
    }
    fn set_send_spread_us(&mut self, us: u32) {
        self.inner.set_send_spread_us(us)
    }
    fn set_input_ring_depth(&mut self, depth: usize) {
        self.inner.set_input_ring_depth(depth)
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.inner.poll()
    }
    fn supports_chunked_poll(&self) -> bool {
        self.inner.supports_chunked_poll()
    }
    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.inner.poll_chunk()
    }
    fn reset(&mut self) -> bool {
        self.inner.reset()
    }
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.inner.reconfigure_bitrate(bps)
    }
    fn applied_bitrate_bps(&self) -> Option<u64> {
        self.inner.applied_bitrate_bps()
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// openh264 rate-control misconfigures if handed a hardware-session bitrate.
/// Shared by both OS software arms.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const SW_BITRATE_CEIL: u64 = 100_000_000;

/// Linux half of [`open_video_backend`] with the pref injected. `set_var`
/// races `getenv` in parallel tests, and `pf_host_config::config()` latches
/// once. Shared dim/fps/chroma checks stay in the caller.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn open_video_backend_linux(
    pref: &str,
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    // Negotiated PyroWave bypasses `PUNKTFUNK_ENCODER` (that pref is a lab override).
    if codec == Codec::PyroWave {
        #[cfg(feature = "pyrowave")]
        {
            // Worker seam, not the encoder: GPU-priority needs `CAP_SYS_NICE`,
            // which only `punktfunk-encode-worker` may carry. See `pyrowave_remote`.
            return pyrowave_remote::open_preferring_worker(
                width,
                height,
                fps,
                bitrate_bps,
                chroma,
            )
            .map(|e| (e, "pyrowave"));
        }
        #[cfg(not(feature = "pyrowave"))]
        anyhow::bail!(
            "session negotiated PyroWave but this host was built without --features \
             punktfunk-host/pyrowave (the advertisement bit should not have been set)"
        );
    }
    // Default VAAPI. With `vulkan-encode` + `PUNKTFUNK_VULKAN_ENCODE`, HEVC/AV1
    // opens Vulkan Video (RFI VAAPI cannot express); a failed open falls back
    // so the stream does not die. `format`/`bit_depth`/`chroma` are VAAPI-only
    // — Vulkan imports the dmabuf and does its own CSC.
    let open_amd_intel = || -> Result<(Box<dyn Encoder>, &'static str)> {
        // HDR keeps Vulkan when the device probe says yes (same profile query
        // the open makes). Gamescope has no embedded cursor — CSC blend is the
        // only pointer path. A `no` goes to VAAPI here, not a failed open.
        #[cfg(feature = "vulkan-encode")]
        let ten_bit_session = bit_depth == 10 && format.is_hdr_rgb10();
        #[cfg(feature = "vulkan-encode")]
        if matches!(codec, Codec::H265 | Codec::Av1)
            && vulkan_encode_enabled()
            && vulkan_encode_available_at(codec, ten_bit_session)
        {
            match vulkan_video::VulkanVideoEncoder::open(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                cursor_blend,
            ) {
                Ok(e) => {
                    tracing::info!(
                        codec = ?codec,
                        "Linux Vulkan Video encode (real RFI via DPB reference slots) — \
                         set PUNKTFUNK_VULKAN_ENCODE=0 for libav VAAPI"
                    );
                    return Ok((Box::new(e) as Box<dyn Encoder>, "vulkan"));
                }
                // Native NV12 has no VAAPI fallback: libav would import the
                // two-plane buffer as packed RGB (silent garbage). Die instead.
                Err(e) if format == PixelFormat::Nv12 => {
                    return Err(e.context(
                        "Vulkan Video open failed on a native-NV12 capture \
                         — no VAAPI fallback exists; set PUNKTFUNK_PIPEWIRE_NV12=0 to \
                         restore the packed-RGB negotiation",
                    ));
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "Vulkan Video encode open failed — falling back to libav VAAPI"
                ),
            }
        }
        // VAAPI also cannot ingest native NV12 (Vulkan ineligible).
        if format == PixelFormat::Nv12 {
            anyhow::bail!(
                "native NV12 capture requires the Vulkan Video encoder (HEVC/AV1 \
                 session, --features vulkan-encode, PUNKTFUNK_VULKAN_ENCODE not 0) — this \
                 session resolved to libav VAAPI; set PUNKTFUNK_PIPEWIRE_NV12=0 to restore \
                 the packed-RGB negotiation"
            );
        }
        vaapi::VaapiEncoder::open(
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            bit_depth,
            chroma,
        )
        .map(|e| (Box::new(e) as Box<dyn Encoder>, "vaapi"))
    };
    let open_nvidia = || -> Result<(Box<dyn Encoder>, &'static str)> {
        open_nvenc_probed(
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )
        .map(|e| (e, "nvenc"))
    };
    // Same resolver the capability mirrors consult, so the alias table exists once.
    match resolve_linux_backend(pref, linux_auto_is_vaapi, cuda) {
        Some(LinuxBackend::Nvenc) => open_nvidia(),
        Some(LinuxBackend::AmdIntel) => open_amd_intel(),
        Some(LinuxBackend::Vulkan) => {
            #[cfg(feature = "vulkan-encode")]
            {
                if !matches!(codec, Codec::H265 | Codec::Av1) {
                    anyhow::bail!(
                        "the Vulkan Video encoder supports HEVC + AV1; the session negotiated {codec:?}"
                    );
                }
                vulkan_video::VulkanVideoEncoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    cursor_blend,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "vulkan"))
            }
            #[cfg(not(feature = "vulkan-encode"))]
            {
                let _ = (format, bit_depth, chroma);
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=vulkan requires a build with --features vulkan-encode"
                )
            }
        }
        // Explicit lab override; ignores the negotiated codec (every AU is intra).
        Some(LinuxBackend::Pyrowave) => {
            #[cfg(feature = "pyrowave")]
            {
                tracing::warn!(
                    ?codec,
                    "PUNKTFUNK_ENCODER=pyrowave forces the all-intra wavelet stream \
                     regardless of the negotiated codec — only a pyrowave-feature client \
                     that ALSO preferred CODEC_PYROWAVE can display it (lab override; \
                     normal sessions negotiate it instead)"
                );
                // Forced onto a session negotiated for another codec, whose
                // chroma may be HEVC 4:4:4 — PyroWave does not. Same worker
                // seam as the negotiated arm; this must not skip it.
                pyrowave_remote::open_preferring_worker(
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    ChromaFormat::Yuv420,
                )
                .map(|e| (e, "pyrowave"))
            }
            #[cfg(not(feature = "pyrowave"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=pyrowave requires a build with --features punktfunk-host/pyrowave"
                )
            }
        }
        // Explicit-only: `auto` never picks it (a dead NVIDIA driver still
        // exposes `/dev/nvidiactl` and would resolve to NVENC). H.264 + CPU RGB.
        Some(LinuxBackend::Software) => {
            if codec != Codec::H264 {
                anyhow::bail!(
                    "the software encoder emits H.264 only; the session negotiated {codec:?} \
                     (a client must advertise CODEC_H264 to reach a software host)"
                );
            }
            let _ = (cuda, bit_depth); // software path is CPU + 8-bit only
            sw::OpenH264Encoder::open(
                format,
                width,
                height,
                fps,
                bitrate_bps.min(SW_BITRATE_CEIL),
            )
            .map(|e| (Box::new(e) as Box<dyn Encoder>, "software"))
        }
        None => anyhow::bail!(
            "unknown PUNKTFUNK_ENCODER={pref:?} — use auto (default), nvenc, vaapi, vulkan, pyrowave, or software"
        ),
    }
}

/// Open the platform encoder. The display label is the branch that opened
/// (`nvenc`/`vaapi`/`vulkan`/`amf`/`qsv`/`software`), including internal
/// fallbacks (Vulkan Video → VAAPI). Feeds the mgmt live-session record.
#[allow(clippy::too_many_arguments)]
fn open_video_backend(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    // Linux vulkan-encode + direct-NVENC only (`max_slices`: the splitter).
    let _ = (cursor_blend, max_slices);
    validate_dimensions(codec, width, height)?;
    // `fps` is `Rational(1, fps)` and `pts * 1e9 / fps`. 0 is a 1/0 rational
    // and a divide-by-zero; 1000 is a sanity ceiling.
    if fps == 0 || fps > 1000 {
        anyhow::bail!("invalid refresh/fps {fps}: must be 1..=1000 Hz");
    }
    // 4:4:4 is HEVC- and PyroWave-only. Degrade rather than emit a stream no
    // decoder expects.
    let chroma = if chroma.is_444() && codec != Codec::H265 && codec != Codec::PyroWave {
        tracing::warn!(
            ?codec,
            "4:4:4 requested for a non-HEVC codec — encoding 4:2:0"
        );
        ChromaFormat::Yuv420
    } else {
        chroma
    };
    #[cfg(target_os = "linux")]
    {
        open_video_backend_linux(
            pf_host_config::config().encoder_pref.as_str(),
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )
    }
    #[cfg(target_os = "windows")]
    {
        // Negotiated PyroWave first, like Linux. Own Vulkan device; H.26x
        // selection below is moot. See `design/pyrowave-windows-host-zerocopy.md`.
        if codec == Codec::PyroWave {
            #[cfg(feature = "pyrowave")]
            {
                let _ = (format, cuda);
                return pyrowave::PyroWaveEncoder::open(
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    chroma,
                    bit_depth,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "pyrowave"));
            }
            #[cfg(not(feature = "pyrowave"))]
            anyhow::bail!(
                "session negotiated PyroWave but this host was built without --features \
                 punktfunk-host/pyrowave (the advertisement bit should not have been set)"
            );
        }
        let _ = cuda; // always false on Windows (no Cuda payload)
        let backend = windows_resolved_backend();
        // Pin vs selected-vendor mismatch is overridden; honoring it can only
        // fail. Warn so the stale pin is removed from host.env.
        if windows_pinned_backend().is_some_and(|pin| pin != backend) {
            tracing::warn!(
                adapter = pf_gpu::selected_gpu()
                    .map(|s| s.info.name)
                    .as_deref()
                    .unwrap_or("?"),
                pinned = %pf_host_config::config().encoder_pref,
                using = ?backend,
                "explicit PUNKTFUNK_ENCODER pin does not match the selected GPU's vendor — the \
                 pin is overridden (remove it from host.env, or point the GPU preference at the \
                 pinned vendor's adapter)"
            );
        }
        match backend {
            WindowsBackend::Nvenc => {
                // DXGI capturer uses `FramePayload::D3d11` under the same env
                // so capture and encode share textures.
                #[cfg(feature = "nvenc")]
                {
                    nvenc::NvencD3d11Encoder::open(
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        bit_depth,
                        chroma,
                        max_slices,
                    )
                    .map(|e| (Box::new(e) as Box<dyn Encoder>, "nvenc"))
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    anyhow::bail!(
                        "NVENC requested/detected but this host was built without it — rebuild \
                         with `--features nvenc`"
                    )
                }
            }
            WindowsBackend::Amf => {
                // Native AMF only; no build feature (`amfrt64.dll` at runtime).
                // A missing runtime fails here — no silent FFmpeg degrade.
                // See `design/native-amf-encoder.md`.
                amf::AmfEncoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    bit_depth,
                    chroma,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "amf"))
                .map_err(|e| {
                    e.context(
                        "native AMF encode failed to open (update the AMD driver / amfrt64.dll \
                         runtime)",
                    )
                })
            }
            WindowsBackend::Qsv => {
                // Native VPL first. ffmpeg on open-failure, or
                // `PUNKTFUNK_QSV_FFMPEG=1`. See `design/native-qsv-encoder.md`.
                #[cfg(feature = "qsv")]
                {
                    // Trim so `"1 "` from a shell/`.env` takes effect. Keep
                    // `TRUE` — a bare `matches!` would drop that spelling.
                    let ffmpeg_forced = std::env::var("PUNKTFUNK_QSV_FFMPEG").is_ok_and(|v| {
                        matches!(
                            v.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    });
                    if !ffmpeg_forced {
                        match qsv::QsvEncoder::open(
                            codec,
                            format,
                            width,
                            height,
                            fps,
                            bitrate_bps,
                            bit_depth,
                            chroma,
                        ) {
                            Ok(e) => return Ok((Box::new(e) as Box<dyn Encoder>, "qsv")),
                            Err(e) => {
                                #[cfg(feature = "amf-qsv")]
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    "native QSV open failed — falling back to the ffmpeg QSV path"
                                );
                                #[cfg(not(feature = "amf-qsv"))]
                                return Err(e.context(
                                    "native QSV encode failed to open (update the Intel driver / \
                                     no VPL runtime on this box)",
                                ));
                            }
                        }
                    } else {
                        tracing::warn!(
                            "PUNKTFUNK_QSV_FFMPEG=1 — skipping native QSV (bring-up escape hatch)"
                        );
                    }
                }
                // Native open-failure fallback and the `PUNKTFUNK_QSV_FFMPEG` hatch.
                #[cfg(feature = "amf-qsv")]
                {
                    ffmpeg_win::FfmpegWinEncoder::open(
                        ffmpeg_win::WinVendor::Qsv,
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        bit_depth,
                        chroma,
                    )
                    .map(|e| (Box::new(e) as Box<dyn Encoder>, "qsv"))
                }
                #[cfg(all(not(feature = "amf-qsv"), not(feature = "qsv")))]
                {
                    anyhow::bail!(
                        "Intel (QSV) encode requested/detected but this host was built without \
                         it — rebuild with `--features qsv` (native VPL) or `--features amf-qsv` \
                         (libavcodec)"
                    )
                }
                #[cfg(all(not(feature = "amf-qsv"), feature = "qsv"))]
                {
                    anyhow::bail!(
                        "native QSV was skipped via PUNKTFUNK_QSV_FFMPEG but this host was built \
                         without the ffmpeg fallback (`amf-qsv`)"
                    )
                }
            }
            WindowsBackend::Software => {
                anyhow::ensure!(
                    codec == Codec::H264,
                    "the Windows software encoder supports H.264 only; client negotiated {codec:?} \
                     (build a GPU backend: --features nvenc or amf-qsv, or request H264)"
                );
                let _ = (bit_depth, chroma); // the software H.264 path is 8-bit 4:2:0 only
                sw::OpenH264Encoder::open(
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps.min(SW_BITRATE_CEIL),
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "software"))
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            max_slices,
        );
        anyhow::bail!("video encode requires Linux or Windows")
    }
}

/// Open NVENC, probing this GPU's real max bitrate. `avcodec_open2` returns
/// EINVAL when the rate exceeds what this chip's codec level can express, and
/// that ceiling is GPU/driver-specific. Open at the requested rate first; step
/// down only if this GPU refuses. The codec's theoretical level is the first
/// step-down candidate, not a blind cap.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn open_nvenc_probed(
    codec: Codec,
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    cuda: bool,
    bit_depth: u8,
    chroma: ChromaFormat,
    cursor_blend: bool,
    max_slices: u32,
) -> Result<Box<dyn Encoder>> {
    #[cfg(not(feature = "nvenc"))]
    let _ = (cursor_blend, max_slices);
    // Default on NVIDIA, CUDADEVICEPTR only — CPU/dmabuf stays on libav.
    // `PUNKTFUNK_NVENC_DIRECT=0` falls back. Self-clamps bitrate, so it skips
    // the probe loop below.
    #[cfg(feature = "nvenc")]
    if cuda && nvenc_direct_enabled() {
        tracing::info!(
            codec = codec.nvenc_name(),
            "Linux direct-SDK NVENC (real RFI + recovery anchor) — set PUNKTFUNK_NVENC_DIRECT=0 for libav"
        );
        return Ok(Box::new(nvenc_cuda::NvencCudaEncoder::open(
            codec,
            format,
            width,
            height,
            fps,
            bitrate_bps,
            cuda,
            bit_depth,
            chroma,
            cursor_blend,
            max_slices,
        )?) as Box<dyn Encoder>);
    }
    // Featureless build compiles the direct path out; a CUDA session then
    // loses RFI and in-place bitrate reconfigure with no log. Skip the warn
    // when the operator chose libav (`PUNKTFUNK_NVENC_DIRECT=0`).
    #[cfg(not(feature = "nvenc"))]
    if cuda
        && !std::env::var("PUNKTFUNK_NVENC_DIRECT")
            .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
            .unwrap_or(false)
    {
        // Once per process: featureless builds rebuild on every bitrate step.
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "direct-SDK NVENC is NOT compiled into this build (`--features punktfunk-host/nvenc`) \
                 — CUDA frames take the libav path: no RFI loss recovery, and every adaptive-bitrate \
                 step costs an encoder rebuild + IDR"
            );
        }
    }
    const MIN_PROBE_BPS: u64 = 50_000_000;
    let mut candidates = vec![bitrate_bps];
    let cap = codec.max_bitrate_bps();
    if cap < bitrate_bps {
        candidates.push(cap);
    }
    let mut b = bitrate_bps.min(cap);
    while b > MIN_PROBE_BPS {
        b = b * 3 / 4;
        candidates.push(b);
    }
    let mut last: Option<anyhow::Error> = None;
    for (i, &b) in candidates.iter().enumerate() {
        match linux::NvencEncoder::open(
            codec, format, width, height, fps, b, cuda, bit_depth, chroma,
        ) {
            Ok(enc) => {
                if i > 0 {
                    tracing::warn!(
                        requested_mbps = bitrate_bps / 1_000_000,
                        opened_mbps = b / 1_000_000,
                        codec = codec.nvenc_name(),
                        "this GPU's NVENC refused the requested bitrate (EINVAL) — opened at the \
                         highest rate it accepts; request AV1 or a lower bitrate for more"
                    );
                }
                return Ok(Box::new(enc) as Box<dyn Encoder>);
            }
            // EINVAL = above this GPU's level ceiling → step down. Any other
            // failure is real — do not mask it with bitrate retries.
            Err(e) if nvenc_open_einval(&e) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("encoder open failed at every probed bitrate")))
}

/// Whether a libav NVENC open failed with EINVAL — the bitrate-ceiling signal
/// [`open_nvenc_probed`]'s ladder steps down on. Match the root `ffmpeg::Error`
/// through the `anyhow` chain; an English strerror match also fired on other
/// wrapped EINVAL (CUDA-context errno) and stole the ladder.
#[cfg(target_os = "linux")]
fn nvenc_open_einval(e: &anyhow::Error) -> bool {
    use ffmpeg_next as ffmpeg;
    matches!(
        e.downcast_ref::<ffmpeg::Error>(),
        Some(ffmpeg::Error::Other {
            errno: ffmpeg::util::error::EINVAL
        })
    )
}

/// Direct-SDK NVENC. Default on. `PUNKTFUNK_NVENC_DIRECT=0` (`false`/`no`/`off`)
/// is the libav hatch. Consulted only for a CUDA payload with `--features nvenc`;
/// the `cuda` gate in [`open_nvenc_probed`] keeps AMD/Intel on VAAPI.
#[cfg(all(target_os = "linux", feature = "nvenc"))]
fn nvenc_direct_enabled() -> bool {
    std::env::var("PUNKTFUNK_NVENC_DIRECT")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

/// Vulkan Video HEVC/AV1 on AMD/Intel. Default on.
/// `PUNKTFUNK_VULKAN_ENCODE=0` (`false`/`no`/`off`) is the libav-VAAPI hatch.
/// A failed open falls back to VAAPI. See `design/linux-vulkan-video-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_enabled() -> bool {
    std::env::var("PUNKTFUNK_VULKAN_ENCODE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

/// Whether this session can ingest producer-native NV12. Only Vulkan Video
/// can; libav VAAPI would import the two-plane buffer as packed RGB.
///
/// Once the producer has been asked for two-plane NV12 there is **no
/// fallback**. [`open_video`] makes a failed Vulkan open fatal rather than
/// degrade to VAAPI (silent garbage). A wrong `true` kills the session at
/// its first frame. Conjuncts are cheapest-first; the device probe runs last.
#[cfg(target_os = "linux")]
pub fn linux_native_nv12_ok(codec: Codec) -> bool {
    #[cfg(feature = "vulkan-encode")]
    {
        matches!(codec, Codec::H265 | Codec::Av1)
            && vulkan_encode_enabled()
            // Same auto+pref decision `open_video` makes. A denylist of explicit
            // skip prefs misses `""` → `auto` → NVENC on NVIDIA.
            && linux_zero_copy_is_vaapi()
            // Last: this opens a Vulkan instance.
            && vulkan_encode_available(codec)
    }
    #[cfg(not(feature = "vulkan-encode"))]
    {
        let _ = codec;
        false
    }
}

/// May an HDR capture stay zero-copy on NVIDIA (packed 10-bit PQ/BT.2020 CUDA)?
///
/// Only direct-SDK NVENC can: it registers `ARGB10`/`ABGR10` and CSCs in the
/// encoder. Libav HDR builds a P010 context; copying 2:10:10:10 words into
/// that surface is garbage. When the direct path is compiled out or vetoed
/// (`PUNKTFUNK_NVENC_DIRECT=0`), the capturer must not build the HDR importer.
#[cfg(target_os = "linux")]
pub fn linux_hdr_cuda_ok() -> bool {
    #[cfg(feature = "nvenc")]
    {
        // Same terms [`open_nvenc_probed`] uses for the direct arm, minus `cuda`
        // (the thing the caller is deciding).
        nvenc_direct_enabled() && !linux_zero_copy_is_vaapi()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// Whether the resolved backend composites [`CapturedFrame::cursor`]. Answered
/// before capture opens: blend-capable backends take cursor-as-metadata; else
/// the compositor must embed the pointer.
///
/// `cuda_planned` is the caller's CUDA-payload prediction; `ten_bit` the
/// negotiated depth. A CPU payload keeps NVIDIA on libav NVENC (no blend).
/// 10-bit keeps Vulkan Video only where the device advertises that profile
/// (`vulkan_encode_available_at`, the same query the open makes).
#[cfg(target_os = "linux")]
pub fn cursor_blend_capable(codec: Codec, cuda_planned: bool, ten_bit: bool) -> bool {
    // Negotiated PyroWave is selected before the pref; its CSC composites the cursor.
    if codec == Codec::PyroWave {
        return true;
    }
    let direct_nvenc = {
        #[cfg(feature = "nvenc")]
        {
            nvenc_direct_enabled()
        }
        #[cfg(not(feature = "nvenc"))]
        {
            false
        }
    };
    let vulkan_csc = {
        // Compute-CSC arm (the one that blends). Probe last: it opens a Vulkan instance.
        #[cfg(feature = "vulkan-encode")]
        {
            // Same as `open_amd_intel`, depth included, so prediction and open agree.
            matches!(codec, Codec::H265 | Codec::Av1)
                && vulkan_encode_enabled()
                && vulkan_encode_available_at(codec, ten_bit)
        }
        #[cfg(not(feature = "vulkan-encode"))]
        {
            let _ = ten_bit; // the depth only ever narrows the Vulkan arm
            false
        }
    };
    let backend = resolve_linux_backend(
        pf_host_config::config().encoder_pref.as_str(),
        linux_auto_is_vaapi,
        cuda_planned,
    );
    cursor_blend_capable_for(backend, cuda_planned, direct_nvenc, vulkan_csc)
}

/// Dispatch-mirroring core of [`cursor_blend_capable`], device-free for tests.
/// `direct_nvenc` / `vulkan_csc` are the blend-capable arms, already gated.
#[cfg(target_os = "linux")]
fn cursor_blend_capable_for(
    backend: Option<LinuxBackend>,
    cuda_planned: bool,
    direct_nvenc: bool,
    vulkan_csc: bool,
) -> bool {
    match backend {
        Some(LinuxBackend::Pyrowave) => true,
        // Direct-SDK only (VkSlotBlend), CUDA payloads only — CPU stays on libav.
        Some(LinuxBackend::Nvenc) => cuda_planned && direct_nvenc,
        // Compute-CSC blends at either depth. Cursor sessions stay off native-NV12
        // / RGB-direct, so CSC eligibility (already carrying depth) is the answer.
        Some(LinuxBackend::AmdIntel) | Some(LinuxBackend::Vulkan) => vulkan_csc,
        // Capturer may composite inline; the encoder does not. Report encoder truth.
        Some(LinuxBackend::Software) | None => false,
    }
}

/// Can this GPU open a Vulkan Video encode session for `codec`? Cached per
/// (selected GPU, codec); probe runs outside the lock.
///
/// Only [`linux_native_nv12_ok`] consults this (no-fallback). Not wired into
/// [`open_video`]: non-NV12 already degrades to VAAPI on a failed open.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_available(codec: Codec) -> bool {
    vulkan_encode_caps(codec).supported
}

/// Can Vulkan Video encode `codec` at this depth on the selected GPU? Per
/// codec: a device can advertise HEVC Main10 and still decline 10-bit AV1.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_available_at(codec: Codec, ten_bit: bool) -> bool {
    let caps = vulkan_encode_caps(codec);
    caps.supported
        && if ten_bit {
            caps.ten_bit
        } else {
            caps.eight_bit
        }
}

/// Vulkan Video encode caps for `codec`, cached per (selected GPU, codec) so
/// a console GPU change re-probes. The probe opens its own Vulkan instance.
///
/// Cfg must include `vulkan-encode`: the return type lives in `vulkan_video`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_caps(codec: Codec) -> vulkan_video::VulkanEncodeCaps {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), vulkan_video::VulkanEncodeCaps>>> =
        OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let caps = vulkan_video::probe_encode_caps(codec);
    if caps.supported {
        tracing::info!(
            ?codec,
            eight_bit = caps.eight_bit,
            ten_bit = caps.ten_bit,
            "Vulkan Video encode probed — producer-native NV12 capture is eligible, and a 10-bit \
             session keeps this backend (real RFI + the compute CSC's cursor blend) where the \
             device accepts the profile"
        );
    } else {
        tracing::info!(
            ?codec,
            "Vulkan Video encode unavailable (no encode queue for this codec) — keeping the \
             packed-RGB capture negotiation (the native-NV12 path has no VAAPI fallback)"
        );
    }
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// NVIDIA-presence for the `auto` selector: these device nodes, no CUDA
/// context (that would allocate GPU state on every maybe-NVIDIA host).
#[cfg(target_os = "linux")]
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// The `auto` Linux backend decision, shared by [`open_video`] and
/// [`linux_zero_copy_is_vaapi`]. Manual GPU preference picks that vendor's
/// backend (NVIDIA still needs the proprietary device nodes); else the
/// presence probe.
///
/// Resolves **`auto` only** — ignores `encoder_pref`. Capability probes must
/// use [`linux_zero_copy_is_vaapi`], which layers the pref on top.
#[cfg(target_os = "linux")]
fn linux_auto_is_vaapi() -> bool {
    if let Some(g) = pf_gpu::manual_selection() {
        if g.vendor_id == pf_gpu::VENDOR_NVIDIA {
            return !nvidia_present();
        }
        return true;
    }
    !nvidia_present()
}

/// Resolved Linux encode backend. One alias table for [`open_video_backend`]
/// and every capability/advertisement mirror. Labels come from the open
/// sites — this picks which arm runs, not what actually opened.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxBackend {
    Nvenc,
    AmdIntel,
    Vulkan,
    Pyrowave,
    Software,
}

/// Pure core. `None` = unknown pref: [`open_video_backend`] bails, capability
/// mirrors map it to auto via [`linux_resolved_backend`]. `auto_is_vaapi` is
/// lazy — `/serverinfo` polls these mirrors; explicit prefs must not probe.
#[cfg(target_os = "linux")]
fn resolve_linux_backend(
    pref: &str,
    auto_is_vaapi: impl FnOnce() -> bool,
    cuda: bool,
) -> Option<LinuxBackend> {
    Some(match pref {
        "nvenc" | "nvidia" | "cuda" => LinuxBackend::Nvenc,
        "vaapi" | "amd" | "intel" => LinuxBackend::AmdIntel,
        "vulkan" | "vulkan-video" => LinuxBackend::Vulkan,
        "pyrowave" => LinuxBackend::Pyrowave,
        "software" | "sw" | "openh264" => LinuxBackend::Software,
        // A CUDA frame can only be consumed by NVENC; else [`linux_auto_is_vaapi`].
        "auto" | "" => {
            if cuda || !auto_is_vaapi() {
                LinuxBackend::Nvenc
            } else {
                LinuxBackend::AmdIntel
            }
        }
        _ => return None,
    })
}

/// Capability-mirror wrapper: unknown pref → auto. `cuda = false` because
/// these answer pre-session questions.
#[cfg(target_os = "linux")]
fn linux_resolved_backend() -> LinuxBackend {
    let pref = pf_host_config::config().encoder_pref.as_str();
    resolve_linux_backend(pref, linux_auto_is_vaapi, false).unwrap_or_else(|| {
        if linux_auto_is_vaapi() {
            LinuxBackend::AmdIntel
        } else {
            LinuxBackend::Nvenc
        }
    })
}

/// Dmabuf modifiers PyroWave's Vulkan device imports for the capture fourcc.
/// VAAPI LINEAR-only starves tiled Mutter+NVIDIA allocations.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
pub fn pyrowave_capture_modifiers(fourcc: u32) -> Vec<u64> {
    pyrowave::capture_modifiers(fourcc)
}

/// True if the Linux GPU backend is VAAPI rather than NVENC — so capture
/// picks dmabuf passthrough vs EGL→CUDA. Mirrors [`open_video`].
#[cfg(target_os = "linux")]
pub fn linux_zero_copy_is_vaapi() -> bool {
    linux_zero_copy_is_vaapi_for(linux_resolved_backend())
}
/// Zero-copy plane for an already-resolved backend, so a polled caller
/// (`host_wire_caps`) pays for resolution once.
#[cfg(target_os = "linux")]
fn linux_zero_copy_is_vaapi_for(backend: LinuxBackend) -> bool {
    match backend {
        LinuxBackend::Nvenc => false,
        LinuxBackend::AmdIntel => true,
        // Raw dmabuf on any vendor — never the EGL→CUDA import.
        LinuxBackend::Pyrowave => true,
        // Preserved `_` fallthrough, not endorsed. Vulkan-on-NVIDIA is a known
        // mismatch (EGL→CUDA for a dmabuf importer); software is latent (H.264).
        LinuxBackend::Vulkan | LinuxBackend::Software => linux_auto_is_vaapi(),
    }
}

/// Codecs the active GPU can encode. AV1 encode is narrow — probe, don't assume.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone, Copy, Debug)]
pub struct CodecSupport {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl CodecSupport {
    /// `quic::CODEC_*` bits, or `None` when the probe found nothing — GPU
    /// unusable at probe time, not "zero codecs". Caller falls back to the
    /// static superset.
    pub fn wire_mask(self) -> Option<u8> {
        let mut m = 0u8;
        if self.h264 {
            m |= punktfunk_core::quic::CODEC_H264;
        }
        if self.h265 {
            m |= punktfunk_core::quic::CODEC_HEVC;
        }
        if self.av1 {
            m |= punktfunk_core::quic::CODEC_AV1;
        }
        (m != 0).then_some(m)
    }
}

/// NVIDIA encode-GUID list (cached once per process). Process-wide because
/// the direct backend opens on shared `cuda::context()` (device 0). Fail-open
/// contract: see [`nvenc_cuda::probe_support`].
#[cfg(all(target_os = "linux", feature = "nvenc"))]
pub fn nvenc_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
    static LOGGED: OnceLock<()> = OnceLock::new();
    let probed = nvenc_cuda::probe_support();
    LOGGED.get_or_init(|| {
        tracing::info!(
            h264 = probed.codecs.h264,
            h265 = probed.codecs.h265,
            av1 = probed.codecs.av1,
            hevc_444 = probed.hevc_444,
            "NVENC encode capabilities probed"
        );
    });
    probed.codecs
}

/// VAAPI encode probe (tiny encoder per codec, cached once). NVIDIA uses
/// [`nvenc_codec_support`]; callers gate on [`linux_zero_copy_is_vaapi`].
#[cfg(target_os = "linux")]
pub fn vaapi_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
    static CACHE: OnceLock<CodecSupport> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let caps = CodecSupport {
            h264: vaapi::probe_can_encode(Codec::H264),
            h265: vaapi::probe_can_encode(Codec::H265),
            av1: vaapi::probe_can_encode(Codec::Av1),
        };
        tracing::info!(
            h264 = caps.h264,
            h265 = caps.h265,
            av1 = caps.av1,
            "VAAPI encode capabilities probed"
        );
        caps
    })
}

/// Whether the active backend can emit 4:4:4 HEVC. Cached per selected GPU
/// before Welcome. 4:4:4 is HEVC-only; VAAPI/AMF/QSV must be probed, never
/// assumed. Non-HEVC is always `false`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_444(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if codec == Codec::PyroWave {
        // Own RGB→YCbCr CSC from a full-chroma source — no GPU encode probe.
        // See `design/pyrowave-444-hdr.md`.
        return true;
    }
    if codec != Codec::H265 {
        return false;
    }
    // Per selected GPU so a console preference change re-probes.
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = pf_gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            if linux_zero_copy_is_vaapi() {
                vaapi::probe_can_encode_444(codec)
            } else {
                // Direct SDK: driver's `YUV444_ENCODE` cap. Never ffmpeg-open in
                // a direct-SDK process — that wedges later opens with
                // `NV_ENC_ERR_INVALID_VERSION` until restart. Libav probe only
                // when the session will actually use libav.
                #[cfg(feature = "nvenc")]
                {
                    if nvenc_direct_enabled() {
                        nvenc_cuda::probe_support().hevc_444
                    } else {
                        linux::probe_can_encode_444(codec)
                    }
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    linux::probe_can_encode_444(codec)
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match windows_resolved_backend() {
                WindowsBackend::Nvenc => {
                    #[cfg(feature = "nvenc")]
                    {
                        nvenc::probe_can_encode_444(codec)
                    }
                    #[cfg(not(feature = "nvenc"))]
                    {
                        false
                    }
                }
                // VCN hardware limit — no probe. See `design/native-amf-encoder.md`.
                WindowsBackend::Amf => false,
                WindowsBackend::Qsv => {
                    #[cfg(feature = "amf-qsv")]
                    {
                        ffmpeg_win::probe_can_encode_444(ffmpeg_win::WinVendor::Qsv, codec)
                    }
                    #[cfg(not(feature = "amf-qsv"))]
                    {
                        false
                    }
                }
                WindowsBackend::Software => false,
            }
        }
    };
    tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
    cache.lock().unwrap().insert(key, supported);
    supported
}

/// No GPU encode backend on this target — 4:4:4 is never advertised.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_444(_codec: Codec) -> bool {
    false
}

/// Whether the active backend can emit 10-bit for `codec` (HEVC Main10 / AV1).
/// Cached per (GPU, codec) before Welcome, like [`can_encode_444`]. Without
/// this gate `PUNKTFUNK_10BIT` would negotiate 10-bit and then emit 8-bit
/// (label HDR / stream SDR).
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_10bit(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if !codec.supports_10bit() {
        return false;
    }
    if codec == Codec::PyroWave {
        // Wavelet is depth-agnostic. HDR CSC exists on the Windows IDD-push
        // path only; Linux capture has no HDR. See `design/pyrowave-444-hdr.md`.
        return cfg!(target_os = "windows");
    }
    // Per (selected GPU, codec) so a console preference change re-probes.
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), bool>>> = OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            // Use [`linux_zero_copy_is_vaapi`] (not [`linux_auto_is_vaapi`]) so
            // `encoder_pref` is honored. AMD/Intel: `open_amd_intel` tries Vulkan
            // then VAAPI — 10-bit is available if either says yes.
            if linux_zero_copy_is_vaapi() {
                let vulkan10 = {
                    #[cfg(feature = "vulkan-encode")]
                    {
                        vulkan_encode_enabled() && vulkan_encode_available_at(codec, true)
                    }
                    #[cfg(not(feature = "vulkan-encode"))]
                    {
                        false
                    }
                };
                vulkan10 || vaapi::probe_can_encode_10bit(codec)
            } else {
                // Same as the 4:4:4 arm: driver's `10BIT_ENCODE` cap, never an
                // ffmpeg open in a direct-SDK process (`NV_ENC_ERR_INVALID_VERSION`
                // wedges later opens). Libav probe only when the session uses libav.
                #[cfg(feature = "nvenc")]
                {
                    if nvenc_direct_enabled() {
                        let t = nvenc_cuda::probe_support().ten_bit;
                        match codec {
                            Codec::H265 => t.h265,
                            Codec::Av1 => t.av1,
                            _ => false,
                        }
                    } else {
                        linux::probe_can_encode_10bit(codec)
                    }
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    linux::probe_can_encode_10bit(codec)
                }
            }
        }
        #[cfg(target_os = "windows")]
        {
            match windows_resolved_backend() {
                WindowsBackend::Nvenc => {
                    #[cfg(feature = "nvenc")]
                    {
                        nvenc::probe_can_encode_10bit(codec)
                    }
                    #[cfg(not(feature = "nvenc"))]
                    {
                        false
                    }
                }
                WindowsBackend::Amf => amf::probe_can_encode_10bit(codec),
                // Native VPL Query. ffmpeg Main10 can silently encode 8-bit, so
                // without the `qsv` feature this stays an honest `false`.
                WindowsBackend::Qsv => {
                    #[cfg(feature = "qsv")]
                    {
                        qsv::probe_can_encode_10bit(codec)
                    }
                    #[cfg(not(feature = "qsv"))]
                    {
                        false
                    }
                }
                WindowsBackend::Software => false,
            }
        }
    };
    tracing::info!(codec = ?codec, supported, "10-bit encode capability probed");
    cache.lock().unwrap().insert(key, supported);
    supported
}

/// No GPU encode backend on this target — 10-bit is never negotiated.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_10bit(_codec: Codec) -> bool {
    false
}

/// Marker in an encoder error's `anyhow` chain: the failure is a deterministic
/// config consequence, so an in-place rebuild can never succeed. The reset
/// ladder downcasts this and ends the session instead of burning rebuilds.
/// Attach with `Error::new(TerminalEncoderError).context("the actual cause")`.
#[derive(Clone, Copy, Debug)]
pub struct TerminalEncoderError;

impl std::fmt::Display for TerminalEncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deterministic configuration error — an encoder rebuild cannot fix this")
    }
}

impl std::error::Error for TerminalEncoderError {}

// Windows backend selection. NVIDIA → NVENC, AMD → AMF, Intel → QSV.
// `auto` uses the selected render adapter so encode matches capture.

// Ungated: the pure reconciliation table is tested on every CI leg, not just Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsBackend {
    Nvenc,
    Amf,
    Qsv,
    Software,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// PCI vendor a Windows hardware backend can open on (`None` for software).
/// Capture, virtual display, and encoder share one adapter.
pub fn windows_backend_vendor_id(backend: WindowsBackend) -> Option<u32> {
    match backend {
        WindowsBackend::Nvenc => Some(pf_gpu::VENDOR_NVIDIA),
        WindowsBackend::Amf => Some(pf_gpu::VENDOR_AMD),
        WindowsBackend::Qsv => Some(pf_gpu::VENDOR_INTEL),
        WindowsBackend::Software => None,
    }
}

/// Pure half of [`windows_resolved_backend`]: reconcile an explicit
/// `PUNKTFUNK_ENCODER` pin with the selected adapter. A hardware pin whose
/// vendor contradicts the selected GPU is overridden (`derived` is lazy) —
/// honoring it can only fail. Software has no vendor and is always honored;
/// with no selected GPU a pin is trusted as-is. [`open_video`] warns on override.
pub fn resolve_windows_backend(
    pinned: Option<WindowsBackend>,
    selected_vendor_id: Option<u32>,
    derived: impl FnOnce() -> WindowsBackend,
) -> WindowsBackend {
    match (pinned, selected_vendor_id) {
        (None, _) => derived(),
        (Some(pin), Some(vendor)) => match windows_backend_vendor_id(pin) {
            Some(required) if required != vendor => derived(),
            _ => pin,
        },
        (Some(pin), None) => pin,
    }
}

/// Explicit `PUNKTFUNK_ENCODER` pin. `None` for `auto`, unset, and unknown.
#[cfg(target_os = "windows")]
fn windows_pinned_backend() -> Option<WindowsBackend> {
    // Latched in HostConfig; do not re-read the env.
    match pf_host_config::config().encoder_pref.as_str() {
        "nvenc" | "hw" | "nvidia" | "cuda" => Some(WindowsBackend::Nvenc),
        "amf" | "amd" => Some(WindowsBackend::Amf),
        "qsv" | "intel" => Some(WindowsBackend::Qsv),
        "sw" | "software" | "openh264" => Some(WindowsBackend::Software),
        _ => None,
    }
}

/// Active Windows backend. `auto` → selected adapter's vendor; a contradicting
/// pin is overridden ([`resolve_windows_backend`]). Shared with GameStream.
#[cfg(target_os = "windows")]
pub fn windows_resolved_backend() -> WindowsBackend {
    let pinned = windows_pinned_backend();
    // Vendor query only to reconcile a pin — auto stays one inventory walk.
    let selected = if pinned.is_some() {
        pf_gpu::selected_gpu().map(|s| s.info.vendor_id)
    } else {
        None
    };
    resolve_windows_backend(pinned, selected, || match windows_gpu_vendor() {
        Some(GpuVendor::Nvidia) => WindowsBackend::Nvenc,
        Some(GpuVendor::Amd) => WindowsBackend::Amf,
        Some(GpuVendor::Intel) => WindowsBackend::Qsv,
        None => WindowsBackend::Software,
    })
}

/// GPU-resident frames (software is the only CPU path). Single source for
/// [`pf_frame::OutputFormat`]'s `gpu` bit — capture must not re-derive it.
#[cfg(target_os = "windows")]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(windows_resolved_backend(), WindowsBackend::Software)
}
#[cfg(target_os = "linux")]
pub fn resolved_backend_is_gpu() -> bool {
    linux_resolved_backend() != LinuxBackend::Software
}
/// No resolver on this target. `not(any(...))`, never a target list, so no
/// exotic target loses the fn.
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    )
}

/// Encoder half of the 4:4:4 capture gate: ingest RGB and CSC to 4:4:4.
/// Only Windows NVENC. Linux 4:4:4 is capture-side (portal RGB → `yuv444p`).
#[cfg(target_os = "windows")]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    windows_resolved_backend() == WindowsBackend::Nvenc
}
#[cfg(not(target_os = "windows"))]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    false
}

/// True if the Windows codec advertisement comes from a real GPU probe
/// ([`windows_codec_support`]) rather than the static superset. AMF always;
/// QSV with `qsv` or `amf-qsv`; NVENC with `nvenc`.
#[cfg(target_os = "windows")]
pub fn windows_backend_is_probed() -> bool {
    match windows_resolved_backend() {
        WindowsBackend::Amf => true,
        WindowsBackend::Qsv => cfg!(feature = "qsv") || cfg!(feature = "amf-qsv"),
        WindowsBackend::Nvenc => cfg!(feature = "nvenc"),
        WindowsBackend::Software => false,
    }
}

/// Encode-GPU vendor from the **selected** render adapter — the same one
/// capture and the IddCx pin sit on. Do not scan DXGI adapter 0: on hybrid
/// boxes that is often the iGPU while textures live on the dGPU. Uncached
/// (preference-dependent; session setup only). Unknown vendor → first known.
#[cfg(target_os = "windows")]
fn windows_gpu_vendor() -> Option<GpuVendor> {
    fn by_id(vendor_id: u32) -> Option<GpuVendor> {
        match vendor_id {
            pf_gpu::VENDOR_NVIDIA => Some(GpuVendor::Nvidia),
            pf_gpu::VENDOR_AMD => Some(GpuVendor::Amd),
            pf_gpu::VENDOR_INTEL => Some(GpuVendor::Intel),
            _ => None,
        }
    }
    let sel = pf_gpu::selected_gpu()?;
    by_id(sel.info.vendor_id)
        .or_else(|| pf_gpu::enumerate().iter().find_map(|g| by_id(g.vendor_id)))
}

/// Windows encode-codec probe, cached per (backend, selected GPU) so a
/// console preference change re-probes. Call only when
/// [`windows_backend_is_probed`]. AV1 and HEVC must be probed, not assumed.
///
/// AMD: native factory probe (the path the session opens). QSV: native VPL
/// Query, else libavcodec. NVIDIA: driver GUID list — never an ffmpeg open.
#[cfg(target_os = "windows")]
pub fn windows_codec_support() -> CodecSupport {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, CodecSupport>>> = OnceLock::new();
    let backend = windows_resolved_backend();
    let key = format!("{backend:?}:{}", pf_gpu::selection_key());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(c) = cache.lock().unwrap().get(&key) {
        return *c;
    }
    let probe_one = |codec: Codec| -> bool {
        match backend {
            WindowsBackend::Amf => amf::probe_can_encode(codec),
            WindowsBackend::Qsv => {
                // Libavcodec probe only on builds without native VPL.
                #[cfg(feature = "qsv")]
                {
                    qsv::probe_can_encode(codec)
                }
                #[cfg(all(not(feature = "qsv"), feature = "amf-qsv"))]
                {
                    ffmpeg_win::probe_can_encode(ffmpeg_win::WinVendor::Qsv, codec)
                }
                #[cfg(all(not(feature = "qsv"), not(feature = "amf-qsv")))]
                {
                    false
                }
            }
            // NVENC answers from one GUID-list session below. Software is never
            // probed. Defensive `false` → static-superset fallback.
            WindowsBackend::Nvenc | WindowsBackend::Software => false,
        }
    };
    let caps = match backend {
        // One throwaway session lists every GUID. Featureless builds fall
        // through to `probe_one`'s all-false (= static superset).
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => nvenc::probe_codec_support(),
        _ => CodecSupport {
            h264: probe_one(Codec::H264),
            h265: probe_one(Codec::H265),
            av1: probe_one(Codec::Av1),
        },
    };
    tracing::info!(
        ?backend,
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "Windows encode capabilities probed"
    );
    // Concurrent first calls may double-probe; last insert wins.
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// Whether one more encode session fits the hardware budget. Display admission
/// declines rather than silently degrading a live sibling. NVENC is the only
/// hard cap today. See `design/windows-parallel-virtual-displays.md`.
#[cfg(target_os = "windows")]
pub fn can_open_another_session() -> bool {
    #[cfg(feature = "nvenc")]
    {
        nvenc::can_open_another_session()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        true
    }
}

// `#[path]` keeps `crate::*` names flat. Native AMF is unconditional on
// Windows — `amfrt64.dll` at runtime, like NVENC. See `design/native-amf-encoder.md`.
#[cfg(target_os = "windows")]
#[path = "enc/windows/amf.rs"]
mod amf;
#[cfg(all(target_os = "windows", feature = "amf-qsv"))]
#[path = "enc/windows/ffmpeg_win.rs"]
mod ffmpeg_win;
// Native QSV (VPL): `qsv` feature, vendored dispatcher, GPU runtime from the
// driver store. See `design/native-qsv-encoder.md`.
#[cfg(target_os = "linux")]
#[path = "enc/linux/mod.rs"]
mod linux;
#[cfg(all(target_os = "windows", feature = "qsv"))]
#[path = "enc/windows/qsv.rs"]
mod qsv;
// Direct-SDK NVENC (CUDA). `.so` at runtime, so `--features nvenc` is safe
// on a driver-less/AMD box. See `design/linux-direct-nvenc.md`.
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "enc/windows/nvenc.rs"]
mod nvenc;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
#[path = "enc/linux/nvenc_cuda.rs"]
mod nvenc_cuda;
// `NVENCSTATUS` → cause for both direct-NVENC backends. Splits the two
// opposite failures the driver reports as the same `INVALID_VERSION`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
#[path = "enc/nvenc_status.rs"]
mod nvenc_status;
// Shared `nvEncodeAPI` glue (`NvStatusExt`/`nv_ok`, `codec_guid`). Sibling of `nvenc_status`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
#[path = "enc/nvenc_core.rs"]
mod nvenc_core;
// Slot-family RFI policy (taint sweep + pre-loss anchor) for AMF, QSV, and
// Vulkan Video. Mechanisms stay in each backend. Cfg is the union of callers
// (`amf` is featureless on Windows; `vulkan_video` needs `vulkan-encode`).
// Items stay live under that whole cfg: `plan_slot_recovery` calls `pick_anchor`.
#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", feature = "vulkan-encode")
))]
#[path = "enc/rfi.rs"]
mod rfi;
// Shared loss-recovery env knobs. Defaults and API clamps stay per-backend.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[path = "enc/policy.rs"]
mod policy;
// Shared libavcodec glue (`pixel_to_av`, swscale consts) for the three libav backends.
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "amf-qsv")))]
#[path = "enc/libav.rs"]
mod libav;
// Software (openh264) H.264 — GPU-less path on both Windows and Linux.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[path = "enc/sw.rs"]
mod sw;
#[cfg(target_os = "linux")]
#[path = "enc/linux/vaapi.rs"]
mod vaapi;
// Vulkan Video on Linux (AMD/Intel). App-owned DPB (real RFI); on-GPU RGB→NV12
// CSC. Needs `--features vulkan-encode`. See `design/linux-vulkan-video-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vulkan_video.rs"]
mod vulkan_video;
// Vendored `VK_KHR_video_encode_av1` — pinned `ash` predates 1.3.290. Do not
// bump `ash` (breaks the SDL/Vulkan client).
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_av1_encode.rs"]
mod vk_av1_encode;
// Vendored `VK_VALVE_video_encode_rgb_conversion`. Same ash-pin as `vk_av1_encode`.
// See `design/vulkan-rgb-direct-encode.md`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_valve_rgb.rs"]
mod vk_valve_rgb;
// Shared ash helpers (dmabuf import, image/memory) for the Linux Vulkan backends.
#[cfg(all(
    target_os = "linux",
    any(feature = "vulkan-encode", feature = "pyrowave")
))]
#[path = "enc/linux/vk_util.rs"]
mod vk_util;
// PyroWave: Vulkan-compute intra wavelet. Explicit `PUNKTFUNK_ENCODER=pyrowave`.
// See `design/pyrowave-codec-plan.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave.rs"]
mod pyrowave;
// `punktfunk-encode-worker` holds `CAP_SYS_NICE`; the host must not (a capped
// host is unidentifiable to KWin). `worker` is `pub` for that binary's `main`.
// See `design/gpu-priority-capability-worker.md`.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave_remote.rs"]
mod pyrowave_remote;
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/worker.rs"]
pub mod worker;
// Windows PyroWave: NV12 D3D11→Vulkan. Same module name as Linux (`#[path]`).
// See `design/pyrowave-windows-host-zerocopy.md`.
#[cfg(all(target_os = "windows", feature = "pyrowave"))]
#[path = "enc/windows/pyrowave.rs"]
mod pyrowave;
// Shared PyroWave AU wire-framing — both platform backends emit this layout.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
#[path = "enc/pyrowave_wire.rs"]
mod pyrowave_wire;

/// Whether a PyroWave mode fits the rate controller's packed 16-bit block
/// index: false ≈ 8K-class 4:4:4. Negotiator downgrades to 4:2:0; encoders refuse.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub fn pyrowave_mode_fits_rdo(width: u32, height: u32, chroma444: bool) -> bool {
    pyrowave_wire::block_count_32x32(width, height, chroma444) <= u16::MAX as u32
}
#[cfg(not(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave")))]
pub fn pyrowave_mode_fits_rdo(_width: u32, _height: u32, _chroma444: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin whose vendor contradicts the selected GPU is overridden — never
    /// "pin + proceed" (that feeds the reset ladder a deterministic failure).
    #[test]
    fn encoder_pin_reconciles_against_the_selected_adapter() {
        use WindowsBackend::*;
        let derived = |b: WindowsBackend| move || b;
        let unreachable = || -> WindowsBackend { panic!("derived must not be consulted") };
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_NVIDIA), derived(Nvenc)),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_INTEL), unreachable),
            Qsv
        );
        assert_eq!(
            resolve_windows_backend(Some(Nvenc), None, unreachable),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Software), Some(pf_gpu::VENDOR_NVIDIA), unreachable),
            Software
        );
        assert_eq!(
            resolve_windows_backend(None, Some(pf_gpu::VENDOR_AMD), derived(Amf)),
            Amf
        );
        assert_eq!(
            resolve_windows_backend(None, None, derived(Software)),
            Software
        );
    }

    /// [`TerminalEncoderError`] must stay downcastable through `context` layers.
    /// A `format!`/stringify on any layer would break the reset ladder.
    #[test]
    fn terminal_encoder_error_survives_the_context_chain() {
        use anyhow::Context as _;
        let site: anyhow::Error = anyhow::Error::new(TerminalEncoderError)
            .context("capture device's adapter is not an Intel VPL implementation");
        let bubbled = Err::<(), _>(site)
            .context("QSV lazy bring-up")
            .context("encoder submit")
            .unwrap_err();
        assert!(bubbled.downcast_ref::<TerminalEncoderError>().is_some());
        assert!(format!("{bubbled:#}").contains("not an Intel VPL implementation"));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn codec_support_wire_mask() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let all = CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        };
        assert_eq!(all.wire_mask(), Some(CODEC_H264 | CODEC_HEVC | CODEC_AV1));
        let hevc_only = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert_eq!(hevc_only.wire_mask(), Some(CODEC_HEVC));
        // All-false = GPU unusable, not "zero codecs" — `None` → static superset.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(none.wire_mask(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cursor_blend_capability_mirrors_the_dispatch() {
        use LinuxBackend::*;
        assert!(cursor_blend_capable_for(
            Some(Pyrowave),
            false,
            false,
            false
        ));
        assert!(cursor_blend_capable_for(Some(Nvenc), true, true, false));
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), false, true, false),
            "a CPU payload stays on libav NVENC, which cannot blend"
        );
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), true, false, false),
            "PUNKTFUNK_NVENC_DIRECT=0 (or a build without the feature) is the libav path"
        );
        assert!(cursor_blend_capable_for(Some(AmdIntel), false, false, true));
        assert!(
            !cursor_blend_capable_for(Some(AmdIntel), false, false, false),
            "no eligible Vulkan CSC arm (H.264, PUNKTFUNK_VULKAN_ENCODE=0, unsupported \
             device) resolves to libav VAAPI, which cannot blend"
        );
        assert!(cursor_blend_capable_for(Some(Vulkan), false, false, true));
        assert!(!cursor_blend_capable_for(Some(Software), false, true, true));
        assert!(!cursor_blend_capable_for(None, false, true, true));
    }

    /// Every `Encoder` method must be forwarded by `TrackedEncoder`. An
    /// unforwarded default silently no-ops — the host loop only holds the
    /// wrapper. Source-text parse: each item ends at the first column-0 `}`;
    /// method names sit on a line starting `fn `.
    #[test]
    fn tracked_encoder_forwards_every_trait_method() {
        fn item_block<'a>(src: &'a str, marker: &str) -> &'a str {
            let start = src
                .find(marker)
                .unwrap_or_else(|| panic!("marker {marker:?} not found — update this guard"));
            let body = &src[start..];
            let end = body
                .find("\n}")
                .unwrap_or_else(|| panic!("no column-0 close brace after {marker:?}"));
            &body[..end]
        }
        fn fn_names(block: &str) -> std::collections::BTreeSet<&str> {
            block
                .lines()
                .map(str::trim_start)
                .filter(|l| !l.starts_with("//"))
                .filter_map(|l| l.strip_prefix("fn "))
                .map(|rest| {
                    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .next()
                        .expect("split yields at least one item")
                })
                .collect()
        }
        // `find` takes the first occurrence: the real impl precedes this test's copy.
        let trait_fns = fn_names(item_block(
            include_str!("enc/codec.rs"),
            "pub trait Encoder: Send {",
        ));
        let impl_fns = fn_names(item_block(
            include_str!("lib.rs"),
            "impl Encoder for TrackedEncoder {",
        ));
        assert!(
            trait_fns.len() >= 12,
            "only {} trait methods parsed — the extraction markers have rotted, fix the parse \
             before trusting this guard",
            trait_fns.len()
        );
        let missing: Vec<_> = trait_fns.difference(&impl_fns).collect();
        assert!(
            missing.is_empty(),
            "Encoder methods NOT forwarded by TrackedEncoder: {missing:?} — the host loop only \
             ever holds the wrapped box, so an unforwarded default silently disables the feature \
             for every session. Forward each one in `impl Encoder for TrackedEncoder`."
        );
        // Reverse (impl fn absent from the trait) is a compile error; equality
        // guards a parse regression.
        assert_eq!(trait_fns, impl_fns);
    }

    /// Typed EINVAL must survive `with_context`. An eager `format!` between
    /// `open_with` and the ladder would stop the step-down.
    #[cfg(target_os = "linux")]
    #[test]
    fn nvenc_open_einval_survives_context_layers() {
        use ffmpeg_next as ffmpeg;
        let e = anyhow::Error::from(ffmpeg::Error::Other {
            errno: ffmpeg::util::error::EINVAL,
        })
        .context("open hevc_nvenc (3840x2160@120, 400000000 bps)")
        .context("outer");
        assert!(nvenc_open_einval(&e));
        // Other errno must not step the ladder.
        let e = anyhow::Error::from(ffmpeg::Error::Other {
            errno: ffmpeg::util::error::ENOSYS,
        })
        .context("open");
        assert!(!nvenc_open_einval(&e));
    }

    /// Untyped English "Invalid argument" must not classify.
    #[cfg(target_os = "linux")]
    #[test]
    fn nvenc_open_einval_ignores_untyped_text() {
        let e = anyhow::anyhow!("driver said: Invalid argument (not a typed libav errno)");
        assert!(!nvenc_open_einval(&e));
    }

    /// Resolver alias table. The panicking closure is the laziness contract:
    /// an explicit pref must not run the auto probe (`/serverinfo` polls).
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_backend_resolver_table() {
        use LinuxBackend::*;
        let no_probe = || -> bool { panic!("explicit prefs must not run the auto probe") };
        for pref in ["nvenc", "nvidia", "cuda"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Nvenc));
        }
        for pref in ["vaapi", "amd", "intel"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(AmdIntel));
        }
        for pref in ["vulkan", "vulkan-video"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Vulkan));
        }
        assert_eq!(
            resolve_linux_backend("pyrowave", no_probe, false),
            Some(Pyrowave)
        );
        for pref in ["software", "sw", "openh264"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Software));
        }
        assert_eq!(resolve_linux_backend("", || true, false), Some(AmdIntel));
        assert_eq!(resolve_linux_backend("auto", || false, false), Some(Nvenc));
        // CUDA is NVENC-only and short-circuits the probe (`||` order).
        assert_eq!(resolve_linux_backend("auto", no_probe, true), Some(Nvenc));
        // Unknown pref: dispatch bails; mirrors map this to auto.
        assert_eq!(resolve_linux_backend("banana", no_probe, false), None);
        // Explicit pref is never overridden by `cuda`.
        assert_eq!(
            resolve_linux_backend("vaapi", no_probe, true),
            Some(AmdIntel)
        );
    }

    /// Linux dispatch through the resolver, GPU-free via the software arm.
    /// Pref is injected — `set_var` races `getenv` in parallel tests.
    #[cfg(target_os = "linux")]
    #[test]
    fn open_video_backend_dispatches_software() {
        let (enc, label) = open_video_backend_linux(
            "software",
            Codec::H264,
            PixelFormat::Bgrx,
            64,
            64,
            30,
            1_000_000,
            false,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("software arm must open GPU-free");
        assert_eq!(label, "software");
        drop(enc);
        let err = match open_video_backend_linux(
            "software",
            Codec::H265,
            PixelFormat::Bgrx,
            64,
            64,
            30,
            1_000_000,
            false,
            8,
            ChromaFormat::Yuv420,
            false,
            4,
        ) {
            // `expect_err` needs `Ok: Debug`; `Box<dyn Encoder>` isn't.
            Ok(_) => panic!("software emits H.264 only; an H.265 session must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("H.264"), "{err:#}");
    }
}
