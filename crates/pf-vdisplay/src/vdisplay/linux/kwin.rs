//! KWin virtual-output backend via the privileged `zkde_screencast_unstable_v1` Wayland
//! protocol (the mechanism KRdp / krfb-virtualmonitor use).
//!
//! `stream_virtual_output(name, width, height, scale, pointer)` asks KWin to create a new output
//! sized to exactly `width`x`height`, rendered natively (no scaling), and hands back a PipeWire
//! node for it. The node lives on the user's default PipeWire daemon, so [`VirtualOutput::remote_fd`]
//! is `None` and capture connects to that daemon directly.
//!
//! Requirements: KWin must expose the privileged `zkde_screencast` global. It is a *restricted*
//! protocol — KWin advertises it only to a client whose installed `.desktop` lists it under
//! `X-KDE-Wayland-Interfaces` (KWin maps the connecting client to a `.desktop` by resolving
//! `/proc/<pid>/exe` against `Exec=`, then caches the grant per-executable for the session's life).
//! So an interactive Plasma session does NOT hand it to a bare client — the host packages ship
//! `io.unom.Punktfunk.Host.desktop` (`Exec=/usr/bin/punktfunk-host`,
//! `X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1,…`) so it is present before the host first
//! connects. That identification is also why **the host binary must carry no file capability**: a
//! process holding capabilities KWin lacks is one the kernel will not let KWin resolve
//! `/proc/<pid>/exe` for, so it can never be matched to a `.desktop` no matter how correctly the
//! file is installed (see [`capability_denial_hint`]). The headless test path instead exposes it to
//! bare clients via `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. The compositor backend must implement
//! `createVirtualOutput`: the **DRM backend** (any version) or the **VirtualBackend since KWin
//! 6.5.6** (`kwin_wayland --virtual`); on `--virtual` < 6.5.6 the request fails with
//! "Could not find output". We talk raw Wayland on `$WAYLAND_DISPLAY`, so the host must run inside
//! the KWin session's environment.

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// Generate the client bindings for the vendored protocol XML inline (no build.rs). Path is
// relative to CARGO_MANIFEST_DIR. See wayland-rs' "implementing a custom protocol" docs.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod zkde {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/zkde-screencast-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/zkde-screencast-unstable-v1.xml");
}

use zkde::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1 as ScreencastStream,
};
use zkde::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1 as Screencast;

/// `pointer` attachment modes (the protocol enum), chosen per session by `set_hw_cursor`
/// (Phase B — the Windows no-regression gate mirrored): a CURSOR-CHANNEL session gets METADATA
/// (`SPA_META_Cursor` on the stream — shapes forwarded to the client, the composite flip blends
/// host-side; embedded would leave both with nothing, the round-1 mutter trap), every other
/// session gets EMBEDDED — KWin composites the pointer into frames itself, zero host-side
/// cursor work, the pre-channel path Moonlight/legacy clients always had.
const POINTER_METADATA: u32 = 4;
const POINTER_EMBEDDED: u32 = 2;

/// Marks the one KWin refusal a retry can clear: the disabled-output repair ran and changed the
/// box between attempts ([`kwin_output_mgmt::enable_disabled_output`]).
///
/// It is load-bearing in TWO places and both are easy to break. The opener keys on it to skip the
/// `KWin virtual output failed` wrapper below — and that wrapper's prefix is exactly what the
/// host's `is_permanent_build_error` matches to short-circuit the retry loop, so a repaired
/// refusal carrying it would be classified permanent and the retry that consumes the repair would
/// never run. It is also the human-readable half of the message; keep it a phrase, not a code.
const REPAIRED_HINT: &str = "enabled it over output management";

/// The name we give the created output; KWin exposes it to output-management as `Virtual-<name>`.
const VOUT_NAME: &str = "punktfunk";

/// Highest interface version we drive. KWin currently advertises 5; we rely on the `created`
/// event (deprecated only since v6) for the node id, so cap the bind at 5.
const MAX_VERSION: u32 = 5;

/// The KWin virtual-display driver. Carries the connecting client's cert fingerprint (set before
/// [`create`](VirtualDisplay::create)) so a paired client gets a STABLE per-slot output NAME
/// (`Virtual-punktfunk-<id>`) — KWin persists per-output config (scale/mode) keyed by name in
/// `kwinoutputconfig.json`, so a stable name makes KDE reapply that client's scaling on reconnect
/// (Stage 3). Each `create` spins up its own Wayland connection/thread that owns the output.
#[derive(Default)]
pub struct KwinDisplay {
    client_fp: Option<[u8; 32]>,
    /// The identity slot the last [`create`](VirtualDisplay::create) resolved (the per-client id, or
    /// `None` for shared/anonymous) — reported to the registry via [`last_identity_slot`] so it can key
    /// the group arrangement + `/display/state` slot to the same id this backend named the output with.
    last_slot: Option<u32>,
    /// The RESOLVED kscreen address of the last `create`'s output — the numeric kscreen output id
    /// when [`resolve_kscreen_addr`] found it, else the `Virtual-<name>` fallback — so
    /// [`apply_position`](VirtualDisplay::apply_position) addresses OUR output even while a
    /// superseded same-name sibling is still alive.
    last_name: Option<String>,
    /// The RESOLVED `kde_output_device_v2` UUID of the last `create`'s output, when the in-process
    /// output-management path handled the topology. A stable per-output id (unlike the shared name),
    /// so [`apply_position`](VirtualDisplay::apply_position) and restore address exactly OUR output
    /// across a supersede — preferred over `last_name`, which is only the kscreen-doctor fallback.
    our_uuid: Option<String>,
    /// The topology-restore action the last `create` prepared (re-enable the outputs an `exclusive`
    /// topology disabled), pending pickup by the registry via [`take_topology_restore`] — so the
    /// physical is re-enabled only when the display GROUP's last member drops (§6.1), not this session's.
    /// A backstop [`Drop`] runs it if the registry never took it (so a physical is never left dark).
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// Out-of-band cursor request (`set_hw_cursor`, i.e. the session negotiated the cursor
    /// channel): METADATA pointer mode at creation; off = EMBEDDED (see the consts above).
    hw_cursor: bool,
}

impl Drop for KwinDisplay {
    fn drop(&mut self) {
        // Backstop only: the registry takes the restore right after `create` (moving it into the group),
        // so this is normally `None`. If some path skipped the take, re-enable here so a physical is
        // never stranded dark.
        if let Some(restore) = self.pending_restore.take() {
            restore();
        }
    }
}

impl KwinDisplay {
    pub fn new() -> Result<Self> {
        Ok(KwinDisplay::default())
    }

    /// Apply the effective display topology for the just-created output `our_prefix` (current size
    /// `dims`), preferring the in-process `kde_output_management_v2` path and falling back to
    /// `kscreen-doctor` if the compositor doesn't answer in budget or the management global is
    /// absent. Records the output's UUID (in-process) or kscreen address (fallback) for
    /// [`apply_position`](VirtualDisplay::apply_position), and returns the disabled outputs (each
    /// `(name, "WxH@Hz")`) for the group teardown restore. `Extend`/`Auto` disable nothing.
    fn apply_topology(
        &mut self,
        name: &str,
        our_prefix: &str,
        dims: (u32, u32),
    ) -> Vec<(String, String)> {
        use crate::kwin_output_mgmt::TopologyKind;
        use crate::policy::Topology;
        let topology = crate::effective_topology();
        let kind = match topology {
            Topology::Exclusive => TopologyKind::Exclusive,
            Topology::Primary => TopologyKind::Primary,
            Topology::Extend | Topology::Auto => {
                // No topology to apply — but the output must still be its OWN desktop rather than a
                // mirror of someone's panel, and KWin restores a stored `replicationSource` onto our
                // (stable) output name for whatever monitor set it was saved under. Applies only if
                // it really is mirroring; nothing else about the user's arrangement is touched.
                crate::kwin_output_mgmt::clear_replication_source(our_prefix, dims.0, dims.1);
                return Vec::new();
            }
        };
        // In-process over Wayland — immune to whatever wedges the standalone kscreen-doctor.
        let outcome = crate::kwin_output_mgmt::apply_topology(our_prefix, dims.0, dims.1, kind);
        if outcome.handled {
            self.our_uuid = outcome.our_uuid;
            return outcome.disabled;
        }
        // Fallback: kscreen-doctor — resolve our address the old way, then shell out the topology.
        tracing::info!(
            "KWin topology: kde_output_management unavailable — kscreen-doctor fallback"
        );
        let addr = resolve_kscreen_addr(name, dims.0, dims.1);
        self.last_name = Some(addr.clone());
        match topology {
            Topology::Exclusive => apply_virtual_primary(&addr),
            Topology::Primary => {
                apply_virtual_primary_only(&addr);
                Vec::new()
            }
            Topology::Extend | Topology::Auto => Vec::new(),
        }
    }
}

impl VirtualDisplay for KwinDisplay {
    fn name(&self) -> &'static str {
        "kwin"
    }

    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn last_identity_slot(&self) -> Option<u32> {
        self.last_slot
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.pending_restore.take()
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn apply_position(&mut self, x: i32, y: i32) {
        // Prefer the in-process path: address OUR output by its stable UUID (supersede-robust) over
        // kde_output_management_v2 — immune to a wedged kscreen-doctor backend (see kwin_output_mgmt).
        if let Some(uuid) = self.our_uuid.clone() {
            if crate::kwin_output_mgmt::set_position(&uuid, x, y) {
                return;
            }
        }
        // Fallback: kscreen-doctor. `last_name` holds the RESOLVED kscreen address (numeric output id,
        // or the `Virtual-<name>` fallback) — never re-derive from the name: during a supersede two
        // outputs share it and the command would hit the old one (see `create`).
        let Some(output) = self.last_name.clone() else {
            return;
        };
        // kscreen-doctor position syntax: `output.<name-or-id>.position.<x>,<y>`.
        let ok = kscreen_ok(&[format!("output.{output}.position.{x},{y}")]);
        if ok {
            tracing::info!(output, x, y, "KWin: placed output in the desktop layout");
        } else {
            tracing::warn!(output, x, y, "KWin: output position apply failed");
        }
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // Per-slot output name (Stage 3): the `identity` policy resolves the client to a stable id →
        // `punktfunk-<id>` (KWin exposes `Virtual-punktfunk-<id>`, whose per-output config KWin
        // persists by name). Shared / anonymous → the base `punktfunk` (today's single name). Linux
        // defaults to Shared when unconfigured, so this is a no-op change until a policy opts in — AND
        // it fixes the latent clash where two concurrent sessions both used `Virtual-punktfunk`.
        let slot = crate::identity::resolve_slot(
            self.client_fp,
            (mode.width, mode.height),
            crate::policy::Identity::Shared,
        );
        self.last_slot = slot; // reported to the registry for the group arrangement + state slot
        let name = match slot {
            Some(id) => format!("{VOUT_NAME}-{id}"),
            None => VOUT_NAME.to_string(),
        };
        // `apply_position`'s kscreen-doctor fallback (the registry-driven §6.2 layout) addresses
        // `last_name`, so seed it with `Virtual-<name>`: the address KWin exposes our output under
        // and the ONLY spelling kscreen-doctor can resolve. The bare `name` we ask KWin for
        // (`punktfunk`) matches no output at all, so seeding it with that left every position apply
        // shelling out against an address that can never exist — and the `is_none()` guard that was
        // supposed to correct it later could never fire, because this write is never `None`.
        let our_prefix = format!("Virtual-{name}");
        self.last_name = Some(our_prefix.clone());
        // Every `create` re-resolves its own output, so the PREVIOUS one's UUID must not survive
        // into this one. A supersede keeps this `KwinDisplay` and creates the replacement while the
        // predecessor is still alive, so a stale UUID still RESOLVES: `set_position` would find the
        // old output, position it, report success, and never reach the fallback — the new display
        // silently stays where it was born. Re-set below only if the in-process path handles us.
        self.our_uuid = None;
        let (width, height) = (mode.width, mode.height);
        let pointer_mode = if self.hw_cursor {
            POINTER_METADATA
        } else {
            POINTER_EMBEDDED
        };
        let spawn_vout = |w: u32, h: u32| -> Result<(u32, Arc<AtomicBool>)> {
            let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let name_thread = name.clone();
            thread::Builder::new()
                .name("punktfunk-kwin-vout".into())
                .spawn(move || {
                    virtual_output_thread(w, h, name_thread, pointer_mode, setup_tx, stop_thread)
                })
                .context("spawn KWin virtual-output thread")?;
            match setup_rx.recv_timeout(OPENER_BUDGET) {
                Ok(Ok(v)) => Ok((v, stop)),
                // Repaired: report it as-is. The wrapper below would prepend the phrase the host
                // reads as "permanent, do not retry", and this is the one refusal whose retry is
                // the entire point — the repair only fixes the NEXT request.
                Ok(Err(e)) if e.contains(REPAIRED_HINT) => bail!("{e}"),
                // KWin's reason is TRANSLATED into the session's language, so it is often
                // unsearchable for the person reading the log. Say what it means once, here.
                Ok(Err(e)) => bail!(
                    "KWin virtual output failed: {e} — KWin declined to create the output. It \
                     needs a Plasma WAYLAND session on KWin's DRM backend; a nested or \
                     `kwin_wayland --virtual` KWin can only do this since 6.5.6, and on KWin 6.6+ \
                     an output KWin creates but leaves DISABLED (stored \
                     ~/.config/kwinoutputconfig.json, or a display config it refused to apply) \
                     reports the same. kwin_wayland's own journal says which"
                ),
                Err(_) => {
                    // Nothing else will ever flip this `stop`: it is dropped with the error, and
                    // the `StopGuard` that normally owns it is only built on the success path. So
                    // the worker — which is by construction still inside `await_created` — would
                    // sit out its own budget holding a half-built output whose Wayland connection
                    // KWin keeps the output alive for. Release it here.
                    stop.store(true, Ordering::Relaxed);
                    bail!("timed out creating the KWin virtual output")
                }
            }
        };
        // KWin creates virtual outputs at a hardcoded 60 Hz, `stream_virtual_output` has no
        // refresh argument — and its screencast stream builds its PipeWire format offer, INCLUDING
        // the `maxFramerate` cap it actively throttles delivery to, ONCE at stream creation
        // (screencaststream.cpp `buildFormats`, verified against the live offer with `pw-dump`:
        // the offer stays `max=60` after a kscreen mode change, and consumers connecting later
        // still negotiate against it). The ONLY path that rebuilds the offer is the stream's own
        // resize handling: when the source's texture size changes while recording, KWin re-runs
        // `buildFormats` — picking up the output's CURRENT refresh — and renegotiates the live
        // stream via `pw_stream_update_params`. So above 60 Hz the output is born at a
        // SACRIFICIAL height: installing + selecting the real high-refresh custom mode (supported
        // on virtual outputs since KWin 6.6) then changes the SIZE, and the first buffers recorded
        // after the consumer connects trigger KWin's resize → a renegotiation to that mode. The
        // capturer holds frames until that lands (`expect_exact_dims`), so the pipeline never
        // builds against the birth mode. The install/select runs in-process over
        // kde_output_management_v2 (`kwin_output_mgmt::set_custom_mode`), with kscreen-doctor
        // (`set_custom_refresh`) as the fallback; either reads back what KWin *actually* gave — both
        // the rate (so the encoder paces to the real source) and the size, which KWin's CVT
        // generator may have aligned down (see `CVT_H_GRANULARITY`). At ≤60 Hz there's nothing to
        // install — the output is born at the real size and 60 Hz is the offer anyway.
        let want_high = mode.refresh_hz > 60;
        let birth_h = if want_high { height + 16 } else { height };
        let (mut node_id, mut stop) = spawn_vout(width, birth_h)?;
        // `requested_*`, NOT `width`/`height`: `spawn_vout` hands back a node id, never a size, so
        // every number on this line is what we ASKED for. Logged as `width=… height=…` it read like
        // a readback of what KWin built, and a field report where KWin had actually built a 1080p
        // output was diagnosed against a log line stating 3840x2160. The readback is below.
        tracing::info!(
            node_id,
            requested_w = width,
            requested_h = height,
            birth_h,
            embedded_pointer = !self.hw_cursor,
            "KWin virtual output ready"
        );
        // Topology + positioning address OUR output by its kde_output_management UUID (resolved
        // in-process in `apply_topology`, supersede-robust) — no early kscreen-doctor resolve, so
        // the path never shells out. `our_prefix` (computed above with `last_name`) is the name
        // KWin exposes our output as.
        let mut expect_exact_dims = false;
        // The size the output actually ENDS UP at — the request, unless KWin's CVT generator had to
        // shrink the width to the cell grain (see `CVT_H_GRANULARITY`). Reported as the output's
        // `preferred_mode`, which is what the capturer's renegotiation gate waits for and what the
        // encoder opens against, so a CVT-aligned mode flows end-to-end instead of starving.
        let mut final_dims = (width, height);
        let achieved_hz = if want_high {
            // >60 Hz needs the real high-refresh custom mode installed + selected (sacrificial-birth,
            // see above). In-process over kde_output_management_v2 first (no kscreen-doctor); fall
            // back to the kscreen-doctor shell-out on pre-6.6 KWin (no `set_custom_modes`) or if the
            // compositor doesn't answer in budget.
            let active = crate::kwin_output_mgmt::set_custom_mode(
                &our_prefix,
                width,
                birth_h,
                width,
                height,
                mode.refresh_hz,
            )
            .or_else(|| {
                // ⚠️ ADDRESS BY NUMERIC KSCREEN ID, NEVER BY NAME: a supersede reuses the per-slot
                // name while the superseded sibling is still alive, so a name-addressed kscreen
                // command hits the OLD output. Resolve our kscreen id for the shell-out install +
                // keep it as apply_position's kscreen fallback.
                let addr = resolve_kscreen_addr(&name, width, birth_h);
                self.last_name = Some(addr.clone());
                set_custom_refresh(width, height, mode.refresh_hz, &addr)
            });
            // Accept only an active mode that IS our custom one: the exact requested height, and a
            // width at or just below the request (a CVT alignment). That also proves the output
            // left the sacrificial birth size, so the recording stream will renegotiate to it.
            match active {
                Some((aw, ah, ahz)) if mode_satisfies((aw, ah), width, height) => {
                    expect_exact_dims = true;
                    final_dims = (aw, ah);
                    ahz
                }
                other => {
                    // Custom-mode install/select rejected (pre-6.6 KWin / stale kscreen-doctor): the
                    // output is STUCK at the sacrificial birth size — unusable. Recreate plain at the
                    // real size (the pre-sacrifice behavior: correct size, KWin's native 60 Hz).
                    tracing::warn!(
                        active = ?other,
                        requested_w = width,
                        requested_h = height,
                        requested_hz = mode.refresh_hz,
                        "KWin rejected the custom mode — recreating the virtual output at the real \
                         size (60 Hz ceiling on this KWin)"
                    );
                    stop.store(true, Ordering::Relaxed);
                    // Let KWin retire the doomed output before re-using its name.
                    std::thread::sleep(Duration::from_millis(300));
                    let (nid, st) = spawn_vout(width, height)?;
                    node_id = nid;
                    stop = st;
                    tracing::info!(
                        node_id,
                        width,
                        height,
                        "KWin virtual output ready (fallback)"
                    );
                    60
                }
            }
        } else {
            // ≤60 Hz installs no mode, so nothing here ever learned what KWin actually built — and
            // KWin does not necessarily build what it was asked for. `OutputConfigurationStore`
            // restores per-output mode AND scale from `kwinoutputconfig.json` keyed by output NAME,
            // and ours is stable across sessions by design (Stage 3, so KDE reapplies that client's
            // scaling) — so a slot that last ran at 1080p gets 1080p put back on top of the 4K we
            // just requested. The >60 Hz arm above is immune only incidentally: it installs a mode,
            // so it gets a readback for free.
            //
            // Unverified, that mismatch is silent and total. The capture builds at KWin's size, the
            // encoder opens against it, and Moonlight — which configured its decoder for the size it
            // negotiated over RTSP — receives a bitstream it cannot decode, asks for a keyframe
            // every ~50 ms, and drops the session. Meanwhile every dims-keyed resolve below
            // (`apply_topology`, `clear_replication_source`, `resolve_kscreen_addr`) is looking for
            // an output at the requested size and quietly finding nothing, so the stream isn't even
            // made primary or de-mirrored.
            match crate::kwin_output_mgmt::actual_dims(&our_prefix) {
                // KWin honoured the request — the overwhelmingly common case. No configuration is
                // built and nothing is applied: byte-for-byte the behaviour this arm always had.
                //
                // The scale is recorded rather than corrected. A non-1.0 scale here is NOT a fault
                // to repair: the stable output name exists precisely so KDE reapplies this client's
                // scaling on reconnect (Stage 3), so forcing the 1.0 we asked `stream_virtual_output`
                // for would undo a feature. It is logged because it is the other half of the stored
                // per-output config, and because the pixel-vs-logical question it raises is exactly
                // what a future "the size is right but the capture is halved" report will turn on —
                // KWin's output screencast streams the source's PIXEL size, so a scale should not
                // move the captured dimensions, and a report showing otherwise would be the evidence
                // that assumption is wrong on some KWin version.
                Some((aw, ah, _, scale)) if (aw, ah) == (width, height) => {
                    if scale != 1.0 {
                        tracing::debug!(
                            width,
                            height,
                            scale,
                            "KWin virtual output verified at the requested size, carrying a stored \
                             non-unity scale (per-client scaling — capture is unaffected)"
                        );
                    }
                }
                Some((aw, ah, _, scale)) => {
                    tracing::warn!(
                        actual_w = aw,
                        actual_h = ah,
                        requested_w = width,
                        requested_h = height,
                        stored_scale = scale,
                        our_prefix,
                        "KWin built our virtual output at a DIFFERENT size than requested (a stored \
                         kwinoutputconfig.json mode/scale for this output name) — re-asserting the \
                         requested mode so the stream matches what the client negotiated"
                    );
                    // Re-assert the requested size through the SAME install+select the sacrificial
                    // birth uses above: an output sitting at a size we don't want, moved to the one
                    // we do, with the screencast stream renegotiating to it on the first buffers
                    // recorded after the consumer connects. `aw`/`ah` play the birth size — that is
                    // literally what they are here, just not deliberately.
                    //
                    // 60 Hz, NOT `mode.refresh_hz`: this arm is ≤60 Hz by construction and only the
                    // SIZE is wrong. Asking for the client's rate would install a 30 Hz mode for a
                    // 30 fps client and throttle the compositor to it — a behaviour change fixing a
                    // size has no business making. KWin's virtual outputs are 60 Hz natively and
                    // `achieved_hz` below stays the client's rate exactly as before.
                    match crate::kwin_output_mgmt::set_custom_mode(
                        &our_prefix,
                        aw,
                        ah,
                        width,
                        height,
                        60,
                    ) {
                        // Same acceptance test as the high-refresh arm — literally, so the two can
                        // never drift. That the mode moved at all also proves the screencast will
                        // renegotiate, which is what `expect_exact_dims` then waits for.
                        Some((cw, ch, _)) if mode_satisfies((cw, ch), width, height) => {
                            expect_exact_dims = true;
                            final_dims = (cw, ch);
                            tracing::info!(
                                active_w = cw,
                                active_h = ch,
                                "KWin virtual output corrected to the requested size"
                            );
                        }
                        other => {
                            // Correction refused (pre-6.6 KWin has no `set_custom_modes`, or the
                            // compositor didn't answer). Report the size that is REALLY there, not
                            // the one we asked for: the dims-keyed resolves below and the encoder
                            // all key on `final_dims`, and carrying the request forward is what
                            // made this a silent failure rather than a degraded one. The session
                            // still runs, at KWin's size: the stream layer warns that the client is
                            // decoding something other than what it negotiated but does NOT refuse
                            // it, because a monitor mirror legitimately streams a size the client
                            // never asked for (§7.3) and failing here would break every one.
                            tracing::warn!(
                                active = ?other,
                                actual_w = aw,
                                actual_h = ah,
                                requested_w = width,
                                requested_h = height,
                                "KWin would not re-assert the requested mode — the output is STUCK \
                                 at its stored size. Clear this output's entry from \
                                 kwinoutputconfig.json (or set it to the streamed resolution in \
                                 System Settings → Display) and reconnect"
                            );
                            final_dims = (aw, ah);
                        }
                    }
                }
                // Management unavailable, or two outputs share our name (a supersede in flight, the
                // one case only a dims-keyed resolve can disambiguate). Nothing verifiable to act
                // on, so carry on exactly as this arm always did rather than reconfigure an output
                // we cannot identify.
                None => {
                    tracing::debug!(
                        our_prefix,
                        "KWin: could not read back the virtual output's actual mode (management \
                         unavailable or a same-named supersede in flight) — proceeding unverified"
                    );
                }
            }
            mode.refresh_hz
        };
        // Display-management topology (Stage 2): `Extend` leaves the streamed output an extension;
        // `Primary` makes it the primary output but keeps the bootstrap/physical outputs enabled;
        // `Exclusive` makes it the SOLE desktop (others disabled, restored on teardown) — so
        // plasmashell + windows land on the streamed surface, not the headless `kwin --virtual`
        // bootstrap output. Applied over kde_output_management_v2 in-process (immune to a wedged
        // kscreen-doctor backend; see `apply_topology`), with a kscreen-doctor fallback. `disabled`
        // is the physical/bootstrap outputs, each `(name, "WxH@Hz")`, to restore on teardown.
        let disabled = self.apply_topology(&name, &our_prefix, final_dims);
        // `last_name` is already the best address we have: `Virtual-<name>` from the top of this
        // function, upgraded in place to the RESOLVED numeric kscreen id by whichever of the
        // `want_high` fallback or `apply_topology`'s fallback actually ran a resolve. Nothing to
        // fill in here — the guard that used to sit at this spot could never fire (`last_name` is
        // written unconditionally above) and only made the plain-name case look handled.
        // Per-group restore (§6.1): DON'T bind the re-enable to this session's keepalive (a per-session
        // `StopGuard` restore would re-enable the physical the moment the FIRST of several exclusive
        // sessions drops — under a still-live sibling). Instead stash it as a closure the registry lifts
        // into the display group and runs once, when the group's LAST member is torn down (ordered before
        // that display's output is reclaimed, so KWin never sees zero outputs). Empty ⇒ nothing to restore.
        self.pending_restore = (!disabled.is_empty()).then(|| {
            let disabled = disabled.clone();
            // In-process first; fall back to kscreen-doctor if the compositor doesn't answer in
            // budget. **Both halves now return honest verdicts** — `reenable_outputs` reports
            // `false` unless every requested output was actually staged (an empty configuration
            // used to ack as `applied` and suppress this backstop), and
            // `reenable_outputs_kscreen` branches on its own exit status instead of logging
            // success unconditionally. Any future extraction of these hand-rolled
            // in-process-then-kscreen ladders into one facade must keep that property: a fallback
            // arm that returns a value the helper never checked would re-introduce exactly the
            // silent-success this pair was fixed for, behind a seam that claims to have one log
            // site for every decline.
            Box::new(move || {
                if !crate::kwin_output_mgmt::reenable_outputs(&disabled) {
                    reenable_outputs_kscreen(&disabled);
                }
            }) as Box<dyn FnOnce() + Send>
        });
        // Layout position (§6.2) is applied by the registry via `apply_position` right after create
        // (it owns the display group, so it computes auto-row / manual placement over the whole group).
        let mut out = VirtualOutput::owned(
            node_id,
            Some((final_dims.0, final_dims.1, achieved_hz)),
            Box::new(StopGuard { stop }),
        );
        out.expect_exact_dims = expect_exact_dims;
        Ok(out)
    }
}

/// Re-enable the outputs an `exclusive` topology disabled (bootstrap / physical) via `kscreen-doctor`
/// — the fallback for the in-process [`crate::kwin_output_mgmt::reenable_outputs`], run by the restore
/// closure only when the in-process path reports the compositor didn't answer. Called by the registry
/// when the display group's last member is torn down (design §6.1), BEFORE that member's output is
/// reclaimed — so KWin is never momentarily left with zero enabled outputs.
///
/// **This is the last line of defence for a physical monitor**, so it reports what actually
/// happened. It used to discard both `kscreen_ok` verdicts and log restored-everything
/// unconditionally — including when the call had been killed at [`KSCREEN_BUDGET`], i.e. exactly
/// the wedged compositor this fallback exists for, with a screen left dark and a green line in the
/// log saying otherwise.
///
/// Reporting honestly is not the same as *stopping* on a bad verdict, and the difference is
/// [`kscreen_verdict`]'s third state: a helper killed at its budget has told us nothing, and this
/// path must go on to the mode re-assert and the settle in that case exactly as the pre-verdict
/// code did — see the `None` arm below for what skipping them costs.
fn reenable_outputs_kscreen(outputs: &[(String, String)]) {
    if outputs.is_empty() {
        return;
    }
    // Enable FIRST, as a standalone apply — a bare `output.X.enable` always succeeds, so a physical
    // can never be left DARK. (Batching a possibly-stale `mode` arg into the same invocation risks
    // kscreen-doctor rejecting the whole config and leaving the output disabled.)
    let enable_args: Vec<String> = outputs
        .iter()
        .map(|(name, _)| format!("output.{name}.enable"))
        .collect();
    let enable_verdict = kscreen_verdict(&enable_args);
    match enable_verdict {
        // It ran and it refused (or could not be run at all). Nothing further to try: both the
        // in-process path and this one have now declined, so the outputs stay as `exclusive` left
        // them. Say so loudly — a dark monitor with no line in the log is what this whole restore
        // chain exists to prevent.
        Some(false) => {
            tracing::error!(
                outputs = ?outputs,
                args = ?enable_args,
                "KWin: could NOT re-enable the physical/bootstrap outputs (kscreen-doctor refused \
                 the config, or could not be run, after the in-process restore already declined) — \
                 a monitor may be left dark"
            );
            return;
        }
        // Killed at [`KSCREEN_BUDGET`] — which is NOT the same as a refusal, and treating it as one
        // is a regression this path already had once. kscreen-doctor applies the config and only
        // THEN waits on the compositor before exiting, so a loaded KWin routinely lands the enable
        // and still gets killed: the output is lit, and returning here would skip both halves of
        // the rest of the restore — the mode re-assert (a 120 Hz panel comes back at the
        // EDID-preferred ~60 Hz without it) and the 200 ms settle that keeps KWin from seeing zero
        // enabled outputs when the caller reclaims the virtual one right after us (§6.1). The
        // second budget this costs on the stream thread is deliberate and bounded, and is what the
        // pre-`match` code spent unconditionally.
        None => tracing::warn!(
            outputs = ?outputs,
            args = ?enable_args,
            "KWin: kscreen-doctor was killed at its budget re-enabling the physical/bootstrap \
             outputs — the apply may well have landed, so continuing with the mode restore"
        ),
        Some(true) => {}
    }
    // THEN re-assert each captured mode, best-effort — a bare re-enable lets KWin fall back to the
    // EDID-preferred mode (a 120 Hz panel returns at ~60 Hz); this restores the exact refresh. The
    // output is enabled now, so the mode set is valid; a rejected mode just leaves KWin's default —
    // a wrong refresh, not a dark screen, which is why only this half degrades to a warn.
    let mode_args: Vec<String> = outputs
        .iter()
        .filter(|(_, mode)| !mode.is_empty())
        .map(|(name, mode)| format!("output.{name}.mode.{mode}"))
        .collect();
    let modes_restored = mode_args.is_empty() || kscreen_ok(&mode_args);
    std::thread::sleep(Duration::from_millis(200));
    // `enable_confirmed` rides along on both lines: after a budget kill the enable is *probable*,
    // not established, and a log that cannot tell the operator which of the two it is put us here
    // in the first place.
    let enable_confirmed = enable_verdict == Some(true);
    if modes_restored {
        tracing::info!(reenabled = ?outputs, enable_confirmed, "KWin: restored the physical/bootstrap outputs at their captured modes (group empty)");
    } else {
        tracing::warn!(
            reenabled = ?outputs,
            args = ?mode_args,
            enable_confirmed,
            "KWin: re-enabled the physical/bootstrap outputs but could not re-assert their captured \
             modes — they are back at KWin's preferred refresh, not the one they were streaming at"
        );
    }
}

/// Resolve the kscreen address of the virtual output the host JUST created: the managed-prefix
/// name alone is ambiguous during a supersede (the replacement deliberately reuses the per-slot
/// name while the superseded sibling is still alive), so match on the birth mode's size too —
/// only the just-created output sits at the sacrificial `(w, h)` — and prefer the HIGHEST output
/// id (the newest) if several match. Returns the numeric id as a string (kscreen-doctor accepts
/// `output.<id>.…`), falling back to the ambiguous `Virtual-<name>` if the output hasn't reached
/// kscreen's model yet after a few tries (single-output sessions are unambiguous anyway).
fn resolve_kscreen_addr(name: &str, w: u32, h: u32) -> String {
    let fallback = format!("Virtual-{name}");
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(150));
        }
        let Some(doc) = kscreen_json() else { continue };
        let Some(outputs) = doc.get("outputs").and_then(|o| o.as_array()) else {
            continue;
        };
        let best = outputs
            .iter()
            .filter(|o| {
                o.get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(&fallback))
                    && output_active_size(o) == Some((w, h))
            })
            .filter_map(|o| o.get("id").and_then(|i| i.as_u64()))
            .max();
        if let Some(id) = best {
            tracing::info!(id, name, w, h, "KWin: resolved the new output's kscreen id");
            return id.to_string();
        }
    }
    tracing::warn!(
        name,
        w,
        h,
        "KWin: could not resolve the new output's kscreen id — falling back to name addressing \
         (ambiguous during a mode-switch supersede)"
    );
    fallback
}

/// Budget for one `kscreen-doctor` call.
///
/// It is a Wayland client of the very compositor it configures, so against a wedged KWin it blocks
/// in its own connect and never returns — and these calls run on the session's stream thread, whose
/// only way to end a session is to return. Generous next to a healthy call (tens of ms).
const KSCREEN_BUDGET: Duration = Duration::from_secs(5);

/// `kscreen-doctor <args>` run for its exit status, bounded by [`KSCREEN_BUDGET`]. A timeout reads
/// as a failed apply — the same best-effort path a rejected argument already takes.
fn kscreen_ok(args: &[String]) -> bool {
    kscreen_verdict(args) == Some(true)
}

/// The same call, keeping the outcome that [`kscreen_ok`]'s `bool` throws away.
///
/// `Some(true)`/`Some(false)`: kscreen-doctor ran to completion and accepted / refused (a helper
/// that cannot be spawned at all counts as a refusal — there is nothing to wait for and no reason
/// to retry the next invocation). `None`: it was **killed at [`KSCREEN_BUDGET`]**, which is a
/// different fact entirely. kscreen-doctor applies the config and then waits on the compositor
/// before exiting, so a slow-but-working KWin gives us a kill on a request that already landed;
/// any caller that treats `None` as "it failed" is asserting something it does not know, and for
/// the restore path that assertion costs a monitor its refresh rate.
pub(crate) fn kscreen_verdict(args: &[String]) -> Option<bool> {
    match crate::proc::status_within(
        std::process::Command::new("kscreen-doctor").args(args),
        KSCREEN_BUDGET,
    ) {
        Ok(status) => Some(status.success()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => None,
        Err(_) => Some(false),
    }
}

/// `kscreen-doctor -j` stdout, bounded by [`KSCREEN_BUDGET`]; `None` on any failure.
fn kscreen_json_bytes() -> Option<Vec<u8>> {
    crate::proc::output_within(
        std::process::Command::new("kscreen-doctor").arg("-j"),
        KSCREEN_BUDGET,
    )
    .ok()
    .map(|o| o.stdout)
}

/// `kscreen-doctor -j` parsed, `None` on any failure.
fn kscreen_json() -> Option<serde_json::Value> {
    serde_json::from_slice(&kscreen_json_bytes()?).ok()
}

/// The CURRENT mode of an output from its `kscreen-doctor -j` entry, as `(width, height,
/// refresh_mHz)`. `None` if the entry names no current mode or that mode carries no size; a mode
/// with no `refreshRate` reports 0 mHz, which is the "unknown" the monitor type documents.
fn output_active_mode(o: &serde_json::Value) -> Option<(u32, u32, u32)> {
    let current = o.get("currentModeId").and_then(json_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(json_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    // Hz → mHz without an intermediate round: `refreshRate` is a float (59.94, 119.92) and whole
    // Hz would throw away exactly the distinction `PhysicalMonitor::refresh_mhz` exists to keep.
    let mhz = mode
        .get("refreshRate")
        .and_then(|r| r.as_f64())
        .map(|hz| (hz * 1000.0).round().max(0.0) as u32)
        .unwrap_or(0);
    Some((w, h, mhz))
}

/// The `(width, height)` of an output's CURRENT mode from its `kscreen-doctor -j` entry.
fn output_active_size(o: &serde_json::Value) -> Option<(u32, u32)> {
    output_active_mode(o).map(|(w, h, _)| (w, h))
}

/// Every head KWin reports, for [`crate::monitors::list`] — the in-process enumerate
/// ([`crate::kwin_output_mgmt::list_monitors`]) with a `kscreen-doctor -j` fallback.
///
/// This was the ONE KWin call site with no fallback at all, while the in-process session it depends
/// on declines for exactly the reasons the other five fall back for: management global absent
/// (pre-6.x KWin), or a compositor that does not answer in budget. The console's monitor picker and
/// `PUNKTFUNK_CAPTURE_MONITOR`'s resolve then failed outright on a box whose `kscreen-doctor` was
/// perfectly able to answer — and a failed `list` is not "no monitors", it is a session that
/// refuses to start (`monitors::resolve` treats a miss as a hard error, deliberately).
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let declined = match crate::kwin_output_mgmt::list_monitors() {
        Ok(monitors) => return Ok(monitors),
        Err(e) => e,
    };
    let Some(doc) = kscreen_json() else {
        return Err(declined.context(
            "kscreen-doctor -j did not answer either (not installed, or killed at its budget)",
        ));
    };
    let monitors = monitors_from_kscreen_json(&doc);
    tracing::info!(
        count = monitors.len(),
        reason = %declined,
        "KWin: enumerated monitors via kscreen-doctor (in-process output management declined)"
    );
    Ok(monitors)
}

/// Parse `kscreen-doctor -j` into the shared monitor type. Split from the process call so it can be
/// tested against captured JSON — the mapping is where a picker's identity keys come from, and
/// `x`/`y` are what make two same-sized heads distinguishable at all.
///
/// Deliberately mirrors the in-process reader's contract: a disabled output has no current mode and
/// reports zeroed geometry rather than an invented one, `primary` accepts either the modern
/// `priority: 1` or the older `primary: true`, and the list is sorted by desktop position so it
/// reads left-to-right the way the desk looks.
fn monitors_from_kscreen_json(doc: &serde_json::Value) -> Vec<crate::monitors::PhysicalMonitor> {
    let Some(outputs) = doc.get("outputs").and_then(|o| o.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<crate::monitors::PhysicalMonitor> = outputs
        .iter()
        .filter_map(|o| {
            let connector = o.get("name").and_then(|n| n.as_str())?.to_string();
            let mode = output_active_mode(o);
            let coord = |k: &str| {
                o.get("pos")
                    .and_then(|p| p.get(k))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32
            };
            Some(crate::monitors::PhysicalMonitor {
                managed: connector.starts_with(MANAGED_PREFIX),
                description: crate::monitors::describe(
                    o.get("vendor").and_then(|v| v.as_str()).unwrap_or(""),
                    o.get("model").and_then(|v| v.as_str()).unwrap_or(""),
                    &connector,
                ),
                width: mode.map(|m| m.0).unwrap_or(0),
                height: mode.map(|m| m.1).unwrap_or(0),
                refresh_mhz: mode.map(|m| m.2).unwrap_or(0),
                x: coord("x"),
                y: coord("y"),
                scale: o
                    .get("scale")
                    .and_then(|v| v.as_f64())
                    .filter(|s| *s > 0.0)
                    .unwrap_or(1.0),
                primary: o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1),
                enabled: o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false),
                connector,
            })
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    out
}

/// CVT's horizontal cell granularity. KWin generates every custom mode's timing with **libxcvt**,
/// whose first step is `hdisplay_rnd = hdisplay - (hdisplay % 8)` — so a width that isn't a multiple
/// of 8 comes back NARROWER than asked, and the clock-step rounding that follows lands a fractional
/// refresh. A 2868x1320@120 request (an iPhone 16 Pro Max panel) becomes **2864x1320@119.92**.
///
/// That is why a custom mode must never be selected by the `WxH@Hz` string we *requested*:
/// kscreen-doctor's `findMode` matches a mode's id or its own `WxH@qRound(Hz)` name, so
/// `2868x1320@120` matches nothing, the select silently no-ops, the output stays on its sacrificial
/// birth mode, and the caller falls back to 60 Hz — while KDE's display list shows the perfectly
/// good 2864x1320@119.92 mode sitting there unselected. Widths like 1920/2560/3840 are all
/// multiples of 8, which is why only phone-shaped clients ever hit it.
///
/// Shared with [`crate::kwin_output_mgmt`], which matches the generated mode back the same way —
/// it used to keep its own copy under a comment claiming the two "match", which is a claim no
/// compiler was checking.
pub(crate) const CVT_H_GRANULARITY: u32 = 8;

/// Does the mode that actually went ACTIVE satisfy a request for `want_w`×`want_h`?
///
/// Exact height, and a width at or just below the request — never an exact width, because KWin
/// generates custom timings through libxcvt and that rounds the width DOWN to the cell grain
/// ([`CVT_H_GRANULARITY`]). Demanding an exact width would reject the very mode we just asked KWin
/// to build, for phone-shaped clients (see the constant's note).
///
/// Both arms of [`VirtualDisplay::create`] that put a mode on the output test their readback
/// through here — the sacrificial high-refresh birth, and the correction for a size KWin restored
/// from its stored per-output config. They are the same question and they were, briefly, two copies
/// of the same expression; one place to change it is the point.
///
/// A width ABOVE the request fails: `aw <= want_w` guards the subtraction on the next line, and a
/// mode wider than we asked for is not a CVT alignment of our request — it is somebody else's mode.
fn mode_satisfies(active: (u32, u32), want_w: u32, want_h: u32) -> bool {
    let (aw, ah) = active;
    ah == want_h && aw <= want_w && want_w - aw < CVT_H_GRANULARITY
}

/// One row of an output's mode list, as parsed from `kscreen-doctor -j`.
#[derive(Clone, Debug, PartialEq)]
struct KModeRow {
    /// kscreen's mode id — what we address the mode by (never the requested `WxH@Hz` string).
    id: String,
    w: u32,
    h: u32,
    hz: f64,
}

/// A kscreen JSON id, which is a string on some KWin versions and a number on others.
fn json_id(v: &serde_json::Value) -> Option<String> {
    v.as_str()
        .map(|s| s.to_string())
        .or_else(|| v.as_u64().map(|n| n.to_string()))
}

/// The full mode list of `output` (a RESOLVED kscreen address — numeric id or name) from a parsed
/// `kscreen-doctor -j` document. Split from the process call so the picker can be tested on
/// captured JSON.
fn modes_from_json(doc: &serde_json::Value, output: &str) -> Vec<KModeRow> {
    let Some(o) = doc
        .get("outputs")
        .and_then(|v| v.as_array())
        .and_then(|outs| {
            outs.iter().find(|o| {
                o.get("name").and_then(|n| n.as_str()) == Some(output)
                    || o.get("id").and_then(json_id).as_deref() == Some(output)
            })
        })
    else {
        return Vec::new();
    };
    o.get("modes")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .filter_map(|m| {
                    let size = m.get("size")?;
                    Some(KModeRow {
                        id: m.get("id").and_then(json_id)?,
                        w: size.get("width").and_then(|v| v.as_u64())? as u32,
                        h: size.get("height").and_then(|v| v.as_u64())? as u32,
                        hz: m.get("refreshRate").and_then(|r| r.as_f64())?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// [`modes_from_json`] against a live `kscreen-doctor -j`.
fn output_modes(output: &str) -> Vec<KModeRow> {
    kscreen_json()
        .map(|doc| modes_from_json(&doc, output))
        .unwrap_or_default()
}

/// The mode in `modes` that actually fulfils a `width`x`height`@`hz` request, tolerating the CVT
/// alignment KWin applies when it generates the timing (see [`CVT_H_GRANULARITY`]): the height must
/// match exactly (CVT never touches the vertical active), the width may be up to one cell narrower
/// than asked (never wider — that would be a different mode), and the refresh must land within 1 Hz
/// of the request (which excludes the output's native 60 Hz entry for every rate we install a custom
/// mode for). Widest wins, then fastest — so an exact-width mode always beats an aligned one, and a
/// list carrying duplicate custom modes from earlier sessions still resolves.
fn pick_custom_mode(modes: &[KModeRow], width: u32, height: u32, hz: u32) -> Option<&KModeRow> {
    modes
        .iter()
        .filter(|m| {
            m.h == height
                && m.w <= width
                && width - m.w < CVT_H_GRANULARITY
                && (m.hz - f64::from(hz)).abs() < 1.0
        })
        .max_by(|a, b| {
            a.w.cmp(&b.w)
                .then(a.hz.partial_cmp(&b.hz).unwrap_or(std::cmp::Ordering::Equal))
        })
}

/// Best-effort: install + select the `width`x`height`@`hz` custom mode on the just-created virtual
/// output via `kscreen-doctor` (`output` is the RESOLVED kscreen address — numeric id or name, see
/// [`resolve_kscreen_addr`] — refresh given in mHz), then **read back the active mode** and return
/// it as `(width, height, refresh_hz)`. `None` if the read-back failed entirely.
///
/// The apply command can report success yet leave the output on its old mode (rejected), and a
/// silent size/rate mismatch surfaces downstream as a starved capture gate or judder — so the
/// caller drives the pipeline off the *achieved* mode, not the requested one. The mode is selected
/// by kscreen **mode id** resolved from the output's own list, never by the requested `WxH@Hz`
/// string, because KWin's CVT generator may hand back a slightly different one
/// ([`CVT_H_GRANULARITY`]).
fn set_custom_refresh(width: u32, height: u32, hz: u32, output: &str) -> Option<(u32, u32, u32)> {
    let output = output.to_string();
    let mhz = hz.saturating_mul(1000);
    let run = |arg: String| kscreen_ok(&[arg]);
    // Install the mode only if the output doesn't already carry a usable one: kscreen-doctor
    // APPENDS to the output's custom-mode list and KWin PERSISTS that list per output name
    // (`kwinoutputconfig.json`, which is why the same per-slot name is reused across sessions) — so
    // re-adding on every connect would grow the user's display list without bound.
    let mut modes = output_modes(&output);
    if pick_custom_mode(&modes, width, height, hz).is_none() {
        let _ = run(format!(
            "output.{output}.addCustomMode.{width}.{height}.{mhz}.full"
        ));
        modes = output_modes(&output);
    }
    let applied = match pick_custom_mode(&modes, width, height, hz) {
        Some(target) => {
            if (target.w, target.h) != (width, height) {
                tracing::info!(
                    output,
                    requested_w = width,
                    requested_h = height,
                    mode_w = target.w,
                    mode_h = target.h,
                    mode_hz = target.hz,
                    "KWin aligned the custom mode to the CVT cell grain — streaming at its size"
                );
            }
            // By id first; the human `WxH@Hz` form (built from the mode's OWN size/refresh, not the
            // request) is the fallback for builds whose ids don't round-trip through the CLI.
            run(format!("output.{output}.mode.{}", target.id))
                || run(format!(
                    "output.{output}.mode.{}x{}@{}",
                    target.w,
                    target.h,
                    target.hz.round() as u32
                ))
        }
        None => {
            tracing::warn!(
                output,
                requested_w = width,
                requested_h = height,
                requested_hz = hz,
                offered = ?modes,
                "KWin offers no mode matching the request after addCustomMode — is kscreen-doctor \
                 up to date, and KWin ≥ 6.6 (custom modes on virtual outputs)?"
            );
            false
        }
    };
    match read_active_mode(&output) {
        Some((w, h, achieved)) => {
            if achieved >= hz && (w, h) == (width, height) {
                tracing::info!(
                    output,
                    requested = hz,
                    achieved,
                    "KWin virtual output: custom refresh applied"
                );
            } else if achieved >= hz {
                tracing::info!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    "KWin virtual output: custom refresh applied at a CVT-aligned size"
                );
            } else {
                tracing::warn!(
                    output,
                    requested = hz,
                    achieved,
                    active_w = w,
                    active_h = h,
                    applied,
                    "KWin virtual output mode below requested — pacing the encoder to the \
                     achieved rate (custom-mode install rejected? is kscreen-doctor up to date?)"
                );
            }
            Some((w, h, achieved.max(1)))
        }
        None => {
            tracing::warn!(
                output,
                requested = hz,
                applied,
                "could not read back KWin virtual output refresh — assuming 60 Hz (is \
                 kscreen-doctor installed?)"
            );
            None
        }
    }
}

/// Read the active mode (`(width, height, refresh_hz)`, Hz rounded) of `output` — a RESOLVED
/// kscreen address (numeric id or name, see [`resolve_kscreen_addr`]) — from `kscreen-doctor -j`.
/// `None` if the tool, the output, or its current mode can't be found. Mode/output ids come
/// through as either JSON strings or numbers depending on the KWin version, so both are accepted.
fn read_active_mode(output: &str) -> Option<(u32, u32, u32)> {
    let doc = kscreen_json()?;
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let o = doc.get("outputs")?.as_array()?.iter().find(|o| {
        o.get("name").and_then(|n| n.as_str()) == Some(output)
            || o.get("id").and_then(as_id).as_deref() == Some(output)
    })?;
    let current = o.get("currentModeId").and_then(as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(as_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    let w = size.get("width").and_then(|v| v.as_u64())? as u32;
    let h = size.get("height").and_then(|v| v.as_u64())? as u32;
    let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?;
    Some((w, h, hz.round() as u32))
}

/// The prefix EVERY managed KWin output shares — Stage 3 names them `punktfunk` / `punktfunk-<id>`,
/// which KWin exposes as `Virtual-punktfunk` / `Virtual-punktfunk-<id>`. Group membership (§6.1) is
/// recognised by this prefix, so we never have to thread the live set through the backend.
///
/// Shared with [`crate::kwin_output_mgmt`] rather than copied: both halves of the ladder decide
/// "is this output one of OURS?" with it, and a drift between two copies would make the in-process
/// path disable a sibling session's output that the kscreen path deliberately spares.
pub(crate) const MANAGED_PREFIX: &str = "Virtual-punktfunk";

/// The current mode of an output as a kscreen-doctor mode setter, from its `-j` entry — preferring
/// the human `WxH@Hz` form (survives a mode-id re-enumeration across disable→enable) and falling back
/// to the raw `currentModeId`. `None` if the current mode can't be resolved.
fn output_current_mode_spec(o: &serde_json::Value) -> Option<String> {
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let current = o.get("currentModeId").and_then(&as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(&as_id).as_deref() == Some(current.as_str()))?;
    let human = (|| {
        let size = mode.get("size")?;
        let w = size.get("width").and_then(|v| v.as_u64())?;
        let h = size.get("height").and_then(|v| v.as_u64())?;
        let hz = mode.get("refreshRate").and_then(|r| r.as_f64())?.round() as u64;
        Some(format!("{w}x{h}@{hz}"))
    })();
    Some(human.unwrap_or(current))
}

/// Currently-ENABLED outputs that are **not managed by us** — the headless session's bootstrap
/// output(s) + any physical monitor, i.e. exactly what `exclusive` must disable — EACH PAIRED WITH ITS
/// CURRENT MODE (`WxH@Hz`, empty if unresolved) so teardown can put it back at that exact refresh (a
/// bare re-enable drops a 120 Hz panel to KWin's default ~60 Hz).
/// **Group-aware (§6.1):** excludes the WHOLE managed family (the [`MANAGED_PREFIX`]), not just this
/// session's own output — so a 2nd `exclusive` session (with a distinct per-slot name) never disables
/// the 1st session's live output. Parsed from `kscreen-doctor -j` (same source as [`read_active_mode`]).
fn other_enabled_outputs() -> Vec<(String, String)> {
    let out = match kscreen_json_bytes() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&out) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter()
                .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
                .filter_map(|o| {
                    let name = o.get("name").and_then(|n| n.as_str())?;
                    (!name.starts_with(MANAGED_PREFIX)).then(|| {
                        (
                            name.to_string(),
                            output_current_mode_spec(o).unwrap_or_default(),
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// True if any managed group member (the [`MANAGED_PREFIX`] family) is ALREADY the KWin primary —
/// first-slot-wins support (§6.1) so a later exclusive session doesn't steal primary from the group's
/// first member. Best-effort: if kscreen reports no primary flag we treat it as "none" (the session
/// then sets itself primary — the pre-group behavior). Recent kscreen marks the primary with
/// `"priority": 1`; older builds used a `"primary": true` bool — accept either.
fn a_managed_output_is_primary() -> bool {
    let Some(out) = kscreen_json_bytes() else {
        return false;
    };
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&out) else {
        return false;
    };
    doc.get("outputs")
        .and_then(|o| o.as_array())
        .map(|outs| {
            outs.iter().any(|o| {
                let managed = o
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
                let primary = o.get("primary").and_then(|p| p.as_bool()).unwrap_or(false)
                    || o.get("priority").and_then(|p| p.as_u64()) == Some(1);
                managed && primary
            })
        })
        .unwrap_or(false)
}

/// Set our output primary and disable the bootstrap output(s) so the managed group becomes
/// the sole desktop (KWin re-homes plasmashell + windows onto it). `ours` is the RESOLVED kscreen
/// address (numeric id or name, see [`resolve_kscreen_addr`]). Returns the disabled outputs for
/// the keepalive to re-enable on teardown. Best-effort: on failure, streaming continues (just possibly
/// showing only the wallpaper) rather than failing the session.
fn apply_virtual_primary(ours: &str) -> Vec<(String, String)> {
    let kscreen = |args: &[String]| kscreen_ok(args);
    // First-slot-wins (§6.1): only grab primary if no managed group member is primary yet — so a 2nd
    // exclusive session joins as a secondary monitor of the shared desktop instead of stealing the
    // shell off the 1st session's output. KWin usually then re-homes the desktop + disables the
    // bootstrap on its own; the belt-and-suspenders disable below covers the rest.
    if !a_managed_output_is_primary() {
        if !kscreen(&[format!("output.{ours}.primary")]) {
            tracing::warn!(
                "KWin: could not set the virtual output primary; client may see only the wallpaper"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Disable everything still enabled that ISN'T a managed group member (bootstrap / physical), so
    // the group is unambiguously the desktop — never a sibling session's output (group-aware filter).
    // Each is captured WITH its current mode so teardown restores its real refresh, not KWin's default.
    let others = other_enabled_outputs();
    if others.is_empty() {
        tracing::info!("KWin: streamed output set as the sole desktop (nothing else was enabled)");
        return others;
    }
    let args: Vec<String> = others
        .iter()
        .map(|(o, _mode)| format!("output.{o}.disable"))
        .collect();
    if kscreen(&args) {
        tracing::info!(also_disabled = ?others, "KWin: streamed output set as the sole desktop");
    } else {
        // Report the request, not a success: the outputs are still enabled, so the client sees the
        // shell wherever KWin left it. They are returned for the restore regardless — re-enabling an
        // output that was never disabled is a harmless no-op, and dropping them here would strand a
        // physical dark if the disable actually landed and only the ack was lost to the budget.
        tracing::warn!(
            attempted_disable = ?others,
            "KWin: could not disable the other outputs for the exclusive topology (kscreen-doctor \
             failed or hit its budget) — the streamed output is not the sole desktop"
        );
    }
    others
}

/// **Primary** (Stage 2): make the streamed output the primary but KEEP the other outputs enabled
/// (don't disable the bootstrap/physical) — so the shell re-homes onto the streamed surface while a
/// physical screen stays usable. Nothing to restore on teardown (we disabled nothing).
fn apply_virtual_primary_only(ours: &str) {
    let ok = kscreen_ok(&[format!("output.{ours}.primary")]);
    if ok {
        tracing::info!("KWin: streamed output set primary (physical outputs kept)");
    } else {
        tracing::warn!("KWin: could not set the virtual output primary");
    }
}

/// Dropping this releases the KWin virtual output: it flips the keepalive thread's `stop`, which
/// drops the Wayland connection and makes KWin reclaim the output. The topology **restore** is no
/// longer bound here — it moved to the registry's display group (§6.1, restored in-process via
/// [`crate::kwin_output_mgmt::reenable_outputs`], `kscreen-doctor` fallback), which runs it once when
/// the group's last member drops, BEFORE this keepalive is dropped.
struct StopGuard {
    stop: Arc<AtomicBool>,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct State {
    screencast: Option<Screencast>,
    node_id: Option<u32>,
    failed: Option<String>,
    closed: bool,
    /// Highest `wl_display.sync` serial whose `done` has arrived — the barrier [`roundtrip_within`]
    /// waits on, so a compositor that accepted the connection and then stopped serving costs a
    /// budget instead of the thread.
    sync_done: u32,
    /// Whether this connection needs `wl_output` objects at all — true ONLY on the monitor-mirror
    /// path. `stream_virtual_output` names its output by string, so the virtual-output path never
    /// reads [`State::outputs`]; binding them there was pure accumulation on a connection that
    /// lives for the whole session, and every managed display this host creates is itself another
    /// `wl_output` global.
    want_outputs: bool,
    /// Every `wl_output` KWin advertises, as (registry global name, proxy, connector once the
    /// `name` event arrives). Only the monitor-mirror path ([`stream_existing_output`]) needs these —
    /// `stream_output` takes a `wl_output` object, so the connector has to be resolved to one. The
    /// global name is carried so `global_remove` can find the entry again ([`State::forget_output`]).
    outputs: Vec<(u32, WlOutput, Option<String>)>,
}

impl State {
    /// Drop the `wl_output` whose registry global just went away.
    ///
    /// Both halves matter. The proxy must be `release`d — wayland-rs sends no destructor when a
    /// proxy is merely dropped, so an unreleased binding is a server-side object leaked for the
    /// life of a connection that lasts as long as the session. And the ENTRY must go, because
    /// [`run_existing`]'s connector resolve scans this vector: a stale row for an unplugged head
    /// would shadow the live output that took its connector name.
    fn forget_output(&mut self, global: u32) {
        let Some(pos) = self.outputs.iter().position(|(n, _, _)| *n == global) else {
            return;
        };
        let (_, out, connector) = self.outputs.remove(pos);
        // `wl_output.release` is `since 3`; below that the object simply has no destructor.
        if out.version() >= 3 {
            out.release();
        }
        tracing::debug!(?connector, "KWin: a wl_output went away — released it");
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == Screencast::interface().name {
                    let v = version.min(MAX_VERSION);
                    state.screencast = Some(registry.bind::<Screencast, _, _>(name, v, qh, ()));
                } else if state.want_outputs && interface == WlOutput::interface().name {
                    // v4 is where `wl_output.name` (the connector) arrives; bind at least that when
                    // the compositor offers it, else bind what it has and let the resolve fail
                    // loudly rather than mirroring an unidentifiable head.
                    let v = version.min(WL_OUTPUT_MAX_VERSION);
                    let out = registry.bind::<WlOutput, _, _>(name, v, qh, ());
                    state.outputs.push((name, out, None));
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.forget_output(name),
            _ => {}
        }
    }
}

/// The `wl_display.sync` callback: `done` releases whichever [`roundtrip_within`] is waiting on
/// this serial. A plain `roundtrip()` would do the same job in one call, but it blocks on the
/// socket with no ceiling — against a compositor that accepted the connection and then stopped
/// answering, that is the session's stream thread pinned forever.
impl Dispatch<WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        serial: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = state.sync_done.max(*serial);
        }
    }
}

/// `wl_output` version we bind at: 4 brings the `name` event carrying the connector
/// (`DP-1`, …) — the only way to tell KWin's outputs apart on this connection.
const WL_OUTPUT_MAX_VERSION: u32 = 4;

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = state.outputs.iter_mut().find(|(_, o, _)| o == output) {
                slot.2 = Some(name);
            }
        }
    }
}

// The manager has no events.
impl Dispatch<Screencast, ()> for State {
    fn event(
        _: &mut Self,
        _: &Screencast,
        _: zkde::zkde_screencast_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ScreencastStream, ()> for State {
    fn event(
        state: &mut Self,
        _: &ScreencastStream,
        event: StreamEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => state.node_id = Some(node),
            StreamEvent::Failed { error } => state.failed = Some(error),
            StreamEvent::Closed => state.closed = true,
            // `serial` (v6) — we use the node id from `created`, so ignore.
            _ => {}
        }
    }
}

/// Worker thread: create a `width`x`height` virtual output on KWin, send its PipeWire node id
/// back over `setup_tx`, then keep the Wayland connection alive (so the output isn't destroyed)
/// until `stop` is set. Mirrors the portal thread's "park to keep the session alive".
fn virtual_output_thread(
    width: u32,
    height: u32,
    name: String,
    pointer_mode: u32,
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
) {
    if let Err(e) = run(width, height, &name, pointer_mode, &setup_tx, &stop) {
        // If we never delivered a node id, report the failure to the waiting opener.
        let _ = setup_tx.send(Err(format!("{e:#}")));
    }
}

/// Start recording the existing KWin output named `connector` (the monitor-mirror path), returning
/// its PipeWire node id and the keepalive whose drop stops the recording.
///
/// `hw_cursor` selects the pointer mode exactly as the virtual-output path does: metadata for a
/// cursor-channel session, embedded otherwise, so the cursor behaves the same whichever source a
/// session is on.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let pointer_mode = if hw_cursor {
        POINTER_METADATA
    } else {
        POINTER_EMBEDDED
    };
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let connector_thread = connector.to_string();
    thread::Builder::new()
        .name("punktfunk-kwin-mirror".into())
        .spawn(move || {
            if let Err(e) = run_existing(&connector_thread, pointer_mode, &setup_tx, &stop_thread) {
                let _ = setup_tx.send(Err(format!("{e:#}")));
            }
        })
        .context("spawn KWin monitor-mirror thread")?;
    let node_id = match setup_rx.recv_timeout(OPENER_BUDGET) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("KWin monitor mirror failed: {e}"),
        Err(_) => {
            // Same leak as the virtual-output opener: `StopOnDrop` only takes ownership of `stop`
            // on the success path, so without this the mirror thread keeps recording a monitor
            // nobody is watching until its own budget runs out.
            stop.store(true, Ordering::Relaxed);
            bail!("timed out recording the KWin output {connector:?}")
        }
    };
    Ok(crate::mirror::MirrorStream {
        node_id,
        // KWin publishes on the user's own PipeWire daemon — no portal remote to carry.
        remote_fd: None,
        // Not an xdg-portal session either: the `zkde_screencast` pointer mode was asked of KWin
        // directly and KWin honours it, so the request IS the answer.
        cursor_mode: None,
        keepalive: Box::new(StopOnDrop(stop)),
    })
}

/// Stops the mirror thread (and thus the recording) when the capturer drops it.
struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Extra sentence appended to every "KWin never advertised the screencast global" error when this
/// process carries capabilities — the one cause that is completely invisible from the Wayland side.
///
/// KWin authorizes a restricted interface by resolving the *client's* `/proc/<pid>/exe` and
/// matching it against an installed `.desktop`. The kernel refuses that readlink to any reader
/// whose effective set is not a superset of the target's **permitted** set
/// (`cap_ptrace_access_check`), and KWin has no capabilities at all. So a host binary carrying any
/// file capability is simply unidentifiable: `executablePath()` comes back empty, no `.desktop` can
/// match, and the global is never advertised — indistinguishable, from here, from a missing
/// `.desktop`. Neither half of the obvious workaround helps: `prctl(PR_SET_DUMPABLE, 1)` leaves the
/// permitted-set check failing, and moving the grant to systemd `AmbientCapabilities=` lands the
/// capability in the same permitted set. Only an uncapped binary is identifiable.
///
/// This is not hypothetical: 0.26.0-1 setcap'd `cap_sys_nice` on the host for the GPU-priority
/// lever and took out desktop streaming on every KDE box until the capability was removed again.
fn capability_denial_hint() -> String {
    let permitted = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| permitted_caps_from_status(&status));
    capability_denial_hint_for(permitted)
}

/// The message half of [`capability_denial_hint`], split from the `/proc/self/status` read so it is
/// testable against a *given* mask instead of whatever the test process happens to hold.
///
/// That distinction is not academic: the first version of this asserted the empty case by calling
/// the real thing and trusting the test process to be uncapped. That holds on a dev box and is
/// false in CI, where the runner container is root with a full permitted set
/// (`CapPrm=0x000001ffffffffff`) — so the hint fired, correctly, and the test failed on a machine
/// where nothing was wrong. A check whose answer depends on the ambient environment tests the
/// environment, not the code.
fn capability_denial_hint_for(permitted: Option<u64>) -> String {
    match permitted {
        Some(caps) if caps != 0 => format!(
            " — NOTE: this process carries capabilities (CapPrm={caps:#018x}), which is enough on \
             its own to cause this: the kernel then refuses KWin the /proc/<pid>/exe read it \
             identifies clients by, so no .desktop can match however correctly it is installed. \
             Clear them with `sudo setcap -r /usr/bin/punktfunk-host` and restart the host"
        ),
        _ => String::new(),
    }
}

/// The permitted-capability mask out of a `/proc/<pid>/status` body, or `None` if the field is
/// absent/unparseable. The kernel prints it as a tab-separated 16-digit hex word with no `0x`
/// (`CapPrm:\t0000000000800000` = CAP_SYS_NICE), which is what the split-and-radix-16 parse below
/// expects — split out from [`capability_denial_hint`] purely so that shape is testable without a
/// capability-carrying process to point at.
fn permitted_caps_from_status(status: &str) -> Option<u64> {
    let field = status.lines().find(|l| l.starts_with("CapPrm:"))?;
    u64::from_str_radix(field.split_whitespace().nth(1)?, 16).ok()
}

#[cfg(test)]
mod capability_hint_tests {
    use super::*;

    /// Verbatim from a `cap_sys_nice=ep` process on CachyOS — the case that broke 0.26.0-1.
    const CAPPED: &str = "Name:\tpunktfunk-host\nUid:\t1000\t1000\t1000\t1000\nCapPrm:\t0000000000800000\nCapEff:\t0000000000800000\n";
    /// ...and from the same binary with no capability, where the hint must stay silent.
    const CLEAN: &str = "Name:\tpunktfunk-host\nUid:\t1000\t1000\t1000\t1000\nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n";

    #[test]
    fn parses_the_kernels_permitted_mask() {
        assert_eq!(permitted_caps_from_status(CAPPED), Some(0x0080_0000));
        assert_eq!(permitted_caps_from_status(CLEAN), Some(0));
        // CapPrm is not guaranteed present (older/again-different kernels): stay quiet, never panic.
        assert_eq!(permitted_caps_from_status("Name:\tx\n"), None);
        assert_eq!(permitted_caps_from_status("CapPrm:\tzzzz\n"), None);
        assert_eq!(permitted_caps_from_status("CapPrm:\n"), None);
    }

    /// A capability-free host must not append the hint — the message it decorates is also printed
    /// on genuinely missing `.desktop` files, and a spurious "you have capabilities" line would
    /// send the reader chasing a setcap that was never there.
    ///
    /// Driven off an explicit mask rather than the test process's own: see
    /// [`capability_denial_hint_for`] for why calling the real reader here fails in CI.
    #[test]
    fn silent_without_capabilities() {
        assert_eq!(
            capability_denial_hint_for(permitted_caps_from_status(CLEAN)),
            ""
        );
        // Absent or unparseable field: also silent, never a panic and never a spurious hint.
        assert_eq!(capability_denial_hint_for(None), "");
    }

    /// ...and the case that matters actually speaks, naming the mask and the repair. Without this
    /// the test above passes just as well against a function that returns `""` unconditionally.
    #[test]
    fn names_the_mask_and_the_repair_when_capped() {
        let hint = capability_denial_hint_for(permitted_caps_from_status(CAPPED));
        assert!(
            hint.contains("0x0000000000800000"),
            "names the mask: {hint}"
        );
        assert!(hint.contains("setcap -r"), "names the repair: {hint}");
    }
}

/// Readiness probe: connect to the KWin Wayland socket, roundtrip the registry, and confirm
/// the privileged `zkde_screencast` global is actually advertised. This is exactly what
/// [`run`] needs before it can create a virtual output, so a session-bringup script can poll
/// this to gate on the compositor being *ready* (not merely the socket existing) instead of
/// racing it with a blind sleep. `Ok(())` = ready; `Err` = not ready / no global yet.
pub fn probe() -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    // Nothing to interrupt a probe: it is a one-shot question, bounded by the roundtrip budget.
    let never = AtomicBool::new(false);
    roundtrip_within(
        &conn,
        &mut queue,
        &mut state,
        &never,
        1,
        "registry roundtrip",
    )?;
    if state.screencast.is_none() {
        bail!(
            "KWin is up but does not expose zkde_screencast_unstable_v1 to this client — KWin gates \
             it on the host's .desktop X-KDE-Wayland-Interfaces (install \
             io.unom.Punktfunk.Host.desktop with Exec=/usr/bin/punktfunk-host, then re-login so KWin \
             re-reads it — the grant is cached per-exe on first connect), or set \
             KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 for the headless test; needs KWin ≥ 6.5.6{}",
            capability_denial_hint()
        );
    }
    Ok(())
}

/// KWin is usable iff we're inside a KWin session exposing `zkde_screencast` — exactly what
/// [`probe`] checks, surfaced as a bool for compositor enumeration.
pub fn is_available() -> bool {
    probe().is_ok()
}

/// Stream an **existing** KWin output — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md` L1). Same privileged global and the same thread/keepalive
/// shape as the virtual-output path; `stream_output` simply takes a `wl_output` instead of minting
/// one, so there is no dialog, no portal, and no chooser: the connector name IS the selection.
///
/// Returns the PipeWire node id. The thread parks until `stop`, holding the Wayland connection that
/// is the cast's lifetime — dropping it stops the recording and leaves the monitor untouched (we
/// never created it, so there is nothing to tear down; §7.1).
fn run_existing(
    connector: &str,
    pointer_mode: u32,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    // The opener started its own clock a moment ago; everything this worker spends before
    // `await_created` comes out of the same 20 s (see [`CREATE_BUDGET`] — this path has two
    // barriers, which is exactly why the create wait cannot be a fixed 15 s here).
    let started = Instant::now();
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // The one path that resolves a connector to a `wl_output`, so the only one that binds them.
    let mut state = State {
        want_outputs: true,
        ..State::default()
    };
    // Two roundtrips: the first processes the globals (binding screencast + every wl_output), the
    // second drains each output's property burst — the `name` event we resolve the connector by.
    roundtrip_within(&conn, &mut queue, &mut state, stop, 1, "registry roundtrip")?;
    roundtrip_within(
        &conn,
        &mut queue,
        &mut state,
        stop,
        2,
        "wl_output property roundtrip",
    )?;

    let screencast = state.screencast.clone().ok_or_else(|| {
        anyhow!(
            "KWin does not expose zkde_screencast_unstable_v1 to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it, or run KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 (headless \
             test){}",
            capability_denial_hint()
        )
    })?;

    // Resolve the connector to a bound wl_output. A miss is a hard error naming what IS there:
    // mirroring some other monitor because the requested one is unplugged shows the operator a
    // screen they did not ask for, which is worse than a session that refuses with a reason.
    let named: Vec<&str> = state
        .outputs
        .iter()
        .filter_map(|(_, _, n)| n.as_deref())
        .collect();
    let output = state
        .outputs
        .iter()
        .find(|(_, _, n)| n.as_deref() == Some(connector))
        .or_else(|| {
            state.outputs.iter().find(|(_, _, n)| {
                n.as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(connector))
            })
        })
        .map(|(_, o, _)| o.clone())
        .ok_or_else(|| {
            if named.is_empty() {
                anyhow!(
                    "KWin advertised no named wl_output (needs wl_output v4 for the connector \
                     name) — cannot mirror {connector:?}"
                )
            } else {
                anyhow!(
                    "KWin has no output named {connector:?} — it has: {}",
                    named.join(", ")
                )
            }
        })?;

    let stream = screencast.stream_output(&output, pointer_mode, &qh, ());
    tracing::info!(
        connector,
        embedded_pointer = pointer_mode != POINTER_METADATA,
        "KWin: recording an existing output; awaiting PipeWire node"
    );

    let node_id = await_created(
        &conn,
        &mut queue,
        &mut state,
        stop,
        "stream_output",
        started,
    )?;
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("monitor-mirror opener went away"))?;

    park_until_stopped(&conn, &mut queue, &mut state, stop, connector, node_id)?;
    stream.close();
    let _ = conn.flush();
    Ok(())
}

fn run(
    width: u32,
    height: u32,
    name: &str,
    pointer_mode: u32,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    // Same clock as the mirror path: one barrier here rather than two, but the create wait is
    // bounded against the opener either way (see [`CREATE_BUDGET`]).
    let started = Instant::now();
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    // `want_outputs` stays false: `stream_virtual_output` names its output by string, so this
    // connection never needs a `wl_output` — and it lives for the whole session (see `State`).
    let mut state = State::default();
    roundtrip_within(&conn, &mut queue, &mut state, stop, 1, "registry roundtrip")?;

    let screencast = state.screencast.clone().ok_or_else(|| {
        anyhow!(
            "KWin does not expose zkde_screencast_unstable_v1 to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it, or run KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 (headless \
             test){}",
            capability_denial_hint()
        )
    })?;

    // Create the virtual output sized to the client; the pointer rides as stream metadata
    // (cursor-channel session) or KWin embeds it into frames (everyone else — see the consts).
    let stream = screencast.stream_virtual_output(
        name.to_string(),
        width as i32,
        height as i32,
        1.0, // scale (logical == physical)
        pointer_mode,
        &qh,
        (),
    );
    tracing::info!(
        width,
        height,
        "KWin: requested virtual output; awaiting PipeWire node"
    );

    // Pump events until KWin reports the node id (or an error, or the budget).
    //
    // A refusal here is where the KWin >= 6.6 disabled-output trap lands, and it is repairable
    // FROM INSIDE THIS SCOPE and nowhere else: KWin destroys the output when our stream is
    // destroyed, so the connection has to stay up while we enable it (see
    // [`kwin_output_mgmt::enable_disabled_output`] for why the output is still alive at all, and
    // why enabling it fixes the NEXT request rather than this one).
    let node_id = match await_created(
        &conn,
        &mut queue,
        &mut state,
        stop,
        "stream_virtual_output",
        started,
    ) {
        Ok(id) => id,
        Err(e) => {
            // `Virtual-<name>` is the address KWin exposes our output under (the same prefix the
            // topology path resolves against).
            match crate::kwin_output_mgmt::enable_disabled_output(&format!("Virtual-{name}")) {
                // Deliberately does NOT carry the "KWin virtual output failed" prefix: that string
                // is what marks a KWin refusal PERMANENT for the session's retry loop, and this is
                // the one refusal where something DID change between attempts. Retrying is the
                // whole point of repairing.
                Some(repaired) => bail!(
                    "KWin created the virtual output disabled and refused to stream it ({e}); \
                     {REPAIRED_HINT} (head {repaired}) — the retry picks up the configuration \
                     KWin just persisted"
                ),
                // Nothing to repair (no such head, already enabled, or the apply was refused):
                // the refusal stands, and its own prefix keeps it permanent so the session fails
                // fast instead of burning the retry budget on an unchanged box.
                None => return Err(e),
            }
        }
    };
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("virtual-output opener went away"))?;

    park_until_stopped(&conn, &mut queue, &mut state, stop, name, node_id)?;

    // Best-effort clean teardown; dropping the connection also makes KWin reclaim the output.
    stream.close();
    let _ = conn.flush();
    Ok(())
}

/// Poll slice while waiting on the Wayland fd — the granularity at which `stop` and a deadline are
/// observed (matches `kwin_output_mgmt`'s `POLL_MS`).
const POLL_MS: i32 = 200;

/// Budget for one compositor roundtrip. Generous next to a healthy one (a few ms); it exists only
/// so a KWin that accepted the connection and then stopped serving cannot pin the calling thread —
/// which for [`probe`] is whatever thread the mgmt API answered a `/display/compositors` on, and
/// for [`run`] is the session's own bring-up.
const ROUNDTRIP_BUDGET: Duration = Duration::from_secs(3);

/// How long an opener ([`spawn_vout`](VirtualDisplay::create), [`stream_existing_output`]) waits
/// for the worker's first word before giving up on it.
const OPENER_BUDGET: Duration = Duration::from_secs(20);

/// Slack subtracted from [`OPENER_BUDGET`] to get the worker's own ceiling: enough for its error to
/// travel one `mpsc` send while the opener is still listening.
const WORKER_MARGIN: Duration = Duration::from_millis(500);

/// Budget for the `created` handshake (the PipeWire node id) — but only as a ceiling, because
/// what actually matters is that the WORKER gives up before its opener does, so the failure the
/// client sees is a REASON ("KWin never created the output") rather than a bare timeout with the
/// worker still parked behind it.
///
/// That is a property of the whole worker, not of this one step, and it cannot be had by comparing
/// this constant with [`OPENER_BUDGET`]: the two workers do a different amount of work before they
/// get here. [`run`] spends one [`ROUNDTRIP_BUDGET`] barrier, so 3 + 15 < 20 ✓ — but [`run_existing`]
/// needs TWO (the registry globals, then the `wl_output` property burst that carries the connector
/// name), so 3 + 3 + 15 = 21 s and the mirror path lost the property that the doc here once claimed
/// for both. Hence [`await_created`] takes the worker's start instant and bounds itself by whichever
/// comes first, this budget or the opener's deadline; adding a third barrier to some future worker
/// cannot silently break it again.
const CREATE_BUDGET: Duration = Duration::from_secs(15);

/// How a bounded pump ended.
enum Pumped {
    /// The predicate held.
    Done,
    /// `stop` was set — the caller's output/recording was released while we waited.
    Stopped,
    /// The deadline passed first.
    Expired,
}

/// Bounded manual event loop: dispatch what's queued, then poll the connection fd for up to
/// [`POLL_MS`] and read, until `done(&state)` holds, `stop` is set, or `deadline` passes.
///
/// This is the only way to wait on this connection. `blocking_dispatch` and `roundtrip` cannot be
/// interrupted and have no ceiling, so a compositor that stops answering turns any wait into a
/// permanently stuck thread — and on the host that thread is the session's, whose only way to end a
/// session is to return. `deadline: None` means "no ceiling", which is correct for exactly one
/// caller: [`park_until_stopped`], where the wait IS the output's lifetime.
fn pump_until(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    deadline: Option<Instant>,
    stop: &AtomicBool,
    done: impl Fn(&State) -> bool,
) -> Result<Pumped> {
    loop {
        queue.dispatch_pending(state).context("dispatch_pending")?;
        if done(state) {
            return Ok(Pumped::Done);
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(Pumped::Stopped);
        }
        let timeout = match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(Pumped::Expired);
                }
                (remaining.as_millis() as i64).clamp(0, i64::from(POLL_MS)) as i32
            }
            None => POLL_MS,
        };
        conn.flush().context("wayland flush")?;
        let Some(guard) = conn.prepare_read() else {
            continue; // events already queued — loop dispatches them
        };
        let mut pfd = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `&mut pfd` points at a single live, fully-initialized `libc::pollfd` on the stack, and
        // the count `1` matches that one-element array, so `poll` reads `fd`/`events` and writes `revents`
        // strictly within `pfd`. `pfd.fd` is the Wayland connection's fd, valid because `conn` (and the
        // `prepare_read` guard) are alive across the call. `poll` blocks up to `timeout` ms and writes
        // only `revents`; `pfd` outlives the synchronous call and aliases nothing (a fresh local).
        let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = guard.read();
        } // else: timeout or signal — drop the guard, re-check `stop` and the deadline
    }
}

/// A `wl_display.sync` barrier bounded by [`ROUNDTRIP_BUDGET`] — the replacement for
/// `EventQueue::roundtrip`, which waits on the socket with no ceiling. `serial` must be unique per
/// connection (callers number theirs from 1); `what` names the wait in the error.
fn roundtrip_within(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    serial: u32,
    what: &str,
) -> Result<()> {
    let qh = queue.handle();
    let _cb = conn.display().sync(&qh, serial);
    let deadline = Instant::now() + ROUNDTRIP_BUDGET;
    match pump_until(conn, queue, state, Some(deadline), stop, |st| {
        st.sync_done >= serial
    })? {
        Pumped::Done => Ok(()),
        Pumped::Stopped => bail!("{what} abandoned — the stream was released while we waited"),
        Pumped::Expired => bail!(
            "KWin accepted the Wayland connection but did not answer the {what} within \
             {ROUNDTRIP_BUDGET:?} — the compositor is not serving this client"
        ),
    }
}

/// Keep the connection (and thus the stream) alive until told to stop, observing `closed`.
/// Shared by the virtual-output and monitor-mirror paths — for a virtual output this connection IS
/// the output's lifetime; for a mirror it is only the recording's, and the monitor itself is
/// untouched either way. The only deadline-free [`pump_until`] in the file, for that reason.
fn park_until_stopped(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    output: &str,
    node_id: u32,
) -> Result<()> {
    match pump_until(conn, queue, state, None, stop, |st| st.closed)? {
        Pumped::Done => {
            tracing::warn!(output = %output, node_id, "KWin closed the screencast stream");
        }
        // `Expired` cannot happen without a deadline; `Stopped` is the ordinary teardown.
        Pumped::Stopped | Pumped::Expired => {}
    }
    Ok(())
}

/// Wait for the `created` event carrying the PipeWire node id, bounded and interruptible by `stop`.
///
/// The loop this replaced was a bare `blocking_dispatch` with no deadline that never read `stop`:
/// a KWin that acknowledged `stream_virtual_output` and then never answered parked the worker
/// thread for good, and the opener's `recv_timeout` arm — which did not set `stop` either — left it
/// there holding a half-built output. `request` names the request in the error.
///
/// `started` is when the WORKER began, not when this wait did: the bound is the earlier of
/// [`CREATE_BUDGET`] and the opener's own deadline, so whatever the barriers before us consumed
/// comes out of this wait rather than out of the opener's patience (see [`CREATE_BUDGET`] for the
/// arithmetic that made a fixed budget wrong on the mirror path).
fn await_created(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    request: &str,
    started: Instant,
) -> Result<u32> {
    let began = Instant::now();
    let deadline = (began + CREATE_BUDGET).min(started + OPENER_BUDGET - WORKER_MARGIN);
    let settled = |st: &State| st.node_id.is_some() || st.failed.is_some() || st.closed;
    match pump_until(conn, queue, state, Some(deadline), stop, settled)? {
        // Node id first: a `closed` that arrives in the same burst as `created` is a stream that
        // was made and then torn down, not a failure to make one.
        Pumped::Done => match (state.node_id, state.failed.take()) {
            (Some(node), _) => Ok(node),
            (None, Some(e)) => bail!("{request} failed: {e}"),
            (None, None) => bail!("KWin closed the stream before it was created"),
        },
        Pumped::Stopped => bail!("{request} abandoned — released before KWin created the stream"),
        // Report the wait we actually got, not the budget we asked for — they differ whenever the
        // opener's deadline was the tighter of the two, and a message naming 15 s after 11 s is the
        // kind of thing that sends the next person hunting for a stall that never happened.
        Pumped::Expired => bail!(
            "KWin acknowledged {request} but never sent the PipeWire node within {:?}",
            began.elapsed()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        mode_satisfies, modes_from_json, monitors_from_kscreen_json, pick_custom_mode, KModeRow,
        MANAGED_PREFIX,
    };

    /// The field failure this predicate now guards, in the shape the log reported it: a client
    /// negotiated 3840x2160, KWin restored a stored 1920x1080 for the output name, and nothing
    /// compared the two — so the session captured 1080p, encoded 1080p, and shipped it to a client
    /// that had configured its decoder for 4K. Half the requested size is not an alignment.
    #[test]
    fn a_restored_stored_mode_does_not_pass_for_the_requested_one() {
        assert!(!mode_satisfies((1920, 1080), 3840, 2160));
    }

    /// The case the predicate must NOT reject, and the reason it can't just test equality: libxcvt
    /// rounds a width down to the 8-px cell grain, so the mode KWin builds for a 2868-wide request
    /// really is 2864 wide. Rejecting it would strand the output on its birth mode.
    #[test]
    fn a_cvt_aligned_width_still_satisfies_the_request() {
        assert!(mode_satisfies((2864, 1320), 2868, 1320));
        assert!(mode_satisfies((3840, 2160), 3840, 2160)); // exact is the common case
    }

    /// The alignment slack is bounded and one-sided. A width 8+ px short is a different mode, not a
    /// rounding of ours; a width ABOVE the request is somebody else's mode entirely (and is what
    /// would underflow the subtraction if the `<=` guard were ever dropped); and the height is
    /// never rounded, so it must match exactly.
    #[test]
    fn the_alignment_slack_is_bounded_one_sided_and_width_only() {
        assert!(mode_satisfies((3833, 2160), 3840, 2160)); // 7 short — inside the grain
        assert!(!mode_satisfies((3832, 2160), 3840, 2160)); // 8 short — a different mode
        assert!(!mode_satisfies((3848, 2160), 3840, 2160)); // wider than asked
        assert!(!mode_satisfies((3840, 2159), 3840, 2160)); // height is never aligned
    }

    fn row(id: &str, w: u32, h: u32, hz: f64) -> KModeRow {
        KModeRow {
            id: id.to_string(),
            w,
            h,
            hz,
        }
    }

    /// The reported regression: an iPhone 16 Pro Max asks for 2868x1320@120; libxcvt rounds the
    /// width down to the 8-pixel cell grain and the clock step lands 119.92, so KWin's list holds
    /// 2864x1320@119.92. Selecting by the REQUESTED `2868x1320@120` string matched nothing — the
    /// output stayed on its birth mode and the session fell back to 60 Hz. The picker must find it.
    #[test]
    fn picks_the_cvt_aligned_mode() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // the virtual output's native/birth mode
            row("2", 2864, 1320, 119.92), // the custom mode KWin actually generated
        ];
        let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("CVT-aligned mode");
        assert_eq!((got.id.as_str(), got.w, got.h), ("2", 2864, 1320));
    }

    /// A width already on the cell grain (every PC resolution: 1920/2560/3840) round-trips exactly,
    /// and an exact-width mode outranks an aligned one when both are offered.
    #[test]
    fn exact_width_outranks_an_aligned_one() {
        let modes = [
            row("1", 2560, 1440, 60.0),
            row("2", 2552, 1440, 119.93), // a stale narrower custom mode from an earlier session
            row("3", 2560, 1440, 119.98),
        ];
        let got = pick_custom_mode(&modes, 2560, 1440, 120).expect("exact mode");
        assert_eq!(got.id, "3");
    }

    /// The picker must never wander onto an unrelated mode: not the 60 Hz native entry (the old
    /// fallback the reporter got stuck on), not a different height, not a wider width, and not a
    /// mode more than one cell narrower than asked.
    #[test]
    fn rejects_modes_that_are_not_the_request() {
        let modes = [
            row("1", 2868, 1320, 60.0),   // native — refresh too far off
            row("2", 2868, 1080, 119.92), // wrong height
            row("3", 2880, 1320, 119.92), // wider than requested
            row("4", 2856, 1320, 119.92), // two cells narrower — not a CVT alignment of 2868
            row("5", 1920, 1080, 120.0),  // unrelated
        ];
        assert!(pick_custom_mode(&modes, 2868, 1320, 120).is_none());
    }

    /// Mode + output ids come through as JSON strings on some KWin versions and numbers on others;
    /// both must parse, and a mode row missing its size/refresh is skipped rather than poisoning
    /// the list.
    #[test]
    fn parses_both_id_encodings() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":7,"name":"Virtual-punktfunk","modes":[
                   {"id":"m1","size":{"width":2868,"height":1320},"refreshRate":60.0},
                   {"id":42,"size":{"width":2864,"height":1320},"refreshRate":119.92},
                   {"id":"broken","size":{"width":800}}
                 ]},
                 {"id":1,"name":"eDP-1","modes":[
                   {"id":"x","size":{"width":2864,"height":1320},"refreshRate":119.92}
                 ]}
               ]}"#,
        )
        .expect("fixture parses");
        // Addressable by numeric id (how `resolve_kscreen_addr` returns it) and by name.
        for addr in ["7", "Virtual-punktfunk"] {
            let modes = modes_from_json(&doc, addr);
            assert_eq!(modes.len(), 2, "the malformed row is dropped ({addr})");
            assert_eq!(modes[1].id, "42", "numeric mode ids stringify ({addr})");
            let got = pick_custom_mode(&modes, 2868, 1320, 120).expect("aligned mode");
            assert_eq!(got.id, "42");
        }
        // Never reads another output's list (the eDP-1 entry carries a matching mode).
        assert!(modes_from_json(&doc, "Virtual-nope").is_empty());
    }

    /// The kscreen fallback for `monitors::list` must produce the same contract the in-process
    /// reader promises: geometry from `pos` (the identity key), the mode in PIXELS with refresh in
    /// mHz precise enough to keep 59.94 apart from 60, a DISABLED head still listed but zeroed
    /// rather than invented, our own managed output flagged, and the list sorted by position.
    #[test]
    fn parses_a_kscreen_monitor_list() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"outputs":[
                 {"id":2,"name":"HDMI-A-1","enabled":true,"priority":2,"scale":1,
                  "pos":{"x":1920,"y":0},"vendor":"ACME","model":"U2720Q",
                  "currentModeId":"m9","modes":[
                    {"id":"m9","size":{"width":1920,"height":1080},"refreshRate":59.94}]},
                 {"id":1,"name":"eDP-1","enabled":true,"priority":1,"scale":1.5,
                  "pos":{"x":0,"y":0},
                  "currentModeId":7,"modes":[
                    {"id":7,"size":{"width":3840,"height":2160},"refreshRate":120.0}]},
                 {"id":3,"name":"DP-3","enabled":false,"scale":1,"pos":{"x":0,"y":0},
                  "modes":[{"id":"z","size":{"width":2560,"height":1440},"refreshRate":60.0}]},
                 {"id":4,"name":"Virtual-punktfunk-7","enabled":true,"scale":1,
                  "pos":{"x":5760,"y":0},"currentModeId":"v1","modes":[
                    {"id":"v1","size":{"width":2560,"height":1440},"refreshRate":119.98}]}
               ]}"#,
        )
        .expect("fixture parses");
        let mons = monitors_from_kscreen_json(&doc);
        let by = |c: &str| {
            mons.iter()
                .find(|m| m.connector == c)
                .unwrap_or_else(|| panic!("{c} missing"))
                .clone()
        };
        // Sorted by desktop position, not by kscreen's own order.
        let order: Vec<&str> = mons.iter().map(|m| m.connector.as_str()).collect();
        assert_eq!(
            order,
            vec!["DP-3", "eDP-1", "HDMI-A-1", "Virtual-punktfunk-7"]
        );
        let edp = by("eDP-1");
        // PIXELS, at the scale the desk actually runs — the whole point of `logical_size`.
        assert_eq!((edp.width, edp.height), (3840, 2160));
        assert_eq!(edp.scale, 1.5);
        assert_eq!(edp.logical_size(), (2560.0, 1440.0));
        assert!(edp.primary, "priority 1 is KWin's primary");
        assert_eq!(edp.refresh_mhz, 120_000);
        // 59.94 must survive as mHz; rounding to whole Hz here is the bug this guards.
        assert_eq!(by("HDMI-A-1").refresh_mhz, 59_940);
        assert_eq!(by("HDMI-A-1").description, "ACME U2720Q");
        assert!(!by("HDMI-A-1").primary);
        // Disabled: listed (so "why can't I pick it?" has an answer) with no invented mode.
        let dark = by("DP-3");
        assert!(!dark.enabled);
        assert_eq!((dark.width, dark.height, dark.refresh_mhz), (0, 0, 0));
        // Ours, and labelled by connector when the entry carries no make/model.
        let ours = by("Virtual-punktfunk-7");
        assert!(ours.managed);
        assert_eq!(ours.description, "Virtual-punktfunk-7");
        assert!(!by("eDP-1").managed);
    }

    /// A document with no `outputs` array (an error object, or a kscreen-doctor whose schema
    /// changed) is an empty list, never a panic — the caller's own error path already covers "the
    /// tool did not answer".
    #[test]
    fn a_malformed_kscreen_document_yields_no_monitors() {
        assert!(monitors_from_kscreen_json(&serde_json::json!({})).is_empty());
        assert!(monitors_from_kscreen_json(&serde_json::json!({"outputs": 7})).is_empty());
        // An output with no name cannot be pinned or resolved, so it is dropped rather than
        // reported under an empty connector.
        assert!(
            monitors_from_kscreen_json(&serde_json::json!({"outputs": [{"enabled": true}]}))
                .is_empty()
        );
    }

    /// Group-aware exclusive (§6.1): with two managed group members + a physical panel enabled,
    /// exclusive disables ONLY the non-managed panel — never a sibling session's per-slot output
    /// (the Stage-3 naming would otherwise make a 2nd exclusive session black out the 1st).
    #[test]
    fn exclusive_disables_only_non_managed() {
        let enabled = [
            "Virtual-punktfunk",   // base name (shared identity)
            "Virtual-punktfunk-1", // client A's per-slot output
            "Virtual-punktfunk-7", // client B's per-slot output
            "eDP-1",               // a physical panel
        ];
        let to_disable: Vec<&str> = enabled
            .iter()
            .copied()
            .filter(|n| !n.starts_with(MANAGED_PREFIX))
            .collect();
        assert_eq!(to_disable, vec!["eDP-1"]);
    }
}
