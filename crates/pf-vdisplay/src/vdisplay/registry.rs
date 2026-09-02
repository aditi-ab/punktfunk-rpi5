//! Host-lifetime virtual-display registry (`design/display-management.md`).
//!
//! Owns display lifecycle so a display can outlive the session that created it
//! (keep-alive) and the management API can list and release kept displays.
//!
//! Windows: [`super::manager::VirtualDisplayManager`] already leases one IddCx
//! monitor; [`acquire`] is `vd.create`, [`snapshot`]/[`release`] read it.
//!
//! Linux: a per-session pool driven by [`super::lifecycle`]. Capture on the
//! default PipeWire daemon (`remote_fd == None`) stays alive with the keepalive;
//! reconnect re-attaches to the same `node_id`. wlroots (`remote_fd == Some`)
//! cannot re-open the portal fd, so it stays teardown-on-drop.
//!
//! [`acquire`] returns a `VirtualOutput` whose `keepalive` is a generation-stamped
//! `DisplayLease`. Dropping it releases the registry refcount; the lifecycle
//! machine decides linger vs teardown.

use anyhow::Result;

#[derive(Clone, Debug)]
pub struct DisplayInfo {
    /// Generation stamp used as the `/display/release` slot argument.
    pub slot: u64,
    pub backend: String,
    pub mode: (u32, u32, u32),
    /// `"active"` | `"lingering"` | `"pinned"`.
    pub state: String,
    /// Milliseconds until linger teardown. `None` when active or pinned.
    pub expires_in_ms: Option<u64>,
    pub sessions: u32,
    /// Cert-fp prefix / peer, when the owner tracks one.
    pub client: Option<String>,
    /// Shared-desktop group. Linux: one per backend session. Windows: always `1`.
    pub group: u32,
    /// Ordinal within the group, acquire order, 0-based.
    pub display_index: u32,
    /// Desktop-space top-left. Auto-row, or the console's manual arrangement.
    pub position: (i32, i32),
    /// Persistent-config / manual-layout key. `None` = shared/anonymous.
    pub identity_slot: Option<u32>,
    /// `"extend"` | `"primary"` | `"exclusive"`.
    pub topology: String,
}

/// Live display set for `/display/state`.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub displays: Vec<DisplayInfo>,
}

/// Snapshot topology string. `effective_topology` resolves `Auto`; the arm is defensive.
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

/// Lease a virtual display: reuse a kept (lingering/pinned) one of the same
/// backend + mode, else create. The returned [`VirtualOutput`](super::VirtualOutput)
/// `keepalive` is a registry lease, so the display outlives the capturer.
///
/// Windows: `vd.create` via [`manager`](super::manager). Linux: the pool below.
///
/// `quit` set means a deliberate stop — teardown now, skip linger. A network
/// drop leaves it false.
///
/// `supersedes` is the pool generation this acquire replaces (create-before-drop
/// on a mode switch). Without it the predecessor still counts as a live sibling
/// and Primary/Exclusive becomes Extend. `None` everywhere else.
pub fn acquire(
    vd: &mut Box<dyn super::VirtualDisplay>,
    mode: super::Mode,
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    supersedes: Option<u64>,
) -> Result<super::VirtualOutput> {
    let backend = vd.name();
    #[cfg(target_os = "linux")]
    let out = linux::acquire(vd, mode, quit, supersedes);
    #[cfg(not(target_os = "linux"))]
    let out = {
        // Windows linger/quit is `VirtualDisplay::set_quit_flag` on the backend
        // (set before any `create`, so retry-hold sees it), not these params.
        // Supersede is Linux-pool-only; the manager resizes in place.
        let _ = (quit, supersedes);
        vd.create(mode)
    };
    // `Created` is existence, not reuse. Linux has `reused_gen`; Windows does
    // not, so JOIN/linger reuse still reports Created until the manager
    // surfaces its acquire outcome. Linger expiry has the matching missing
    // `Released` — see `release`.
    #[cfg(target_os = "linux")]
    let created = matches!(&out, Ok(o) if o.reused_gen.is_none());
    #[cfg(not(target_os = "linux"))]
    let created = out.is_ok();
    if created {
        crate::emit_display_event(crate::DisplayEvent::Created {
            backend: backend.to_string(),
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
        });
    }
    out
}

/// Cheap lock-read of the host's managed virtual displays.
pub fn snapshot() -> Snapshot {
    #[cfg(target_os = "windows")]
    {
        // One shared-desktop group. `identity_slot` is None for anonymous slot 0.
        let displays = super::manager::snapshot()
            .into_iter()
            .enumerate()
            .map(|(idx, i)| DisplayInfo {
                slot: i.generation,
                backend: i.backend.to_string(),
                mode: i.mode,
                state: i.state.to_string(),
                expires_in_ms: i.expires_in_ms,
                sessions: i.sessions,
                client: None,
                group: 1,
                display_index: idx as u32,
                position: i.position,
                identity_slot: (i.slot_id != 0).then_some(i.slot_id),
                topology: topology_str(),
            })
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

/// `/display/release`: force-release kept (lingering/pinned) displays. `slot`
/// selects one by [`DisplayInfo::slot`]; `None` releases every kept display.
/// Active displays are refused. Returns the count released.
pub fn release(slot: Option<u64>) -> usize {
    #[cfg(target_os = "windows")]
    let released = super::manager::force_release(slot);
    #[cfg(target_os = "linux")]
    let released = linux::force_release(slot);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let released = {
        let _ = slot;
        0
    };
    // Linux already emits from every pool teardown; emitting here would
    // double-count. Windows has no other hook, so this endpoint is the only
    // `Released` the console sees.
    #[cfg(not(target_os = "linux"))]
    if released > 0 {
        crate::emit_display_event(crate::DisplayEvent::Released {
            count: released as u32,
        });
    }
    released
}

/// Tear down a reused-but-dead pool entry by generation. The pipeline builder
/// calls this when the first frame fails on a REUSED [`acquire`] so the next
/// acquire creates fresh. No-op off Linux or if already gone; the later
/// stale-generation lease drop no-ops too.
pub fn mark_failed(generation: u64) {
    #[cfg(target_os = "linux")]
    linux::mark_failed(generation);
    #[cfg(not(target_os = "linux"))]
    let _ = generation;
}

/// Force-release a superseded kept display by generation
/// (`design/midstream-resolution-resize.md`). A mode-switch lease drop looks
/// like a disconnect, so linger/forever would keep stale-mode monitors. Called
/// once the new pipeline is up. Active entries are refused; already-gone is a
/// no-op. No-op off Linux (Windows resizes in place).
pub fn retire(generation: u64) {
    #[cfg(target_os = "linux")]
    linux::retire(generation);
    #[cfg(not(target_os = "linux"))]
    let _ = generation;
}

/// Reap every kept display of `backend` whose compositor is gone
/// (`design/gamemode-and-dedicated-sessions.md`). Called from the session-switch
/// watcher. No-op off Linux.
pub fn invalidate_backend(backend: &str) {
    #[cfg(target_os = "linux")]
    linux::invalidate_backend(backend);
    #[cfg(not(target_os = "linux"))]
    let _ = backend;
}

/// Identity slots driving a pooled display — eviction guard for
/// [`identity::live_slot_ids`](crate::identity). Lingering/pinned count: the
/// output still exists. Linux-only.
#[cfg(target_os = "linux")]
pub(crate) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
    linux::live_identity_slots()
}

/// Linux pool size (live and kept) for [`admission`](crate::admission)
/// `max_displays`. A kept display still owns a compositor output, so it
/// consumes the budget — same rule as `manager::snapshot().len()` on Windows.
#[cfg(target_os = "linux")]
pub(crate) fn live_display_count() -> u32 {
    linux::live_display_count()
}

/// Pure pool rules (entry, group, reuse, expiry, snapshot). No OS API — `mod
/// linux` owns the global and the backend calls — so the sweep is unit-tested
/// on every host this crate builds on.
///
/// Off Linux nothing calls it; the `dead_code` allow is cfg-gated so a truly
/// dead helper still fails on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod pool {
    use std::time::Instant;

    use super::DisplayInfo;
    use crate::lifecycle;
    use crate::policy::{Layout, Linger};
    use crate::Mode;

    /// One pooled display. The backend keepalive lives here so the compositor
    /// output (and its PipeWire `node_id`) outlives the session.
    pub(super) struct Entry {
        pub(super) life: lifecycle::State,
        /// Backend keepalive. Never read: holding it is the behaviour; `Drop`
        /// releases the compositor output. Deleting the field as unused would
        /// tear every pooled display down at create.
        #[allow(dead_code)]
        pub(super) keepalive: Box<dyn Send>,
        pub(super) node_id: u32,
        pub(super) preferred_mode: Option<(u32, u32, u32)>,
        /// Compositor output name ([`VirtualOutput::output_name`]). Kept across
        /// reuse so the reused head answers with the same name as a fresh create.
        pub(super) output_name: Option<String>,
        pub(super) mode: Mode,
        pub(super) backend: &'static str,
        /// Identity slot at create (`None` = anonymous). Kept across reuse; keys
        /// group arrangement and `/display/state`.
        pub(super) identity_slot: Option<u32>,
        /// Per-group topology restore: re-enable physicals that `exclusive`
        /// disabled. At most one entry per group holds it; teardown hands it to
        /// a sibling, and it runs only when the last member drops.
        pub(super) topology_restore: Option<Restore>,
        /// Launch command at create. Reuse requires an exact match so game A
        /// never serves game B. `None` = desktop / no nested command.
        pub(super) launch: Option<String>,
        /// Isolation identity at create (`design/gamescope-multiuser.md`). Reuse
        /// requires an exact match (EIS path + Pulse sinks baked into env).
        /// `None` = not isolated.
        pub(super) isolation: Option<String>,
        /// Session epoch at create. Reuse requires a match; linger reaps a
        /// stale-epoch entry (compositor replaced under it).
        pub(super) epoch: u64,
        /// `DisplayLease` releases only if this still matches; a stale lease
        /// (reused + re-stamped) is a no-op.
        pub(super) generation: u64,
        /// Cursor mode at create (metadata-pointer vs compositor-embedded).
        /// Reuse requires an exact match or the pointer is missing or unforwardable.
        pub(super) hw_cursor: bool,
        /// Colourimetry at create (HDR vs SDR). Reuse requires an exact match:
        /// a kept SDR gamescope has no `--hdr-enabled`, and the reverse would
        /// negotiate 8-bit off a PQ composite.
        pub(super) hdr: bool,
    }

    pub(super) type Restore = Box<dyn FnOnce() + Send>;

    /// Display group: one per desktop compositor backend. Each gamescope spawn
    /// is its own group — never auto-rowed or restore-grouped with another.
    pub(super) fn group_key(backend: &str, generation: u64) -> String {
        if backend == "gamescope" {
            format!("gamescope#{generation}")
        } else {
            backend.to_string()
        }
    }

    /// Group membership for restore hand-off, first-in-group, and layout.
    ///
    /// `supersedes` is the display a mode switch is replacing (create-before-drop).
    /// It is still Active but leaving; counting it made the newcomer defer
    /// topology and auto-row one width to the right on every resize.
    ///
    /// Lifecycle is ignored: a lingering/pinned entry still occupies desktop
    /// space. Only first-in-group adds a liveness term (live sessions, not outputs).
    pub(super) fn in_group(
        e_backend: &str,
        e_gen: u64,
        backend: &str,
        generation: u64,
        supersedes: Option<u64>,
    ) -> bool {
        Some(e_gen) != supersedes && group_key(e_backend, e_gen) == group_key(backend, generation)
    }

    /// Move a departing display's topology restore onto a same-[group](in_group)
    /// sibling, or return it if the group is empty so the caller runs it
    /// before dropping the keepalive (the compositor must not see zero outputs).
    ///
    /// Both `backend` and `generation` identify the departing display: keyed on
    /// backend alone, a gamescope restore floated onto another client's spawn.
    pub(super) fn hand_off_restore(
        remaining: &mut [Entry],
        backend: &'static str,
        generation: u64,
        restore: Option<Restore>,
    ) -> Option<Restore> {
        let action = restore?;
        // At most one restore per group, so any surviving sibling has `None`.
        match remaining
            .iter_mut()
            .find(|e| in_group(e.backend, e.generation, backend, generation, None))
        {
            Some(sibling) => {
                sibling.topology_restore = Some(action);
                None
            }
            None => Some(action), // group empty: caller runs it now
        }
    }

    /// Whether this entry's session epoch still matches for reuse/expiry.
    /// Epoch tracks the desktop compositor; a stale-epoch kept output is a
    /// corpse. Gamescope is exempt: its node lives with its child, unrelated
    /// to the desktop compositor. Liveness is `kept_display_alive` + `mark_failed`.
    pub(super) fn epoch_matches(backend: &str, entry_epoch: u64, cur_epoch: u64) -> bool {
        backend == "gamescope" || entry_epoch == cur_epoch
    }

    /// Take entries past their linger deadline so the caller drops them after
    /// releasing the lock — keepalive `Drop` (Mutter D-Bus Stop) can block.
    /// Each restore is [handed off](hand_off_restore) or returned when the
    /// group empties (run before the entries drop).
    pub(super) fn take_expired(
        entries: &mut Vec<Entry>,
        now: Instant,
        cur_epoch: u64,
    ) -> (Vec<Entry>, Vec<Restore>) {
        let mut expired = Vec::new();
        let mut restores = Vec::new();
        // Also reap a kept (non-Active) desktop display whose epoch is stale —
        // the compositor was replaced, so the node id is a corpse. Gamescope is
        // exempt (`epoch_matches`). Active stays for its session's rebuild.
        let mut i = 0;
        while i < entries.len() {
            let dead_epoch = !epoch_matches(entries[i].backend, entries[i].epoch, cur_epoch)
                && !matches!(entries[i].life, lifecycle::State::Active { .. });
            if entries[i].life.poll_expiry(now) || dead_epoch {
                let mut e = entries.remove(i);
                let (backend, generation) = (e.backend, e.generation);
                if let Some(r) =
                    hand_off_restore(entries, backend, generation, e.topology_restore.take())
                {
                    restores.push(r);
                }
                expired.push(e);
            } else {
                i += 1;
            }
        }
        (expired, restores)
    }

    /// Linger applied on release. A deliberate quit (`force_immediate`) turns a
    /// linger window into Immediate. `Forever` outranks quit: the screen stays
    /// until `/display/release`.
    pub(super) fn effective_linger(force_immediate: bool, policy: Linger) -> Linger {
        match (force_immediate, policy) {
            (true, Linger::Forever) => Linger::Forever,
            (true, _) => Linger::Immediate,
            (false, l) => l,
        }
    }

    /// Flattened live/kept row so group/layout math runs outside the pool lock.
    pub(super) struct Row {
        pub(super) generation: u64,
        pub(super) backend: &'static str,
        pub(super) mode: Mode,
        pub(super) identity_slot: Option<u32>,
        pub(super) state: &'static str,
        pub(super) expires_in_ms: Option<u64>,
        pub(super) sessions: u32,
    }

    /// Desktop position for a display just appended to its group: existing
    /// members plus `new`, ordered by acquire `generation`, arranged by
    /// [`layout`](crate::layout). Pure so tests do not need the pool lock.
    pub(super) fn position_for_new(
        mut existing: Vec<(u64, crate::layout::Member)>,
        new: crate::layout::Member,
        layout_policy: &Layout,
    ) -> crate::layout::Placement {
        existing.sort_by_key(|(g, _)| *g);
        let mut members: Vec<crate::layout::Member> =
            existing.into_iter().map(|(_, m)| m).collect();
        members.push(new);
        *crate::layout::arrange(&members, layout_policy)
            .last()
            .expect("members is non-empty (just pushed `new`)")
    }

    /// Assign stable group ids: known keys keep their id, new ones take `next`,
    /// gone keys are dropped. Do not use sorted-list index — a `gamescope#N`
    /// key sorts ahead of `"kwin"` and would renumber the desktop. Prune live
    /// keys or per-spawn ids accumulate for the process lifetime.
    pub(super) fn assign_group_ids(
        known: &mut std::collections::BTreeMap<String, u32>,
        next: &mut u32,
        keys: &[String],
    ) {
        known.retain(|k, _| keys.iter().any(|live| live == k));
        for k in keys {
            if !known.contains_key(k) {
                known.insert(k.clone(), *next);
                *next += 1;
            }
        }
    }

    /// `/display/state` view: rows grouped by [`group_key`], ordered by acquire
    /// `generation`, positions from [`layout`](crate::layout). Pure for tests.
    pub(super) fn assemble_displays(
        rows: Vec<Row>,
        layout_policy: &Layout,
        topology: &str,
        ids: &std::collections::BTreeMap<String, u32>,
    ) -> Vec<DisplayInfo> {
        use crate::layout::{self, Member};

        let mut keys: Vec<String> = rows
            .iter()
            .map(|r| group_key(r.backend, r.generation))
            .collect();
        keys.sort();
        keys.dedup();

        let mut out: Vec<DisplayInfo> = Vec::new();
        for key in keys.iter() {
            let mut idx: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, row)| &group_key(row.backend, row.generation) == key)
                .map(|(i, _)| i)
                .collect();
            idx.sort_by_key(|&i| rows[i].generation);
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
                    slot: row.generation,
                    backend: row.backend.to_string(),
                    mode: (row.mode.width, row.mode.height, row.mode.refresh_hz),
                    state: row.state.to_string(),
                    expires_in_ms: row.expires_in_ms,
                    sessions: row.sessions,
                    client: None,
                    // 0 = ungrouped. The caller derives `ids` from these rows;
                    // panicking a mgmt read would be worse than a missing key.
                    group: ids.get(key).copied().unwrap_or(0),
                    display_index: ord as u32,
                    position: (p.x, p.y),
                    identity_slot: row.identity_slot,
                    topology: topology.to_string(),
                });
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::policy::{Layout, LayoutMode, Position};
        use std::collections::BTreeMap;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// Dummy keepalive; `hand_off_restore` only reads backend, generation, restore.
        fn test_entry(backend: &'static str, generation: u64, restore: Option<Restore>) -> Entry {
            Entry {
                life: lifecycle::State::default(),
                keepalive: Box::new(()),
                node_id: 0,
                preferred_mode: None,
                output_name: None,
                mode: Mode {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                },
                backend,
                identity_slot: None,
                topology_restore: restore,
                launch: None,
                isolation: None,
                epoch: 0,
                generation,
                hw_cursor: false,
                hdr: false,
            }
        }

        fn flag_restore(flag: &Arc<AtomicBool>) -> Restore {
            let f = flag.clone();
            Box::new(move || f.store(true, Ordering::SeqCst))
        }

        /// Snapshot-style group ids from an empty map so tests do not share state.
        fn ids_for(rows: &[Row]) -> BTreeMap<String, u32> {
            let mut known = BTreeMap::new();
            let mut next = 1;
            ids_into(&mut known, &mut next, rows);
            known
        }

        /// Like `ids_for` but against a carried map (stability across two assemblies).
        fn ids_into(known: &mut BTreeMap<String, u32>, next: &mut u32, rows: &[Row]) {
            let mut keys: Vec<String> = rows
                .iter()
                .map(|r| group_key(r.backend, r.generation))
                .collect();
            keys.sort();
            keys.dedup();
            assign_group_ids(known, next, &keys);
        }

        #[test]
        fn deliberate_quit_skips_the_linger_window_but_never_a_pin() {
            use std::time::Duration;
            assert_eq!(
                effective_linger(true, Linger::For(Duration::from_secs(10))),
                Linger::Immediate
            );
            assert_eq!(effective_linger(true, Linger::Immediate), Linger::Immediate);
            assert_eq!(effective_linger(true, Linger::Forever), Linger::Forever);
            assert_eq!(
                effective_linger(false, Linger::For(Duration::from_secs(10))),
                Linger::For(Duration::from_secs(10))
            );
            assert_eq!(effective_linger(false, Linger::Forever), Linger::Forever);
        }

        #[test]
        fn topology_restore_floats_to_a_sibling_then_runs_on_the_last_teardown() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![
                test_entry("kwin", 1, Some(flag_restore(&ran))),
                test_entry("kwin", 2, None),
            ];

            let mut e1 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take());
            assert!(out.is_none(), "transferred, not run");
            assert!(!ran.load(Ordering::SeqCst));
            assert!(pool[0].topology_restore.is_some());

            let mut e2 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take());
            let action = out.expect("group empty → run the restore");
            assert!(!ran.load(Ordering::SeqCst), "not run yet");
            action();
            assert!(ran.load(Ordering::SeqCst), "runs on the last drop");
        }

        #[test]
        fn single_session_topology_restore_runs_on_its_own_teardown() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("kwin", 1, Some(flag_restore(&ran)))];
            let mut e = pool.remove(0);
            let action = hand_off_restore(&mut pool, "kwin", 1, e.topology_restore.take())
                .expect("last (only) member → run");
            action();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn tearing_down_a_non_carrier_first_leaves_the_restore_for_last() {
            let ran = Arc::new(AtomicBool::new(false));
            // Gen 1 has no restore: a later exclusive session found physicals already off.
            let mut pool = vec![
                test_entry("kwin", 1, None),
                test_entry("kwin", 2, Some(flag_restore(&ran))),
            ];
            let mut e1 = pool.remove(0);
            assert!(hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take()).is_none());
            assert!(pool[0].topology_restore.is_some());
            let mut e2 = pool.remove(0);
            hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take())
                .expect("last member → run")();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn restore_never_floats_across_backends() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("mutter", 2, None)];
            let out = hand_off_restore(&mut pool, "kwin", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "no same-backend sibling → return to run");
            assert!(
                pool[0].topology_restore.is_none(),
                "restore must not cross into another backend's group"
            );
        }

        #[test]
        fn restore_never_floats_between_gamescope_spawns() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("gamescope", 2, None)];
            let out = hand_off_restore(&mut pool, "gamescope", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "another client's spawn is not a sibling");
            assert!(pool[0].topology_restore.is_none());
        }

        #[test]
        fn group_membership_splits_spawns_and_excludes_the_superseded() {
            assert!(in_group("kwin", 1, "kwin", 2, None));
            assert!(!in_group("mutter", 1, "kwin", 2, None));
            assert!(!in_group("gamescope", 1, "gamescope", 2, None));
            assert!(in_group("gamescope", 7, "gamescope", 7, None));
            assert!(!in_group("kwin", 1, "kwin", 2, Some(1)));
            assert!(in_group("kwin", 3, "kwin", 2, Some(1)));
        }

        fn row(generation: u64, backend: &'static str, w: u32, slot: Option<u32>) -> Row {
            Row {
                generation,
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
            // Acquired gen 5 then 2 (vec order is not acquire order) plus one Mutter.
            let rows = vec![
                row(5, "kwin", 2560, Some(1)),
                row(2, "kwin", 1920, Some(7)),
                row(9, "mutter", 3840, None),
            ];
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "exclusive", &ids);

            let kwin: Vec<&DisplayInfo> = out.iter().filter(|d| d.backend == "kwin").collect();
            assert_eq!(kwin.len(), 2);
            assert_eq!(kwin[0].slot, 2);
            assert_eq!(kwin[0].display_index, 0);
            assert_eq!(kwin[0].position, (0, 0));
            assert_eq!(kwin[1].slot, 5);
            assert_eq!(kwin[1].display_index, 1);
            assert_eq!(kwin[1].position, (1920, 0));
            assert_eq!(kwin[0].topology, "exclusive");

            let mutter = out.iter().find(|d| d.backend == "mutter").unwrap();
            assert_ne!(mutter.group, kwin[0].group);
            assert_eq!(mutter.display_index, 0);
            assert_eq!(mutter.position, (0, 0));
        }

        /// `gamescope#3` sorts before `"kwin"`; that must not renumber the desktop.
        #[test]
        fn a_new_group_never_renumbers_an_existing_one() {
            let mut known = BTreeMap::new();
            let mut next = 1;
            let desktop = vec![row(1, "kwin", 1920, None)];
            ids_into(&mut known, &mut next, &desktop);
            let before = assemble_displays(desktop, &Layout::default(), "extend", &known);
            let kwin_group = before[0].group;

            let both = vec![row(1, "kwin", 1920, None), row(3, "gamescope", 1280, None)];
            ids_into(&mut known, &mut next, &both);
            let after = assemble_displays(both, &Layout::default(), "extend", &known);
            let kwin_after = after.iter().find(|d| d.backend == "kwin").unwrap();
            let gs = after.iter().find(|d| d.backend == "gamescope").unwrap();
            assert_eq!(
                kwin_after.group, kwin_group,
                "the untouched desktop keeps its group id"
            );
            assert_ne!(gs.group, kwin_group);
        }

        #[test]
        fn position_for_new_appends_right_in_acquire_order() {
            use crate::layout::{Member, Placement};
            let m = |slot, w| Member {
                identity_slot: slot,
                width: w,
            };
            // Gen 8 @ 1920 acquired after gen 3 @ 2560 (vec is not acquire order).
            let existing = vec![(8, m(Some(2), 1920)), (3, m(Some(1), 2560))];
            let pos = position_for_new(existing, m(Some(5), 1280), &Layout::default());
            assert_eq!(pos, Placement { x: 4480, y: 0 });
            // Origin so the registry skips apply_position.
            let first = position_for_new(vec![], m(None, 3840), &Layout::default());
            assert_eq!(first, Placement { x: 0, y: 0 });
        }

        #[test]
        fn position_for_new_honors_a_manual_pin() {
            use crate::layout::{Member, Placement};
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
            let rows = vec![
                row(1, "gamescope", 1920, None),
                row(2, "gamescope", 1280, None),
            ];
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "extend", &ids);
            assert_eq!(out.len(), 2);
            assert_ne!(out[0].group, out[1].group, "distinct groups");
            assert_eq!(out[0].display_index, 0);
            assert_eq!(out[1].display_index, 0);
            assert_eq!(out[0].position, (0, 0));
            assert_eq!(out[1].position, (0, 0));
        }

        #[test]
        fn manual_layout_keys_positions_by_identity_slot() {
            // Slot 7 left of slot 1 — reversed vs auto-row.
            let rows = vec![row(1, "kwin", 2560, Some(1)), row(2, "kwin", 1920, Some(7))];
            let mut positions = BTreeMap::new();
            positions.insert("1".to_string(), Position { x: 1920, y: 0 });
            positions.insert("7".to_string(), Position { x: 0, y: 0 });
            let layout = Layout {
                mode: LayoutMode::Manual,
                positions,
            };
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &layout, "extend", &ids);
            let by_slot = |s: u32| out.iter().find(|d| d.identity_slot == Some(s)).unwrap();
            assert_eq!(by_slot(1).position, (1920, 0));
            assert_eq!(by_slot(7).position, (0, 0));
        }

        /// Expiry is pure over `(entries, now, epoch)` so stale-epoch and Active exemption pin here.
        #[test]
        fn expiry_reaps_deadlines_and_stale_epoch_corpses_but_never_an_active_entry() {
            use std::time::Duration;
            let t0 = Instant::now();
            let mut es = Vec::new();
            let mut e1 = test_entry("kwin", 1, None);
            e1.life = lifecycle::State::Lingering {
                until: t0 - Duration::from_millis(1),
            };
            es.push(e1);
            let mut e2 = test_entry("kwin", 2, None);
            e2.life = lifecycle::State::Lingering {
                until: t0 + Duration::from_secs(60),
            };
            e2.epoch = 5;
            es.push(e2);
            let mut e3 = test_entry("kwin", 3, None);
            e3.life = lifecycle::State::Pinned;
            e3.epoch = 4;
            es.push(e3);
            let mut e4 = test_entry("kwin", 4, None);
            e4.life = lifecycle::State::Active { refs: 1 };
            e4.epoch = 4;
            es.push(e4);
            let mut e5 = test_entry("gamescope", 5, None);
            e5.life = lifecycle::State::Pinned;
            e5.epoch = 1;
            es.push(e5);

            let (expired, restores) = take_expired(&mut es, t0, 5);
            assert!(restores.is_empty());
            let gone: Vec<u64> = expired.iter().map(|e| e.generation).collect();
            assert_eq!(gone, vec![1, 3]);
            let left: Vec<u64> = es.iter().map(|e| e.generation).collect();
            assert_eq!(left, vec![2, 4, 5]);
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use anyhow::Result;

    use super::pool::{
        assemble_displays, assign_group_ids, effective_linger, epoch_matches, group_key,
        hand_off_restore, in_group, position_for_new, take_expired, Entry, Restore, Row,
    };
    use super::DisplayInfo;
    use crate::lifecycle::{self, Release};
    use crate::policy::{self, Layout, Linger};
    use crate::{Mode, VirtualDisplay, VirtualOutput};

    enum ReuseOutcome {
        Reused(VirtualOutput),
        /// Dead kept display, already removed, plus its group restore (run
        /// before the keepalive drops). Caller creates fresh.
        Dead(Entry, Option<Restore>),
        Miss,
    }

    struct Reg {
        entries: Mutex<Vec<Entry>>,
        generation: AtomicU64,
    }

    static REG: OnceLock<Reg> = OnceLock::new();

    fn reg() -> &'static Reg {
        REG.get_or_init(|| Reg {
            entries: Mutex::new(Vec::new()),
            generation: AtomicU64::new(1),
        })
    }

    /// Identity slots currently in the pool. Takes the pool lock — never call
    /// while holding it.
    pub(super) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
        let Some(r) = REG.get() else {
            return Default::default();
        };
        let es = r.entries.lock().unwrap();
        es.iter().filter_map(|e| e.identity_slot).collect()
    }

    /// Pool size (live + kept) for admission. `0` before the first acquire
    /// (registry not initialised).
    pub(super) fn live_display_count() -> u32 {
        REG.get()
            .map(|r| r.entries.lock().unwrap().len() as u32)
            .unwrap_or(0)
    }

    /// Linger from console `keep_alive`, else Immediate (disconnect tears down now).
    fn linger() -> Linger {
        policy::prefs()
            .configured_effective()
            .map(|e| e.keep_alive.linger())
            .unwrap_or(Linger::Immediate)
    }

    /// Do not wrap `spawn` in `Once` and discard `Result`: a failed spawn
    /// (EAGAIN / RLIMIT_NPROC) would consume the Once and leave kept displays
    /// unreaped for the process lifetime. Set the flag only on success; the
    /// mutex makes check-and-spawn atomic.
    fn ensure_timer() {
        static STARTED: Mutex<bool> = Mutex::new(false);
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        match std::thread::Builder::new()
            .name("vdisplay-linger".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_millis(500));
                    let (expired, restores) = {
                        let mut es = reg().entries.lock().unwrap();
                        take_expired(&mut es, Instant::now(), crate::session_epoch())
                    };
                    // Restore physicals (group emptied) before dropping outputs, outside the lock.
                    for restore in restores {
                        restore();
                    }
                    let reaped = expired.len();
                    for e in expired {
                        tracing::info!(
                            backend = e.backend,
                            "virtual display: linger expired — torn down"
                        );
                        drop(e); // outside the lock
                    }
                    emit_released(reaped);
                }
            }) {
            Ok(_) => *started = true,
            Err(e) => tracing::error!(
                error = %e,
                "virtual display: could not start the keep-alive linger reaper — kept displays \
                 will not expire until a later session retries"
            ),
        }
    }

    /// Emit `Released` for `n` displays. Every Linux teardown path goes through
    /// here so linger expiry and compositor-gone invalidation leave the console.
    fn emit_released(n: usize) {
        if n > 0 {
            crate::emit_display_event(crate::DisplayEvent::Released { count: n as u32 });
        }
    }

    /// Session-facing output: kept node + generation-stamped lease. Only
    /// poolable (`remote_fd == None`) backends reach here, so `remote_fd` is None.
    fn output_for(
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        output_name: Option<String>,
        generation: u64,
        quit: Arc<AtomicBool>,
        reused: bool,
    ) -> VirtualOutput {
        let mut out = VirtualOutput::owned(
            node_id,
            preferred_mode,
            Box::new(DisplayLease { generation, quit }),
        );
        // Same head as at create, so it answers with the same name.
        out.output_name = output_name;
        // First-frame failure on reuse can `mark_failed` instead of re-wedging.
        out.reused_gen = reused.then_some(generation);
        // Mode-switch rebuild `retire`s the entry this output's successor supersedes.
        out.pool_gen = Some(generation);
        out
    }

    pub(super) fn acquire(
        vd: &mut Box<dyn VirtualDisplay>,
        mode: Mode,
        quit: Arc<AtomicBool>,
        supersedes: Option<u64>,
    ) -> Result<VirtualOutput> {
        ensure_timer();
        let backend = vd.name();
        // Reuse keys: launch/isolation must match (game A never serves B); epoch is current.
        let launch = vd.launch_command();
        let isolation = vd.isolation_key();
        let cur_epoch = crate::session_epoch();
        let r = reg();

        let (expired, restores) = {
            let mut es = r.entries.lock().unwrap();
            take_expired(&mut es, Instant::now(), cur_epoch)
        };
        for restore in restores {
            restore();
        }
        let reaped = expired.len();
        drop(expired);
        emit_released(reaped);

        // Gated on `poolable_now()`: gamescope managed/attach shares the
        // `"gamescope"` name with a bare spawn and must not reuse it.
        if vd.poolable_now() {
            // Probe `kept_display_alive` (may shell `pw-dump`) outside the lock:
            // snapshot (generation, node_id), probe, re-find by generation.
            // A concurrent reuse/remove just misses and creates fresh.
            let candidate = {
                let es = r.entries.lock().unwrap();
                es.iter()
                    .find(|e| {
                        matches!(
                            e.life,
                            lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
                        ) && e.backend == backend
                            && e.mode == mode
                            && e.launch == launch
                            && e.isolation == isolation
                            && e.hw_cursor == vd.hw_cursor()
                            && e.hdr == vd.hdr()
                            && epoch_matches(e.backend, e.epoch, cur_epoch)
                    })
                    .map(|e| (e.generation, e.node_id))
            };
            if let Some((cand_gen, node_id)) = candidate {
                let alive = vd.kept_display_alive(node_id); // OUTSIDE the lock (may block)
                let reuse = {
                    let mut es = r.entries.lock().unwrap();
                    match es.iter().position(|e| {
                        e.generation == cand_gen
                            && matches!(
                                e.life,
                                lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
                            )
                    }) {
                        Some(idx) if alive => {
                            es[idx].life.acquire();
                            let generation = r.generation.fetch_add(1, Ordering::Relaxed);
                            es[idx].generation = generation;
                            let preferred_mode = es[idx].preferred_mode;
                            let output_name = es[idx].output_name.clone();
                            tracing::info!(
                                backend,
                                node_id,
                                "virtual display reused (keep-alive reconnect)"
                            );
                            ReuseOutcome::Reused(output_for(
                                node_id,
                                preferred_mode,
                                output_name,
                                generation,
                                quit.clone(),
                                true,
                            ))
                        }
                        Some(idx) => {
                            let mut dead = es.remove(idx);
                            let (b, g) = (dead.backend, dead.generation);
                            let restore =
                                hand_off_restore(&mut es, b, g, dead.topology_restore.take());
                            ReuseOutcome::Dead(dead, restore)
                        }
                        None => ReuseOutcome::Miss, // adopted/removed by another thread
                    }
                };
                match reuse {
                    ReuseOutcome::Reused(out) => return Ok(out),
                    ReuseOutcome::Dead(dead, restore) => {
                        // Outside the lock: restore physicals, then drop keepalive (may block).
                        if let Some(rst) = restore {
                            rst();
                        }
                        tracing::info!(
                            backend,
                            "virtual display: kept display was dead — recreating (validated reuse)"
                        );
                        drop(dead);
                        emit_released(1);
                    }
                    ReuseOutcome::Miss => {}
                }
            }
        }

        // Stamp generation before group questions: a gamescope spawn's group
        // IS its generation. A burned stamp on failed create is fine (opaque,
        // monotonic, never an index).
        let generation = r.generation.fetch_add(1, Ordering::Relaxed);

        // Do not enforce `max_displays` here: `admit` runs once per session;
        // `acquire` reruns on rebuild and the old lease still counts (max=1
        // never recovers capture-loss). First-in-group excludes `supersedes`
        // (still Active); kept leftovers have no session to clobber.
        let first_in_group = {
            let es = r.entries.lock().unwrap();
            !es.iter().any(|e| {
                in_group(e.backend, e.generation, backend, generation, supersedes)
                    && matches!(e.life, lifecycle::State::Active { .. })
            })
        };
        vd.set_first_in_group(first_in_group);

        // Not under the lock: `vd.create` blocks and spawns threads.
        let real = vd.create(mode)?;
        let identity_slot = vd.last_identity_slot();

        // Pool only `Owned` on the default PipeWire daemon. Pass through
        // `External`/`SessionManaged` (gamescope owns those; pooling wedges on
        // a stale node) and `remote_fd = Some` (wlroots portal fd cannot reopen).
        if real.ownership != crate::DisplayOwnership::Owned || real.remote_fd.is_some() {
            tracing::debug!(
                backend,
                ownership = ?real.ownership,
                "virtual display not registry-poolable — keep-alive off (owner keeps it / portal fd)"
            );
            return Ok(real);
        }

        let node_id = real.node_id;
        let preferred_mode = real.preferred_mode;
        let output_name = real.output_name.clone();
        // Fresh create may start at a sacrificial mode (KWin >60 Hz) that must
        // renegotiate before frames count. Reuse already did; leave the flag off.
        let expect_exact_dims = real.expect_exact_dims;
        // Group restore: run once when the last member drops, not at this
        // session's teardown. `None` for non-exclusive / non-first / auto-revert.
        let topology_restore = vd.take_topology_restore();
        let mut life = lifecycle::State::default();
        life.acquire(); // Idle → Active{refs:1} (Acquire::Create)
        let entry = Entry {
            life,
            keepalive: real.keepalive,
            node_id,
            preferred_mode,
            output_name: output_name.clone(),
            mode,
            backend,
            identity_slot,
            topology_restore,
            launch: launch.clone(),
            isolation: isolation.clone(),
            epoch: cur_epoch,
            generation,
            hw_cursor: vd.hw_cursor(),
            hdr: vd.hdr(),
        };

        // Position then push under the same lock (I/O-free). Apply is below,
        // outside the lock.
        let position = {
            use crate::layout::Member;
            let layout_policy = policy::prefs()
                .configured_effective()
                .map(|e| e.layout)
                .unwrap_or_default();
            let mut es = r.entries.lock().unwrap();
            // Same-group, excluding `supersedes` — else a resize auto-rows past
            // the predecessor and walks one width right on every mode switch.
            let existing: Vec<(u64, Member)> = es
                .iter()
                .filter(|e| in_group(e.backend, e.generation, backend, generation, supersedes))
                .map(|e| {
                    (
                        e.generation,
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
        // Apply position outside the lock (kscreen blocks). Skip (0, 0): that
        // is the compositor default, so first-of-group and non-KWin (no-op
        // `apply_position`) issue no positioning.
        if (position.x, position.y) != (0, 0) {
            vd.apply_position(position.x, position.y);
        }
        let mut out = output_for(
            node_id,
            preferred_mode,
            output_name,
            generation,
            quit,
            false,
        );
        out.expect_exact_dims = expect_exact_dims;
        Ok(out)
    }

    /// [`DisplayLease`] drop: lifecycle decides linger / pin / teardown.
    /// Torn-down keepalive drops after the lock is released.
    fn release(generation: u64, force_immediate: bool) {
        let Some(r) = REG.get() else { return };
        let linger = effective_linger(force_immediate, linger());
        let (torn_down, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.generation == generation) else {
                return; // stale lease (entry reused + re-stamped, or already gone) — no-op
            };
            match es[idx].life.release(Instant::now(), linger) {
                Release::Teardown => {
                    let mut e = es.remove(idx);
                    let (backend, g) = (e.backend, e.generation);
                    let restore = hand_off_restore(&mut es, backend, g, e.topology_restore.take());
                    (Some(e), restore)
                }
                // No live hold: do nothing. Do not treat this as Teardown — a
                // stale/duplicate drop would kill the display. Unreachable
                // (lookup is by unique generation) but must match Windows.
                Release::Noop => (None, None),
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
        // Restore physicals (group emptied) before dropping the output, outside the lock.
        if let Some(restore) = restore {
            restore();
        }
        if let Some(e) = torn_down {
            if force_immediate {
                tracing::info!(
                    backend = e.backend,
                    "virtual display torn down (deliberate quit — keep-alive skipped)"
                );
            } else {
                tracing::info!(
                    backend = e.backend,
                    "virtual display torn down (keep-alive off / released)"
                );
            }
            drop(e); // outside the lock — the keepalive Drop may block
            emit_released(1);
        }
    }

    pub(super) fn snapshot() -> Vec<DisplayInfo> {
        let Some(r) = REG.get() else {
            return Vec::new();
        };
        let now = Instant::now();

        // Flatten under the lock. Skip Idle — never stored, but the match is exhaustive.
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
                        generation: e.generation,
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
        let layout_policy: Layout = policy::prefs()
            .configured_effective()
            .map(|e| e.layout)
            .unwrap_or_default();
        // Process-lifetime group ids. Lives here, not in the pure core: a new
        // group must never renumber an existing one under the console.
        let mut keys: Vec<String> = rows
            .iter()
            .map(|r| group_key(r.backend, r.generation))
            .collect();
        keys.sort();
        keys.dedup();
        static GROUP_IDS: Mutex<Option<(std::collections::BTreeMap<String, u32>, u32)>> =
            Mutex::new(None);
        let ids = {
            let mut g = GROUP_IDS.lock().unwrap_or_else(|e| e.into_inner());
            let (known, next) = g.get_or_insert_with(|| (Default::default(), 1));
            assign_group_ids(known, next, &keys);
            known.clone()
        };

        assemble_displays(rows, &layout_policy, &topology, &ids)
    }

    pub(super) fn force_release(slot: Option<u64>) -> usize {
        release_kept(slot, "released (mgmt /display/release)")
    }

    /// Force-release a display superseded by a mid-stream mode switch. Same as
    /// [`force_release`] (kept only; Active refused; already-gone no-op); distinct log.
    pub(super) fn retire(generation: u64) {
        release_kept(Some(generation), "retired (superseded by a mode switch)");
    }

    /// Tear down kept (lingering/pinned) entries — all, or one by generation —
    /// with keepalive drops outside the lock. Shared by [`force_release`] and [`retire`].
    fn release_kept(slot: Option<u64>, why: &'static str) -> usize {
        let Some(r) = REG.get() else { return 0 };
        let (released, restores) = {
            let mut es = r.entries.lock().unwrap();
            let mut out = Vec::new();
            let mut restores = Vec::new();
            let mut i = 0;
            while i < es.len() {
                let selected = slot.is_none_or(|s| es[i].generation == s);
                if selected && es[i].life.force_release() {
                    let mut e = es.remove(i);
                    let (backend, g) = (e.backend, e.generation);
                    let restore = e.topology_restore.take();
                    if let Some(rst) = hand_off_restore(&mut es, backend, g, restore) {
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
        // Restore physicals (group emptied) before dropping outputs, outside the lock.
        for restore in restores {
            restore();
        }
        for e in released {
            tracing::info!(backend = e.backend, "virtual display {why}");
            drop(e);
        }
        emit_released(n);
        n
    }

    /// Tear down a reused-but-dead pool entry by generation. Drops keepalive
    /// outside the lock. Idempotent (already gone → no-op).
    pub(super) fn mark_failed(generation: u64) {
        let Some(r) = REG.get() else { return };
        let (torn, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.generation == generation) else {
                return; // already gone — the subsequent stale-generation lease drop no-ops too
            };
            let mut e = es.remove(idx);
            let (backend, g) = (e.backend, e.generation);
            let restore = hand_off_restore(&mut es, backend, g, e.topology_restore.take());
            (e, restore)
        };
        if let Some(rst) = restore {
            rst(); // outside the lock, before the keepalive drops
        }
        tracing::warn!(
            backend = torn.backend,
            "virtual display: reused kept display was dead on first frame — torn down (A2 mark_failed)"
        );
        drop(torn); // keepalive Drop outside the lock (may block)
        emit_released(1);
    }

    /// Invalidate every display of `backend` (compositor gone). Any lifecycle,
    /// including Active — those sessions rebuild. Drops keepalives outside the
    /// lock (dead sockets fail fast). Selects by backend, not slot/state.
    pub(super) fn invalidate_backend(backend: &str) {
        let Some(r) = REG.get() else { return };
        let (removed, restores) = {
            let mut es = r.entries.lock().unwrap();
            let mut out = Vec::new();
            let mut restores = Vec::new();
            let mut i = 0;
            while i < es.len() {
                if es[i].backend == backend {
                    let mut e = es.remove(i);
                    let (b, g) = (e.backend, e.generation);
                    if let Some(rst) = hand_off_restore(&mut es, b, g, e.topology_restore.take()) {
                        restores.push(rst);
                    }
                    out.push(e);
                } else {
                    i += 1;
                }
            }
            (out, restores)
        };
        if removed.is_empty() {
            return;
        }
        for restore in restores {
            restore();
        }
        tracing::info!(
            backend,
            count = removed.len(),
            "virtual displays invalidated — compositor instance gone (A4 session switch)"
        );
        let n = removed.len();
        for e in removed {
            drop(e); // outside the lock
        }
        emit_released(n);
    }

    /// Session keepalive. Drop releases the registry hold; a stale lease
    /// (reused + re-stamped, or torn down) is a no-op.
    struct DisplayLease {
        generation: u64,
        /// Deliberate stop, not a network drop. Drop tears down immediately
        /// when set; false on a bare disconnect → normal linger.
        quit: Arc<AtomicBool>,
    }

    impl Drop for DisplayLease {
        fn drop(&mut self) {
            release(self.generation, self.quit.load(Ordering::SeqCst));
        }
    }
}
