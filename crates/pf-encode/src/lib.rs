//! Hardware video encode (plan §7). Binds FFmpeg; never rewrites codecs. Low-latency preset,
//! B-frames off. The backend is per-GPU: NVENC on NVIDIA (`*_nvenc`, accepts `bgr0` and does
//! RGB→YUV on the GPU, so no host-side CSC) and VAAPI on AMD/Intel (`*_vaapi`; the CPU-input
//! fallback swscales RGB→NV12, the zero-copy path imports the capture dmabuf straight into a
//! VA surface). One [`Encoder`] trait, selected in [`open_video`]. Extracted into a subsystem crate
//! (plan §W6): depends on the shared frame vocabulary (`pf-frame`) + zero-copy plumbing
//! (`pf-zerocopy`), never on capture — the capture→encode edge is one-way.
// NOTE: no crate-wide `#![allow(dead_code)]`. It was inherited from the pre-extraction host crate
// root as scaffolding for backend paths defined ahead of the build that used them, but a census
// across every feature combination on both platforms found it was hiding exactly two items — so it
// bought nothing and blinded the crate to future rot. Genuinely test-only helpers carry
// `#[cfg(test)]` instead.
// Every unsafe block in this module tree carries a `// SAFETY:` proof; enforce it (unsafe-proof
// program). As a parent module this also covers the child modules (windows/linux backends).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;
use pf_frame::{CapturedFrame, PixelFormat};

#[path = "enc/codec.rs"]
mod codec;
pub use codec::*;

impl Codec {
    /// The `quic` codec bitfield the host can currently **emit** on the punktfunk/1 native path,
    /// given the resolved encode backend — the same GPU-aware advertisement GameStream builds for
    /// Moonlight (the host `gamestream::serverinfo`), in `quic::CODEC_*` bits. The GPU-less software
    /// encoder (openh264) produces H.264 only; the probed backends (Linux VAAPI, Windows AMF/QSV)
    /// advertise exactly what the GPU encodes ([`vaapi_codec_support`] / [`windows_codec_support`] —
    /// AV1 encode is narrow, an old iGPU might lack HEVC); NVENC keeps the Moonlight-validated
    /// static superset. An empty probe means the GPU wasn't usable at probe time (GPU-less CI,
    /// wrong-vendor pref), not that it encodes nothing — fall back to the superset so `resolve_codec`
    /// still lands on HEVC for an auto client, exactly the pre-probe behaviour. Fed to
    /// [`punktfunk_core::quic::resolve_codec`] against the client's advertised codecs.
    pub fn host_wire_caps() -> u8 {
        // PyroWave rides ON TOP of whatever H.26x set resolves below: feature-gated, Linux-only
        // for now (the Windows host encoder is future work), and inert in negotiation unless the
        // client explicitly prefers it (resolve_codec ignores the bit in its ladder). Advertised
        // whenever the backend could open: AMD/Intel capture hands raw dmabufs it imports
        // directly, and an NVIDIA-auto host's PyroWave sessions flip capture to CPU RGB
        // per-session instead (the host `session_plan::SessionPlan::output_format`) — the EGL→CUDA
        // frames the `auto` GPU path would deliver are NVENC-only. Only a software/GPU-less pref
        // keeps the bit off (no Vulkan device to open).
        // Resolved ONCE for every gate below (pyro bit, software pin, vulkan ceiling, the
        // zero-copy plane): this fn backs the codec advertisement on polled paths, and the
        // resolver's auto arm samples live GPU-preference state — four samples per call was the
        // probe-amplification the WP7.6 diff critic caught.
        #[cfg(target_os = "linux")]
        let backend = linux_resolved_backend();
        #[cfg(all(target_os = "linux", feature = "pyrowave"))]
        let pyro = if backend != LinuxBackend::Software {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0u8
        };
        // Windows: the wavelet encoder rides on top of whatever GPU backend the box has (NVENC/AMF/
        // QSV) — it opens its OWN Vulkan device by the render GPU's vendor/device-id and
        // zero-copy-imports the capturer's NV12 D3D11 texture, so the H.26x backend is irrelevant to
        // it. Only a software/GPU-less host keeps the bit off (no Vulkan GPU to open). Whether the
        // Session-0 external-memory import actually works is confirmed at encoder open
        // (`pyrowave_device_confirm_interop_support`); a failed open renegotiates to HEVC.
        #[cfg(all(target_os = "windows", feature = "pyrowave"))]
        let pyro = if windows_resolved_backend() != WindowsBackend::Software {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0u8
        };
        #[cfg(not(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave")))]
        let pyro = 0u8;
        let base = (|| {
            /// The static GPU superset (H.264 | HEVC | AV1) — mirrors the GameStream
            /// `SERVER_CODEC_MODE_SUPPORT` advertisement for the unprobed backends.
            const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
                | punktfunk_core::quic::CODEC_HEVC
                | punktfunk_core::quic::CODEC_AV1;
            #[cfg(target_os = "linux")]
            {
                if backend == LinuxBackend::Software {
                    return punktfunk_core::quic::CODEC_H264;
                }
                // A pref that FORCES the raw Vulkan Video backend can only ever serve what that
                // backend encodes: `open_video_backend`'s `vulkan` arm bails outright for anything
                // that is not HEVC/AV1 ("the Vulkan Video encoder supports HEVC + AV1; the session
                // negotiated {codec:?}"). Advertising H.264 there let a client negotiate it and die
                // at encoder open. Without the `vulkan-encode` feature that arm cannot open at all,
                // so the pref advertises nothing.
                //
                // This is a CEILING intersected with the device probe below, never a replacement
                // for it: pinning a static HEVC|AV1 would ADD AV1 on the AMD/Intel hosts whose probe
                // currently withholds it (pre-RDNA3, pre-Arc), i.e. it would re-create this very bug
                // for a different codec. Intersecting can only narrow.
                let pref_ceiling: u8 = match backend {
                    // Feature-blindness rider (WP7.6 critics): the resolver knows the PREF is
                    // vulkan; only this cfg! knows the BUILD can open it. Dropping the inner
                    // branch would advertise HEVC|AV1 on a featureless build — the
                    // advertise-then-die-at-open bug this ceiling exists to prevent.
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
                // NVENC (static superset, like GameStream) — or an empty VAAPI probe (see above).
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
                // NVENC (static superset, like GameStream) — or an empty AMF/QSV probe (see above).
                GPU_SUPERSET
            }
            // The macOS dev/test host has no GPU encode backend — keep the pre-probe advertisement.
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

/// Open a hardware video encoder for frames of the given `format` and mode, selecting the GPU
/// backend for this host: **NVENC** on NVIDIA (Linux/Windows), **VAAPI** on AMD/Intel (Linux).
/// When `cuda` is true the encoder takes GPU frames (`AV_PIX_FMT_CUDA`) from the NVIDIA zero-copy
/// path; otherwise it takes packed RGB/BGR CPU frames (and, on VAAPI, a future dmabuf payload).
/// `format`/`bitrate_bps`/`codec`/mode come from session negotiation; the caller derives `cuda`
/// from the first captured frame's payload. The Linux backend is auto-detected (override:
/// `PUNKTFUNK_ENCODER=auto|nvenc|vaapi`).
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
    // The session may hand this encoder cursor bitmaps to composite (cursor-as-metadata
    // captures). Backends whose fast path can't blend (Vulkan EFC RGB-direct) key off it.
    cursor_blend: bool,
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
    )?;
    // Record what this session encodes on (the mgmt API's "currently used GPU"): the backend label
    // is reported by `open_video_backend` from the branch that ACTUALLY opened — not re-derived by
    // mirroring its dispatch, which went stale the moment a backend gained an internal fallback
    // (the default-on Vulkan Video path falls back to VAAPI on a failed open, and a dispatch
    // mirror would report "vaapi" for every Vulkan session or vice versa). The GPU identity is the
    // same selection the capturer was created on ([`pf_gpu::selected_gpu`]). Dropping the
    // returned encoder ends the record, so the live count is correct by construction.
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
    // The session asked for a composited pointer; say so loudly if the backend that actually opened
    // cannot deliver one. `cursor_blend` was a REQUEST with no answer for most of this crate's life
    // (`let _ = cursor_blend;` below), and the result was a stream with no mouse cursor and nothing
    // in the logs — confirmed on the VAAPI dmabuf path and the libav-NVENC CUDA path.
    //
    // A warning is deliberately all this does. `open_video` cannot re-plan capture, so refusing here
    // would only trade a missing pointer for a dead session; the host owns `plan.cursor_blend` and is
    // the only layer that can fall back to capturer-side compositing. This makes the condition
    // visible and queryable (`EncoderCaps::blends_cursor`) so that decision can be made upstream.
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
    // Forwarded for the same reason as `set_wire_chunking` below — the unforwarded default
    // (`false` = "backend can't pipeline, stop asking") silently killed the §7 LN3 contention
    // escalation for every session, since the host loop only ever holds the wrapped box.
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.inner.set_pipelined(on)
    }
    // The classic TrackedEncoder trap: a defaulted trait method that isn't forwarded
    // silently no-ops through the wrapper (bit the direct-NVENC work, then THIS — the
    // §4.4 chunking probe run hit the default while the plan said Some(1408)).
    fn set_wire_chunking(&mut self, shard_payload: usize) {
        self.inner.set_wire_chunking(shard_payload)
    }
    // Forwarded for the same reason as `set_wire_chunking` above — an unforwarded default here
    // would silently leave the in-place backends pipelining past the capturer's ring.
    fn set_input_ring_depth(&mut self, depth: usize) {
        self.inner.set_input_ring_depth(depth)
    }
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.inner.poll()
    }
    // Both chunked-poll methods forwarded (the same trap class): the defaults would report
    // "not chunked" and wrap whole AUs, silently discarding the sub-frame overlap.
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
    // Forwarded (the same trap class as `set_wire_chunking`): the unforwarded default `None`
    // would tell the session loop "no encoder truth here" and it would keep pacing/acking the
    // requested rate even where NVENC clamped to the codec-level ceiling.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        self.inner.applied_bitrate_bps()
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

/// Ceiling applied to the negotiated bitrate before it reaches openh264: software H.264 realistically
/// caps far below the rates a hardware session negotiates, and handing it the full figure just
/// misconfigures its rate control. Module-scope so BOTH software arms share one value — the Linux
/// arm was missing the clamp the Windows arm applied.
#[cfg(any(target_os = "linux", target_os = "windows"))]
const SW_BITRATE_CEIL: u64 = 100_000_000;

/// The Linux half of [`open_video_backend`], with the pref INJECTED — the seam exists so the
/// dispatch is testable without mutating process env (`set_var` races `getenv` in parallel
/// tests) or fighting the once-latched `pf_host_config::config()`. Production takes the
/// config pref via the caller; shared validation (dims/fps/chroma degrade) stays there too.
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
) -> Result<(Box<dyn Encoder>, &'static str)> {
    // A NEGOTIATED PyroWave session (client advertised + preferred it, plan §3) routes
    // straight to that backend — the PUNKTFUNK_ENCODER pref below stays a lab override.
    if codec == Codec::PyroWave {
        #[cfg(feature = "pyrowave")]
        {
            return pyrowave::PyroWaveEncoder::open(width, height, fps, bitrate_bps, chroma)
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "pyrowave"));
        }
        #[cfg(not(feature = "pyrowave"))]
        anyhow::bail!(
            "session negotiated PyroWave but this host was built without --features \
             punktfunk-host/pyrowave (the advertisement bit should not have been set)"
        );
    }
    // Pick the GPU encode backend. NVIDIA → NVENC/CUDA (the original path, unchanged);
    // AMD/Intel → VAAPI (one libavcodec backend for both). Auto-detect by default so a single
    // Linux binary serves any GPU; `PUNKTFUNK_ENCODER` forces a specific backend (and surfaces
    // its errors crisply instead of silently trying the other).
    // AMD/Intel opener. Default = libav VAAPI. With `--features vulkan-encode` +
    // PUNKTFUNK_VULKAN_ENCODE, an HEVC session instead opens the raw Vulkan Video backend (real
    // RFI loss recovery the VAAPI path can't express); a failed open falls back to VAAPI so the
    // stream never dies over the new path. `format`/`bit_depth`/`chroma` only matter to VAAPI —
    // the Vulkan backend imports the dmabuf and does its own 8-bit 4:2:0 CSC.
    let open_amd_intel = || -> Result<(Box<dyn Encoder>, &'static str)> {
        // An HDR session (10-bit + a PQ/BT.2020 capture format) must skip the Vulkan Video
        // backend — it hardcodes an 8-bit 4:2:0 BT.709 CSC — and take the libav VAAPI path,
        // which has the P010/Main10/PQ wiring. SDR sessions keep the Vulkan default.
        #[cfg(feature = "vulkan-encode")]
        if matches!(codec, Codec::H265 | Codec::Av1)
            && vulkan_encode_enabled()
            && !(bit_depth == 10 && format.is_hdr_rgb10())
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
                // Native NV12 (PUNKTFUNK_PIPEWIRE_NV12 capture) has no VAAPI fallback:
                // libav's dmabuf lane would import the two-plane buffer as packed RGB
                // (silent garbage) and its CPU lane bails per frame — die crisply instead.
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
        // Same rule when the Vulkan backend was never eligible (H264 session,
        // PUNKTFUNK_VULKAN_ENCODE=0, or a build without the feature).
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
        )
        .map(|e| (e, "nvenc"))
    };
    // WP7.6: the dispatch consumes the SAME resolver the capability mirrors consult (the
    // Windows shape — `windows_resolved_backend` + `match backend` below) so the alias table
    // exists once. Arm bodies and their open-site labels are untouched.
    match resolve_linux_backend(pref, linux_auto_is_vaapi, cuda) {
        Some(LinuxBackend::Nvenc) => open_nvidia(),
        Some(LinuxBackend::AmdIntel) => open_amd_intel(),
        // Force the raw Vulkan Video HEVC backend (real RFI). Needs `--features vulkan-encode`.
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
        // PyroWave — the opt-in wired-LAN intra-only wavelet codec. Explicit-only, and
        // EXPERIMENTAL until CODEC_PYROWAVE negotiation lands (plan Phase 2): no shipping
        // client can decode the stream yet, so this arm exists for host-side bring-up and
        // latency work only. Vendor-agnostic (any Vulkan 1.3 GPU); ignores the negotiated
        // codec — every AU is an independently-decodable wavelet frame.
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
                // The lab override forces the wavelet stream onto a session negotiated for
                // another codec — that session's chroma may be HEVC-4:4:4, which the
                // pyrowave encoder doesn't do yet, so pin the override to 4:2:0.
                pyrowave::PyroWaveEncoder::open(
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    ChromaFormat::Yuv420,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "pyrowave"))
            }
            #[cfg(not(feature = "pyrowave"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=pyrowave requires a build with --features punktfunk-host/pyrowave"
                )
            }
        }
        // GPU-less software H.264 (openh264) — for a headless / GPU-lost box. Explicit-only:
        // `auto` never picks it (a box with `/dev/nvidiactl` present but a dead driver would
        // otherwise wrongly resolve to NVENC). Needs H.264 (openh264 emits only that) and a CPU
        // RGB frame, which the capturer delivers because the software backend resolves `gpu=false`.
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
        // The auto arm lives in the resolver now (a CUDA frame can only be consumed by
        // NVENC; else the shared manual-preference/NVIDIA-presence decision).
        None => anyhow::bail!(
            "unknown PUNKTFUNK_ENCODER={pref:?} — use auto (default), nvenc, vaapi, vulkan, pyrowave, or software"
        ),
    }
}

/// Open the platform encoder backend. Returns the encoder together with the display label of the
/// branch that ACTUALLY opened (`nvenc`/`vaapi`/`vulkan`/`amf`/`qsv`/`software`) — the label feeds
/// the mgmt API's live-session record, and only the open site knows which internal fallback won
/// (e.g. Vulkan Video falling back to VAAPI).
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
) -> Result<(Box<dyn Encoder>, &'static str)> {
    let _ = cursor_blend; // consumed only by the Linux vulkan-encode + direct-NVENC arms below
    validate_dimensions(codec, width, height)?;
    // Refresh/fps must be positive and sane: fps feeds the encoder time_base (`Rational(1, fps)`)
    // and the pts→ns conversion (`pts * 1e9 / fps`), so 0 builds a 1/0 rational / divides by zero.
    // The mid-stream Reconfigure path already guards `refresh_hz > 0`; enforcing it at this single
    // open chokepoint makes EVERY path (initial Hello, GameStream ANNOUNCE, Reconfigure) safe
    // regardless of which backend opens (security-review 2026-06-28 S5).
    if fps == 0 || fps > 1000 {
        anyhow::bail!("invalid refresh/fps {fps}: must be 1..=1000 Hz");
    }
    // 4:4:4 is HEVC- and PyroWave-only. The negotiator should never pass `Yuv444` for another
    // codec (it gates on the codec + `can_encode_444`), but defend the contract here so a future
    // caller can't silently emit a stream no decoder expects: an unsupported 4:4:4 request
    // degrades to 4:2:0 with a warning.
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
        )
    }
    #[cfg(target_os = "windows")]
    {
        // A NEGOTIATED PyroWave session (client advertised + preferred it) routes straight to the
        // NV12 zero-copy wavelet backend (design/pyrowave-windows-host-zerocopy.md) — placed FIRST,
        // like the Linux branch. It opens its own Vulkan device by the render GPU's vendor/device-id
        // and imports the capturer's shared NV12 texture; the H.26x backend selection below is moot.
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
                      // NVIDIA → NVENC (direct SDK), AMD → AMF, Intel → QSV (both libavcodec), else → software
                      // H.264. `auto` (the default) resolves from the selected render adapter's vendor.
        let backend = windows_resolved_backend();
        // With `auto` the backend is derived from the selected GPU, so this can only fire when an
        // explicit PUNKTFUNK_ENCODER contradicts the GPU the pipeline sits on (e.g. `nvenc` forced
        // while the web-console preference pins the Intel iGPU) — the open below will then fail on
        // a wrong-vendor device; say why up front instead of leaving an opaque encoder error.
        if let Some(sel) = pf_gpu::selected_gpu() {
            let mismatched = match backend {
                WindowsBackend::Nvenc => sel.info.vendor_id != pf_gpu::VENDOR_NVIDIA,
                WindowsBackend::Amf => sel.info.vendor_id != pf_gpu::VENDOR_AMD,
                WindowsBackend::Qsv => sel.info.vendor_id != pf_gpu::VENDOR_INTEL,
                WindowsBackend::Software => false,
            };
            if mismatched {
                tracing::warn!(
                    adapter = sel.info.name,
                    ?backend,
                    "encoder backend does not match the selected GPU's vendor (explicit \
                     PUNKTFUNK_ENCODER conflicting with the GPU preference?) — the encoder \
                     open will likely fail on this device"
                );
            }
        }
        match backend {
            WindowsBackend::Nvenc => {
                // Hardware path: NVENC over D3D11. The DXGI capturer switches to its zero-copy
                // FramePayload::D3d11 output under the same env var so capture + encode share textures.
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
                // AMD: the native AMF SDK encoder, unconditionally (design/native-amf-encoder.md
                // Phase 3). The libavcodec AMF fallback and the `PUNKTFUNK_AMF_FFMPEG` hatch were
                // removed once the native path was validated — two permanently-maintained AMF
                // paths double the driver-matrix burden, and the one kept "for safety" is exactly
                // the one with the wedge/latency pathology. No build feature: amfrt64.dll resolves
                // at runtime like NVENC's DLL. A missing/ancient runtime fails HERE with the
                // "install/update the AMD driver" message `AmfEncoder::open` raises (§6), rather
                // than silently degrading — FFmpeg now serves QSV only.
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
                // Intel: native VPL first (design/native-qsv-encoder.md — supersedes the
                // native-amf-encoder.md §2 "QSV stays on FFmpeg" decision). During bring-up the
                // ffmpeg path remains as an automatic open-failure fallback plus the explicit
                // `PUNKTFUNK_QSV_FFMPEG=1` escape hatch (the AMF §7 pattern); both go away in
                // Phase 4 after the field-silence gate.
                #[cfg(feature = "qsv")]
                {
                    // Trimmed like every sibling knob, so `"1 "` from a shell script or a `.env`
                    // line takes effect instead of silently doing nothing. Case-insensitivity is
                    // kept deliberately — this knob already accepted `TRUE` via
                    // `eq_ignore_ascii_case`, and the bare house `matches!` would have dropped
                    // that spelling; widening to `yes`/`on` only adds accepted values.
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
                // The libavcodec path: the pre-native default, the native open-failure fallback,
                // and the `PUNKTFUNK_QSV_FFMPEG` hatch (async_depth=1 + low_power VDEnc).
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
        );
        anyhow::bail!("video encode requires Linux or Windows")
    }
}

/// Open NVENC, probing this GPU's real max bitrate. NVENC rejects `avcodec_open2` with EINVAL
/// when the bitrate exceeds what any codec level can express, and that ceiling is
/// GPU/driver-specific (an RTX 4090 caps HEVC at ~800 Mbps; an RTX 5070 Ti accepts >1 Gbps). So
/// open at the requested rate first and step down ONLY if this GPU refuses it — each GPU then
/// runs at its own actual maximum, and a capable card is never clamped to a conservative guess.
/// The codec's theoretical level ceiling is just the first step-down candidate, not a blind cap.
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
) -> Result<Box<dyn Encoder>> {
    #[cfg(not(feature = "nvenc"))]
    let _ = cursor_blend; // consumed by the direct-SDK arm below
                          // Direct-SDK NVENC (design/linux-direct-nvenc.md): the DEFAULT on NVIDIA, and only for a CUDA
                          // capture payload (it registers CUDADEVICEPTR inputs — a CPU/dmabuf frame can't feed it, so those
                          // keep the libav path; and `cuda` is false on AMD/Intel, so they stay on VAAPI). Set
                          // PUNKTFUNK_NVENC_DIRECT=0 to fall back to libav. It self-clamps the bitrate internally (its own
                          // level-ceiling binary search at session open), so it skips the probe-loop stepping below.
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
        )?) as Box<dyn Encoder>);
    }
    // The silent-degrade trap: a build without `--features nvenc` compiles the direct-SDK
    // path OUT, and a CUDA session quietly loses real RFI + the no-IDR bitrate reconfigure
    // with nothing in the logs. This bit the Linux packagers once (fixed e89b2f60) and an
    // ad-hoc host deploy again on 2026-07-14 — say it loudly instead. (Skipped when the
    // operator explicitly chose libav via PUNKTFUNK_NVENC_DIRECT=0.)
    #[cfg(not(feature = "nvenc"))]
    if cuda
        && !std::env::var("PUNKTFUNK_NVENC_DIRECT")
            .map(|v| matches!(v.trim(), "0" | "false" | "no" | "off"))
            .unwrap_or(false)
    {
        // Once per process — featureless builds rebuild the encoder on every bitrate step,
        // and one line is enough to diagnose the build.
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
            // EINVAL = above this GPU's level ceiling → step down. Any other failure (no GPU,
            // bad mode, OOM) is real — surface it rather than masking it with bitrate retries.
            Err(e) if nvenc_open_einval(&e) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("encoder open failed at every probed bitrate")))
}

/// Whether a libav NVENC open failed with EINVAL — the "bitrate above this GPU's level ceiling"
/// signal [`open_nvenc_probed`]'s ladder steps down on. Typed: the root `ffmpeg::Error` survives
/// the `anyhow` context chain, so match it there instead of substring-matching the English
/// strerror rendering of the whole chain — which also fired on any OTHER wrapped EINVAL (e.g. a
/// CUDA-context errno) and steered the ladder on failures that have nothing to do with bitrate.
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

/// Whether the direct-SDK NVENC path is active. **Default ON** — on-glass validated 2026-07-12:
/// real RFI landed 73/73 as clean P-frame recovery anchors (never IDR) on an RTX host with a real
/// Steam Deck client (design/linux-direct-nvenc.md §9). `PUNKTFUNK_NVENC_DIRECT=0` (also `false`/
/// `no`/`off`) is the libav escape hatch. Only consulted for a CUDA capture payload on an NVIDIA
/// host — the `cuda` gate in `open_nvenc_probed` keeps AMD/Intel on VAAPI regardless — and only
/// with `--features nvenc`.
#[cfg(all(target_os = "linux", feature = "nvenc"))]
fn nvenc_direct_enabled() -> bool {
    std::env::var("PUNKTFUNK_NVENC_DIRECT")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

/// Whether the raw Vulkan Video HEVC encode backend is active for AMD/Intel. **Default ON** —
/// on-glass validated 2026-07-12 on an AMD RADV 780M with a real Deck-class client: the pipelined
/// encoder ran a rock-solid 1080p@240 HEVC session and healed loss with clean P-frame recovery
/// anchors (never IDR) via explicit DPB reference slots — real reference-frame invalidation the
/// libavcodec VAAPI path can't express (design/linux-vulkan-video-encode.md). `PUNKTFUNK_VULKAN_ENCODE=0`
/// (also `false`/`no`/`off`) is the libav-VAAPI escape hatch. Only consulted with
/// `--features vulkan-encode`, and a failed open falls back to VAAPI, so an unsupported device
/// (e.g. a Mesa without h265 encode, or an untested Intel/ANV where the path misbehaves at open)
/// degrades gracefully to the old backend rather than breaking the stream.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_enabled() -> bool {
    std::env::var("PUNKTFUNK_VULKAN_ENCODE")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

/// Whether THIS session's encoder can ingest a producer-native NV12 capture: only the raw
/// Vulkan Video backend does (libav VAAPI would misread the two-plane buffer as packed RGB —
/// [`open_video`] refuses the combination), so the session's codec must be one it encodes and
/// the backend must be eligible to open. The host facade threads the verdict into the capture
/// negotiation (`OutputFormat::nv12_native` → `ZeroCopyPolicy::native_nv12_session`), which
/// then PREFERS gamescope's producer-side NV12 pod (default-on; `PUNKTFUNK_PIPEWIRE_NV12=0`
/// escapes at the capture gate).
///
/// This verdict is load-bearing in a way the other capability helpers are not: once the producer
/// has been asked for two-plane NV12 there is **no fallback**. [`open_video`] deliberately makes a
/// failed Vulkan open FATAL for an NV12 capture rather than degrading to libav VAAPI, because VAAPI
/// would import that buffer as packed RGB and stream silent garbage. So a wrong `true` here does
/// not cost quality — it kills the session at its first frame. Hence the real device probe below,
/// and hence the conjuncts are ordered cheapest-first so it only runs when everything else already
/// said yes.
#[cfg(target_os = "linux")]
pub fn linux_native_nv12_ok(codec: Codec) -> bool {
    #[cfg(feature = "vulkan-encode")]
    {
        matches!(codec, Codec::H265 | Codec::Av1)
            && vulkan_encode_enabled()
            // Which backend this host actually resolves to. This used to be a denylist of the
            // EXPLICIT prefs that skip Vulkan Video ("nvenc"|"nvidia"|"cuda"|"pyrowave"), which
            // silently missed the one that matters: the DEFAULT `encoder_pref` is `""`, and `""`
            // resolves to `auto`, which on an NVIDIA box opens NVENC. So a stock NVIDIA host passed
            // this gate. `linux_zero_copy_is_vaapi` layers the pref on top of the same auto decision
            // `open_video` makes, which is exactly what the note on `linux_auto_is_vaapi` says a
            // capability probe must consult — and it is what the downstream consumer of this very
            // verdict already uses to pick its zero-copy path.
            && linux_zero_copy_is_vaapi()
            // …and only then ask the GPU. Ordered last on purpose: it opens a Vulkan instance.
            && vulkan_encode_available(codec)
    }
    #[cfg(not(feature = "vulkan-encode"))]
    {
        let _ = codec;
        false
    }
}

/// Can this GPU + driver actually open a Vulkan Video **encode** session for `codec`? Cached per
/// (selected GPU, codec) — the [`can_encode_10bit`] idiom, with the probe run outside the lock.
///
/// Only [`linux_native_nv12_ok`] consults this, and only for the no-fallback decision described
/// there. It is deliberately NOT wired into [`open_video`]'s dispatch: that path already degrades
/// to VAAPI on a failed open for every non-NV12 capture, so making it pay for a probe would buy
/// nothing. Prediction and truth stay separate — the probe answers "would it open", the open itself
/// reports what actually did.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
fn vulkan_encode_available(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), bool>>> = OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let verdict = vulkan_video::probe_encode_support(codec);
    let ok = verdict.is_ok();
    match &verdict {
        Ok(()) => tracing::info!(
            ?codec,
            "Vulkan Video encode probed OK — producer-native NV12 capture is eligible"
        ),
        Err(why) => tracing::info!(
            ?codec,
            why = *why,
            "Vulkan Video encode unavailable — keeping the packed-RGB capture negotiation \
             (the native-NV12 path has no VAAPI fallback)"
        ),
    }
    cache.lock().unwrap().insert(key, ok);
    ok
}

/// Cheap, side-effect-free NVIDIA-presence probe for the `auto` backend selector: the NVIDIA
/// kernel driver exposes these device nodes, AMD/Intel boxes have neither. Deliberately does NOT
/// create a CUDA context (that would allocate GPU state on every host that merely *might* be
/// NVIDIA). `PUNKTFUNK_ENCODER` overrides this entirely.
#[cfg(target_os = "linux")]
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// The `auto` Linux backend decision, shared by [`open_video`] and [`linux_zero_copy_is_vaapi`]:
/// a manual web-console GPU preference (when that GPU is present — [`pf_gpu::manual_selection`])
/// picks its vendor's backend — AMD/Intel → VAAPI on that GPU's render node, NVIDIA → NVENC (still
/// requiring the proprietary driver's device nodes; a nouveau NVIDIA GPU can't NVENC) — otherwise
/// today's NVIDIA-presence probe, unchanged.
///
/// ⚠ This resolves the **`auto` case only** — it deliberately ignores `encoder_pref`. It is NOT a
/// mirror of [`open_video`]'s dispatch and must not be used to decide which backend a capability
/// probe should ask: use [`linux_zero_copy_is_vaapi`], which layers `encoder_pref` on top of this.
/// (`can_encode_10bit` used this directly and answered for the wrong backend whenever a host
/// forced one.)
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

/// The resolved Linux encode backend — the Linux twin of [`WindowsBackend`] (WP7.6). ONE alias
/// table, consumed by BOTH [`open_video_backend`]'s dispatch and every capability/advertisement
/// mirror below, so the two can never drift again (the audit counted five partial hand-copies of
/// this decision). Labels still come from the open sites (the mgmt-record convention) — this
/// resolves which ARM runs, never what actually opened.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxBackend {
    Nvenc,
    AmdIntel,
    Vulkan,
    Pyrowave,
    Software,
}

/// The pure core. `None` = the pref names no backend — [`open_video_backend`] BAILS on that,
/// while the capability mirrors historically resolved it as auto (via
/// [`linux_resolved_backend`]); that divergence is recorded, not accidental. `auto_is_vaapi` is
/// deliberately LAZY: explicit prefs must stay zero-probe — `/serverinfo` polls through the
/// mirrors, and `gamestream/serverinfo.rs` already litigated exactly that probe cost.
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
        // A CUDA frame can ONLY be consumed by NVENC. Otherwise the shared auto decision
        // (manual web-console GPU preference, else the NVIDIA-presence probe) picks the backend —
        // see `linux_auto_is_vaapi`.
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

/// Config-reading wrapper for the capability/advertisement mirrors: an unknown pref resolves as
/// auto — exactly what every mirror's old `_` arm did. `cuda = false`: the mirrors answer
/// pre-session questions (a CUDA payload exists only inside a live NVIDIA session).
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

/// The dmabuf modifiers the PyroWave encoder's Vulkan device imports for the capture's
/// packed-RGB fourcc — advertised by the capture when the pyrowave passthrough is active
/// (the VAAPI LINEAR-only policy starves it on Mutter+NVIDIA, which allocates tiled only).
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
pub fn pyrowave_capture_modifiers(fourcc: u32) -> Vec<u64> {
    pyrowave::capture_modifiers(fourcc)
}

/// True if the Linux GPU encode backend resolves to VAAPI (AMD/Intel) rather than NVENC — mirrors
/// [`open_video`]'s dispatch so the capturer can choose the matching zero-copy path (raw dmabuf
/// passthrough for VAAPI vs the EGL→CUDA import for NVENC).
#[cfg(target_os = "linux")]
pub fn linux_zero_copy_is_vaapi() -> bool {
    linux_zero_copy_is_vaapi_for(linux_resolved_backend())
}
/// The zero-copy-plane decision for an ALREADY-resolved backend — so a caller that has just
/// resolved (e.g. `host_wire_caps`, which consults several gates per call on a POLLED endpoint)
/// pays for the resolution once instead of once per gate (the `serverinfo.rs` probe-cost class,
/// caught by the WP7.6 diff critic).
#[cfg(target_os = "linux")]
fn linux_zero_copy_is_vaapi_for(backend: LinuxBackend) -> bool {
    match backend {
        LinuxBackend::Nvenc => false,
        LinuxBackend::AmdIntel => true,
        // PyroWave ingests the raw capture dmabuf itself (Vulkan import + compute CSC) on ANY
        // vendor — it must get the passthrough payload, never the EGL→CUDA import.
        LinuxBackend::Pyrowave => true,
        // EXACTLY the old `_` fallthrough for these two prefs — preserved, not endorsed. The
        // vulkan-on-NVIDIA half is FILED (EGL→CUDA capture negotiated for a dmabuf-importing
        // backend; reachable only via the explicit lab pref); the software half is latent (a
        // software host pins H.264, so the GPU-backend probes this steers never negotiate).
        LinuxBackend::Vulkan | LinuxBackend::Software => linux_auto_is_vaapi(),
    }
}

/// Which codecs the active GPU can actually ENCODE. Used to build the GameStream codec
/// advertisement so a client never negotiates a codec the GPU can't do (AV1 encode is narrow —
/// Intel Arc/Xe2+, AMD RDNA3+/RDNA4 — so it must be probed, not assumed).
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[derive(Clone, Copy, Debug)]
pub struct CodecSupport {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl CodecSupport {
    /// The probed codecs as a `quic::CODEC_*` bitfield, or `None` when the probe found nothing —
    /// meaning the GPU wasn't usable at probe time (GPU-less CI, a wrong-vendor pref), NOT that it
    /// encodes zero codecs; the caller then falls back to the static superset (the native-path
    /// analogue of `gamestream::serverinfo::probed_mask`).
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

/// Probe the active Linux GPU backend for its encodable codecs (cached; opens a tiny encoder per
/// codec, once). Only the VAAPI (AMD/Intel) backend is probed — NVENC keeps its Moonlight-validated
/// static advertisement (callers gate on [`linux_zero_copy_is_vaapi`]).
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

/// Whether the active GPU encode backend can actually produce a full-chroma **4:4:4** HEVC stream.
/// Resolved (and cached, once) *before* the Welcome so the host advertises the chroma it will really
/// encode — the honest-downgrade channel. 4:4:4 is HEVC-only; the probe opens a tiny encoder on the
/// active backend (NVENC FREXT is broad on NVIDIA, but VAAPI / AMF / QSV 4:4:4 is hardware-specific,
/// so it must be probed, never assumed). Non-HEVC codecs are always `false`.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_444(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if codec == Codec::PyroWave {
        // PyroWave does its own RGB→YCbCr CSC (capture always hands it a full-chroma source),
        // so 4:4:4 needs no GPU encode probe — only the full-res-chroma CSC variant:
        // `rgb2yuv444.comp` on Linux (Phase 2) and the mode-aware `BgraToYuvPlanes` on
        // Windows (Phase 3) — both landed (design/pyrowave-444-hdr.md).
        return true;
    }
    if codec != Codec::H265 {
        return false;
    }
    // Cached per selected GPU (was a process-lifetime OnceLock): a web-console preference change
    // re-probes on the newly selected adapter before the next Welcome.
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let key = pf_gpu::selection_key();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            // Mirror open_video's backend dispatch: VAAPI (AMD/Intel) vs NVENC (NVIDIA).
            if linux_zero_copy_is_vaapi() {
                vaapi::probe_can_encode_444(codec)
            } else {
                linux::probe_can_encode_444(codec)
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
                // AMD: native AMF never encodes 4:4:4 — VCN hardware limit, permanent, no probe
                // needed (design/native-amf-encoder.md §3.5, Phase 3).
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

/// Non-Linux/Windows (the macOS dev/test build of the host — synthetic-source loopback only):
/// no GPU encode backend exists here, so 4:4:4 is never advertised.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_444(_codec: Codec) -> bool {
    false
}

/// Whether the active GPU encode backend can actually produce a **10-bit** stream for `codec`
/// (HEVC Main10 / AV1 10-bit). Resolved (and cached per selected GPU) *before* the Welcome so the
/// negotiated bit depth — and the HDR/SDR colour label derived from it — matches what the encoder
/// will really emit: the honest-downgrade channel, exactly like [`can_encode_444`]. Without this
/// gate a default-on `PUNKTFUNK_10BIT` would negotiate 10-bit on a GPU/backend that then silently
/// falls back to 8-bit post-Welcome (label HDR / stream SDR).
///
/// Backend truth: Windows **NVENC** queries the per-codec `NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` cap;
/// native **AMF** `Init`s a tiny P010 encoder with the 10-bit profile props (the driver rejects
/// what the VCN can't do). **QSV** stays `false` until validated on Intel glass — the libavcodec
/// Main10 incantation can silently encode 8-bit, the same stance as its 4:4:4 probe. **Linux**
/// probes a tiny real Main10 open on the auto-resolved backend — libav NVENC (the HDR X2RGB10→
/// P010 swscale path) or VAAPI (P010 pool + Main10) — for the GNOME 50+ HDR portal capture;
/// the direct-SDK CUDA path and Vulkan-video stay 8-bit and a 10-bit session routes around them.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub fn can_encode_10bit(codec: Codec) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if !codec.supports_10bit() {
        return false;
    }
    if codec == Codec::PyroWave {
        // PyroWave needs no GPU encode probe (the wavelet is depth-agnostic) — only the HDR
        // capture CSC (scRGB FP16 → 16-bit studio-code planes), which exists on the Windows
        // IDD-push path only (design/pyrowave-444-hdr.md Phase 3; Linux capture has no HDR).
        return cfg!(target_os = "windows");
    }
    // Cached per (selected GPU, codec) — a web-console preference change re-probes on the newly
    // selected adapter before the next Welcome, mirroring `can_encode_444`.
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), bool>>> = OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().unwrap().get(&key) {
        return *v;
    }
    let supported = {
        #[cfg(target_os = "linux")]
        {
            // NVENC (libav, the HDR P010 swscale path) or VAAPI (P010 upload / dmabuf graph),
            // probed by opening a tiny real Main10 encoder — the same honesty contract as
            // `can_encode_444`. Vulkan-video and the direct-SDK CUDA path stay 8-bit; a 10-bit
            // session routes around them (see `open_video_backend`). NOTE: encode capability is
            // only half the Linux gate — the capture side (GNOME 50+ portal monitor in HDR mode)
            // is resolved separately by the host (`capturer_supports_hdr` / the GameStream RTSP
            // honor), since this probe can't know what the compositor will negotiate.
            // Resolve through the SAME helper `can_encode_444` uses (and which mirrors
            // `open_video`'s dispatch): `linux_auto_is_vaapi` ignores `encoder_pref`, so on a box
            // that forces a backend — e.g. `encoder_pref = "vaapi"` on an NVIDIA host — this probe
            // would answer for NVENC while the session actually opens VAAPI, and the negotiated bit
            // depth (plus the HDR/SDR colour label derived from it) would describe a backend that
            // never runs. That is exactly the dishonesty this probe exists to prevent.
            if linux_zero_copy_is_vaapi() {
                vaapi::probe_can_encode_10bit(codec)
            } else {
                linux::probe_can_encode_10bit(codec)
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
                // QSV: the native VPL probe (`MFXVideoENCODE_Query` with the Main10/P010 or
                // 10-bit-AV1 parameter block) — the driver clears/errors what the silicon can't
                // do. The ffmpeg path stays unprobed (its Main10 incantation can silently
                // encode 8-bit), so without the `qsv` feature this remains an honest `false`.
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

/// Non-Linux/Windows (the macOS dev/test build of the host — synthetic-source loopback only):
/// no GPU encode backend exists here, so 10-bit is never negotiated.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn can_encode_10bit(_codec: Codec) -> bool {
    false
}

// ---------------------------------------------------------------------------------------------
// Windows backend selection (the analogue of the Linux nvidia_present / linux_zero_copy_is_vaapi
// logic). NVIDIA → NVENC, AMD → AMF, Intel → QSV; `auto` (default) reads the vendor of the
// SELECTED render adapter (pf_gpu — web-console preference / env pin / max VRAM), so the
// backend always matches the GPU the capture ring and virtual display sit on.
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
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

/// Resolve the active Windows encode backend from `PUNKTFUNK_ENCODER` (`auto` → the selected
/// render adapter's vendor). Shared by [`open_video`] and the GameStream codec advertisement so
/// both agree.
#[cfg(target_os = "windows")]
pub fn windows_resolved_backend() -> WindowsBackend {
    // Resolved ONCE in HostConfig (Goal-1) — was re-read from PUNKTFUNK_ENCODER on every call.
    match pf_host_config::config().encoder_pref.as_str() {
        "nvenc" | "hw" | "nvidia" | "cuda" => WindowsBackend::Nvenc,
        "amf" | "amd" => WindowsBackend::Amf,
        "qsv" | "intel" => WindowsBackend::Qsv,
        "sw" | "software" | "openh264" => WindowsBackend::Software,
        _ => match windows_gpu_vendor() {
            Some(GpuVendor::Nvidia) => WindowsBackend::Nvenc,
            Some(GpuVendor::Amd) => WindowsBackend::Amf,
            Some(GpuVendor::Intel) => WindowsBackend::Qsv,
            None => WindowsBackend::Software,
        },
    }
}

/// True if the session's resolved encode backend produces GPU-resident frames (so the capturer should
/// hand GPU surfaces straight through rather than CPU-stage them) — only the GPU-less software encoder
/// wants CPU staging. This is the single source for [`pf_frame::OutputFormat`]'s `gpu` bit:
/// resolving it in `encode` and threading it *into* the capturer (rather than having `capture` re-derive
/// the backend) keeps the capture→encode dependency one-way, so the two can never disagree on whether
/// frames are GPU-resident (plan §2.4 / §W4).
#[cfg(target_os = "windows")]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(windows_resolved_backend(), WindowsBackend::Software)
}
/// Linux: every backend but the GPU-less software encoder (openh264) is GPU-resident. Resolver-
/// backed (WP7.6); cross-crate mirrors still exist in `session_plan::resolve_encoder` and
/// `gamestream/serverinfo.rs` — out of this crate's reach, listed in the WP7.6 design doc.
#[cfg(target_os = "linux")]
pub fn resolved_backend_is_gpu() -> bool {
    linux_resolved_backend() != LinuxBackend::Software
}
/// Other non-Windows targets (the macOS dev host): keep the string gate — no resolver exists
/// there. `not(any(...))`, NEVER a target list, so no exotic target loses the fn (the
/// `can_encode_444`/`can_encode_10bit` shape).
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
pub fn resolved_backend_is_gpu() -> bool {
    !matches!(
        pf_host_config::config().encoder_pref.as_str(),
        "software" | "sw" | "openh264"
    )
}

/// True if the resolved encode backend can ingest a full-chroma (RGB) source and CSC it to 4:4:4 itself —
/// the *encoder* half of the 4:4:4 capture gate (the host capture `capturer_supports_444`). Only Windows
/// direct-NVENC does (measured on-glass: ARGB + `chromaFormatIDC=3` → true 4:4:4); AMF/QSV can't. On Linux
/// the 4:4:4 source is the capturer's own (portal RGB → `yuv444p`), independent of the auto-detected
/// backend, so the gate never consults this there.
#[cfg(target_os = "windows")]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    windows_resolved_backend() == WindowsBackend::Nvenc
}
#[cfg(not(target_os = "windows"))]
pub fn resolved_backend_ingests_rgb_444() -> bool {
    false
}

/// True if the active Windows backend's codec advertisement comes from a **real GPU probe**
/// ([`windows_codec_support`]) rather than the NVENC static superset. AMF always qualifies — the
/// native factory probe (`amf::probe_can_encode`) needs no build feature — while QSV qualifies
/// with either the native VPL build (`qsv`, the authoritative Query probe) or the `amf-qsv`
/// (libavcodec) build. Formerly `windows_backend_is_ffmpeg`, renamed when the
/// native AMF probe replaced the ffmpeg open-probe (design/native-amf-encoder.md §4, Phase 2).
#[cfg(target_os = "windows")]
pub fn windows_backend_is_probed() -> bool {
    match windows_resolved_backend() {
        WindowsBackend::Amf => true,
        WindowsBackend::Qsv => cfg!(feature = "qsv") || cfg!(feature = "amf-qsv"),
        WindowsBackend::Nvenc | WindowsBackend::Software => false,
    }
}

/// Detect the encode-GPU vendor from the **selected render adapter** ([`pf_gpu::selected_gpu`]:
/// web-console preference > `PUNKTFUNK_RENDER_ADAPTER` > max VRAM) — the same adapter the capture
/// ring and the IddCx render pin sit on, so the encoder backend can never disagree with where the
/// captured frames live. The old first-DXGI-adapter scan did exactly that on hybrid boxes: adapter
/// 0 is often the iGPU (e.g. Intel Arc) while capture/encode pin the dGPU — resolving QSV for a
/// pipeline whose textures sit on the NVIDIA card. Uncached: selection is preference-dependent and
/// only consulted at session setup / serverinfo time, never per-frame. Falls back to the first
/// known-vendor adapter when the selected one is an unknown vendor.
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

/// Probe the active Windows AMF/QSV backend for its encodable codecs (cached **per (backend,
/// selected GPU)** — a web-console preference change re-probes on the newly selected adapter
/// instead of serving the old GPU's answer for the process lifetime). Mirrors
/// [`vaapi_codec_support`]; called only when [`windows_backend_is_probed`] is true. AV1 is narrow
/// (AMD RDNA3+, Intel Arc/Xe2+), so it must be probed, not assumed.
///
/// Mirrors the session dispatch (design/native-amf-encoder.md Phase 3): **AMD advertises from the
/// native AMF factory probe alone** (`amf::probe_can_encode`, on the selected adapter — the same
/// path the session opens, so the advertisement can never claim a codec the session can't emit);
/// **Intel/QSV advertises from the native VPL Query probe** (`qsv::probe_can_encode`,
/// design/native-qsv-encoder.md §4), falling back to the libavcodec probe on builds without the
/// `qsv` feature (all-`false` without either feature, matching a build that cannot open QSV).
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
            // AMD: the native factory probe is authoritative — it opens exactly the component the
            // session will, so the advertisement matches what the encoder can emit by construction.
            WindowsBackend::Amf => amf::probe_can_encode(codec),
            WindowsBackend::Qsv => {
                // Native VPL Query probe first (the same parameter block the session opens, so
                // the advertisement matches what the encoder can emit by construction); the
                // libavcodec probe only answers on builds without the native backend.
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
            // Callers gate on `windows_backend_is_probed` — defensively answer "nothing probed"
            // (the advertisement then falls back to the static superset).
            WindowsBackend::Nvenc | WindowsBackend::Software => false,
        }
    };
    let caps = CodecSupport {
        h264: probe_one(Codec::H264),
        h265: probe_one(Codec::H265),
        av1: probe_one(Codec::Av1),
    };
    tracing::info!(
        ?backend,
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "Windows AMF/QSV encode capabilities probed"
    );
    // A concurrent first call may double-probe; both arrive at the same answer, last insert wins.
    cache.lock().unwrap().insert(key, caps);
    caps
}

/// Stage-W3 encoder session-budget seam (`design/windows-parallel-virtual-displays.md` §4.5):
/// whether one more encode session fits the hardware budget — consulted by the display admission
/// before admitting a parallel display, so an unaffordable display is DECLINED instead of silently
/// degrading a live sibling's encode. NVENC is the only backend with hard session caps today
/// (GeForce consumer limit); AMF/QSV equivalents follow the same seam when they grow accounting.
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

// Goal-1 stage 6: GPU/CPU encoders confined to `encode/windows/` (NVENC, native AMF, AMF/QSV
// ffmpeg, software) and `encode/linux/` (NVENC/CUDA + VAAPI); `#[path]` keeps the
// `crate::*` module names flat.
// Native AMF (direct SDK, design/native-amf-encoder.md): compiled unconditionally on Windows —
// no build feature, the driver-installed amfrt64.dll resolves at runtime like NVENC's DLL.
#[cfg(target_os = "windows")]
#[path = "enc/windows/amf.rs"]
mod amf;
#[cfg(all(target_os = "windows", feature = "amf-qsv"))]
#[path = "enc/windows/ffmpeg_win.rs"]
mod ffmpeg_win;
// Native Intel QSV (VPL, design/native-qsv-encoder.md): behind the `qsv` feature — the vendored
// dispatcher is statically linked (libvpl-sys, cmake+bindgen at build), the GPU runtime resolves
// from the Intel driver store at run time.
#[cfg(target_os = "linux")]
#[path = "enc/linux/mod.rs"]
mod linux;
#[cfg(all(target_os = "windows", feature = "qsv"))]
#[path = "enc/windows/qsv.rs"]
mod qsv;
// Direct-SDK NVENC on Linux (CUDA input; design/linux-direct-nvenc.md) — real RFI + recovery anchor
// + reset() lever the libavcodec `linux::NvencEncoder` can't express. Opt-in behind
// `PUNKTFUNK_NVENC_DIRECT` until on-glass validated; the `.so` resolves at runtime like the Windows
// path, so `--features nvenc` stays safe on a driver-less/AMD Linux box.
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "enc/windows/nvenc.rs"]
mod nvenc;
#[cfg(all(target_os = "linux", feature = "nvenc"))]
#[path = "enc/linux/nvenc_cuda.rs"]
mod nvenc_cuda;
// Actionable `NVENCSTATUS` → cause mapping shared by both direct-NVENC backends, so a failed
// session open names its real cause instead of the old misleading "(no NVIDIA GPU?)" — including
// splitting the two opposite failures the driver reports as the SAME `INVALID_VERSION` (a genuine
// driver skew vs. this process's driver state going bad after a session already opened).
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
#[path = "enc/nvenc_status.rs"]
mod nvenc_status;
// Platform-agnostic direct-SDK NVENC glue (`NvStatusExt`/`nv_ok`, `codec_guid`) shared by both
// `nvEncodeAPI` backends — the byte-identical Tier-2 leaves (plan §2.2). Sibling of `nvenc_status`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
#[path = "enc/nvenc_core.rs"]
mod nvenc_core;
// The slot-family RFI recovery policy (WP7.2) shared by native AMF, native QSV and Vulkan Video —
// the taint sweep + pre-loss anchor pick extracted from three hand-copies that had already
// diverged once (the fecbec2d sweep predates the Vulkan backend's carve-out and was never ported
// to it until a later fix). Policy only: every mechanism (LTR bitfield, RefListCtrl, DPB slot
// table) stays in its backend. cfg = the union of the callers': amf.rs is featureless on Windows,
// vulkan_video needs `vulkan-encode` on Linux — and each ITEM inside is live under the module's
// whole cfg because `plan_slot_recovery` delegates to `pick_anchor` (dead_code is an item lint;
// module-granular reasoning is how a Windows-dead helper reaches main).
#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", feature = "vulkan-encode")
))]
#[path = "enc/rfi.rs"]
mod rfi;
// Shared libavcodec glue (`pixel_to_av`, swscale consts) for the three libav backends — Linux
// NVENC + VAAPI and Windows AMF/QSV — so the byte-identical pieces live once (plan §2.2, Tier 2).
#[cfg(any(target_os = "linux", all(target_os = "windows", feature = "amf-qsv")))]
#[path = "enc/libav.rs"]
mod libav;
// Software (openh264) H.264 encoder — the GPU-less path on BOTH Windows and Linux (a headless /
// GPU-less test box, or a fallback when no hardware encoder is available). Platform-agnostic: it
// consumes CPU RGB `CapturedFrame`s and the statically-bundled openh264 build.
#[cfg(any(target_os = "windows", target_os = "linux"))]
#[path = "enc/sw.rs"]
mod sw;
#[cfg(target_os = "linux")]
#[path = "enc/linux/vaapi.rs"]
mod vaapi;
// Raw Vulkan Video HEVC encode on Linux (AMD/Intel; design/linux-vulkan-video-encode.md) — real RFI
// via explicit DPB reference slots (the app owns the DPB), the open-stack twin of the direct-NVENC
// path. Does an on-GPU RGB→NV12 compute CSC since capture delivers packed-RGB dmabufs. Opt-in behind
// `PUNKTFUNK_VULKAN_ENCODE` until on-glass validated; needs `--features vulkan-encode`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vulkan_video.rs"]
mod vulkan_video;
// Vendored `VK_KHR_video_encode_av1` bindings (host-only) — the AV1 encode structs our pinned
// `ash 0.38.0+1.3.281` predates (finalized Vulkan 1.3.290). Copied verbatim from ash-master's
// generated code rather than bumping `ash` (which breaks the SDL/Vulkan client). Consumed by
// `vulkan_video.rs` via `super::vk_av1_encode`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_av1_encode.rs"]
mod vk_av1_encode;
// Vendored `VK_VALVE_video_encode_rgb_conversion` bindings (host-only) — RGB encode source with
// the VCN EFC front-end doing the CSC (design/vulkan-rgb-direct-encode.md). ash 0.38 predates
// the extension; same vendoring rationale as `vk_av1_encode`.
#[cfg(all(target_os = "linux", feature = "vulkan-encode"))]
#[path = "enc/linux/vk_valve_rgb.rs"]
mod vk_valve_rgb;
// Small ash leaf helpers shared by the Linux Vulkan encode backends (dmabuf import, image/memory
// utilities) — extracted from `vulkan_video.rs` when the PyroWave backend arrived.
#[cfg(all(
    target_os = "linux",
    any(feature = "vulkan-encode", feature = "pyrowave")
))]
#[path = "enc/linux/vk_util.rs"]
mod vk_util;
// PyroWave — the opt-in wired-LAN intra-only wavelet codec (design/pyrowave-codec-plan.md §4.3):
// pure Vulkan compute via the vendored `pyrowave-sys`, sub-ms encode, every frame a keyframe.
// Explicit-only behind PUNKTFUNK_ENCODER=pyrowave; EXPERIMENTAL until CODEC_PYROWAVE lands.
#[cfg(all(target_os = "linux", feature = "pyrowave"))]
#[path = "enc/linux/pyrowave.rs"]
mod pyrowave;
// The Windows PyroWave encoder — NV12 zero-copy D3D11→Vulkan via pyrowave's own compat device
// (design/pyrowave-windows-host-zerocopy.md). Same module name as the Linux one (per-platform
// `#[path]`, mutually-exclusive cfg) so `crate::pyrowave::*` is flat on both.
#[cfg(all(target_os = "windows", feature = "pyrowave"))]
#[path = "enc/windows/pyrowave.rs"]
mod pyrowave;
// Shared PyroWave AU wire-framing (§4.4) — the single source of truth both platform backends emit,
// so the on-wire access-unit layout the clients parse can never drift between Linux and Windows.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
#[path = "enc/pyrowave_wire.rs"]
mod pyrowave_wire;

/// Whether a PyroWave mode fits the vendored rate controller's packed 16-bit block index
/// (`patches/0002-rdo-saving-clamp.patch` note): false ≈ 8K-class 4:4:4. The negotiator
/// downgrades such a session to 4:2:0 before the Welcome; the encoders also refuse outright.
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

    /// The probed-capability → wire-bitfield mapping the native codec advertisement is built from.
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
        // An all-false probe means "GPU unusable at probe time", not "zero codecs" — `None` tells
        // the caller to advertise the static superset instead of refusing every handshake.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(none.wire_mask(), None);
    }

    /// WP7.7 guard (the cheap half): every `Encoder` trait method must be explicitly forwarded by
    /// `TrackedEncoder`. A defaulted trait method that isn't forwarded silently no-ops through the
    /// wrapper — the trap has bitten three times (`set_wire_chunking`'s §4.4 chunking probe,
    /// `set_pipelined`'s LN3 escalation, `applied_bitrate_bps`'s ABR truth), and every Phase 7
    /// consolidation that adds a trait method re-arms it. Source-text parse, not reflection: both
    /// blocks are top-level rustfmt items, so each ends at the first column-0 `}` and every method
    /// name sits on a line starting `fn ` (a wrapped signature still carries the name there).
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
        // `find` takes the FIRST occurrence: the real impl precedes this test's own copy of the
        // marker string in the included source.
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
        // The reverse direction (impl fn absent from the trait) is a compile error, so equality
        // here is pure belt-and-braces against a parse regression.
        assert_eq!(trait_fns, impl_fns);
    }

    /// The typed-EINVAL classifier the bitrate ladder keys on (Phase 8): the `ffmpeg::Error`
    /// must survive `with_context` layers as a downcastable source — pinned here because the
    /// entire ladder's step-down behavior rests on it, and an eager `format!` anywhere between
    /// `open_with` and the ladder would silently break it (the ladder would stop stepping and
    /// 4K sessions would surface errors instead of degrading).
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
        // ENOSYS (or any other errno) must not step the ladder.
        let e = anyhow::Error::from(ffmpeg::Error::Other {
            errno: ffmpeg::util::error::ENOSYS,
        })
        .context("open");
        assert!(!nvenc_open_einval(&e));
    }

    /// The phrase WITHOUT the type no longer classifies — the fragility that was removed: the
    /// old string match trusted any error whose rendering contained "Invalid argument".
    #[cfg(target_os = "linux")]
    #[test]
    fn nvenc_open_einval_ignores_untyped_text() {
        let e = anyhow::anyhow!("driver said: Invalid argument (not a typed libav errno)");
        assert!(!nvenc_open_einval(&e));
    }

    /// WP7.6: the resolver's full alias table, pinned. The panicking closure is the laziness
    /// contract — an explicit pref must resolve WITHOUT running the auto probe (`/serverinfo`
    /// polls through the mirrors; `gamestream/serverinfo.rs` litigated exactly that cost).
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
        // auto (and the default empty pref): the probe decides…
        assert_eq!(resolve_linux_backend("", || true, false), Some(AmdIntel));
        assert_eq!(resolve_linux_backend("auto", || false, false), Some(Nvenc));
        // …except a CUDA frame, which only NVENC can consume — and that short-circuits the
        // probe too (`||` order).
        assert_eq!(resolve_linux_backend("auto", no_probe, true), Some(Nvenc));
        // Unknown prefs name no backend: the dispatch bails; the mirrors' wrapper maps this to
        // auto (the historical `_`-arm behavior, recorded in the design doc).
        assert_eq!(resolve_linux_backend("banana", no_probe, false), None);
        // An explicit pref is never overridden by `cuda` (a CUDA payload cannot appear in a
        // session whose pref forced a non-NVENC backend; the table must not mask such a bug).
        assert_eq!(
            resolve_linux_backend("vaapi", no_probe, true),
            Some(AmdIntel)
        );
    }

    /// WP7.6: the REAL dispatch through the resolver, GPU-free via the software arm (openh264 is
    /// vendored + CPU-only; its own tests already run on every leg). This is the only in-CI
    /// execution of the Linux dispatch — before this test it had zero test call sites. The pref
    /// is INJECTED through `open_video_backend_linux` — the first draft mutated `PUNKTFUNK_ENCODER`
    /// via `set_var`, the binary's first non-ignored env mutation, racing `getenv` in the sw/codec/
    /// nvenc_core tests (diff-critic catch); the seam removes the race and the config-latch
    /// coupling outright.
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
        )
        .expect("software arm must open GPU-free");
        assert_eq!(label, "software");
        drop(enc);
        // The software arm is codec-gated — a non-H.264 session must take the bail, not fall
        // through to another backend.
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
        ) {
            // `expect_err` needs `Ok: Debug` and `Box<dyn Encoder>` isn't — match instead.
            Ok(_) => panic!("software emits H.264 only; an H.265 session must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("H.264"), "{err:#}");
    }
}
