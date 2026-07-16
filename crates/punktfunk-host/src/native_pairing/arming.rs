//! The on-demand arming PIN window (plan §W5 — carved out of the [`super`] facade). Owns the
//! [`Armed`] state behind a [`Mutex`]: a short-lived (or CLI-flag, no-expiry) PIN the host mints
//! and the operator reads from the web console, optionally bound to one device fingerprint (#9).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The current arming window. `pin == None` ⇒ disarmed. `expires_at == None` ⇒ armed with no
/// expiry (the CLI `--allow-pairing` flag); `Some(t)` ⇒ a web-armed window that auto-disarms.
///
/// `bound_fp == Some(fp)` ⇒ the window is **bound to one operator-selected device fingerprint**:
/// only a pairing attempt from that fingerprint may consume it (security-review 2026-06-28 #9). This
/// closes the window-burn DoS — an unpaired LAN peer cannot consume a window armed for a specific
/// device, because the QUIC client-auth proves cert possession (it can't forge the bound fingerprint).
/// `None` ⇒ unbound (the CLI flag / a console "arm open"): any well-formed attempt consumes it (the
/// legacy behavior, retaining the window-burn DoS — acceptable only on a trusted LAN).
#[derive(Default)]
struct Armed {
    pin: Option<String>,
    expires_at: Option<Instant>,
    bound_fp: Option<String>,
}

/// The result of resolving the armed PIN for a specific client fingerprint
/// (`NativePairing::pin_for_attempt`).
pub enum PinAttempt {
    /// No window is armed (disarmed/expired) — reject; do not run the ceremony.
    Disarmed,
    /// A window IS armed but **bound to a different fingerprint** — reject WITHOUT consuming it, so
    /// an unrelated (attacker) fingerprint can't burn the operator's armed window (#9).
    BoundToOther,
    /// Proceed: the PIN to run the ceremony with (the window is unbound, or bound to this fingerprint).
    Pin(String),
}

fn random_pin() -> String {
    use rand::Rng;
    format!("{:04}", rand::thread_rng().gen_range(0..10_000u32))
}

/// A snapshot of the arming window for the management API: `(armed, pin, expires_in_secs)`.
pub(super) type ArmSnapshot = (bool, Option<String>, Option<u64>);

/// The arming-PIN window behind a [`Mutex`].
pub(super) struct ArmState {
    arm: Mutex<Armed>,
}

impl ArmState {
    /// A fresh window. If `arm_at_start` (the CLI `--allow-pairing`/`--require-pairing` flags), arm
    /// immediately with `fixed_pin` (or a fresh random PIN) and **no expiry** — back-compat with the
    /// headless CLI flow. Otherwise disarmed.
    pub(super) fn new(arm_at_start: bool, fixed_pin: Option<String>) -> ArmState {
        let arm = if arm_at_start {
            Armed {
                pin: Some(fixed_pin.unwrap_or_else(random_pin)),
                expires_at: None,
                bound_fp: None,
            }
        } else {
            Armed::default()
        };
        ArmState {
            arm: Mutex::new(arm),
        }
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. If `bound_fp` is `Some`, the window is
    /// bound to that device fingerprint: only a pairing attempt from it consumes the window, so an
    /// unrelated (attacker) fingerprint can neither pair nor burn the window (#9). Returns the PIN.
    pub(super) fn arm_for(&self, ttl: Duration, bound_fp: Option<String>) -> String {
        let pin = random_pin();
        *self.arm.lock().unwrap() = Armed {
            pin: Some(pin.clone()),
            expires_at: Some(Instant::now() + ttl),
            bound_fp,
        };
        pin
    }

    /// Resolve the PIN for an attempt from `client_fp_hex`, honoring fingerprint binding (#9):
    /// `Disarmed` if no window is armed; `BoundToOther` if a window is armed but bound to a different
    /// fingerprint (the caller MUST reject without consuming it); else `Pin` to run the ceremony.
    pub(super) fn pin_for_attempt(&self, client_fp_hex: &str) -> PinAttempt {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        match &arm.pin {
            None => PinAttempt::Disarmed,
            Some(pin) => match &arm.bound_fp {
                Some(bound) if !bound.eq_ignore_ascii_case(client_fp_hex) => {
                    PinAttempt::BoundToOther
                }
                _ => PinAttempt::Pin(pin.clone()),
            },
        }
    }

    /// Disarm pairing (no new ceremonies accepted).
    pub(super) fn disarm(&self) {
        *self.arm.lock().unwrap() = Armed::default();
    }

    /// Expire a timed window if its deadline passed (called under the lock before any read).
    fn expire(arm: &mut Armed) {
        if let Some(t) = arm.expires_at {
            if Instant::now() >= t {
                *arm = Armed::default();
            }
        }
    }

    /// The current valid PIN, or `None` if disarmed/expired. The QUIC ceremony reads this
    /// per-attempt, so a window that lapsed mid-connection no longer pairs.
    pub(super) fn current_pin(&self) -> Option<String> {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        arm.pin.clone()
    }

    /// A snapshot for the management API: `(armed, pin, expires_in_secs)`.
    pub(super) fn snapshot(&self) -> ArmSnapshot {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        let expires_in_secs = arm
            .expires_at
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs());
        (arm.pin.is_some(), arm.pin.clone(), expires_in_secs)
    }
}
