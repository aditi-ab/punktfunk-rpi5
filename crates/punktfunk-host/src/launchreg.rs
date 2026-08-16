//! What this host launched, for whom, and when — the record a *second* session needs in order not to
//! launch the same title twice (design/session-game-lifetime.md).
//!
//! A client that re-dials mid-session re-sends its `Hello::launch` **verbatim**. It cannot drop the
//! field: on Linux the per-session gamescope is re-adopted through pf-vdisplay's display registry,
//! whose reuse key includes the launch command, so a retry without it orphans the running game. The
//! host therefore has to be the one that notices, and two things go wrong when it doesn't:
//!
//! * **the title is launched twice.** `steam://rungameid`, Epic's launcher URI and an AUMID
//!   activation all dedupe inside the launcher — it focuses the copy that is already up — but a
//!   `gog:` or `custom:` target is a plain spawn and really does start a second copy of the game.
//! * **the reconnected session never notices the game exit.** A fresh session mints a fresh
//!   [`crate::gamelease::launch_clock`] stamp, and [`crate::procscan`] refuses to adopt any process
//!   that started more than [`crate::procscan::START_SLACK_SECS`] before it. The game was started by
//!   the *original* session, minutes earlier, so it can never be adopted — and the reconnected
//!   session has no game-exit detection for the rest of its life.
//!
//! ### Why a registry, and not "is this title already running?"
//!
//! Because that second question has a catastrophic answer. [`crate::procscan`]'s first rule is that a
//! process predating the launch is never adopted: a player may already have the game open when a
//! session starts, and treating that instance as "this session's game" would let a session end kill
//! something it never started — on Windows the host runs as SYSTEM and can signal anything. A
//! registry of the host's **own** launches preserves that rule *by construction*: a game the player
//! started for themselves was never recorded here, so it can never be reclaimed from here, and the
//! only reference instant a session can inherit is one this host took immediately before a spawn it
//! performed itself.
//!
//! The same care runs through the liveness probe: it only ever *re-verifies the processes a lease
//! actually adopted* ([`Liveness`]), never re-scans by [`crate::library::DetectSpec`]. A fresh scan
//! would find a copy the player started since, which is exactly the process rule 1 exists to keep out.
//!
//! ### Not the grace registry
//!
//! Deliberately separate from [`crate::gamelease::arm_grace`]. That one is **policy**: it exists only
//! under `GameOnSessionEnd::Always` and a non-deliberate end, so on the shipped default (`Keep`) it is
//! empty — which is precisely the configuration both defects above were reported on. This one is a
//! **record**: written whenever the host launches a title, whatever the operator's termination policy
//! says, and read at the next session's launch decision. Different owners, different lifetimes, and
//! no shared state between them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long after its last session let go a launch is still treated as the *same* launch even though
/// nothing of it has ever been seen running.
///
/// This window covers exactly one shape: a client that tears its session down and re-dials while the
/// launcher is still bringing the game up. The presenter's HEVC→H.264 codec fallback does that within
/// seconds; a client crash-and-restart within tens of them. Once the game *has* been seen, [`Liveness`]
/// answers the question exactly and this window stops mattering — and a launch whose processes are
/// confirmed gone is re-launchable immediately, whatever the window says.
///
/// Kept short on purpose. The cost of it being too long is a title the player asked for and did not
/// get, which is a far worse failure than the second copy it exists to prevent.
const IN_FLIGHT_WINDOW: Duration = Duration::from_secs(90);

/// How long an unheld record survives at all, so the registry can't grow without bound across a long
/// host uptime. Generous: a launch idle this long whose game is somehow *still* running gets started
/// again, which is exactly what the host did before this module existed.
const MAX_RECORD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The processes a launch's watcher adopted, published as it sees them so the record can still answer
/// "is *our* launch up?" after the session — and therefore the watcher — is gone.
///
/// Written by [`crate::gamelease`]'s watch loop, read here. Only ever re-verified through
/// [`crate::procscan::Scanner::alive`], which re-checks each process's start time and so cannot be
/// fooled by a recycled pid (rule 2), and never re-scanned by spec (rule 1 — see the module docs).
pub type LiveProcs = Arc<Mutex<Vec<crate::procscan::ProcRef>>>;

/// What became of the processes a recorded launch adopted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// At least one process this launch adopted is still the same live process.
    Running,
    /// Every process it adopted is gone. The launch is over.
    Gone,
    /// No opinion — nothing was ever adopted (the game has not appeared yet, or the title has no
    /// detect signals and its only liveness signal was a child handle that died with its session), or
    /// this platform has no process matcher at all (macOS, which has no launch path either).
    ///
    /// The no-signals case is worth naming: a title the host can only track through the child it
    /// spawned is [`crate::gamelease::LeaseKind::Child`], and adopting it hands the new session a
    /// lease with no child and no signals — [`crate::gamelease::LeaseKind::Untracked`], for which
    /// both lifetime behaviors were already inert. So inside [`IN_FLIGHT_WINDOW`] such a reconnect
    /// trades game-exit detection for not handing the player a second copy of the game. That is the
    /// right way round: the missing detection is an annoyance, a second running copy is not.
    Unknown,
}

/// What a starting session must do about the title it was asked to launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Start it, and adopt its processes against the freshly minted reference instant.
    Spawn,
    /// This host already launched this title for this client and that launch is still ours: do **not**
    /// start a second copy, and adopt against the **original** launch's reference instant so the
    /// lease can still see (and therefore notice the exit of) the game that is already running.
    Adopt,
}

/// Who a launch belongs to. Both halves are required — see [`key_for`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    fingerprint: String,
    game_id: String,
}

/// The identity half of the match rule, pure and total: which (client, title) pair may ever reclaim a
/// launch. `None` = this launch is not recordable at all, so it is started exactly as it always was.
///
/// Both sides must name a client **and** a title:
///
/// * no fingerprint — an anonymous client (TOFU / `--open`, and the whole GameStream compat plane) —
///   because otherwise any unidentified client could reclaim any other unidentified client's launch,
///   and on Linux the second client would then get its own empty display with the first one's game
///   nowhere on it. This is the same conservatism [`crate::gamelease::readopt`] already applies.
/// * no library id — an operator-typed `apps.json` command, which has no library entry behind it — because
///   every such launch would otherwise share the one `None` id and reclaim each other.
///
/// Both exclusions are the safe direction: the affected launches simply keep the pre-existing
/// behavior (start it again), rather than reclaiming something that might not be theirs.
pub fn key_for(fingerprint: Option<&str>, game_id: Option<&str>) -> Option<Key> {
    Some(Key {
        fingerprint: fingerprint?.to_string(),
        game_id: game_id?.to_string(),
    })
}

impl Key {
    /// The fingerprint prefix the rest of the host logs clients by (`client_label`), so a launch line
    /// can be lined up with the session lines around it without printing a full cert hash.
    fn short_client(&self) -> &str {
        self.fingerprint.get(..12).unwrap_or(&self.fingerprint)
    }
}

/// One launch this host performed, for one client, for one title.
struct Record {
    key: Key,
    /// The reference instant taken immediately before that launch ([`crate::gamelease::launch_clock`]).
    /// This is the value a reconnecting session inherits; `None` only ever means "this platform has no
    /// process-start clock", never "we failed to inherit one" — a session that inherits nothing gets
    /// [`Plan::Spawn`] and its own fresh stamp instead.
    stamp: Option<f64>,
    /// The processes the launch's lease adopted; see [`LiveProcs`].
    procs: LiveProcs,
    /// Set once the launch *actually happened*. A record made by a session that then failed to spawn
    /// (or never had a launch path at all) is never matched — nothing is running for it to reclaim.
    launched: bool,
    /// How many live sessions hold this record. A count, not a flag: an old session's teardown and a
    /// new session's launch decision overlap, and the old one releasing must never zero out the new
    /// one's hold.
    holders: u32,
    /// When the last holder let go. `None` while held.
    released_at: Option<Instant>,
    /// The newest claim taken on this record. An older session compares its own claim against this to
    /// find out that its game now belongs to somebody else ([`Claim::superseded`]).
    claim: u64,
}

impl Record {
    fn new(key: Key, stamp: Option<f64>, claim: u64) -> Self {
        Self {
            key,
            stamp,
            // A fresh slot per launch: the previous launch's dead processes must never be inherited
            // by the new one, and its lease may still be writing into the old handle.
            procs: Arc::new(Mutex::new(Vec::new())),
            launched: false,
            holders: 1,
            released_at: None,
            claim,
        }
    }
}

/// **The match rule.** Does `rec` cover a new session's request to launch the title it is keyed on?
///
/// Pure and total: the caller supplies the liveness verdict, the clock and the window, so the rule is
/// unit-testable without a live session, a process table or real time. Identity is not checked here —
/// it is the record's key, decided once by [`key_for`].
///
/// Liveness is authoritative wherever it has an opinion. Only when it has none do the two tie-breakers
/// apply, and they are the two shapes a reconnect actually takes: another session is holding the
/// launch right now (the teardown and the re-dial overlapped), or the client came back promptly while
/// the launcher was still working (the [`IN_FLIGHT_WINDOW`]).
fn covers(rec: &Record, live: Liveness, now: Instant, window: Duration) -> bool {
    // A launch that never happened has nothing running to reclaim, and inheriting its reference
    // instant would hand the new lease a floor with no game above it.
    if !rec.launched {
        return false;
    }
    match live {
        // Processes this very launch adopted are still alive. This IS the game — start a second copy
        // and the player gets two.
        Liveness::Running => true,
        // Every process it adopted is dead. Whatever the client is asking for now, it is not this
        // launch — so a title that crashed on startup, or that the player quit, launches again at once.
        Liveness::Gone => false,
        Liveness::Unknown => {
            rec.holders > 0
                || rec
                    .released_at
                    .is_some_and(|t| now.saturating_duration_since(t) <= window)
        }
    }
}

/// Re-verify the processes this launch adopted. Never a fresh scan — see the module docs.
fn liveness(rec: &Record) -> Liveness {
    let procs = rec.procs.lock().unwrap_or_else(|e| e.into_inner());
    if procs.is_empty() {
        return Liveness::Unknown;
    }
    match alive_count(&procs) {
        Some(0) => Liveness::Gone,
        Some(_) => Liveness::Running,
        None => Liveness::Unknown,
    }
}

/// How many of `procs` are still the same live processes. `None` on a platform with no matcher
/// (macOS), which is "no opinion" — never "gone".
fn alive_count(procs: &[crate::procscan::ProcRef]) -> Option<usize> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Some(crate::procscan::Scanner::system().alive(procs).len())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = procs;
        None
    }
}

/// Forget records nothing can reclaim: an unheld launch that never happened, and an unheld one idle
/// past [`MAX_RECORD_AGE`]. Deliberately free of any process scan — it runs under the registry lock,
/// on the one path that touches the registry at all (a session deciding its launch).
fn sweep(recs: &mut Vec<Record>, now: Instant) {
    recs.retain(|r| {
        if r.holders > 0 {
            return true;
        }
        let idle = r
            .released_at
            .map_or(Duration::ZERO, |t| now.saturating_duration_since(t));
        r.launched && idle < MAX_RECORD_AGE
    });
}

struct Reg {
    records: Mutex<Vec<Record>>,
    next_claim: AtomicU64,
}

fn reg() -> &'static Reg {
    static REG: OnceLock<Reg> = OnceLock::new();
    REG.get_or_init(|| Reg {
        records: Mutex::new(Vec::new()),
        // 0 is reserved for an unrecorded claim, which must never look current.
        next_claim: AtomicU64::new(1),
    })
}

/// One of this client's earlier launches that is still running — the input to
/// [`crate::session_settings::GameOnNewLaunch::End`].
pub struct StillRunning {
    /// The store-qualified library id it was launched as.
    pub game_id: String,
    /// The processes its lease adopted, as last published. Re-verified by the caller immediately
    /// before anything is signalled, so a pid recycled since is never hit (rule 2).
    pub procs: Vec<crate::procscan::ProcRef>,
}

/// Every **other** title this client has running on this host from a launch the host performed.
///
/// The safety of ending-on-new-launch rests entirely on where this list comes from, so it is worth
/// being explicit about what it can never contain:
///
/// * **another client's game.** Records are keyed by cert fingerprint, and this filters on it — so
///   one device picking a new title can never close a game somebody else is mid-way through.
/// * **a game the player started themselves.** Only the host's own launches are recorded here at
///   all; a copy started at the machine was never written, so it cannot be read back out (rule 1,
///   preserved by construction rather than by a check).
/// * **the title being launched now.** Filtered by `keep_game_id`, so relaunching what is already
///   running still resolves to [`Plan::Adopt`] rather than closing the game and starting it again.
/// * **anything already gone.** Liveness is re-verified per record, and only [`Liveness::Running`]
///   qualifies — `Unknown` is deliberately excluded, because "no opinion" must never authorize
///   signalling a process.
pub fn others_still_running(
    fingerprint: Option<&str>,
    keep_game_id: Option<&str>,
) -> Vec<StillRunning> {
    let Some(fp) = fingerprint else {
        // An anonymous client (TOFU/`--open`, and the whole GameStream plane) owns no records, and
        // must not be able to reach anyone else's.
        return Vec::new();
    };
    reg()
        .records
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|r| is_other_running(r, fp, keep_game_id, liveness(r)))
        .map(|r| StillRunning {
            game_id: r.key.game_id.clone(),
            procs: r
                .procs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .copied()
                .collect(),
        })
        .collect()
}

/// The filter behind [`others_still_running`] — pure, so the four rules that make ending-on-launch
/// safe can be tested without a registry, a process table or a clock.
///
/// The caller supplies the liveness verdict for the same reason [`covers`] takes one.
fn is_other_running(rec: &Record, fp: &str, keep_game_id: Option<&str>, live: Liveness) -> bool {
    rec.key.fingerprint == fp
        && Some(rec.key.game_id.as_str()) != keep_game_id
        && rec.launched
        // `Unknown` deliberately does NOT qualify. It is the verdict for a launch that adopted
        // nothing — a title with no detect signals, or a platform with no matcher — and "no opinion
        // about what is running" must never authorize signalling a process.
        && live == Liveness::Running
}

/// Decide what this session must do about its launch, and claim the answer.
///
/// `fresh_stamp` is this session's own [`crate::gamelease::launch_clock`] reading, taken before
/// anything spawns; it is used when the answer is [`Plan::Spawn`], and discarded in favour of the
/// recorded one when it is [`Plan::Adopt`].
///
/// The returned [`Claim`] is an RAII guard: hold it for the whole session (see
/// [`crate::gamelease::SessionGuard`]), because its drop is what starts the reconnect window.
pub fn claim(fingerprint: Option<&str>, game_id: Option<&str>, fresh_stamp: Option<f64>) -> Claim {
    let Some(key) = key_for(fingerprint, game_id) else {
        // Nothing to key a record on. Launch exactly as this host always has.
        return Claim {
            key: None,
            id: 0,
            plan: Plan::Spawn,
            stamp: fresh_stamp,
            procs: None,
        };
    };
    let reg = reg();
    let now = Instant::now();
    let mut recs = reg.records.lock().unwrap_or_else(|e| e.into_inner());
    // Allocated **under the lock**, so claim ids and record writes agree on their order. An id handed
    // out before the lock could be stamped onto a record after a higher one had been, and then neither
    // session would see itself superseded.
    let id = reg.next_claim.fetch_add(1, Ordering::Relaxed);
    sweep(&mut recs, now);

    let procs = if let Some(i) = recs.iter().position(|r| r.key == key) {
        let live = liveness(&recs[i]);
        let rec = &mut recs[i];
        if covers(rec, live, now, IN_FLIGHT_WINDOW) {
            rec.holders += 1;
            rec.released_at = None;
            rec.claim = id;
            let (stamp, procs) = (rec.stamp, rec.procs.clone());
            drop(recs);
            tracing::info!(
                app = %key.game_id,
                client = %key.short_client(),
                ?live,
                "this client's own launch of this title is still this host's — adopting it instead \
                 of starting a second copy"
            );
            return Claim {
                key: Some(key),
                id,
                plan: Plan::Adopt,
                stamp,
                procs: Some(procs),
            };
        }
        // The previous launch of this title by this client is over (or never happened). Re-stamp the
        // record for the launch about to happen: a fresh reference instant and a fresh process slot,
        // so the dead launch's processes can never be inherited by the new one.
        //
        // Reset in place rather than replaced, because `holders` must survive: an older session may
        // still be holding this record, and its release has to decrement the count it incremented.
        rec.stamp = fresh_stamp;
        rec.procs = Arc::new(Mutex::new(Vec::new()));
        rec.launched = false;
        rec.holders += 1;
        rec.released_at = None;
        rec.claim = id;
        rec.procs.clone()
    } else {
        let rec = Record::new(key.clone(), fresh_stamp, id);
        let procs = rec.procs.clone();
        recs.push(rec);
        procs
    };
    drop(recs);
    tracing::debug!(
        app = %key.game_id,
        client = %key.short_client(),
        "recording this host's launch of the title"
    );
    Claim {
        key: Some(key),
        id,
        plan: Plan::Spawn,
        stamp: fresh_stamp,
        procs: Some(procs),
    }
}

/// A session's hold on its launch record. Its drop starts the reconnect window.
pub struct Claim {
    /// `None` for an unrecordable launch ([`key_for`]) — every method is then inert.
    key: Option<Key>,
    id: u64,
    plan: Plan,
    stamp: Option<f64>,
    procs: Option<LiveProcs>,
}

impl Claim {
    /// Must this session actually start the title?
    pub fn must_spawn(&self) -> bool {
        matches!(self.plan, Plan::Spawn)
    }

    /// The reference instant this session's lease must adopt against
    /// ([`crate::gamelease::LeaseRequest::launch_stamp`]) — freshly taken for a [`Plan::Spawn`],
    /// inherited from the original launch for a [`Plan::Adopt`].
    ///
    /// `None` here always means what it means everywhere else in [`crate::procscan`]: this platform
    /// has no process-start clock, so there is no start-time filter. It never means "inheritance
    /// failed" — a session that finds nothing to inherit is given [`Plan::Spawn`] and its own fresh
    /// reading instead.
    pub fn stamp(&self) -> Option<f64> {
        self.stamp
    }

    /// The slot this launch's lease publishes its adopted processes into
    /// ([`crate::gamelease::LeaseRequest::procs`]). For a [`Plan::Adopt`] it is the *original*
    /// launch's slot, so the record keeps tracking the same processes across the handover.
    pub fn procs(&self) -> Option<LiveProcs> {
        self.procs.clone()
    }

    /// The launch happened. Only a confirmed record is ever matched by a later session.
    ///
    /// Ignored once a newer session has re-stamped the record: what it would be confirming is that
    /// session's launch, not this one's, and that session confirms its own.
    pub fn launched(&self) {
        self.with_record(|r| {
            if r.claim == self.id {
                r.launched = true;
            }
        });
    }

    /// The launch did not happen — it failed, or this platform has no launch path. Forget the record
    /// entirely, so a retry starts the title rather than inheriting a launch that never occurred.
    pub fn abandon(&self) {
        let Some(key) = self.key.as_ref() else {
            return;
        };
        let mut recs = reg().records.lock().unwrap_or_else(|e| e.into_inner());
        recs.retain(|r| &r.key != key || r.claim != self.id);
    }

    /// Has a **newer** session claimed this launch? `false` when there is no record at all, so only a
    /// positive signal ever changes a caller's behavior.
    pub fn superseded(&self) -> bool {
        let Some(key) = self.key.as_ref() else {
            return false;
        };
        let recs = reg().records.lock().unwrap_or_else(|e| e.into_inner());
        recs.iter().any(|r| &r.key == key && r.claim > self.id)
    }

    /// Run `f` against this claim's record, if it still exists. Deliberately **not** claim-checked:
    /// the release in [`Drop`] must decrement the very count it incremented, even after a newer
    /// session re-stamped the record. Callers that need "only if it is still mine" check `claim`
    /// themselves ([`Claim::launched`]).
    fn with_record(&self, f: impl FnOnce(&mut Record)) {
        let Some(key) = self.key.as_ref() else {
            return;
        };
        let mut recs = reg().records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = recs.iter_mut().find(|r| &r.key == key) {
            f(r);
        }
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.with_record(|r| {
            r.holders = r.holders.saturating_sub(1);
            if r.holders == 0 {
                r.released_at = Some(Instant::now());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record shaped for the pure-rule table below. Never touches the global registry.
    fn rec(launched: bool, holders: u32, released_at: Option<Instant>) -> Record {
        Record {
            key: Key {
                fingerprint: "fp".into(),
                game_id: "steam:1".into(),
            },
            stamp: Some(1.0),
            procs: Arc::new(Mutex::new(Vec::new())),
            launched,
            holders,
            released_at,
            claim: 1,
        }
    }

    /// The four rules that keep `GameOnNewLaunch::End` from closing something it must not.
    ///
    /// Each line here is a game somebody could otherwise lose mid-play, so they are asserted
    /// individually rather than through one composite case.
    #[test]
    fn ending_on_a_new_launch_only_ever_reaches_this_clients_other_live_games() {
        let mut r = rec(true, 0, None);
        r.key.game_id = "steam:1".into();

        // The case it exists for: same client, a different title, still up.
        assert!(is_other_running(
            &r,
            "fp",
            Some("steam:2"),
            Liveness::Running
        ));

        // Another device's game — one client picking a new title must never close someone else's.
        assert!(!is_other_running(
            &r,
            "other-fp",
            Some("steam:2"),
            Liveness::Running
        ));

        // The title being launched right now: that is a relaunch, which `Plan::Adopt` handles by
        // keeping the game. Closing and restarting it would be the opposite of the intent.
        assert!(!is_other_running(
            &r,
            "fp",
            Some("steam:1"),
            Liveness::Running
        ));

        // A launch that never actually happened has nothing running behind it.
        let never = rec(false, 0, None);
        assert!(!is_other_running(
            &never,
            "fp",
            Some("steam:2"),
            Liveness::Running
        ));

        // Already gone, and — the one that matters most — NO OPINION. `Unknown` means nothing was
        // ever adopted, so there is no verified pid set to signal.
        assert!(!is_other_running(&r, "fp", Some("steam:2"), Liveness::Gone));
        assert!(!is_other_running(
            &r,
            "fp",
            Some("steam:2"),
            Liveness::Unknown
        ));
    }

    /// An anonymous client owns no records, and must not be able to reach anybody else's.
    #[test]
    fn an_anonymous_client_can_never_end_another_clients_game() {
        assert!(others_still_running(None, Some("steam:2")).is_empty());
    }

    /// Identity is the record's key, and a launch that can't be keyed is never reclaimed.
    #[test]
    fn a_launch_is_keyed_by_both_the_client_and_the_title() {
        assert!(key_for(Some("fp"), Some("steam:570")).is_some());
        // An anonymous client, or a title with no library entry, is not recordable — the launch
        // behaves exactly as it did before this module existed.
        assert!(key_for(None, Some("steam:570")).is_none());
        assert!(key_for(Some("fp"), None).is_none());
        assert!(key_for(None, None).is_none());
        // Different client, or different title, is a different launch.
        assert_ne!(
            key_for(Some("a"), Some("steam:570")),
            key_for(Some("b"), Some("steam:570"))
        );
        assert_ne!(
            key_for(Some("a"), Some("steam:570")),
            key_for(Some("a"), Some("gog:1"))
        );
    }

    /// The match rule itself: liveness first, then the two tie-breakers.
    #[test]
    fn the_match_rule_puts_liveness_ahead_of_the_window() {
        let t0 = Instant::now();
        let window = Duration::from_secs(90);
        let inside = t0 + Duration::from_secs(30);
        let outside = t0 + Duration::from_secs(600);

        // A launch that never happened is never reclaimed, however alive something looks.
        let never = rec(false, 1, None);
        assert!(!covers(&never, Liveness::Running, inside, window));

        // Our own processes are still up: reclaim it, no matter how long ago the session let go.
        let old = rec(true, 0, Some(t0));
        assert!(covers(&old, Liveness::Running, outside, window));

        // Confirmed gone beats everything — including a live holder and a fresh release. This is what
        // keeps a title that crashed on startup (or that the player quit) launchable at once.
        let held = rec(true, 1, None);
        assert!(!covers(&held, Liveness::Gone, inside, window));
        assert!(!covers(&old, Liveness::Gone, inside, window));

        // Nothing seen yet: a live holder is itself the answer (the teardown and the re-dial
        // overlapped), and a prompt return is the same launch.
        assert!(covers(&held, Liveness::Unknown, inside, window));
        assert!(covers(&old, Liveness::Unknown, inside, window));
        // ...but a return long after the window, with nothing ever seen running, is a new launch.
        assert!(!covers(&old, Liveness::Unknown, outside, window));
    }

    /// Why "click the game that is already running" resumes on some hosts and started a **second
    /// copy** on others — and what fixed it.
    ///
    /// The rule above is authoritative only where liveness has an opinion, and liveness comes from
    /// the processes a launch's lease actually adopted. A lease that adopts nothing publishes
    /// nothing, answers `Unknown`, and so falls back to the 90-second window — past which the same
    /// title launches again.
    ///
    /// That is precisely the state a Windows launch used to be stuck in for any title whose
    /// provider published no detect signals: no child, no matched processes, nothing to publish.
    /// Carrying the spawned pid (`gamelease::LeaseRequest::spawned`) is what moves such a launch
    /// from the left column to the right one here.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn only_a_launch_that_adopted_something_stays_adoptable_past_the_window() {
        let t0 = Instant::now();
        let window = Duration::from_secs(90);
        let outside = t0 + Duration::from_secs(600);

        // Adopted nothing: no opinion, so past the window this launches a second copy.
        let blind = rec(true, 0, Some(t0));
        assert_eq!(liveness(&blind), Liveness::Unknown);
        assert!(!covers(&blind, liveness(&blind), outside, window));

        // Adopted a process that is demonstrably alive — this very test binary — so the record
        // answers `Running` and stays adoptable however long ago its session let go.
        let seeing = rec(true, 0, Some(t0));
        let self_ref = crate::procscan::resolve(std::process::id())
            .expect("this process must be resolvable by the scanner");
        seeing.procs.lock().unwrap().push(self_ref);
        assert_eq!(liveness(&seeing), Liveness::Running);
        assert!(
            covers(&seeing, liveness(&seeing), outside, window),
            "a launch whose game is still up must resume, not start a second copy"
        );
    }

    /// The sweep drops what nobody can reclaim and keeps what somebody can.
    #[test]
    fn the_sweep_keeps_only_reclaimable_records() {
        let t0 = Instant::now();
        let mut recs = vec![
            rec(false, 0, Some(t0)), // never launched, nobody holding
            rec(true, 0, Some(t0)),  // launched, recently released
            rec(false, 1, None),     // never launched but HELD — its session is still deciding
        ];
        sweep(&mut recs, t0 + Duration::from_secs(1));
        assert_eq!(recs.len(), 2);
        // ...and an ancient one goes too.
        let mut recs = vec![rec(true, 0, Some(t0))];
        sweep(&mut recs, t0 + MAX_RECORD_AGE + Duration::from_secs(1));
        assert!(recs.is_empty());
    }

    /// **Defect A.** A client that re-dials and re-sends `Hello::launch` verbatim must not get a
    /// second copy of its game.
    ///
    /// Before this module the host launched unconditionally at
    /// `native/stream.rs`'s launch site — i.e. the second decision here was always "spawn".
    #[test]
    fn a_reconnect_does_not_launch_the_title_a_second_time() {
        let (fp, app) = (Some("fp-double"), Some("gog:double"));
        let first = claim(fp, app, Some(100.0));
        assert!(first.must_spawn(), "the first session starts the title");
        first.launched();
        drop(first); // the session ends; the reconnect window opens

        let second = claim(fp, app, Some(900.0));
        assert!(
            !second.must_spawn(),
            "a reconnect inside the window must adopt the running launch, not start a second copy"
        );
        second.abandon(); // leave the process-global registry as we found it
    }

    /// **Defect B.** The reconnected session must adopt against the ORIGINAL launch's reference
    /// instant, or [`crate::procscan`] rejects the game — started minutes before this session — and
    /// the session has no game-exit detection for the rest of its life.
    #[test]
    fn a_reconnect_inherits_the_original_launchs_reference_instant() {
        let (fp, app) = (Some("fp-stamp"), Some("steam:stamp"));
        let first = claim(fp, app, Some(100.0));
        assert_eq!(first.stamp(), Some(100.0));
        first.launched();
        drop(first);

        // The new session mints its own (much later) reading and passes it in; the record's wins.
        let second = claim(fp, app, Some(900.0));
        assert_eq!(
            second.stamp(),
            Some(100.0),
            "the reconnect must adopt against the original launch, not against its own start"
        );
        // Spelled out, because this is exactly what the host did before: a fresh reading here is a
        // floor minutes above the running game's start time, and `procscan` rejects everything under
        // it — the reconnected session then has no game-exit detection for the rest of its life.
        assert_ne!(second.stamp(), Some(900.0));
        // Both sessions publish into the SAME slot, so the record keeps tracking the same processes
        // across the handover.
        assert!(second.procs().is_some());
        second.abandon();
    }

    /// Inheriting nothing must never turn into "adopt anything": wherever a launch has a reference
    /// instant, every decision made from it has one too.
    #[test]
    fn a_decision_never_downgrades_a_reference_instant_to_no_filter() {
        let (fp, app) = (Some("fp-filter"), Some("steam:filter"));
        let first = claim(fp, app, Some(42.0));
        assert!(first.stamp().is_some());
        first.launched();
        drop(first);
        let second = claim(fp, app, Some(99.0));
        assert!(
            second.stamp().is_some(),
            "a reconnect must never end up with the start-time filter disabled"
        );
        second.abandon();
        // A launch that cannot be recorded still carries this session's own fresh reading through.
        let anon = claim(None, app, Some(7.0));
        assert!(anon.must_spawn());
        assert_eq!(anon.stamp(), Some(7.0));
        assert!(anon.procs().is_none());
    }

    /// A launch that failed (or a platform with no launch path) leaves nothing behind: the next
    /// attempt starts the title and gets its own reference instant.
    #[test]
    fn an_abandoned_launch_is_never_reclaimed() {
        let (fp, app) = (Some("fp-fail"), Some("custom:fail"));
        let first = claim(fp, app, Some(100.0));
        assert!(first.must_spawn());
        first.abandon(); // the spawn failed
        drop(first);

        let second = claim(fp, app, Some(900.0));
        assert!(second.must_spawn(), "a failed launch must be retried");
        assert_eq!(second.stamp(), Some(900.0));
        second.abandon();
    }

    /// A confirmed launch that was never released is still adopted by an overlapping second session —
    /// the order where the new session's handshake beats the old session's teardown.
    #[test]
    fn an_overlapping_session_adopts_a_still_held_launch() {
        let (fp, app) = (Some("fp-overlap"), Some("steam:overlap"));
        let old = claim(fp, app, Some(100.0));
        old.launched();
        // The old session has NOT torn down yet.
        let new = claim(fp, app, Some(900.0));
        assert!(!new.must_spawn(), "a held launch is still ours");
        assert_eq!(new.stamp(), Some(100.0));
        // ...and the old session can see that its game now belongs to the new one, so its teardown
        // policy leaves it alone.
        assert!(old.superseded());
        assert!(!new.superseded());
        drop(old);
        assert!(
            !new.superseded(),
            "the older session releasing must not look like a newer claim"
        );
        new.abandon();
    }

    /// An unrecordable launch is inert in every direction.
    #[test]
    fn an_unrecordable_launch_never_supersedes_anything() {
        let anon = claim(None, None, None);
        assert!(anon.must_spawn());
        assert!(!anon.superseded());
        anon.launched(); // no-op
        anon.abandon(); // no-op
    }
}
