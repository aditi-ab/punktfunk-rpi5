//! Hardware video encode (plan §7). Binds FFmpeg; never rewrites codecs. Low-latency preset,
//! B-frames off. The backend is per-GPU: NVENC on NVIDIA (`*_nvenc`, accepts `bgr0` and does
//! RGB→YUV on the GPU, so no host-side CSC) and VAAPI on AMD/Intel (`*_vaapi`; the CPU-input
//! fallback swscales RGB→NV12, the zero-copy path imports the capture dmabuf straight into a
//! VA surface). One [`Encoder`] trait, selected in [`open_video`].
// This file's own unsafe block carries a `// SAFETY:` proof, but the file-level
// `#![deny(clippy::undocumented_unsafe_blocks)]` is deliberately NOT set yet: as a parent module it
// would propagate the lint to `encode::windows::nvenc` (in-flight parallel work, not yet proven).
// The deny lands here once every child module (incl. nvenc.rs) is documented.

use crate::capture::{CapturedFrame, PixelFormat};
use anyhow::Result;

/// An encoded access unit (one NAL/AU) to hand to `punktfunk_core` for FEC + packetization.
/// `data` is in-band Annex-B (the encoder is opened without a global header), so each
/// keyframe carries its own VPS/SPS/PPS — the bytes are both a playable elementary
/// stream and a self-contained AU for the wire.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts_ns: u64,
    /// True for IDR/keyframes (sets the SOF/keyframe wire flags).
    pub keyframe: bool,
}

/// Codec selection negotiated with the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    /// The FFmpeg NVENC encoder name (selected by name, not codec id — the latter would
    /// pick the software encoder).
    pub fn nvenc_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_nvenc",
            Codec::H265 => "hevc_nvenc",
            Codec::Av1 => "av1_nvenc",
        }
    }

    /// The FFmpeg VAAPI encoder name (AMD via Mesa `radeonsi`, Intel via `iHD`/`i965`). One
    /// libavcodec encoder per codec covers both vendors — the kernel driver differs, the libva
    /// userspace API is identical. Selected by name (the codec id would pick the SW encoder).
    /// AV1 VAAPI encode is narrow (Intel Arc/Xe2+, AMD RDNA3+/RDNA4) — gate it on a capability
    /// probe, never assume it (see [`open_video`]).
    pub fn vaapi_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_vaapi",
            Codec::H265 => "hevc_vaapi",
            Codec::Av1 => "av1_vaapi",
        }
    }

    /// The FFmpeg AMD **AMF** encoder name (the Windows AMD backend). Selected by name (the codec id
    /// would pick the software encoder). AV1 (`av1_amf`) is RDNA3+/RX 7000+ — probe, never assume.
    pub fn amf_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_amf",
            Codec::H265 => "hevc_amf",
            Codec::Av1 => "av1_amf",
        }
    }

    /// The FFmpeg Intel **QSV** encoder name (the Windows Intel backend). Selected by name. AV1
    /// (`av1_qsv`) is Arc/Xe2+; HEVC Main10 is Gen9.5+ — probe, never assume.
    pub fn qsv_name(self) -> &'static str {
        match self {
            Codec::H264 => "h264_qsv",
            Codec::H265 => "hevc_qsv",
            Codec::Av1 => "av1_qsv",
        }
    }
}

/// Static capabilities an [`Encoder`] declares so the session glue routes loss-recovery and HDR
/// plumbing by *query* rather than relying on a method's no-op/`false` default. Cheap `Copy`; fixed
/// for the session (an HDR toggle re-initialises the encoder — re-query if that matters).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncoderCaps {
    /// The encoder can perform real reference-frame invalidation — i.e.
    /// [`invalidate_ref_frames`](Encoder::invalidate_ref_frames) can return `true`. When `false`
    /// the caller skips that always-`false` call and forces a keyframe directly on loss recovery.
    /// Only the Windows direct-NVENC path implements RFI; libavcodec (Linux NVENC), VAAPI and
    /// AMF/QSV always keyframe.
    pub supports_rfi: bool,
    /// The encoder emits in-band HDR mastering/CLL SEI from [`set_hdr_meta`](Encoder::set_hdr_meta).
    /// When `false`, `set_hdr_meta` is a no-op and no in-band grade reaches the client. Only the
    /// Windows direct-NVENC path attaches it today.
    pub supports_hdr_metadata: bool,
}

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// This encoder's static [capabilities](EncoderCaps) (RFI, HDR SEI), so the session glue can
    /// route by query rather than rely on the no-op/`false` defaults of
    /// [`invalidate_ref_frames`](Self::invalidate_ref_frames) / [`set_hdr_meta`](Self::set_hdr_meta).
    /// Default: no optional capabilities (the SDR / libavcodec backends) — only the direct-NVENC
    /// path overrides it.
    fn caps(&self) -> EncoderCaps {
        EncoderCaps::default()
    }
    /// Force the next submitted frame to be an IDR keyframe (e.g. after a client
    /// reference-frame-invalidation request). Default: no-op.
    fn request_keyframe(&mut self) {}
    /// Set the source's static HDR mastering metadata (from the capturer). An HDR encoder emits it
    /// as in-band SEI (`mastering_display_colour_volume` + `content_light_level_info`) on each
    /// keyframe so any decoder — including stock Moonlight — tone-maps from the source's real grade.
    /// Default: no-op (SDR encoders / libavcodec paths that don't attach it yet). Cheap to call
    /// every frame; only the direct-NVENC path consumes it.
    fn set_hdr_meta(&mut self, _meta: Option<punktfunk_core::quic::HdrMeta>) {}
    /// Invalidate a contiguous range of previously-encoded reference frames (client frame numbers,
    /// as reported in a loss-recovery request) so the encoder re-references an older still-valid
    /// frame instead of emitting a full IDR. Returns `true` if a real reference invalidation was
    /// performed; `false` means the encoder couldn't (range older than the DPB, or the backend has
    /// no RFI) and the caller should fall back to [`request_keyframe`](Self::request_keyframe).
    /// Default: `false` — only the Windows direct-NVENC path implements true RFI; libavcodec
    /// (Linux NVENC) and VAAPI can't express `nvEncInvalidateRefFrames`, so they keyframe.
    fn invalidate_ref_frames(&mut self, _first_frame: i64, _last_frame: i64) -> bool {
        false
    }
    /// Pull the next encoded AU if one is ready.
    fn poll(&mut self) -> Result<Option<EncodedFrame>>;
    /// Signal end-of-stream. After this, drain the remaining AUs with [`poll`](Self::poll)
    /// until it returns `None` — NVENC buffers frames internally even at `delay=0`.
    fn flush(&mut self) -> Result<()>;
}

impl Codec {
    /// Maximum encodable dimension (px) per side for this codec on NVENC. H.264 tops out at
    /// 4096 (level constraint); HEVC and AV1 allow 8192. Used to reject out-of-range client
    /// modes up front (see [`validate_dimensions`]).
    pub fn max_dimension(self) -> u32 {
        match self {
            Codec::H264 => 4096,
            Codec::H265 | Codec::Av1 => 8192,
        }
    }

    /// The codec's *spec* top level/tier bitrate (bits/s) — the usual boundary at which NVENC
    /// starts rejecting `avcodec_open2` with EINVAL. NOT a hard cap: [`open_video`](crate::encode::
    /// open_video) probes the actual GPU ceiling by stepping DOWN from the requested bitrate only on
    /// EINVAL, and uses this purely as the first step-down candidate (so a card that accepts more —
    /// an RTX 5070 Ti does >1 Gbps HEVC where a 4090 caps at ~800 Mbps — is never clamped to it).
    /// HEVC Level 6.2 High tier = 800 Mbps; H.264 High level 6.2 ≈ 480 Mbps; AV1's levels allow more.
    pub fn max_bitrate_bps(self) -> u64 {
        match self {
            Codec::H264 => 480_000_000,
            Codec::H265 => 800_000_000,
            Codec::Av1 => 1_200_000_000,
        }
    }
}

/// Validate a requested encode resolution before we allocate buffers or open NVENC. Rejects
/// zero/odd-sized and out-of-range modes with a clear error instead of letting buffer math
/// overflow or the encoder open fail with an opaque NVENC code. A client can request any
/// `mode=WxHxFPS`, so this is the gate on attacker/typo-controlled dimensions.
pub fn validate_dimensions(codec: Codec, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be non-zero");
    }
    // NVENC requires even dimensions for the chroma subsampling it does internally.
    if width % 2 != 0 || height % 2 != 0 {
        anyhow::bail!("invalid encode resolution {width}x{height}: dimensions must be even");
    }
    let max = codec.max_dimension();
    if width > max || height > max {
        anyhow::bail!(
            "{codec:?} max dimension is {max}px; requested {width}x{height} \
             (use HEVC/AV1 above 4096, or lower the client resolution)"
        );
    }
    Ok(())
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
) -> Result<Box<dyn Encoder>> {
    validate_dimensions(codec, width, height)?;
    #[cfg(target_os = "linux")]
    {
        // Pick the GPU encode backend. NVIDIA → NVENC/CUDA (the original path, unchanged);
        // AMD/Intel → VAAPI (one libavcodec backend for both). Auto-detect by default so a single
        // Linux binary serves any GPU; `PUNKTFUNK_ENCODER` forces a specific backend (and surfaces
        // its errors crisply instead of silently trying the other).
        let pref = crate::config::config().encoder_pref.as_str();
        let open_vaapi = || -> Result<Box<dyn Encoder>> {
            vaapi::VaapiEncoder::open(codec, format, width, height, fps, bitrate_bps, bit_depth)
                .map(|e| Box::new(e) as Box<dyn Encoder>)
        };
        match pref {
            "nvenc" | "nvidia" | "cuda" => open_nvenc_probed(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                cuda,
                bit_depth,
            ),
            "vaapi" | "amd" | "intel" => open_vaapi(),
            "auto" | "" => {
                // A CUDA frame can ONLY be consumed by NVENC, and a box with the NVIDIA device
                // nodes always prefers it. Everything else (AMD/Intel) takes the VAAPI path.
                if cuda || nvidia_present() {
                    open_nvenc_probed(
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        cuda,
                        bit_depth,
                    )
                } else {
                    open_vaapi()
                }
            }
            other => anyhow::bail!(
                "unknown PUNKTFUNK_ENCODER={other:?} — use auto (default), nvenc, or vaapi"
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = cuda; // always false on Windows (no Cuda payload)
                      // NVIDIA → NVENC (direct SDK), AMD → AMF, Intel → QSV (both libavcodec), else → software
                      // H.264. `auto` (the default) resolves from the DXGI adapter vendor.
        match windows_resolved_backend() {
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
                    )
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
                }
                #[cfg(not(feature = "nvenc"))]
                {
                    anyhow::bail!(
                        "NVENC requested/detected but this host was built without it — rebuild \
                         with `--features nvenc` (needs the NVENC SDK's nvencodeapi.lib at link time)"
                    )
                }
            }
            backend @ (WindowsBackend::Amf | WindowsBackend::Qsv) => {
                // AMD AMF / Intel QSV via libavcodec (the Windows analogue of the Linux VAAPI path).
                #[cfg(feature = "amf-qsv")]
                {
                    let vendor = if matches!(backend, WindowsBackend::Amf) {
                        ffmpeg_win::WinVendor::Amf
                    } else {
                        ffmpeg_win::WinVendor::Qsv
                    };
                    ffmpeg_win::FfmpegWinEncoder::open(
                        vendor,
                        codec,
                        format,
                        width,
                        height,
                        fps,
                        bitrate_bps,
                        bit_depth,
                    )
                    .map(|e| Box::new(e) as Box<dyn Encoder>)
                }
                #[cfg(not(feature = "amf-qsv"))]
                {
                    let _ = backend;
                    anyhow::bail!(
                        "AMD/Intel (AMF/QSV) encode requested/detected but this host was built \
                         without it — rebuild with `--features amf-qsv` (needs ffmpeg-next + a \
                         FFMPEG_DIR with the AMF/QSV encoders at build time)"
                    )
                }
            }
            WindowsBackend::Software => {
                anyhow::ensure!(
                    codec == Codec::H264,
                    "the Windows software encoder supports H.264 only; client negotiated {codec:?} \
                     (build a GPU backend: --features nvenc or amf-qsv, or request H264)"
                );
                let _ = bit_depth; // the software H.264 path is 8-bit only
                                   // Software H.264 realistically caps far below the negotiated hardware rates.
                const SW_BITRATE_CEIL: u64 = 100_000_000;
                sw::OpenH264Encoder::open(
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps.min(SW_BITRATE_CEIL),
                )
                .map(|e| Box::new(e) as Box<dyn Encoder>)
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
) -> Result<Box<dyn Encoder>> {
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
        match linux::NvencEncoder::open(codec, format, width, height, fps, b, cuda, bit_depth) {
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
            Err(e) if format!("{e:#}").contains("Invalid argument") => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("encoder open failed at every probed bitrate")))
}

/// Cheap, side-effect-free NVIDIA-presence probe for the `auto` backend selector: the NVIDIA
/// kernel driver exposes these device nodes, AMD/Intel boxes have neither. Deliberately does NOT
/// create a CUDA context (that would allocate GPU state on every host that merely *might* be
/// NVIDIA). `PUNKTFUNK_ENCODER` overrides this entirely.
#[cfg(target_os = "linux")]
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// True if the Linux GPU encode backend resolves to VAAPI (AMD/Intel) rather than NVENC — mirrors
/// [`open_video`]'s dispatch so the capturer can choose the matching zero-copy path (raw dmabuf
/// passthrough for VAAPI vs the EGL→CUDA import for NVENC).
#[cfg(target_os = "linux")]
pub fn linux_zero_copy_is_vaapi() -> bool {
    match crate::config::config().encoder_pref.as_str() {
        "nvenc" | "nvidia" | "cuda" => false,
        "vaapi" | "amd" | "intel" => true,
        _ => !nvidia_present(),
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

// ---------------------------------------------------------------------------------------------
// Windows backend selection (the analogue of the Linux nvidia_present / linux_zero_copy_is_vaapi
// logic). NVIDIA → NVENC, AMD → AMF, Intel → QSV; `auto` (default) reads the DXGI adapter vendor.
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowsBackend {
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

/// Resolve the active Windows encode backend from `PUNKTFUNK_ENCODER` (`auto` → the DXGI adapter
/// vendor). Shared by [`open_video`] and the GameStream codec advertisement so both agree.
#[cfg(target_os = "windows")]
pub(crate) fn windows_resolved_backend() -> WindowsBackend {
    // Resolved ONCE in HostConfig (Goal-1) — was re-read from PUNKTFUNK_ENCODER on every call.
    match crate::config::config().encoder_pref.as_str() {
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

/// True if the active Windows backend is the libavcodec AMF/QSV path (so the codec advertisement
/// consults a real GPU probe rather than the NVENC static superset). Always false when the
/// `amf-qsv` feature is off — there's then no ffmpeg backend to probe.
#[cfg(target_os = "windows")]
pub fn windows_backend_is_ffmpeg() -> bool {
    cfg!(feature = "amf-qsv")
        && matches!(
            windows_resolved_backend(),
            WindowsBackend::Amf | WindowsBackend::Qsv
        )
}

/// Detect the host GPU vendor from the first hardware DXGI adapter (Windows has no `/dev/nvidia*`
/// probe). Cached. NVIDIA=0x10DE, AMD=0x1002, Intel=0x8086; the software/WARP adapter is skipped.
#[cfg(target_os = "windows")]
fn windows_gpu_vendor() -> Option<GpuVendor> {
    use std::sync::OnceLock;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    static CACHE: OnceLock<Option<GpuVendor>> = OnceLock::new();
    // SAFETY: `CreateDXGIFactory1` returns a fresh owned `IDXGIFactory1` COM object (refcounted by the
    // windows-rs wrapper, Released when the local drops); `.ok()?` bails on failure so `factory` is a
    // valid interface before any use. `EnumAdapters1(i)` hands back the i-th adapter as an owned
    // `IDXGIAdapter1` (or an error past the last adapter, which ends the loop). `GetDesc1()` returns the
    // `DXGI_ADAPTER_DESC1` by value (no out-pointer), so reading `desc.Flags`/`desc.VendorId` is plain
    // field access. Every call only touches COM objects this closure owns; the `OnceLock` runs the
    // closure once (no data race) and all interfaces are Released as the locals drop. No raw pointer is
    // dereferenced and nothing is aliased.
    *CACHE.get_or_init(|| unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            // windows-rs 0.62: GetDesc1 returns the desc by value (no out-param).
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                continue; // skip the Microsoft Basic Render / WARP adapter
            }
            match desc.VendorId {
                0x10DE => return Some(GpuVendor::Nvidia),
                0x1002 => return Some(GpuVendor::Amd),
                0x8086 => return Some(GpuVendor::Intel),
                _ => continue,
            }
        }
        None
    })
}

/// Probe the active Windows AMF/QSV backend for its encodable codecs (cached; opens a tiny encoder
/// per codec, once). Mirrors [`vaapi_codec_support`]; called only when [`windows_backend_is_ffmpeg`]
/// is true. AV1 is narrow (AMD RDNA3+, Intel Arc/Xe2+), so it must be probed, not assumed.
#[cfg(all(target_os = "windows", feature = "amf-qsv"))]
pub fn windows_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
    static CACHE: OnceLock<CodecSupport> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let vendor = match windows_resolved_backend() {
            WindowsBackend::Qsv => ffmpeg_win::WinVendor::Qsv,
            _ => ffmpeg_win::WinVendor::Amf,
        };
        let caps = CodecSupport {
            h264: ffmpeg_win::probe_can_encode(vendor, Codec::H264),
            h265: ffmpeg_win::probe_can_encode(vendor, Codec::H265),
            av1: ffmpeg_win::probe_can_encode(vendor, Codec::Av1),
        };
        tracing::info!(
            backend = ?vendor,
            h264 = caps.h264,
            h265 = caps.h265,
            av1 = caps.av1,
            "Windows AMF/QSV encode capabilities probed"
        );
        caps
    })
}

// Goal-1 stage 6: GPU/CPU encoders confined to `encode/windows/` (NVENC, AMF/QSV ffmpeg, software) and
// `encode/linux/` (NVENC/CUDA + VAAPI); `#[path]` keeps the `crate::encode::*` module names flat.
#[cfg(all(target_os = "windows", feature = "amf-qsv"))]
#[path = "encode/windows/ffmpeg_win.rs"]
mod ffmpeg_win;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "encode/windows/nvenc.rs"]
mod nvenc;
#[cfg(target_os = "windows")]
#[path = "encode/windows/sw.rs"]
mod sw;
#[cfg(target_os = "linux")]
#[path = "encode/linux/vaapi.rs"]
mod vaapi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_and_odd_dimensions() {
        assert!(validate_dimensions(Codec::H265, 0, 1080).is_err());
        assert!(validate_dimensions(Codec::H265, 1920, 0).is_err());
        assert!(validate_dimensions(Codec::H265, 1921, 1080).is_err()); // odd width
        assert!(validate_dimensions(Codec::H265, 1920, 1081).is_err()); // odd height
    }

    #[test]
    fn h264_capped_at_4096() {
        assert!(validate_dimensions(Codec::H264, 3840, 2160).is_ok()); // 4K fits (width < 4096)
        assert!(validate_dimensions(Codec::H264, 4096, 4096).is_ok()); // exactly at the limit
        assert!(validate_dimensions(Codec::H264, 4098, 2160).is_err());
        assert!(validate_dimensions(Codec::H264, 3840, 4098).is_err());
    }

    #[test]
    fn hevc_and_av1_allow_up_to_8192() {
        for c in [Codec::H265, Codec::Av1] {
            assert!(validate_dimensions(c, 3840, 2160).is_ok());
            assert!(validate_dimensions(c, 7680, 4320).is_ok()); // 8K fits
            assert!(validate_dimensions(c, 8192, 8192).is_ok());
            assert!(validate_dimensions(c, 8194, 4320).is_err());
        }
    }

    #[test]
    fn common_modes_accepted() {
        for c in [Codec::H264, Codec::H265, Codec::Av1] {
            for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440)] {
                assert!(validate_dimensions(c, w, h).is_ok(), "{c:?} {w}x{h}");
            }
        }
    }
}
