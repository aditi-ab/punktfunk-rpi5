//! `/api/v1/actions` — the host-action registry (`design/host-actions.md`): discovery of the
//! actions this host offers *as seen by the caller*, and the id-only invoke. v1 ships the three
//! `power.*` built-ins; future host- or plugin-provided actions reuse these two routes, so
//! clients that render the discovery generically need no release to pick them up.
//!
//! Lane split (see `auth`): the **admin bearer** reaches both routes with everything permitted
//! (the console is the owner surface); a **paired streaming cert** reaches both too — the
//! lane's third write route after `POST /client-logs` and the whole point of the design
//! ("Sleep host" from the couch, out of session) — but invoke demands the `GRANT_POWER` bit,
//! re-read via `effective(fp, now)` PER REQUEST so console edits, expiry and unpair apply to
//! the very next call. The **plugin token** gets neither route: a plugin that wants to
//! power-manage the host is an operator-hook story, not a shared-token capability.
//!
//! The invoke invariant (the `host-update` recipe): the request is a trigger — the id selects a
//! fixed host-side behavior, the body is empty, and **no request field ever reaches the
//! privileged path**. Ordering on accept: reply `202` → end every session (typed
//! `RejectReason::HostPower` close, which also drops our own sleep-inhibit hold) → ~1 s grace
//! so the reply flushes before the NIC goes away → act.

use super::auth::AuthLane;
use super::shared::*;
use crate::gamestream::tls::PeerCertFingerprint;
use crate::power::PowerVerb;
use axum::Extension;
use std::sync::atomic::{AtomicBool, Ordering};

/// One built-in action: a stable id (`<group>.<verb>` — `plugin:<id>:<verb>` is reserved for
/// plugin-provided actions later) bound to a fixed executor. The registry is code on purpose:
/// no persistence, nothing a request can add to.
struct Builtin {
    id: &'static str,
    /// English display title — clients map KNOWN ids to their own localized strings and fall
    /// back to this for ids they don't know yet.
    title: &'static str,
    /// Two-press confirm hint for client UIs (reboot/shutdown lose state; sleep is reversible).
    danger: bool,
    verb: PowerVerb,
}

/// v1: the three machine-power verbs, all under the `power` group / `GRANT_POWER` bit.
const BUILTINS: [Builtin; 3] = [
    Builtin {
        id: "power.sleep",
        title: "Sleep host",
        danger: false,
        verb: PowerVerb::Sleep,
    },
    Builtin {
        id: "power.reboot",
        title: "Restart host",
        danger: true,
        verb: PowerVerb::Reboot,
    },
    Builtin {
        id: "power.shutdown",
        title: "Shut down host",
        danger: true,
        verb: PowerVerb::Shutdown,
    },
];

/// One action as the caller sees it (`GET /actions`).
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionInfo {
    /// Stable action id (`power.sleep`, …) — the invoke path parameter.
    #[schema(example = "power.sleep")]
    pub id: String,
    /// Display title. Clients localize known ids and fall back to this for unknown ones.
    pub title: String,
    /// Action group (`power` for the built-ins).
    pub group: String,
    /// Whether a client UI should double-confirm (the action loses state — reboot/shutdown).
    pub danger: bool,
    /// Whether this host can run it right now (platform probe — a VM that can't S3 lists
    /// sleep as unavailable rather than offering a dead switch).
    pub available: bool,
    /// Why it is unavailable, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Whether THIS caller may invoke it (admin lane: always; cert lane: the `GRANT_POWER`
    /// bit of the device's live access mask).
    pub permitted: bool,
}

/// `GET /actions` response.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionList {
    pub actions: Vec<ActionInfo>,
}

/// Whether this caller may invoke the `power.*` actions: the admin bearer always; a paired
/// cert exactly when its live mask (re-read NOW — expiry- and edit-aware) carries
/// [`punktfunk_core::quic::GRANT_POWER`]. Any other lane: no.
fn power_permitted(st: &MgmtState, lane: AuthLane, fp: Option<&str>) -> bool {
    match lane {
        AuthLane::Admin => true,
        AuthLane::Cert => fp.is_some_and(|fp| {
            st.native
                .as_ref()
                .and_then(|n| n.effective(fp, unix_now()))
                .is_some_and(|mask| mask & punktfunk_core::quic::GRANT_POWER != 0)
        }),
        AuthLane::Plugin | AuthLane::Public => false,
    }
}

/// Host wall clock, unix seconds (the clock the stored access deadlines are expressed in).
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// List host actions
///
/// The actions this host offers, as seen by the caller: platform availability (with the honest
/// reason when something can't run) and whether THIS caller is permitted to invoke each one.
/// Admin lane: everything permitted. Paired-cert lane: permission follows the device's live
/// access mask (the Host power grant). Clients render rows generically — unknown ids still
/// work with the server-supplied title.
#[utoipa::path(
    get,
    path = "/actions",
    tag = "actions",
    operation_id = "listActions",
    responses(
        (status = OK, description = "The actions, per-caller", body = ActionList),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn list_actions(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
) -> Json<ActionList> {
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    let permitted = power_permitted(&st, lane, fp);
    // The probes are D-Bus round trips on Linux — off the async worker, all three in one hop.
    let probed = tokio::task::spawn_blocking(|| BUILTINS.map(|b| crate::power::probe(b.verb)))
        .await
        .expect("power probe task panicked");
    let actions = BUILTINS
        .iter()
        .zip(probed)
        .map(|(b, avail)| ActionInfo {
            id: b.id.into(),
            title: b.title.into(),
            group: "power".into(),
            danger: b.danger,
            available: crate::power::supported() && avail.available,
            unavailable_reason: if crate::power::supported() {
                avail.reason
            } else {
                Some("not supported on this host platform".into())
            },
            permitted,
        })
        .collect();
    Json(ActionList { actions })
}

/// One action is in flight host-wide (`409 busy` otherwise) — the actions themselves end the
/// conversation, so this is all the rate limiting v1 needs.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Denials are logged once per (fingerprint, action) per boot — a retrying client must not turn
/// the host log into the DoS (the `GrantDrops` discipline).
fn log_denial_once(fp: &str, action: &str, device: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static LOGGED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let mut set = LOGGED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.insert((fp.to_string(), action.to_string())) {
        tracing::info!(
            device,
            fingerprint = fp,
            action,
            "denied a host action — this device's access lacks the Host power grant \
             (further denials of this pair are silent this boot)"
        );
    }
}

/// Invoke a host action
///
/// Runs one action by id — empty body, no parameters: the id selects a fixed host-side
/// behavior, and nothing in the request reaches the privileged path. On `202` the host first
/// ends every streaming session cleanly (clients see a typed "the host is going to sleep /
/// shutting down" close), waits ~1 s so this response flushes, then acts.
///
/// Paired-cert callers need the **Host power** grant, and are refused (`409`) while another
/// device's session is live — a granted guest cannot yank the host out from under the owner
/// mid-stream. The admin console is never blocked (it warns instead). One action runs at a
/// time host-wide.
#[utoipa::path(
    post,
    path = "/actions/{id}",
    tag = "actions",
    operation_id = "invokeAction",
    params(("id" = String, Path, description = "Action id (`power.sleep`, `power.reboot`, `power.shutdown`)")),
    responses(
        (status = ACCEPTED, description = "Accepted — sessions are being ended and the action follows in about a second"),
        (status = FORBIDDEN, description = "This caller's access does not include this action (no Host power grant)", body = ApiError),
        (status = NOT_FOUND, description = "Unknown action id", body = ApiError),
        (status = CONFLICT, description = "Refused: an action is already in flight, another device's session is live (cert lane), or the platform said no (a foreign sleep inhibitor, a second local user, …)", body = ApiError),
        (status = NOT_IMPLEMENTED, description = "This host platform has no executor for it (macOS host)", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn invoke_action(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
    Path(id): Path<String>,
) -> Response {
    let Some(builtin) = BUILTINS.iter().find(|b| b.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "unknown action id");
    };
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    // The invoking device's roster identity (cert lane) — for the audit line and the event.
    let device = fp.and_then(|fp| {
        st.native.as_ref().and_then(|n| {
            n.list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(fp))
                .map(|c| crate::events::DeviceRef {
                    name: c.name,
                    fingerprint: c.fingerprint,
                    plane: crate::events::Plane::Native,
                })
        })
    });
    if !power_permitted(&st, lane, fp) {
        let device_name = device.as_ref().map(|d| d.name.as_str()).unwrap_or("");
        log_denial_once(fp.unwrap_or(""), builtin.id, device_name);
        return api_error(
            StatusCode::FORBIDDEN,
            "this device's access does not include host power — ask the host's operator to \
             enable the Host power grant",
        );
    }
    if !crate::power::supported() {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "host power actions are not supported on this host platform",
        );
    }
    // Busy policy (design §5.5): another device's LIVE session blocks a cert-lane invoke —
    // your own session doesn't (you know what you asked for), and the admin console is never
    // blocked (it is the owner surface; the console warns before sending). A GameStream stream
    // is always another device on this policy: its cert identity is never the native one.
    if lane == AuthLane::Cert {
        let others_native = crate::session_status::other_client_live(fp.unwrap_or(""));
        let gamestream = st.app.streaming.load(Ordering::SeqCst);
        if others_native || gamestream {
            return api_error(
                StatusCode::CONFLICT,
                "blocked: another device is streaming from this host right now",
            );
        }
    }
    let verb = builtin.verb;
    let avail = tokio::task::spawn_blocking(move || crate::power::probe(verb))
        .await
        .expect("power probe task panicked");
    if !avail.available {
        return api_error(
            StatusCode::CONFLICT,
            &format!(
                "blocked: {}",
                avail.reason.as_deref().unwrap_or("the platform said no")
            ),
        );
    }
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return api_error(
            StatusCode::CONFLICT,
            "a host action is already in flight — the host is on its way down",
        );
    }
    let invoker = device
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "the host console".into());
    tracing::info!(action = builtin.id, invoked_by = %invoker, "host action accepted");
    crate::events::emit(crate::events::EventKind::ActionInvoked {
        id: builtin.id.into(),
        device: device.clone(),
        outcome: "accepted".into(),
    });
    // Reply first, act after: the 202 must flush before the NIC goes away. The typed close
    // fires now so every paired session ends as "the host is going to sleep", the quit-flavored
    // stop catches the rest (anonymous sessions, belt for the rest), and the compat plane's
    // teardown runs its own path.
    let id_owned: String = builtin.id.into();
    let app = st.app.clone();
    tokio::spawn(async move {
        crate::power::set_closing(true);
        crate::session_status::stop_all_quit();
        let _ = app.quit_session("host power action");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // Nothing may outlive the box going under — the displays go first.
        drain_displays().await;
        // Our own suspend veto must be gone before logind is asked (we never hold
        // -ignore-inhibit rights): synchronous belt over the session-teardown braces.
        crate::sleep_inhibit::release_now();
        let outcome = tokio::task::spawn_blocking(move || crate::power::act(verb))
            .await
            .unwrap_or_else(|e| Err(format!("executor task panicked: {e}")));
        match outcome {
            // A reboot/shutdown ends this process shortly; a sleep resumes here on wake.
            Ok(()) => tracing::info!(action = %id_owned, "host power action handed to the OS"),
            Err(e) => {
                tracing::warn!(action = %id_owned, error = %e, "host power action FAILED");
                crate::events::emit(crate::events::EventKind::ActionInvoked {
                    id: id_owned,
                    device,
                    outcome: format!("failed: {e}"),
                });
            }
        }
        crate::power::set_closing(false);
        IN_FLIGHT.store(false, Ordering::SeqCst);
    });
    StatusCode::ACCEPTED.into_response()
}

/// How long a power action waits for the signaled sessions to drop their virtual displays before
/// it goes under anyway. Over the 1.5 s release grace `native/handshake.rs` gives the same "let the
/// session drop its display" wait, and short enough that a wedged teardown cannot strand someone
/// waiting for the box to sleep.
const DISPLAY_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
const DISPLAY_DRAIN_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// One sweep's plan over a display snapshot: the `lingering` slots to release now, and how many
/// displays we are still waiting on. **Pinned displays are exempt from both** — see
/// [`drain_displays`]. Pure so the exemption is testable; the registry is a process global.
fn drain_plan<'a>(displays: impl Iterator<Item = (&'a str, u64)>) -> (Vec<u64>, usize) {
    let (mut release, mut waiting) = (Vec::new(), 0usize);
    for (state, slot) in displays {
        match state {
            "lingering" => {
                release.push(slot);
                waiting += 1;
            }
            // Active = a session still tearing down; wait it out. Pinned = deliberate, leave it.
            "pinned" => {}
            _ => waiting += 1,
        }
    }
    (release, waiting)
}

/// Tear the virtual displays down BEFORE the box goes under.
///
/// [`crate::session_status::stop_all_quit`] above marks the teardown deliberate, so the display is
/// meant to skip its keep-alive linger — but that teardown is ASYNC: the stream loops have to
/// notice the flag and drop the lease, and the reply-flush grace above is shorter than the 1.5 s
/// the same wait gets in `native/handshake.rs`.
///
/// ⚠ Going under on top of a display that is still up is NOT a transient. The linger deadline is a
/// `std::time::Instant`, and on Linux that clock does not advance while the box is suspended — so
/// the box wakes with the stale display standing and its window unspent. On a SteamOS managed
/// takeover that display is a headless gamescope session holding the box's own panels DPMS-off.
///
/// [`crate::vdisplay::registry::release`] refuses ACTIVE displays (releasing one with live sessions
/// is session management), so the poll IS the wait: each tick sweeps whatever has reached
/// `lingering`, and we return once nothing but pinned displays is left.
///
/// 🛑 **A Pinned display is left standing on purpose.** `KeepAlive::Forever` means "until host
/// shutdown or an explicit `POST /display/release`" — a disconnect does not free it and neither
/// does a different client connecting, because reuse is keyed on backend + mode. On gamescope's
/// bare spawn that keep-alive also covers the nested session AND ITS GAME, so force-releasing a pin
/// here would kill a running game on every sleep, which is the exact thing the operator pinned it
/// to avoid. The stale-display hazard above therefore still applies under `forever` — that is the
/// operator's standing choice, and the info line below is what makes it legible in a field log.
async fn drain_displays() {
    let deadline = std::time::Instant::now() + DISPLAY_DRAIN_BUDGET;
    loop {
        let snap = crate::vdisplay::registry::snapshot();
        let (release, waiting) =
            drain_plan(snap.displays.iter().map(|d| (d.state.as_str(), d.slot)));
        let pinned = snap.displays.len() - waiting;
        if waiting == 0 {
            if pinned > 0 {
                tracing::info!(
                    pinned,
                    "pinned display(s) left standing across the power action (keep-alive is \
                     `forever`) — free them with POST /display/release if a wake lands dark"
                );
            }
            return;
        }
        // Checked BEFORE the release so a slot that refuses to clear cannot spin past the budget.
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                displays = waiting,
                "virtual display(s) still up at the power-action budget — going under anyway; \
                 a wake may land on a stale display"
            );
            return;
        }
        if !release.is_empty() {
            // `release` tears the display down INLINE — it kills the gamescope session and runs
            // the topology/DPMS restores — so it must not run on a runtime worker, for the same
            // reason `power::act` below is spawned blocking.
            let _ = tokio::task::spawn_blocking(move || {
                for slot in release {
                    crate::vdisplay::registry::release(Some(slot));
                }
            })
            .await;
        }
        tokio::time::sleep(DISPLAY_DRAIN_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{drain_displays, drain_plan, DISPLAY_DRAIN_BUDGET};

    /// The common case is "Sleep host" with nobody streaming: no display is registered, so the
    /// drain must cost nothing. An inverted emptiness check here would add the whole budget to
    /// every power action on every host.
    #[tokio::test]
    async fn drain_displays_returns_at_once_when_no_display_is_up() {
        let t0 = std::time::Instant::now();
        drain_displays().await;
        assert!(
            t0.elapsed() < DISPLAY_DRAIN_BUDGET / 2,
            "drain burned the budget with no displays up: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn lingering_is_released_active_is_waited_out_pinned_is_left_alone() {
        let (release, waiting) =
            drain_plan([("lingering", 1u64), ("active", 2), ("pinned", 3)].into_iter());
        assert_eq!(release, vec![1], "only a lingering display is released");
        assert_eq!(
            waiting, 2,
            "active + lingering are waited on, pinned is not"
        );
    }

    /// The regression this guards: force-releasing a pin would kill the nested gamescope session
    /// and its running game on every sleep. A box with nothing BUT pinned displays must drain
    /// clean, releasing none of them.
    #[test]
    fn a_pinned_only_box_drains_clean_and_releases_nothing() {
        let (release, waiting) = drain_plan([("pinned", 7u64), ("pinned", 8)].into_iter());
        assert!(release.is_empty(), "a pin must survive a power action");
        assert_eq!(waiting, 0, "pinned displays must not hold the drain open");
    }
}
