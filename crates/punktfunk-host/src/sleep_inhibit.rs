//! Session-scoped suspend/idle inhibition: while at least one client is streaming **and is not
//! sending input**, the host holds a logind `sleep:idle` BLOCK inhibitor so the box doesn't
//! auto-suspend out from under a passive viewer. Remote INPUT resets the compositor's idle timers,
//! but a video-only viewer sends none — observed live on a SteamOS Game-Mode host, which s2idled
//! mid-stream-day and dropped off the network (and, in a VM with GPU passthrough, never woke
//! again). Refcounted across planes (native sessions + GameStream media): the first hold acquires,
//! the last drop releases. Best-effort — no logind (containers, non-systemd boxes) logs once and
//! streams on. Off Linux this is a no-op: macOS/Windows hosts manage their own power assertions.
//!
//! **The quiet gate is the point, and it is not an optimisation.** A `block` lock on `sleep`
//! refuses EVERY suspend, not just the idle timer's: "Sleep" in Steam's Big Picture power menu
//! reaches logind as exactly the same `Suspend()` call, and logind answers the person who pressed
//! it with `Operation inhibited by "Punktfunk" (…), reason is "a client is streaming"` — silently,
//! because nothing in that UI surfaces a D-Bus error. Held unconditionally for the length of a
//! stream (as it was from 2026-07-22 to this commit), the lock made a host impossible to put to
//! sleep from the machine's own screen for as long as anyone was watching it. Reproduced verbatim
//! on a Bazzite box, 2026-08-24.
//!
//! So the veto is held only while the stream is QUIET. Any client input ([`note_input`]) drops it
//! **synchronously** — releasing is a `close(2)` on the inhibitor fd, no round trip, so a Sleep
//! press cannot race it — and it is re-taken only after [`QUIET_BEFORE_VETO`] of silence. That is
//! the same line the original justification already drew ("a video-only viewer sends none"): a
//! person choosing Sleep is, by definition, sending input, and a passive viewer never does.
//!
//! What this deliberately does NOT cover is a local suspend request typed at a box that a passive
//! viewer is streaming from — the veto is still standing, so it is still refused. That case wants
//! a person-vs-timer signal we do not have, and the remote viewer's claim on the box is at least
//! arguable. `ponytail:` if it turns up in the field, the lever is a config knob, not a heuristic.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a stream must go without client input before the host vetoes suspend on its behalf.
/// Comfortably under every idle-suspend timer worth catching (Steam's shortest offer is 5 min,
/// KDE's default 10), and long enough that a menu press followed by a slow "are you sure?" cannot
/// re-arm the veto mid-decision.
const QUIET_BEFORE_VETO: Duration = Duration::from_secs(30);

/// How often [`watch`] re-checks the quiet time. Only the RE-ARM edge waits for a tick — the
/// release edge is synchronous in [`note_input`] — so this bounds nothing a user can feel.
#[cfg(target_os = "linux")]
const WATCH_TICK: Duration = Duration::from_secs(5);

/// RAII share of the host-wide inhibitor — hold one per live session/stream.
pub struct StreamHold(());

struct State {
    count: u32,
    /// Whether [`watch`] is running. Its exit is the 1→0 edge, so without this flag a session that
    /// ends and restarts inside one tick would leave two watchers racing for the same fd slot.
    #[cfg(target_os = "linux")]
    watching: bool,
    /// The logind inhibitor pipe fd — inhibition lasts exactly as long as it stays open.
    #[cfg(target_os = "linux")]
    fd: Option<ashpd::zbus::zvariant::OwnedFd>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            count: 0,
            #[cfg(target_os = "linux")]
            watching: false,
            #[cfg(target_os = "linux")]
            fd: None,
        })
    })
}

/// Monotonic ms since first use — a plain `AtomicU64` clock the input path can stamp with one
/// relaxed store, which `Instant` itself is too fat to be.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// Whether a veto is standing right now. Read once per input event, so it is what keeps
/// [`note_input`] off the mutex on the hot path.
static VETOING: AtomicBool = AtomicBool::new(false);

/// Whether the stream has been quiet long enough to veto suspend on the viewer's behalf.
fn quiet_for(last_input_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_input_ms) >= QUIET_BEFORE_VETO.as_millis() as u64
}

/// Client input arrived on any plane — the person at the other end is driving this box, so no
/// suspend veto may be standing when their next button press is "Sleep".
///
/// Called per decoded input event (keyboard, pointer, pad, pen, motion): one relaxed store, plus a
/// relaxed load that only ever takes the lock on the rare edge where a veto is actually standing.
pub fn note_input() {
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    if VETOING.load(Ordering::Relaxed) {
        release("the client is sending input again — a deliberate suspend now reaches logind");
    }
}

/// Take a share. The underlying inhibitor is NOT acquired here: the `watch` thread takes it once
/// the stream has been quiet for [`QUIET_BEFORE_VETO`], and never while someone is driving the box.
pub fn hold() -> StreamHold {
    // A fresh stream gets the full quiet window before anything is vetoed, so an ordinary connect
    // costs zero D-Bus round trips.
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.count += 1;
    #[cfg(target_os = "linux")]
    if !st.watching {
        st.watching = true;
        drop(st);
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-sleep-veto".into())
            .spawn(watch)
        {
            state().lock().unwrap_or_else(|e| e.into_inner()).watching = false;
            tracing::warn!(
                error = %e,
                "could not start the sleep-veto watcher — the box may auto-suspend under a \
                 passive (video-only) viewer"
            );
        }
    }
    StreamHold(())
}

impl Drop for StreamHold {
    fn drop(&mut self) {
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        st.count = st.count.saturating_sub(1);
        #[cfg(target_os = "linux")]
        if st.count == 0 {
            release_locked(&mut st, "no live sessions");
        }
    }
}

/// Drop any standing veto. Closing the fd is all it takes — no D-Bus, so this is safe to call from
/// the input path.
#[cfg(target_os = "linux")]
fn release(why: &str) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    release_locked(&mut st, why);
}

#[cfg(not(target_os = "linux"))]
fn release(_why: &str) {}

/// Drop any standing veto RIGHT NOW — the host-power path (`design/host-actions.md` §6): an
/// explicit `power.sleep` must not be refused by our own block inhibitor (we deliberately never
/// hold `-ignore-inhibit` rights). The session teardown that precedes it drops the holds too,
/// but via the video loops' next stop-flag check — this is the synchronous belt so the
/// `Suspend()` call can never race a veto that is already on its way out.
pub fn release_now() {
    release("a host power action is suspending/stopping this machine");
}

#[cfg(target_os = "linux")]
fn release_locked(st: &mut State, why: &str) {
    if st.fd.take().is_some() {
        VETOING.store(false, Ordering::Relaxed);
        tracing::info!(why, "released the sleep/idle inhibitor");
    }
}

/// Own the veto's arm/disarm edges for as long as any session lives.
///
/// Acquiring is the only expensive edge (a thread spawn + a D-Bus round trip), so it happens here
/// rather than on the input path, and outside the lock — a `note_input` on a hot input stream must
/// never queue behind a logind call.
#[cfg(target_os = "linux")]
fn watch() {
    loop {
        std::thread::sleep(WATCH_TICK);
        {
            let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
            if st.count == 0 {
                st.watching = false; // the last `Drop` already released the fd
                return;
            }
            if st.fd.is_some() {
                // Belt for the nanosecond window in which input lands after the re-check below but
                // before `VETOING` is published: that press releases nothing, so catch it here
                // rather than leave a veto standing over a live viewer.
                if !quiet_for(LAST_INPUT_MS.load(Ordering::Relaxed), now_ms()) {
                    release_locked(&mut st, "the client is sending input again");
                }
                continue;
            }
            if !quiet_for(LAST_INPUT_MS.load(Ordering::Relaxed), now_ms()) {
                continue;
            }
        }
        let Some(fd) = acquire() else {
            continue; // no logind / refused — `acquire` said so once, don't spin on it
        };
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        if st.count == 0 || !quiet_for(LAST_INPUT_MS.load(Ordering::Relaxed), now_ms()) {
            drop(fd); // raced: the stream ended, or the viewer came back — never veto for those
            continue;
        }
        st.fd = Some(fd);
        VETOING.store(true, Ordering::Relaxed);
    }
}

/// One logind `Inhibit` call on a dedicated plain thread — zbus's blocking API must not run on
/// a tokio worker (its internal `block_on` panics there), and callers of [`hold`] may be either.
/// The join blocks the caller for the D-Bus round-trip (~ms), which every call site tolerates.
#[cfg(target_os = "linux")]
fn acquire() -> Option<ashpd::zbus::zvariant::OwnedFd> {
    let fd = std::thread::spawn(|| -> Option<ashpd::zbus::zvariant::OwnedFd> {
        use ashpd::zbus;
        // zbus's blocking API is configured out by ashpd's feature set — drive the async API on
        // a private current-thread runtime instead (still on this plain thread; see fn doc).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let attempt: zbus::Result<zbus::zvariant::OwnedFd> = rt.block_on(async {
            let conn = zbus::Connection::system().await?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &("sleep:idle", "Punktfunk", "a client is streaming", "block"),
                )
                .await?;
            reply.body().deserialize()
        });
        match attempt {
            Ok(fd) => Some(fd),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "could not take a logind sleep/idle inhibitor — the box may auto-suspend \
                     under a passive (video-only) viewer"
                );
                None
            }
        }
    })
    .join()
    .ok()
    .flatten();
    if fd.is_some() {
        tracing::info!(
            quiet_s = QUIET_BEFORE_VETO.as_secs(),
            "holding a logind sleep/idle inhibitor — this stream has gone quiet"
        );
    }
    fd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole fix in one assertion: a stream that is being driven must never be vetoing, or the
    /// "Sleep" the viewer just picked in Steam's power menu is refused with no visible reason.
    #[test]
    fn a_driven_stream_is_never_vetoed_but_a_quiet_one_is() {
        let quiet_ms = QUIET_BEFORE_VETO.as_millis() as u64;
        assert!(!quiet_for(1_000, 1_000), "input this instant is not quiet");
        assert!(
            !quiet_for(1_000, 1_000 + quiet_ms - 1),
            "one ms short of the window still counts as driven"
        );
        assert!(
            quiet_for(1_000, 1_000 + quiet_ms),
            "the window elapsed — a passive viewer gets the veto"
        );
        // The clock starts at zero, so an un-stamped stream would look infinitely quiet: `hold`
        // seeds it precisely so a connect never vetoes before anyone could have pressed anything.
        assert!(quiet_for(0, quiet_ms), "an unseeded clock reads as quiet");
    }

    #[test]
    fn note_input_stamps_the_clock() {
        note_input();
        let stamped = LAST_INPUT_MS.load(Ordering::Relaxed);
        assert!(
            !quiet_for(stamped, now_ms()),
            "input just arrived — the veto must not be armable"
        );
    }
}
