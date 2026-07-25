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
//!
//! One status needs process state to explain honestly: the driver reports BOTH "your headers are
//! newer than my kernel module" and "I can no longer hand this process a session" as
//! `NV_ENC_ERR_INVALID_VERSION`. [`note_session_opened`] latches the fact that a session already
//! opened here, which tells the two apart — see [`explain`].

use std::sync::atomic::{AtomicBool, Ordering};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

/// Latched the first time `nvEncOpenEncodeSessionEx` succeeds in this process (the caps probe, a
/// real session open, or the Windows availability probe — every one of them completes the
/// userspace↔kernel-module handshake).
///
/// The load-time gate (`NvEncodeAPIGetMaxSupportedVersion`, both backends' `load_api`) can NOT
/// serve this purpose: it is a pure userspace query, so the genuine "updated the driver, didn't
/// reboot" skew sails through it and fails later, at the open. Only a session that actually opened
/// proves the kernel module agreed.
static SESSION_OPENED: AtomicBool = AtomicBool::new(false);

/// Record that an NVENC session opened. Call right after every successful
/// `open_encode_session_ex`, so [`explain`] can rule a version skew out for the rest of the
/// process.
pub(super) fn note_session_opened() {
    SESSION_OPENED.store(true, Ordering::Relaxed);
}

/// The two very different failures the driver reports as `NV_ENC_ERR_INVALID_VERSION`, split on
/// whether a session has already opened here (`session_opened`). Pure, so both halves are testable
/// without touching the process-wide latch.
fn invalid_version(session_opened: bool) -> String {
    if session_opened {
        // Same status, opposite cause: a session ALREADY opened in this process, so the driver's
        // kernel module accepted this exact build's version word minutes ago. A version skew is
        // static — it cannot come and go — so "update the driver / reboot" is the wrong advice
        // here, and following it costs the operator a reboot per stream (2026-07 field report: one
        // stream works, the next fails at the caps probe, forever, until the PROCESS restarts).
        // What is left is per-process driver state: a resource the last session did not give back,
        // or a wedged/lost device. Say that, and point at the cheap fix.
        // Worded for ANY call (`explain` also serves `lock_bitstream`); `call_err` already names
        // the entry point ahead of this text, so it must not assume the session open.
        return "this process already opened an NVENC session successfully, so this is NOT a driver \
                version mismatch — that cannot come and go within a process, and a reboot is not \
                the fix. The NVIDIA driver state in THIS process is exhausted or wedged: restart \
                the Punktfunk host service to clear it, and please report this with the host log \
                so it can be fixed properly"
            .to_string();
    }
    // No session has ever opened here, so the version word really is in question. Either the
    // driver is genuinely older than our headers, or (the sneaky case) the userspace
    // `libnvidia-encode` reports a new-enough version to the pre-flight probe but the running
    // kernel module is older and rejects the session — the classic "updated the driver, didn't
    // reboot" skew. Both heal the same way.
    format!(
        "the NVIDIA driver is older than this build's NVENC headers (needs NVENC API {}.{} or \
         newer), or the userspace and kernel-module driver versions are mismatched — common right \
         after a driver update without a reboot. Update the NVIDIA driver, or reboot if you just \
         updated it (a host restart is the usual fix).",
        nv::NVENCAPI_MAJOR_VERSION,
        nv::NVENCAPI_MINOR_VERSION,
    )
}

/// A one-line, operator-actionable cause for an NVENC status. Does not repeat the raw code —
/// callers print that alongside (see [`call_err`]). Public for the few sites that build a
/// `String`/`format!` error instead of an `anyhow::Error`.
pub(super) fn explain(status: nv::NVENCSTATUS) -> String {
    match status {
        nv::NVENCSTATUS::NV_ENC_ERR_INVALID_VERSION => {
            invalid_version(SESSION_OPENED.load(Ordering::Relaxed))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Before any session has opened, the version word IS in question — keep the skew advice.
    #[test]
    fn invalid_version_before_any_session_blames_the_driver_version() {
        let msg = invalid_version(false);
        assert!(
            msg.contains("older than this build's NVENC headers"),
            "{msg}"
        );
        assert!(msg.contains("reboot if you just updated it"), "{msg}");
    }

    /// Once a session has opened here, a skew is impossible — the message must stop sending
    /// operators to reboot (2026-07 field report: one stream per boot, forever).
    #[test]
    fn invalid_version_after_a_session_blames_process_state_not_the_driver() {
        let msg = invalid_version(true);
        assert!(msg.contains("NOT a driver version mismatch"), "{msg}");
        assert!(msg.contains("restart the Punktfunk host service"), "{msg}");
        assert!(
            !msg.contains("older than this build's NVENC headers"),
            "must not repeat the version-skew advice: {msg}"
        );
        assert!(
            !msg.contains("Update the NVIDIA driver"),
            "must not tell the operator to update a driver that just worked: {msg}"
        );
    }

    /// The latch is one-way and only touches this status.
    #[test]
    fn note_session_opened_latches() {
        note_session_opened();
        assert!(SESSION_OPENED.load(Ordering::Relaxed));
        note_session_opened();
        assert!(SESSION_OPENED.load(Ordering::Relaxed));
        assert_eq!(
            explain(nv::NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY),
            "the GPU is out of memory"
        );
    }
}
