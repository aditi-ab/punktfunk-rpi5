//! Mode-conflict **admission** (`design/display-management.md`).
//!
//! When a *different* client connects while a session is live, `mode_conflict`
//! decides before Welcome / RTSP so the client gets a typed answer, not a
//! mid-build failure:
//!
//! * `separate` — fresh display at the requested mode (Linux default).
//! * `join` — admit at the live display's mode (Welcome carries the real mode).
//! * `steal` — signal victim stop flags, wait the release grace, then serve.
//! * `reject` — handshake error naming the live mode and client.
//!
//! [`register`] exposes identity + mode + stop flag; the session drops
//! [`LiveGuard`] on end. [`decide`] is pure over that slice.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::policy::{self, ModeConflict};

#[derive(Clone)]
pub struct LiveSession {
    id: u64,
    /// Cert fingerprint; `None` = anonymous / no client cert.
    pub identity: Option<[u8; 32]>,
    pub mode: (u32, u32, u32),
    /// Signaled to preempt this session on `steal`.
    pub stop: Arc<AtomicBool>,
    /// Client label interpolated into `reject` messages.
    pub label: String,
}

#[derive(Debug)]
pub enum Admission {
    Separate,
    /// Admit at this live mode; Welcome must carry it, not the request.
    Join((u32, u32, u32)),
    /// Victim stop flags; caller signals them and waits the release grace.
    Steal(Vec<Arc<AtomicBool>>),
    Reject(String),
}

fn table() -> &'static Mutex<Vec<LiveSession>> {
    static T: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Two identities match only when both are `Some` and equal. Anonymous
/// (`None`) never matches, so two anonymous clients conflict under
/// `steal` / `reject`.
fn same_client(a: Option<[u8; 32]>, b: Option<[u8; 32]>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

/// Pure over the live slice. A conflict is a live session owned by a
/// *different* client; a same-client reconnect is always `Separate` here
/// and preempts downstream.
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
        return Admission::Separate;
    }
    match conflict {
        ModeConflict::Separate => Admission::Separate,
        // Oldest other session: the established primary the desktop is built on.
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

/// Console `mode_conflict`, default `Separate` when unconfigured.
///
/// On Windows this still maps `separate` (including the unconfigured
/// default) to `reject` unless `PUNKTFUNK_WIN_SEPARATE=1`. Each identity
/// already has its own monitor slot, so the flip is a validation hatch,
/// not a correctness guard (`design/windows-parallel-virtual-displays.md`).
/// `join` / `steal` stay explicit opt-ins. Linux is real `separate`.
/// Shared by the native and GameStream admission paths.
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

/// [`effective_conflict`] + [`decide`] against the live set. When
/// `Separate` would mint a second display, also apply `max_displays` and
/// encoder headroom. Fail-closed: a display we cannot afford is declined
/// here, never admitted then degrading a live sibling
/// (`design/windows-parallel-virtual-displays.md`).
pub fn admit(req_identity: Option<[u8; 32]>) -> Admission {
    // Scope the table lock to `decide` only. Budget checks call
    // `manager::snapshot` (holds `state` across DDC, SetupAPI, 3 s
    // activation ladders) and an NVENC probe; holding the process-wide
    // table across those stalls every connect, disconnect, and mgmt read.
    let (decision, any_live) = {
        let live = table().lock().unwrap();
        (
            decide(effective_conflict(), req_identity, &live),
            !live.is_empty(),
        )
    };
    let _ = any_live; // used only by the cfg-gated budget blocks below

    // Enforce `max_displays` here, not in `acquire`. A mid-stream rebuild
    // (capture loss, Game↔Desktop) mints the new display before dropping
    // the old one, so a ceiling there would count the session against
    // itself and refuse recovery.
    #[cfg(target_os = "linux")]
    if matches!(decision, Admission::Separate) && any_live {
        // Linux reuse key includes the client-supplied mode, so a reconnect
        // at a different resolution misses reuse and would mint unbounded
        // compositor outputs.
        let max = policy::prefs().get().effective().max_displays;
        let live = super::registry::live_display_count();
        if live >= max {
            return Admission::Reject(format!(
                "host display budget exhausted: {live} display(s) live/kept, max_displays = {max}"
            ));
        }
    }
    #[cfg(windows)]
    if matches!(decision, Admission::Separate) && any_live {
        let max = policy::prefs().get().effective().max_displays;
        let slots = super::manager::snapshot().len() as u32;
        if slots >= max {
            return Admission::Reject(format!(
                "host display budget exhausted: {slots} display(s) live/kept, max_displays = {max}"
            ));
        }
        if !pf_encode::can_open_another_session() {
            return Admission::Reject(
                "host encoder budget exhausted: no NVENC session headroom for another display"
                    .to_string(),
            );
        }
    }
    decision
}

/// Stop flags of live sessions owned by `req_identity` (its own zombies).
/// Testable over a slice; the public fn locks the global table.
fn same_identity_stops(
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Vec<Arc<AtomicBool>> {
    live.iter()
        .filter(|s| same_client(s.identity, req_identity))
        .map(|s| Arc::clone(&s.stop))
        .collect()
}

/// Stop flags of this client's still-live session(s).
///
/// A new connection from an already-registered identity is a reconnect:
/// the old session is a zombie whose QUIC idle timer has not fired
/// (`max_idle_timeout`, seconds). The caller signals these flags and waits
/// the release grace so this reconnect reuses the kept display instead of
/// landing on a second one. Anonymous (`None`) never matches. Call before
/// [`admit`] and before this session [`register`]s, so only a *prior*
/// session's flag is signaled.
pub fn preempt_same_identity(req_identity: Option<[u8; 32]>) -> Vec<Arc<AtomicBool>> {
    same_identity_stops(req_identity, &table().lock().unwrap())
}

/// Register an admitted session; the guard removes it on drop. Call after
/// [`admit`] (so a session never conflicts with itself) once mode and stop
/// are known.
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
        let live = [sess(None, (1920, 1080, 60))];
        assert!(matches!(
            decide(ModeConflict::Reject, None, &live),
            Admission::Reject(_)
        ));
    }

    #[test]
    fn same_identity_stops_targets_own_zombie_only() {
        let live = [
            sess(Some(1), (2560, 1440, 60)),
            sess(Some(2), (1920, 1080, 60)),
        ];
        assert_eq!(same_identity_stops(fp(1), &live).len(), 1);
        assert_eq!(same_identity_stops(fp(3), &live).len(), 0);
        assert_eq!(same_identity_stops(None, &live).len(), 0);
    }

    #[test]
    fn join_targets_the_oldest_other_session() {
        let live = [
            sess(Some(1), (3840, 2160, 60)),
            sess(Some(2), (1280, 720, 120)),
        ];
        assert!(matches!(
            decide(ModeConflict::Join, fp(3), &live),
            Admission::Join((3840, 2160, 60))
        ));
    }
}
