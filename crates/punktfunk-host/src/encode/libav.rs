//! Shared libavcodec (FFmpeg) glue for the three libav encode backends — Linux NVENC
//! (`encode/linux/mod.rs`), VAAPI (`encode/linux/vaapi.rs`), and Windows AMF/QSV
//! (`encode/windows/ffmpeg_win.rs`) — so the byte-identical pieces live once (plan §2.2, the Tier-2
//! gap). Free functions and consts over borrowed handles; nothing here is per-frame `dyn`,
//! allocating, or on the zero-copy ingest path.
use ffmpeg_next::ffi; // = ffmpeg_sys_next
use ffmpeg_next::format::Pixel;
use std::os::raw::c_int;

/// swscale: nearest-neighbour scaler flag (`SWS_POINT`). We never rescale (src dims == dst dims), so
/// the resampler choice only governs the colour-conversion path; POINT is the cheapest.
pub(crate) const SWS_POINT: c_int = 0x10;
/// swscale colorspace id for ITU-R BT.709 (`SWS_CS_ITU709`) — the CSC coefficients for our RGB→YUV.
pub(crate) const SWS_CS_ITU709: c_int = 1;
/// swscale colorspace id for ITU-R BT.2020 non-constant-luminance (`SWS_CS_BT2020`) — the CSC
/// coefficients for the HDR X2BGR10→P010 path (Windows only today).
pub(crate) const SWS_CS_BT2020: c_int = 9;

/// `Pixel` → `AVPixelFormat`. `Pixel` is `#[repr(i32)]`-compatible with `AVPixelFormat` (the bindgen
/// enum) via this documented conversion in ffmpeg-next.
pub(crate) fn pixel_to_av(p: Pixel) -> ffi::AVPixelFormat {
    ffi::AVPixelFormat::from(p)
}
