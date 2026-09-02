//! Compositor-backend contract for virtual outputs.
//!
//! [`DisplayOwnership`] is who may pool or tear an output down. [`VirtualOutput`] is the created
//! capture target plus the RAII keepalive that releases the compositor resource. [`VirtualDisplay`]
//! is the boxed trait `super::open` returns; per-backend `impl`s and the factory stay in `super`.
//!
//! Pin a backend by name (`"kwin"`, `"mutter"`, `"wlroots"`, `"gamescope"`). Evidence:
//! `design/gamemode-and-dedicated-sessions.md`, `design/display-management.md`.

use super::*;

/// Who may pool or tear this output down. The registry keep-alives only [`Self::Owned`];
/// [`Self::External`] and [`Self::SessionManaged`] pass through because their lifecycle lives
/// elsewhere (gamescope attach / session-plus). See `design/gamemode-and-dedicated-sessions.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayOwnership {
    /// Registry may pool, linger, pin, and tear down (KWin, Mutter, wlroots, gamescope bare spawn,
    /// Windows). Default: a silent backend is owned.
    #[default]
    Owned,
    /// Mirrored foreign display: no keep-alive, topology, or reuse (gamescope attach).
    External,
    /// Gamescope-module session (`gamescope-session-plus` / SteamOS). Registry pass-through;
    /// restore stays in the gamescope module.
    SessionManaged,
}

/// Isolation identity for an independent gamescope bare-spawn (`design/gamescope-multiuser.md`).
/// Private planes instead of host-lifetime shared ones. Carried on the backend like
/// [`GamescopeRoute`] (`VirtualDisplay::set_session_isolation`); only the spawn path consumes it.
///
/// `id` is stable per client (cert-fingerprint prefix). A kept spawn's env is baked in, so the
/// registry may hand it back only to the same `isolation_key`.
///
/// Defined on every platform: the host's `SessionContext` carries it beside `compositor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIsolation {
    /// Cert-fingerprint prefix (lowercase hex) or `anon<seq>`.
    pub id: String,
    /// This session's `LIBEI_SOCKET` relay (`pf_paths::gamescope_ei_socket_file_for`);
    /// the pinned injector reads it, the spawn wrapper writes it.
    pub ei_relay: std::path::PathBuf,
    /// Nested apps' `PULSE_SINK` `node.name`. `None` when the operator forced monitor-mode
    /// capture (no per-session sink exists).
    pub sink: Option<String>,
    /// Nested apps' `PULSE_SOURCE` `node.name`.
    pub mic_source: Option<String>,
}

#[cfg(target_os = "linux")]
impl SessionIsolation {
    /// Build the identity, computing the relay path under the session env lock: the producer-side
    /// `XDG_RUNTIME_DIR` read must not race a concurrent handshake's `apply_session_env`.
    pub fn new(id: String, sink: Option<String>, mic_source: Option<String>) -> SessionIsolation {
        let ei_relay = crate::with_env_lock(|| pf_paths::gamescope_ei_socket_file_for(&id));
        SessionIsolation {
            id,
            ei_relay,
            sink,
            mic_source,
        }
    }
}

/// Capture target plus RAII keepalive; drop releases the compositor resource.
///
/// `dead_code` allowed: constructors sit behind per-OS `cfg`, so this type looks unused here.
#[allow(dead_code)]
pub struct VirtualOutput {
    pub node_id: u32,
    /// PipeWire fd of a sandboxed remote node (Mutter RemoteDesktop+ScreenCast). `None` = default
    /// daemon (KWin `zkde_screencast`); connect there directly.
    #[cfg(target_os = "linux")]
    pub remote_fd: Option<OwnedFd>,
    /// `(width, height, refresh_hz)` for PipeWire format negotiation. KWin/gamescope already
    /// created at this size; Mutter sizes the virtual monitor FROM the negotiation.
    pub preferred_mode: Option<(u32, u32, u32)>,
    /// DXGI adapter LUID + GDI output name; host `capture::capture_virtual_output` duplicates this.
    #[cfg(target_os = "windows")]
    pub win_capture: Option<pf_frame::dxgi::WinCaptureTarget>,
    /// Holds the compositor resource (and its connection/thread) until drop.
    pub keepalive: Box<dyn Send>,
    /// Registry pools only [`DisplayOwnership::Owned`]; the rest pass through to the capturer.
    pub ownership: DisplayOwnership,
    /// Generation when [`registry::acquire`](crate::registry::acquire) reused a kept display, so a
    /// first-frame failure can [`registry::mark_failed`](crate::registry::mark_failed) instead of
    /// retrying the same dead node. `None` on a fresh create or a non-poolable output.
    #[cfg(target_os = "linux")]
    pub reused_gen: Option<u64>,
    /// Pool generation of this display (fresh or reused, unlike `reused_gen`). A mid-stream
    /// mode-switch [`registry::retire`](crate::registry::retire)s the superseded display so it
    /// does not linger. `None` for non-poolable outputs. See `design/midstream-resolution-resize.md`.
    #[cfg(target_os = "linux")]
    pub pool_gen: Option<u64>,
    /// Created at a sacrificial size so KWin rebuilds its format offer (refresh cap only updates
    /// on a size change while recording). Capturer holds frames until renegotiation to
    /// `preferred_mode`. See kwin `create`.
    #[cfg(target_os = "linux")]
    pub expect_exact_dims: bool,
    /// Compositor `wl_output.name` (Hyprland `PF-<pid>-<n>`, sway `HEADLESS-N`, mirrored connector)
    /// so the host can aim absolute input (`pf_inject::set_stream_output`). Protocol-stable across
    /// clients. `None` when mapping does not need it (KWin/Mutter libei-by-region; gamescope owns
    /// the seat). Carried only: this crate must not depend on pf-inject.
    #[cfg(target_os = "linux")]
    pub output_name: Option<String>,
}

impl VirtualOutput {
    /// Registry-owned output. Caller fills the platform fields (`remote_fd`, `win_capture`, …).
    pub fn owned(
        node_id: u32,
        preferred_mode: Option<(u32, u32, u32)>,
        keepalive: Box<dyn Send>,
    ) -> VirtualOutput {
        VirtualOutput {
            node_id,
            #[cfg(target_os = "linux")]
            remote_fd: None,
            preferred_mode,
            #[cfg(target_os = "windows")]
            win_capture: None,
            keepalive,
            ownership: DisplayOwnership::Owned,
            #[cfg(target_os = "linux")]
            reused_gen: None,
            #[cfg(target_os = "linux")]
            pool_gen: None,
            #[cfg(target_os = "linux")]
            expect_exact_dims: false,
            #[cfg(target_os = "linux")]
            output_name: None,
        }
    }
}

/// Per-compositor virtual-output backend. [`create`](Self::create) is RAII (drop the keepalive).
/// Setters are instance-local, not process env, so concurrent sessions cannot stomp.
pub trait VirtualDisplay: Send {
    /// Backend pin name (`"kwin"`, `"wlroots"`, `"mutter"`).
    fn name(&self) -> &'static str;
    /// Create a virtual output of the given mode. Teardown is RAII: drop the returned
    /// [`VirtualOutput`]'s `keepalive`.
    fn create(&mut self, mode: Mode) -> Result<VirtualOutput>;
    /// Nested launch command. Instance-local, not env: concurrent sessions must not stomp.
    /// Default no-op; only gamescope spawn uses it.
    fn set_launch_command(&mut self, _cmd: Option<String>) {}
    /// Resolved gamescope sub-mode ([`resolve_gamescope_route`](crate::resolve_gamescope_route)).
    /// Instance-local: env (`PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION`) races concurrent sessions.
    /// Default no-op.
    fn set_gamescope_route(&mut self, _route: Option<crate::GamescopeRoute>) {}
    /// [`SessionIsolation`] for an independent gamescope spawn (`design/gamescope-multiuser.md`).
    /// Instance-local. Default no-op; only gamescope bare-spawn isolates.
    fn set_session_isolation(&mut self, _iso: Option<SessionIsolation>) {}
    /// Isolation identity baked into the next `create`. Registry reuse key: a kept spawn's env
    /// (relay, audio) is process-baked, so it may only return to the same identity. `None` matches
    /// only other `None`.
    fn isolation_key(&self) -> Option<String> {
        None
    }
    /// Client cert fingerprint for a stable virtual-monitor identity across reconnects (DPI, etc.).
    /// Windows: EDID serial. KWin: per-slot output name. Mutter: host-side persistence (virtual
    /// monitors cannot carry identity). `None` = anonymous / GameStream → auto slot. Default no-op.
    fn set_client_identity(&mut self, _fingerprint: Option<[u8; 32]>) {}
    /// Deliberate-quit flag (QUIT application code, not a network drop). Last lease drop tears
    /// down immediately (`Linger::Immediate` on Linux). Default no-op: only Windows pf-vdisplay
    /// needs it — its leases live in `VirtualDisplayManager`, which `registry::acquire` does not
    /// reach. Linux gets this through the registry.
    fn set_quit_flag(&mut self, _quit: std::sync::Arc<std::sync::atomic::AtomicBool>) {}
    /// Client panel HDR volume (`Hello::display_hdr`) for the virtual output's EDID (CTA-861.3).
    /// Host apps then tone-map to the panel the stream lands on. `None` = unknown/SDR → default
    /// EDID. Default no-op: only Windows pf-vdisplay mints per-monitor EDIDs.
    fn set_client_hdr(&mut self, _hdr: Option<punktfunk_core::quic::HdrMeta>) {}
    /// Stream negotiated HDR (10-bit BT.2020/PQ, `bit_depth >= 10` in Welcome). Distinct from
    /// [`set_client_hdr`](Self::set_client_hdr) (panel volume for EDID). Default no-op; gamescope
    /// adds `--hdr-enabled --hdr-debug-force-support` so nested WSI surfaces are HDR and capture
    /// is 10-bit PQ.
    fn set_hdr(&mut self, _on: bool) {}
    /// Current HDR request. Registry reuse key: an SDR-spawned display cannot serve HDR (no HDR
    /// WSI surfaces; PQ over an SDR composite) nor vice versa.
    fn hdr(&self) -> bool {
        false
    }
    /// Out-of-band cursor channel: compositor stops embedding the pointer; capture surfaces
    /// shape/position separately. Off = embedded (no host cursor work) — Moonlight / GameStream /
    /// legacy. Default no-op (gamescope has no cursor either way).
    fn set_hw_cursor(&mut self, _on: bool) {}
    /// Current out-of-band-cursor request. Registry reuse key: an embedded-pointer display has no
    /// cursor metadata; a cursor-channel display would miss the pointer in frames.
    fn hw_cursor(&self) -> bool {
        false
    }
    /// Portal-negotiated ScreenCast cursor mode of the last [`create`](Self::create) — the answer
    /// to [`set_hw_cursor`](Self::set_hw_cursor), which is only a request.
    ///
    /// wlr (xdph, xdpw) advertises `Hidden|Embedded`, so a metadata request is served `Embedded`:
    /// the pointer is painted into frames and `SPA_META_Cursor` never arrives. Reading "no overlay"
    /// as "pointer is off the recorded view" is noise there (true on Mutter). See
    /// [`PortalCursorMode::delivers_metadata`](crate::PortalCursorMode::delivers_metadata).
    ///
    /// `None` (default, every non-portal backend, and before first `create`) means nothing was
    /// negotiated through xdg ScreenCast: KWin/Mutter/gamescope/Windows use their own protocols.
    /// wlr and [`mirror`](crate::open_mirror) report this; they are never pooled (`remote_fd` is
    /// `Some`), so a reuse cannot return a stale answer.
    fn last_portal_cursor_mode(&self) -> Option<crate::PortalCursorMode> {
        None
    }
    /// Identity slot of the last [`create`](Self::create) (`Some` = per-client policy, `None` =
    /// shared/anonymous). Registry keys group arrangement and `/display/state` on it. Default
    /// `None` (wlroots/gamescope auto-row). KWin and Mutter report a real slot.
    fn last_identity_slot(&self) -> Option<u32> {
        None
    }
    /// Place the last [`create`](Self::create) at `(x, y)` in desktop space. Registry owns the
    /// group and calls this after `create` (auto-row or console arrangement). Never called for
    /// origin `(0, 0)`. Default no-op (only KWin positions). Best-effort: failure keeps compositor
    /// default. See `design/display-management.md`.
    fn apply_position(&mut self, _x: i32, _y: i32) {}
    /// Topology restore this [`create`](Self::create) prepared (re-enable heads an `exclusive` /
    /// `primary` change disabled). Registry lifts it into the group so it runs once, when the
    /// last display in the group tears down — a sibling exclusive session must not have physicals
    /// re-enabled under it. Called after `create`; the backend must not also run it. Default `None`
    /// (Mutter `APPLY_TEMPORARY` auto-reverts). See `design/display-management.md`.
    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
    /// Whether this [`create`](Self::create) is the first display in its group. Mutter exclusive
    /// `ApplyMonitorsConfig` applies only on first; a later sibling extends, because a fresh
    /// sole-monitor config would disable the first virtual. Registry sets this before `create`.
    /// Default no-op: KWin sees siblings by output name.
    fn set_first_in_group(&mut self, _first: bool) {}
    /// Whether the current request's [`create`](Self::create) yields a poolable
    /// ([`DisplayOwnership::Owned`]) display. Registry consults this *before* reuse lookup so a
    /// gamescope managed/attach acquire cannot reuse a kept bare-spawn (same backend name).
    /// Gamescope: `false` for `Managed`/`Attach`, `true` for `Spawn` and for no route (`create`'s
    /// `None` arm is bare spawn, including operator-pinned `PUNKTFUNK_COMPOSITOR`). Mirror: always
    /// `false`. See `design/gamemode-and-dedicated-sessions.md`.
    ///
    /// Default `true` is a default, not a fact: it must agree with the ownership `create` reports,
    /// and nothing enforces that.
    fn poolable_now(&self) -> bool {
        true
    }
    /// Launch command on this instance ([`set_launch_command`](Self::set_launch_command)). Registry
    /// reuse key `(backend, mode, launch)`: a kept game A must not serve a session that asked for
    /// B. Default `None`; only gamescope reports it.
    fn launch_command(&self) -> Option<String> {
        None
    }
    /// Is this kept `node_id` still live? Registry checks before reuse; `false` tears it down and
    /// creates fresh. Default `true` — [`mark_failed`](crate::registry::mark_failed) is the backstop
    /// if it dies between this check and first frame. Only gamescope overrides (nested session dies
    /// with the game); KWin/Mutter nodes die with the compositor, already reaped by session-epoch.
    fn kept_display_alive(&mut self, _node_id: u32) -> bool {
        true
    }
}

/// Keep the first topology restore this backend instance captured; later `None` (or a subset) must
/// not overwrite it.
///
/// One instance serves every attempt of the host pipeline retry loop, so `create` runs against this
/// slot repeatedly. Attempt 1 disables heads and prepares the restore; attempts 2..n correctly
/// find nothing left and prepare `None`. Overwriting would drop attempt 1's closure, and Drop
/// would have nothing to re-enable.
///
/// First-wins is also the right *set*: attempt 1 saw every lit head; later attempts see a subset.
///
/// Registry-drained backends (KWin, pooled — [`VirtualDisplay::take_topology_restore`] empties the
/// slot) arrive with `None` held and are unaffected. Pass-through backends (Hyprland, wlroots;
/// portal fd, never taken) need this.
pub(crate) fn stash_topology_restore(
    slot: &mut Option<Box<dyn FnOnce() + Send>>,
    prepared: Option<Box<dyn FnOnce() + Send>>,
) {
    if slot.is_none() {
        *slot = prepared;
    }
}

#[cfg(test)]
mod topology_restore_tests {
    use super::stash_topology_restore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn counting(hits: &Arc<AtomicUsize>) -> Option<Box<dyn FnOnce() + Send>> {
        let hits = Arc::clone(hits);
        Some(Box::new(move || {
            hits.fetch_add(1, Ordering::SeqCst);
        }))
    }

    /// Retry loop: eight `create`s on one instance; only the first prepared a restore; Drop is
    /// the only runner. The slot must still hold it.
    #[test]
    fn eight_failed_attempts_do_not_strand_the_restore() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut slot: Option<Box<dyn FnOnce() + Send>> = None;

        // Attempt 1: heads were on; restore prepared.
        stash_topology_restore(&mut slot, counting(&hits));
        // Attempts 2..8 prepare `None` (nothing left to disable). Must not drop attempt 1.
        for _ in 0..7 {
            stash_topology_restore(&mut slot, None);
        }

        let restore = slot.expect("the retry loop stranded the restore — the desk stays dark");
        restore();
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the restore must run exactly once, on the failure unwind"
        );
    }

    /// A later prepared restore never displaces the first (attempt 1 saw the full lit set).
    #[test]
    fn a_later_restore_never_displaces_the_first() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut slot: Option<Box<dyn FnOnce() + Send>> = None;

        stash_topology_restore(&mut slot, counting(&first));
        stash_topology_restore(&mut slot, counting(&second));

        slot.expect("a restore should be held")();
        assert_eq!(first.load(Ordering::SeqCst), 1, "the first must be kept");
        assert_eq!(
            second.load(Ordering::SeqCst),
            0,
            "the second must be dropped"
        );
    }

    /// A drained slot (KWin / registry take) still accepts a later restore.
    #[test]
    fn an_empty_slot_still_accepts_a_restore() {
        let hits = Arc::new(AtomicUsize::new(0));
        let mut slot: Option<Box<dyn FnOnce() + Send>> = None;

        stash_topology_restore(&mut slot, counting(&hits));
        let _drained = slot.take(); // registry lifted it into the group
        assert!(slot.is_none());
        stash_topology_restore(&mut slot, counting(&hits));
        assert!(slot.is_some(), "a drained slot must be refillable");
    }

    /// Empty + `None` stays empty: an extend session must not grow a restore.
    #[test]
    fn nothing_prepared_leaves_the_slot_empty() {
        let mut slot: Option<Box<dyn FnOnce() + Send>> = None;
        stash_topology_restore(&mut slot, None);
        assert!(slot.is_none());
    }
}
