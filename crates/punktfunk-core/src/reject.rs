//! Typed QUIC application close codes and the [`RejectReason`] vocabulary a
//! host uses to turn a connection away. Lives outside the `quic` feature
//! because [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected)
//! carries it in every build; `crate::quic` re-exports it.

/// `mode_conflict = reject` admission close. Distinct from a transport
/// failure so a client can render "host busy". Reason bytes carry live mode
/// + client label.
pub const REJECT_BUSY_CLOSE_CODE: u32 = 0x42;

/// Pairing-gate close. Occupies 0x60.., disjoint from
/// [`REJECT_BUSY_CLOSE_CODE`] (0x42) and the deliberate-end codes (0x51/0x52).
/// Decode with [`RejectReason::from_close_code`].
pub const PAIR_NOT_ARMED_CLOSE_CODE: u32 = 0x60;
/// Armed window is bound to a different fingerprint; the attempt does not
/// consume it.
pub const PAIR_BOUND_OTHER_CLOSE_CODE: u32 = 0x61;
/// Inside the host's global pairing cooldown.
pub const PAIR_RATE_LIMITED_CLOSE_CODE: u32 = 0x62;
/// No client certificate: SPAKE2 has nothing to bind.
pub const PAIR_NO_IDENTITY_CLOSE_CODE: u32 = 0x63;
pub const PAIR_DENIED_CLOSE_CODE: u32 = 0x64;
pub const PAIR_APPROVAL_TIMEOUT_CLOSE_CODE: u32 = 0x65;
/// Only the newest knock from this device is admitted.
pub const PAIR_SUPERSEDED_CLOSE_CODE: u32 = 0x66;
pub const WIRE_VERSION_CLOSE_CODE: u32 = 0x67;
/// Admitted, then compositor / capture / encoder setup failed. Reason bytes
/// carry the host error; clients render a stable "host-side failure" sentence.
pub const SETUP_FAILED_CLOSE_CODE: u32 = 0x68;
/// Per-client access deadline, or console "Expire now". Only this device's
/// sessions close; a reconnect parks in the pending list.
/// `design/per-client-access.md`.
pub const ACCESS_EXPIRED_CLOSE_CODE: u32 = 0x69;
/// `Hello.launch` named a game this device's grants lack the `LAUNCH` bit
/// for. Refused at handshake; connecting without a launch request still works.
pub const LAUNCH_NOT_PERMITTED_CLOSE_CODE: u32 = 0x6A;
/// Host power action (`power.sleep` / `reboot` / `shutdown`) is ending every
/// session. `design/host-actions.md`.
pub const HOST_POWER_CLOSE_CODE: u32 = 0x6B;

/// Client-side view of the host's QUIC application close code. Surfaces as
/// [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    PairingNotArmed,
    PairingBoundToOtherDevice,
    PairingRateLimited,
    IdentityRequired,
    Denied,
    ApprovalTimeout,
    Superseded,
    WireVersionMismatch,
    Busy,
    SetupFailed,
    AccessExpired,
    LaunchNotPermitted,
    HostPower,
}

impl RejectReason {
    /// `None` for codes outside this vocabulary — a bare/legacy close stays a
    /// transport error.
    pub fn from_close_code(code: u32) -> Option<Self> {
        Some(match code {
            PAIR_NOT_ARMED_CLOSE_CODE => Self::PairingNotArmed,
            PAIR_BOUND_OTHER_CLOSE_CODE => Self::PairingBoundToOtherDevice,
            PAIR_RATE_LIMITED_CLOSE_CODE => Self::PairingRateLimited,
            PAIR_NO_IDENTITY_CLOSE_CODE => Self::IdentityRequired,
            PAIR_DENIED_CLOSE_CODE => Self::Denied,
            PAIR_APPROVAL_TIMEOUT_CLOSE_CODE => Self::ApprovalTimeout,
            PAIR_SUPERSEDED_CLOSE_CODE => Self::Superseded,
            WIRE_VERSION_CLOSE_CODE => Self::WireVersionMismatch,
            REJECT_BUSY_CLOSE_CODE => Self::Busy,
            SETUP_FAILED_CLOSE_CODE => Self::SetupFailed,
            ACCESS_EXPIRED_CLOSE_CODE => Self::AccessExpired,
            LAUNCH_NOT_PERMITTED_CLOSE_CODE => Self::LaunchNotPermitted,
            HOST_POWER_CLOSE_CODE => Self::HostPower,
            _ => return None,
        })
    }

    /// Inverse of [`Self::from_close_code`].
    pub fn close_code(self) -> u32 {
        match self {
            Self::PairingNotArmed => PAIR_NOT_ARMED_CLOSE_CODE,
            Self::PairingBoundToOtherDevice => PAIR_BOUND_OTHER_CLOSE_CODE,
            Self::PairingRateLimited => PAIR_RATE_LIMITED_CLOSE_CODE,
            Self::IdentityRequired => PAIR_NO_IDENTITY_CLOSE_CODE,
            Self::Denied => PAIR_DENIED_CLOSE_CODE,
            Self::ApprovalTimeout => PAIR_APPROVAL_TIMEOUT_CLOSE_CODE,
            Self::Superseded => PAIR_SUPERSEDED_CLOSE_CODE,
            Self::WireVersionMismatch => WIRE_VERSION_CLOSE_CODE,
            Self::Busy => REJECT_BUSY_CLOSE_CODE,
            Self::SetupFailed => SETUP_FAILED_CLOSE_CODE,
            Self::AccessExpired => ACCESS_EXPIRED_CLOSE_CODE,
            Self::LaunchNotPermitted => LAUNCH_NOT_PERMITTED_CLOSE_CODE,
            Self::HostPower => HOST_POWER_CLOSE_CODE,
        }
    }

    /// Stable kebab-case token for FFI (Android JNI). Do not reword — clients
    /// match on these.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PairingNotArmed => "not-armed",
            Self::PairingBoundToOtherDevice => "bound-other",
            Self::PairingRateLimited => "rate-limited",
            Self::IdentityRequired => "identity-required",
            Self::Denied => "denied",
            Self::ApprovalTimeout => "approval-timeout",
            Self::Superseded => "superseded",
            Self::WireVersionMismatch => "wire-version",
            Self::Busy => "busy",
            Self::SetupFailed => "setup-failed",
            Self::AccessExpired => "access-expired",
            Self::LaunchNotPermitted => "launch-not-permitted",
            Self::HostPower => "host-power",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PairingNotArmed => "pairing is not armed on the host",
            Self::PairingBoundToOtherDevice => {
                "the host's pairing window is armed for a different device"
            }
            Self::PairingRateLimited => "pairing attempts are rate-limited — retry shortly",
            Self::IdentityRequired => "the host requires a client identity",
            Self::Denied => "the request was denied on the host",
            Self::ApprovalTimeout => "nobody approved the request on the host in time",
            Self::Superseded => "a newer request from this device replaced this one",
            Self::WireVersionMismatch => "client and host versions do not match",
            Self::Busy => "the host is busy with another session",
            Self::SetupFailed => "the host could not start the stream session",
            Self::AccessExpired => "your access to this host has expired",
            Self::LaunchNotPermitted => "this device is not permitted to launch games on the host",
            Self::HostPower => "the host is going to sleep or shutting down",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RejectReason; 13] = [
        RejectReason::PairingNotArmed,
        RejectReason::PairingBoundToOtherDevice,
        RejectReason::PairingRateLimited,
        RejectReason::IdentityRequired,
        RejectReason::Denied,
        RejectReason::ApprovalTimeout,
        RejectReason::Superseded,
        RejectReason::WireVersionMismatch,
        RejectReason::Busy,
        RejectReason::SetupFailed,
        RejectReason::AccessExpired,
        RejectReason::LaunchNotPermitted,
        RejectReason::HostPower,
    ];

    #[test]
    fn close_codes_round_trip() {
        for r in ALL {
            assert_eq!(RejectReason::from_close_code(r.close_code()), Some(r));
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<u32> = ALL.iter().map(|r| r.close_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), ALL.len());
    }

    #[test]
    fn foreign_codes_stay_untyped() {
        // Bare closes, pair-done, and 0x51/0x52 (deliberate-end) must never
        // decode as a rejection. 0x6C is the next free id in the 0x60 block.
        for code in [0u32, 1, 0x41, 0x51, 0x52, 0x5f, 0x6C, 0x70, u32::MAX] {
            assert_eq!(RejectReason::from_close_code(code), None);
        }
    }
}
