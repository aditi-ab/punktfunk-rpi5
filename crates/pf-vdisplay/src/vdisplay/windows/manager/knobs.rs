//! Runtime display-management knobs: linger window, keep-alive-forever pin,
//! and per-monitor topology action. Readers of [`crate::policy`] plus legacy
//! env fallbacks — no manager state.

/// 10 s: historical default, and the fallback when a rung cannot answer.
const DEFAULT_LINGER_MS: u64 = 10_000;

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

/// Console policy outranks the env knob: an operator who set the console
/// must not have it silently overridden by a leftover
/// `PUNKTFUNK_MONITOR_LINGER_MS`.
fn resolve_linger_ms(configured: Option<crate::policy::Linger>, env_ms: Option<u64>) -> u64 {
    use crate::policy::Linger;
    match configured {
        Some(Linger::Immediate) => 0,
        Some(Linger::For(d)) => d.as_millis() as u64,
        // `forever` is handled by `keep_alive_forever()` in `release` (→ `Pinned`).
        // Reached only if a caller skipped the pin check — fall back to the
        // default, not a huge linger.
        Some(Linger::Forever) => DEFAULT_LINGER_MS,
        // Unconfigured: env knob, else the default. Unparseable arrives as `None`
        // (`parse().ok()`), i.e. unset, not zero.
        None => env_ms.unwrap_or(DEFAULT_LINGER_MS),
    }
}

/// Whether configured `keep_alive` is forever (`Pinned`). `release` keeps
/// the last-released monitor indefinitely. Unconfigured hosts are never forever.
pub(super) fn keep_alive_forever() -> bool {
    use crate::policy::{prefs, Linger};
    prefs()
        .configured_effective()
        .map(|eff| matches!(eff.keep_alive.linger(), Linger::Forever))
        .unwrap_or(false)
}

/// Exclusive-topology re-assert cadence. Default 2000 ms; `0` disables.
/// A verified isolate is not durable — see
/// `VirtualDisplayManager::ensure_exclusive_watch`.
pub(super) fn exclusive_reassert_ms() -> u64 {
    std::env::var("PUNKTFUNK_EXCLUSIVE_REASSERT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000)
}

/// Topology for a freshly-created monitor (never `Auto`): console
/// [`effective_topology`](crate::effective_topology) when configured, else
/// `PUNKTFUNK_NO_ISOLATE` → `Extend`, otherwise `Exclusive`.
pub(super) fn topology_action() -> crate::policy::Topology {
    let configured = crate::policy::prefs()
        .configured_effective()
        .map(|_| crate::effective_topology());
    resolve_topology_action(configured, std::env::var("PUNKTFUNK_NO_ISOLATE").is_ok())
}

/// Unconfigured host: `PUNKTFUNK_NO_ISOLATE` → `Extend`, else `Exclusive`.
/// A configured answer is passed through; the env knob does not override it.
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

    #[test]
    fn configured_policy_beats_the_legacy_env_knob() {
        assert_eq!(
            resolve_linger_ms(Some(Linger::For(Duration::from_secs(3))), Some(60_000)),
            3_000
        );
        assert_eq!(resolve_linger_ms(Some(Linger::Immediate), Some(60_000)), 0);
    }

    /// Unparseable env reaches here as `None` (`parse().ok()`), so it reads as
    /// unset, not zero. `linger_ms = 0` would tear the monitor down on every
    /// disconnect.
    #[test]
    fn an_unconfigured_host_honours_the_env_knob_then_the_default() {
        assert_eq!(resolve_linger_ms(None, Some(250)), 250);
        assert_eq!(resolve_linger_ms(None, None), DEFAULT_LINGER_MS);
    }

    /// `Forever` is the `Pinned` lifecycle, resolved by `keep_alive_forever()`
    /// before any ms are asked. Reaching here skipped the pin check; the answer
    /// is the default window, not an infinite linger that keeps physical panels
    /// dark with nothing to release them.
    #[test]
    fn forever_resolves_to_the_default_not_a_huge_linger() {
        assert_eq!(
            resolve_linger_ms(Some(Linger::Forever), None),
            DEFAULT_LINGER_MS
        );
    }

    /// Unconfigured rungs are `Exclusive` by default, `Extend` under the legacy
    /// opt-out — never `Auto`, which the manager's `match` would treat as extend
    /// without saying so.
    #[test]
    fn the_unconfigured_topology_rungs_never_yield_auto() {
        assert_eq!(resolve_topology_action(None, false), Topology::Exclusive);
        assert_eq!(resolve_topology_action(None, true), Topology::Extend);
        assert_eq!(
            resolve_topology_action(Some(Topology::Primary), true),
            Topology::Primary
        );
    }
}
