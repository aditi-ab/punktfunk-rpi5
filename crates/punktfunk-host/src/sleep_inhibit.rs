//! Refcounted, session-scoped Linux suspend inhibition for passive streams.
//!
//! After [`QUIET_BEFORE_VETO`] without client input, the host holds a logind
//! `sleep:idle` block so an idle timer cannot suspend the machine.
//! [`note_input`] releases it synchronously: the same inhibitor would refuse
//! an intentional Sleep. Silence re-arms it. A local Sleep during a passive
//! stream is indistinguishable from idle and stays blocked.
//!
//! Acquisition is best-effort, shared by native and GameStream sessions, and
//! a no-op off Linux.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Quiet window before the host vetoes suspend. Under Steam's 5 min and KDE's
/// 10 min idle timers, and long enough that a Sleep confirmation cannot re-arm
/// mid-click.
const QUIET_BEFORE_VETO: Duration = Duration::from_secs(30);

/// [`watch`] poll interval. Only re-arm waits for a tick; [`note_input`] releases synchronously.
#[cfg(target_os = "linux")]
const WATCH_TICK: Duration = Duration::from_secs(5);

/// One RAII share of the host-wide inhibitor. Hold one per live session.
pub struct StreamHold(());

struct State {
    count: u32,
    /// True while [`watch`] is running. Exit is the 1→0 edge; without this flag a
    /// session that ends and restarts inside one tick would spawn a second watcher.
    #[cfg(target_os = "linux")]
    watching: bool,
    /// Logind inhibitor pipe. Inhibition lasts exactly as long as this fd stays open.
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

/// `Instant` is too fat for one relaxed store on the input path.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// One relaxed load per input event keeps [`note_input`] off the mutex.
static VETOING: AtomicBool = AtomicBool::new(false);

fn quiet_for(last_input_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_input_ms) >= QUIET_BEFORE_VETO.as_millis() as u64
}

/// Stamp last client input. Releases a standing veto so an intentional Sleep is
/// not refused by our own block.
pub fn note_input() {
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    if VETOING.load(Ordering::Relaxed) {
        release("the client is sending input again — a deliberate suspend now reaches logind");
    }
}

/// Take a share. The inhibitor is not acquired here: [`watch`] takes it after
/// [`QUIET_BEFORE_VETO`] of quiet, never while someone is driving the box.
pub fn hold() -> StreamHold {
    // Seed the quiet clock so a connect never vetoes and costs no D-Bus round trip.
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

/// Drop a standing veto. Closing the fd is enough — no D-Bus, so the input path can call this.
#[cfg(target_os = "linux")]
fn release(why: &str) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    release_locked(&mut st, why);
}

#[cfg(not(target_os = "linux"))]
fn release(_why: &str) {}

/// Drop a standing veto before an explicit `power.sleep` (`design/host-actions.md`).
/// We never hold `-ignore-inhibit`, so our own block would refuse Sleep. Session
/// Drop also releases, but only on the next stop-flag check; this is the sync
/// path so `Suspend()` cannot race a still-open fd.
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

/// Own the veto's arm/disarm edges while any session lives.
///
/// Acquire is the expensive edge (thread spawn + D-Bus). It happens here,
/// outside the lock, so a hot `note_input` never queues behind a logind call.
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
                // Input can land after the quiet re-check below and before `VETOING`
                // is stored; that press would not call `release`, so catch it here.
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
            continue; // no logind / refused — `acquire` logged once; do not spin
        };
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        if st.count == 0 || !quiet_for(LAST_INPUT_MS.load(Ordering::Relaxed), now_ms()) {
            drop(fd); // raced: stream ended or viewer returned — do not store the fd
            continue;
        }
        st.fd = Some(fd);
        VETOING.store(true, Ordering::Relaxed);
    }
}

/// One logind `Inhibit` on a dedicated plain thread. zbus's blocking API panics
/// on a tokio worker (`block_on` nested), and [`hold`] callers may be either.
/// The join waits the D-Bus round-trip (~ms); every call site tolerates that.
#[cfg(target_os = "linux")]
fn acquire() -> Option<ashpd::zbus::zvariant::OwnedFd> {
    let fd = std::thread::spawn(|| -> Option<ashpd::zbus::zvariant::OwnedFd> {
        use ashpd::zbus;
        // ashpd disables zbus's blocking API. Drive the async API on a private
        // current-thread runtime, still on this plain thread (see fn doc).
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
        // The clock starts at zero, so an unstamped stream looks infinitely quiet.
        // `hold` seeds it so a connect never vetoes before anyone could have pressed.
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
