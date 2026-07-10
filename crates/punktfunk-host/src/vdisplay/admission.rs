//! Mode-conflict **admission** (design: `design/display-management.md` §5.3, Stage 4). When a
//! *different* client connects while another client's session is already live, the `mode_conflict`
//! policy decides what happens — BEFORE the Welcome / RTSP launch, so the client gets an honest answer
//! instead of a mid-build failure:
//!
//! * `separate` — proceed on a fresh display at the requested mode (today's Linux multi-view / the
//!   default; no behavior change unconfigured).
//! * `join` — admit at the live display's mode (honest-downgrade: the Welcome carries the real mode).
//! * `steal` — signal the victim session(s)' stop flag(s), wait the release grace, then serve.
//! * `reject` — refuse with a typed handshake error naming the live mode + client.
//!
//! A **live-session registry** ([`register`]) lets the decision see the current sessions (identity +
//! mode + stop flag); each session registers once admitted and drops its [`LiveGuard`] on end. The
//! decision itself ([`decide`]) is pure over a session slice, so it is unit-tested exhaustively.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::vdisplay::policy::{self, ModeConflict};

/// A currently-live session, as admission sees it.
#[derive(Clone)]
pub struct LiveSession {
    id: u64,
    /// The owning client's cert fingerprint (`None` = anonymous / no client cert presented).
    pub identity: Option<[u8; 32]>,
    pub mode: (u32, u32, u32),
    /// The session's stop flag — signaled to preempt it on `steal`.
    pub stop: Arc<AtomicBool>,
    /// Short client label for `reject` messages.
    pub label: String,
}

/// The admission outcome for a connecting session.
#[derive(Debug)]
pub enum Admission {
    /// No conflict / `separate`: proceed on a fresh display at the requested mode.
    Separate,
    /// `join`: admit at this (live) mode — share the existing display (honest-downgrade).
    Join((u32, u32, u32)),
    /// `steal`: signal these victim stop flags, wait the release grace, then proceed at the requested mode.
    Steal(Vec<Arc<AtomicBool>>),
    /// `reject`: refuse with this reason (host-busy + live mode + client label).
    Reject(String),
}

fn table() -> &'static Mutex<Vec<LiveSession>> {
    static T: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Two identities are the same client iff both are present and equal. Anonymous (`None`) never
/// matches — we can't prove it's the same client, so two anonymous clients are treated as distinct
/// (each conflicts), which is the safe side for `steal`/`reject`.
fn same_client(a: Option<[u8; 32]>, b: Option<[u8; 32]>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

/// The mode-conflict decision, pure over the live-session slice (so it's unit-testable). A conflict is
/// a live session owned by a DIFFERENT client — a same-client reconnect adopts / reconfigures its own
/// display and never conflicts (so it always resolves to `Separate` here and preempts downstream).
pub fn decide(
    conflict: ModeConflict,
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Admission {
    let others: Vec<&LiveSession> = live
        .iter()
        .filter(|s| !same_client(s.identity, req_identity))
        .collect();
    if others.is_empty() {
        return Admission::Separate; // no other client is live → no conflict
    }
    match conflict {
        ModeConflict::Separate => Admission::Separate,
        // Join at the OLDEST other session's mode (the established "primary" the desktop is built on).
        ModeConflict::Join => Admission::Join(others[0].mode),
        ModeConflict::Steal => {
            Admission::Steal(others.iter().map(|s| Arc::clone(&s.stop)).collect())
        }
        ModeConflict::Reject => {
            let v = others[0];
            Admission::Reject(format!(
                "host busy: streaming {}x{}@{} to {}",
                v.mode.0, v.mode.1, v.mode.2, v.label
            ))
        }
    }
}

/// The effective `mode_conflict` policy for THIS host: the console value (default `Separate` when
/// unconfigured), with the **Windows default applied**. On Windows `separate` — including the
/// unconfigured default — still resolves to **`reject`** UNLESS the Stage-W3 validation hatch
/// `PUNKTFUNK_WIN_SEPARATE=1` is set (`design/windows-parallel-virtual-displays.md` §4.3 — the
/// default flips to real `separate` in W5, after the on-glass matrix is green).
///
/// The historical `reject` override guarded against a real wedge: two concurrent Windows sessions
/// both drove the SAME pf-vdisplay monitor's single-capturer IDD-push channel
/// ("newest-delivery-wins"), which froze the live client and could wedge the driver. With the
/// manager's slot map (Stage W1) that wedge is structurally impossible — a second identity gets its
/// OWN slot → own monitor → own sealed ring — so the override is now a validation-soak guard, not a
/// correctness one. `join`/`steal` stay as explicit opt-ins. Linux keeps `separate` (real
/// multi-view). Shared by the native + GameStream admission paths.
pub fn effective_conflict() -> ModeConflict {
    let conflict = policy::prefs()
        .configured_effective()
        .map(|e| e.mode_conflict)
        .unwrap_or(ModeConflict::Separate);
    #[cfg(windows)]
    if matches!(conflict, ModeConflict::Separate)
        && !std::env::var("PUNKTFUNK_WIN_SEPARATE").is_ok_and(|v| v == "1")
    {
        return ModeConflict::Reject;
    }
    conflict
}

/// Resolve the admission decision for a connecting native session: [`effective_conflict`] + [`decide`]
/// against the live set, then — when a SECOND display would actually be created (`Separate` with
/// other clients live, Windows) — the Stage-W3 resource budgets: `max_displays` across the manager's
/// live/kept slots, and the encoder's session headroom. Fail-closed at admission
/// (`design/windows-parallel-virtual-displays.md` §2.5): a display we can't afford is DECLINED here,
/// never admitted-then-degrading a live sibling.
pub fn admit(req_identity: Option<[u8; 32]>) -> Admission {
    let live = table().lock().unwrap();
    let decision = decide(effective_conflict(), req_identity, &live);
    #[cfg(windows)]
    if matches!(decision, Admission::Separate) && !live.is_empty() {
        let max = policy::prefs().get().effective().max_displays;
        let slots = super::manager::snapshot().len() as u32;
        if slots >= max {
            return Admission::Reject(format!(
                "host display budget exhausted: {slots} display(s) live/kept, max_displays = {max}"
            ));
        }
        if !crate::encode::can_open_another_session() {
            return Admission::Reject(
                "host encoder budget exhausted: no NVENC session headroom for another display"
                    .to_string(),
            );
        }
    }
    decision
}

/// Pure core of [`preempt_same_identity`]: the stop flags of live sessions owned by the SAME client
/// as `req_identity` (its own zombies). Testable over a slice (the public fn locks the global table).
fn same_identity_stops(
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Vec<Arc<AtomicBool>> {
    live.iter()
        .filter(|s| same_client(s.identity, req_identity))
        .map(|s| Arc::clone(&s.stop))
        .collect()
}

/// Preempt this reconnecting client's OWN still-live session(s). A client has at most one live
/// session, so a new connection from an already-registered identity is a **reconnect** — the old
/// session is a zombie whose QUIC idle timer hasn't fired yet (an unwanted disconnect is only
/// declared dead after `max_idle_timeout`, ~seconds later). Return its stop flag(s) so the caller
/// signals them and waits the release grace: the zombie tears its display down, which (keep-alive on)
/// lingers, and THIS reconnect **reuses** that kept display instead of landing on a fresh SECOND one
/// (the "thrown onto a second display while the old one keeps streaming" bug). Anonymous (`None`)
/// never matches — same limitation as `steal`/`reject`. Call this BEFORE [`admit`] and before this
/// session registers itself, so it only ever signals a *prior* session's flag, never its own.
pub fn preempt_same_identity(req_identity: Option<[u8; 32]>) -> Vec<Arc<AtomicBool>> {
    same_identity_stops(req_identity, &table().lock().unwrap())
}

/// Register a now-admitted, live session; the returned guard removes it on drop (session end). Call
/// AFTER [`admit`] (so a session never conflicts with itself) and once the mode + stop flag are known.
pub fn register(
    identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
    label: String,
) -> LiveGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    table().lock().unwrap().push(LiveSession {
        id,
        identity,
        mode,
        stop,
        label,
    });
    LiveGuard { id }
}

/// RAII handle: removes its live-session entry from the registry on drop (session end).
pub struct LiveGuard {
    id: u64,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        table().lock().unwrap().retain(|s| s.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(identity: Option<u8>, mode: (u32, u32, u32)) -> LiveSession {
        LiveSession {
            id: 0,
            identity: identity.map(|n| {
                let mut f = [0u8; 32];
                f[0] = n;
                f
            }),
            mode,
            stop: Arc::new(AtomicBool::new(false)),
            label: "peer".into(),
        }
    }
    fn fp(n: u8) -> Option<[u8; 32]> {
        let mut f = [0u8; 32];
        f[0] = n;
        Some(f)
    }

    #[test]
    fn no_live_session_is_always_separate() {
        for c in [
            ModeConflict::Separate,
            ModeConflict::Join,
            ModeConflict::Steal,
            ModeConflict::Reject,
        ] {
            assert!(matches!(decide(c, fp(1), &[]), Admission::Separate));
        }
    }

    #[test]
    fn same_client_never_conflicts() {
        let live = [sess(Some(1), (2560, 1440, 60))];
        // Even under reject/steal, the SAME client (fp 1) reconnecting is not a conflict.
        assert!(matches!(
            decide(ModeConflict::Reject, fp(1), &live),
            Admission::Separate
        ));
        assert!(matches!(
            decide(ModeConflict::Steal, fp(1), &live),
            Admission::Separate
        ));
    }

    #[test]
    fn different_client_applies_policy() {
        let live = [sess(Some(1), (2560, 1440, 60))];
        assert!(matches!(
            decide(ModeConflict::Separate, fp(2), &live),
            Admission::Separate
        ));
        assert!(matches!(
            decide(ModeConflict::Join, fp(2), &live),
            Admission::Join((2560, 1440, 60))
        ));
        assert!(matches!(
            decide(ModeConflict::Steal, fp(2), &live),
            Admission::Steal(v) if v.len() == 1
        ));
        assert!(matches!(
            decide(ModeConflict::Reject, fp(2), &live),
            Admission::Reject(r) if r.contains("2560x1440@60")
        ));
    }

    #[test]
    fn two_anonymous_clients_conflict() {
        // Anonymous (None) can't be proven same-client, so a second anon client DOES conflict.
        let live = [sess(None, (1920, 1080, 60))];
        assert!(matches!(
            decide(ModeConflict::Reject, None, &live),
            Admission::Reject(_)
        ));
    }

    #[test]
    fn same_identity_stops_targets_own_zombie_only() {
        let live = [
            sess(Some(1), (2560, 1440, 60)), // this client's prior (zombie) session
            sess(Some(2), (1920, 1080, 60)), // a different client
        ];
        // Reconnecting as client 1 → its own zombie's stop is returned (to preempt), not client 2's.
        assert_eq!(same_identity_stops(fp(1), &live).len(), 1);
        // A client with no prior session (fp 3) has nothing of its own to preempt.
        assert_eq!(same_identity_stops(fp(3), &live).len(), 0);
        // Anonymous never matches — we can't prove it's the same client.
        assert_eq!(same_identity_stops(None, &live).len(), 0);
    }

    #[test]
    fn join_targets_the_oldest_other_session() {
        let live = [
            sess(Some(1), (3840, 2160, 60)), // oldest
            sess(Some(2), (1280, 720, 120)),
        ];
        assert!(matches!(
            decide(ModeConflict::Join, fp(3), &live),
            Admission::Join((3840, 2160, 60))
        ));
    }
}
