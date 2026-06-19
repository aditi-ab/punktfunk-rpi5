//! Hardware video encode (plan §7). Binds FFmpeg; never rewrites codecs. Low-latency preset,
//! B-frames off. The backend is per-GPU: NVENC on NVIDIA (`*_nvenc`, accepts `bgr0` and does
//! RGB→YUV on the GPU, so no host-side CSC) and VAAPI on AMD/Intel (`*_vaapi`; the CPU-input
//! fallback swscales RGB→NV12, the zero-copy path imports the capture dmabuf straight into a
//! VA surface). One [`Encoder`] trait, selected in [`open_video`].

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
}

/// A hardware encoder. One per session; runs on the encode thread.
pub trait Encoder: Send {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()>;
    /// Force the next submitted frame to be an IDR keyframe (e.g. after a client
    /// reference-frame-invalidation request). Default: no-op.
    fn request_keyframe(&mut self) {}
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
        let pref = std::env::var("PUNKTFUNK_ENCODER")
            .unwrap_or_default()
            .to_ascii_lowercase();
        let open_vaapi = || -> Result<Box<dyn Encoder>> {
            vaapi::VaapiEncoder::open(codec, format, width, height, fps, bitrate_bps, bit_depth)
                .map(|e| Box::new(e) as Box<dyn Encoder>)
        };
        match pref.as_str() {
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
        let _ = bit_depth; // used by the NVENC path below; the software H.264 path is 8-bit only
        let pref = std::env::var("PUNKTFUNK_ENCODER")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(pref.as_str(), "nvenc" | "hw" | "nvidia") {
            // Hardware path: NVENC over D3D11. The DXGI capturer switches to its zero-copy
            // FramePayload::D3d11 output under the same env var so capture + encode share textures.
            #[cfg(feature = "nvenc")]
            {
                let enc = nvenc::NvencD3d11Encoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    bit_depth,
                )?;
                return Ok(Box::new(enc) as Box<dyn Encoder>);
            }
            #[cfg(not(feature = "nvenc"))]
            {
                anyhow::bail!(
                    "NVENC requested but this host was built without it — rebuild with \
                     `--features nvenc` (needs the NVENC SDK's nvencodeapi.lib at link time)"
                );
            }
        }
        anyhow::ensure!(
            codec == Codec::H264,
            "the Windows software encoder supports H.264 only; client negotiated {codec:?} \
             (set PUNKTFUNK_ENCODER=nvenc for a GPU host, or request H264)"
        );
        // Software H.264 realistically caps far below the negotiated hardware rates.
        const SW_BITRATE_CEIL: u64 = 100_000_000;
        let enc = sw::OpenH264Encoder::open(
            format,
            width,
            height,
            fps,
            bitrate_bps.min(SW_BITRATE_CEIL),
        )?;
        Ok(Box::new(enc) as Box<dyn Encoder>)
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

#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(target_os = "windows", feature = "nvenc"))]
mod nvenc;
#[cfg(target_os = "windows")]
mod sw;
#[cfg(target_os = "linux")]
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
