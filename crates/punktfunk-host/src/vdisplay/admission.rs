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

/// Resolve the effective decision for a connecting session: read the console `mode_conflict` policy
/// (default `Separate` when unconfigured — no behavior change) and [`decide`] against the live set.
pub fn admit(req_identity: Option<[u8; 32]>) -> Admission {
    let conflict = policy::prefs()
        .configured_effective()
        .map(|e| e.mode_conflict)
        .unwrap_or(ModeConflict::Separate);
    decide(conflict, req_identity, &table().lock().unwrap())
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
