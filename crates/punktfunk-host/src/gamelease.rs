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
//! Where the host *did* start the process itself it keeps that too, as a second signal rather than
//! the definition: a `Child` on Linux, and on Windows the bare pid `CreateProcessAsUserW` hands back
//! ([`LeaseRequest::spawned`]). That pid used to be dropped, which left Windows strictly worse off
//! than Linux — a title whose provider published no detect signals had *nothing* identifying it, so
//! its exit was never noticed and it could not be ended (field report 2026-08-16, Windows 0.29.0).
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
/// How long [`crate::procscan::running_hint`] may hold off the exit once the game's processes have
/// all gone.
///
/// The hint is a tie-breaker for a scan that momentarily cannot see the game — a launcher re-execing,
/// an engine relaunching itself into a new pid — and those gaps are over in seconds, an order of
/// magnitude inside this window. Past it, a game nothing can find is gone whatever the hint says.
///
/// **Bounded because the hint's backing state is not guaranteed to be truthful.** Windows reads
/// Steam's per-app `Running` registry flag, which Steam leaves set whenever it does not cleanly
/// observe the exit (Steam crashed or was closed first, the game re-parented, a launcher appid stays
/// set) — and `steam_running_hint` believes the first hive that says so, including a stale one left
/// in another profile. An UNBOUNDED veto turns that into a session that never ends on its own: the
/// console shows the game running for as long as the host does, `session_on_game_exit` never fires,
/// and only a manual "End" gets the stream back (field report 2026-08-06, Windows host 0.24.0).
///
/// Ending a moment too early is the cheaper failure: the stream drops while the game lives (the user
/// reconnects, and `finish` never kills anything). Ending never is the bug above.
const VETO_LIMIT: Duration = Duration::from_secs(30);

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
    /// A launcher owns the game and **tells us** when it starts and stops
    /// ([`crate::runstate`]) — no process signal of our own.
    ///
    /// The one lease kind whose liveness the host does not determine for itself, and the answer to
    /// a title that has nothing to scan for: Playnite launches an emulated or manually-added game
    /// through its own tracking and reports the edges, where the host could see only a
    /// `playnite://` forwarder exiting. Before this such a title was [`Untracked`](Self::Untracked)
    /// — the honest answer at the time, and a dead end.
    Reported,
    /// Nothing identifies this title's process — no detect signals, no child we own, and no
    /// provider reporting on it. Both lifetime behaviors stay inert for it, and the host says so
    /// once in the log rather than guessing.
    Untracked,
}

impl LeaseKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::Child => "child",
            Self::Matched => "matched",
            Self::Reported => "reported",
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
    /// The host launched this title but has no way to recognize its process
    /// ([`LeaseKind::Untracked`]), so it will never observe the game starting *or* stopping.
    ///
    /// A terminal state, and the honest answer to "what is this game doing?" — which
    /// [`Running`](Self::Running) was not. Reporting `Running` here (the shipped behavior until
    /// 0.30) made three separate things indistinguishable in the console: a game being watched, a
    /// game that quit and was never noticed, and a game the host cannot see at all. It is also
    /// exactly what a 2026-08-16 field report hit — a Windows title quit mid-stream, the console
    /// stayed on "running" forever, and no session setting made any difference, because
    /// `session_on_game_exit` can never fire for a lease nothing is watching.
    Untracked = 3,
}

impl GameState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Exited,
            3 => Self::Untracked,
            _ => Self::Launching,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Untracked => "untracked",
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
    launch_stamp: Option<f64>,
    /// The child the host spawned for this launch, when it spawned one ([`LeaseKind::Child`]).
    /// Cleared once that child is reaped.
    child: Mutex<Option<OwnedChild>>,
    /// The process the host spawned for this launch on a platform with no `Child` to hold
    /// ([`LeaseRequest::spawned`]), pinned to its start time.
    ///
    /// Never cleared: unlike a `Child` there is no handle to reap, and every read re-verifies the
    /// pair through [`crate::procscan::Scanner::alive`] before counting it live or signalling it —
    /// so a stale entry answers "gone", which is the correct answer, rather than a recycled pid.
    spawned: Option<crate::procscan::ProcRef>,
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
    /// This entry opens a LAUNCHER rather than a game (design D4), which makes the lease
    /// [`LeaseKind::Untracked`] no matter what else is known about it.
    ///
    /// A launcher has no "the game exited" moment to detect, and trying to infer one is worse than
    /// not trying. Steam is the clean counterexample: Big Picture is a *mode* of an already-running
    /// Steam client, not a process — and on a Deck or SteamOS host Steam is always running — so no
    /// process signal can express "the Big Picture window closed".
    ///
    /// Without this flag the lifetime would also be decided by something the user cannot see:
    /// launching a launcher that is NOT yet running leaves the host holding a live child (tracked,
    /// so quitting it ends the session), while launching one that IS running has the command
    /// forward and exit inside [`SHIM_WINDOW`] (untracked, so the session persists). Same tile, two
    /// behaviours. Untracked is the honest one of the two, so it is the one that always applies.
    pub launcher: bool,
    /// The child the host spawned for this launch, when it spawned one directly, and whether it
    /// leads its own process group (see [`OwnedChild::group_leader`]).
    pub child: Option<(std::process::Child, bool)>,
    /// The pid the host spawned for this launch on a platform where it gets a **pid instead of a
    /// child** — Windows, where a launch goes through `CreateProcessAsUserW` into the interactive
    /// session and there is no `std::process::Child` to hold.
    ///
    /// Tracked for exactly the same reason [`Self::child`] is, and it closes the gap that made
    /// Windows strictly worse than Linux at this: a title whose provider supplied no detect hint
    /// has an empty [`DetectSpec`], and with no child either the lease had *nothing* — so it went
    /// [`LeaseKind::Untracked`], its exit was never noticed, and `POST /game/end` had no pid to
    /// signal. The same title on Linux was fully tracked, because the host held its child there.
    ///
    /// Resolved to a (pid, start time) pair at [`open`] time so a recycled pid can never be
    /// mistaken for it; see [`crate::procscan::Scanner::resolve`].
    pub spawned: Option<u32>,
    /// Seconds-since-boot from **before** the launch ([`launch_clock`]): the floor for adopting a
    /// process, which is what keeps a copy of the game the player already had open from being
    /// mistaken for this session's. `None` disables the filter (no readable uptime clock).
    ///
    /// A *reconnecting* session inherits this from [`crate::launchreg`] rather than minting its own,
    /// which is the only way its lease can see a game the previous session started.
    pub launch_stamp: Option<f64>,
    /// Where the watcher publishes the processes it adopts, so the host's launch record can still
    /// answer "is our launch up?" after this lease and its watcher are gone
    /// ([`crate::launchreg::LiveProcs`]). `None` for a launch that isn't recorded.
    pub procs: Option<crate::launchreg::LiveProcs>,
}

/// The reference instant for adopting this launch's processes, in seconds since boot. Call it
/// **before** anything spawns; see [`LeaseRequest::launch_stamp`].
pub fn launch_clock() -> Option<f64> {
    // Delegate unconditionally: `procscan::launch_stamp` already answers `None` on a platform with
    // no matcher. Gating this on Linux here silently disabled the start-time floor on Windows —
    // `find(spec, None)` skips the filter entirely — so a copy of the game the player already had
    // open was adopted and then ended with the session (caught on glass, .173: Steam focused a
    // running instance instead of starting one, and the lease took it). The Windows matcher is
    // exactly where that rule matters most, since the host is SYSTEM and can see every process.
    crate::procscan::launch_stamp()
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
        launcher,
        child,
        spawned,
        launch_stamp,
        procs,
    } = req;

    // Pin the spawned pid to its start time before anything else can recycle it. A pid we cannot
    // resolve (it already exited, or it is not queryable) is simply dropped: a bare number is never
    // allowed further in, which is procscan's rule 2.
    let spawned = spawned.and_then(crate::procscan::resolve);

    // A launcher tile is untracked FIRST, before anything else is considered — see
    // `LeaseRequest::launcher`. Checking it ahead of `child` is the whole point: a launcher the host
    // just started leaves a live child behind, and tracking that child is exactly the inconsistency
    // this removes.
    let kind = if launcher {
        LeaseKind::Untracked
    } else if nested {
        LeaseKind::Nested
    } else if child.is_some() || spawned.is_some() {
        // A pid we spawned is our own child in every sense that matters here — the only difference
        // is that this platform hands back a number instead of a handle — so it takes the same
        // lifetime rules, shim reclassification included.
        LeaseKind::Child
    } else if !spec.is_empty() {
        LeaseKind::Matched
    } else if crate::runstate::speaks_for(game.id.as_deref()) {
        // Nothing to scan for, but the provider that published this title is reporting liveness for
        // it — so it is tracked after all. Asked once, here, rather than every poll: a lease's kind
        // is what decides whether it is watched at all, and a title that flipped kind mid-flight
        // would make both lifetime behaviors depend on a plugin's uptime.
        LeaseKind::Reported
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
        launch_stamp,
        child: Mutex::new(owned),
        spawned,
        terminating: AtomicBool::new(false),
        created_ms: now_ms(),
        was_running: AtomicBool::new(false),
        last_seen_ms: AtomicU64::new(0),
    });

    if launcher {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            "this entry opens a launcher, not a game — the session stays up until the client \
             leaves, and closing the launcher does not end it"
        );
    } else if matches!(kind, LeaseKind::Untracked) {
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

    let watcher = spawn_watcher(shared.clone(), child, procs, on_exit);
    if watcher.is_none() {
        // Nothing is polling this lease, so its state will never advance on its own — and which
        // state to leave it in depends on WHY, because the two cases make opposite promises.
        //
        // * An UNTRACKED lease has no signal at all: nothing will ever notice this game starting or
        //   stopping. `Untracked` says so. It used to report `Running`, on the reasoning that the
        //   host had just launched it and a row stuck at "launching" reads as broken — but that
        //   made a console row assert liveness the host cannot back up, and it is what a
        //   2026-08-16 field report ran into (see [`GameState::Untracked`]).
        // * A NESTED lease with no store signals is genuinely being watched, just not from here:
        //   a bare-spawn gamescope dies with its app, so the capture loop's node-death check is its
        //   exit detection. Its game really is running, and its exit really will end the session,
        //   so `Running` stays the truthful answer for it.
        //
        // Keyed on `Nested` rather than on `Untracked` so the two remaining ways to reach here —
        // a platform with no process matcher, and a watcher thread that failed to spawn — land on
        // the honest answer too: in both, nothing is watching, whatever the lease's kind says.
        shared.set_state(if matches!(shared.kind, LeaseKind::Nested) {
            GameState::Running
        } else {
            GameState::Untracked
        });
    }
    GameLease { shared, watcher }
}

/// Start the per-lease watch thread. Off Linux (and for an untracked/nested lease with nothing to
/// poll) there is nothing to watch, so no thread is spawned at all.
fn spawn_watcher(
    shared: Arc<LeaseShared>,
    child: Option<std::process::Child>,
    procs: Option<crate::launchreg::LiveProcs>,
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
    // Platforms with no matcher at all (macOS has no launch path either) keep a lease for the status
    // surface, but nothing polls it.
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (child, procs, on_exit);
        return None;
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        std::thread::Builder::new()
            .name("pf1-gamelease".into())
            .spawn(move || watch(shared, child, procs, on_exit))
            .ok()
    }
}

/// The watch loop: wait for the game to appear, then for it to go away.
#[cfg(any(target_os = "linux", windows))]
fn watch(
    shared: Arc<LeaseShared>,
    mut child: Option<std::process::Child>,
    procs: Option<crate::launchreg::LiveProcs>,
    on_exit: OnExit,
) {
    let scanner = crate::procscan::Scanner::system();
    let cancelled = || shared.cancel.load(Ordering::Relaxed);
    // Publish what this lease adopted to the host's launch record, so a LATER session can tell "this
    // host's launch is still up" from "nothing of ours is running" — which is what lets it inherit
    // this launch instead of starting a second copy (`crate::launchreg`).
    //
    // Only ever the CONCRETE processes, and only ever a non-empty set. Never the spec: a later re-scan
    // by spec would find a copy the player started for themselves since, and adopting that is exactly
    // what procscan's rule 1 forbids. And never cleared on exit: the last set the watcher saw is what
    // makes the record answer `Gone` (every recorded pid re-verified dead) rather than "no opinion",
    // which is how a game the player quit becomes relaunchable at once.
    let publish = |live: &[crate::procscan::ProcRef]| {
        if live.is_empty() {
            return;
        }
        if let Some(slot) = procs.as_ref() {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = live.to_vec();
        }
    };
    let spawned_at = Instant::now();
    let mut kind = shared.kind.clone();

    // The game's processes as last seen. Phase 2 re-verifies these rather than re-scanning the whole
    // process table each second: `alive` costs one query per known process, while `find` costs one per
    // process on the box (and on Windows that means an `OpenProcess` each). A full re-scan still
    // happens the moment they all vanish, which is what catches a game that re-execs into a new pid.
    //
    // Deliberately uninitialized: the only way out of phase 1 and into phase 2 is the branch that
    // assigns it, so an initial value would be dead.
    let mut known: Vec<crate::procscan::ProcRef>;

    // The process the host spawned on a platform that hands back a pid rather than a `Child`
    // (Windows). Cleared once it is seen gone, so `is_some()` reads "ours is still up" — the same
    // one-way transition `child` makes when it is reaped, which is what lets both phases treat the
    // two the same way.
    let mut spawned = shared.spawned;
    // Re-verified every time rather than remembered: `alive` checks the (pid, start) pair, so a
    // recycled pid answers "gone" instead of impersonating our launch.
    let spawned_up = |s: &Option<crate::procscan::ProcRef>| -> bool {
        s.is_some_and(|p| !scanner.alive(&[p]).is_empty())
    };

    // What this title's provider says about it, when one reports at all ([`crate::runstate`]) —
    // `None` on every host with no reporting plugin, which is what keeps all of this inert until
    // someone opts in. Re-read each poll rather than captured: the whole value of it is that it
    // changes while the lease is alive.
    let reported = || shared.game.id.as_deref().and_then(crate::runstate::opinion);

    // What a `Child` lease falls back to once its child turns out to be a shim: the store's own
    // signals, else the provider's reporting, else nothing. The same ladder [`open`] walks, minus
    // the child that has just gone away — and the reason a hint-less Playnite title is tracked at
    // all on Windows, where the launch is `explorer.exe "playnite://…"` and therefore ALWAYS a
    // hand-off, so every such lease arrives here.
    let fallback_kind = || {
        if !shared.spec.is_empty() {
            LeaseKind::Matched
        } else if crate::runstate::speaks_for(shared.game.id.as_deref()) {
            LeaseKind::Reported
        } else {
            LeaseKind::Untracked
        }
    };

    // ---- Phase 1: wait for the game to show up. ----
    let start_deadline = spawned_at + START_GRACE;
    // How long the scan has *continuously* seen something for this title — the scan-side twin of
    // [`SHIM_WINDOW`]. See `scan_settled` below for what it is protecting against.
    let mut seen_since: Option<Instant> = None;
    loop {
        if cancelled() {
            return;
        }
        // The pid-shaped twin of the `try_wait` arm below: our own process is gone, and the same
        // two questions decide what that means. There is no exit status to read (no handle), so
        // "quick" alone stands in for "a launcher handing off" — which is the right reading on the
        // platform this applies to, where every launch goes through a launcher or the shell.
        if matches!(kind, LeaseKind::Child)
            && child.is_none()
            && spawned.is_some()
            && !spawned_up(&spawned)
        {
            spawned = None;
            let quick = spawned_at.elapsed() < SHIM_WINDOW;
            kind = fallback_kind();
            if quick {
                if matches!(kind, LeaseKind::Untracked) {
                    tracing::info!(
                        title = %shared.game.title,
                        "the launch command exited immediately (a launcher handing off) and this \
                         title has no detect signals — stopping game tracking for it"
                    );
                    // Nothing will ever observe this game again, so say that rather than leave the
                    // console on "launching" forever — the same honest answer `open` reaches when it
                    // starts no watcher at all.
                    shared.set_state(GameState::Untracked);
                    return;
                }
                tracing::debug!(
                    title = %shared.game.title,
                    kind = kind.as_str(),
                    "the launch command handed off and exited — recognizing the game another way"
                );
            } else if matches!(kind, LeaseKind::Untracked) {
                // It ran long enough to have BEEN the game, and nothing else identifies it.
                shared.was_running.store(true, Ordering::Relaxed);
                finish(&shared, &on_exit, "the launched process exited");
                return;
            }
            // With signals available, let the scan below decide — it may have been a wrapper whose
            // game is still up.
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
                        // the game by its store's signals (or its provider's reporting); with
                        // neither, stop tracking entirely rather than pretend the shim's exit was
                        // the game's.
                        kind = fallback_kind();
                        if matches!(kind, LeaseKind::Untracked) {
                            tracing::info!(
                                title = %shared.game.title,
                                "the launch command exited immediately (a launcher handing off) and \
                                 this title has no detect signals — stopping game tracking for it"
                            );
                            shared.set_state(GameState::Untracked);
                            return;
                        }
                        tracing::debug!(
                            title = %shared.game.title,
                            kind = kind.as_str(),
                            "the launch command handed off and exited — recognizing the game \
                             another way"
                        );
                    } else {
                        // It ran long enough to have BEEN the game (or failed outright). Either way
                        // the game is gone; only a success after a real run counts as "played".
                        kind = fallback_kind();
                        if matches!(kind, LeaseKind::Untracked) {
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
                    kind = fallback_kind();
                    if matches!(kind, LeaseKind::Untracked) {
                        shared.set_state(GameState::Untracked);
                        return;
                    }
                }
                _ => {}
            }
        }

        // The child being alive counts as the game running for a `Child` lease, so a title with no
        // detect signals is still fully tracked.
        //
        // But a launcher that is about to hand off and exit looks *exactly* like the game for its
        // first few seconds, so wait out the shim window before believing this child is it —
        // otherwise the lease leaves this phase on its very first poll, the reclassification above
        // never gets to run, and the hand-off that follows is read as the game exiting. On Linux
        // that ended a session ~7 s after launching any Steam title, before the game had even
        // started (on glass, .41).
        //
        // ⚠ This used to be skipped whenever the title had **no** detect signals, on the reasoning
        // that the child was then all we had — which quietly made the no-signals case the one shape
        // the shim window could not protect. It is the shape that needs it most: a hint-less title
        // is exactly the one whose launch is a bare protocol hand-off, and `spec.is_empty()` is
        // *fewer* reasons to trust the child, not more. On Windows every launch recipe is a
        // hand-off by construction (`explorer.exe "playnite://…"`, `Steam.exe "steam://…"`), so
        // carrying its pid (0.30) made a hint-less title report `running` on its first poll and
        // `exited` a second later, when the forwarder quit — ending the session and dropping the
        // stream while the game was still starting. Both callers of the pid path already documented
        // this window as their protection; now they have it.
        let child_alive = matches!(kind, LeaseKind::Child)
            && (child.is_some() || spawned.is_some())
            && spawned_at.elapsed() >= SHIM_WINDOW;
        let live = scanner.find(&shared.spec, shared.launch_stamp);
        // The same rule for what the *scan* finds, and for the same reason. A store's launch is a
        // chain of process trees, and the ones that run before the game carry the signals the game
        // carries: Steam wraps its shader pre-caching and its Proton prefix work in the very
        // `reaper SteamLaunch AppId=<appid>` the game gets, so the first poll of a launch can match
        // a tree that was never the game.
        //
        // Latching on one poll is what costs, because the two phases are patient in opposite ways.
        // This one waits [`START_GRACE`] — five minutes — and ending it never ends the session.
        // Phase 2 waits [`EXIT_CONFIRM`] — three seconds — and ending it *does*. A single sighting
        // flips the lease from the first to the second, permanently; when that tree then exits with
        // the real game not yet started, the stream drops mid-launch. On Linux that ended a Rocket
        // League session 10 s after launch, while Steam was still compiling its shaders, and the
        // player had to launch a second time to get one that stayed up (field report 2026-08-22).
        //
        // Requiring the sighting to persist buys that back for a few seconds of `GameRunning`
        // latency and nothing else — exit detection is untouched. ⚠ It is a window, not a proof: a
        // pre-launch tree that outlives the window still latches. Signals sharp enough to tell one
        // from the other belong in [`crate::procscan`] (where Steam's shader job is already excluded
        // by name); this bounds what no signal caught.
        let scan_settled = if live.is_empty() {
            seen_since = None;
            false
        } else {
            seen_since.get_or_insert_with(Instant::now).elapsed() >= SHIM_WINDOW
        };
        // A provider saying so is as good as seeing it — better, for a title there is nothing to
        // see: it is the launcher that started the game telling us it did. This is the only way a
        // [`LeaseKind::Reported`] lease ever leaves this phase, and for a `Matched` one it just
        // gets there sooner than the scan would. Not gated by the window above: a report is the
        // launcher's own statement about the game, not an inference from a process that resembles
        // it, so there is nothing to wait out.
        let said_running = reported().is_some_and(|l| l.running);
        if scan_settled || child_alive || said_running {
            known = live.clone();
            publish(&live);
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
                // Which processes, not just how many: see [`crate::procscan::names`].
                names = ?crate::procscan::names(&live),
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
    let mut vetoed = false;
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
            // Same conclusion for a pid-shaped launch: past the start phase there is no shim
            // question left to ask, so our process going away IS the game going away when nothing
            // else identifies it.
            if spawned.is_some() && !spawned_up(&spawned) {
                spawned = None;
                if shared.spec.is_empty() {
                    finish(&shared, &on_exit, "the launched process exited");
                    return;
                }
            }
        }
        let child_alive =
            matches!(kind, LeaseKind::Child) && (child.is_some() || spawned.is_some());
        // Cheap first: are the processes we already know about still there? Only when none of them is
        // do we pay for a full scan — which is also what notices a game that re-exec'd into a new pid
        // (a launcher stub becoming the real binary, an engine relaunching itself).
        let live = {
            let still = scanner.alive(&known);
            if still.is_empty() {
                scanner.find(&shared.spec, shared.launch_stamp)
            } else {
                still
            }
        };
        if !live.is_empty() || child_alive {
            publish(&live);
            known = live;
            gone_since = None;
            vetoed = false;
            shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
        } else if let Some(said) = reported() {
            // Nothing of the game is visible to us, but its provider is still reporting on it — and
            // that report is decisive in BOTH directions, where `running_hint` below may only ever
            // delay an exit.
            //
            // The difference is what backs each claim. Steam's registry flag is a leftover that
            // survives an unclean exit, so believing it indefinitely produces a session that never
            // ends; a provider report is an event from the launcher that started the game, restated
            // continuously, and it stops counting the moment it goes stale
            // ([`crate::runstate::REPORT_TTL`]) — after which this branch simply stops being taken
            // and the scan-only path below resumes. So a *live* provider is allowed to hold the
            // session open for a game the host cannot see at all, which is the entire point for a
            // title with no detect signals, and a dead one costs at most one TTL.
            if said.running {
                gone_since = None;
                vetoed = false;
                shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
            } else {
                finish(&shared, &on_exit, "its provider reported the game stopped");
                return;
            }
        } else {
            // How long the game's processes have been CONTINUOUSLY absent. Deliberately not reset by
            // the veto below — letting it run on is exactly what bounds the veto.
            let gone_for = gone_since.get_or_insert_with(Instant::now).elapsed();
            if gone_for >= EXIT_CONFIRM {
                // Last check before ending a session: does anything outside the process scan still
                // think the game is up? Only a veto, never a reason to call it running — see
                // `procscan::running_hint`.
                let hint_running = crate::procscan::running_hint(&shared.spec) == Some(true);
                if !exit_confirmed(gone_for, hint_running) {
                    if !vetoed {
                        vetoed = true;
                        tracing::info!(
                            title = %shared.game.title,
                            veto_limit_s = VETO_LIMIT.as_secs(),
                            "no game processes found, but its launcher still reports it running — \
                             holding off on ending the session"
                        );
                    }
                } else {
                    if hint_running {
                        // The veto outlived its usefulness: nothing this scan can see has existed
                        // for VETO_LIMIT, so the launcher's opinion is stale, not early.
                        tracing::warn!(
                            title = %shared.game.title,
                            gone_for_s = gone_for.as_secs(),
                            "its launcher still reports the game running, but nothing of it has \
                             been on the box for {}s — treating that as a stale flag and ending \
                             the session",
                            VETO_LIMIT.as_secs()
                        );
                    }
                    finish(&shared, &on_exit, "the game exited");
                    return;
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Whether a game nothing can find any more counts as exited: absent for at least [`EXIT_CONFIRM`],
/// and either unopposed or absent long enough that the opposition ([`crate::procscan::running_hint`]
/// saying `Some(true)`) has been overruled by [`VETO_LIMIT`].
///
/// Split out of the watch loop because it is the one rule in this file whose *bound* is the fix:
/// the loop itself polls a live process table and cannot be unit-tested, which is how an unbounded
/// veto shipped. Pure, so the table below is the whole contract.
#[cfg(any(target_os = "linux", windows))]
fn exit_confirmed(gone_for: Duration, hint_running: bool) -> bool {
    gone_for >= EXIT_CONFIRM && (!hint_running || gone_for >= VETO_LIMIT)
}

/// Record the exit and, unless the host itself ended the game, run the session-ending action.
#[cfg(any(target_os = "linux", windows))]
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
        LeaseKind::Child | LeaseKind::Matched | LeaseKind::Reported => {
            #[cfg(target_os = "linux")]
            unix_term_ladder(shared);
            #[cfg(windows)]
            windows_term_ladder(shared);
        }
        LeaseKind::Untracked => {}
    }
}

/// The process this lease's provider reports for its game, re-resolved and pinned to its start
/// time, or `None`.
///
/// The reason the wire carries a pid at all: for a [`LeaseKind::Reported`] title the matcher finds
/// nothing by construction, so without this "End" would have no target and would silently do
/// nothing — the exact failure a spawned pid was folded into the Windows ladder to fix. Resolved at
/// the moment of use rather than stored on the lease, so a report that has since gone stale, or a
/// pid the kernel has since recycled, contributes nothing.
#[cfg(any(target_os = "linux", windows))]
fn reported_proc(shared: &LeaseShared) -> Option<crate::procscan::ProcRef> {
    let pid = shared
        .game
        .id
        .as_deref()
        .and_then(crate::runstate::opinion)
        .filter(|l| l.running)?
        .pid?;
    crate::procscan::resolve(pid)
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
    // Everything the matcher can find, plus the pid the provider reported (see `reported_proc`) —
    // which for a `Reported` lease is the only member of this set.
    let targets = || {
        let mut procs = scanner.find(&shared.spec, shared.launch_stamp);
        if let Some(p) = reported_proc(shared) {
            if !procs.iter().any(|q| q.pid == p.pid) {
                procs.push(p);
            }
        }
        procs
    };
    let signal_matched = |sig: i32| -> usize {
        // Re-scan and re-verify immediately before signalling, so a pid recycled since the last
        // sweep is never hit.
        scanner
            .alive(&targets())
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
        let still = scanner.alive(&targets()).len();
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

/// Windows: ask the game's windows to close, wait, then terminate what's left.
///
/// Same shape as the Unix ladder, different primitives — and one structural difference: the host
/// holds no `Child` here. Every Windows launch goes through a launcher or the shell (`steam://`,
/// `com.epicgames.launcher://`, `shell:AppsFolder\…`), so the game is normally *recognized* rather
/// than owned and the pid set comes from the matcher.
///
/// It does, however, know the pid it spawned ([`LeaseShared::spawned`]), and that is folded in here
/// — otherwise a title with no detect signals could not be ended at all: the matcher returns
/// nothing for an empty spec, so "End" found no pids and silently did nothing.
#[cfg(windows)]
fn windows_term_ladder(shared: &LeaseShared) {
    let scanner = crate::procscan::Scanner::system();
    let live = || {
        let mut procs = scanner.alive(&scanner.find(&shared.spec, shared.launch_stamp));
        // Re-verified like everything else, so a dead or recycled pid contributes nothing, and
        // de-duplicated: the matcher may well have found this same process by its image. The
        // provider's reported pid joins on the same terms, and for a `Reported` lease it is the
        // only thing here (see `reported_proc`).
        let mut fold = |p: crate::procscan::ProcRef| {
            if !scanner.alive(&[p]).is_empty() && !procs.iter().any(|q| q.pid == p.pid) {
                procs.push(p);
            }
        };
        if let Some(p) = shared.spawned {
            fold(p);
        }
        if let Some(p) = reported_proc(shared) {
            fold(p);
        }
        procs
    };

    let pids: Vec<u32> = live().into_iter().map(|p| p.pid).collect();
    if pids.is_empty() {
        tracing::info!(title = %shared.game.title, "the game is already gone — nothing to end");
        return;
    }
    // Polite first: a `WM_CLOSE` is what clicking the window's X does, so the game runs its own
    // shutdown and can save.
    let asked = crate::game_term::request_close(&pids);
    tracing::debug!(
        title = %shared.game.title,
        windows_asked = asked,
        procs = pids.len(),
        grace_s = TERM_GRACE.as_secs(),
        "asked the game to close"
    );
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if live().is_empty() {
            tracing::info!(title = %shared.game.title, "the game closed when asked");
            return;
        }
    }
    // Re-verify (pid, creation time) one last time, then insist. Windows recycles pids briskly, so the
    // list handed to `kill` must be freshly checked rather than the one gathered above.
    let remaining: Vec<u32> = live().into_iter().map(|p| p.pid).collect();
    let killed = crate::game_term::kill(&remaining);
    tracing::warn!(
        title = %shared.game.title,
        killed,
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killed it"
    );
}

/// End the processes an **earlier** launch adopted — the action behind
/// [`crate::session_settings::GameOnNewLaunch::End`].
///
/// Its own ladder rather than a call into [`terminate`] because the input is different in kind. That
/// one ends a game a *live lease* is tracking, and can therefore re-scan by [`DetectSpec`] and
/// signal a child it owns. By the time a player picks a new title the previous game usually has no
/// lease at all — its session ended, the lease was dropped, and all that survives is the pid set its
/// watcher published to [`crate::launchreg`]. So this signals exactly that set, and nothing else.
///
/// Blocking: the caller is a launch about to spawn, and starting the new title *before* the old one
/// has let go of the display, the audio device and the gamepad is precisely the mess this exists to
/// avoid. Bounded by [`TERM_GRACE`].
///
/// Returns how many processes were still alive when asked.
pub fn end_previous_launch(title: &str, procs: &[crate::procscan::ProcRef], why: &str) -> usize {
    let scanner = crate::procscan::Scanner::system();
    // Re-verified before every signal, so a pid recycled since the watcher last published it is
    // never hit (procscan rule 2). This is the whole safety of signalling a remembered pid.
    let live = || scanner.alive(procs);

    let first = live();
    if first.is_empty() {
        return 0;
    }
    tracing::info!(
        title,
        procs = first.len(),
        reason = why,
        "ending the previous game before launching the new one"
    );
    ask_to_close(&first);
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if live().is_empty() {
            tracing::info!(title, "the previous game closed when asked");
            return first.len();
        }
    }
    let remaining = live();
    tracing::warn!(
        title,
        remaining = remaining.len(),
        grace_s = TERM_GRACE.as_secs(),
        "the previous game did not close when asked — killing it"
    );
    force_close(&remaining);
    first.len()
}

/// Ask these processes to close the way a user would: `WM_CLOSE` on Windows, `SIGTERM` on Unix. The
/// game runs its own shutdown and can save.
fn ask_to_close(procs: &[crate::procscan::ProcRef]) {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        crate::game_term::request_close(&pids);
    }
    #[cfg(target_os = "linux")]
    for p in procs {
        // SAFETY: `kill` returns a status code and touches no memory of ours. Always a POSITIVE pid
        // — these are processes the matcher adopted, not a group this host leads, so a negative
        // target would signal an unrelated process group.
        unsafe {
            libc::kill(p.pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = procs;
}

/// Insist, for whatever ignored [`ask_to_close`].
fn force_close(procs: &[crate::procscan::ProcRef]) {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        crate::game_term::kill(&pids);
    }
    #[cfg(target_os = "linux")]
    for p in procs {
        // SAFETY: as above.
        unsafe {
            libc::kill(p.pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = procs;
}

/// Apply [`crate::session_settings::GameOnNewLaunch`] for a session about to launch `game_id`.
///
/// Called immediately **before** the spawn, so the old game is gone (or has had its grace) by the
/// time the new one starts. A no-op on the shipped default, and for a client the host holds no
/// launch records for.
///
/// ⚠ One path is ordered the other way round: a Linux **bare-spawn gamescope** launch is nested by
/// the display layer when the source opens, which is well before this point — so there the previous
/// game is closed just *after* the new one starts rather than just before. The policy still holds
/// (the old game does not linger), and the contention this ordering exists to avoid is largely moot
/// there anyway, since a nested launch brings up its own display rather than sharing one.
pub fn end_others_for_new_launch(fingerprint: Option<&str>, game_id: Option<&str>) {
    if crate::session_settings::get().game_on_new_launch
        != crate::session_settings::GameOnNewLaunch::End
    {
        return;
    }
    for other in crate::launchreg::others_still_running(fingerprint, game_id) {
        end_previous_launch(
            &other.game_id,
            &other.procs,
            "the player launched a different title",
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The grace registry: leases whose session is gone but whose game is on probation
// ---------------------------------------------------------------------------------------------

/// A lease waiting out its reconnect window. If the client comes back before the deadline the
/// pending termination is dropped and the game keeps running; if it doesn't, the game ends.
///
/// The lease object itself is **not** handed to the new session, and cannot be: by the time an entry
/// lands here its [`GameLease`] has already been dropped (the guard's `Drop` runs [`on_session_end`]
/// and then drops the lease), which cancels its watcher — and its `on_exit` action closes a
/// connection that no longer exists. What the new session re-adopts is the *game*, through
/// [`crate::launchreg`], which is what carries the original launch's reference instant across
/// sessions so a fresh lease can see a game started before it. (This doc used to claim the lease was
/// handed over; nothing ever did that, and a reconnecting session was left with no game-exit
/// detection at all.)
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
/// title matches `app`.
///
/// Returns the reprieved leases, so a caller can name what it saved (and read the launch it came
/// from) rather than being handed a bare count. They are **corpses by design** — see [`Pending`]:
/// their watchers are cancelled and their exit actions point at a dead connection. The new session
/// opens its own lease; what it needs from the old launch (the reference instant to adopt against)
/// comes from [`crate::launchreg`], not from here.
pub fn readopt(fingerprint: Option<&str>, app: Option<&str>) -> Vec<Arc<LeaseShared>> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut reprieved = Vec::new();
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
            reprieved.push(p.shared.clone());
            false
        } else {
            true
        }
    });
    reprieved
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
///
/// `launch` is this session's hold on the host's launch record ([`crate::launchreg`]), consulted for
/// one question only: has a newer session already taken this launch over?
pub fn on_session_end(
    lease: &GameLease,
    deliberate: bool,
    fingerprint: Option<&str>,
    launch: Option<&crate::launchreg::Claim>,
) {
    use crate::session_settings::GameOnSessionEnd;
    let settings = crate::session_settings::get();
    let shared = lease.shared();
    if !shared.is_trackable() || shared.state() == GameState::Exited {
        return; // nothing to end (or it already ended on its own)
    }
    // A newer session has already claimed this launch: the game this lease tracks is the game that
    // session is now streaming. Anything this policy would do to "our" game would be done to theirs,
    // so it does nothing at all.
    //
    // **Which order this runs in relative to the new session's handshake does not matter, and that is
    // the point.** The two are concurrent — the old session's stream loop exits (here) while the new
    // one is already deciding its launch. If the teardown wins the race, `superseded` is false and the
    // policy runs exactly as it always has; the new session then finds the record released and adopts
    // it through the window/liveness arms. If the handshake wins, the new session's claim is already
    // recorded when this runs, and this returns — which is the case that needed fixing: under
    // `Always`, the handshake's `readopt` would have run BEFORE this `arm_grace` and so could not
    // reprieve it, and the reaper would have ended the new session's game when the window closed.
    if launch.is_some_and(|c| c.superseded()) {
        tracing::info!(
            title = %shared.game.title,
            "this client already came back for this game — leaving it to the session that has it now"
        );
        return;
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

/// Applies [`on_session_end`] when a session's stream loop exits, whichever way it exits (`return`,
/// `?`, a `break` out of the loop, a panic-unwind) — the same RAII shape the per-title prep/undo
/// guard uses, for the same reason: teardown paths are many and easy to miss one of.
///
/// Reading `quit` at **drop** time is the point: it is what distinguishes a client that deliberately
/// stopped (or an operator who did) from one that merely vanished, and that distinction is the whole
/// safety story for [`GameOnSessionEnd::Always`](crate::session_settings::GameOnSessionEnd::Always) —
/// a vanish gets a reconnect window, a deliberate stop does not.
///
/// Both planes use it, so "the session ended" means the same thing to a game whichever way the client
/// was talking to the host.
pub struct SessionGuard {
    lease: GameLease,
    quit: Arc<AtomicBool>,
    /// Hex client fingerprint, so a reconnecting client can reclaim its own game and nothing else.
    fingerprint: Option<String>,
    /// This session's hold on the host's launch record. Held here because its lifetime is exactly the
    /// session's: its drop is what opens the reconnect window a re-dial is matched against, and it
    /// must not happen until after the policy above has read it. Rust drops fields **after** the
    /// `Drop` body, so declaring it here is what orders those two.
    launch: Option<crate::launchreg::Claim>,
}

impl SessionGuard {
    /// Bind `lease` to the calling session's lifetime. `quit` is the session's deliberate-stop flag,
    /// read at drop; `fingerprint` identifies the client allowed to reclaim the game on reconnect;
    /// `launch` is this session's claim on the host's launch record ([`crate::launchreg::claim`]).
    pub fn new(
        lease: GameLease,
        quit: Arc<AtomicBool>,
        fingerprint: Option<String>,
        launch: Option<crate::launchreg::Claim>,
    ) -> Self {
        Self {
            lease,
            quit,
            fingerprint,
            launch,
        }
    }

    /// The lease's shared state, for the status surface.
    pub fn shared(&self) -> Arc<LeaseShared> {
        self.lease.shared()
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        on_session_end(
            &self.lease,
            self.quit.load(Ordering::SeqCst),
            self.fingerprint.as_deref(),
            self.launch.as_ref(),
        );
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
            launcher: false,
            child: None,
            spawned: None,
            // No start-time floor: these leases are never matched against real processes.
            launch_stamp: None,
            // Not a recorded launch — nothing here spawns anything (`crate::launchreg`).
            procs: None,
        }
    }

    /// Design D4: an entry that opens a LAUNCHER is untracked, whatever else is known about it.
    ///
    /// Both cases below are the same tile - "Steam Big Picture" - differing only in whether Steam
    /// happened to be running already, which the user cannot see:
    ///
    ///   * not running: the host's spawned child stays alive, which would otherwise be a `Child`
    ///     lease, so quitting the launcher would end the session;
    ///   * already running: the command forwards to the live instance and exits inside
    ///     `SHIM_WINDOW`, leaving nothing to track, so the session would persist.
    ///
    /// Untracked is the honest answer of the two. Big Picture is a *mode* of an already-running
    /// Steam client rather than a process, and on a Deck or SteamOS host Steam is always running,
    /// so no process signal can express "the launcher's window closed". Pinning it here keeps the
    /// tile's behaviour from depending on invisible state.
    #[test]
    fn a_launcher_entry_is_untracked_however_it_was_started() {
        // Already running: nothing held, nothing to detect.
        let mut r = req("steam:big-picture", DetectSpec::default(), false);
        r.launcher = true;
        let lease = open(r, Box::new(|| {}));
        assert!(matches!(lease.shared().kind, LeaseKind::Untracked));
        assert!(!lease.shared().is_trackable());

        // Not running: the entry also carries detect signals, which would normally make this a
        // `Matched` lease. `launcher` outranks them.
        let mut r = req(
            "steam:big-picture-2",
            DetectSpec::exe("/usr/bin/steam"),
            false,
        );
        r.launcher = true;
        assert!(
            !r.spec.is_empty(),
            "the guard is only meaningful with signals"
        );
        let lease = open(r, Box::new(|| {}));
        assert!(matches!(lease.shared().kind, LeaseKind::Untracked));
        assert!(!lease.shared().is_trackable());

        // The same request WITHOUT the flag is tracked - so the assertions above are the flag's
        // doing, not an artifact of the fixture.
        let plain = open(
            req("steam:570", DetectSpec::exe("/usr/bin/steam"), false),
            Box::new(|| {}),
        );
        assert!(matches!(plain.shared().kind, LeaseKind::Matched));
        assert!(plain.shared().is_trackable());
    }

    /// Is a lease for `id` currently on probation?
    fn is_pending(id: &str) -> bool {
        pending_snapshot()
            .iter()
            .any(|(s, _)| s.game.id.as_deref() == Some(id))
    }

    /// The exit rule, including the thing that was missing: the veto ENDS.
    ///
    /// Field 2026-08-06 (Windows 0.24.0): Steam's per-app `Running` flag was left set after the game
    /// exited, the watcher honoured it on every pass and reset its own confirm window each time, so
    /// the game read as running for the life of the host and the stream never auto-ended. The last
    /// case below is that regression.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_launcher_veto_expires_instead_of_pinning_a_session_open() {
        let brief = EXIT_CONFIRM / 2;
        let confirmed = EXIT_CONFIRM + Duration::from_secs(1);
        let long = VETO_LIMIT + Duration::from_secs(1);

        // Too early to call it either way — a process swap is still plausible.
        assert!(!exit_confirmed(brief, false));
        assert!(!exit_confirmed(brief, true));
        // Gone past the confirm window with nothing objecting: exited.
        assert!(exit_confirmed(confirmed, false));
        // Same, but the launcher objects — that is what the veto is FOR, so hold off.
        assert!(!exit_confirmed(confirmed, true));
        // …and this is the bound. Still objecting, but nothing of the game has existed for
        // VETO_LIMIT, so the objection is stale and the session ends anyway.
        assert!(exit_confirmed(long, true));
        assert!(exit_confirmed(long, false));
        // (The middle two cases together also pin VETO_LIMIT > EXIT_CONFIRM: a veto that did not
        // outlast the window it overrides could never hold anything off in the first place.)
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

    /// A title with nothing to scan for is tracked after all when its provider reports on it.
    ///
    /// This is the Playnite case the static `detect` hints could never reach: an emulated game, a
    /// manually added one, a library plugin that records no install directory. The launch is a
    /// `playnite://` hand-off, so the host holds nothing; the spec is empty, so the matcher finds
    /// nothing; and the honest verdict used to be [`LeaseKind::Untracked`] — no exit detection, and
    /// `POST /game/end` with nothing to aim at. Playnite knew the whole time.
    #[test]
    fn a_reported_title_is_tracked_where_it_used_to_be_untracked() {
        // The same request with no provider reporting: unchanged, and the control for what follows.
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
        assert!(!l.shared().is_trackable());
        drop(l);

        // A provider that speaks for the title — while reporting it NOT running, which is exactly
        // what a report looks like at the moment a game is launched. Trackability follows from the
        // provider *reporting*, not from what it currently says; a lease whose kind flipped with
        // the answer would make both lifetime behaviours depend on a plugin's timing.
        crate::runstate::report(
            "playnite-lease-test",
            ["playnite:lease-test".to_string()].into_iter().collect(),
            std::collections::HashMap::new(),
        );
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Reported));
        assert!(
            l.shared().is_trackable(),
            "so its exit is noticed and `POST /game/end` has a target"
        );
        drop(l);
        crate::runstate::forget("playnite-lease-test");

        // …and once the provider is gone, so is the tracking. Pinned because a report that outlived
        // its plugin is the one way this could hold a session open forever.
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
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
        assert!(readopt(Some("fp-other"), Some(id)).is_empty());
        assert!(readopt(Some("fp-130"), Some("steam:9999")).is_empty());
        // A missing fingerprint on either side must not reprieve anything either — otherwise any
        // unidentified reconnect could keep any game alive.
        assert!(readopt(None, Some(id)).is_empty());
        assert!(is_pending(id), "none of those should have reprieved it");
        // The right client coming back for the right title does — and names what it saved.
        let saved = readopt(Some("fp-130"), Some(id));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].game.id.as_deref(), Some(id));
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
        assert_eq!(readopt(Some("fp-140"), Some(id)).len(), 1);
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
        assert_eq!(readopt(Some("fp-151"), Some(b)).len(), 1);
    }

    /// A launcher that hands off and exits must never be mistaken for the game.
    ///
    /// This is the `steam steam://rungameid/…` shape: the host spawns a launcher as its own child,
    /// the launcher tells the already-running Steam to start the game and exits within a couple of
    /// seconds, and the game itself appears later (or, here, never). Ending the session on that exit
    /// is the failure this guards — it killed Linux Steam launches ~7 s in, before the game started.
    ///
    /// The spec deliberately matches nothing: what is asserted is that a *quick, successful* child
    /// exit is treated as a hand-off to wait out, not as the game being gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_launcher_that_hands_off_and_exits_is_not_the_game_exiting() {
        use std::sync::atomic::AtomicUsize;

        static EXITS: AtomicUsize = AtomicUsize::new(0);
        EXITS.store(0, Ordering::SeqCst);
        // Resolved through PATH, not `/bin/true`: NixOS ships only `/bin/sh` in `/bin`, so the
        // absolute path made this test — and nothing else about the code under test — fail there.
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn the fake launcher");
        let lease = open(
            LeaseRequest {
                game: GameRef {
                    id: Some("steam:999001".into()),
                    store: Some("steam".into()),
                    title: "Handoff".into(),
                },
                client: "test".into(),
                plane: crate::events::Plane::Native,
                // A real signal that no process will ever match — the game never shows up.
                spec: DetectSpec::steam(999_001),
                nested: false,
                launcher: false,
                child: Some((child, false)),
                spawned: None,
                launch_stamp: None,
                procs: None,
            },
            Box::new(|| {
                EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // Past the shim window plus the exit-confirmation window: comfortably longer than the buggy
        // path took to declare the game gone.
        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(2));
        assert_eq!(
            EXITS.load(Ordering::SeqCst),
            0,
            "a launcher handing off must not end the session"
        );
        assert_ne!(
            lease.shared().state(),
            GameState::Exited,
            "the game never ran, so it cannot have exited"
        );
    }

    /// A launch the host knows only as a **pid** — no `Child`, no detect signals — is still its
    /// own child, and must classify as one.
    ///
    /// This is the Windows shape (`CreateProcessAsUserW` returns a pid) reproduced on Linux, where
    /// it can actually be driven. Before the pid was carried, this exact combination was
    /// [`LeaseKind::Untracked`]: nothing watched the game, nothing could end it, and the console
    /// reported it running forever.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_only_launch_is_tracked_like_a_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the fake game");
        let pid = child.id();
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                // The field-report case: the provider gave nothing to recognize the game by.
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("custom:pid-only", DetectSpec::default(), false)
            },
            Box::new(|| {}),
        );
        assert!(
            matches!(lease.shared().kind(), LeaseKind::Child),
            "a pid we spawned is our own child, whatever the spec says"
        );
        assert!(
            lease.shared().is_trackable(),
            "and therefore endable — `POST /game/end` had no pid to signal before this"
        );
        drop(lease);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The same launch, driven to its exit: the pid dying is the game exiting, and that fires the
    /// action that ends the session — which is precisely what never happened in the field report.
    ///
    /// ⚠ The process must outlive [`SHIM_WINDOW`] for that reading to be the right one. It used to
    /// be a 4-second `sleep`, which is *inside* the window — the test passed only because a lease
    /// with no detect signals skipped the window entirely, which is the bug the sibling test below
    /// pins. Keep this fixture longer than the window: a launch that quits sooner is a hand-off, and
    /// treating it as a game exit is what dropped the stream a second after every Windows launch.
    ///
    /// Ignored by default: it outlives the shim window and then waits out [`EXIT_CONFIRM`], ~12 s.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
    fn a_pid_only_launch_reports_its_exit() {
        use std::sync::atomic::AtomicUsize;

        // Reaped on its own thread: an unwaited child becomes a zombie, and a zombie keeps its
        // `/proc/<pid>` entry with an unchanged start time — so the scan would call it alive
        // forever and the exit under test could never be observed.
        let mut child = std::process::Command::new("sleep")
            .arg("8")
            .spawn()
            .expect("spawn the fake game");
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static PID_EXITS: AtomicUsize = AtomicUsize::new(0);
        PID_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("custom:pid-only-exit", DetectSpec::default(), false)
            },
            Box::new(|| {
                PID_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && shared.state() != GameState::Exited {
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(shared.state(), GameState::Exited, "the game should be gone");
        assert_eq!(
            PID_EXITS.load(Ordering::SeqCst),
            1,
            "the player quitting must end the session exactly once"
        );
    }

    /// 🛑 The 2026-08-18 field report, in one test: a Windows launch is a **protocol hand-off**, and
    /// a hand-off must never be mistaken for the game exiting.
    ///
    /// The shape is `explorer.exe "playnite://playnite/start/<id>"` — the host spawns a forwarder,
    /// gets its pid, and the forwarder quits about a second later having handed the launch to
    /// Playnite. The title carries no detect hint (the Playnite plugin only sends `install_dir` when
    /// Playnite knows one), so the lease has the pid and nothing else.
    ///
    /// What shipped in 0.30 did this: the empty spec skipped [`SHIM_WINDOW`], so the lease called
    /// the forwarder "the game running" on its first poll, and a second later called the
    /// forwarder's exit "the game exited" — closing the connection with `APP_EXITED`. The player
    /// saw the game start on the host and the stream drop, with the console reporting no running
    /// game. Two things have to hold for that not to happen, and both are asserted here.
    ///
    /// Ignored by default: it must outlive [`SHIM_WINDOW`] and [`EXIT_CONFIRM`] to prove the
    /// session is not ended *later* either.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~10s (shim window + exit confirmation)"]
    fn a_pid_only_handoff_with_no_signals_never_ends_the_session() {
        use std::sync::atomic::AtomicUsize;

        // Reaped on its own thread — see the sibling test: a zombie keeps its `/proc` entry and
        // would read as alive forever.
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .expect("spawn the fake forwarder");
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static HANDOFF_EXITS: AtomicUsize = AtomicUsize::new(0);
        HANDOFF_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("playnite:handoff", DetectSpec::default(), false)
            },
            Box::new(|| {
                HANDOFF_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();

        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(2));
        assert_eq!(
            HANDOFF_EXITS.load(Ordering::SeqCst),
            0,
            "a launch command handing off must not end the session — this is the field report"
        );
        // ...and the console must not be told the game is up either. `Untracked` is the honest
        // answer: nothing is watching this title, so nothing will ever report it starting or
        // stopping. Sitting at `Launching` (or claiming `Running`) are the two lies 0.30 set out
        // to remove, and giving up on tracking must not quietly reinstate one of them.
        assert_eq!(
            shared.state(),
            GameState::Untracked,
            "nothing is watching this title any more, and the row has to say so"
        );
    }

    /// 🛑 The 2026-08-22 field report: a **pre-launch** process tree must not be mistaken for the
    /// game.
    ///
    /// Steam wraps its shader pre-caching in the same `SteamLaunch AppId=` reaper the game itself
    /// gets, so the first poll of a launch matches a tree that was never the game. What shipped
    /// latched on that single sighting: the lease left the start phase immediately, and when the
    /// compile finished and that tree exited — with Rocket League still starting — the exit watch
    /// called it the game exiting and closed the session with `APP_EXITED`, 10 s after launch. On
    /// the player's screen the stream dropped mid-"Processing Vulkan shaders"; their workaround was
    /// to launch the game twice.
    ///
    /// The scanner now knows Steam's replayer by name ([`crate::procscan`]). This pins the bound
    /// behind that: a matched process that does not outlive [`SHIM_WINDOW`] never arms the exit
    /// watch, whatever it was — which is what covers the pre-launch trees nobody has named yet.
    ///
    /// Ignored by default: it outlives the shim window and then waits out [`EXIT_CONFIRM`], ~11 s.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~11s (shim window + exit confirmation)"]
    fn a_pre_launch_tree_that_exits_never_ends_the_session() {
        use std::sync::atomic::AtomicUsize;

        // The stand-in has to keep the name `sleep`: coreutils is a multi-call binary that
        // dispatches on `argv[0]`, and under any other name it exits instantly — which would pass
        // this test for entirely the wrong reason. (Same trap as the live matcher test in
        // [`crate::procscan`].)
        let td = tempfile::tempdir().expect("tempdir");
        let stand_in = td.path().join("sleep");
        std::fs::copy("/bin/sleep", &stand_in).expect("copy a stand-in pre-launch binary");
        let launch_stamp = launch_clock();

        // Alive for less than the shim window — Steam's shader job, in miniature.
        let mut child = std::process::Command::new(&stand_in)
            .arg("3")
            .spawn()
            .expect("spawn the fake pre-launch tree");
        // Reaped on its own thread: a zombie keeps its `/proc` entry with an unchanged start time,
        // so the scan would call it alive forever and the exit under test never happen.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static PRE_EXITS: AtomicUsize = AtomicUsize::new(0);
        PRE_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                launch_stamp,
                // No child and no pid: the scan is the only signal, which is the field-report shape
                // (`steam steam://rungameid/…` had already handed off and exited).
                ..req("steam:pre-launch", DetectSpec::dir(td.path()), false)
            },
            Box::new(|| {
                PRE_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();
        assert!(matches!(shared.kind(), LeaseKind::Matched));

        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(3));
        assert_eq!(
            PRE_EXITS.load(Ordering::SeqCst),
            0,
            "a tree that ran before the game must not end the session when it exits — this is the \
             field report"
        );
        assert_ne!(
            shared.state(),
            GameState::Exited,
            "the game never started, so nothing of it can have exited"
        );
    }

    /// The whole point of the module, against a real process: a `Child` lease sees its game running,
    /// notices when it exits, and reports that exit exactly once.
    ///
    /// Ignored by default because it must outlive [`SHIM_WINDOW`] to prove the game was not mistaken
    /// for a launcher shim, and then wait out [`EXIT_CONFIRM`] — about 12 s. Run it on a Linux box
    /// with `cargo test -p punktfunk-host -- --ignored gamelease`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
    fn a_real_child_is_tracked_from_running_to_exited() {
        use std::os::unix::process::CommandExt;
        use std::sync::atomic::AtomicUsize;

        let td = tempfile::tempdir().expect("tempdir");
        let script = td.path().join("game.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let launch_stamp = launch_clock();
        let child = std::process::Command::new("/bin/sh")
            .arg(&script)
            .process_group(0)
            .spawn()
            .expect("spawn the fake game");

        static EXITS: AtomicUsize = AtomicUsize::new(0);
        EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                game: GameRef {
                    id: Some("custom:live".into()),
                    store: Some("custom".into()),
                    title: "Live Child".into(),
                },
                client: "test".into(),
                plane: crate::events::Plane::Native,
                spec: DetectSpec::dir(td.path()),
                nested: false,
                launcher: false,
                child: Some((child, true)),
                spawned: None,
                launch_stamp,
                procs: None,
            },
            Box::new(|| {
                EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();
        assert!(matches!(shared.kind(), LeaseKind::Child));

        // This script `exec`s a binary OUTSIDE the install dir, so neither the image path nor the
        // command line carries the directory: the host's own child is the only signal there is.
        // Which means the shim window gates it — a live child that might still turn out to be a
        // launcher is not called the game until that window has passed. Waiting it out is the point
        // of the assertion, not an accident of timing.
        std::thread::sleep(SHIM_WINDOW + Duration::from_millis(1_500));
        assert_eq!(
            shared.state(),
            GameState::Running,
            "a child that outlives the shim window IS the game"
        );
        assert_eq!(EXITS.load(Ordering::SeqCst), 0, "nothing has exited yet");

        terminate(shared.clone(), "test asked");

        // The ladder asks politely first; `sleep` dies on SIGTERM, so this resolves well inside the
        // termination grace, and the watcher then confirms it gone.
        let deadline = Instant::now() + TERM_GRACE + EXIT_CONFIRM + Duration::from_secs(4);
        while Instant::now() < deadline && shared.state() != GameState::Exited {
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(shared.state(), GameState::Exited, "the game should be gone");
        // The host asked for this exit, so it must NOT also end the session — that is the difference
        // between "the player quit" and "we closed it".
        assert_eq!(
            EXITS.load(Ordering::SeqCst),
            0,
            "a host-requested end must not fire the session-ending action"
        );
    }

    /// Wherever the host can match processes at all, a launch MUST have a reference instant to
    /// match them against — it is the only thing standing between this feature and ending a copy of
    /// the game the player already had open. `None` here doesn't fail loudly; it silently turns the
    /// filter off ([`crate::procscan::Scanner::find`] skips it), so nothing downstream can notice.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn a_launch_always_gets_a_reference_instant_to_adopt_against() {
        assert!(
            launch_clock().is_some(),
            "no launch reference on a platform that matches processes — every pre-existing \
             instance of a title would be adoptable, and endable"
        );
    }

    #[test]
    fn state_strings_are_stable() {
        // These reach the API and the console; renaming one is a wire change.
        assert_eq!(GameState::Launching.as_str(), "launching");
        assert_eq!(GameState::Running.as_str(), "running");
        assert_eq!(GameState::Exited.as_str(), "exited");
        assert_eq!(GameState::Untracked.as_str(), "untracked");
        assert_eq!(GameState::from_u8(1), GameState::Running);
        assert_eq!(GameState::from_u8(3), GameState::Untracked);
        assert_eq!(GameState::from_u8(99), GameState::Launching);
    }

    /// The 2026-08-16 field report in one assertion: a title with nothing to recognize it by must
    /// not report `running`. It used to, which made the console assert liveness the host had no way
    /// to back up — and left a user who had just quit the game watching a row that would never
    /// change, with no setting that could affect it.
    #[test]
    fn a_lease_with_nothing_to_watch_says_so_instead_of_claiming_to_run() {
        let lease = open(
            req("custom:no-signals", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(!lease.shared().is_trackable());
        assert_eq!(
            lease.shared().state(),
            GameState::Untracked,
            "an unwatchable lease must not report `running`"
        );
    }

    /// The other half of that rule: a nested lease with no store signals IS watched, just from the
    /// capture loop rather than from here (a bare-spawn gamescope dies with its app). Its game
    /// really is running and its exit really does end the session, so it keeps saying `running` —
    /// the fix must not flatten the two cases together.
    #[test]
    fn a_nested_lease_still_reports_running_because_something_else_watches_it() {
        let lease = open(
            req("steam:nested-no-signals", DetectSpec::default(), true),
            Box::new(|| {}),
        );
        assert_eq!(lease.shared().state(), GameState::Running);
    }
}
