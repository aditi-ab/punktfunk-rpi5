//! Runtime display-management knobs read from the console policy (with legacy env-var fallbacks),
//! carved out of the manager (plan §W3): the linger window, the keep-alive-forever pin, and the
//! per-monitor topology action. Pure readers of [`crate::policy`] + env — no manager state.

/// Linger window before a session-less monitor is torn down. The console display-management policy
/// wins when configured (`keep_alive`); otherwise the legacy `PUNKTFUNK_MONITOR_LINGER_MS` env knob,
/// else the 10 s default.
pub(super) fn linger_ms() -> u64 {
    use crate::policy::{prefs, Linger};
    if let Some(eff) = prefs().configured_effective() {
        return match eff.keep_alive.linger() {
            Linger::Immediate => 0,
            Linger::For(d) => d.as_millis() as u64,
            // `forever` is handled BEFORE this by `keep_alive_forever()` in `release` (→ `Pinned`), so
            // this arm is only reached defensively (e.g. a caller that resolves ms without the pin
            // check) — fall back to the default rather than a huge linger.
            Linger::Forever => 10_000,
        };
    }
    std::env::var("PUNKTFUNK_MONITOR_LINGER_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000)
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

/// The effective display topology for a freshly-created monitor (never `Auto`): the console policy's
/// [`effective_topology`](crate::effective_topology) when configured, else the legacy
/// `PUNKTFUNK_NO_ISOLATE` env knob (`Extend`) / `Exclusive` (today's default). `Extend` leaves the IDD
/// extended; `Primary` makes it primary while keeping the physical(s) active; `Exclusive` disables the
/// physical(s) so the IDD is the sole composited desktop.
pub(super) fn topology_action() -> crate::policy::Topology {
    use crate::policy::Topology;
    if crate::policy::prefs().configured_effective().is_some() {
        return crate::effective_topology();
    }
    if std::env::var("PUNKTFUNK_NO_ISOLATE").is_ok() {
        Topology::Extend
    } else {
        Topology::Exclusive
    }
}
