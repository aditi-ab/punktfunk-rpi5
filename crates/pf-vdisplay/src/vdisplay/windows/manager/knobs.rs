//! Runtime display-management knobs read from the console policy (with legacy env-var fallbacks),
//! carved out of the manager (plan §W3): the linger window, the keep-alive-forever pin, and the
//! per-monitor topology action. Pure readers of [`crate::policy`] + env — no manager state.

/// The historical Windows linger window, and the fallback for every rung that cannot answer.
const DEFAULT_LINGER_MS: u64 = 10_000;

/// Linger window before a session-less monitor is torn down. The console display-management policy
/// wins when configured (`keep_alive`); otherwise the legacy `PUNKTFUNK_MONITOR_LINGER_MS` env knob,
/// else the 10 s default.
pub(super) fn linger_ms() -> u64 {
    resolve_linger_ms(
        crate::policy::prefs()
            .configured_effective()
            .map(|eff| eff.keep_alive.linger()),
        std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
            .ok()
            .and_then(|s| s.parse().ok()),
    )
}

/// The precedence itself, lifted out of the readers so it is pinnable without a settings file, an
/// environment or a manager (this module's decisions are the ONLY ones on the Windows lifecycle path
/// that need neither a driver nor a desktop, and they had no tests at all).
///
/// `configured` is the console policy's resolved [`Linger`](crate::policy::Linger) (`None` = the
/// host was never configured), `env_ms` the parsed legacy knob. The configured policy outranks the
/// env knob entirely — an operator who set the console must not have it silently overridden by a
/// leftover variable.
fn resolve_linger_ms(configured: Option<crate::policy::Linger>, env_ms: Option<u64>) -> u64 {
    use crate::policy::Linger;
    match configured {
        Some(Linger::Immediate) => 0,
        Some(Linger::For(d)) => d.as_millis() as u64,
        // `forever` is handled BEFORE this by `keep_alive_forever()` in `release` (→ `Pinned`), so
        // this arm is only reached defensively (e.g. a caller that resolves ms without the pin
        // check) — fall back to the default rather than a huge linger.
        Some(Linger::Forever) => DEFAULT_LINGER_MS,
        // Unconfigured: the legacy env knob, else the historical default. An unparseable value
        // arrives here as `None` (the caller's `parse().ok()`), i.e. it reads as unset.
        None => env_ms.unwrap_or(DEFAULT_LINGER_MS),
    }
}

/// Whether the configured console policy's `keep_alive` resolves to **forever** (`Pinned`) — the
/// gaming-rig preset. `release` uses this to keep the last-released monitor indefinitely instead of
/// lingering. Unconfigured hosts are never forever (default is a short linger).
pub(super) fn keep_alive_forever() -> bool {
    use crate::policy::{prefs, Linger};
    prefs()
        .configured_effective()
        .map(|eff| matches!(eff.keep_alive.linger(), Linger::Forever))
        .unwrap_or(false)
}

/// Cadence of the exclusive-topology re-assert watchdog (`PUNKTFUNK_EXCLUSIVE_REASSERT_MS`,
/// default 2000, `0` disables — the pre-watchdog behavior). Why it exists: a verified isolate is
/// not durable — see `VirtualDisplayManager::ensure_exclusive_watch` in the parent module.
pub(super) fn exclusive_reassert_ms() -> u64 {
    std::env::var("PUNKTFUNK_EXCLUSIVE_REASSERT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000)
}

/// The effective display topology for a freshly-created monitor (never `Auto`): the console policy's
/// [`effective_topology`](crate::effective_topology) when configured, else the legacy
/// `PUNKTFUNK_NO_ISOLATE` env knob (`Extend`) / `Exclusive` (today's default). `Extend` leaves the IDD
/// extended; `Primary` makes it primary while keeping the physical(s) active; `Exclusive` disables the
/// physical(s) so the IDD is the sole composited desktop.
pub(super) fn topology_action() -> crate::policy::Topology {
    let configured = crate::policy::prefs()
        .configured_effective()
        .map(|_| crate::effective_topology());
    resolve_topology_action(configured, std::env::var("PUNKTFUNK_NO_ISOLATE").is_ok())
}

/// The precedence for [`topology_action`], lifted out for the same reason as [`resolve_linger_ms`].
/// `configured` is [`crate::effective_topology`]'s answer when the console configured anything at
/// all (that fn is the rung responsible for never returning `Auto`); `no_isolate_env` is the legacy
/// `PUNKTFUNK_NO_ISOLATE` opt-out, which an unconfigured host still honors.
fn resolve_topology_action(
    configured: Option<crate::policy::Topology>,
    no_isolate_env: bool,
) -> crate::policy::Topology {
    use crate::policy::Topology;
    match configured {
        Some(t) => t,
        None if no_isolate_env => Topology::Extend,
        None => Topology::Exclusive,
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_linger_ms, resolve_topology_action, DEFAULT_LINGER_MS};
    use crate::policy::{Linger, Topology};
    use std::time::Duration;

    /// The console policy is the top rung: a host that configured `keep_alive` must not have it
    /// silently overridden by a leftover `PUNKTFUNK_MONITOR_LINGER_MS`.
    #[test]
    fn configured_policy_beats_the_legacy_env_knob() {
        assert_eq!(
            resolve_linger_ms(Some(Linger::For(Duration::from_secs(3))), Some(60_000)),
            3_000
        );
        assert_eq!(resolve_linger_ms(Some(Linger::Immediate), Some(60_000)), 0);
    }

    /// Unconfigured hosts keep the historical behavior: the env knob, else the 10 s default. An
    /// unparseable value reaches this fn as `None` (the reader's `parse().ok()`), so it reads as
    /// unset rather than as zero — a `linger_ms = 0` would tear the monitor down on every
    /// disconnect.
    #[test]
    fn an_unconfigured_host_honours_the_env_knob_then_the_default() {
        assert_eq!(resolve_linger_ms(None, Some(250)), 250);
        assert_eq!(resolve_linger_ms(None, None), DEFAULT_LINGER_MS);
    }

    /// `Forever` is the `Pinned` lifecycle, resolved by `keep_alive_forever()` before any ms are
    /// asked for; reaching this fn with it means a caller skipped the pin check, and the answer is
    /// the default window — NOT an effectively infinite linger that would keep the physical panels
    /// dark with nothing to release them.
    #[test]
    fn forever_resolves_to_the_default_not_a_huge_linger() {
        assert_eq!(
            resolve_linger_ms(Some(Linger::Forever), None),
            DEFAULT_LINGER_MS
        );
    }

    /// The unconfigured rungs are `Exclusive` by default, `Extend` under the legacy opt-out — and
    /// neither is `Auto`, which the manager's `match` would treat as plain extend without ever
    /// saying so.
    #[test]
    fn the_unconfigured_topology_rungs_never_yield_auto() {
        assert_eq!(resolve_topology_action(None, false), Topology::Exclusive);
        assert_eq!(resolve_topology_action(None, true), Topology::Extend);
        // A configured host's answer is whatever `effective_topology()` resolved — passed through
        // verbatim, env knob or not.
        assert_eq!(
            resolve_topology_action(Some(Topology::Primary), true),
            Topology::Primary
        );
    }
}
