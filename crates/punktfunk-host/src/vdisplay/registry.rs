//! Neutral **facade over the per-OS virtual-display lifecycle owners**, for the management API's
//! `/display/state` + `/display/release` (design: `design/display-management.md` §7).
//!
//! Windows already owns its display lifecycle in [`super::manager::VirtualDisplayManager`] (one
//! shared IddCx monitor, refcounted, lingering); this facade reads and controls it. Linux keep-alive
//! (a per-session output pool driven by [`super::lifecycle`]) lands in a following increment — it
//! needs on-glass validation on a GPU box, which the current headless VM can't provide — so until
//! then the Linux side reports no managed displays and release is a no-op.
//!
//! The lifecycle *state machine* ([`super::lifecycle::State`]) is the platform-neutral core both
//! sides converge on; Windows adopts it when its manager is refactored onto it (that unification is
//! deferred so the on-glass-validated Windows path stays untouched this stage).

/// One live or kept virtual display, for the mgmt snapshot.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// A stable-enough id for the `/display/release` slot argument (the backend's generation stamp).
    pub slot: u64,
    /// Backend name (`"pf-vdisplay"`, `"kwin"`, …).
    pub backend: String,
    /// `(width, height, refresh_hz)`.
    pub mode: (u32, u32, u32),
    /// `"active"` | `"lingering"` | `"pinned"`.
    pub state: String,
    /// Milliseconds until a lingering display is torn down (`None` when active/pinned).
    pub expires_in_ms: Option<u64>,
    /// Live sessions holding the display.
    pub sessions: u32,
    /// Short client label (cert-fp prefix / peer), when the owner tracks it.
    pub client: Option<String>,
}

/// The live display set for the mgmt `/display/state` endpoint.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub displays: Vec<DisplayInfo>,
}

/// Snapshot the host's managed virtual displays. Cheap + side-effect-free (a state-lock read);
/// safe per management request.
pub fn snapshot() -> Snapshot {
    #[cfg(target_os = "windows")]
    {
        let displays = super::manager::snapshot()
            .map(|i| DisplayInfo {
                slot: i.gen,
                backend: i.backend.to_string(),
                mode: i.mode,
                state: i.state.to_string(),
                expires_in_ms: i.expires_in_ms,
                sessions: i.sessions,
                client: None,
            })
            .into_iter()
            .collect();
        Snapshot { displays }
    }
    #[cfg(not(target_os = "windows"))]
    {
        // Linux keep-alive pool: not yet (needs GPU-box validation) — no managed displays to report.
        Snapshot::default()
    }
}

/// Force-release kept (lingering/pinned) displays now — the `/display/release` endpoint. `slot`
/// selects one by [`DisplayInfo::slot`]; `None` releases every kept display. Active displays are
/// refused (releasing a display with live sessions is session management). Returns the number
/// released.
pub fn release(_slot: Option<u64>) -> usize {
    #[cfg(target_os = "windows")]
    {
        // Windows manages a single shared monitor at Stage 1, so `slot` is moot — release the one
        // lingering monitor if present. (Multi-monitor gives `slot` meaning later.)
        usize::from(super::manager::force_release())
    }
    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}
