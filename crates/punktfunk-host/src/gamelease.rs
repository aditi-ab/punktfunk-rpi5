//! The lifetime of a launched game, and the two behaviors bound to it
//! (design/session-game-lifetime.md).
//!
//! A session that launches a title takes a **lease** on the resulting game. The lease knows three
//! things the host previously had no way to know:
//!
//! * whether the game has actually started (as opposed to a launcher shim that exits immediately),
//! * when it exits — which ends the streaming session, so the client returns to its library
//!   instead of staring at a desktop,
//! * how to end it — so a session ending can take the game with it, when the operator asks for that.
//!
//! ### Why a lease rather than a pid
//!
//! Most stores never hand the host the game's process. `steam://rungameid/…`, Epic's launcher URI,
//! an AUMID activation and Playnite's `playnite://` all hand off to a launcher that starts the game
//! somewhere else entirely and exits. So a lease is defined by *how to recognize* the game
//! ([`crate::library::DetectSpec`]), not by a pid we happen to hold — and a lease we do hold a child
//! for ([`LeaseKind::Child`]) falls back to recognition the moment that child turns out to be a shim.
//!
//! ### Safety posture
//!
//! Ending a game is destructive: it can cost unsaved progress. Three rules bound it.
//!
//! 1. **Opt-in.** [`GameOnSessionEnd::Keep`] is the default; nothing is ever ended unless the
//!    operator asked ([`crate::session_settings`]).
//! 2. **Never a surprise on a network blip.** `Always` waits out a reconnect grace window before
//!    ending anything, and a reconnecting client re-adopts its lease.
//! 3. **Only ever this session's game.** Every pid is adopted only if it started *after* this
//!    session's launch, and re-verified against its start time before being signalled
//!    ([`crate::procscan`]) — so a pre-existing copy of the same game, or a recycled pid, is never
//!    touched.

use crate::library::DetectSpec;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cold-start budget: a launcher may take minutes to bring a game up (a Steam cold boot plus
/// first-run shader precompile is the worst case). A game that never appears in this window leaves
/// the session alone — the player is still looking at the launcher, which is a legitimate thing to
/// stream. Matches the shipped dedicated-session watcher, which this generalizes.
const START_GRACE: Duration = Duration::from_secs(300);
/// Poll interval for the watcher. One second is imperceptible against a game's startup or shutdown
/// and keeps a `/proc` sweep to a fraction of a percent of one core.
const POLL: Duration = Duration::from_secs(1);
/// The game must stay unseen across this window before it counts as exited, so a process swap (a
/// launcher re-execing, a mid-game restart) can't end the session early.
const EXIT_CONFIRM: Duration = Duration::from_secs(3);
/// A child that exits *successfully* within this window of being spawned was a launcher handing off,
/// not the game. Its exit must never end the session. (Apollo calls the same heuristic `auto_detach`;
/// the window matches.)
const SHIM_WINDOW: Duration = Duration::from_secs(5);
/// How long a game gets to close on its own after a polite request, before it is killed outright.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// A child process the host spawned for a launch, and what may safely be signalled for it.
#[derive(Clone, Copy, Debug)]
pub struct OwnedChild {
    pub pid: u32,
    /// Whether the child leads its own process group.
    ///
    /// Load-bearing for termination: `kill(-pid, …)` addresses the *process group* whose id is
    /// `pid`, which only means "this child and its descendants" if the child was made a group
    /// leader. For a child that shares the host's group, that same call would signal an unrelated
    /// group — so a non-leader is only ever signalled by its own pid.
    pub group_leader: bool,
}

/// What the host knows about a launched title, for display and for identity.
#[derive(Clone, Debug, Default)]
pub struct GameRef {
    /// Store-qualified library id (`steam:570`). `None` for an operator-typed GameStream
    /// `apps.json` command, which has no library entry behind it.
    pub id: Option<String>,
    /// Which store surfaced it (`steam`, `heroic`, `custom`, …), when known.
    pub store: Option<String>,
    /// Display title. Falls back to the id, then to the command, so this is never empty in the UI.
    pub title: String,
}

/// How this lease tracks its game.
#[derive(Clone, Debug)]
pub enum LeaseKind {
    /// A bare-spawn gamescope owns the game as its own nested child: gamescope exits when the game
    /// does, so its PipeWire node dying *is* the exit signal, and releasing the display ends the
    /// game. Detection and termination both stay with the display layer.
    Nested,
    /// The host spawned the command itself and holds the child. Its process group is the game (until
    /// it proves to be a shim, at which point the lease re-resolves to [`LeaseKind::Matched`]).
    Child,
    /// A launcher owns the game; it is recognized by its [`DetectSpec`].
    Matched,
    /// Nothing identifies this title's process — no detect signals and no child we own. Both
    /// lifetime behaviors stay inert for it, and the host says so once in the log rather than
    /// guessing.
    Untracked,
}

impl LeaseKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::Child => "child",
            Self::Matched => "matched",
            Self::Untracked => "untracked",
        }
    }
}

/// Where a lease is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GameState {
    /// Launched; the game has not been seen running yet.
    Launching = 0,
    /// Seen running.
    Running = 1,
    /// Confirmed gone.
    Exited = 2,
}

impl GameState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Exited,
            _ => Self::Launching,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Exited => "exited",
        }
    }
}

/// The part of a lease that outlives its session: identity, live state, and enough to end the game.
///
/// Held behind an `Arc` by the session, the watcher thread, and — after a disconnect that armed a
/// grace window — the [`grace`] registry.
pub struct LeaseShared {
    /// The title this lease is for.
    pub game: GameRef,
    /// The client that asked for it, for the same reason.
    pub client: String,
    /// Which plane the session was on.
    pub plane: crate::events::Plane,
    kind: LeaseKind,
    state: AtomicU8,
    /// Signals the watcher thread to stop (session ended, or the lease was terminated).
    cancel: Arc<AtomicBool>,
    /// How to recognize the game. Empty for [`LeaseKind::Nested`]/[`LeaseKind::Untracked`].
    spec: DetectSpec,
    /// Seconds-since-boot at launch: the floor for adopting a process (`None` = the uptime clock was
    /// unreadable, so no start-time filtering is possible and only signals are used).
    launch_uptime: Option<f64>,
    /// The child the host spawned for this launch, when it spawned one ([`LeaseKind::Child`]).
    /// Cleared once that child is reaped.
    child: Mutex<Option<OwnedChild>>,
    /// Whether this lease's game has been asked to end (so a second request is a no-op, and the
    /// watcher's exit doesn't look like the player quitting).
    terminating: AtomicBool,
    /// Monotonic-ish clock for the status surface: unix ms when the lease was created.
    pub created_ms: u64,
    /// Set when the game was seen running at least once — distinguishes "exited" from "never
    /// started" for logging and for the exit decision.
    was_running: AtomicBool,
    /// Wall-clock ms the game was last seen running (0 = never), for diagnostics.
    last_seen_ms: AtomicU64,
}

impl LeaseShared {
    pub fn state(&self) -> GameState {
        GameState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn kind(&self) -> &LeaseKind {
        &self.kind
    }

    /// True once the game has been asked to end (by policy, grace expiry, or the API).
    pub fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::Relaxed)
    }

    /// Can this lease's game be ended at all? `false` for a title the host can't identify.
    pub fn is_trackable(&self) -> bool {
        !matches!(self.kind, LeaseKind::Untracked)
    }

    fn set_state(&self, s: GameState) {
        self.state.store(s as u8, Ordering::Relaxed);
    }

    /// The host's own child for this launch, while it is still running.
    fn owned_child(&self) -> Option<OwnedChild> {
        *self.child.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drop the record of the child once it has been reaped — its pid must never be signalled after
    /// that, since the kernel is free to hand that number to something else.
    fn forget_child(&self) {
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Unix ms now — the same clock the event bus and status API use.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A session's handle on its game. Dropping it stops the watcher; what happens to the *game* is the
/// caller's decision, taken through [`crate::session_settings`] at teardown (see
/// [`GameLease::on_session_end`]).
pub struct GameLease {
    shared: Arc<LeaseShared>,
    /// Joined on drop only to stop the thread promptly; the watcher self-terminates on `cancel`.
    watcher: Option<std::thread::JoinHandle<()>>,
}

impl GameLease {
    /// The shared state, for the status surface and the grace registry.
    pub fn shared(&self) -> Arc<LeaseShared> {
        self.shared.clone()
    }
}

impl Drop for GameLease {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        // The watcher wakes at most `POLL` later; don't block teardown waiting for it.
        drop(self.watcher.take());
    }
}

/// Everything needed to open a lease. Built by the launch site, which is the only place that knows
/// all of it.
pub struct LeaseRequest {
    pub game: GameRef,
    pub client: String,
    pub plane: crate::events::Plane,
    pub spec: DetectSpec,
    /// The game's own compositor-nested-ness: `true` when a bare-spawn gamescope owns it.
    pub nested: bool,
    /// The child the host spawned for this launch, when it spawned one directly, and whether it
    /// leads its own process group (see [`OwnedChild::group_leader`]).
    pub child: Option<(std::process::Child, bool)>,
    /// Seconds-since-boot from **before** the launch ([`launch_clock`]): the floor for adopting a
    /// process, which is what keeps a copy of the game the player already had open from being
    /// mistaken for this session's. `None` disables the filter (no readable uptime clock).
    pub launch_uptime: Option<f64>,
}

/// The reference instant for adopting this launch's processes, in seconds since boot. Call it
/// **before** anything spawns; see [`LeaseRequest::launch_uptime`].
pub fn launch_clock() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        crate::procscan::Scanner::system().uptime()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// What the watcher does when it concludes the game has exited.
pub type OnExit = Box<dyn Fn() + Send + Sync>;

/// Open a lease and start watching. `on_exit` fires **at most once**, and only when the game was
/// seen running and then confirmed gone — never for a game that never started, never for a shim, and
/// never when the host itself asked the game to end.
pub fn open(req: LeaseRequest, on_exit: OnExit) -> GameLease {
    let LeaseRequest {
        game,
        client,
        plane,
        spec,
        nested,
        child,
        launch_uptime,
    } = req;

    let kind = if nested {
        LeaseKind::Nested
    } else if child.is_some() {
        LeaseKind::Child
    } else if !spec.is_empty() {
        LeaseKind::Matched
    } else {
        LeaseKind::Untracked
    };

    let owned = child.as_ref().map(|(c, leader)| OwnedChild {
        pid: c.id(),
        group_leader: *leader,
    });
    let child = child.map(|(c, _)| c);
    let shared = Arc::new(LeaseShared {
        game,
        client,
        plane,
        kind: kind.clone(),
        state: AtomicU8::new(GameState::Launching as u8),
        cancel: Arc::new(AtomicBool::new(false)),
        spec,
        launch_uptime,
        child: Mutex::new(owned),
        terminating: AtomicBool::new(false),
        created_ms: now_ms(),
        was_running: AtomicBool::new(false),
        last_seen_ms: AtomicU64::new(0),
    });

    if matches!(kind, LeaseKind::Untracked) {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            "this title exposes nothing the host can use to recognize its process — \
             game-exit detection and end-game-on-disconnect stay off for it"
        );
    } else {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            kind = kind.as_str(),
            "watching the launched game"
        );
    }

    let watcher = spawn_watcher(shared.clone(), child, on_exit);
    if watcher.is_none() {
        // Nothing is polling this lease (no signals to poll, or a platform without a matcher yet), so
        // its state will never advance on its own. Report it as running rather than leaving the
        // console showing "launching" forever: the host did just launch it, and with no watcher it is
        // making no claim about noticing the exit. The row disappears when the session ends.
        shared.set_state(GameState::Running);
    }
    GameLease { shared, watcher }
}

/// Start the per-lease watch thread. Off Linux (and for an untracked/nested lease with nothing to
/// poll) there is nothing to watch, so no thread is spawned at all.
fn spawn_watcher(
    shared: Arc<LeaseShared>,
    child: Option<std::process::Child>,
    on_exit: OnExit,
) -> Option<std::thread::JoinHandle<()>> {
    // An untracked lease has nothing to observe (it still exposes state for the status surface).
    //
    // A *nested* lease is watched whenever its store gave us something to look for. The display
    // layer's node-death check would eventually notice a nested game exiting, but it can't see the
    // case that matters most: a Steam launch nests the resident Steam *client*, which stays up after
    // the game quits, so gamescope never dies. Watching the game itself is what covers that — and it
    // notices every other nested title sooner than node death does. Node death remains the backstop
    // for a nested title with no signals at all.
    if matches!(shared.kind, LeaseKind::Untracked) {
        return None;
    }
    if matches!(shared.kind, LeaseKind::Nested) && shared.spec.is_empty() {
        return None;
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Windows lands in phase 2 (Job Objects + Toolhelp matching); until then a lease exists for
        // the status surface but nothing polls.
        let _ = (child, on_exit);
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        std::thread::Builder::new()
            .name("pf1-gamelease".into())
            .spawn(move || watch(shared, child, on_exit))
            .ok()
    }
}

/// The watch loop: wait for the game to appear, then for it to go away.
#[cfg(target_os = "linux")]
fn watch(shared: Arc<LeaseShared>, mut child: Option<std::process::Child>, on_exit: OnExit) {
    let scanner = crate::procscan::Scanner::system();
    let cancelled = || shared.cancel.load(Ordering::Relaxed);
    let spawned_at = Instant::now();
    let mut kind = shared.kind.clone();

    // ---- Phase 1: wait for the game to show up. ----
    let start_deadline = spawned_at + START_GRACE;
    loop {
        if cancelled() {
            return;
        }
        // A `Child` lease's own child is the primary signal; a shim exit re-resolves the lease.
        if matches!(kind, LeaseKind::Child) {
            match child.as_mut().map(|c| c.try_wait()) {
                Some(Ok(Some(status))) => {
                    let quick = spawned_at.elapsed() < SHIM_WINDOW;
                    child = None; // reaped
                    shared.forget_child();
                    if quick && status.success() {
                        // A launcher that handed the game off and exited. Fall back to recognizing
                        // the game by its store's signals; with none, stop tracking entirely rather
                        // than pretend the shim's exit was the game's.
                        kind = if shared.spec.is_empty() {
                            tracing::info!(
                                title = %shared.game.title,
                                "the launch command exited immediately (a launcher handing off) and \
                                 this title has no detect signals — stopping game tracking for it"
                            );
                            LeaseKind::Untracked
                        } else {
                            tracing::debug!(
                                title = %shared.game.title,
                                "the launch command handed off and exited — recognizing the game by \
                                 its store signals instead"
                            );
                            LeaseKind::Matched
                        };
                        if matches!(kind, LeaseKind::Untracked) {
                            return;
                        }
                    } else {
                        // It ran long enough to have BEEN the game (or failed outright). Either way
                        // the game is gone; only a success after a real run counts as "played".
                        if shared.spec.is_empty() {
                            if spawned_at.elapsed() >= SHIM_WINDOW {
                                shared.was_running.store(true, Ordering::Relaxed);
                                finish(&shared, &on_exit, "the launched process exited");
                            }
                            return;
                        }
                        // With signals available, let the scan below decide — the command may have
                        // been a wrapper whose game is still up.
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "could not poll the launched child — falling back to scanning");
                    child = None;
                    kind = if shared.spec.is_empty() {
                        LeaseKind::Untracked
                    } else {
                        LeaseKind::Matched
                    };
                    if matches!(kind, LeaseKind::Untracked) {
                        return;
                    }
                }
                _ => {}
            }
        }

        // The child being alive counts as the game running for a `Child` lease, so a title with no
        // detect signals is still fully tracked.
        let child_alive = matches!(kind, LeaseKind::Child) && child.is_some();
        let live = scanner.find(&shared.spec, shared.launch_uptime);
        if !live.is_empty() || child_alive {
            shared.was_running.store(true, Ordering::Relaxed);
            shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
            shared.set_state(GameState::Running);
            crate::events::emit(crate::events::EventKind::GameRunning {
                game: game_event_ref(&shared),
            });
            tracing::info!(
                title = %shared.game.title,
                kind = kind.as_str(),
                procs = live.len(),
                "the launched game is running"
            );
            break;
        }
        if Instant::now() >= start_deadline {
            tracing::info!(
                title = %shared.game.title,
                grace_s = START_GRACE.as_secs(),
                "the launched game never appeared — leaving the session alone"
            );
            return;
        }
        std::thread::sleep(POLL);
    }

    // ---- Phase 2: wait for it to go away, confirmed across a window. ----
    let mut gone_since: Option<Instant> = None;
    loop {
        if cancelled() {
            return;
        }
        if matches!(kind, LeaseKind::Child) {
            if let Some(Ok(Some(_))) = child.as_mut().map(|c| c.try_wait()) {
                child = None;
                shared.forget_child();
                if shared.spec.is_empty() {
                    finish(&shared, &on_exit, "the launched process exited");
                    return;
                }
            }
        }
        let child_alive = matches!(kind, LeaseKind::Child) && child.is_some();
        // Re-scan rather than only re-checking known pids: a game that re-execs (a launcher stub
        // becoming the real binary, an engine relaunching itself) keeps the same identity.
        let live = scanner.find(&shared.spec, shared.launch_uptime);
        if !live.is_empty() || child_alive {
            gone_since = None;
            shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
        } else if gone_since.get_or_insert_with(Instant::now).elapsed() >= EXIT_CONFIRM {
            finish(&shared, &on_exit, "the game exited");
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// Record the exit and, unless the host itself ended the game, run the session-ending action.
#[cfg(target_os = "linux")]
fn finish(shared: &Arc<LeaseShared>, on_exit: &OnExit, why: &str) {
    shared.set_state(GameState::Exited);
    let terminated = shared.is_terminating();
    crate::events::emit(crate::events::EventKind::GameExited {
        game: game_event_ref(shared),
        reason: if terminated {
            crate::events::GameEndReason::Terminated
        } else {
            crate::events::GameEndReason::Exited
        },
    });
    if terminated {
        // The host asked for this. Ending the session too would be double-counting: whatever asked
        // for the termination is already tearing down (or deliberately isn't).
        tracing::info!(title = %shared.game.title, "the game the host asked to end is gone");
        return;
    }
    tracing::info!(title = %shared.game.title, reason = why, "the launched game exited");
    on_exit();
}

/// The event-payload view of a lease.
pub fn game_event_ref(shared: &LeaseShared) -> crate::events::GameRefPayload {
    crate::events::GameRefPayload {
        app: shared.game.id.clone(),
        title: shared.game.title.clone(),
        store: shared.game.store.clone(),
        client: shared.client.clone(),
        plane: shared.plane,
    }
}

// ---------------------------------------------------------------------------------------------
// Termination
// ---------------------------------------------------------------------------------------------

/// End the game this lease tracks: politely first, then decisively. Runs on a detached thread — a
/// caller is usually a teardown path or a `Drop`, and neither may block on a game's shutdown.
///
/// Idempotent: a lease already terminating is left alone.
pub fn terminate(shared: Arc<LeaseShared>, why: &'static str) {
    if !shared.is_trackable() {
        tracing::debug!(
            title = %shared.game.title,
            "asked to end an untracked title — nothing to end"
        );
        return;
    }
    if shared.terminating.swap(true, Ordering::SeqCst) {
        return; // already on its way out
    }
    tracing::info!(title = %shared.game.title, reason = why, "ending the launched game");
    let name = "pf1-gameterm".to_string();
    let _ = std::thread::Builder::new().name(name).spawn(move || {
        terminate_blocking(&shared);
        // A lease whose session is still up has a live watcher, which reports the exit itself
        // (with the right reason) once it confirms the game is gone. A lease whose session has
        // already ended — every grace expiry and every `POST /game/end` — has no watcher left, so
        // this is the only place its exit can be reported from.
        if shared.cancel.load(Ordering::Relaxed) {
            shared.set_state(GameState::Exited);
            crate::events::emit(crate::events::EventKind::GameExited {
                game: game_event_ref(&shared),
                reason: crate::events::GameEndReason::Terminated,
            });
        }
    });
}

/// The ladder itself. Separate so it can be driven synchronously from a test.
fn terminate_blocking(shared: &LeaseShared) {
    match shared.kind {
        // gamescope owns the game: releasing the display tears the nested session — and therefore
        // the game — down. One mechanism for both, so a kept display can't outlive an ended game.
        LeaseKind::Nested => {
            // The same force-release the `/display/release` endpoint performs: gamescope exits with
            // its display and takes its nested game with it. It refuses displays that still have live
            // sessions, so this only reaches the *kept* display this ended session left behind.
            //
            // But it is not slot-targeted — it retires every kept display — and a lease has no handle
            // on which one is its own. So while anyone else is streaming, leave it alone: the cost of
            // being wrong here is disturbing an unrelated client, and the cost of doing nothing is a
            // game that keeps running, which is also the default everywhere else.
            let others = crate::session_status::count();
            if others > 0 {
                tracing::info!(
                    live_sessions = others,
                    title = %shared.game.title,
                    "not releasing kept displays to end this game — another session is streaming and \
                     the release is not per-display"
                );
                return;
            }
            let released = crate::vdisplay::registry::release(None);
            tracing::info!(
                released,
                title = %shared.game.title,
                "released the nested session's kept display to end its game"
            );
        }
        LeaseKind::Child | LeaseKind::Matched => {
            #[cfg(target_os = "linux")]
            unix_term_ladder(shared);
        }
        LeaseKind::Untracked => {}
    }
}

/// SIGTERM everything that belongs to the game, wait, then SIGKILL whatever ignored it.
///
/// Every pid is re-verified against its recorded start time immediately before each signal, so a pid
/// recycled during the grace window is never signalled ([`crate::procscan::Scanner::alive`]).
#[cfg(target_os = "linux")]
fn unix_term_ladder(shared: &LeaseShared) {
    let scanner = crate::procscan::Scanner::system();
    let owned = shared.owned_child();

    // Signal the host's own child, as a group when it leads one. A group signal is what catches a
    // shell wrapper's grandchildren — the actual game — which a per-pid sweep would miss.
    let signal_child = |sig: i32| -> bool {
        let Some(c) = owned else { return false };
        let target = if c.group_leader {
            -(c.pid as i32)
        } else {
            c.pid as i32
        };
        // SAFETY: `kill` returns a status code and touches no memory of ours. A negative target is
        // only ever used for a group this host created with the child as its leader (see
        // `OwnedChild::group_leader`) — never for a child sharing the host's own group.
        unsafe { libc::kill(target, sig) == 0 }
    };
    let signal_matched = |sig: i32| -> usize {
        // Re-scan and re-verify immediately before signalling, so a pid recycled since the last
        // sweep is never hit.
        scanner
            .alive(&scanner.find(&shared.spec, shared.launch_uptime))
            .into_iter()
            // SAFETY: as above, for a single pid just re-verified to be the process we adopted.
            .filter(|p| unsafe { libc::kill(p.pid as i32, sig) == 0 })
            .count()
    };
    let signal_all = |sig: i32| -> usize { usize::from(signal_child(sig)) + signal_matched(sig) };

    let asked = signal_all(libc::SIGTERM);
    tracing::debug!(
        title = %shared.game.title,
        signalled = asked,
        grace_s = TERM_GRACE.as_secs(),
        "asked the game to close"
    );
    // Give it a chance to save and exit on its own.
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        let still = scanner
            .alive(&scanner.find(&shared.spec, shared.launch_uptime))
            .len();
        // Signal 0 only probes for existence — the child (or its group) is gone once it fails.
        let child_gone = !signal_child(0);
        if still == 0 && child_gone {
            tracing::info!(title = %shared.game.title, "the game closed when asked");
            return;
        }
    }
    let killed = signal_all(libc::SIGKILL);
    tracing::warn!(
        title = %shared.game.title,
        killed,
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killed it"
    );
}

// ---------------------------------------------------------------------------------------------
// The grace registry: leases whose session is gone but whose game is on probation
// ---------------------------------------------------------------------------------------------

/// A lease waiting out its reconnect window. If the client comes back before the deadline the lease
/// is handed to the new session and nothing is ended; if it doesn't, the game ends.
pub struct Pending {
    pub shared: Arc<LeaseShared>,
    pub deadline: Instant,
    /// The client fingerprint that may re-adopt this lease (identity match on reconnect).
    pub fingerprint: Option<String>,
}

fn registry() -> &'static Mutex<Vec<Pending>> {
    static REG: OnceLock<Mutex<Vec<Pending>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Put a lease on probation: its game ends in `grace` unless the same client reconnects first.
pub fn arm_grace(shared: Arc<LeaseShared>, fingerprint: Option<String>, grace: Duration) {
    if !shared.is_trackable() {
        return;
    }
    tracing::info!(
        title = %shared.game.title,
        grace_s = grace.as_secs(),
        "the client is gone — the game ends when the reconnect window closes"
    );
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Pending {
            shared,
            deadline: Instant::now() + grace,
            fingerprint,
        });
    start_reaper();
}

/// A reconnecting client takes its game back: drops any pending termination for `fingerprint` whose
/// title matches `app`. Returns the number of leases reprieved.
pub fn readopt(fingerprint: Option<&str>, app: Option<&str>) -> usize {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let before = reg.len();
    reg.retain(|p| {
        let same_client = match (&p.fingerprint, fingerprint) {
            (Some(a), Some(b)) => a == b,
            // With no fingerprint on either side there is nothing to match on; be conservative and
            // keep the lease pending rather than let any reconnect reprieve any game.
            _ => false,
        };
        let same_app = p.shared.game.id.as_deref() == app;
        if same_client && same_app {
            tracing::info!(
                title = %p.shared.game.title,
                "the client reconnected inside the window — the game keeps running"
            );
            false
        } else {
            true
        }
    });
    before - reg.len()
}

/// Every lease currently on probation, with the time left, for the status surface.
pub fn pending_snapshot() -> Vec<(Arc<LeaseShared>, u64)> {
    let now = Instant::now();
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|p| {
            (
                p.shared.clone(),
                p.deadline.saturating_duration_since(now).as_secs(),
            )
        })
        .collect()
}

/// End the games of pending leases immediately — the console's "End now" for a game whose session is
/// already gone. `app` filters to one title; `None` ends all of them. Returns how many were ended.
pub fn end_pending(app: Option<&str>) -> usize {
    let taken: Vec<Arc<LeaseShared>> = {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let (hit, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *reg)
            .into_iter()
            .partition(|p| app.is_none() || p.shared.game.id.as_deref() == app);
        *reg = keep;
        hit.into_iter().map(|p| p.shared).collect()
    };
    let n = taken.len();
    for shared in taken {
        terminate(shared, "ended from the management API");
    }
    n
}

/// The single background thread that ends pending leases when their window closes. Started on demand
/// and lives for the process, sleeping while the registry is empty.
fn start_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("pf1-gamegrace".into())
            .spawn(|| loop {
                std::thread::sleep(POLL);
                let now = Instant::now();
                let due: Vec<Arc<LeaseShared>> = {
                    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
                    let (due, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *reg)
                        .into_iter()
                        .partition(|p| now >= p.deadline);
                    *reg = keep;
                    due.into_iter().map(|p| p.shared).collect()
                };
                for shared in due {
                    // The player may have quit the game themselves during the window; terminate is a
                    // no-op then (nothing matches the spec any more).
                    terminate(shared, "the reconnect window closed with no client");
                }
            });
    });
}

/// What a session should do with its game when it ends. The policy lives in
/// [`crate::session_settings`]; this is the one place that turns it into an action, so both planes
/// behave identically.
pub fn on_session_end(lease: &GameLease, deliberate: bool, fingerprint: Option<&str>) {
    use crate::session_settings::GameOnSessionEnd;
    let settings = crate::session_settings::get();
    let shared = lease.shared();
    if !shared.is_trackable() || shared.state() == GameState::Exited {
        return; // nothing to end (or it already ended on its own)
    }
    let end_now = |shared: Arc<LeaseShared>| {
        // A deliberate stop already forces this session's display down immediately (the `quit` flag
        // beats keep-alive linger), and for a nested launch that teardown *is* what ends the game —
        // gamescope exits with its display and takes its child with it. Asking again here would race
        // that teardown for a display that still has a live session, which the registry refuses
        // anyway. So: let it happen, and say so.
        if matches!(shared.kind, LeaseKind::Nested) {
            tracing::debug!(
                title = %shared.game.title,
                "the nested session's display teardown ends its game — nothing extra to do"
            );
            return;
        }
        terminate(shared, "the session was stopped deliberately");
    };
    match settings.game_on_session_end {
        GameOnSessionEnd::Keep => {}
        GameOnSessionEnd::OnQuit => {
            if deliberate {
                end_now(shared);
            }
        }
        GameOnSessionEnd::Always => {
            if deliberate {
                end_now(shared);
            } else {
                // A drop is not a decision. Give the client its window back — and note that for a
                // nested lease the *display* may outlive the window under its own keep-alive policy,
                // which is exactly why the expiry path releases it.
                arm_grace(
                    shared,
                    fingerprint.map(str::to_string),
                    Duration::from_secs(settings.disconnect_grace_seconds.into()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lease request for `id`. The id is a parameter because the grace registry is process-wide and
    /// these tests run in parallel: each must arm and inspect an entry only it can name, or one test's
    /// re-adoption would reprieve another's lease.
    fn req(id: &str, spec: DetectSpec, nested: bool) -> LeaseRequest {
        LeaseRequest {
            game: GameRef {
                id: Some(id.to_string()),
                store: Some("steam".into()),
                title: format!("Test Title {id}"),
            },
            client: "Deck".into(),
            plane: crate::events::Plane::Native,
            spec,
            nested,
            child: None,
            // No start-time floor: these leases are never matched against real processes.
            launch_uptime: None,
        }
    }

    /// Is a lease for `id` currently on probation?
    fn is_pending(id: &str) -> bool {
        pending_snapshot()
            .iter()
            .any(|(s, _)| s.game.id.as_deref() == Some(id))
    }

    #[test]
    fn kind_follows_what_the_launch_gave_us() {
        // Nested wins over everything: the display layer owns the lifetime.
        let l = open(
            req("steam:100", DetectSpec::steam(100), true),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Nested));
        // With signals but no child and no nesting, the game is recognized.
        let l = open(
            req("steam:101", DetectSpec::steam(101), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Matched));
        // With neither, the lease is inert and says so.
        let l = open(
            req("steam:102", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
        assert!(!l.shared().is_trackable());
    }

    #[test]
    fn an_untracked_lease_is_never_terminated() {
        let l = open(
            req("steam:110", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        let shared = l.shared();
        terminate(shared.clone(), "test");
        assert!(!shared.is_terminating(), "untracked leases must stay inert");
    }

    #[test]
    fn terminate_is_idempotent() {
        // Nested, so the ladder is a display release rather than a signal to a real process.
        let l = open(
            req("steam:120", DetectSpec::steam(120), true),
            Box::new(|| {}),
        );
        let shared = l.shared();
        terminate(shared.clone(), "first");
        assert!(shared.is_terminating());
        // A second request must not re-run the ladder; the flag was already set.
        terminate(shared.clone(), "second");
        assert!(shared.is_terminating());
    }

    #[test]
    fn grace_readoption_matches_client_and_title() {
        let id = "steam:130";
        let l = open(req(id, DetectSpec::steam(130), false), Box::new(|| {}));
        arm_grace(
            l.shared(),
            Some("fp-130".into()),
            Duration::from_secs(3_600),
        );
        // A different client, or a different title, does not reprieve it.
        assert_eq!(readopt(Some("fp-other"), Some(id)), 0);
        assert_eq!(readopt(Some("fp-130"), Some("steam:9999")), 0);
        // A missing fingerprint on either side must not reprieve anything either — otherwise any
        // unidentified reconnect could keep any game alive.
        assert_eq!(readopt(None, Some(id)), 0);
        assert!(is_pending(id), "none of those should have reprieved it");
        // The right client coming back for the right title does.
        assert_eq!(readopt(Some("fp-130"), Some(id)), 1);
        assert!(!is_pending(id));
    }

    #[test]
    fn pending_snapshot_reports_the_window_left() {
        let id = "steam:140";
        let l = open(req(id, DetectSpec::steam(140), false), Box::new(|| {}));
        arm_grace(l.shared(), Some("fp-140".into()), Duration::from_secs(300));
        let mine = pending_snapshot()
            .into_iter()
            .find(|(s, _)| s.game.id.as_deref() == Some(id))
            .expect("armed lease is pending");
        assert!(mine.1 > 290 && mine.1 <= 300, "remaining was {}", mine.1);
        // Leave the registry as we found it, so a sibling test's sweep can't see this entry.
        assert_eq!(readopt(Some("fp-140"), Some(id)), 1);
    }

    #[test]
    fn end_pending_only_takes_the_named_title() {
        let (a, b) = ("steam:150", "steam:151");
        let la = open(req(a, DetectSpec::steam(150), true), Box::new(|| {}));
        let lb = open(req(b, DetectSpec::steam(151), true), Box::new(|| {}));
        arm_grace(
            la.shared(),
            Some("fp-150".into()),
            Duration::from_secs(3_600),
        );
        arm_grace(
            lb.shared(),
            Some("fp-151".into()),
            Duration::from_secs(3_600),
        );
        // Ending one title leaves the other waiting.
        assert_eq!(end_pending(Some(a)), 1);
        assert!(!is_pending(a));
        assert!(is_pending(b));
        assert!(la.shared().is_terminating());
        assert!(!lb.shared().is_terminating());
        // An id nobody is waiting on ends nothing.
        assert_eq!(end_pending(Some("steam:99999")), 0);
        assert_eq!(readopt(Some("fp-151"), Some(b)), 1);
    }

    #[test]
    fn state_strings_are_stable() {
        // These reach the API and the console; renaming one is a wire change.
        assert_eq!(GameState::Launching.as_str(), "launching");
        assert_eq!(GameState::Running.as_str(), "running");
        assert_eq!(GameState::Exited.as_str(), "exited");
        assert_eq!(GameState::from_u8(1), GameState::Running);
        assert_eq!(GameState::from_u8(99), GameState::Launching);
    }
}
