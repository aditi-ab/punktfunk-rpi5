//! Live game-liveness reports supplied by provider plugins when process scanning cannot infer
//! the answer reliably.
//!
//! A provider PUTs its complete running set, including a pid when known. Whole-set replacement
//! makes missed events and restarts converge on the next update. Reports expire after
//! [`REPORT_TTL`] so a dead plugin cannot keep a session alive indefinitely; expired entries fall
//! back to [`crate::procscan`]. This complements the static recognition hints in
//! [`crate::library::DetectHint`].

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// How long a provider's report stays authoritative without being restated.
///
/// Generous enough that a plugin refreshing every 30s survives a slow reconcile or a paused runner,
/// short enough that a *dead* plugin stops vetoing a session end within a couple of minutes. The
/// cost of expiring too early is the pre-existing behaviour (scan-only); the cost of never expiring
/// is a session that can never end on its own, which is the bug this whole area exists to kill.
pub const REPORT_TTL: Duration = Duration::from_secs(90);

/// What a provider says about one of its titles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    /// Whether the provider lists this title as running right now.
    pub running: bool,
    /// The pid the provider started for it, when it knows one. Never trusted as a bare number —
    /// every use re-verifies it through [`crate::procscan`], which pins it to its start time.
    pub pid: Option<u32>,
}

/// One provider's most recent report.
struct Report {
    /// When it landed — the TTL clock.
    at: Instant,
    /// Every library id this provider speaks for. What makes "not in `running`" mean *not running*
    /// rather than *no opinion*: without it an omitted title is indistinguishable from a title
    /// belonging to some other provider entirely.
    owned: HashSet<String>,
    /// The subset that is running, each with the pid the provider started (when it has one).
    running: HashMap<String, Option<u32>>,
}

impl Report {
    fn fresh(&self) -> bool {
        self.at.elapsed() < REPORT_TTL
    }
}

fn table() -> MutexGuard<'static, HashMap<String, Report>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Report>>> = OnceLock::new();
    TABLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Record a provider's report, replacing whatever it said before.
///
/// `owned` is every library id the provider currently publishes; `running` is the subset that is
/// running, keyed the same way, valued by pid where one is known.
pub fn report(provider: &str, owned: HashSet<String>, running: HashMap<String, Option<u32>>) {
    table().insert(
        provider.to_string(),
        Report {
            at: Instant::now(),
            owned,
            running,
        },
    );
}

/// Forget everything a provider said — its entries are gone, so its opinions are meaningless.
pub fn forget(provider: &str) {
    table().remove(provider);
}

/// What a *fresh* provider says about this library id, or `None` when none speaks for it.
///
/// `None` is the answer for every title on a host with no reporting plugin, which is what keeps
/// this entirely inert until someone opts in.
pub fn opinion(app_id: &str) -> Option<Liveness> {
    let table = table();
    table
        .values()
        .filter(|r| r.fresh())
        .find(|r| r.owned.contains(app_id))
        .map(|r| match r.running.get(app_id) {
            Some(pid) => Liveness {
                running: true,
                pid: *pid,
            },
            None => Liveness {
                running: false,
                pid: None,
            },
        })
}

/// Whether any fresh provider reports liveness for this title at all — regardless of what it
/// currently says.
///
/// Asked once, when a lease opens: a title whose provider will tell us when it stops is trackable
/// even with no detect signals whatsoever, which is the whole point (see
/// [`crate::gamelease::LeaseKind::Reported`]).
pub fn speaks_for(app_id: Option<&str>) -> bool {
    app_id.is_some_and(|id| opinion(id).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    fn running(ids: &[(&str, Option<u32>)]) -> HashMap<String, Option<u32>> {
        ids.iter().map(|(s, p)| ((*s).to_string(), *p)).collect()
    }

    // The table is process-global and these tests run in parallel, so each takes a provider id and
    // app ids only it uses, and cleans up only its own row. An earlier draft shared the id
    // `playnite` and cleared the whole table between cases, which made the three of them flip each
    // other's answers depending on scheduling — the same shape as `mgmt`'s `local_summary` race.

    /// The three answers, and the distinction the whole module turns on: a title its provider omits
    /// is *not running*, while a title nobody speaks for has *no opinion*. Conflating them would
    /// make every unreported game on the box look like it had just quit.
    #[test]
    fn omitted_is_not_running_but_unknown_is_no_opinion() {
        report(
            "answers-test",
            owned(&["answers:a", "answers:b"]),
            running(&[("answers:a", Some(4242))]),
        );
        assert_eq!(
            opinion("answers:a"),
            Some(Liveness {
                running: true,
                pid: Some(4242)
            })
        );
        assert_eq!(
            opinion("answers:b"),
            Some(Liveness {
                running: false,
                pid: None
            })
        );
        assert_eq!(opinion("answers:never-published"), None);
        assert!(speaks_for(Some("answers:b")));
        assert!(!speaks_for(Some("answers:never-published")));
        assert!(!speaks_for(None));
        forget("answers-test");
    }

    /// A report replaces its predecessor wholesale. The set is the message: a title that dropped out
    /// of it has stopped, and carrying the old entry forward would be exactly the stuck-running
    /// state this exists to prevent.
    #[test]
    fn a_report_replaces_the_previous_one() {
        report(
            "replace-test",
            owned(&["replace:a"]),
            running(&[("replace:a", None)]),
        );
        report("replace-test", owned(&["replace:a"]), running(&[]));
        assert_eq!(
            opinion("replace:a"),
            Some(Liveness {
                running: false,
                pid: None
            })
        );
        forget("replace-test");
        assert_eq!(opinion("replace:a"), None);
    }

    /// A stale report stops counting — the bound that makes it safe to let a plugin's claim hold a
    /// session open. Seeded with an aged timestamp rather than by sleeping for 90 seconds.
    #[test]
    fn a_stale_report_has_no_opinion() {
        table().insert(
            "stale-test".to_string(),
            Report {
                at: Instant::now() - REPORT_TTL - Duration::from_secs(1),
                owned: owned(&["stale:a"]),
                running: running(&[("stale:a", Some(7))]),
            },
        );
        assert_eq!(opinion("stale:a"), None);
        assert!(!speaks_for(Some("stale:a")));
        forget("stale-test");
    }
}
