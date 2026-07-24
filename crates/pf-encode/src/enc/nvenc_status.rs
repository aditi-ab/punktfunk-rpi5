//! Actionable explanations for `NVENCSTATUS` failures — shared by the direct-SDK NVENC backends on
//! Windows (`encode/windows/nvenc.rs`) and Linux (`encode/linux/nvenc_cuda.rs`).
//!
//! Every NVENC entry-point failure used to be annotated `(no NVIDIA GPU?)`, which actively misled
//! triage: the direct-NVENC path only loads on a machine that HAS an NVIDIA GPU, and the failure a
//! user actually hit — `NV_ENC_ERR_INVALID_VERSION` from a userspace/kernel driver version skew,
//! fixed by a reboot — has nothing to do with a missing GPU. This maps each status to what it really
//! means and what the operator should do, and folds that cause into the `anyhow::Error` at
//! construction, so every downstream `{e:#}` log (the encode-recovery loop, session teardown) says
//! the useful thing without extra plumbing.

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// A one-line, operator-actionable cause for an NVENC status. Does not repeat the raw code —
/// callers print that alongside (see [`call_err`]). Public for the few sites that build a
/// `String`/`format!` error instead of an `anyhow::Error`.
pub(super) fn explain(status: nv::NVENCSTATUS) -> String {
    match status {
        // The one this whole module exists for: a version word the driver rejects. Either the
        // driver is genuinely older than our headers, or (the sneaky case) the userspace
        // `libnvidia-encode` reports a new-enough version to the pre-flight probe but the running
        // kernel module is older and rejects the session — the classic "updated the driver, didn't
        // reboot" skew. Both heal the same way.
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_VERSION => format!(
            "the NVIDIA driver is older than this build's NVENC headers (needs NVENC API {}.{} or \
             newer), or the userspace and kernel-module driver versions are mismatched — common \
             right after a driver update without a reboot. Update the NVIDIA driver, or reboot if \
             you just updated it (a host restart is the usual fix).",
            nv::NVENCAPI_MAJOR_VERSION,
            nv::NVENCAPI_MINOR_VERSION,
        ),
        nv::NVENCSTATUS::NV_ENC_ERR_NO_ENCODE_DEVICE => {
            "this GPU exposes no usable NVENC engine — it has no hardware video encoder, or NVENC is \
             disabled on this card"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_DEVICE => {
            "this GPU model is not supported by NVENC".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_ENCODERDEVICE
        | nv::NVENCSTATUS::NV_ENC_ERR_INVALID_DEVICE => {
            "the device/context handed to NVENC is invalid — a GPU reset or driver reload can cause \
             this"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_DEVICE_NOT_EXIST => {
            "the NVENC device no longer exists — the driver reset, or the GPU fell off the bus"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY => "the GPU is out of memory".to_string(),
        nv::NVENCSTATUS::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => {
            "NVENC rejected the client key — the GeForce concurrent-NVENC-session limit was reached, \
             or the driver is unlicensed for this many encoders"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_UNIMPLEMENTED
        | nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_PARAM => {
            "this driver/GPU does not implement the requested NVENC encode mode".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM => {
            "NVENC rejected a parameter — an encode mode this GPU does not support".to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY => {
            "the NVENC engine is busy — retry, or reduce the number of concurrent encode sessions"
                .to_string()
        }
        nv::NVENCSTATUS::NV_ENC_ERR_GENERIC => {
            "the NVIDIA driver returned a generic NVENC failure — check dmesg and the driver install"
                .to_string()
        }
        other => format!("unexpected NVENC status ({other:?})"),
    }
}

/// Typed root of a failed NVENC entry-point call: carries the raw status so callers can classify
/// the failure class, not just print it — the bitrate-clamp search must only read a
/// parameter/caps rejection as "above the codec-level ceiling"; a transient failure shrinking the
/// search would discover (and cache) a bogus ceiling. Recover it through an `anyhow` chain with
/// `err.downcast_ref::<NvCallError>()` (see [`is_param_rejection`]).
#[derive(Debug)]
pub(super) struct NvCallError(pub(super) nv::NVENCSTATUS);

impl std::fmt::Display for NvCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} — {}", self.0, explain(self.0))
    }
}

impl std::error::Error for NvCallError {}

/// Whether `err` is an NVENC parameter/capability rejection: the driver understood the request
/// and says THIS config is not encodable — the clamp search's "bitrate above the ceiling"
/// evidence. Everything else (busy engine, session limit, OOM, device loss, version skew) is
/// environmental and must propagate instead of steering the search.
pub(super) fn is_param_rejection(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<NvCallError>(),
        Some(NvCallError(
            nv::NVENCSTATUS::NV_ENC_ERR_INVALID_PARAM
                | nv::NVENCSTATUS::NV_ENC_ERR_UNSUPPORTED_PARAM
                | nv::NVENCSTATUS::NV_ENC_ERR_UNIMPLEMENTED,
        ))
    )
}

/// Build an actionable `anyhow::Error` for a failed NVENC entry-point call. `call` names the API
/// (e.g. `"open_encode_session_ex"`); the chain carries both the raw status and its real-world
/// cause, so triage never again reads a version mismatch as "(no NVIDIA GPU?)". The
/// [`NvCallError`] root keeps the status downcastable for failure-class checks.
pub(super) fn call_err(call: &str, status: nv::NVENCSTATUS) -> anyhow::Error {
    anyhow::Error::new(NvCallError(status)).context(format!("NVENC {call} failed"))
}
