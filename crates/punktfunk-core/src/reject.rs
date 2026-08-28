//! Why a host turns a connection away: typed QUIC application close codes + the
//! [`RejectReason`] vocabulary shared by host and every client. Lives OUTSIDE the `quic`
//! feature gate because [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected)
//! carries it in every build; `crate::quic` re-exports it.

/// QUIC application error code the host closes with on a `mode_conflict = reject` admission
/// refusal, carrying the human-readable busy reason (live mode + client label). A distinct code
/// lets a client tell "host busy" apart from a transport failure. Shared so clients can render it.
pub const REJECT_BUSY_CLOSE_CODE: u32 = 0x42;

/// QUIC application close codes the host sends on **pairing-gate rejections**, so a client can
/// tell the user WHY it was turned away instead of collapsing every close into a generic
/// "not accepted" (the failure mode behind more than one support thread: a PIN attempt against a
/// disarmed host, an operator denial, and a dead network path all looked identical). Grouped in
/// their own 0x60 block, disjoint from [`REJECT_BUSY_CLOSE_CODE`] (0x42) and the deliberate-end
/// codes (0x51/0x52). Purely additive: an older client treats them as a bare close (exactly the
/// pre-code behavior), an older host never sends them. Decode with [`RejectReason::from_close_code`].
pub const PAIR_NOT_ARMED_CLOSE_CODE: u32 = 0x60;
/// Pairing window armed, but bound to a DIFFERENT device fingerprint (the attempt does not
/// consume the window). See [`PAIR_NOT_ARMED_CLOSE_CODE`] for the block's contract.
pub const PAIR_BOUND_OTHER_CLOSE_CODE: u32 = 0x61;
/// PIN attempt inside the host's global pairing cooldown — retry shortly.
pub const PAIR_RATE_LIMITED_CLOSE_CODE: u32 = 0x62;
/// Unpaired client presented no certificate: nothing to approve, and the SPAKE2 ceremony needs an
/// identity to bind — the PIN flow with a client identity is the way in.
pub const PAIR_NO_IDENTITY_CLOSE_CODE: u32 = 0x63;
/// The operator explicitly denied this pairing request in the host console.
pub const PAIR_DENIED_CLOSE_CODE: u32 = 0x64;
/// Nobody decided on the parked pairing request before the host's approval wait elapsed.
pub const PAIR_APPROVAL_TIMEOUT_CLOSE_CODE: u32 = 0x65;
/// This parked knock was superseded by a newer connection from the same device — only the
/// newest is admitted on approval.
pub const PAIR_SUPERSEDED_CLOSE_CODE: u32 = 0x66;
/// The client's wire (protocol) version does not match the host's — one side needs updating.
pub const WIRE_VERSION_CLOSE_CODE: u32 = 0x67;
/// The host admitted the connection but could not stand the stream session up (compositor /
/// capture / encoder setup failed host-side). The close reason bytes carry the specific error
/// text for logs/diagnostics; clients render a stable "host-side failure" sentence. Before this
/// code, a setup failure reached the client as a bare dropped connection ("control stream
/// finished mid-frame") — indistinguishable from transport trouble.
pub const SETUP_FAILED_CLOSE_CODE: u32 = 0x68;
/// This device's temporary access ran out (per-client access, `design/per-client-access.md` §4)
/// — sent when the deadline fires mid-session, and by "Expire now" in the console. Only the
/// expiring device's sessions close with it; a reconnect lands in the console's pending list
/// for a one-click re-grant.
pub const ACCESS_EXPIRED_CLOSE_CODE: u32 = 0x69;
/// The `Hello.launch` request named a game this device's grants don't cover (no `LAUNCH` bit).
/// Refused AT the handshake — a crisp typed reason beats silently dropping the user onto a
/// bare desktop they didn't ask for. Connecting *without* a launch request still works.
pub const LAUNCH_NOT_PERMITTED_CLOSE_CODE: u32 = 0x6A;
/// A host power action (`power.sleep`/`reboot`/`shutdown`, `design/host-actions.md`) is ending
/// every session: the host is going to sleep or shutting down, deliberately — not a crash, not
/// the network. Old clients render the generic close; acceptable degrade.
pub const HOST_POWER_CLOSE_CODE: u32 = 0x6B;

/// Why a host turned a connection away, decoded from the QUIC application close code — the
/// client-side view of [`PAIR_NOT_ARMED_CLOSE_CODE`]..[`WIRE_VERSION_CLOSE_CODE`] plus
/// [`REJECT_BUSY_CLOSE_CODE`]. Surfaces as
/// [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected) so every client can show
/// the real reason ("pairing not armed", "denied in the console") instead of a generic failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// No pairing window is armed on the host (arm it in the console).
    PairingNotArmed,
    /// The armed window is bound to a different device fingerprint.
    PairingBoundToOtherDevice,
    /// Inside the host's pairing cooldown — retry shortly.
    PairingRateLimited,
    /// The client presented no certificate identity to approve/bind.
    IdentityRequired,
    /// The operator denied the request in the console.
    Denied,
    /// The parked request expired with no operator decision.
    ApprovalTimeout,
    /// A newer knock from the same device replaced this one.
    Superseded,
    /// Client/host wire versions differ.
    WireVersionMismatch,
    /// The host refused admission because a conflicting session is live.
    Busy,
    /// The host admitted the connection but failed to start the stream session (host-side
    /// setup error — the host log has the specific cause).
    SetupFailed,
    /// This device's temporary access to the host has expired (per-client access).
    AccessExpired,
    /// This device's grants don't include launching games (the `LAUNCH` bit is clear).
    LaunchNotPermitted,
    /// The host is going to sleep or shutting down (a host power action ended the session).
    HostPower,
}

impl RejectReason {
    /// Decode a QUIC application close code into a reason; `None` for codes outside the
    /// shared vocabulary (a bare/legacy close stays a plain transport error).
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

    /// The close code this reason travels as (inverse of [`Self::from_close_code`]).
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

    /// Stable machine token (kebab-case) for FFI layers that pass the reason as a string
    /// (e.g. the Android JNI bridge). Do not reword existing tokens — clients match on them.
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
        // Bare closes, the client's own pair-done codes, and the deliberate-end codes must
        // never read as a host rejection. (0x69/0x6A/0x6B left this list when they became the
        // access-expired / launch-not-permitted / host-power codes; 0x6C is the block's next
        // free id.)
        for code in [0u32, 1, 0x41, 0x51, 0x52, 0x5f, 0x6C, 0x70, u32::MAX] {
            assert_eq!(RejectReason::from_close_code(code), None);
        }
    }
}
