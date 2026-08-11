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
/// `quit` is the session's deliberate-quit flag: when the session ends with it set (the client closed
/// with the quit application code — a user "stop", not a network drop), the display is torn down
/// **immediately**, skipping the keep-alive linger. A bare disconnect leaves it `false` → normal linger.
///
/// `supersedes`: the pool gen of a display this acquire REPLACES (a mid-stream mode switch creates
/// the new display before retiring the old — create-before-drop). The replacement inherits group
/// topology ownership: without this, the dying predecessor counts as a live sibling and the new
/// display "extends" behind it, losing a Primary/Exclusive topology on every resize. `None`
/// everywhere else.
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
        // Windows leases in the manager (its own linger); its deliberate-quit skip is wired through
        // `VirtualDisplay::set_quit_flag` on the backend instance (set by the session before any
        // `create`, so the retry-hold lease gets it too) — not through this parameter. The
        // supersede handoff is Linux-pool-only too (the manager resizes in place).
        let _ = (quit, supersedes);
        vd.create(mode)
    };
    // A `Created` event means a display came into EXISTENCE. A keep-alive reuse does not: the
    // console was already told about that display when it was created (and at the mode it was
    // created with), so emitting again double-counted it — and, since the pool emits exactly one
    // `Released` when it finally goes away, left a phantom display in the console's live view for
    // the rest of the host's life. `reused_gen` is the pool's own answer to "did this already
    // exist" — and Linux is the only backend that HAS one.
    //
    // The non-Linux arm is therefore not a claim that nothing was reused, it is the absence of a
    // signal, and on Windows that gap is live: the manager keeps `Lingering`/`Pinned` monitors and
    // has an explicit JOIN branch that refcounts an already-`Active` one (`windows/manager.rs`,
    // "Active falls through to the JOIN path below (refcount++, no ADD)"), so `vd.create` returns
    // `Ok` for a monitor that already existed and the console is told `Created` again — the very
    // double-count this fixed for Linux. Its mirror image is the linger expiry, which emits no
    // `Released` at all (see the note in `release`). Closing it needs the manager to report its
    // acquire outcome, not another `cfg` here; that is the work, and until it is done this arm
    // over-reports rather than being right by construction.
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

/// Snapshot the host's managed virtual displays. Cheap + side-effect-free (a state-lock read);
/// safe per management request.
pub fn snapshot() -> Snapshot {
    #[cfg(target_os = "windows")]
    {
        // Windows slots (Stage W1): one group — the shared desktop — with the manager's slot list in
        // acquire order (`display_index`), each at its group-layout position. `identity_slot` is the
        // slot key (`None` for the anonymous slot 0).
        let displays = super::manager::snapshot()
            .into_iter()
            .enumerate()
            .map(|(idx, i)| DisplayInfo {
                slot: i.gen,
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

/// Force-release kept (lingering/pinned) displays now — the `/display/release` endpoint. `slot`
/// selects one by [`DisplayInfo::slot`]; `None` releases every kept display. Active displays are
/// refused (releasing a display with live sessions is session management). Returns the number
/// released.
pub fn release(slot: Option<u64>) -> usize {
    #[cfg(target_os = "windows")]
    // Windows slots (Stage W1): `slot` selects one kept monitor by its gen stamp
    // ([`DisplayInfo::slot`]); `None` releases every kept one.
    let released = super::manager::force_release(slot);
    #[cfg(target_os = "linux")]
    let released = linux::force_release(slot);
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let released = {
        let _ = slot;
        0
    };
    // Linux emits from every teardown site the pool owns (lease drop, linger expiry, the A2 dead-
    // display teardown, this mgmt release, the A4 invalidation), so emitting here as well would
    // double-count the one path it already covers. The Windows manager has no such hook — this
    // endpoint is the only teardown the console can learn about — so it emits here.
    #[cfg(not(target_os = "linux"))]
    if released > 0 {
        crate::emit_display_event(crate::DisplayEvent::Released {
            count: released as u32,
        });
    }
    released
}

/// Tear down a **reused-but-dead** pool entry by its generation stamp (A2). Called by the pipeline
/// builder when the first frame fails on a display [`acquire`] handed back as REUSED — so the retry
/// loop's next `acquire` creates fresh instead of re-wedging on the same corpse. No-op off Linux / if
/// the entry is already gone (idempotent — the subsequent stale-gen lease drop no-ops too).
pub fn mark_failed(gen: u64) {
    #[cfg(target_os = "linux")]
    linux::mark_failed(gen);
    #[cfg(not(target_os = "linux"))]
    let _ = gen;
}

/// Force-release a **superseded** kept display by its generation stamp
/// (`design/midstream-resolution-resize.md` H4): after a mid-stream mode-switch rebuild, the old
/// display's lease drop is indistinguishable from a disconnect, so under a `linger`/`forever`
/// keep-alive policy every resize would accumulate kept monitors at stale modes. The mode-switch
/// arm calls this once the new pipeline is up and the old capturer is dropped. Only a KEPT
/// (lingering/pinned) entry is released — an Active one is refused, like `/display/release` — and
/// a gen that's already gone (immediate teardown) is a no-op. No-op off Linux (Windows
/// reconfigures the same monitor in place — nothing is superseded).
pub fn retire(gen: u64) {
    #[cfg(target_os = "linux")]
    linux::retire(gen);
    #[cfg(not(target_os = "linux"))]
    let _ = gen;
}

/// Invalidate every kept display of `backend` — its compositor instance is gone (a Game↔Desktop switch
/// tore it down), so `/display/state` must stop listing it and its keepalive must be reaped
/// (`design/gamemode-and-dedicated-sessions.md` A4). Called from the session-switch watcher / a
/// per-connect re-detect that finds the previous backend's compositor gone. No-op off Linux.
pub fn invalidate_backend(backend: &str) {
    #[cfg(target_os = "linux")]
    linux::invalidate_backend(backend);
    #[cfg(not(target_os = "linux"))]
    let _ = backend;
}

/// The identity slots currently driving a pooled display — the eviction guard the identity map
/// consults before it hands a newly-arriving client an id (see
/// [`identity::live_slot_ids`](crate::identity)). KEPT (lingering/pinned) entries count: their
/// compositor output still exists and their owner is expected back. Linux-only — Windows reads the
/// manager's slot map instead, and no other platform pools displays.
#[cfg(target_os = "linux")]
pub(crate) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
    linux::live_identity_slots()
}

/// How many displays the Linux pool is holding — live AND kept — for [`admission`](crate::admission)'s
/// `max_displays` ceiling, which is the Linux counterpart of the Windows manager's slot count.
///
/// Deliberately counts kept/lingering entries too: a kept display still owns a real compositor
/// output on the desktop, so it consumes the operator's budget exactly like a streaming one, and
/// this is the same rule `manager::snapshot().len()` applies on Windows.
#[cfg(target_os = "linux")]
pub(crate) fn live_display_count() -> u32 {
    linux::live_display_count()
}

// ---------------------------------------------------------------------------------------------
// Pool core — the platform-NEUTRAL half of the Linux keep-alive pool
// ---------------------------------------------------------------------------------------------

/// The pool's pure machinery: the entry record, the group/reuse/budget predicates, the expiry sweep
/// and the `/display/state` assembly. None of it touches an OS API or a `cfg`-gated type — only
/// `mod linux` (which owns the global pool, the backend calls and the `VirtualOutput`'s Linux-only
/// fields) does. Split out so the rules the sweep findings live in are unit-testable on any host
/// this crate builds on, instead of only on the one CI leg that compiles `mod linux`.
///
/// Off Linux nothing calls it, hence the conditional `dead_code` allowance — kept conditional so a
/// genuinely dead helper is still reported on the platform that runs this code.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod pool {
    use std::time::Instant;

    use super::DisplayInfo;
    use crate::lifecycle;
    use crate::policy::{Layout, Linger};
    use crate::Mode;

    /// One pooled display: the lifecycle state + the backend's REAL keepalive (kept alive here so the
    /// compositor output — and thus its PipeWire `node_id` — survives past the session), plus the
    /// capture coordinates a reconnecting session needs.
    pub(super) struct Entry {
        pub(super) life: lifecycle::State,
        /// The backend's keepalive (KWin Wayland conn / Mutter D-Bus session / gamescope child). Its
        /// `Drop` releases the compositor output — so it is dropped only on teardown/expiry.
        // NEVER READ, ON PURPOSE — `dead_code` is right that nothing loads this field and wrong
        // about what that means. It is an RAII guard: holding it is the whole behaviour, and its
        // `Drop` is what releases the compositor output. Deleting it as "unused" would release
        // every pooled display the instant it was created.
        #[allow(dead_code)]
        pub(super) keepalive: Box<dyn Send>,
        pub(super) node_id: u32,
        pub(super) preferred_mode: Option<(u32, u32, u32)>,
        pub(super) mode: Mode,
        pub(super) backend: &'static str,
        /// The identity slot the backend resolved for this display (KWin per-slot naming; `None` for
        /// shared/anonymous or a backend with no per-client identity) — keys the group arrangement +
        /// the `/display/state` slot. Captured at create; kept across a keep-alive reuse.
        pub(super) identity_slot: Option<u32>,
        /// The topology-restore action for this display's GROUP (design §6.1): re-enable the physical
        /// outputs an `exclusive` topology disabled. At most ONE entry per group carries it (the first
        /// exclusive session); on teardown it hands off to a surviving sibling, and only runs when the
        /// group's last member drops. `None` for extend/primary and non-first / non-exclusive members.
        pub(super) topology_restore: Option<Restore>,
        /// The launch command this display was created with (`design/gamemode-and-dedicated-sessions.md`
        /// A2): keep-alive reuse requires an exact match, so a kept spawn running game A never serves a
        /// session launching game B. `None` = a plain desktop / no nested command.
        pub(super) launch: Option<String>,
        /// The session epoch at creation (A4). Reuse requires an epoch match; the linger timer reaps
        /// entries whose epoch is stale (their compositor instance was replaced under them).
        pub(super) epoch: u64,
        /// Generation stamp: a `DisplayLease` only releases if its gen still matches (a stale lease
        /// — its entry was reused + re-stamped — is a no-op).
        pub(super) gen: u64,
        /// The out-of-band-cursor mode this display was CREATED with (Phase B): metadata-pointer
        /// (cursor-channel session) vs compositor-embedded. Reuse requires an exact match — a kept
        /// embedded display has no cursor metadata for a channel session to forward, and a kept
        /// metadata display would leave a channel-less session with no pointer in its frames.
        pub(super) hw_cursor: bool,
        /// The stream colourimetry this display was BROUGHT UP for: 10-bit BT.2020/PQ (HDR) vs
        /// 8-bit SDR. Reuse requires an exact match — a kept SDR gamescope was spawned without
        /// `--hdr-enabled`, so an HDR session reusing it would get a game with no HDR surfaces
        /// under a stream that negotiated PQ over an SDR composite (wrong, and not obviously
        /// broken); the reverse would try to negotiate 8-bit off a PQ composite.
        pub(super) hdr: bool,
    }

    /// A per-group topology-restore action (see [`Entry::topology_restore`]).
    pub(super) type Restore = Box<dyn FnOnce() + Send>;

    /// The display **group** a backend+display belongs to (design §6.1). The desktop compositors
    /// (KWin/Mutter/wlroots) put every managed output on ONE desktop → one group per backend. A
    /// gamescope **spawn** is an independent nested session per client (no shared desktop), so each
    /// gamescope display is its OWN group — never auto-rowed against, or topology-/restore-grouped with,
    /// another gamescope session.
    pub(super) fn group_key(backend: &str, gen: u64) -> String {
        if backend == "gamescope" {
            format!("gamescope#{gen}")
        } else {
            backend.to_string()
        }
    }

    /// Is the pooled entry `(e_backend, e_gen)` a member of the group the display `(backend, gen)`
    /// belongs to — the ONE definition of membership, shared by the restore hand-off, the
    /// first-in-group probe and the layout collection.
    ///
    /// `supersedes` names the display a mid-stream mode switch is REPLACING (create-before-drop): it
    /// is still in the pool, and still Active, but it is leaving and its successor takes its place —
    /// so it is not a sibling of its own replacement. Counting it made the newcomer both defer to it
    /// for group topology and auto-row *past* it, walking the display one width to the right on every
    /// resize.
    ///
    /// Membership deliberately says nothing about lifecycle: a kept (Lingering/Pinned) entry still
    /// owns a real compositor output occupying real desktop space, so it counts for placement. Only
    /// the first-in-group probe adds a liveness term, because "may I establish this group's topology"
    /// is a question about live *sessions*, not about outputs.
    pub(super) fn in_group(
        e_backend: &str,
        e_gen: u64,
        backend: &str,
        gen: u64,
        supersedes: Option<u64>,
    ) -> bool {
        Some(e_gen) != supersedes && group_key(e_backend, e_gen) == group_key(backend, gen)
    }

    /// Hand off a torn-down display's topology restore (design §6.1 — per-group restore): if a
    /// same-[group](in_group) sibling survives in `remaining`, MOVE the restore onto it (a later teardown
    /// runs it); if the group is now empty, RETURN the action so the caller runs it (before dropping the
    /// reclaimed display's keepalive, so the physical is re-enabled while our output still exists —
    /// the compositor never sees zero outputs). `None` in → `None` out.
    ///
    /// `backend`+`gen` identify the DEPARTING display, and both are needed: keyed on the backend name
    /// alone, one gamescope spawn's restore floated onto an unrelated client's spawn — where it would
    /// run when THAT session ended and never when its own did.
    pub(super) fn hand_off_restore(
        remaining: &mut [Entry],
        backend: &'static str,
        gen: u64,
        restore: Option<Restore>,
    ) -> Option<Restore> {
        let action = restore?;
        // At most one restore per group, so any surviving sibling has `None` to receive it.
        match remaining
            .iter_mut()
            .find(|e| in_group(e.backend, e.gen, backend, gen, None))
        {
            Some(sibling) => {
                sibling.topology_restore = Some(action);
                None
            }
            None => Some(action), // group empty → run it now
        }
    }

    /// Does a pooled entry's session `epoch` still match the current one for reuse / expiry purposes?
    /// The session epoch tracks the box's **active-session (desktop) compositor** instance (KWin /
    /// Mutter / wlroots) — whose PipeWire node dies with the compositor, so a stale-epoch kept output
    /// is a corpse. A **gamescope** spawn is the exact opposite: an independent nested session (its own
    /// group), whose node lives with its own child process, wholly unrelated to whatever desktop /
    /// game-mode compositor the epoch tracks. So gamescope entries are EXEMPT from the epoch — a desktop
    /// switch, or a game-mode gamescope restart, must never invalidate a kept dedicated game session
    /// (review findings #2/#5/#6/#7/#10). Their liveness is the `kept_display_alive` node probe + the B2
    /// game-exit path + `mark_failed`, not the epoch.
    pub(super) fn epoch_matches(backend: &str, entry_epoch: u64, cur_epoch: u64) -> bool {
        backend == "gamescope" || entry_epoch == cur_epoch
    }

    /// Remove entries whose linger deadline has passed, returning them so the caller drops (tears
    /// them down) *after* releasing the lock — a backend keepalive `Drop` (Mutter D-Bus Stop) can
    /// block, and holding the pool lock across it would stall every other acquire/release. Each
    /// expired entry's topology restore is [handed off](hand_off_restore) to a surviving group sibling,
    /// or collected into the returned `restores` when its group empties (run before the entries drop).
    pub(super) fn take_expired(
        entries: &mut Vec<Entry>,
        now: Instant,
        cur_epoch: u64,
    ) -> (Vec<Entry>, Vec<Restore>) {
        let mut expired = Vec::new();
        let mut restores = Vec::new();
        // A4 backstop: also reap a KEPT (non-Active) DESKTOP display whose session epoch is stale — its
        // compositor instance was replaced (a Game↔Desktop switch / same-kind restart), so its node id
        // now means nothing. gamescope spawns are exempt (`epoch_matches` — independent nested sessions).
        // An Active entry is left to its own session's capture-loss rebuild (which, under the bumped
        // epoch, won't reuse it); `invalidate_backend` clears a whole desktop backend on a known switch.
        let mut i = 0;
        while i < entries.len() {
            let dead_epoch = !epoch_matches(entries[i].backend, entries[i].epoch, cur_epoch)
                && !matches!(entries[i].life, lifecycle::State::Active { .. });
            if entries[i].life.poll_expiry(now) || dead_epoch {
                let mut e = entries.remove(i);
                let (backend, gen) = (e.backend, e.gen);
                if let Some(r) = hand_off_restore(entries, backend, gen, e.topology_restore.take())
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

    /// The linger a releasing session actually gets. A deliberate quit (`force_immediate` — the
    /// client closed with the quit code, a user "stop") downgrades a linger WINDOW to an immediate
    /// teardown; a bare disconnect honors the policy. `keep_alive = forever` (the gaming-rig
    /// preset) OUTRANKS the quit: its promise is "the screen stays alive", so a deliberate quit
    /// still pins — only an explicit `/display/release` frees it.
    pub(super) fn effective_linger(force_immediate: bool, policy: Linger) -> Linger {
        match (force_immediate, policy) {
            (true, Linger::Forever) => Linger::Forever,
            (true, _) => Linger::Immediate,
            (false, l) => l,
        }
    }

    /// One live/kept display, flattened out of the pool under the lock — so the group + arrangement
    /// math (which calls the layout engine) runs OUTSIDE the lock.
    pub(super) struct Row {
        pub(super) gen: u64,
        pub(super) backend: &'static str,
        pub(super) mode: Mode,
        pub(super) identity_slot: Option<u32>,
        pub(super) state: &'static str,
        pub(super) expires_in_ms: Option<u64>,
        pub(super) sessions: u32,
    }

    /// The desktop position for a display just appended to its group (design §6.2): the group's
    /// `existing` members (each with its acquire `gen`) plus `new` last, ordered by `gen`, arranged by
    /// the pure [`layout`](crate::layout) engine, taking the new member's placement. Pure — so the
    /// append-in-acquire-order + auto-row/manual arrangement is unit-tested independent of the
    /// pool/global.
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

    /// Bring the group-key → **id** map up to date for the currently-live `keys`: every key already
    /// known keeps the id it had, every new one takes `next` (bumped), and a key whose group is gone
    /// is dropped. Pure over its `known`/`next` state — the caller owns the process-lifetime copy.
    ///
    /// Ids used to be the index into the sorted key list, which meant a new group could RENUMBER an
    /// untouched one: with one KWin desktop at group 1, a gamescope spawn at gen 3 sorts ahead of
    /// `"kwin"` and silently moved the unchanged desktop to group 2 on the next `/display/state` poll.
    /// A monotonic counter, remembered per key, cannot do that. Pruning to the live keys is what keeps
    /// the map bounded: the per-spawn keys (`gamescope#<gen>`, one per dedicated session) would
    /// otherwise accumulate for the host's lifetime — an id is retired with its group.
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

    /// Group the flattened rows into the mgmt `/display/state` view (design §6.1/§6.2) by
    /// [`group_key`], ordered by acquire (`gen`), with each member's position from the pure
    /// [`layout`](crate::layout) engine. `ids` maps each group key to its reported group id (see
    /// [`group_ids`]). Pure — no I/O, no global — so the grouping / ordering / position assignment is
    /// unit-tested against synthetic rows.
    pub(super) fn assemble_displays(
        rows: Vec<Row>,
        layout_policy: &Layout,
        topology: &str,
        ids: &std::collections::BTreeMap<String, u32>,
    ) -> Vec<DisplayInfo> {
        use crate::layout::{self, Member};

        let mut keys: Vec<String> = rows.iter().map(|r| group_key(r.backend, r.gen)).collect();
        keys.sort();
        keys.dedup();

        let mut out: Vec<DisplayInfo> = Vec::new();
        for key in keys.iter() {
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
                    // A key with no id can't happen (the caller derives `ids` from these same rows);
                    // 0 is the honest "ungrouped" answer rather than a panic in a mgmt read.
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

        /// A minimal pool entry for the pure teardown/restore tests (dummy keepalive; the
        /// `hand_off_restore` logic only reads `backend` + `gen` + `topology_restore`).
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
                launch: None,
                epoch: 0,
                gen,
                hw_cursor: false,
                hdr: false,
            }
        }

        /// A restore closure that flips `flag` when run — so a test can assert exactly WHEN it fires.
        fn flag_restore(flag: &Arc<AtomicBool>) -> Restore {
            let f = flag.clone();
            Box::new(move || f.store(true, Ordering::SeqCst))
        }

        /// Group ids for a set of rows, as `snapshot` derives them — starting from an EMPTY map, so
        /// the tests are independent of each other (the real caller threads one process-lifetime map).
        fn ids_for(rows: &[Row]) -> BTreeMap<String, u32> {
            let mut known = BTreeMap::new();
            let mut next = 1;
            ids_into(&mut known, &mut next, rows);
            known
        }

        /// `ids_for` against a CARRIED map — for the stability test, which needs the same state
        /// across two assemblies.
        fn ids_into(known: &mut BTreeMap<String, u32>, next: &mut u32, rows: &[Row]) {
            let mut keys: Vec<String> = rows.iter().map(|r| group_key(r.backend, r.gen)).collect();
            keys.sort();
            keys.dedup();
            assign_group_ids(known, next, &keys);
        }

        #[test]
        fn deliberate_quit_skips_the_linger_window_but_never_a_pin() {
            use std::time::Duration;
            // Quit downgrades a linger window (and a no-linger policy stays immediate)…
            assert_eq!(
                effective_linger(true, Linger::For(Duration::from_secs(10))),
                Linger::Immediate
            );
            assert_eq!(effective_linger(true, Linger::Immediate), Linger::Immediate);
            // …but never a pin: keep_alive=forever (gaming-rig) promises the screen stays alive.
            assert_eq!(effective_linger(true, Linger::Forever), Linger::Forever);
            // A bare disconnect honors the policy untouched.
            assert_eq!(
                effective_linger(false, Linger::For(Duration::from_secs(10))),
                Linger::For(Duration::from_secs(10))
            );
            assert_eq!(effective_linger(false, Linger::Forever), Linger::Forever);
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
            let out = hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take());
            assert!(out.is_none(), "transferred, not run");
            assert!(!ran.load(Ordering::SeqCst));
            // The restore floated onto the surviving sibling.
            assert!(pool[0].topology_restore.is_some());

            // Tear down the last member → group empty → the restore is returned to run.
            let mut e2 = pool.remove(0);
            let out = hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take());
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
            let action = hand_off_restore(&mut pool, "kwin", 1, e.topology_restore.take())
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
            assert!(hand_off_restore(&mut pool, "kwin", 1, e1.topology_restore.take()).is_none());
            // The carrier (gen 2) still holds the group's restore.
            assert!(pool[0].topology_restore.is_some());
            // Now the carrier (last member) → run.
            let mut e2 = pool.remove(0);
            hand_off_restore(&mut pool, "kwin", 2, e2.topology_restore.take())
                .expect("last member → run")();
            assert!(ran.load(Ordering::SeqCst));
        }

        #[test]
        fn restore_never_floats_across_backends() {
            // group = backend: a KWin restore must not land on a Mutter display (a different desktop).
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("mutter", 2, None)];
            let out = hand_off_restore(&mut pool, "kwin", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "no same-backend sibling → return to run");
            assert!(
                pool[0].topology_restore.is_none(),
                "restore must not cross into another backend's group"
            );
        }

        /// Each gamescope **spawn** is its own group (`group_key`), so a departing spawn's restore
        /// must not float onto another client's spawn — where it would run at THAT session's end and
        /// never at its own. Keyed on the backend name alone (the old rule), it did.
        #[test]
        fn restore_never_floats_between_gamescope_spawns() {
            let ran = Arc::new(AtomicBool::new(false));
            let mut pool = vec![test_entry("gamescope", 2, None)];
            let out = hand_off_restore(&mut pool, "gamescope", 1, Some(flag_restore(&ran)));
            assert!(out.is_some(), "another client's spawn is not a sibling");
            assert!(pool[0].topology_restore.is_none());
        }

        /// The one definition of group membership, exercised on the three cases the two old
        /// hand-rolled ones disagreed about.
        #[test]
        fn group_membership_splits_spawns_and_excludes_the_superseded() {
            // Same desktop backend → same group.
            assert!(in_group("kwin", 1, "kwin", 2, None));
            assert!(!in_group("mutter", 1, "kwin", 2, None));
            // Distinct gamescope spawns are distinct groups; a spawn is in its own.
            assert!(!in_group("gamescope", 1, "gamescope", 2, None));
            assert!(in_group("gamescope", 7, "gamescope", 7, None));
            // The display this acquire replaces is not a sibling of its replacement.
            assert!(!in_group("kwin", 1, "kwin", 2, Some(1)));
            assert!(in_group("kwin", 3, "kwin", 2, Some(1)));
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
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "exclusive", &ids);

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

        /// 10.7: a group id is a property of the GROUP, not of where its key happens to sort. A
        /// gamescope spawn (`gamescope#3` < `kwin`) must not renumber the untouched desktop.
        #[test]
        fn a_new_group_never_renumbers_an_existing_one() {
            let mut known = BTreeMap::new();
            let mut next = 1;
            let desktop = vec![row(1, "kwin", 1920, None)];
            ids_into(&mut known, &mut next, &desktop);
            let before = assemble_displays(desktop, &Layout::default(), "extend", &known);
            let kwin_group = before[0].group;

            // The SAME map, one poll later, with a gamescope spawn alongside.
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
            // Two independent gamescope spawns must NOT share a group or auto-row against each other.
            let rows = vec![
                row(1, "gamescope", 1920, None),
                row(2, "gamescope", 1280, None),
            ];
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &Layout::default(), "extend", &ids);
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
            let ids = ids_for(&rows);
            let out = assemble_displays(rows, &layout, "extend", &ids);
            let by_slot = |s: u32| out.iter().find(|d| d.identity_slot == Some(s)).unwrap();
            assert_eq!(by_slot(1).position, (1920, 0));
            assert_eq!(by_slot(7).position, (0, 0));
        }

        /// The expiry sweep is the linger timer's whole job, and it is pure over `(entries, now,
        /// epoch)` — so the A4 stale-epoch backstop and the Active exemption are pinnable here.
        #[test]
        fn expiry_reaps_deadlines_and_stale_epoch_corpses_but_never_an_active_entry() {
            use std::time::Duration;
            let t0 = Instant::now();
            let mut es = Vec::new();
            // gen 1: lingering, deadline passed.
            let mut e1 = test_entry("kwin", 1, None);
            e1.life = lifecycle::State::Lingering {
                until: t0 - Duration::from_millis(1),
            };
            es.push(e1);
            // gen 2: lingering, deadline in the future, current epoch → survives.
            let mut e2 = test_entry("kwin", 2, None);
            e2.life = lifecycle::State::Lingering {
                until: t0 + Duration::from_secs(60),
            };
            e2.epoch = 5;
            es.push(e2);
            // gen 3: pinned, but from a DEAD epoch → reaped (its compositor is gone).
            let mut e3 = test_entry("kwin", 3, None);
            e3.life = lifecycle::State::Pinned;
            e3.epoch = 4;
            es.push(e3);
            // gen 4: ACTIVE from a dead epoch → left to its own session's rebuild.
            let mut e4 = test_entry("kwin", 4, None);
            e4.life = lifecycle::State::Active { refs: 1 };
            e4.epoch = 4;
            es.push(e4);
            // gen 5: a gamescope spawn from a "dead" epoch — exempt (independent nested session).
            let mut e5 = test_entry("gamescope", 5, None);
            e5.life = lifecycle::State::Pinned;
            e5.epoch = 1;
            es.push(e5);

            let (expired, restores) = take_expired(&mut es, t0, 5);
            assert!(restores.is_empty());
            let gone: Vec<u64> = expired.iter().map(|e| e.gen).collect();
            assert_eq!(gone, vec![1, 3]);
            let left: Vec<u64> = es.iter().map(|e| e.gen).collect();
            assert_eq!(left, vec![2, 4, 5]);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Linux keep-alive pool
// ---------------------------------------------------------------------------------------------

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

    /// The result of the keep-alive reuse lookup (A2 validated reuse): a live kept display was reused,
    /// a dead one was pulled out (recreate), or nothing matched.
    enum ReuseOutcome {
        /// A live kept display — the session-facing output to return.
        Reused(VirtualOutput),
        /// A dead kept display, removed from the pool, plus its group restore (run before the corpse's
        /// keepalive drops); the caller falls through to a fresh create.
        Dead(Entry, Option<Restore>),
        /// No matching kept display.
        Miss,
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

    /// The identity slots of the displays currently in the pool (see
    /// [`super::live_identity_slots`]). Takes the pool lock only — never call it while holding it.
    pub(super) fn live_identity_slots() -> std::collections::BTreeSet<u32> {
        let Some(r) = REG.get() else {
            return Default::default();
        };
        let es = r.entries.lock().unwrap();
        es.iter().filter_map(|e| e.identity_slot).collect()
    }

    /// Pool size (live + kept) for the admission ceiling. `0` before the first acquire, when the
    /// registry has not been initialised at all.
    pub(super) fn live_display_count() -> u32 {
        REG.get()
            .map(|r| r.entries.lock().unwrap().len() as u32)
            .unwrap_or(0)
    }

    /// The linger resolution for Linux: the console policy's `keep_alive` when configured, else
    /// **Immediate** (today's behavior — a Linux disconnect tears the output down at once).
    fn linger() -> Linger {
        policy::prefs()
            .configured_effective()
            .map(|e| e.keep_alive.linger())
            .unwrap_or(Linger::Immediate)
    }

    /// Start the background reaper (lingering displays past their deadline) — once, but only once it
    /// has actually STARTED.
    ///
    /// This used to be a `Once` around a `spawn()` whose `Result` was discarded, so a single failed
    /// spawn (EAGAIN under thread pressure, an RLIMIT_NPROC ceiling) consumed the `Once` and left
    /// every kept display un-reaped for the process lifetime — silently, with no later acquire ever
    /// retrying. The flag is set only on success, so a failure is loud and the next acquire tries
    /// again; the mutex makes the check-and-spawn atomic against concurrent acquires.
    fn ensure_timer() {
        static STARTED: Mutex<bool> = Mutex::new(false);
        let mut started = STARTED.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            return;
        }
        match std::thread::Builder::new()
            .name("vdisplay-linger".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_millis(500));
                let (expired, restores) = {
                    let mut es = reg().entries.lock().unwrap();
                    take_expired(&mut es, Instant::now(), crate::session_epoch())
                };
                // Re-enable physicals (group emptied) BEFORE dropping the outputs — outside the lock.
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
            }) {
            Ok(_) => *started = true,
            Err(e) => tracing::error!(
                error = %e,
                "virtual display: could not start the keep-alive linger reaper — kept displays \
                 will not expire until a later session retries"
            ),
        }
    }

    /// Tell the console `n` managed displays just went away (design §7 — the SSE display view).
    /// Every Linux teardown path funnels through here: the lease drop, the linger reaper, the A2
    /// dead-display teardown, the mgmt release / mode-switch retire and the A4 backend invalidation.
    /// Before, only `/display/release` emitted, so a display that lingered out — or that a
    /// Game↔Desktop switch invalidated — stayed in the console's live view forever.
    fn emit_released(n: usize) {
        if n > 0 {
            crate::emit_display_event(crate::DisplayEvent::Released { count: n as u32 });
        }
    }

    /// Build the session-facing [`VirtualOutput`]: the kept node + a fresh gen-stamped lease. Only
    /// the poolable (`remote_fd == None`) backends reach here, so `remote_fd` is always `None`.
    fn output_for(
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        gen: u64,
        quit: Arc<AtomicBool>,
        reused: bool,
    ) -> VirtualOutput {
        // The pooled display is registry-owned; the session holds a gen-stamped lease as its keepalive.
        let mut out = VirtualOutput::owned(
            node_id,
            preferred_mode,
            Box::new(DisplayLease { gen, quit }),
        );
        // A2: tell the pipeline builder this was a REUSED kept display, so a first-frame failure can
        // `mark_failed(gen)` (tear the corpse down) rather than re-wedge the retry loop on the same node.
        out.reused_gen = reused.then_some(gen);
        // H4: every pooled display carries its gen, so a mode-switch rebuild can `retire` the entry
        // this output's successor supersedes.
        out.pool_gen = Some(gen);
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
        // A2 reuse key: the launch command this acquire carries (a kept spawn running game A must never
        // be reused for a session launching game B). A4 reuse key: the current session epoch.
        let launch = vd.launch_command();
        let cur_epoch = crate::session_epoch();
        let r = reg();

        // Reap expired first (run any group restores + drop outside the lock).
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

        // Reuse: a kept (lingering/pinned) display of the same backend + mode + launch + epoch. A
        // reconnecting session re-attaches a fresh PipeWire consumer to the still-live `node_id`. Gated
        // on `vd.poolable_now()` (A1): a gamescope managed/attach acquire must NOT reuse a kept bare-spawn
        // (they share the backend name `"gamescope"`); its `create` builds a `SessionManaged`/`External`
        // output that passes through below.
        if vd.poolable_now() {
            // Reuse a kept display, matching backend + mode + launch (+ epoch for the desktop backends;
            // gamescope spawns are independent nested sessions, exempt from the active-session epoch —
            // see `epoch_matches`). The liveness probe (`kept_display_alive`, which may shell `pw-dump`
            // for gamescope) must NOT run under the pool lock (it can block / hang the daemon), so:
            //   1. find the candidate + snapshot (gen, node_id) UNDER the lock, then release it;
            //   2. probe liveness OUTSIDE the lock;
            //   3. re-lock and re-find the SAME entry by its gen (another thread may have reused/removed
            //      it meanwhile — then we just miss and create fresh).
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
                            && e.hw_cursor == vd.hw_cursor()
                            && e.hdr == vd.hdr()
                            && epoch_matches(e.backend, e.epoch, cur_epoch)
                    })
                    .map(|e| (e.gen, e.node_id))
            };
            if let Some((cand_gen, node_id)) = candidate {
                let alive = vd.kept_display_alive(node_id); // OUTSIDE the lock (may block)
                let reuse = {
                    let mut es = r.entries.lock().unwrap();
                    // Re-find the SAME entry by its snapshot gen; skip if it's gone or no longer kept
                    // (a concurrent reconnect adopted it) — we then miss and create fresh.
                    match es.iter().position(|e| {
                        e.gen == cand_gen
                            && matches!(
                                e.life,
                                lifecycle::State::Lingering { .. } | lifecycle::State::Pinned
                            )
                    }) {
                        Some(idx) if alive => {
                            es[idx].life.acquire();
                            let gen = r.gen.fetch_add(1, Ordering::Relaxed);
                            es[idx].gen = gen;
                            let preferred_mode = es[idx].preferred_mode;
                            tracing::info!(
                                backend,
                                node_id,
                                "virtual display reused (keep-alive reconnect)"
                            );
                            ReuseOutcome::Reused(output_for(
                                node_id,
                                preferred_mode,
                                gen,
                                quit.clone(),
                                true,
                            ))
                        }
                        Some(idx) => {
                            // Dead kept display: remove it, hand off its group restore, create fresh.
                            let mut dead = es.remove(idx);
                            let (b, g) = (dead.backend, dead.gen);
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
                        // Outside the lock: re-enable physicals (if the group emptied) then drop the
                        // corpse's keepalive (may block) — then fall through to a fresh create below.
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

        // The new display's generation stamp, taken BEFORE the group questions below: it is this
        // display's identity for the rest of the acquire, and `group_key` needs it — a gamescope
        // spawn's group IS its gen. A gen burned by a failed create is harmless (they are opaque
        // and monotonic, never an index).
        let gen = r.gen.fetch_add(1, Ordering::Relaxed);

        // NOTE: the operator's `max_displays` ceiling is NOT enforced here. It belongs at
        // admission (`admission::admit`), which is where Windows has always applied it, and the
        // difference is not cosmetic: `admit` runs ONCE per connecting session, while `acquire` runs
        // again on every mid-stream rebuild. A ceiling applied here refuses the create-before-drop
        // rebuilds — capture loss, a Game↔Desktop switch — because the session being rebuilt still
        // holds its old lease and so counts itself against the budget. Only the mode-switch path
        // passes `supersedes`; the other two pass `None`, so at `max_displays = 1` a single
        // streaming client could never recover from a capture loss. See `admission::admit`.
        // Tell the backend whether it's the FIRST display of its group (no live sibling in the same
        // §6.1 group) — so a topology-establishing backend (Mutter exclusive) extends into an
        // already-exclusive desktop rather than re-clobbering the first session's virtual.
        // Best-effort (a concurrent create is a narrow race); single-session is always `first ==
        // true` → today's behavior. Group membership is [`in_group`]'s single definition, so the
        // display this acquire SUPERSEDES is excluded (still Active here — the old lease drops only
        // after the new pipeline is up — but it is leaving, and deferring to it loses the group's
        // Primary/Exclusive topology on every resize) and a second gamescope spawn is correctly its
        // own group rather than an extension of another client's. Only the liveness term is local to
        // this question: kept (Lingering/Pinned) entries have no session owning them, so there is no
        // live desktop to clobber and a new session next to an unclaimed leftover should still
        // establish topology.
        let first_in_group = {
            let es = r.entries.lock().unwrap();
            !es.iter().any(|e| {
                in_group(e.backend, e.gen, backend, gen, supersedes)
                    && matches!(e.life, lifecycle::State::Active { .. })
            })
        };
        vd.set_first_in_group(first_in_group);

        // Create a fresh display (NOT under the lock — `vd.create` blocks + spawns threads).
        let real = vd.create(mode)?;
        // The identity slot the backend just resolved (KWin per-slot naming; `None` elsewhere) — keys
        // the group arrangement (manual per-slot positions) + the state slot.
        let identity_slot = vd.last_identity_slot();

        // Pool ONLY a registry-owned display on the default PipeWire daemon
        // (design/gamemode-and-dedicated-sessions.md A1). Pass through, unchanged (capturer owns the
        // keepalive, teardown on drop), everything else:
        //   * `External`/`SessionManaged` — gamescope attach / managed session: the gamescope module
        //     owns their lifecycle (its own restore machinery), so the registry must not keep them
        //     (the stale-node reuse wedge). Their unit keepalive tears nothing down on drop.
        //   * `remote_fd = Some` — wlroots' sandboxed xdpw portal fd can't be re-opened per attach.
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
        // Fresh creates only: the backend may have birthed the output at a sacrificial mode whose
        // stream must renegotiate before frames count (KWin >60 Hz — see backend.rs). A REUSED kept
        // display already renegotiated in its prior session (the producer's rebuilt offer persists
        // across consumer reconnects), so the reuse path above correctly leaves the flag off.
        let expect_exact_dims = real.expect_exact_dims;
        // The backend's topology-restore action (KWin `exclusive` → re-enable the disabled physicals),
        // lifted into the group so it runs once when the group's last member drops (§6.1), not at this
        // session's teardown. `None` for non-exclusive / non-first / backends whose topology auto-reverts.
        let topology_restore = vd.take_topology_restore();
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
            launch: launch.clone(),
            epoch: cur_epoch,
            gen,
            hw_cursor: vd.hw_cursor(),
            hdr: vd.hdr(),
        };

        // Compute this new display's position in its group (design §6.2) BEFORE pushing, then push
        // under the same lock: the new one appends last (rightmost under auto-row).
        // `position_for_new` is pure; the lock is held only across it (I/O-free) — the backend apply
        // is below, outside the lock.
        let position = {
            use crate::layout::Member;
            let layout_policy = policy::prefs()
                .configured_effective()
                .map(|e| e.layout)
                .unwrap_or_default();
            let mut es = r.entries.lock().unwrap();
            // Same-group members ([`in_group`], design §6.1): one group per desktop backend, each
            // gamescope spawn its own — and NOT the display this acquire supersedes, or a mid-stream
            // resize would auto-row the replacement past its own predecessor and walk the display one
            // width to the right on every mode switch.
            let existing: Vec<(u64, Member)> = es
                .iter()
                .filter(|e| in_group(e.backend, e.gen, backend, gen, supersedes))
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
        let mut out = output_for(node_id, preferred_mode, gen, quit, false);
        out.expect_exact_dims = expect_exact_dims;
        Ok(out)
    }

    /// The [`DisplayLease`] `Drop` path: release the session's hold on the pooled display. The
    /// lifecycle machine decides linger / pin / teardown; a torn-down entry's keepalive drops *after*
    /// the lock is released.
    fn release(gen: u64, force_immediate: bool) {
        let Some(r) = REG.get() else { return };
        let linger = effective_linger(force_immediate, linger());
        let (torn_down, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.gen == gen) else {
                return; // stale lease (entry reused + re-stamped, or already gone) — no-op
            };
            match es[idx].life.release(Instant::now(), linger) {
                Release::Teardown => {
                    let mut e = es.remove(idx);
                    let (backend, g) = (e.backend, e.gen);
                    // Per-group restore (§6.1): hand the physical re-enable to a surviving sibling, or run
                    // it now if this was the group's last member.
                    let restore = hand_off_restore(&mut es, backend, g, e.topology_restore.take());
                    (Some(e), restore)
                }
                // A release against a slot with NO live hold — a stale or duplicate lease drop. The
                // machine's contract for it is "do nothing", and it was wired to the teardown arm:
                // the one outcome that means the caller has no claim on this display would have torn
                // the display down. Unreachable today (a lease's gen is unique per acquire and the
                // lookup above is by gen, so a stale lease misses the pool entirely and returns
                // early), which is exactly why it has to be right by construction rather than by
                // luck — matching the Windows manager's own Noop arm.
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
        // Re-enable the physicals (group emptied) BEFORE dropping the output — outside the lock.
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
        // Stable per-group ids, carried across polls (see `assign_group_ids`) — a new group must
        // never renumber an existing one under the console. The state is process-lifetime and this
        // is the only thing that touches it, so it lives here rather than in the pure core.
        let mut keys: Vec<String> = rows.iter().map(|r| group_key(r.backend, r.gen)).collect();
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

    /// H4 — force-release a display superseded by a mid-stream mode switch. Same machinery as
    /// [`force_release`] (kept entries only — an Active entry is refused, and a gen already torn
    /// down under `immediate` is a no-op), distinct log line.
    pub(super) fn retire(gen: u64) {
        release_kept(Some(gen), "retired (superseded by a mode switch)");
    }

    /// Remove + tear down KEPT (lingering/pinned) entries — all of them, or one by gen — running /
    /// handing off group topology restores, with keepalive drops outside the lock. The shared core
    /// of [`force_release`] (mgmt) and [`retire`] (mode-switch supersede).
    fn release_kept(slot: Option<u64>, why: &'static str) -> usize {
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
                    let (backend, g) = (e.backend, e.gen);
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
        // Re-enable physicals (group emptied) BEFORE dropping the outputs — outside the lock.
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

    /// A2 — tear down a reused-but-dead pool entry by its generation stamp. Removes it (hand off /
    /// run its group restore), drops the keepalive outside the lock. Idempotent (already gone → no-op).
    pub(super) fn mark_failed(gen: u64) {
        let Some(r) = REG.get() else { return };
        let (torn, restore) = {
            let mut es = r.entries.lock().unwrap();
            let Some(idx) = es.iter().position(|e| e.gen == gen) else {
                return; // already gone — the subsequent stale-gen lease drop no-ops too
            };
            let mut e = es.remove(idx);
            let (backend, g) = (e.backend, e.gen);
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

    /// A4 — invalidate every kept display of `backend` (its compositor instance is gone). Removes them
    /// all (any lifecycle state — a dead compositor's Active entries are doomed too; their sessions
    /// rebuild), runs/hands off group restores, drops keepalives outside the lock (they hit dead
    /// sockets and fail fast). Mirrors `force_release`'s shape but selects by backend, not slot/state.
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
                    let (b, g) = (e.backend, e.gen);
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

    /// The session's refcount handle — the `keepalive` the capturer holds. `Drop` releases the
    /// registry hold; a stale lease (its entry was reused + re-stamped, or torn down) is a no-op.
    struct DisplayLease {
        gen: u64,
        /// The session's deliberate-quit flag: set when the client closes with the quit application
        /// code (a user "stop", not a network drop), so this lease's `Drop` tears the display down
        /// immediately instead of lingering. `false` on a bare disconnect → normal keep-alive.
        quit: Arc<AtomicBool>,
    }

    impl Drop for DisplayLease {
        fn drop(&mut self) {
            release(self.gen, self.quit.load(Ordering::SeqCst));
        }
    }
}
