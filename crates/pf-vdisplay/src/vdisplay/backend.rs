//! Virtual-display backend contract (plan §W3 — the trait facade carved out of [`super`]).
//! [`DisplayOwnership`] declares who owns an output's lifecycle, [`VirtualOutput`] is the created
//! output (PipeWire node + RAII keepalive), and [`VirtualDisplay`] is the per-compositor backend
//! trait `super::open` returns boxed. The per-backend `impl`s and the factory stay in `super`.

use super::*;

/// Who owns a [`VirtualOutput`]'s lifecycle — the honest declaration that lets the registry
/// (`design/gamemode-and-dedicated-sessions.md` Part A1) pool **only what it owns** instead of
/// keeping outputs whose real lifecycle lives elsewhere (the gamescope managed/attach paths, which
/// are governed by the gamescope module's own session machinery). Extends the CLAUDE.md invariant
/// "the registry owns display lifecycle" with its converse: what the registry does not own, it must
/// not pretend to keep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayOwnership {
    /// The registry owns the lifecycle: it may pool, linger, pin, and tear this display down (KWin,
    /// Mutter, wlroots, gamescope **bare spawn**, and the Windows manager-delegated monitor). The
    /// default — a backend that says nothing is registry-owned.
    #[default]
    Owned,
    /// Someone else's display, merely mirrored: no keep-alive, no topology, no reuse (gamescope
    /// **attach** to a foreign session). Codifies the design-doc §7 "attach = unmanaged pass-through"
    /// row.
    External,
    /// A box-level session the gamescope module manages (the managed `gamescope-session-plus` /
    /// SteamOS takeover). Passed through by the registry (its restore lifecycle is the gamescope
    /// module's until Part A3 hands the registry a real keepalive + restore duty).
    SessionManaged,
}

/// A created virtual output: a PipeWire source to capture, plus an owned keepalive whose drop
/// tears the output down (releases the compositor-side resource).
///
/// Allowed dead on non-Linux: the backends that construct it are all `cfg(target_os = "linux")`.
#[allow(dead_code)]
pub struct VirtualOutput {
    /// PipeWire node id of the output's screencast stream.
    pub node_id: u32,
    /// Portal/remote PipeWire fd when the node lives on a sandboxed remote (e.g. Mutter's
    /// RemoteDesktop+ScreenCast). `None` means the node is on the user's default PipeWire daemon
    /// (KWin `zkde_screencast`), captured by connecting to that daemon directly.
    #[cfg(target_os = "linux")]
    pub remote_fd: Option<OwnedFd>,
    /// `(width, height, refresh_hz)` to prefer in the PipeWire format negotiation. KWin and
    /// gamescope outputs are created at the exact size, so this just confirms it; **Mutter sizes
    /// its virtual monitor FROM the negotiation**, so here it's what makes the client's mode real.
    pub preferred_mode: Option<(u32, u32, u32)>,
    /// Windows capture identity (DXGI adapter LUID + GDI output name) for the pf-vdisplay backend —
    /// what the host `capture::capture_virtual_output` needs to duplicate the right output.
    #[cfg(target_os = "windows")]
    pub win_capture: Option<pf_frame::dxgi::WinCaptureTarget>,
    /// Keeps the output — and whatever connection/thread backs it — alive; dropped on teardown.
    pub keepalive: Box<dyn Send>,
    /// Who owns this display's lifecycle (`design/gamemode-and-dedicated-sessions.md` A1). The
    /// registry pools/keep-alives only [`DisplayOwnership::Owned`] outputs; `External`/`SessionManaged`
    /// pass through (the capturer holds the keepalive, teardown on drop). Defaults to `Owned`.
    pub ownership: DisplayOwnership,
    /// `Some(gen)` when [`registry::acquire`](crate::registry::acquire) handed this back as a
    /// **reused** kept display (`design/gamemode-and-dedicated-sessions.md` A2), so the pipeline builder
    /// can [`registry::mark_failed(gen)`](crate::registry::mark_failed) if the first frame
    /// fails on it — tearing the corpse down so the retry loop's next acquire creates fresh instead of
    /// re-wedging on the same dead node. `None` on a fresh create / non-poolable output. Linux-only (the
    /// keep-alive pool is Linux).
    #[cfg(target_os = "linux")]
    pub reused_gen: Option<u64>,
    /// The registry pool generation of this display (fresh AND reused — unlike `reused_gen`), so a
    /// mid-stream mode-switch rebuild can [`registry::retire`](crate::registry::retire) the
    /// display it supersedes instead of leaving it to accumulate under a linger/forever keep-alive
    /// policy (`design/midstream-resolution-resize.md` H4). `None` for non-poolable outputs.
    /// Linux-only (the keep-alive pool is Linux).
    #[cfg(target_os = "linux")]
    pub pool_gen: Option<u64>,
    /// The backend created the output at a SACRIFICIAL mode and the producer will renegotiate the
    /// live stream to `preferred_mode`'s dims (KWin's screencast only rebuilds its format offer —
    /// and thus its refresh cap — on a size change while recording; see kwin.rs `create`). The
    /// capturer must hold frames until that renegotiation lands. Linux-only.
    #[cfg(target_os = "linux")]
    pub expect_exact_dims: bool,
    /// The compositor's own name for this output (Hyprland's `PF-<pid>-<n>`, sway's `HEADLESS-N`,
    /// a mirrored head's connector) — the Linux answer to what `win_capture` carries on Windows:
    /// the identity the host needs to aim **absolute input** at the head it is streaming
    /// (`pf_inject::set_stream_output`, called from `capture::capture_virtual_output`).
    ///
    /// It is the `wl_output.name` of that head, which the protocol guarantees is the same string
    /// for every client — so the injector can match it on its own Wayland connection. `None` on
    /// the backends whose absolute mapping does not need it (KWin/Mutter inject through libei,
    /// which selects by region; gamescope owns its whole seat).
    ///
    /// This crate must not depend on pf-inject (see the crate doc), so the name is only CARRIED
    /// here — the host publishes it.
    #[cfg(target_os = "linux")]
    pub output_name: Option<String>,
}

impl VirtualOutput {
    /// A registry-[owned](DisplayOwnership::Owned) output — the common case (KWin/Mutter/wlroots,
    /// gamescope bare-spawn, Windows). Fills `ownership: Owned`; the caller sets the platform fields.
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

/// Pluggable virtual-output creation, per compositor.
pub trait VirtualDisplay: Send {
    /// Human-readable backend name (e.g. `"kwin"`, `"wlroots"`, `"mutter"`).
    fn name(&self) -> &'static str;
    /// Create a virtual output of the given mode. Teardown is RAII: drop the returned
    /// [`VirtualOutput`]'s `keepalive`.
    fn create(&mut self, mode: Mode) -> Result<VirtualOutput>;
    /// Set the per-session command this display should launch into its nested output (the resolved
    /// app/game). Carried on the backend instance — NOT a process-global env var — so concurrent
    /// sessions can't stomp each other's launch target. Default: no-op (backends that attach to an
    /// existing session / don't spawn a nested command ignore it; only gamescope's spawn path uses it).
    fn set_launch_command(&mut self, _cmd: Option<String>) {}
    /// Set the RESOLVED gamescope sub-mode for this session (from
    /// [`apply_input_env`](crate::apply_input_env)). Carried on the backend instance for the same
    /// reason as [`set_launch_command`](Self::set_launch_command) — it used to travel through
    /// `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION`, where the GameStream plane and the mid-session switch
    /// watcher could overwrite one session's decision before another session's `create` read it.
    /// Default: no-op (only the gamescope backend has sub-modes).
    fn set_gamescope_route(&mut self, _route: Option<crate::GamescopeRoute>) {}
    /// Set the connecting client's cert fingerprint so the backend can give that client a STABLE virtual
    /// monitor identity across reconnects and its saved per-monitor config (notably DPI scaling) is
    /// reapplied — via the OS (Windows EDID serial), the compositor (KWin per-slot output name), or
    /// host-side persistence (Mutter, whose virtual monitors can't carry a stable identity). Carried on
    /// the backend instance; set once before [`create`](Self::create). Default: no-op (wlroots/gamescope
    /// have no per-client identity). `None` = anonymous/unpaired/GameStream → the backend's auto
    /// (slot-based/shared) identity.
    fn set_client_identity(&mut self, _fingerprint: Option<[u8; 32]>) {}
    /// Hand the backend the session's deliberate-quit flag (set when the client closes with the QUIT
    /// application code — a user "stop", not a network drop) so the last lease's drop can tear the
    /// display down IMMEDIATELY, skipping the keep-alive linger — the Windows analogue of the Linux
    /// registry's `Linger::Immediate` path. Carried on the backend instance; set once before
    /// [`create`](Self::create). Default: no-op — only the Windows pf-vdisplay backend needs it (its
    /// leases live in the `VirtualDisplayManager`, which the registry's quit plumbing does not reach;
    /// Linux backends get the flag through `registry::acquire`).
    fn set_quit_flag(&mut self, _quit: std::sync::Arc<std::sync::atomic::AtomicBool>) {}
    /// Hand the backend the CLIENT display's HDR colour volume (`Hello::display_hdr` — primaries /
    /// white point / luminance range as reported by the client OS), so a freshly created virtual
    /// output can advertise the client's REAL panel in its EDID (pf-vdisplay codes the luminance
    /// into the CTA-861.3 HDR static-metadata block) — host apps and the OS then tone-map to the
    /// panel the stream actually lands on instead of a built-in placeholder volume. Carried on the
    /// backend instance; set once before [`create`](Self::create). `None` = unknown/SDR client →
    /// the backend's default EDID. Default: no-op — only the Windows pf-vdisplay backend can mint
    /// per-monitor EDIDs today (the Linux compositors' virtual outputs take no EDID from us).
    fn set_client_hdr(&mut self, _hdr: Option<punktfunk_core::quic::HdrMeta>) {}
    /// Tell the backend THIS SESSION negotiated HDR — a 10-bit BT.2020/PQ stream (`bit_depth >=
    /// 10`, decided in the punktfunk/1 Welcome before any display exists). Distinct from
    /// [`set_client_hdr`](Self::set_client_hdr), which describes the *client panel's* colour
    /// volume for the EDID; this is the stream's own colourimetry, and the backend may have to
    /// bring the output up differently for it. Carried on the backend instance; set once before
    /// [`create`](Self::create). Default: no-op — only the Linux gamescope backend uses it, where
    /// it adds `--hdr-enabled --hdr-debug-force-support` to the spawn so nested games get HDR
    /// surfaces from the WSI layer and the composite can be captured as 10-bit PQ.
    fn set_hdr(&mut self, _on: bool) {}
    /// The HDR request currently set (see [`set_hdr`](Self::set_hdr)). The registry includes it in
    /// the keep-alive REUSE key: a kept display brought up SDR cannot serve an HDR session (it was
    /// spawned without the HDR flags — the game would see no HDR surfaces while the stream
    /// negotiated PQ over an SDR composite) nor vice versa.
    fn hdr(&self) -> bool {
        false
    }
    /// Ask the backend for an OUT-OF-BAND cursor on the created output (the cursor channel):
    /// the compositor/OS stops compositing the pointer into captured frames and the capture
    /// layer surfaces shape/position separately. Carried on the backend instance; set once
    /// before [`create`](Self::create) (both session paths pass `cursor_forward`). Off = the
    /// compositor EMBEDS the pointer into frames — zero host-side cursor work, the pre-channel
    /// path — which is what every session without the negotiated cursor cap gets (Moonlight /
    /// GameStream / legacy clients / capture-mode starts), mirroring the Windows no-regression
    /// gate. Implementations: Windows pf-vdisplay (IddCx hardware cursor, driver proto v5);
    /// KWin (zkde `pointer` metadata vs embedded); Mutter (`cursor-mode` metadata vs embedded);
    /// wlroots/hyprland (portal `CursorMode`). Default: no-op (gamescope has no cursor either
    /// way — see the Phase C source).
    fn set_hw_cursor(&mut self, _on: bool) {}
    /// The out-of-band-cursor request currently set (see [`set_hw_cursor`](Self::set_hw_cursor)).
    /// The registry includes it in the keep-alive REUSE key: a kept embedded-pointer display can
    /// never serve a cursor-channel session (its stream has no cursor metadata to forward) nor
    /// vice versa (the pointer would be missing from frames).
    fn hw_cursor(&self) -> bool {
        false
    }
    /// The stable identity slot the backend resolved for the most recent [`create`](Self::create) —
    /// the per-client id the identity policy assigned (`Some`), or `None` for shared/anonymous. The
    /// registry reads it right after `create` to key the display's group **arrangement** (manual
    /// per-slot positions) and to label the mgmt `/display/state` slot. Default `None`: a backend
    /// with no per-client identity (wlroots/gamescope) always auto-rows. KWin (per-slot output
    /// naming) and Mutter (host-persisted per-client scale) report a real slot on Linux.
    fn last_identity_slot(&self) -> Option<u32> {
        None
    }
    /// Place the most-recently-[created](Self::create) output at `(x, y)` in the desktop coordinate
    /// space (design `display-management.md` §6.2 — layout). The registry, which owns the display
    /// **group**, computes the position from the whole group (auto-row or the console's manual
    /// arrangement) and calls this right after `create`. Default no-op: only backends that can position
    /// an output (KWin) implement it; the registry never calls it for the desktop origin `(0, 0)`, so a
    /// single-display / first-of-group session issues no positioning at all. Best-effort — a failure
    /// leaves the compositor's default placement.
    fn apply_position(&mut self, _x: i32, _y: i32) {}
    /// Take the topology **restore** action this [`create`](Self::create) prepared — the work that
    /// un-does an `exclusive`/`primary` topology change (e.g. re-enable the physical outputs KWin
    /// disabled). The registry lifts it into the display **group** so it runs **once, when the group's
    /// last display is torn down** (design §6.1 — per-group restore), not when this one session's
    /// display drops: a sibling `exclusive` session must not have the physical re-enabled under it.
    /// Called right after `create`; the backend must not also run it itself. Default `None` — a backend
    /// whose topology auto-reverts (Mutter `APPLY_TEMPORARY`) or that changes nothing has nothing to
    /// hand off.
    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        None
    }
    /// Tell the backend whether this create will be the **first** display in its group — i.e. no
    /// sibling of the same backend is already live (design §6.1). A backend that *establishes* the
    /// group's topology (Mutter's sole-monitor `exclusive` `ApplyMonitorsConfig`) applies it only when
    /// first; a later sibling **extends** into the already-exclusive desktop instead of re-clobbering it
    /// (a fresh sole-monitor config would disable the first session's virtual output). Set by the
    /// registry right before [`create`](Self::create). Default no-op: KWin recognises siblings at
    /// runtime by output name (first-slot-wins + a group-aware disable filter), and single-display
    /// backends never have a sibling.
    fn set_first_in_group(&mut self, _first: bool) {}
    /// Will a [`create`](Self::create) for the CURRENT request produce a registry-poolable
    /// ([`DisplayOwnership::Owned`], keep-alive-able) display? The registry consults this **before**
    /// its keep-alive reuse lookup, so it never hands a kept display of one flavor to a request of
    /// another — specifically a gamescope managed/attach acquire must not reuse a kept **bare-spawn**
    /// (they share the backend name `"gamescope"`). Overridden by gamescope, which reads the
    /// resolved [`GamescopeRoute`](crate::GamescopeRoute) carried on the instance (`self.route`, NOT
    /// env — the sub-mode stopped travelling through `PUNKTFUNK_GAMESCOPE_NODE`/`_SESSION` in Phase
    /// 2.3): `false` for `Managed` and `Attach`, `true` for `Spawn` **and for no route at all**,
    /// since `create`'s own `None` arm falls through to the bare spawn — so an instance nobody
    /// called `set_gamescope_route` on (the operator-pinned `PUNKTFUNK_COMPOSITOR` path) is
    /// poolable, and takes both the reuse lookup and the `max_displays` ceiling. Also overridden by
    /// the mirror backend (`false` always). See `design/gamemode-and-dedicated-sessions.md` A1.
    ///
    /// The default `true` is a DEFAULT, not a fact: it happens to be right for every backend that
    /// creates a display it owns, and it is wrong for any backend whose `create` reports something
    /// other than [`DisplayOwnership::Owned`] — this answer and that one must agree, and nothing
    /// enforces it. A required method would; making it one costs an impl in each of the five
    /// per-compositor backends plus Windows.
    fn poolable_now(&self) -> bool {
        true
    }
    /// The resolved launch command carried on this backend instance (set via
    /// [`set_launch_command`](Self::set_launch_command)). The registry reads it to key keep-alive reuse
    /// on `(backend, mode, launch)` (`design/gamemode-and-dedicated-sessions.md` A2) — a kept display
    /// running game A must never be handed to a session that asked to launch game B. Default `None`
    /// (backends that never nest a command); only gamescope reports its `cmd`.
    fn launch_command(&self) -> Option<String> {
        None
    }
    /// Is the kept display's `node_id` still live, checked **before** the registry REUSES it on a
    /// reconnect (`design/gamemode-and-dedicated-sessions.md` A2)? A `false` tells the registry to tear
    /// the dead entry down and create fresh instead of handing back a corpse (which would then fail
    /// capture and burn a retry). Default `true` (honest optimism — the [`mark_failed`] path is the
    /// backstop for a display that dies between this check and first frame). Only gamescope overrides
    /// it (its nested session dies when the game exits, independently of any compositor); KWin/Mutter
    /// nodes die only with their compositor, which the session-epoch invalidation (A4) already reaps.
    ///
    /// [`mark_failed`]: crate::registry::mark_failed
    fn kept_display_alive(&mut self, _node_id: u32) -> bool {
        true
    }
}
