//! Host-lifetime **virtual-display registry** (design: `design/display-management.md` §3/§7): the
//! owner of the display lifecycle, so a display can outlive the session that created it (keep-alive)
//! and the management API can list + release kept displays.
//!
//! **Windows** already owns its lifecycle in [`super::manager::VirtualDisplayManager`] (one shared
//! IddCx monitor, refcounted, lingering); [`acquire`] there is a pass-through to `vd.create` (the
//! manager does the leasing), and [`snapshot`]/[`release`] read/control it.
//!
//! **Linux** gains a per-session **pool** here, driven by the pure [`super::lifecycle`] machine. The
//! key enabling fact: KWin / Mutter / gamescope put their capture node on the *default* PipeWire
//! daemon (`VirtualOutput::remote_fd == None`), reachable by `node_id` alone — so keeping the
//! backend's keepalive alive keeps the node alive, and a reconnect just re-attaches a fresh PipeWire
//! consumer to the same `node_id`. No fd dup / re-open needed. wlroots (`remote_fd == Some`, the
//! sandboxed xdpw portal) can't be kept without re-opening the portal fd per attach, so it is passed
//! through unchanged (teardown-on-drop, today's behavior) until that fresh-portal-capture re-attach
//! lands — a runtime gate on `remote_fd.is_some()`.
//!
//! The ownership split: the session's capturer no longer owns the real keepalive — the registry does.
//! [`acquire`] hands the session a `VirtualOutput` whose `keepalive` is a lightweight, gen-stamped
//! `DisplayLease` (mirrors the Windows `MonitorLease`); dropping it releases the registry refcount,
//! and the lifecycle machine decides linger / teardown. `capture_virtual_output`'s signature is
//! unchanged — it just holds a lease instead of the real keepalive.

use anyhow::Result;

/// One live or kept virtual display, for the mgmt snapshot.
#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// A stable-enough id for the `/display/release` slot argument (the owner's generation stamp).
    pub slot: u64,
    /// Backend name (`"pf-vdisplay"`, `"kwin"`, `"mutter"`, …).
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

/// Acquire a virtual display for a session: reuse a kept (lingering/pinned) display of the same
/// backend + mode if one exists, else create a fresh one. Returns a [`VirtualOutput`](super::VirtualOutput)
/// the capturer consumes as before — but its `keepalive` is a registry lease, so the *display*
/// outlives the capturer per the keep-alive policy.
///
/// Windows delegates to the [`manager`](super::manager) via `vd.create` (unchanged); Linux uses the
/// pool below; other platforms pass through.
pub fn acquire(
    vd: &mut Box<dyn super::VirtualDisplay>,
    mode: super::Mode,
) -> Result<super::VirtualOutput> {
    #[cfg(target_os = "linux")]
    {
        linux::acquire(vd, mode)
    }
    #[cfg(not(target_os = "linux"))]
    {
        vd.create(mode)
    }
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
    #[cfg(target_os = "linux")]
    {
        Snapshot {
            displays: linux::snapshot(),
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Snapshot::default()
    }
}

/// Force-release kept (lingering/pinned) displays now — the `/display/release` endpoint. `slot`
/// selects one by [`DisplayInfo::slot`]; `None` releases every kept display. Active displays are
/// refused (releasing a display with live sessions is session management). Returns the number
/// released.
pub fn release(slot: Option<u64>) -> usize {
    #[cfg(target_os = "windows")]
    {
        // Windows manages a single shared monitor at Stage 1, so `slot` is moot — release the one
        // lingering monitor if present. (Multi-monitor gives `slot` meaning later.)
        let _ = slot;
        usize::from(super::manager::force_release())
    }
    #[cfg(target_os = "linux")]
    {
        linux::force_release(slot)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = slot;
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Linux keep-alive pool
// ---------------------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, Once, OnceLock};
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use super::DisplayInfo;
    use crate::vdisplay::lifecycle::{self, Release};
    use crate::vdisplay::policy::{self, Linger};
    use crate::vdisplay::{Mode, VirtualDisplay, VirtualOutput};

    /// One pooled display: the lifecycle state + the backend's REAL keepalive (kept alive here so the
    /// compositor output — and thus its PipeWire `node_id` — survives past the session), plus the
    /// capture coordinates a reconnecting session needs.
    struct Entry {
        life: lifecycle::State,
        /// The backend's keepalive (KWin Wayland conn / Mutter D-Bus session / gamescope child). Its
        /// `Drop` releases the compositor output — so it is dropped only on teardown/expiry.
        keepalive: Box<dyn Send>,
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        mode: Mode,
        backend: &'static str,
        /// Generation stamp: a [`DisplayLease`] only releases if its gen still matches (a stale lease
        /// — its entry was reused + re-stamped — is a no-op).
        gen: u64,
    }

    struct Reg {
        entries: Mutex<Vec<Entry>>,
        gen: AtomicU64,
    }

    static REG: OnceLock<Reg> = OnceLock::new();

    fn reg() -> &'static Reg {
        REG.get_or_init(|| Reg {
            entries: Mutex::new(Vec::new()),
            gen: AtomicU64::new(1),
        })
    }

    /// The linger resolution for Linux: the console policy's `keep_alive` when configured, else
    /// **Immediate** (today's behavior — a Linux disconnect tears the output down at once).
    fn linger() -> Linger {
        policy::prefs()
            .configured_effective()
            .map(|e| e.keep_alive.linger())
            .unwrap_or(Linger::Immediate)
    }

    /// Remove entries whose linger deadline has passed, returning them so the caller drops (tears
    /// them down) *after* releasing the lock — a backend keepalive `Drop` (Mutter D-Bus Stop) can
    /// block, and holding the pool lock across it would stall every other acquire/release.
    fn take_expired(entries: &mut Vec<Entry>, now: Instant) -> Vec<Entry> {
        let mut expired = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            if entries[i].life.poll_expiry(now) {
                expired.push(entries.remove(i));
            } else {
                i += 1;
            }
        }
        expired
    }

    /// Background thread (started once): reap lingering displays past their deadline.
    fn ensure_timer() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("vdisplay-linger".into())
                .spawn(|| loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let expired = {
                        let mut es = reg().entries.lock().unwrap();
                        take_expired(&mut es, Instant::now())
                    };
                    for e in expired {
                        tracing::info!(
                            backend = e.backend,
                            "virtual display: linger expired — torn down"
                        );
                        drop(e); // outside the lock
                    }
                });
        });
    }

    /// Build the session-facing [`VirtualOutput`]: the kept node + a fresh gen-stamped lease. Only
    /// the poolable (`remote_fd == None`) backends reach here, so `remote_fd` is always `None`.
    fn output_for(
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        gen: u64,
    ) -> VirtualOutput {
        VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode,
            keepalive: Box::new(DisplayLease { gen }),
        }
    }

    pub(super) fn acquire(vd: &mut Box<dyn VirtualDisplay>, mode: Mode) -> Result<VirtualOutput> {
        ensure_timer();
        let backend = vd.name();
        let r = reg();

        // Reap expired first (drop outside the lock).
        let expired = {
            let mut es = r.entries.lock().unwrap();
            take_expired(&mut es, Instant::now())
        };
        drop(expired);

        // Reuse: a kept (lingering/pinned) display of the same backend + mode. A reconnecting session
        // re-attaches a fresh PipeWire consumer to the still-live `node_id`.
        {
            let mut es = r.entries.lock().unwrap();
            if let Some(e) = es.iter_mut().find(|e| {
                matches!(
                    e.life,
                    lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
                ) && e.backend == backend
                    && e.mode == mode
            }) {
                // Lingering/Pinned → Active (Acquire::Reuse); side effect matters, value is known.
                e.life.acquire();
                let gen = r.gen.fetch_add(1, Ordering::Relaxed);
                e.gen = gen;
                let out = output_for(e.node_id, e.preferred_mode, gen);
                tracing::info!(
                    backend,
                    node_id = e.node_id,
                    "virtual display reused (keep-alive reconnect)"
                );
                return Ok(out);
            }
        }

        // Create a fresh display (NOT under the lock — `vd.create` blocks + spawns threads).
        let real = vd.create(mode)?;

        // wlroots (remote_fd = Some, sandboxed xdpw portal) can't be kept without re-opening the
        // portal fd per attach — pass it through unchanged (capturer owns it, teardown on drop). The
        // poolable backends put their node on the default daemon (remote_fd = None).
        if real.remote_fd.is_some() {
            tracing::debug!(
                backend,
                "virtual display not poolable (portal fd) — keep-alive off for this backend"
            );
            return Ok(real);
        }

        let node_id = real.node_id;
        let preferred_mode = real.preferred_mode;
        let gen = r.gen.fetch_add(1, Ordering::Relaxed);
        let mut life = lifecycle::State::default();
        life.acquire(); // Idle → Active{refs:1} (Acquire::Create)
        let entry = Entry {
            life,
            keepalive: real.keepalive,
            node_id,
            preferred_mode,
            mode,
            backend,
            gen,
        };
        r.entries.lock().unwrap().push(entry);
        Ok(output_for(node_id, preferred_mode, gen))
    }

    /// The [`DisplayLease`] `Drop` path: release the session's hold on the pooled display. The
    /// lifecycle machine decides linger / pin / teardown; a torn-down entry's keepalive drops *after*
    /// the lock is released.
    fn release(gen: u64) {
        let Some(r) = REG.get() else { return };
        let linger = linger();
        let torn_down = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.gen == gen) else {
                return; // stale lease (entry reused + re-stamped, or already gone) — no-op
            };
            match es[idx].life.release(Instant::now(), linger) {
                Release::Teardown | Release::Noop => Some(es.remove(idx)),
                Release::Linger => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — lingering (keep-alive)"
                    );
                    None
                }
                Release::Pin => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — pinned (keep-alive forever)"
                    );
                    None
                }
                // Linux entries are single-session (refs == 1), so Decref never occurs; harmless.
                Release::Decref => None,
            }
        };
        if let Some(e) = torn_down {
            tracing::info!(
                backend = e.backend,
                "virtual display torn down (keep-alive off / released)"
            );
            drop(e); // outside the lock — the keepalive Drop may block
        }
    }

    pub(super) fn snapshot() -> Vec<DisplayInfo> {
        let Some(r) = REG.get() else {
            return Vec::new();
        };
        let now = Instant::now();
        r.entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let (state, expires_in_ms, sessions) = match e.life {
                    lifecycle::State::Active { refs } => ("active", None, refs),
                    lifecycle::State::Lingering { until } => (
                        "lingering",
                        Some(until.saturating_duration_since(now).as_millis() as u64),
                        0,
                    ),
                    lifecycle::State::Pinned => ("pinned", None, 0),
                    // Idle entries are never stored (removed on teardown).
                    lifecycle::State::Idle => return None,
                };
                Some(DisplayInfo {
                    slot: e.gen,
                    backend: e.backend.to_string(),
                    mode: (e.mode.width, e.mode.height, e.mode.refresh_hz),
                    state: state.to_string(),
                    expires_in_ms,
                    sessions,
                    client: None,
                })
            })
            .collect()
    }

    pub(super) fn force_release(slot: Option<u64>) -> usize {
        let Some(r) = REG.get() else { return 0 };
        let released = {
            let mut es = r.entries.lock().unwrap();
            let mut out = Vec::new();
            let mut i = 0;
            while i < es.len() {
                let selected = slot.is_none_or(|s| es[i].gen == s);
                if selected && es[i].life.force_release() {
                    out.push(es.remove(i));
                } else {
                    i += 1;
                }
            }
            out
        };
        let n = released.len();
        for e in released {
            tracing::info!(
                backend = e.backend,
                "virtual display released (mgmt /display/release)"
            );
            drop(e);
        }
        n
    }

    /// The session's refcount handle — the `keepalive` the capturer holds. `Drop` releases the
    /// registry hold; a stale lease (its entry was reused + re-stamped, or torn down) is a no-op.
    struct DisplayLease {
        gen: u64,
    }

    impl Drop for DisplayLease {
        fn drop(&mut self) {
            release(self.gen);
        }
    }
}
