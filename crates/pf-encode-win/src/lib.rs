//! Windows encoder backends behind one [`Encoder`] trait: direct-SDK NVENC,
//! native AMF, native QSV (VPL), and PyroWave. Plus the pieces the Linux
//! backends in `pf-encode` share with them: the NVENC glue, the slot-RFI
//! policy, the loss-recovery env knobs, and the PyroWave wire framing.
//!
//! Speaks `pf-frame` only. Backend selection, GPU inventory, and the wire
//! codec bits stay in `pf-encode`, which re-exports this crate; the adapter
//! a backend should open on arrives as a `LUID` / vendor-device pair.
//! Evidence: `design/windows-video-plane-overhaul.md` §2.6.

mod codec;
/// The encoder knobs (`host.env` via the driver request, or the environment).
pub mod knobs;
pub use codec::*;

// D3D11 colour converters + cursor blend, run on the capture device before encode.
#[cfg(target_os = "windows")]
pub mod convert;

// The manual-reset completion event AMF, QSV and Media Foundation hand out through
// `Encoder::ready_event`. Private: it is how those three answer the trait, not an API.
#[cfg(target_os = "windows")]
#[path = "windows/retrieve.rs"]
mod retrieve;
// `#[path]` keeps `crate::*` names flat. Native AMF is unconditional on
// Windows — `amfrt64.dll` at runtime, like NVENC. See `design/native-amf-encoder.md`.
#[cfg(target_os = "windows")]
#[path = "windows/amf.rs"]
pub mod amf;
// Direct-SDK NVENC on D3D11. `nvEncodeAPI64.dll` at runtime, so `--features
// nvenc` is safe on an AMD/Intel box.
#[cfg(all(target_os = "windows", feature = "nvenc"))]
#[path = "windows/nvenc.rs"]
pub mod nvenc;
// Native QSV (VPL): `qsv` feature, vendored dispatcher, GPU runtime from the
// driver store. See `design/native-qsv-encoder.md`.
#[cfg(all(target_os = "windows", feature = "qsv"))]
#[path = "windows/qsv.rs"]
pub mod qsv;
// Media Foundation: the vendor-agnostic rung below the native SDKs, and the only
// hardware encoder on Adreno. `mfplat.dll` is an OS component, so no feature — a hand
// build cannot lose it. See `design/media-foundation-encoder.md`.
#[cfg(target_os = "windows")]
#[path = "windows/mf.rs"]
pub mod mf;
// `NVENCSTATUS` → cause for both direct-NVENC backends. Splits the two
// opposite failures the driver reports as the same `INVALID_VERSION`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
pub mod nvenc_status;
// Shared `nvEncodeAPI` glue (`NvStatusExt`/`nv_ok`, `codec_guid`). Sibling of `nvenc_status`.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "nvenc"))]
pub mod nvenc_core;
// Slot-family RFI policy (taint sweep + pre-loss anchor) for AMF, QSV, Vulkan
// Video and the native VAAPI encoder. Mechanisms stay in each backend. Cfg is
// the union of callers, and the VAAPI one is featureless on Linux.
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub mod rfi;
pub mod smoke_pattern;
// Shared loss-recovery env knobs. Defaults and API clamps stay per-backend.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub mod policy;
// Windows PyroWave: NV12 D3D11→Vulkan. See `design/pyrowave-windows-host-zerocopy.md`.
#[cfg(all(target_os = "windows", feature = "pyrowave"))]
#[path = "windows/pyrowave.rs"]
pub mod pyrowave;
// Shared PyroWave AU wire-framing — both platform backends emit this layout.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub mod pyrowave_wire;
// The pyrowave-sys calls both PyroWave backends make the same way.
#[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "pyrowave"))]
pub mod pyrowave_ffi;

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

/// Codecs the active GPU can encode. AV1 encode is narrow — probe, don't assume.
/// All-`false` means the probe found nothing (GPU unusable at probe time), not
/// "zero codecs"; `pf_encode` maps that to the static superset.
#[derive(Clone, Copy, Debug)]
pub struct CodecSupport {
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
}
