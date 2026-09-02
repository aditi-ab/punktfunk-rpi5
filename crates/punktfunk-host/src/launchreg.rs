//! What this host launched, for whom, and when.
//!
//! A reconnecting client re-sends `Hello::launch` verbatim. Linux gamescope
//! reuse keys on that command, so dropping it orphans the game. The host
//! must notice: `gog:`/`custom:` would spawn a second copy, and a fresh
//! [`crate::gamelease::launch_clock`] would put the original process outside
//! [`crate::procscan::START_SLACK_SECS`], so the session never sees the exit.
//!
//! Identity is [`key_for`]. Liveness re-verifies adopted processes only
//! ([`Liveness`]), never a [`crate::library::DetectSpec`] scan — that would
//! adopt a copy the player started (procscan rule 1; on Windows the host
//! is SYSTEM). Separate from [`crate::gamelease::arm_grace`]: that is
//! policy and empty on default `Keep`; this is a record of every launch.
//!
//! See `design/session-game-lifetime.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Covers teardown-and-redial while the launcher is still starting the
/// game (codec fallback is seconds; crash-restart is tens). Once
/// [`Liveness`] has an opinion this window is unused; confirmed-gone
/// relaunches immediately. Too long: the player asked and did not get it.
const IN_FLIGHT_WINDOW: Duration = Duration::from_secs(90);

/// Bound so the registry cannot grow across a long host uptime. Idle
/// this long, a still-running game is started again.
const MAX_RECORD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Processes this launch's watcher adopted. Survives the session so the
/// record can still ask "is *our* launch up?". Re-verified through
/// [`crate::procscan::Scanner::alive`] (pid reuse is rule 2); never
/// re-scanned by spec (rule 1 — module docs).
pub type LiveProcs = Arc<Mutex<Vec<crate::procscan::ProcRef>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Liveness {
    /// An adopted process is still the same live process (not a recycled pid).
    Running,
    Gone,
    /// No adopted processes (game not up yet, or a Child handle that died
    /// with its session), or no matcher (macOS). A reconnect inside
    /// [`IN_FLIGHT_WINDOW`] then skips a second copy and also skips
    /// game-exit detection.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Spawn, and adopt against this session's freshly minted stamp.
    Spawn,
    /// Do not spawn. Adopt against the original launch's stamp so the
    /// lease can still see that game's exit.
    Adopt,
}

/// Who a launch belongs to. Both halves are required — see [`key_for`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    fingerprint: String,
    game_id: String,
}

/// Identity half of the match: (client, title). `None` = not recordable,
/// so the launch is started, not reclaimed.
///
/// Both sides required. No fingerprint (TOFU / `--open` / GameStream)
/// would let any anonymous client reclaim any other anonymous launch —
/// on Linux the second client gets an empty display. No library id
/// (`apps.json` command) would share one `None` and reclaim each other.
/// Same conservatism as [`crate::gamelease::readopt`].
pub fn key_for(fingerprint: Option<&str>, game_id: Option<&str>) -> Option<Key> {
    Some(Key {
        fingerprint: fingerprint?.to_string(),
        game_id: game_id?.to_string(),
    })
}

impl Key {
    /// Same 12-char prefix as `client_label`, so launch lines match session lines.
    fn short_client(&self) -> &str {
        self.fingerprint.get(..12).unwrap_or(&self.fingerprint)
    }
}

struct Record {
    key: Key,
    /// [`crate::gamelease::launch_clock`] taken immediately before spawn.
    /// `None` means this platform has no process-start clock, never "failed
    /// to inherit" — that path is [`Plan::Spawn`] with a fresh stamp.
    stamp: Option<f64>,
    procs: LiveProcs,
    /// Set only after spawn succeeded. An un-launched record is never matched.
    launched: bool,
    /// Live sessions holding this record. A count, not a flag: teardown and
    /// the next launch overlap, and the old release must not zero the new hold.
    holders: u32,
    /// When the last holder let go. `None` while held.
    released_at: Option<Instant>,
    /// Newest claim id on this record. An older session compares to learn
    /// its game now belongs to somebody else ([`Claim::superseded`]).
    claim: u64,
}

impl Record {
    fn new(key: Key, stamp: Option<f64>, claim: u64) -> Self {
        Self {
            key,
            stamp,
            // Fresh slot: the previous launch's dead procs must not be inherited,
            // and that lease may still be writing the old handle.
            procs: Arc::new(Mutex::new(Vec::new())),
            launched: false,
            holders: 1,
            released_at: None,
            claim,
        }
    }
}

/// Pure: caller supplies liveness, clock, and window. Identity is the
/// record's key ([`key_for`]), not checked here. Liveness wins when it
/// has an opinion; [`Liveness::Unknown`] falls through to a live holder
/// (teardown overlapping redial) or [`IN_FLIGHT_WINDOW`].
fn covers(rec: &Record, live: Liveness, now: Instant, window: Duration) -> bool {
    // Never launched: nothing to reclaim, and inheriting the stamp would
    // filter out every process (none started after it).
    if !rec.launched {
        return false;
    }
    match live {
        Liveness::Running => true,
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

/// How many of `procs` are still the same live processes. `None` on a
/// platform with no matcher (macOS) is "no opinion" — never "gone".
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

/// Drop unheld never-launched records and unheld ones idle past
/// [`MAX_RECORD_AGE`]. No process scan: runs under the registry lock on
/// the only path that touches it (a session deciding its launch).
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
    pub game_id: String,
    /// Last published adopted processes. Caller re-verifies immediately
    /// before signalling so a recycled pid is never hit (rule 2).
    pub procs: Vec<crate::procscan::ProcRef>,
}

/// Other titles this client has running from a host-performed launch.
///
/// Never: another client's game (keyed by cert fingerprint); a game the
/// player started themselves (only host launches are recorded — rule 1);
/// the title being launched now (`keep_game_id`, so relaunch is
/// [`Plan::Adopt`]); anything already gone. Only [`Liveness::Running`]
/// qualifies — `Unknown` does not authorize a signal (no verified pid).
pub fn others_still_running(
    fingerprint: Option<&str>,
    keep_game_id: Option<&str>,
) -> Vec<StillRunning> {
    let Some(fp) = fingerprint else {
        // Anonymous (TOFU / `--open` / GameStream) owns no records and
        // must not reach anyone else's.
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

/// Filter behind [`others_still_running`]. Pure so the four exclusions
/// can be tested without a registry. Caller supplies liveness as in [`covers`].
fn is_other_running(rec: &Record, fp: &str, keep_game_id: Option<&str>, live: Liveness) -> bool {
    rec.key.fingerprint == fp
        && Some(rec.key.game_id.as_str()) != keep_game_id
        && rec.launched
        // `Unknown` is "no opinion" — it does not authorize a signal.
        && live == Liveness::Running
}

/// Decide this session's launch plan and claim the record.
///
/// `fresh_stamp` is this session's [`crate::gamelease::launch_clock`],
/// taken before spawn; used for [`Plan::Spawn`], discarded for
/// [`Plan::Adopt`]. Hold the returned [`Claim`] for the session
/// ([`crate::gamelease::SessionGuard`]); drop starts the reconnect window.
pub fn claim(fingerprint: Option<&str>, game_id: Option<&str>, fresh_stamp: Option<f64>) -> Claim {
    let Some(key) = key_for(fingerprint, game_id) else {
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
    // Id allocated under the lock so ids and writes agree on order. An
    // id taken before the lock could land after a higher one, and then
    // neither session would see itself superseded.
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
        // Reset in place so `holders` survives: an older session may
        // still hold this record. Fresh stamp and proc slot so dead
        // processes are never inherited.
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
    pub fn must_spawn(&self) -> bool {
        matches!(self.plan, Plan::Spawn)
    }

    /// Stamp the lease must adopt ([`crate::gamelease::LeaseRequest::launch_stamp`]).
    /// Fresh for [`Plan::Spawn`], inherited for [`Plan::Adopt`]. `None`
    /// is "no process-start clock", never "inheritance failed".
    pub fn stamp(&self) -> Option<f64> {
        self.stamp
    }

    /// Slot the lease publishes into ([`crate::gamelease::LeaseRequest::procs`]).
    /// [`Plan::Adopt`] uses the original launch's slot so tracking
    /// survives the handover.
    pub fn procs(&self) -> Option<LiveProcs> {
        self.procs.clone()
    }

    /// Mark the launch as having happened. No-op if a newer session has
    /// already re-stamped the record — that session confirms its own.
    pub fn launched(&self) {
        self.with_record(|r| {
            if r.claim == self.id {
                r.launched = true;
            }
        });
    }

    /// Spawn failed or this platform has no launch path. Drop the record
    /// so a retry starts the title rather than inheriting a never-launch.
    pub fn abandon(&self) {
        let Some(key) = self.key.as_ref() else {
            return;
        };
        let mut recs = reg().records.lock().unwrap_or_else(|e| e.into_inner());
        recs.retain(|r| &r.key != key || r.claim != self.id);
    }

    /// Has a newer session claimed this launch? `false` if there is no
    /// record, so only a positive signal changes a caller's behavior.
    pub fn superseded(&self) -> bool {
        let Some(key) = self.key.as_ref() else {
            return false;
        };
        let recs = reg().records.lock().unwrap_or_else(|e| e.into_inner());
        recs.iter().any(|r| &r.key == key && r.claim > self.id)
    }

    /// Run `f` on this claim's record if it still exists. Not claim-checked:
    /// [`Drop`] must decrement the count it incremented even after a newer
    /// session re-stamped. Callers that need "still mine" check `claim`
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

    /// Fixture for the pure-rule table. Never touches the global registry.
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

    /// The four exclusions for `GameOnNewLaunch::End`, asserted one by
    /// one: a composite case would hide which game a miss would close.
    #[test]
    fn ending_on_a_new_launch_only_ever_reaches_this_clients_other_live_games() {
        let mut r = rec(true, 0, None);
        r.key.game_id = "steam:1".into();

        assert!(is_other_running(
            &r,
            "fp",
            Some("steam:2"),
            Liveness::Running
        ));

        assert!(!is_other_running(
            &r,
            "other-fp",
            Some("steam:2"),
            Liveness::Running
        ));

        // Relaunch of the same title is Adopt, not close-and-restart.
        assert!(!is_other_running(
            &r,
            "fp",
            Some("steam:1"),
            Liveness::Running
        ));

        let never = rec(false, 0, None);
        assert!(!is_other_running(
            &never,
            "fp",
            Some("steam:2"),
            Liveness::Running
        ));

        // Gone, and Unknown: no verified pid set to signal.
        assert!(!is_other_running(&r, "fp", Some("steam:2"), Liveness::Gone));
        assert!(!is_other_running(
            &r,
            "fp",
            Some("steam:2"),
            Liveness::Unknown
        ));
    }

    #[test]
    fn an_anonymous_client_can_never_end_another_clients_game() {
        assert!(others_still_running(None, Some("steam:2")).is_empty());
    }

    #[test]
    fn a_launch_is_keyed_by_both_the_client_and_the_title() {
        assert!(key_for(Some("fp"), Some("steam:570")).is_some());
        assert!(key_for(None, Some("steam:570")).is_none());
        assert!(key_for(Some("fp"), None).is_none());
        assert!(key_for(None, None).is_none());
        assert_ne!(
            key_for(Some("a"), Some("steam:570")),
            key_for(Some("b"), Some("steam:570"))
        );
        assert_ne!(
            key_for(Some("a"), Some("steam:570")),
            key_for(Some("a"), Some("gog:1"))
        );
    }

    #[test]
    fn the_match_rule_puts_liveness_ahead_of_the_window() {
        let t0 = Instant::now();
        let window = Duration::from_secs(90);
        let inside = t0 + Duration::from_secs(30);
        let outside = t0 + Duration::from_secs(600);

        // Never-launched is not reclaimed even if something looks alive.
        let never = rec(false, 1, None);
        assert!(!covers(&never, Liveness::Running, inside, window));

        let old = rec(true, 0, Some(t0));
        assert!(covers(&old, Liveness::Running, outside, window));

        // Gone beats a live holder and a fresh release (crash/quit relaunches now).
        let held = rec(true, 1, None);
        assert!(!covers(&held, Liveness::Gone, inside, window));
        assert!(!covers(&old, Liveness::Gone, inside, window));

        // Unknown: a live holder or a prompt return is the same launch.
        assert!(covers(&held, Liveness::Unknown, inside, window));
        assert!(covers(&old, Liveness::Unknown, inside, window));
        // Unknown past the window, never seen running: new launch.
        assert!(!covers(&old, Liveness::Unknown, outside, window));
    }

    /// Past [`IN_FLIGHT_WINDOW`], only a launch that adopted a live
    /// process stays adoptable. Empty adopted set is `Unknown` and
    /// falls through to spawn. Carrying `LeaseRequest::spawned` is what
    /// moves a no-detect-signals title out of that column.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn only_a_launch_that_adopted_something_stays_adoptable_past_the_window() {
        let t0 = Instant::now();
        let window = Duration::from_secs(90);
        let outside = t0 + Duration::from_secs(600);

        let blind = rec(true, 0, Some(t0));
        assert_eq!(liveness(&blind), Liveness::Unknown);
        assert!(!covers(&blind, liveness(&blind), outside, window));

        // This test binary is a live process, so the record stays Running.
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

    #[test]
    fn the_sweep_keeps_only_reclaimable_records() {
        let t0 = Instant::now();
        let mut recs = vec![
            rec(false, 0, Some(t0)), // never launched, unheld
            rec(true, 0, Some(t0)),  // launched, recently released
            rec(false, 1, None),     // never launched but held
        ];
        sweep(&mut recs, t0 + Duration::from_secs(1));
        assert_eq!(recs.len(), 2);
        let mut recs = vec![rec(true, 0, Some(t0))];
        sweep(&mut recs, t0 + MAX_RECORD_AGE + Duration::from_secs(1));
        assert!(recs.is_empty());
    }

    #[test]
    fn a_reconnect_does_not_launch_the_title_a_second_time() {
        let (fp, app) = (Some("fp-double"), Some("gog:double"));
        let first = claim(fp, app, Some(100.0));
        assert!(first.must_spawn(), "the first session starts the title");
        first.launched();
        drop(first); // opens the reconnect window

        let second = claim(fp, app, Some(900.0));
        assert!(
            !second.must_spawn(),
            "a reconnect inside the window must adopt the running launch, not start a second copy"
        );
        second.abandon(); // process-global registry; leave it as we found it
    }

    /// Reconnect must inherit the original stamp; a fresh one is above
    /// the running game and [`crate::procscan`] will not adopt it.
    #[test]
    fn a_reconnect_inherits_the_original_launchs_reference_instant() {
        let (fp, app) = (Some("fp-stamp"), Some("steam:stamp"));
        let first = claim(fp, app, Some(100.0));
        assert_eq!(first.stamp(), Some(100.0));
        first.launched();
        drop(first);

        let second = claim(fp, app, Some(900.0));
        assert_eq!(
            second.stamp(),
            Some(100.0),
            "the reconnect must adopt against the original launch, not against its own start"
        );
        assert_ne!(second.stamp(), Some(900.0));
        // Same proc slot so tracking survives the handover.
        assert!(second.procs().is_some());
        second.abandon();
    }

    /// A recorded stamp must never become `None` (that disables the
    /// start-time filter and would adopt anything).
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
        // Unrecordable still carries this session's own stamp.
        let anon = claim(None, app, Some(7.0));
        assert!(anon.must_spawn());
        assert_eq!(anon.stamp(), Some(7.0));
        assert!(anon.procs().is_none());
    }

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

    #[test]
    fn an_overlapping_session_adopts_a_still_held_launch() {
        let (fp, app) = (Some("fp-overlap"), Some("steam:overlap"));
        let old = claim(fp, app, Some(100.0));
        old.launched();
        // The old session has not torn down yet.
        let new = claim(fp, app, Some(900.0));
        assert!(!new.must_spawn(), "a held launch is still ours");
        assert_eq!(new.stamp(), Some(100.0));
        // Old session sees superseded, so teardown policy leaves the game.
        assert!(old.superseded());
        assert!(!new.superseded());
        drop(old);
        assert!(
            !new.superseded(),
            "the older session releasing must not look like a newer claim"
        );
        new.abandon();
    }

    #[test]
    fn an_unrecordable_launch_never_supersedes_anything() {
        let anon = claim(None, None, None);
        assert!(anon.must_spawn());
        assert!(!anon.superseded());
        anon.launched(); // no-op
        anon.abandon(); // no-op
    }
}
