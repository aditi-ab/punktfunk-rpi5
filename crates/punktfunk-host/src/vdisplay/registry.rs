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
    /// Display **group** (shared desktop) id (design §6.1): Linux gives every backend session one
    /// group; Windows is single-group (`1`).
    pub group: u32,
    /// This display's ordinal within its group, in acquire order (0-based) — the §6A "which monitor".
    pub display_index: u32,
    /// Desktop-space top-left origin `(x, y)` (design §6.2): auto-row, or the console's manual
    /// arrangement when configured.
    pub position: (i32, i32),
    /// The stable per-client identity slot keying this display's persistent config + manual layout
    /// (§5.4); `None` for a shared/anonymous identity.
    pub identity_slot: Option<u32>,
    /// The effective topology for this display's group (`"extend"` | `"primary"` | `"exclusive"`).
    pub topology: String,
}

/// The live display set for the mgmt `/display/state` endpoint.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub displays: Vec<DisplayInfo>,
}

/// The effective display topology as a lowercase string for the snapshot (`effective_topology`
/// resolves `Auto` away; the arm is defensive).
fn topology_str() -> String {
    use super::policy::Topology;
    match super::effective_topology() {
        Topology::Extend => "extend",
        Topology::Primary => "primary",
        Topology::Exclusive => "exclusive",
        Topology::Auto => "auto",
    }
    .to_string()
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
        // Windows is single-monitor at this stage (§6.6 multi-monitor is Stage 7): one group, index 0,
        // origin. Its per-client identity lives in the driver (EDID serial / ConnectorIndex), not
        // surfaced here yet.
        let displays = super::manager::snapshot()
            .map(|i| DisplayInfo {
                slot: i.gen,
                backend: i.backend.to_string(),
                mode: i.mode,
                state: i.state.to_string(),
                expires_in_ms: i.expires_in_ms,
                sessions: i.sessions,
                client: None,
                group: 1,
                display_index: 0,
                position: (0, 0),
                identity_slot: None,
                topology: topology_str(),
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
    use crate::vdisplay::policy::{self, Layout, Linger};
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
        /// The identity slot the backend resolved for this display (KWin per-slot naming; `None` for
        /// shared/anonymous or a backend with no per-client identity) — keys the group arrangement +
        /// the `/display/state` slot. Captured at create; kept across a keep-alive reuse.
        identity_slot: Option<u32>,
        /// The topology-restore action for this display's GROUP (design §6.1): re-enable the physical
        /// outputs an `exclusive` topology disabled. At most ONE entry per group carries it (the first
        /// exclusive session); on teardown it hands off to a surviving sibling, and only runs when the
        /// group's last member drops. `None` for extend/primary and non-first / non-exclusive members.
        topology_restore: Option<Restore>,
        /// Generation stamp: a [`DisplayLease`] only releases if its gen still matches (a stale lease
        /// — its entry was reused + re-stamped — is a no-op).
        gen: u64,
    }

    /// A per-group topology-restore action (see [`Entry::topology_restore`]).
    type Restore = Box<dyn FnOnce() + Send>;

    /// Hand off a torn-down display's topology restore (design §6.1 — per-group restore): if a
    /// same-group (backend) sibling survives in `remaining`, MOVE the restore onto it (a later teardown
    /// runs it); if the group is now empty, RETURN the action so the caller runs it (before dropping the
    /// reclaimed display's keepalive, so the physical is re-enabled while our output still exists —
    /// the compositor never sees zero outputs). `None` in → `None` out.
    fn hand_off_restore(
        remaining: &mut [Entry],
        backend: &'static str,
        restore: Option<Restore>,
    ) -> Option<Restore> {
        let action = restore?;
        // At most one restore per group, so any surviving sibling has `None` to receive it.
        match remaining.iter_mut().find(|e| e.backend == backend) {
            Some(sibling) => {
                sibling.topology_restore = Some(action);
                None
            }
            None => Some(action), // group empty → run it now
        }
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
    /// block, and holding the pool lock across it would stall every other acquire/release. Each
    /// expired entry's topology restore is [handed off](hand_off_restore) to a surviving group sibling,
    /// or collected into the returned `restores` when its group empties (run before the entries drop).
    fn take_expired(entries: &mut Vec<Entry>, now: Instant) -> (Vec<Entry>, Vec<Restore>) {
        let mut expired = Vec::new();
        let mut restores = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            if entries[i].life.poll_expiry(now) {
                let mut e = entries.remove(i);
                let backend = e.backend;
                if let Some(r) = hand_off_restore(entries, backend, e.topology_restore.take()) {
                    restores.push(r);
                }
                expired.push(e);
            } else {
                i += 1;
            }
        }
        (expired, restores)
    }

    /// Background thread (started once): reap lingering displays past their deadline.
    fn ensure_timer() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("vdisplay-linger".into())
                .spawn(|| loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let (expired, restores) = {
                        let mut es = reg().entries.lock().unwrap();
                        take_expired(&mut es, Instant::now())
                    };
                    // Re-enable physicals (group emptied) BEFORE dropping the outputs — outside the lock.
                    for restore in restores {
                        restore();
                    }
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

        // Reap expired first (run any group restores + drop outside the lock).
        let (expired, restores) = {
            let mut es = r.entries.lock().unwrap();
            take_expired(&mut es, Instant::now())
        };
        for restore in restores {
            restore();
        }
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

        // Tell the backend whether it's the FIRST display of its group (no same-backend sibling live,
        // §6.1) — so a topology-establishing backend (Mutter exclusive) extends into an already-exclusive
        // desktop rather than re-clobbering the first session's virtual. Best-effort (a concurrent create
        // is a narrow race); single-session is always `first == true` → today's behavior.
        let first_in_group = {
            let es = r.entries.lock().unwrap();
            !es.iter().any(|e| e.backend == backend)
        };
        vd.set_first_in_group(first_in_group);

        // Create a fresh display (NOT under the lock — `vd.create` blocks + spawns threads).
        let real = vd.create(mode)?;
        // The identity slot the backend just resolved (KWin per-slot naming; `None` elsewhere) — keys
        // the group arrangement (manual per-slot positions) + the state slot.
        let identity_slot = vd.last_identity_slot();

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
        // The backend's topology-restore action (KWin `exclusive` → re-enable the disabled physicals),
        // lifted into the group so it runs once when the group's last member drops (§6.1), not at this
        // session's teardown. `None` for non-exclusive / non-first / backends whose topology auto-reverts.
        let topology_restore = vd.take_topology_restore();
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
            identity_slot,
            topology_restore,
            gen,
        };

        // Compute this new display's position in its group (design §6.2) BEFORE pushing, then push
        // under the same lock: the group is the same-backend entries; the new one appends last
        // (rightmost under auto-row). `position_for_new` is pure; the lock is held only across it
        // (I/O-free) — the backend apply is below, outside the lock.
        let position = {
            use crate::vdisplay::layout::Member;
            let layout_policy = policy::prefs()
                .configured_effective()
                .map(|e| e.layout)
                .unwrap_or_default();
            let mut es = r.entries.lock().unwrap();
            // Same-group members (design §6.1): same backend for a shared desktop, but each gamescope
            // spawn is its own group, so a new gamescope never auto-rows against another.
            let new_group = group_key(backend, gen);
            let existing: Vec<(u64, Member)> = es
                .iter()
                .filter(|e| group_key(e.backend, e.gen) == new_group)
                .map(|e| {
                    (
                        e.gen,
                        Member {
                            identity_slot: e.identity_slot,
                            width: e.mode.width as i32,
                        },
                    )
                })
                .collect();
            let new_member = Member {
                identity_slot,
                width: mode.width as i32,
            };
            let pos = position_for_new(existing, new_member, &layout_policy);
            es.push(entry);
            pos
        };
        // Place the new output (design §6.2), best-effort, OUTSIDE the lock (kscreen blocks). Skip the
        // desktop origin `(0, 0)` — it's the compositor default, so a single-display / first-of-group
        // session (and every non-KWin backend, which no-ops `apply_position`) issues no positioning at
        // all: the historical single-display path is untouched. *On-glass-validation-pending.*
        if (position.x, position.y) != (0, 0) {
            vd.apply_position(position.x, position.y);
        }
        Ok(output_for(node_id, preferred_mode, gen))
    }

    /// The [`DisplayLease`] `Drop` path: release the session's hold on the pooled display. The
    /// lifecycle machine decides linger / pin / teardown; a torn-down entry's keepalive drops *after*
    /// the lock is released.
    fn release(gen: u64) {
        let Some(r) = REG.get() else { return };
        let linger = linger();
        let (torn_down, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.gen == gen) else {
                return; // stale lease (entry reused + re-stamped, or already gone) — no-op
            };
            match es[idx].life.release(Instant::now(), linger) {
                Release::Teardown | Release::Noop => {
                    let mut e = es.remove(idx);
                    let backend = e.backend;
                    // Per-group restore (§6.1): hand the physical re-enable to a surviving sibling, or run
                    // it now if this was the group's last member.
                    let restore = hand_off_restore(&mut es, backend, e.topology_restore.take());
                    (Some(e), restore)
                }
                Release::Linger => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — lingering (keep-alive)"
                    );
                    (None, None)
                }
                Release::Pin => {
                    tracing::info!(
                        backend = es[idx].backend,
                        "virtual display: last session left — pinned (keep-alive forever)"
                    );
                    (None, None)
                }
                // Linux entries are single-session (refs == 1), so Decref never occurs; harmless.
                Release::Decref => (None, None),
            }
        };
        // Re-enable the physicals (group emptied) BEFORE dropping the output — outside the lock.
        if let Some(restore) = restore {
            restore();
        }
        if let Some(e) = torn_down {
            tracing::info!(
                backend = e.backend,
                "virtual display torn down (keep-alive off / released)"
            );
            drop(e); // outside the lock — the keepalive Drop may block
        }
    }

    /// One live/kept display, flattened out of the pool under the lock — so the group + arrangement
    /// math (which calls the layout engine) runs OUTSIDE the lock.
    struct Row {
        gen: u64,
        backend: &'static str,
        mode: Mode,
        identity_slot: Option<u32>,
        state: &'static str,
        expires_in_ms: Option<u64>,
        sessions: u32,
    }

    pub(super) fn snapshot() -> Vec<DisplayInfo> {
        let Some(r) = REG.get() else {
            return Vec::new();
        };
        let now = Instant::now();

        // Flatten the live/kept entries under the lock (skip Idle — never stored anyway).
        let rows: Vec<Row> = {
            let es = r.entries.lock().unwrap();
            es.iter()
                .filter_map(|e| {
                    let (state, expires_in_ms, sessions) = match e.life {
                        lifecycle::State::Active { refs } => ("active", None, refs),
                        lifecycle::State::Lingering { until } => (
                            "lingering",
                            Some(until.saturating_duration_since(now).as_millis() as u64),
                            0,
                        ),
                        lifecycle::State::Pinned => ("pinned", None, 0),
                        lifecycle::State::Idle => return None,
                    };
                    Some(Row {
                        gen: e.gen,
                        backend: e.backend,
                        mode: e.mode,
                        identity_slot: e.identity_slot,
                        state,
                        expires_in_ms,
                        sessions,
                    })
                })
                .collect()
        };

        let topology = super::topology_str();
        // The arrangement policy: the console's manual layout when configured, else auto-row.
        let layout_policy: Layout = policy::prefs()
            .configured_effective()
            .map(|e| e.layout)
            .unwrap_or_default();

        assemble_displays(rows, &layout_policy, &topology)
    }

    /// The desktop position for a display just appended to its group (design §6.2): the group's
    /// `existing` members (each with its acquire `gen`) plus `new` last, ordered by `gen`, arranged by
    /// the pure [`layout`] engine, taking the new member's placement. Pure — so the append-in-acquire-
    /// order + auto-row/manual arrangement is unit-tested independent of the pool/global.
    fn position_for_new(
        mut existing: Vec<(u64, crate::vdisplay::layout::Member)>,
        new: crate::vdisplay::layout::Member,
        layout_policy: &Layout,
    ) -> crate::vdisplay::layout::Placement {
        existing.sort_by_key(|(g, _)| *g);
        let mut members: Vec<crate::vdisplay::layout::Member> =
            existing.into_iter().map(|(_, m)| m).collect();
        members.push(new);
        *crate::vdisplay::layout::arrange(&members, layout_policy)
            .last()
            .expect("members is non-empty (just pushed `new`)")
    }

    /// The display **group** a backend+display belongs to (design §6.1). The desktop compositors
    /// (KWin/Mutter/wlroots) put every managed output on ONE desktop → one group per backend. A
    /// gamescope **spawn** is an independent nested session per client (no shared desktop), so each
    /// gamescope display is its OWN group — never auto-rowed against, or topology-/restore-grouped with,
    /// another gamescope session.
    fn group_key(backend: &str, gen: u64) -> String {
        if backend == "gamescope" {
            format!("gamescope#{gen}")
        } else {
            backend.to_string()
        }
    }

    /// Group the flattened rows into the mgmt `/display/state` view (design §6.1/§6.2) by
    /// [`group_key`], ordered by acquire (`gen`), with each member's position from the pure [`layout`]
    /// engine. Pure — no I/O, no global — so the grouping / ordering / position assignment is
    /// unit-tested against synthetic rows.
    fn assemble_displays(
        rows: Vec<Row>,
        layout_policy: &Layout,
        topology: &str,
    ) -> Vec<DisplayInfo> {
        use crate::vdisplay::layout::{self, Member};

        // Small stable group ids by sorted group key — deterministic; in practice a host runs one live
        // desktop backend → group 1 (with each gamescope spawn its own group).
        let mut keys: Vec<String> = rows.iter().map(|r| group_key(r.backend, r.gen)).collect();
        keys.sort();
        keys.dedup();

        let mut out: Vec<DisplayInfo> = Vec::new();
        for (gi, key) in keys.iter().enumerate() {
            // This group's members in acquire order (gen ascending) → display_index + arrangement.
            let mut idx: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| &group_key(row.backend, row.gen) == key)
                .map(|(i, _)| i)
                .collect();
            idx.sort_by_key(|&i| rows[i].gen);
            let members: Vec<Member> = idx
                .iter()
                .map(|&i| Member {
                    identity_slot: rows[i].identity_slot,
                    width: rows[i].mode.width as i32,
                })
                .collect();
            let places = layout::arrange(&members, layout_policy);
            for (ord, &i) in idx.iter().enumerate() {
                let row = &rows[i];
                let p = places[ord];
                out.push(DisplayInfo {
                    slot: row.gen,
                    backend: row.backend.to_string(),
                    mode: (row.mode.width, row.mode.height, row.mode.refresh_hz),
                    state: row.state.to_string(),
                    expires_in_ms: row.expires_in_ms,
                    sessions: row.sessions,
                    client: None,
                    group: gi as u32 + 1,
                    display_index: ord as u32,
                    position: (p.x, p.y),
                    identity_slot: row.identity_slot,
                    topology: topology.to_string(),
                });
            }
        }
        out
    }

    pub(super) fn force_release(slot: Option<u64>) -> usize {
        let Some(r) = REG.get() else { return 0 };
        let (released, restores) = {
            let mut es = r.entries.lock().unwrap();
            let mut out = Vec::new();
            let mut restores = Vec::new();
            let mut i = 0;
            while i < es.len() {
                let selected = slot.is_none_or(|s| es[i].gen == s);
                if selected && es[i].life.force_release() {
                    let mut e = es.remove(i);
                    let backend = e.backend;
                    let restore = e.topology_restore.take();
                    if let Some(rst) = hand_off_restore(&mut es, backend, restore) {
                        restores.push(rst);
                    }
                    out.push(e);
                } else {
                    i += 1;
                }
            }
            (out, restores)
        };
        let n = released.len();
        // Re-enable physicals (group emptied) BEFORE dropping the outputs — outside the lock.
        for restore in restores {
            restore();
        }
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vdisplay::policy::{Layout, LayoutMode, Position};
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// A minimal pool entry for the pure teardown/restore tests (dummy keepalive; the
        /// `hand_off_restore` logic only reads `backend` + `topology_restore`).
        fn test_entry(backend: &'static str, gen: u64, restore: Option<Restore>) -> Entry {
            Entry {
                life: lifecycle::State::default(),
                keepalive: Box::new(()),
                node_id: 0,
                preferred_mode: None,
                mode: Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                },
                backend,
                identity_slot: None,
                topology_restore: restore,
                gen,
            }
        }

        /// A restore closure that flips `flag` when run — so a test can assert exactly WHEN it fires.
        fn flag_restore(flag: &Arc<AtomicBool>) -> Restore {
            let f = flag.clone();
            Box::new(move || f.store(true, Ordering::SeqCst))
        }

        #[test]
        fn topology_restore_floats_to_a_sibling_then_runs_on_the_last_teardown() {
            let ran = Arc::new(AtomicBool::new(false));
            // Two KWin displays in one group; the first (gen 1) carries the group's restore.
            let mut pool = vec![
                test_entry("kwin", 1, Some(flag_restore(&ran))),
                test_entry("kwin", 2, None),
            ];

            // Tear down the restore-carrier while its sibling is still alive → transfer, don't run.
            let mut e1 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", e1.topology_restore.take());
            assert!(out.is_none(), "transferred, not run");
            assert!(!ran.load(Ordering::SeqCst));
            // The restore floated onto the surviving sibling.
            assert!(pool[0].topology_restore.is_some());

            // Tear down the last member → group empty → the restore is returned to run.
            let mut e2 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", e2.topology_restore.take());
            let action = out.expect("group empty → run the restore");
            assert!(!ran.load(Ordering::SeqCst), "not run yet");
            action();
            assert!(ran.load(Ordering::SeqCst), "runs on the last drop");
        }

        #[test]
        fn single_session_topology_restore_runs_on_its_own_teardown() {
            // The validated single-display case: one exclusive session → restore runs at its teardown.
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("kwin", 1, Some(flag_restore(&ran)))];
            let mut e = pool.remove(0);
            let action = hand_off_restore(&mut pool, "kwin", e.topology_restore.take())
                .expect("last (only) member → run");
            action();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn tearing_down_a_non_carrier_first_leaves_the_restore_for_last() {
            let ran = Arc::new(AtomicBool::new(false));
            // gen 2 carries the restore; gen 1 does not (a later exclusive session found the physical
            // already disabled).
            let mut pool = vec![
                test_entry("kwin", 1, None),
                test_entry("kwin", 2, Some(flag_restore(&ran))),
            ];
            // Tear down the non-carrier first → nothing to hand off, carrier untouched.
            let mut e1 = pool.remove(0);
            assert!(hand_off_restore(&mut pool, "kwin", e1.topology_restore.take()).is_none());
            // The carrier (gen 2) still holds the group's restore.
            assert!(pool[0].topology_restore.is_some());
            // Now the carrier (last member) → run.
            let mut e2 = pool.remove(0);
            hand_off_restore(&mut pool, "kwin", e2.topology_restore.take())
                .expect("last member → run")();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn restore_never_floats_across_backends() {
            // group = backend: a KWin restore must not land on a Mutter display (a different desktop).
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("mutter", 2, None)];
            let out = hand_off_restore(&mut pool, "kwin", Some(flag_restore(&ran)));
            assert!(out.is_some(), "no same-backend sibling → return to run");
            assert!(
                pool[0].topology_restore.is_none(),
                "restore must not cross into another backend's group"
            );
        }

        fn row(gen: u64, backend: &'static str, w: u32, slot: Option<u32>) -> Row {
            Row {
                gen,
                backend,
                mode: Mode {
                    width: w,
                    height: 1080,
                    refresh_hz: 60,
                },
                identity_slot: slot,
                state: "active",
                expires_in_ms: None,
                sessions: 1,
            }
        }

        #[test]
        fn groups_by_backend_and_auto_rows_in_acquire_order() {
            // Two KWin displays (acquired gen 5 then gen 2 — deliberately out of vec order) + a Mutter one.
            let rows = vec![
                row(5, "kwin", 2560, Some(1)),
                row(2, "kwin", 1920, Some(7)),
                row(9, "mutter", 3840, None),
            ];
            let out = assemble_displays(rows, &Layout::default(), "exclusive");

            let kwin: Vec<&DisplayInfo> = out.iter().filter(|d| d.backend == "kwin").collect();
            assert_eq!(kwin.len(), 2);
            assert_eq!(kwin[0].slot, 2); // lower gen (earlier acquire) sorts to index 0
            assert_eq!(kwin[0].display_index, 0);
            assert_eq!(kwin[0].position, (0, 0));
            assert_eq!(kwin[1].slot, 5);
            assert_eq!(kwin[1].display_index, 1);
            assert_eq!(kwin[1].position, (1920, 0)); // auto-row: after the 1920px gen-2 display
            assert_eq!(kwin[0].topology, "exclusive");

            // A distinct backend is a distinct group.
            let mutter = out.iter().find(|d| d.backend == "mutter").unwrap();
            assert_ne!(mutter.group, kwin[0].group);
            assert_eq!(mutter.display_index, 0);
            assert_eq!(mutter.position, (0, 0));
        }

        #[test]
        fn position_for_new_appends_right_in_acquire_order() {
            use crate::vdisplay::layout::{Member, Placement};
            let m = |slot, w| Member {
                identity_slot: slot,
                width: w,
            };
            // Existing group (given out of gen order): gen 8 @ 1920 acquired AFTER gen 3 @ 2560.
            let existing = vec![(8, m(Some(2), 1920)), (3, m(Some(1), 2560))];
            // A new 1280-wide display appends to the right of 2560 + 1920.
            let pos = position_for_new(existing, m(Some(5), 1280), &Layout::default());
            assert_eq!(pos, Placement { x: 4480, y: 0 });
            // First-of-group lands at the origin (so the registry skips the apply).
            let first = position_for_new(vec![], m(None, 3840), &Layout::default());
            assert_eq!(first, Placement { x: 0, y: 0 });
        }

        #[test]
        fn position_for_new_honors_a_manual_pin() {
            use crate::vdisplay::layout::{Member, Placement};
            let mut positions = BTreeMap::new();
            positions.insert("5".to_string(), Position { x: 100, y: 200 });
            let layout = Layout {
                mode: LayoutMode::Manual,
                positions,
            };
            let new = Member {
                identity_slot: Some(5),
                width: 1280,
            };
            let pos = position_for_new(vec![(1, new)], new, &layout);
            assert_eq!(pos, Placement { x: 100, y: 200 });
        }

        #[test]
        fn gamescope_spawns_are_separate_groups() {
            // Two independent gamescope spawns must NOT share a group or auto-row against each other.
            let rows = vec![
                row(1, "gamescope", 1920, None),
                row(2, "gamescope", 1280, None),
            ];
            let out = assemble_displays(rows, &Layout::default(), "extend");
            assert_eq!(out.len(), 2);
            assert_ne!(out[0].group, out[1].group, "distinct groups");
            // Each is display 0 of its own group, at the origin (not auto-rowed against the other).
            assert_eq!(out[0].display_index, 0);
            assert_eq!(out[1].display_index, 0);
            assert_eq!(out[0].position, (0, 0));
            assert_eq!(out[1].position, (0, 0));
        }

        #[test]
        fn manual_layout_keys_positions_by_identity_slot() {
            // Client 7 arranged to the LEFT of client 1 (reversed vs. auto-row).
            let rows = vec![row(1, "kwin", 2560, Some(1)), row(2, "kwin", 1920, Some(7))];
            let mut positions = BTreeMap::new();
            positions.insert("1".to_string(), Position { x: 1920, y: 0 });
            positions.insert("7".to_string(), Position { x: 0, y: 0 });
            let layout = Layout {
                mode: LayoutMode::Manual,
                positions,
            };
            let out = assemble_displays(rows, &layout, "extend");
            let by_slot = |s: u32| out.iter().find(|d| d.identity_slot == Some(s)).unwrap();
            assert_eq!(by_slot(1).position, (1920, 0));
            assert_eq!(by_slot(7).position, (0, 0));
        }
    }
}
