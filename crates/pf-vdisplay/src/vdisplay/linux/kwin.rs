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
//! connects. The headless test path instead exposes it to bare clients via
//! `KWIN_WAYLAND_NO_PERMISSION_CHECKS=1`. The compositor backend must implement
//! `createVirtualOutput`: the **DRM backend** (any version) or the **VirtualBackend since KWin
//! 6.5.6** (`kwin_wayland --virtual`); on `--virtual` < 6.5.6 the request fails with
//! "Could not find output". We talk raw Wayland on `$WAYLAND_DISPLAY`, so the host must run inside
//! the KWin session's environment.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
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
        // `last_name` holds the RESOLVED kscreen address (numeric output id, or the
        // `Virtual-<name>` fallback) — never re-derive from the name: during a supersede two
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
        self.last_name = Some(name.clone()); // for apply_position (registry-driven §6.2 layout)
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
            match setup_rx.recv_timeout(Duration::from_secs(20)) {
                Ok(Ok(v)) => Ok((v, stop)),
                Ok(Err(e)) => bail!("KWin virtual output failed: {e}"),
                Err(_) => bail!("timed out creating the KWin virtual output"),
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
        // builds against the birth mode. First cut shells out to kscreen-doctor; the in-process
        // kde_output_management_v2 client is a follow-up. `set_custom_refresh` reads back what
        // KWin *actually* gave — both the rate (so the encoder paces to the real source) and the
        // size, which KWin's CVT generator may have aligned down (see `CVT_H_GRANULARITY`). At
        // ≤60 Hz there's nothing to install — the output is born at the real size and 60 Hz is the
        // offer anyway.
        let want_high = mode.refresh_hz > 60;
        let birth_h = if want_high { height + 16 } else { height };
        let (mut node_id, mut stop) = spawn_vout(width, birth_h)?;
        tracing::info!(
            node_id,
            width,
            height,
            birth_h,
            embedded_pointer = !self.hw_cursor,
            "KWin virtual output ready"
        );
        // ⚠️ ADDRESS BY NUMERIC KSCREEN ID, NEVER BY NAME: a supersede (mode switch) creates the
        // replacement output — SAME per-slot name, deliberately, for KWin's per-name config
        // persistence — while the superseded sibling is still alive (create-before-drop). Every
        // name-addressed kscreen command then hits the FIRST match = the OLD output: on-glass this
        // resized the LIVE session's display out from under it (wrong-res/black), read back the
        // OLD output as "custom mode applied", made the OLD output primary, and positioned it —
        // while the new output never left its birth mode and the capturer's dims gate starved.
        // Resolve OUR output's kscreen id once (match: managed-prefix name AND current mode ==
        // the just-created birth size; newest id wins), and use it for every kscreen operation.
        let mut addr = resolve_kscreen_addr(&name, width, birth_h);
        self.last_name = Some(addr.clone()); // for apply_position (registry-driven §6.2 layout)
        let mut expect_exact_dims = false;
        // The size the output actually ENDS UP at — the request, unless KWin's CVT generator had to
        // shrink the width to the cell grain (see `CVT_H_GRANULARITY`). Reported as the output's
        // `preferred_mode`, which is what the capturer's renegotiation gate waits for and what the
        // encoder opens against, so a CVT-aligned mode flows end-to-end instead of starving.
        let mut final_dims = (width, height);
        let achieved_hz = if want_high {
            let active = set_custom_refresh(width, height, mode.refresh_hz, &addr);
            // Accept only an active mode that IS our custom one: the exact requested height, and a
            // width at or just below the request (a CVT alignment). That also proves the output
            // left the sacrificial birth size, so the recording stream will renegotiate to it.
            match active {
                Some((aw, ah, ahz))
                    if ah == height && aw <= width && width - aw < CVT_H_GRANULARITY =>
                {
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
                    addr = resolve_kscreen_addr(&name, width, height);
                    self.last_name = Some(addr.clone());
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
            mode.refresh_hz
        };
        // Display-management topology (Stage 2): `Extend` leaves the streamed output an extension;
        // `Primary` makes it the primary output but keeps the bootstrap/physical outputs enabled;
        // `Exclusive` makes it the SOLE desktop (others disabled, restored on teardown) — so
        // plasmashell + windows land on the streamed surface, not the headless `kwin --virtual`
        // bootstrap output. Read from the policy (replacing the PUNKTFUNK_KWIN_VIRTUAL_PRIMARY boolean).
        use crate::policy::Topology;
        let disabled = match crate::effective_topology() {
            Topology::Exclusive => apply_virtual_primary(&addr),
            Topology::Primary => {
                apply_virtual_primary_only(&addr);
                Vec::new() // nothing disabled → nothing to restore
            }
            Topology::Extend | Topology::Auto => Vec::new(),
        };
        // Per-group restore (§6.1): DON'T bind the re-enable to this session's keepalive (a per-session
        // `StopGuard` restore would re-enable the physical the moment the FIRST of several exclusive
        // sessions drops — under a still-live sibling). Instead stash it as a closure the registry lifts
        // into the display group and runs once, when the group's LAST member is torn down (ordered before
        // that display's output is reclaimed, so KWin never sees zero outputs). Empty ⇒ nothing to restore.
        self.pending_restore = (!disabled.is_empty()).then(|| {
            let disabled = disabled.clone();
            Box::new(move || reenable_outputs(&disabled)) as Box<dyn FnOnce() + Send>
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

/// Re-enable the outputs an `exclusive` topology disabled (bootstrap / physical), so KWin re-homes onto
/// them. Called by the registry when the display group's last member is torn down (design §6.1), BEFORE
/// that member's output is reclaimed — so KWin is never momentarily left with zero enabled outputs.
fn reenable_outputs(outputs: &[(String, String)]) {
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
    let _ = kscreen_ok(&enable_args);
    // THEN re-assert each captured mode, best-effort — a bare re-enable lets KWin fall back to the
    // EDID-preferred mode (a 120 Hz panel returns at ~60 Hz); this restores the exact refresh. The
    // output is enabled now, so the mode set is valid; a rejected mode just leaves KWin's default.
    let mode_args: Vec<String> = outputs
        .iter()
        .filter(|(_, mode)| !mode.is_empty())
        .map(|(name, mode)| format!("output.{name}.mode.{mode}"))
        .collect();
    if !mode_args.is_empty() {
        let _ = kscreen_ok(&mode_args);
    }
    std::thread::sleep(Duration::from_millis(200));
    tracing::info!(reenabled = ?outputs, "KWin: restored the physical/bootstrap outputs at their captured modes (group empty)");
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
    crate::proc::status_within(
        std::process::Command::new("kscreen-doctor").args(args),
        KSCREEN_BUDGET,
    )
    .map(|s| s.success())
    .unwrap_or(false)
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

/// The `(width, height)` of an output's CURRENT mode from its `kscreen-doctor -j` entry.
fn output_active_size(o: &serde_json::Value) -> Option<(u32, u32)> {
    let as_id = |v: &serde_json::Value| -> Option<String> {
        v.as_str()
            .map(|s| s.to_string())
            .or_else(|| v.as_u64().map(|n| n.to_string()))
    };
    let current = o.get("currentModeId").and_then(as_id)?;
    let mode = o
        .get("modes")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(as_id).as_deref() == Some(current.as_str()))?;
    let size = mode.get("size")?;
    Some((
        size.get("width").and_then(|v| v.as_u64())? as u32,
        size.get("height").and_then(|v| v.as_u64())? as u32,
    ))
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
const CVT_H_GRANULARITY: u32 = 8;

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
fn pick_custom_mode<'a>(
    modes: &'a [KModeRow],
    width: u32,
    height: u32,
    hz: u32,
) -> Option<&'a KModeRow> {
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
const MANAGED_PREFIX: &str = "Virtual-punktfunk";

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
    if !others.is_empty() {
        let args: Vec<String> = others
            .iter()
            .map(|(o, _mode)| format!("output.{o}.disable"))
            .collect();
        let _ = kscreen(&args);
    }
    tracing::info!(also_disabled = ?others, "KWin: streamed output set as the sole desktop");
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
/// longer bound here — it moved to the registry's display group (§6.1, [`reenable_outputs`]), which
/// runs it once when the group's last member drops, BEFORE this keepalive is dropped.
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
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == Screencast::interface().name {
                let v = version.min(MAX_VERSION);
                state.screencast = Some(registry.bind::<Screencast, _, _>(name, v, qh, ()));
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
    queue.roundtrip(&mut state).context("registry roundtrip")?;
    if state.screencast.is_none() {
        bail!(
            "KWin is up but does not expose zkde_screencast_unstable_v1 to this client — KWin gates \
             it on the host's .desktop X-KDE-Wayland-Interfaces (install \
             io.unom.Punktfunk.Host.desktop with Exec=/usr/bin/punktfunk-host, then re-login so KWin \
             re-reads it — the grant is cached per-exe on first connect), or set \
             KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 for the headless test; needs KWin ≥ 6.5.6"
        );
    }
    Ok(())
}

/// KWin is usable iff we're inside a KWin session exposing `zkde_screencast` — exactly what
/// [`probe`] checks, surfaced as a bool for compositor enumeration.
pub fn is_available() -> bool {
    probe().is_ok()
}

fn run(
    width: u32,
    height: u32,
    name: &str,
    pointer_mode: u32,
    setup_tx: &Sender<Result<u32, String>>,
    stop: &AtomicBool,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state).context("registry roundtrip")?;

    let screencast = state.screencast.clone().ok_or_else(|| {
        anyhow!(
            "KWin does not expose zkde_screencast_unstable_v1 to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it, or run KWin with KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 (headless test)"
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

    // Pump events until KWin reports the node id (or an error).
    let node_id = loop {
        queue
            .blocking_dispatch(&mut state)
            .context("wayland dispatch (awaiting created)")?;
        if let Some(node) = state.node_id {
            break node;
        }
        if let Some(e) = state.failed.take() {
            bail!("stream_virtual_output failed: {e}");
        }
        if state.closed {
            bail!("KWin closed the stream before it was created");
        }
    };
    setup_tx
        .send(Ok(node_id))
        .map_err(|_| anyhow!("virtual-output opener went away"))?;

    // Keep the connection (and thus the virtual output) alive until told to stop, observing
    // `closed`. blocking_dispatch can't be interrupted, so poll the connection fd with a short
    // timeout so `stop` is honored within ~200 ms.
    while !stop.load(Ordering::Relaxed) {
        queue
            .dispatch_pending(&mut state)
            .context("dispatch_pending")?;
        if state.closed {
            tracing::warn!(output = %name, node_id, "KWin closed the virtual-output stream");
            break;
        }
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
        // `prepare_read` guard) are alive across the call. `poll` blocks up to 200 ms and writes only
        // `revents`; `pfd` outlives the synchronous call and aliases nothing (a fresh local).
        let r = unsafe { libc::poll(&mut pfd, 1, 200) };
        if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
            let _ = guard.read();
        } // else: timeout or signal — drop the guard, re-check `stop`
    }

    // Best-effort clean teardown; dropping the connection also makes KWin reclaim the output.
    stream.close();
    let _ = conn.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{modes_from_json, pick_custom_mode, KModeRow, MANAGED_PREFIX};

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
