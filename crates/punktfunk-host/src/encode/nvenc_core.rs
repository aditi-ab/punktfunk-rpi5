//! Shared direct-SDK NVENC core — the platform-agnostic pieces of the two `nvEncodeAPI` backends,
//! Windows D3D11 (`encode/windows/nvenc.rs`) and Linux CUDA (`encode/linux/nvenc_cuda.rs`), so the
//! byte-identical glue lives once (plan §2.2, the direct-NVENC Tier-2). The per-platform parts —
//! the entry-table load (`nvEncodeAPI64.dll` via `LoadLibrary` vs `libnvidia-encode.so` via
//! `libloading`), the device binding (D3D11 vs CUDA), input-surface registration, and the
//! Windows-only async retrieve — stay in their backends. Sibling of [`super::nvenc_status`].

use super::Codec;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// Local `NVENCSTATUS` → `Result` (replaces the sdk's `result_without_string`, which lives in the
/// crate's `safe` module — code these backends must not pull in). The raw status's Debug repr
/// (`NV_ENC_ERR_INVALID_PARAM`, …) is the error payload; callers fold it through
/// [`super::nvenc_status`] for an operator-actionable cause.
pub(super) trait NvStatusExt {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS>;
}
impl NvStatusExt for nv::NVENCSTATUS {
    fn nv_ok(self) -> std::result::Result<(), nv::NVENCSTATUS> {
        match self {
            nv::NVENCSTATUS::NV_ENC_SUCCESS => Ok(()),
            err => Err(err),
        }
    }
}

/// The NVENC codec GUID for a session [`Codec`]. PyroWave never opens the direct-NVENC backend
/// (guarded by the `open_video` dispatch), so it is unreachable here.
pub(super) fn codec_guid(codec: Codec) -> nv::GUID {
    match codec {
        Codec::H264 => nv::NV_ENC_CODEC_H264_GUID,
        Codec::H265 => nv::NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => nv::NV_ENC_CODEC_AV1_GUID,
        Codec::PyroWave => unreachable!("PyroWave never opens the direct-NVENC backend"),
    }
}
