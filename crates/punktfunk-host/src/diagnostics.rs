//! Host diagnostics: one structured channel for the health verdicts this host already computes.
//!
//! The class of bug this exists for is **the host knows, and the person operating it has no way to
//! find out**. The `.181` incident is the type specimen: `preflight_takeover_privilege()` is a
//! careful, applicability-gated probe that distinguishes user-database membership from the running
//! process's supplementary groups — and it spends all of that care on one WARN log line, which a
//! console-driven update never shows anyone. Every failure class before this had either its own
//! bespoke surface or none at all.
//!
//! So verdicts become data. The registry lives here; the **probes stay in their owning crates**
//! (`pf-inject`, `pf-vdisplay`, `crate::detect`) and export plain verdict enums — nothing in those
//! crates learns about [`HostCheck`], and no reverse dependency is created. This module maps
//! verdict → check and owns every wire string.
//!
//! Two rules the catalog must keep:
//!
//! * **English fallback text is mandatory, not a courtesy.** The web console is a separate package
//!   and canary setups pair console N with host N±1, so the console localizes by `id` when it knows
//!   the id and renders the wire text when it does not. An id that ships without `summary`/`impact`
//!   is unreadable on any console that predates it — [`tests::every_non_ok_check_carries_fallback_text`]
//!   enforces this rather than trusting a convention.
//! * **`inapplicable` is a first-class status, not an absent row.** A box that will never attempt a
//!   takeover must not be nagged, but the troubleshooting page still has to be able to answer "why
//!   isn't this check relevant here?" on demand.
//!
//! Served by `mgmt/diagnostics.rs` on the authenticated admin lane only: usernames, group layout and
//! device-node state must not widen the unauthenticated loopback surface the tray reads.

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

pub(crate) mod catalog;

/// Stable check ids. These are the console's i18n keys, so they are API: renaming one silently
/// drops a translation back to the wire fallback on every console.
pub mod ids {
    pub const TAKEOVER_PRIVILEGE: &str = "takeover_privilege";
    pub const VIRTUAL_DECK_VHCI: &str = "virtual_deck_vhci";
    pub const UINPUT_ACCESS: &str = "uinput_access";
    pub const SERVER_CONFLICT: &str = "server_conflict";
    pub const HYPRLAND_PERMISSIONS: &str = "hyprland_permissions";
    pub const OMARCHY_UPDATES: &str = "omarchy_updates";
}

/// What a probe found. `Inapplicable` is deliberately distinct from `Ok`: "this box will never do
/// the thing" and "the thing works here" are different answers, and the troubleshooting page shows
/// them differently.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Inapplicable,
}

impl CheckStatus {
    /// Does this status want the operator's attention? `inapplicable` does not — that is the whole
    /// point of the status existing.
    pub fn needs_attention(self) -> bool {
        matches!(self, CheckStatus::Warn | CheckStatus::Fail)
    }
}

/// How much a non-ok status matters. Orthogonal to [`CheckStatus`] on purpose: a check can be
/// `warn` about something `critical` (degraded, not dead) and the console sorts by both.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        }
    }
}

/// What the operator should do about it. Always copy-paste — the host runs unprivileged and the
/// console must never trigger privileged mutation. The `punktfunk` group in particular is
/// deliberately opt-in: writing the vhci `attach` node materialises arbitrary emulated USB devices
/// (security review 2026-08-05, M-4), so joining it stays a deliberate act with the caveat attached.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct Remedy {
    /// Plain-language instruction. English fallback — the console overrides it by check id.
    pub text: String,
    /// A single pasteable shell command, when one fixes it outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// True when the fix only takes effect after logging out and back in — a `systemd --user`
    /// manager keeps the supplementary group set it started with. This distinction is the
    /// difference between "I already added myself!" and a working virtual pad.
    pub relogin_required: bool,
}

/// Where a verdict came from. `Event` is reserved for the live feeds (transitions push instead of
/// waiting for a refresh); v1 produces only `Startup` and `Refresh`.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckSource {
    Startup,
    Event,
    Refresh,
}

/// One health verdict. This IS the wire shape.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct HostCheck {
    /// Stable snake_case machine code — the console's i18n key (see [`ids`]).
    pub id: String,
    pub status: CheckStatus,
    /// What a non-ok status means. Meaningless when `status` is `ok`/`inapplicable`; carried anyway
    /// so a check never changes shape as it flips.
    pub severity: Severity,
    /// One line, English. The console replaces this with a localized message when it knows `id`.
    pub summary: String,
    /// What actually breaks, in the operator's terms. Empty only for `ok`/`inapplicable` rows.
    pub impact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<Remedy>,
    /// Interpolation values for the console's localized strings (`{user}`, `{group}`, …). The
    /// console needs these because it cannot re-derive them: only the host can see the username.
    pub params: BTreeMap<String, String>,
    /// First non-ok observation in this host run. Per-run bookkeeping, not a time series — there is
    /// no history here by design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix: Option<u64>,
    pub source: CheckSource,
}

impl HostCheck {
    /// An applicable, healthy result.
    pub fn ok(id: &str, summary: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: CheckStatus::Ok,
            severity: Severity::Info,
            summary: summary.into(),
            impact: String::new(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    /// "This check does not apply to this box" — `why` says which gate excluded it, so the
    /// troubleshooting page can answer the question instead of hiding the row.
    pub fn inapplicable(id: &str, why: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: CheckStatus::Inapplicable,
            severity: Severity::Info,
            summary: why.into(),
            impact: String::new(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    /// A problem. `impact` is required here — a warning without a consequence is noise the operator
    /// cannot act on.
    pub fn problem(
        id: &str,
        status: CheckStatus,
        severity: Severity,
        summary: impl Into<String>,
        impact: impl Into<String>,
    ) -> Self {
        Self {
            id: id.to_string(),
            status,
            severity,
            summary: summary.into(),
            impact: impact.into(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    pub fn with_remedy(mut self, remedy: Remedy) -> Self {
        self.remedy = Some(remedy);
        self
    }

    pub fn with_param(mut self, key: &str, value: impl Into<String>) -> Self {
        self.params.insert(key.to_string(), value.into());
        self
    }

    /// Worst-first ordering key: attention-needing rows before healthy ones, then by severity, then
    /// `fail` ahead of `warn`, then by id so the list is stable across refreshes.
    fn order_key(&self) -> (u8, u8, u8, &str) {
        let bucket = match self.status {
            CheckStatus::Fail | CheckStatus::Warn => 0,
            CheckStatus::Ok => 1,
            CheckStatus::Inapplicable => 2,
        };
        let status_rank = match self.status {
            CheckStatus::Fail => 0,
            CheckStatus::Warn => 1,
            _ => 2,
        };
        (bucket, self.severity.rank(), status_rank, &self.id)
    }
}

/// The `GET /diagnostics` body.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct DiagnosticsReport {
    /// When the probes last ran (unix seconds).
    pub ran_at_unix: u64,
    /// Every registered check, worst-first. Includes `ok` and `inapplicable` rows — the console
    /// decides what to hide, because "what's working" is the reassurance the dashboard omits.
    pub checks: Vec<HostCheck>,
}

type Probe = Box<dyn Fn() -> HostCheck + Send + Sync>;

/// The registry: probes in, current verdicts out.
#[derive(Default)]
pub struct Diagnostics {
    probes: RwLock<Vec<Probe>>,
    checks: RwLock<BTreeMap<String, HostCheck>>,
    ran_at_unix: RwLock<u64>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a probe. Called once per check at startup; probes are cheap by contract (an `open()`, a
    /// `getgrnam`, a stat) because `POST /diagnostics/refresh` re-runs all of them synchronously.
    pub fn register(&self, probe: impl Fn() -> HostCheck + Send + Sync + 'static) {
        self.probes.write().unwrap().push(Box::new(probe));
    }

    /// Run every probe and replace the cached verdicts. `since_unix` is preserved across runs for a
    /// check that was already non-ok, so "since" means what it says.
    pub fn run_all(&self, source: CheckSource) {
        // Probes are run WITHOUT the checks lock held: they touch the filesystem and spawn `id`, and
        // a slow NSS lookup must not block a concurrent GET.
        let fresh: Vec<HostCheck> = {
            let probes = self.probes.read().unwrap();
            probes.iter().map(|p| p()).collect()
        };
        let now = now_unix();
        let mut checks = self.checks.write().unwrap();
        for mut check in fresh {
            let previous_since = prior_since(checks.get(&check.id));
            check.source = source;
            carry_since(&mut check, previous_since, now);
            checks.insert(check.id.clone(), check);
        }
        *self.ran_at_unix.write().unwrap() = now;
    }

    /// Feed one verdict from an event source (a `PadGate` transition, a driver watcher). Returns
    /// whether the *status* actually changed — the caller emits an SSE event only on a transition,
    /// never once per backoff retry.
    pub fn set(&self, mut check: HostCheck) -> bool {
        let now = now_unix();
        let mut checks = self.checks.write().unwrap();
        let previous = checks.get(&check.id);
        let changed = previous.is_none_or(|p| p.status != check.status);
        let previous_since = prior_since(previous);
        check.source = CheckSource::Event;
        carry_since(&mut check, previous_since, now);
        checks.insert(check.id.clone(), check);
        changed
    }

    /// Current verdicts, worst-first.
    pub fn report(&self) -> DiagnosticsReport {
        let checks = self.checks.read().unwrap();
        let mut checks: Vec<HostCheck> = checks.values().cloned().collect();
        checks.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        DiagnosticsReport {
            ran_at_unix: *self.ran_at_unix.read().unwrap(),
            checks,
        }
    }

    /// How many attention-needing checks there are, split by severity — the only diagnostics shape
    /// the unauthenticated loopback summary may ever carry (counts, never details).
    #[allow(dead_code)] // consumed by the tray's LocalSummary once the live feeds land
    pub fn attention_counts(&self) -> (u32, u32) {
        let checks = self.checks.read().unwrap();
        let mut warning = 0;
        let mut critical = 0;
        for c in checks.values().filter(|c| c.status.needs_attention()) {
            match c.severity {
                Severity::Critical => critical += 1,
                _ => warning += 1,
            }
        }
        (warning, critical)
    }
}

/// The stamp a still-unhealthy check should inherit — `None` once it has recovered, so a later
/// relapse is dated from the relapse rather than from the original.
fn prior_since(previous: Option<&HostCheck>) -> Option<u64> {
    previous
        .filter(|p| p.status.needs_attention())
        .and_then(|p| p.since_unix)
}

/// Keep the original first-observed stamp while a check stays non-ok; clear it when it recovers.
fn carry_since(check: &mut HostCheck, previous_since: Option<u64>, now: u64) {
    check.since_unix = check
        .status
        .needs_attention()
        .then(|| previous_since.unwrap_or(now));
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The process-wide registry. A global rather than an `AppState` field because the probes are
/// process-scoped (there is one set of device nodes and one group membership per host), and because
/// it keeps the mgmt handlers free of a state extractor — the same shape `crate::hooks::store()`,
/// `crate::detect::snapshot()` and the log ring already use.
pub fn registry() -> &'static Diagnostics {
    static REGISTRY: OnceLock<Diagnostics> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg
    })
}

/// Take the first reading. Called once from `native::serve` startup, after the subsystems the
/// probes inspect are up. The catalog itself is registered lazily by [`registry`], so a `GET` that
/// somehow arrives first still describes a known set of checks rather than an empty list.
pub fn preflight() {
    let reg = registry();
    reg.run_all(CheckSource::Startup);
    let report = reg.report();
    let attention = report
        .checks
        .iter()
        .filter(|c| c.status.needs_attention())
        .count();
    tracing::debug!(
        checks = report.checks.len(),
        attention,
        "diagnostics: startup probes complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fail(id: &str) -> HostCheck {
        HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            "broken",
            "nothing works",
        )
        .with_remedy(Remedy {
            text: "fix it".into(),
            command: None,
            relogin_required: false,
        })
    }

    #[test]
    fn refresh_reruns_every_probe() {
        let reg = Diagnostics::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        reg.register(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            HostCheck::ok("probe", "fine")
        });

        reg.run_all(CheckSource::Startup);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(reg.report().checks[0].source, CheckSource::Startup);

        reg.run_all(CheckSource::Refresh);
        assert_eq!(runs.load(Ordering::SeqCst), 2, "refresh must re-run probes");
        assert_eq!(reg.report().checks[0].source, CheckSource::Refresh);
    }

    #[test]
    fn report_is_worst_first_with_healthy_and_inapplicable_last() {
        let reg = Diagnostics::new();
        reg.register(|| HostCheck::ok("b_ok", "fine"));
        reg.register(|| HostCheck::inapplicable("a_na", "no display manager"));
        reg.register(|| {
            HostCheck::problem(
                "c_warn",
                CheckStatus::Warn,
                Severity::Warning,
                "degraded",
                "half works",
            )
        });
        reg.register(|| fail("d_fail"));
        reg.run_all(CheckSource::Startup);

        let ids: Vec<String> = reg.report().checks.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["d_fail", "c_warn", "b_ok", "a_na"]);
    }

    #[test]
    fn since_is_stamped_once_and_cleared_on_recovery() {
        let reg = Diagnostics::new();
        reg.set(fail("flapper"));
        let first = reg.report().checks[0].since_unix;
        assert!(first.is_some(), "a non-ok check records when it started");

        // Still failing: the original stamp survives, so "since" does not reset every probe run.
        reg.set(fail("flapper"));
        assert_eq!(reg.report().checks[0].since_unix, first);

        // Recovered: the stamp goes away rather than lingering as a lie.
        reg.set(HostCheck::ok("flapper", "fine"));
        assert_eq!(reg.report().checks[0].since_unix, None);
    }

    #[test]
    fn set_reports_only_real_transitions() {
        let reg = Diagnostics::new();
        assert!(reg.set(fail("pads")), "first observation is a transition");
        assert!(
            !reg.set(fail("pads")),
            "a repeated identical verdict is not a transition — one SSE event per flip, not per retry"
        );
        assert!(
            reg.set(HostCheck::ok("pads", "fine")),
            "fail → ok is a transition"
        );
    }

    #[test]
    fn attention_counts_ignore_healthy_and_inapplicable_rows() {
        let reg = Diagnostics::new();
        reg.register(|| fail("bad"));
        reg.register(|| {
            HostCheck::problem(
                "meh",
                CheckStatus::Warn,
                Severity::Warning,
                "degraded",
                "half works",
            )
        });
        reg.register(|| HostCheck::ok("good", "fine"));
        reg.register(|| HostCheck::inapplicable("na", "not here"));
        reg.run_all(CheckSource::Startup);

        assert_eq!(reg.attention_counts(), (1, 1));
    }

    /// The N/N−1 console-drift guarantee, as a test rather than a convention: the console renders
    /// the wire text whenever it does not recognize an id, so an id that ships without text is
    /// unreadable on every console that predates it.
    #[test]
    fn every_non_ok_check_carries_fallback_text() {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg.run_all(CheckSource::Startup);

        for check in reg.report().checks {
            assert!(
                !check.summary.trim().is_empty(),
                "{}: every check needs a summary — it is the only text an older console has",
                check.id
            );
            if check.status.needs_attention() {
                assert!(
                    !check.impact.trim().is_empty(),
                    "{}: a non-ok check must say what breaks",
                    check.id
                );
            }
            if check.status == CheckStatus::Fail {
                let remedy = check
                    .remedy
                    .as_ref()
                    .unwrap_or_else(|| panic!("{}: a failing check must carry a remedy", check.id));
                assert!(
                    !remedy.text.trim().is_empty(),
                    "{}: remedy text must not be empty",
                    check.id
                );
            }
        }
    }

    /// Ids are the console's i18n keys, so they are API. Catch a rename in review, not in a
    /// bug report about a check that suddenly renders in English.
    #[test]
    fn catalog_registers_the_documented_ids() {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg.run_all(CheckSource::Startup);

        let ids: Vec<String> = reg.report().checks.into_iter().map(|c| c.id).collect();
        for expected in [
            ids::TAKEOVER_PRIVILEGE,
            ids::VIRTUAL_DECK_VHCI,
            ids::UINPUT_ACCESS,
            ids::SERVER_CONFLICT,
            ids::HYPRLAND_PERMISSIONS,
            ids::OMARCHY_UPDATES,
        ] {
            assert!(
                ids.iter().any(|i| i == expected),
                "missing check {expected}"
            );
        }
    }
}
