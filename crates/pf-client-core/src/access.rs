//! The session's effective access, client-side (design/per-client-access.md §7): one
//! snapshot type over the shared grant vocabulary, the preset label derived from the mask
//! (never stored — §3.2), the overlay chip's text, and the toast wording for a mid-session
//! [`AccessUpdate`](punktfunk_core::quic::AccessUpdate). Pure presentation logic on purpose —
//! the HOST enforces the mask whatever a client renders; everything here is the courtesy
//! that makes a limited session say what it is instead of feeling broken.
//!
//! The Apple/Android clients mirror these rules rather than link them — the labels, the
//! chip/notice wording and the derive-not-store rule below are the contract they copy.

use punktfunk_core::quic::{GRANT_ALL, GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_VIEW_ONLY};
use std::time::{Duration, Instant};

/// What this session may do and for how long — the client-side snapshot of the host's
/// [`Welcome`](punktfunk_core::quic::Welcome) advert, revised by every mid-session
/// [`AccessUpdate`](punktfunk_core::quic::AccessUpdate) (latest wins). Carried on
/// [`SessionEvent::Access`](crate::session::SessionEvent::Access); the default — full
/// control, permanent — is exactly what an old host's Welcome decodes to, so a session
/// against one renders today's chrome unchanged (no chip, everything enabled).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionAccess {
    /// The effective grant bitmask ([`punktfunk_core::quic::GRANT_GAMEPAD`] family).
    pub grants: u32,
    /// When this access ends, on the CLIENT's monotonic clock; `None` = permanent.
    /// Monotonic so the chip's countdown never jumps with a wall-clock step.
    pub deadline: Option<Instant>,
}

impl Default for SessionAccess {
    fn default() -> Self {
        SessionAccess {
            grants: GRANT_ALL,
            deadline: None,
        }
    }
}

impl SessionAccess {
    /// Snapshot the connector's live access truth (grants + deadline), converting the
    /// wall-clock deadline the core keeps into this process's monotonic clock.
    pub fn from_connector(c: &punktfunk_core::client::NativeClient) -> SessionAccess {
        let deadline = c.access_deadline_unix().map(|deadline_unix| {
            let now_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            Instant::now() + Duration::from_secs(deadline_unix.saturating_sub(now_unix))
        });
        SessionAccess {
            grants: c.access_grants(),
            deadline,
        }
    }

    /// Whether traffic needing `bit` (one `GRANT_*` constant) may land on the host.
    pub fn allows(&self, bit: u32) -> bool {
        self.grants & bit != 0
    }

    /// Full control, permanent — today's default look, which must stay unchanged: no chip,
    /// no gating, no toasts (design §7; old-host degrade).
    pub fn is_default(&self) -> bool {
        self.grants == GRANT_ALL && self.deadline.is_none()
    }

    /// Time left before this access expires — `None` = permanent, zero = already due
    /// (the host's expiry close is on its way).
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    /// The overlay chip's text — "Controller only · ends in 1 h 58 m" — or `None` for the
    /// default session, which shows no chip at all.
    pub fn chip_text(&self, now: Instant) -> Option<String> {
        if self.is_default() {
            return None;
        }
        let label = preset_label(self.grants);
        match self.remaining(now) {
            Some(left) => Some(format!("{label} · ends in {}", format_remaining(left))),
            None => Some(label.to_string()),
        }
    }
}

/// The user-facing preset name DERIVED from the mask (design §3.2 — never stored, no
/// drift): the three presets, and "Custom" for any other combination.
pub fn preset_label(grants: u32) -> &'static str {
    match grants {
        GRANT_ALL => "Full control",
        GRANT_PRESET_CONTROLLER_ONLY => "Controller only",
        GRANT_PRESET_VIEW_ONLY => "View only",
        _ => "Custom",
    }
}

/// A remaining-time figure the chip/toast can wear: "1 h 58 m", "2 h", "58 m", and
/// "under 1 m" below the resolution the wire's whole seconds can honestly promise.
pub fn format_remaining(left: Duration) -> String {
    let mins = left.as_secs() / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "under 1 m".to_string(),
        (0, m) => format!("{m} m"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} m"),
    }
}

/// The toast for a mid-session access change (design §7 "end honestly"): a grants edit
/// names the new level; an unchanged-grants update is the host's expiry warning (T−5 m /
/// T−1 m) and names the time left. `None` = nothing worth interrupting for (an update
/// that reaffirmed a permanent, unchanged mask).
pub fn update_notice(prev_grants: u32, next: &SessionAccess, now: Instant) -> Option<String> {
    if next.grants != prev_grants {
        return Some(format!("Access is now {}", preset_label(next.grants)));
    }
    match next.remaining(now) {
        Some(left) if left > Duration::ZERO => {
            Some(format!("Access ends in {}", format_remaining(left)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{GRANT_CLIPBOARD, GRANT_GAMEPAD, GRANT_KEYBOARD, GRANT_POINTER};

    #[test]
    fn labels_derive_from_the_mask_per_the_design() {
        assert_eq!(preset_label(GRANT_ALL), "Full control");
        assert_eq!(preset_label(GRANT_GAMEPAD), "Controller only");
        assert_eq!(preset_label(0), "View only");
        // Anything off the three presets is Custom — including "controller + clipboard",
        // the media-remote example, and a full mask missing one bit.
        assert_eq!(preset_label(GRANT_GAMEPAD | GRANT_CLIPBOARD), "Custom");
        assert_eq!(preset_label(GRANT_ALL & !GRANT_KEYBOARD), "Custom");
    }

    #[test]
    fn the_default_session_wears_no_chip() {
        let now = Instant::now();
        assert!(SessionAccess::default().is_default());
        assert_eq!(SessionAccess::default().chip_text(now), None);
        // …and each departure from the default brings one: a narrower mask, or a deadline.
        let limited = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: None,
        };
        assert_eq!(limited.chip_text(now).as_deref(), Some("Controller only"));
        let expiring = SessionAccess {
            grants: GRANT_ALL,
            deadline: Some(now + Duration::from_secs(2 * 3600 - 120)),
        };
        assert_eq!(
            expiring.chip_text(now).as_deref(),
            Some("Full control · ends in 1 h 58 m")
        );
    }

    #[test]
    fn remaining_time_formats_at_honest_granularity() {
        assert_eq!(format_remaining(Duration::from_secs(0)), "under 1 m");
        assert_eq!(format_remaining(Duration::from_secs(59)), "under 1 m");
        assert_eq!(format_remaining(Duration::from_secs(60)), "1 m");
        assert_eq!(format_remaining(Duration::from_secs(58 * 60)), "58 m");
        assert_eq!(format_remaining(Duration::from_secs(2 * 3600)), "2 h");
        assert_eq!(
            format_remaining(Duration::from_secs(3600 + 58 * 60 + 30)),
            "1 h 58 m"
        );
    }

    #[test]
    fn notices_name_a_grants_change_first_and_warnings_by_time_left() {
        let now = Instant::now();
        // A console edit: the new level is the news, even with a deadline running.
        let narrowed = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: Some(now + Duration::from_secs(300)),
        };
        assert_eq!(
            update_notice(GRANT_ALL, &narrowed, now).as_deref(),
            Some("Access is now Controller only")
        );
        // The host's T−5 m warning: same grants, a deadline — name the time.
        let warned = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: Some(now + Duration::from_secs(300)),
        };
        assert_eq!(
            update_notice(GRANT_GAMEPAD, &warned, now).as_deref(),
            Some("Access ends in 5 m")
        );
        // An update that reaffirmed a permanent, unchanged mask: nothing to say.
        let same = SessionAccess {
            grants: GRANT_POINTER,
            deadline: None,
        };
        assert_eq!(update_notice(GRANT_POINTER, &same, now), None);
    }
}
