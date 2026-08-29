//! Virtual-display **management policy** — the user-configurable behavior surface for how virtual
//! displays are created, kept alive, and arranged (design: `design/display-management.md`).
//!
//! This is the pure config layer that sits **above** the per-compositor [`VirtualDisplay`](super)
//! backends: a small set of orthogonal options ([`DisplayPolicy`]) with safe defaults and named
//! [`Preset`]s, persisted to `<config>/display-settings.json` and editable from the web console.
//! Every axis here is now *acted on*, so nothing in this file is a stored-but-inert knob: `keep_alive`
//! by the display lifecycle (`lifecycle` + [`super::registry`]), `topology` by each backend's
//! [`super::effective_topology`] apply, `mode_conflict` by [`super::admission`] before the Welcome,
//! `identity` by the `identity` slot table (whose carriers are the Windows EDID serial + IddCx
//! connector index, KWin's per-slot output name and the host-persisted Mutter scale map), and
//! `layout` by `layout::arrange` — on Linux the *position apply* is KWin-only, everywhere else the
//! arrangement is the `/display/state` readout. This file plus the mgmt endpoints remain the single
//! surface the console writes.
//!
//! Precedence, mirroring the GPU preference (`console preference > env pin > default`): a present,
//! valid `display-settings.json` (console-written) **wins**; when it is absent the host keeps its
//! historical env-knob / default behavior untouched ([`DisplayPolicyStore::configured`] returns
//! `None`, and every Stage-0 call site falls back to exactly what it did before). The policy is
//! read at each acquire/teardown (file state, not a startup-frozen env var), so a console change
//! applies to the next connect without a host restart.
//!
//! The pure logic here — preset expansion, [`DisplayPolicy::effective`], the [`KeepAlive`] linger
//! resolution — is unit-tested; the store adds file I/O around it (the `gpu.rs` discipline:
//! private dir, temp-write + atomic rename, in-memory rollback on a failed write).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

/// How long a virtual display (and, on gamescope's bare spawn, the nested session + its game)
/// survives after the last client session detaches. Serialized as an object tagged on `mode`
/// (`{"mode":"off"}` / `{"mode":"duration","seconds":300}` / `{"mode":"forever"}`) so the web form
/// and the OpenAPI schema stay simple.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum KeepAlive {
    /// Tear the display down at session end (today's default on every backend but Windows, which
    /// lingers 10 s).
    Off,
    /// Keep the display for `seconds` after the last session leaves, then tear it down; a reconnect
    /// inside the window reuses it.
    Duration {
        /// Linger window in seconds, clamped to `0..=86400` on write (see
        /// [`DisplayPolicy::sanitized`]): a window longer than a day is `forever` by any honest
        /// reading, and `u32` seconds is ~136 years — a deadline the reaper would never reach and a
        /// nonsense `expires_in_ms` in `/display/state`.
        seconds: u32,
    },
    /// Keep the display until host shutdown or an explicit release (the `Pinned` lifecycle state).
    /// Honored end-to-end: the registry resolves it to `Release::Pin`, so the display survives every
    /// disconnect — free it with `POST /display/release` (which force-releases `Pinned` exactly like
    /// a `Lingering` display). This is what the `gaming-rig` preset selects.
    Forever,
}

impl Default for KeepAlive {
    fn default() -> Self {
        // The historical Windows behavior, made explicit: 10 s is long enough for a client's own
        // reconnect (a mode change, a network blip) to reuse the display instead of re-minting it,
        // short enough that a walk-away leaves no ghost head on the desktop. Every backend now runs
        // the same lifecycle, so this is the linger on Linux too.
        KeepAlive::Duration { seconds: 10 }
    }
}

/// Resolved linger for the display lifecycle: teardown immediately, after a fixed window, or never.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linger {
    /// Tear down as soon as the last session leaves.
    Immediate,
    /// Linger for this window, then tear down.
    For(Duration),
    /// Never auto-tear-down (Pinned).
    Forever,
}

impl KeepAlive {
    /// The [`Linger`] this keep-alive resolves to.
    pub fn linger(self) -> Linger {
        match self {
            KeepAlive::Off => Linger::Immediate,
            KeepAlive::Duration { seconds } => Linger::For(Duration::from_secs(seconds as u64)),
            KeepAlive::Forever => Linger::Forever,
        }
    }
}

/// What the host does to the box's display topology while managed virtual displays are up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Today's behavior, resolved per host at acquire time (see [`super::effective_topology`]):
    /// exclusive on Windows and the auto-detected Linux desktop path, extend under an explicit
    /// `PUNKTFUNK_COMPOSITOR` pin.
    #[default]
    Auto,
    /// Add the virtual display(s); touch nothing else.
    Extend,
    /// Make the group's primary virtual display the OS primary; physical outputs stay enabled.
    Primary,
    /// The managed virtual displays become the only enabled outputs (physical outputs disabled,
    /// restored on teardown).
    Exclusive,
}

/// Admission when a *different* client connects while a display/session is already live and asks for
/// a different mode. Enforced by [`super::admission`] before the Welcome is sent, so a `reject` is a
/// clean handshake error rather than a half-built session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModeConflict {
    /// Give the new client its own virtual display on the same desktop (today's Linux multi-view).
    #[default]
    Separate,
    /// Stop the existing session(s), tear down / reconfigure, serve the new client.
    Steal,
    /// Admit the new client at the live display's mode (the honest-downgrade convention).
    Join,
    /// Refuse the new client with a clear handshake error.
    Reject,
}

/// Stable display identity, so desktop environments persist per-display config (KDE scaling). The
/// slot this resolves to is carried per backend: the Windows EDID serial + IddCx connector index,
/// KWin's per-slot output name, and the host-persisted Mutter scale map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Identity {
    /// One identity for everything (today's Linux behavior).
    Shared,
    /// One identity per paired client cert fingerprint (today's Windows behavior).
    #[default]
    PerClient,
    /// One identity per (client, resolution) — distinct scaling per resolution, at the cost of
    /// identity slots.
    PerClientMode,
}

/// How group members are arranged in the desktop coordinate space, resolved by `layout::arrange` —
/// which both the `/display/state` readout and (on Linux, KWin only) the per-backend position apply
/// consume, so the answer is computed in exactly one place.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    /// Left-to-right in acquire order, top-aligned (deterministic default).
    #[default]
    AutoRow,
    /// Per-identity-slot offsets from [`Layout::positions`] (console-arranged).
    Manual,
}

/// A desktop-space offset for a display (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

/// Group layout: the arrangement mode plus, for [`LayoutMode::Manual`], per-slot offsets keyed by
/// identity-slot id (string keys for stable JSON).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Layout {
    #[serde(default)]
    pub mode: LayoutMode,
    /// Keys are the **canonical decimal** identity-slot id (`"1"`..`"15"`) — the exact string
    /// `arrange` looks a member up by. [`DisplayPolicy::sanitized`] re-canonicalizes them on write
    /// (`"01"` → `"1"`) and drops anything that is not a slot id, because a key that never matches is
    /// a pin the operator can see in the console and in `GET /display/settings` while every session
    /// silently auto-rows past it.
    #[serde(default)]
    pub positions: BTreeMap<String, Position>,
}

/// How a session that **launches a game** (a library id on the Hello / apps.json / Decky pin) is
/// served (`design/gamemode-and-dedicated-sessions.md` §5.2). Orthogonal to the preset/lifecycle axes
/// — a top-level [`DisplayPolicy`] field, NOT part of [`EffectivePolicy`], so a preset never clobbers
/// it. Linux-only in effect (a launching Windows session opens into the one desktop).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameSession {
    /// Today's routing: the launch rides whatever session the box is in (managed Steam session on
    /// Bazzite/SteamOS, bare spawn on plain distros, spawned into the live desktop on KWin/Mutter/wlroots).
    #[default]
    Auto,
    /// A launching session always gets its OWN headless gamescope at the client's mode, nesting just
    /// the game — no Steam Big Picture, no game mode. Degrades to `auto` when gamescope is unavailable.
    Dedicated,
}

/// A named bundle of the fields below. `Custom` (the default) means the explicit fields rule; any
/// other preset ignores the stored fields and expands to its own ([`DisplayPolicy::effective`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// The explicit fields below define the policy.
    #[default]
    Custom,
    /// Today's behavior, made explicit.
    Default,
    /// Dedicated headless/couch box: displays + game survive disconnects; whoever connects takes over.
    GamingRig,
    /// A desktop someone also uses physically: never blank the real monitors, never keep ghosts.
    SharedDesktop,
    /// One user at a time with fast reattach; a second user is told the box is busy.
    Hotdesk,
    /// The multi-monitor daily driver: manual arrangement, per-client identity, exclusive.
    Workstation,
}

/// The user-facing display-management policy — what `display-settings.json` holds and what the mgmt
/// API GETs/PUTs. When [`preset`](Self::preset) is not [`Preset::Custom`] the explicit fields are
/// ignored (the console writes one or the other); [`effective`](Self::effective) resolves both to a
/// single [`EffectivePolicy`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DisplayPolicy {
    /// Schema version (currently 1) — lets a future field addition migrate rather than reject. Read
    /// at load time ([`DisplayPolicyStore::load_from`] warns when a file claims a version this host
    /// does not know, then reads it best-effort) and pinned back to the current version on write.
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub preset: Preset,
    #[serde(default)]
    pub keep_alive: KeepAlive,
    #[serde(default)]
    pub topology: Topology,
    #[serde(default)]
    pub mode_conflict: ModeConflict,
    #[serde(default)]
    pub identity: Identity,
    #[serde(default)]
    pub layout: Layout,
    /// Upper bound on simultaneously-live virtual displays (clamped to `1..=16` on write).
    #[serde(default = "default_max_displays")]
    pub max_displays: u32,
    /// How a game-launching session is served (`design/gamemode-and-dedicated-sessions.md` §5.2).
    /// Orthogonal to `preset`/lifecycle — preserved across preset changes; `#[serde(default)]` = `Auto`
    /// so existing `display-settings.json` files are untouched.
    #[serde(default)]
    pub game_session: GameSession,
    /// EXPERIMENTAL (Windows): command physical monitors' panels off over DDC/CI (VCP 0xD6 →
    /// DPMS off) right before an `Exclusive` isolate deactivates them, and back on at restore.
    /// Targets the "connected-but-dark head" periodic-stutter class (monitor standby
    /// auto-input-scan / DP link churn while the virtual display is the sole active display) at
    /// the monitor-firmware level. Best-effort — monitors without DDC/CI (or with it disabled in
    /// the OSD) are skipped. Orthogonal to `preset` (like `game_session`): preserved across
    /// preset changes; `#[serde(default)]` = off so existing `display-settings.json` files are
    /// untouched.
    #[serde(default)]
    pub ddc_power_off: bool,
    /// EXPERIMENTAL (Windows): DISABLE the OPERATOR'S OWN physical monitors' PnP device nodes for
    /// the stream's duration (persistently, so a monitor whose hot-plug events re-arrive stays
    /// disabled) and re-enable them at teardown — the monitors an `Exclusive` isolate
    /// deactivated. Still opt-in, because it takes displays the operator was actually using.
    ///
    /// The *other* selector — external monitors connected but part of NO topology (the standby
    /// TV that was never active, whose input auto-scan / instant-on HPD cycling re-probes the
    /// link every few seconds) — no longer needs this flag: it runs by default, see
    /// [`standby_sink_neutralise`]. Setting this flag still implies it. Targets the same
    /// "connected-but-dark head" periodic-stutter class as [`Self::ddc_power_off`], but at the
    /// Windows-reaction level: a disabled devnode's wake events trigger no PnP arrival, no CCD
    /// re-evaluation, no DWM invalidation. A crash-recovery journal re-enables leftovers on host
    /// startup. Orthogonal to `preset` (like `game_session`); `#[serde(default)]` = off.
    #[serde(default)]
    pub pnp_disable_monitors: bool,
    /// **EXPERIMENTAL, AMD-only in effect: pin connector EDID emulation while streaming** — the
    /// software equivalent of an HPD-holding dummy plug (`pf_win_display::adl_emul`). Locked at
    /// the first Exclusive isolate BEFORE the physicals deactivate (an awake sink answers its
    /// live-EDID read), unlocked at last-member teardown, crash-journaled so a dead host unlocks
    /// on its next start. Targets the standby-sink stall class at its SOURCE: with emulation
    /// pinned the KMD stops servicing the sleeping sink's HPD/DDC/link. Inert without an AMD
    /// driver (`atiadlxx.dll` absent) and on non-Windows. Orthogonal to `preset` (like
    /// `game_session`); `#[serde(default)]` = off.
    #[serde(default)]
    pub edid_lock: bool,
    /// **Mirror a physical monitor instead of creating a virtual display**: the connector name
    /// (`DP-1`, `HDMI-A-2`) sessions should stream, or `None` for the normal virtual-display path.
    ///
    /// Orthogonal to `preset`/lifecycle (like `game_session`): a preset change never clears it, and
    /// `#[serde(default)]` leaves existing `display-settings.json` files untouched. It is a
    /// **host-wide** setting, not per-client — the host-pinned decision of record in
    /// `design/per-monitor-portal-capture.md` §5.3. `PUNKTFUNK_CAPTURE_MONITOR` overrides it (see
    /// [`capture_monitor`]), so an appliance can pin in `host.env` without the console fighting it.
    #[serde(default)]
    pub capture_monitor: Option<String>,
}

/// The schema version this host writes and understands. A file carrying anything else is still read
/// (every field is `#[serde(default)]`, so a newer document degrades to "the axes we know"), but the
/// mismatch is logged — silently treating a future document as ours is how a migration gets skipped.
const CURRENT_VERSION: u32 = 1;

/// Upper bound on `KeepAlive::Duration.seconds` (24 h). Anything longer is `forever` in every
/// practical sense, and the unclamped `u32` a PUT could carry (~136 years) produced a deadline the
/// lifecycle reaper never reaches plus a ~4.29e12 ms `expires_in_ms` in the `/display/state` readout.
/// `forever` is the honest way to say "keep it": it is releasable by design via `POST /display/release`.
const MAX_KEEP_ALIVE_SECS: u32 = 24 * 60 * 60;

/// The highest identity-slot id `identity`'s slot table can ever hand out (its `MAX_ID`) — the upper
/// bound on a usable [`Layout::positions`] key. Mirrored rather than imported because the slot table
/// is a private module; a key above it can never match a member and is dropped at write time.
const MAX_IDENTITY_SLOT: u32 = 15;

fn one() -> u32 {
    1
}
fn default_max_displays() -> u32 {
    4
}

impl Default for DisplayPolicy {
    fn default() -> Self {
        // Bit-for-bit today's behavior (the `default` preset expanded), so an unconfigured host reads
        // the same policy the un-overridden call sites already produce.
        DisplayPolicy {
            version: CURRENT_VERSION,
            preset: Preset::Custom,
            keep_alive: KeepAlive::default(),
            topology: Topology::Auto,
            mode_conflict: ModeConflict::default(),
            identity: Identity::default(),
            layout: Layout::default(),
            max_displays: 4,
            game_session: GameSession::default(),
            ddc_power_off: false,
            pnp_disable_monitors: false,
            edid_lock: false,
            capture_monitor: None,
        }
    }
}

/// The six resolved fields after preset expansion — what the lifecycle/registry and the policy call
/// sites read, and what the mgmt API echoes as the "currently in force" policy. Pure output of
/// [`DisplayPolicy::effective`].
///
/// **Every field is required on the wire, deliberately.** Unlike [`DisplayPolicy`] — which is only
/// ever a *file* — this shape is also the `fields` member of [`CustomPresetInput`], i.e. the request
/// body of `POST /display/presets` and `PUT /display/presets/{id}`, and a *response* member three
/// times over (`DisplaySettingsState.effective`, `PresetInfo.fields`, `CustomPreset.fields`).
/// `#[serde(default)]` here would (a) turn `{"name":"Kiosk","fields":{}}` — or any camelCase typo —
/// from a serde rejection into a 201 storing a preset that expands to six axes nobody chose, and
/// (b) make all six OPTIONAL in the generated OpenAPI schema, so every codegen'd client has to
/// null-check them. The *persisted* catalog's tolerance for an entry written before an axis existed
/// is bought where it belongs, on the read path only: see [`StoredEffectivePolicy`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EffectivePolicy {
    pub keep_alive: KeepAlive,
    pub topology: Topology,
    pub mode_conflict: ModeConflict,
    pub identity: Identity,
    pub layout: Layout,
    pub max_displays: u32,
}

impl DisplayPolicy {
    /// Resolve to the [`EffectivePolicy`]: a named preset expands to its bundle; `Custom` uses the
    /// explicit fields. Pure — the single source of truth shared by the preset docs and the runtime.
    pub fn effective(&self) -> EffectivePolicy {
        if let Some(mut e) = preset_fields(self.preset) {
            // A preset fixes the six behavior fields but honors an explicit manual layout table
            // (positions are data, not behavior — the `workstation` preset only sets the *mode*).
            if self.preset == Preset::Workstation && !self.layout.positions.is_empty() {
                e.layout.positions = self.layout.positions.clone();
            }
            e
        } else {
            EffectivePolicy {
                keep_alive: self.keep_alive,
                topology: self.topology,
                mode_conflict: self.mode_conflict,
                identity: self.identity,
                layout: self.layout.clone(),
                max_displays: self.max_displays,
            }
        }
    }

    /// Clamp fields to their valid ranges (called on write, and on load so a hand-edited file gets
    /// the same treatment as a console PUT). `max_displays` to `1..=16` (the pf-vdisplay connector
    /// ceiling / a sane Linux bound), the linger window to `MAX_KEEP_ALIVE_SECS`, and the manual
    /// layout keys to canonical slot ids.
    pub fn sanitized(mut self) -> Self {
        self.version = CURRENT_VERSION;
        self.max_displays = self.max_displays.clamp(1, 16);
        self.keep_alive = clamp_keep_alive(self.keep_alive);
        self.layout.positions = canonical_positions(std::mem::take(&mut self.layout.positions));
        // A picker that clears its selection sends `""`; that means "no pin", not "match the
        // monitor named empty string" — same normalization the env knob does.
        self.capture_monitor = self
            .capture_monitor
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }
}

/// Clamp a keep-alive's linger window to `MAX_KEEP_ALIVE_SECS`. Shared by [`DisplayPolicy::sanitized`]
/// and [`sanitize_preset_fields`] so a custom preset can never smuggle in a window a direct PUT is
/// refused; `Off`/`Forever` carry no window and pass through untouched.
fn clamp_keep_alive(keep_alive: KeepAlive) -> KeepAlive {
    match keep_alive {
        KeepAlive::Duration { seconds } if seconds > MAX_KEEP_ALIVE_SECS => KeepAlive::Duration {
            seconds: MAX_KEEP_ALIVE_SECS,
        },
        other => other,
    }
}

/// Re-key a manual layout table to canonical identity-slot ids, dropping (loudly) what can never
/// match a member.
///
/// `layout::arrange` looks a pin up by `u32::to_string()` — an exact string match — so `"01"`,
/// `"slot1"`, `" 1"` or `"99"` are accepted by the PUT, echoed back by `GET /display/settings` and
/// then silently ignored at arrange time, leaving the operator with a manual arrangement that never
/// takes effect and no signal anywhere. Parsing here makes `"01"` *work* and makes junk visible in
/// the log at write time rather than invisible at stream time. A key already in canonical form wins
/// over an equivalent non-canonical spelling of the same slot, so the result never depends on
/// `BTreeMap` iteration order.
fn canonical_positions(positions: BTreeMap<String, Position>) -> BTreeMap<String, Position> {
    use std::collections::btree_map::Entry;
    let mut out: BTreeMap<String, Position> = BTreeMap::new();
    for (key, pos) in positions {
        let id = key.parse::<u32>().ok().filter(|id| {
            // `identity`'s slot table only ever hands out 1..=MAX_ID; a pin outside that range is
            // addressed to a display that cannot exist.
            (1..=MAX_IDENTITY_SLOT).contains(id)
        });
        let Some(id) = id else {
            tracing::warn!(
                key = %key,
                "display layout pin keyed by something that is not an identity slot \
                 (1..={MAX_IDENTITY_SLOT}) — dropping it; it could never have been applied"
            );
            continue;
        };
        let canonical = id.to_string();
        let already_canonical = key == canonical;
        match out.entry(canonical) {
            Entry::Vacant(v) => {
                v.insert(pos);
            }
            Entry::Occupied(mut o) if already_canonical => {
                o.insert(pos);
            }
            Entry::Occupied(_) => {}
        }
    }
    out
}

impl EffectivePolicy {
    /// Build a persistable `Custom` [`DisplayPolicy`] that keeps THIS effective behavior but replaces
    /// the arrangement with a **manual** layout at `positions` — the `/display/layout` endpoint's
    /// transform, factored out pure so arranging displays stays orthogonal to the other axes and is
    /// unit-tested without touching the global store. (`Custom` so the explicit fields — incl. the new
    /// layout — rule; a named preset would ignore them.)
    pub fn with_manual_layout(
        &self,
        positions: BTreeMap<String, Position>,
        game_session: GameSession,
        ddc_power_off: bool,
        pnp_disable_monitors: bool,
        edid_lock: bool,
        capture_monitor: Option<String>,
    ) -> DisplayPolicy {
        DisplayPolicy {
            version: CURRENT_VERSION,
            preset: Preset::Custom,
            keep_alive: self.keep_alive,
            topology: self.topology,
            mode_conflict: self.mode_conflict,
            identity: self.identity,
            layout: Layout {
                mode: LayoutMode::Manual,
                positions,
            },
            max_displays: self.max_displays,
            // Preserve the orthogonal axes (EffectivePolicy doesn't carry them). Dropping any of
            // them here would mean "saving a display arrangement silently cleared my setting" —
            // for `capture_monitor` that would swap the streamed screen out from under the operator.
            game_session,
            ddc_power_off,
            pnp_disable_monitors,
            edid_lock,
            capture_monitor,
        }
    }
}

/// The field bundle a named preset expands to; `None` for [`Preset::Custom`]. The single expansion
/// table — the docs' preset table mirrors this and the `presets_match_doc` test guards the shape.
pub fn preset_fields(preset: Preset) -> Option<EffectivePolicy> {
    let base = |keep_alive, topology, mode_conflict, identity, layout_mode| EffectivePolicy {
        keep_alive,
        topology,
        mode_conflict,
        identity,
        layout: Layout {
            mode: layout_mode,
            positions: BTreeMap::new(),
        },
        max_displays: 4,
    };
    Some(match preset {
        Preset::Custom => return None,
        Preset::Default => base(
            KeepAlive::Duration { seconds: 10 },
            Topology::Auto,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::GamingRig => base(
            KeepAlive::Forever,
            Topology::Exclusive,
            ModeConflict::Steal,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::SharedDesktop => base(
            KeepAlive::Off,
            Topology::Extend,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::AutoRow,
        ),
        Preset::Hotdesk => base(
            KeepAlive::Duration { seconds: 300 },
            Topology::Exclusive,
            ModeConflict::Reject,
            Identity::PerClientMode,
            LayoutMode::AutoRow,
        ),
        Preset::Workstation => base(
            KeepAlive::Duration { seconds: 300 },
            Topology::Exclusive,
            ModeConflict::Separate,
            Identity::PerClient,
            LayoutMode::Manual,
        ),
    })
}

/// The persisted policy store: the loaded file value (or `None` when no file exists) behind its
/// JSON path. Mirrors `pf_gpu::GpuPrefStore` — private dir, temp-write + atomic rename,
/// in-memory rollback if the disk write fails.
pub struct DisplayPolicyStore {
    path: PathBuf,
    /// `Some` only when a valid `display-settings.json` was loaded / written — the "console has
    /// configured this host" signal that gates whether the policy call sites override their
    /// historical env/default behavior.
    cur: Mutex<Option<DisplayPolicy>>,
    /// Serializes the whole write transaction (serialize → temp-write → rename → publish to `cur`).
    /// Without it two concurrent `PUT /display/settings` can rename in one order and publish to
    /// memory in the other, leaving the file and the in-memory value disagreeing — exactly what
    /// [`Self::set`]'s contract promises cannot happen. Held *around* the `cur` lock rather than
    /// instead of it, so a reader (`get`, on the acquire path) never blocks on disk I/O.
    write: Mutex<()>,
}

impl DisplayPolicyStore {
    /// Load from `path`. A missing file ⇒ unconfigured (`None`); a corrupt file ⇒ best-effort
    /// per-axis salvage, and only a file we cannot make any sense of falls back to unconfigured with
    /// a warning (never fail host startup over a settings file).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&path, &bytes),
            // A settings file that exists but cannot be READ (EACCES after a bad chown, EIO on a
            // failing disk) is NOT the same thing as an unconfigured host, and the two used to be
            // folded into one silent `None` — the console's entire configuration reverting to
            // built-in defaults with nothing in the log to say why. Only NotFound is quiet.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "display-settings.json exists but could not be read ({e}) — this host is \
                     running on BUILT-IN DEFAULTS, not on its configured policy");
                None
            }
        };
        DisplayPolicyStore {
            path,
            cur: Mutex::new(cur),
            write: Mutex::new(()),
        }
    }

    /// Parse the settings document, salvaging what we can. Split out of [`Self::load_from`] so the
    /// recovery rules are unit-tested without touching the filesystem.
    ///
    /// Three layers, because the failure that used to discard the whole policy was almost never
    /// "the file is garbage": (1) the strict parse, which is what a console-written file always
    /// takes; (2) a `version` check, so a document from a future host is read best-effort but
    /// *announced* rather than silently treated as ours; (3) a per-axis salvage — every field is
    /// `#[serde(default)]`, so a member that does not deserialize on its own (an enum variant this
    /// build has never heard of, a hand-typed `"max_displays": "four"`) is dropped and the other
    /// eleven survive. Dropping one axis to its default is a much smaller lie than reverting the
    /// operator's entire configuration.
    ///
    /// **Two things the per-axis rule must NOT be applied to**, both discovered after the fact:
    ///
    /// * `preset` is not an axis, it is the SELECTOR: [`DisplayPolicy::effective`] branches on it,
    ///   and its `#[serde(default)]` is `Preset::Custom` — the one value that means "ignore the
    ///   preset, the explicit fields govern". The console PUTs the whole object, so a document that
    ///   names a preset also carries the six explicit fields left over from whatever the operator
    ///   last had. Salvaging an unreadable preset name (a downgrade, a `"gaming_rig"` typo) to
    ///   `Custom` therefore does not degrade *that* axis — it re-points the entire document at
    ///   stale data, e.g. quietly running `exclusive` + `forever` + `steal`. A preset name we
    ///   cannot read means we do not know what the file asks for, so the file is refused whole.
    /// * a salvage that salvaged *nothing* must not report the host as configured. `configured()`
    ///   is the "the console has configured this host" gate and `DisplayPolicy::default()` is NOT
    ///   the unconfigured fallback: `identity::resolve_slot` is passed `Identity::Shared` on both
    ///   Linux backends and consults it only while `configured_effective()` is `None`, whereas the
    ///   default policy's `identity` is `PerClient` — so returning `Some(default)` for a document
    ///   we understood nothing of renames the KWin output and discards KDE's stored per-output
    ///   config. "Nothing" is judged on the RESULT — a salvage that dropped something and landed
    ///   bit-for-bit on `DisplayPolicy::default()` taught us nothing — not on a list of surviving
    ///   keys, since serde ignores unknown members and `version` selects no behaviour, so either
    ///   could survive a document whose every real axis was thrown away.
    fn parse(path: &std::path::Path, bytes: &[u8]) -> Option<DisplayPolicy> {
        let value: serde_json::Value = match serde_json::from_slice(bytes) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "display-settings.json is not valid JSON ({e}) — this host is running on \
                     BUILT-IN DEFAULTS, not on its configured policy");
                return None;
            }
        };
        let claimed = value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(CURRENT_VERSION as u64);
        if claimed != CURRENT_VERSION as u64 {
            tracing::warn!(path = %path.display(), claimed, current = CURRENT_VERSION,
                "display-settings.json claims a schema version this host does not know — reading it \
                 best-effort (unknown axes are ignored); the next write pins it back to the current \
                 version");
        }
        match serde_json::from_value::<DisplayPolicy>(value.clone()) {
            Ok(p) => Some(p.sanitized()),
            Err(e) => {
                let mut obj = match value {
                    serde_json::Value::Object(o) => o,
                    _ => {
                        tracing::warn!(path = %path.display(),
                            "display-settings.json is not a JSON object ({e}) — this host is running \
                             on BUILT-IN DEFAULTS, not on its configured policy");
                        return None;
                    }
                };
                // Probe each member on its own: because every field defaults, a one-key document
                // parses iff that key's value is valid, which localizes the failure with no
                // hand-maintained field list to drift when an axis is added.
                let probe = |key: &str, member: &serde_json::Value| -> bool {
                    let one = serde_json::Value::Object(
                        std::iter::once((key.to_string(), member.clone())).collect(),
                    );
                    serde_json::from_value::<DisplayPolicy>(one).is_ok()
                };
                // The selector first, and it is all-or-nothing (see this function's doc): dropping
                // it to `Custom` would hand governance to the explicit fields the console shipped
                // alongside it, which is a different policy — not a degraded one.
                if let Some(preset) = obj.get("preset") {
                    if !probe("preset", preset) {
                        tracing::warn!(path = %path.display(), preset = %preset,
                            "display-settings.json names a preset this host does not know — the \
                             preset SELECTS the other settings, so falling back to its default \
                             would silently activate whatever explicit fields the file happens to \
                             carry; this host is running on BUILT-IN DEFAULTS instead");
                        return None;
                    }
                }
                let before = obj.len();
                obj.retain(|key, member| {
                    let ok = probe(key.as_str(), &*member);
                    if !ok {
                        tracing::warn!(path = %path.display(), field = %key,
                            "display-settings.json carries an unreadable value for this setting — \
                             falling back to its built-in default and keeping the rest of the policy");
                    }
                    ok
                });
                let dropped = before - obj.len();
                match serde_json::from_value::<DisplayPolicy>(serde_json::Value::Object(obj)) {
                    // "We threw away every setting you wrote" is not a configured host. The test is
                    // on the RESULT rather than on a key list, because a surviving key need not be
                    // an axis at all (serde ignores unknown members, so `{"foo":1,"topology":"…"}`
                    // would otherwise look like a survivor): if the salvage dropped something and
                    // what is left is bit-for-bit the built-in default, we learned nothing from the
                    // file and must say so. See this function's doc for why `Some(default)` is not
                    // a safe stand-in for `None` — it flips Linux identity Shared → PerClient.
                    Ok(p) => {
                        let p = p.sanitized();
                        if dropped > 0 && p == DisplayPolicy::default() {
                            tracing::warn!(path = %path.display(),
                                "display-settings.json had no setting this host could read — this \
                                 host is running on BUILT-IN DEFAULTS, not on its configured policy");
                            return None;
                        }
                        Some(p)
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(),
                            "display-settings.json unreadable even per-setting ({e}) — this host is \
                             running on BUILT-IN DEFAULTS, not on its configured policy");
                        None
                    }
                }
            }
        }
    }

    /// The stored policy, or [`DisplayPolicy::default`] when unconfigured (for the mgmt GET).
    pub fn get(&self) -> DisplayPolicy {
        self.cur.lock().unwrap().clone().unwrap_or_default()
    }

    /// The console-configured policy, or `None` when no settings file exists. Stage-0 call sites use
    /// this to decide whether to override their historical behavior (`None` ⇒ leave it untouched).
    pub fn configured(&self) -> Option<DisplayPolicy> {
        self.cur.lock().unwrap().clone()
    }

    /// The effective (preset-expanded) policy the console configured, or `None` when unconfigured.
    pub fn configured_effective(&self) -> Option<EffectivePolicy> {
        self.configured().map(|p| p.effective())
    }

    /// The game-session routing axis (`design/gamemode-and-dedicated-sessions.md` §5.2). Orthogonal to
    /// the preset — read directly off the stored policy (or the default `Auto` when unconfigured), so a
    /// preset selection never resets it.
    pub fn game_session(&self) -> GameSession {
        self.get().game_session
    }

    /// The experimental DDC/CI panel-off axis — orthogonal to the preset (like
    /// [`Self::game_session`]), read directly off the stored policy (default off when
    /// unconfigured).
    pub fn ddc_power_off(&self) -> bool {
        self.get().ddc_power_off
    }

    /// The experimental PnP monitor-devnode-disable axis — orthogonal to the preset (like
    /// [`Self::game_session`]), read directly off the stored policy (default off when
    /// unconfigured).
    pub fn pnp_disable_monitors(&self) -> bool {
        self.get().pnp_disable_monitors
    }

    /// The experimental AMD connector-EDID-emulation axis — orthogonal to the preset (like
    /// [`Self::game_session`]), read directly off the stored policy (default off when
    /// unconfigured).
    pub fn edid_lock(&self) -> bool {
        self.get().edid_lock
    }

    /// Whether to neutralise CONNECTED-BUT-INACTIVE EXTERNAL sinks (the standby TV/monitor that
    /// is not part of the desktop in any topology) for the stream's duration — **on by default**,
    /// see [`standby_sink_neutralise`]. The user's own displays are NOT in scope here: that is the
    /// deactivated-set selector, still gated on the opt-in [`Self::pnp_disable_monitors`].
    pub fn standby_sink_neutralise(&self) -> bool {
        standby_sink_neutralise(std::env::var("PUNKTFUNK_STANDBY_SINK_KEEP").ok().as_deref())
            || self.get().pnp_disable_monitors
    }

    /// Persist + adopt a new policy (sanitized first). The in-memory value changes only if the disk
    /// write succeeds, so a full disk can't leave memory and file disagreeing — and the whole
    /// transaction runs under [`Self::write`], so neither can two concurrent PUTs.
    pub fn set(&self, policy: DisplayPolicy) -> Result<()> {
        let policy = policy.sanitized();
        let _tx = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(&policy)?;
        // Armed BEFORE the write: `write_secret_file` creates+truncates and only then writes, so an
        // ENOSPC/EIO mid-write returns through `?` with the temp file already on disk. With a fixed
        // `.json.tmp` that leak was self-limiting (the next attempt overwrote it); with a unique
        // name every retry would leave one more full copy in a config dir that is, by hypothesis,
        // already out of space. Nothing in this crate reaps `*.tmp`, and it deliberately must not:
        // the unique name exists so a SECOND host process can write concurrently, and its in-flight
        // temp file is indistinguishable from our stale one.
        let tmp = TmpFile::arm(unique_tmp_path(&self.path));
        pf_paths::write_secret_file(tmp.path(), &bytes)?;
        std::fs::rename(tmp.path(), &self.path)?;
        tmp.published();
        *self.cur.lock().unwrap() = Some(policy);
        Ok(())
    }
}

/// A temp path for a temp-write + atomic-rename that no other writer can be using: `<name>.<pid>.<n>.tmp`.
/// A *fixed* `.json.tmp` is safe only while one thread writes at a time — two host processes over the
/// same config dir (a service plus a hand-run binary, an upgrade overlap) would otherwise interleave
/// their partial writes into one file and rename the mixture over the real one.
fn unique_tmp_path(path: &std::path::Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Owns a [`unique_tmp_path`] for the length of one temp-write + rename, deleting it on **every**
/// early exit — the failed write, the failed rename, and a panic in between. A guard rather than a
/// cleanup at each `?` because unique names have no self-healing: a fixed `.json.tmp` was
/// overwritten by the next attempt, whereas a leaked `<name>.<pid>.<n>.tmp` stays forever and the
/// next attempt mints another one. (What a guard cannot cover: a crash or power loss between write
/// and rename. That leak needs a reaper, and a reaper cannot safely exist here — see the caller.)
struct TmpFile(Option<PathBuf>);

impl TmpFile {
    fn arm(path: PathBuf) -> Self {
        TmpFile(Some(path))
    }
    fn path(&self) -> &std::path::Path {
        self.0.as_deref().expect("disarmed only by consuming self")
    }
    /// The rename succeeded: the path is now the real file (or, if another writer raced us, theirs)
    /// and must NOT be removed.
    fn published(mut self) {
        self.0 = None;
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The process-wide display-policy store (config-dir file), loaded once on first access — the same
/// global-accessor shape as `pf_gpu::prefs`, because display setup happens deep in the
/// capture/vdisplay path where no app state is threaded.
pub fn prefs() -> &'static DisplayPolicyStore {
    static STORE: OnceLock<DisplayPolicyStore> = OnceLock::new();
    STORE.get_or_init(|| {
        DisplayPolicyStore::load_from(pf_paths::config_dir().join("display-settings.json"))
    })
}

// ---------------------------------------------------------------------------------------
// User-defined custom presets (`<config>/display-presets.json`)
// ---------------------------------------------------------------------------------------

/// A user-defined named preset: a saved bundle of the six display-behavior axes (exactly what a
/// built-in [`Preset`] expands to) plus the orthogonal game-session axis, that the operator names
/// and applies from the console.
///
/// Unlike the built-in [`Preset`]s (a closed enum), custom presets are **data** — a catalog stored in
/// `<config>/display-presets.json`. Applying one writes a `Custom` [`DisplayPolicy`] carrying these
/// fields (the console reuses `PUT /display/settings`), so [`DisplayPolicy::effective`] stays pure and
/// the built-in set is never touched. The catalog is decoupled from the active `display-settings.json`:
/// editing or deleting a preset never mutates the running policy (re-apply to adopt a change).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CustomPreset {
    /// Host-assigned, stable for the life of the entry (the `{id}` in the CRUD path).
    pub id: String,
    /// User-facing name shown on the preset card; editable.
    pub name: String,
    /// The six display-behavior axes this preset applies (the same shape a built-in preset expands to).
    pub fields: EffectivePolicy,
    /// The game-session routing this preset applies (orthogonal to the six axes; see [`GameSession`]).
    /// A custom preset captures the operator's *full* setup, so — unlike a built-in preset — applying
    /// one does set this axis.
    #[serde(default)]
    pub game_session: GameSession,
}

/// Request body to create or replace a custom preset (no `id` — the host owns it).
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomPresetInput {
    pub name: String,
    pub fields: EffectivePolicy,
    #[serde(default)]
    pub game_session: GameSession,
}

fn custom_presets_path() -> PathBuf {
    pf_paths::config_dir().join("display-presets.json")
}

/// Clamp a saved preset's fields to their valid ranges — the same bounds [`DisplayPolicy::sanitized`]
/// enforces, so a preset can never carry an out-of-range `max_displays` or linger window that a later
/// apply would silently smuggle past the direct PUT's checks.
fn sanitize_preset_fields(mut fields: EffectivePolicy) -> EffectivePolicy {
    fields.max_displays = fields.max_displays.clamp(1, 16);
    fields.keep_alive = clamp_keep_alive(fields.keep_alive);
    fields.layout.positions = canonical_positions(std::mem::take(&mut fields.layout.positions));
    fields
}

/// What a catalog read recovered: the entries we could make sense of, plus whether anything was lost
/// getting there. `lossy` is the flag the CRUD path checks before it overwrites the file — a save
/// that would drop entries must preserve the original first.
struct CatalogRead {
    presets: Vec<CustomPreset>,
    lossy: bool,
}

/// The lenient READ shape of a catalog entry — [`CustomPreset`] with every behaviour axis defaulted.
///
/// The tolerance a persisted catalog needs (an entry written before an axis existed, or one a hand
/// edit dropped a key from, must still load) and the strictness the mgmt API needs (an omitted axis
/// in a `POST/PUT /display/presets` body is an operator mistake, and all six are required in the
/// generated OpenAPI response schema) are different contracts. They were briefly satisfied by one
/// type — `#[serde(default)]` on [`EffectivePolicy`] itself — which silently loosened the request
/// body: `{"name":"Kiosk","fields":{}}` went from a serde rejection to a 201 storing six axes the
/// operator never chose. Keep them separate: this mirror is `Deserialize`-only, private, and reached
/// exclusively from [`parse_catalog`].
///
/// It must gain a field whenever [`EffectivePolicy`] does; the `From` below is exhaustive by
/// construction, so a forgotten axis is a compile error rather than a silently ignored key.
#[derive(Deserialize)]
struct StoredEffectivePolicy {
    #[serde(default)]
    keep_alive: KeepAlive,
    #[serde(default)]
    topology: Topology,
    #[serde(default)]
    mode_conflict: ModeConflict,
    #[serde(default)]
    identity: Identity,
    #[serde(default)]
    layout: Layout,
    #[serde(default = "default_max_displays")]
    max_displays: u32,
}

impl From<StoredEffectivePolicy> for EffectivePolicy {
    fn from(s: StoredEffectivePolicy) -> Self {
        let StoredEffectivePolicy {
            keep_alive,
            topology,
            mode_conflict,
            identity,
            layout,
            max_displays,
        } = s;
        EffectivePolicy {
            keep_alive,
            topology,
            mode_conflict,
            identity,
            layout,
            max_displays,
        }
    }
}

/// The lenient READ shape of a catalog entry. `id`/`name` stay required — an entry without them is
/// not a preset, and the entry-wise skip already keeps the rest of the catalog.
#[derive(Deserialize)]
struct StoredCustomPreset {
    id: String,
    name: String,
    fields: StoredEffectivePolicy,
    #[serde(default)]
    game_session: GameSession,
}

impl From<StoredCustomPreset> for CustomPreset {
    fn from(s: StoredCustomPreset) -> Self {
        CustomPreset {
            id: s.id,
            name: s.name,
            fields: s.fields.into(),
            game_session: s.game_session,
        }
    }
}

/// Parse the catalog **entry-wise**. Pure (no I/O) so the recovery rules are unit-tested.
///
/// The whole-document `from_slice::<Vec<CustomPreset>>` this replaces made every entry hostage to
/// every other: one hand-edited preset missing a field, or naming an enum variant this build does not
/// know, returned `Vec::new()` — and because the CRUD path is load → mutate → save, the very next
/// "create preset" atomically renamed that one-element vector over the operator's entire catalog.
/// Here a bad entry costs exactly itself, and the caller learns (via `lossy`) that the file on disk
/// holds more than we understood.
fn parse_catalog(bytes: &[u8]) -> CatalogRead {
    let entries: Vec<serde_json::Value> = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e,
                "display-presets.json is not a JSON array of presets — ignoring the custom-preset \
                 catalog; it is preserved as display-presets.json.bad if anything overwrites it");
            return CatalogRead {
                presets: Vec::new(),
                lossy: true,
            };
        }
    };
    let mut presets = Vec::with_capacity(entries.len());
    let mut lossy = false;
    for (i, entry) in entries.into_iter().enumerate() {
        // Keep the id/name for the log even when the body is unreadable — "entry 3" is useless to an
        // operator staring at a console list of names.
        let named = entry
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<unnamed>")
            .to_string();
        match serde_json::from_value::<StoredCustomPreset>(entry) {
            Ok(p) => {
                let mut p = CustomPreset::from(p);
                p.fields = sanitize_preset_fields(p.fields);
                presets.push(p);
            }
            Err(e) => {
                lossy = true;
                tracing::warn!(index = i, name = %named, error = %e,
                    "display-presets.json entry is unreadable — skipping just this preset, the rest \
                     of the catalog is kept");
            }
        }
    }
    CatalogRead { presets, lossy }
}

/// Read + parse the catalog file. `Ok(CatalogRead)` for "absent" (an empty catalog) and for
/// "readable, possibly lossy"; `Err` only when the file exists and the OS refused to hand it over
/// (EACCES/EIO) — which the CRUD path must NOT paper over, because writing back what we could read
/// (nothing) would erase a catalog that is merely unreachable.
fn read_catalog() -> Result<CatalogRead> {
    match std::fs::read(custom_presets_path()) {
        Ok(bytes) => Ok(parse_catalog(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CatalogRead {
            presets: Vec::new(),
            lossy: false,
        }),
        Err(e) => Err(e.into()),
    }
}

/// Copy the catalog aside to `display-presets.json.bad` before a save that would not round-trip it.
/// A copy, not a rename: the original stays in place until the atomic rename replaces it, so a crash
/// in between still leaves a catalog where the host looks for one. Best-effort — failing to preserve
/// a file we already could not fully read must not fail the operator's write.
fn quarantine_catalog() {
    let path = custom_presets_path();
    let bad = path.with_extension("json.bad");
    match std::fs::copy(&path, &bad) {
        Ok(_) => tracing::warn!(path = %bad.display(),
            "the custom-preset catalog held entries this host could not read; the original was \
             copied aside before being rewritten"),
        Err(e) => tracing::warn!(error = %e, path = %bad.display(),
            "could not preserve the unreadable custom-preset catalog before rewriting it"),
    }
}

/// Serializes the catalog's read → mutate → save transaction. The three CRUD entry points are free
/// functions over one shared file, so without this a concurrent add + delete each write back the
/// catalog they loaded and one of the two edits vanishes wholesale.
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

/// Persist the catalog (private dir, temp-write + atomic rename — the [`DisplayPolicyStore::set`]
/// discipline, so a crash mid-write never truncates it). Callers hold [`CATALOG_LOCK`].
fn save_custom_presets(presets: &[CustomPreset]) -> Result<()> {
    let path = custom_presets_path();
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(presets)?;
    // Same guard as `DisplayPolicyStore::set`, for the same reason: a failed `write_secret_file`
    // returns with the temp file already created, and a unique name never gets reused.
    let tmp = TmpFile::arm(unique_tmp_path(&path));
    pf_paths::write_secret_file(tmp.path(), &bytes)?;
    std::fs::rename(tmp.path(), &path)?;
    tmp.published();
    Ok(())
}

/// Load the saved custom presets (empty + non-fatal if the file is absent, unreadable or malformed —
/// a bad catalog never breaks the console's settings GET).
pub fn load_custom_presets() -> Vec<CustomPreset> {
    match read_catalog() {
        Ok(c) => c.presets,
        Err(e) => {
            tracing::warn!(error = %e,
                "display-presets.json exists but could not be read — the console will show no custom \
                 presets; the file itself is untouched");
            Vec::new()
        }
    }
}

/// Should a connected-but-inactive EXTERNAL sink be neutralised while streaming? **Yes unless the
/// operator opts out** with `PUNKTFUNK_STANDBY_SINK_KEEP` (any value but `0`/`off`/empty).
///
/// Default-on because it is measured: 16 alternating pairs on the .173 lab box (standby LG TV on
/// HDMI, IDD-push loopback under continuous cursor damage, 150 s per leg), each leg asserting the
/// treatment actually applied via the sweep's own `PnP-disable: monitor devnode disabled` line.
/// **The baseline produced a hole-free leg 0 times in 16; the sweep produced one 8 times in 16**
/// (Fisher p≈0.002), median hole 6.3 s → 0.7 s, total 370.7 s → 213.6 s. It is an improvement,
/// NOT a cure: 6 of the 16 treated legs still took FRAME-GENERATION holes — the OS dropping
/// composed frames for the virtual head while other processes present normally — some tens of
/// seconds long, and the hole-time rank-sum is only borderline (p≈0.055).
///
/// The operator's own displays are out of scope: this selector only ever sees external physicals
/// that are in NO topology, so an internal laptop panel can never be picked (see
/// `monitor_devnode::disable_connected_inactive`).
pub fn standby_sink_neutralise(opt_out: Option<&str>) -> bool {
    !matches!(opt_out, Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("off"))
}

/// 12 hex chars from the name + wall-clock nanos + a `nonce` — no uuid dep (the host `library`
/// custom-entry id scheme). The nonce exists because the name+nanos pair is NOT unique: two creates
/// of the same name inside one clock tick (a double-clicked Save, two console tabs, a clock that
/// stepped back) hash identically, after which `update_custom_preset`'s `find(|p| p.id == id)`
/// silently edits whichever landed first.
fn preset_id(name: &str, nanos: u128, nonce: u64) -> String {
    hex::encode(&Sha256::digest(format!("{name}:{nanos}:{nonce}").as_bytes())[..6])
}

/// The first id not already taken in `presets`, re-rolling the nonce against a **single** clock read
/// — 48 bits is collision-free in practice only if something actually checks, and re-reading the
/// clock per attempt would make the retry indistinguishable from luck (and untestable).
fn free_preset_id_at(presets: &[CustomPreset], name: &str, nanos: u128) -> String {
    (0u64..)
        .map(|nonce| preset_id(name, nanos, nonce))
        .find(|id| presets.iter().all(|p| &p.id != id))
        .expect("the nonce space is unbounded, so some id is always free")
}

fn free_preset_id(presets: &[CustomPreset], name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    free_preset_id_at(presets, name, nanos)
}

/// Create a custom preset, returning it with its assigned id.
pub fn add_custom_preset(input: CustomPresetInput) -> Result<CustomPreset> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let preset = CustomPreset {
        id: free_preset_id(&presets, &input.name),
        name: input.name,
        fields: sanitize_preset_fields(input.fields),
        game_session: input.game_session,
    };
    presets.push(preset.clone());
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(preset)
}

/// Replace a custom preset's fields (id preserved). `None` ⇒ no preset with that id.
pub fn update_custom_preset(id: &str, input: CustomPresetInput) -> Result<Option<CustomPreset>> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let Some(slot) = presets.iter_mut().find(|p| p.id == id) else {
        return Ok(None);
    };
    slot.name = input.name;
    slot.fields = sanitize_preset_fields(input.fields);
    slot.game_session = input.game_session;
    let updated = slot.clone();
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(Some(updated))
}

/// Delete a custom preset. `false` ⇒ no preset with that id.
pub fn delete_custom_preset(id: &str) -> Result<bool> {
    let _tx = CATALOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let catalog = read_catalog()?;
    let mut presets = catalog.presets;
    let before = presets.len();
    presets.retain(|p| p.id != id);
    if presets.len() == before {
        return Ok(false);
    }
    if catalog.lossy {
        quarantine_catalog();
    }
    save_custom_presets(&presets)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_preset_serde_roundtrips_and_defaults_game_session() {
        let preset = CustomPreset {
            id: "abc123".into(),
            name: "My Rig".into(),
            fields: preset_fields(Preset::GamingRig).unwrap(),
            game_session: GameSession::Dedicated,
        };
        let json = serde_json::to_string(&preset).unwrap();
        assert_eq!(serde_json::from_str::<CustomPreset>(&json).unwrap(), preset);

        // A catalog written before `game_session` existed still loads (defaults to `Auto`).
        let legacy: CustomPreset = serde_json::from_value(serde_json::json!({
            "id": "x",
            "name": "Legacy",
            "fields": serde_json::to_value(preset_fields(Preset::Default).unwrap()).unwrap(),
        }))
        .unwrap();
        assert_eq!(legacy.game_session, GameSession::Auto);
    }

    #[test]
    fn sanitize_preset_fields_clamps_max_displays() {
        let mut f = preset_fields(Preset::Default).unwrap();
        f.max_displays = 999;
        assert_eq!(sanitize_preset_fields(f.clone()).max_displays, 16);
        f.max_displays = 0;
        assert_eq!(sanitize_preset_fields(f).max_displays, 1);
    }

    #[test]
    fn keep_alive_serializes_tagged_on_mode() {
        assert_eq!(
            serde_json::to_value(KeepAlive::Duration { seconds: 300 }).unwrap(),
            serde_json::json!({ "mode": "duration", "seconds": 300 })
        );
        assert_eq!(
            serde_json::to_value(KeepAlive::Off).unwrap(),
            serde_json::json!({ "mode": "off" })
        );
        assert_eq!(
            serde_json::to_value(KeepAlive::Forever).unwrap(),
            serde_json::json!({ "mode": "forever" })
        );
        // Round-trips.
        for k in [
            KeepAlive::Off,
            KeepAlive::Duration { seconds: 42 },
            KeepAlive::Forever,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            assert_eq!(serde_json::from_str::<KeepAlive>(&s).unwrap(), k);
        }
    }

    #[test]
    fn keep_alive_linger_resolution() {
        assert_eq!(KeepAlive::Off.linger(), Linger::Immediate);
        assert_eq!(
            KeepAlive::Duration { seconds: 30 }.linger(),
            Linger::For(Duration::from_secs(30))
        );
        assert_eq!(KeepAlive::Forever.linger(), Linger::Forever);
    }

    #[test]
    fn default_policy_is_todays_behavior() {
        let e = DisplayPolicy::default().effective();
        assert_eq!(e.keep_alive, KeepAlive::Duration { seconds: 10 });
        assert_eq!(e.topology, Topology::Auto);
        assert_eq!(e.mode_conflict, ModeConflict::Separate);
        assert_eq!(e.identity, Identity::PerClient);
        assert_eq!(e.layout.mode, LayoutMode::AutoRow);
    }

    #[test]
    fn custom_uses_explicit_fields_presets_override_them() {
        // Custom: explicit fields flow through.
        let p = DisplayPolicy {
            preset: Preset::Custom,
            keep_alive: KeepAlive::Off,
            topology: Topology::Extend,
            ..DisplayPolicy::default()
        };
        assert_eq!(p.effective().keep_alive, KeepAlive::Off);
        assert_eq!(p.effective().topology, Topology::Extend);

        // A named preset ignores the explicit fields.
        let p = DisplayPolicy {
            preset: Preset::GamingRig,
            keep_alive: KeepAlive::Off, // ignored
            topology: Topology::Extend, // ignored
            ..DisplayPolicy::default()
        };
        let e = p.effective();
        assert_eq!(e.keep_alive, KeepAlive::Forever);
        assert_eq!(e.topology, Topology::Exclusive);
        assert_eq!(e.mode_conflict, ModeConflict::Steal);
    }

    #[test]
    fn workstation_preset_keeps_manual_layout_positions() {
        let mut positions = BTreeMap::new();
        positions.insert("1".to_string(), Position { x: 2560, y: 0 });
        let p = DisplayPolicy {
            preset: Preset::Workstation,
            layout: Layout {
                mode: LayoutMode::AutoRow, // preset forces Manual regardless
                positions,
            },
            ..DisplayPolicy::default()
        };
        let e = p.effective();
        assert_eq!(e.layout.mode, LayoutMode::Manual);
        assert_eq!(
            e.layout.positions.get("1"),
            Some(&Position { x: 2560, y: 0 })
        );
    }

    #[test]
    fn every_preset_expands() {
        for preset in [
            Preset::Default,
            Preset::GamingRig,
            Preset::SharedDesktop,
            Preset::Hotdesk,
            Preset::Workstation,
        ] {
            assert!(preset_fields(preset).is_some(), "{preset:?} must expand");
        }
        assert!(preset_fields(Preset::Custom).is_none());
    }

    #[test]
    fn sanitize_clamps_max_displays_and_pins_version() {
        let p = DisplayPolicy {
            version: 99,
            max_displays: 0,
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(p.version, 1);
        assert_eq!(p.max_displays, 1);
        let p = DisplayPolicy {
            max_displays: 999,
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(p.max_displays, 16);
    }

    #[test]
    fn with_manual_layout_preserves_behavior_and_sets_positions() {
        // Start from a preset's effective behavior (workstation: 5-min linger, exclusive, per-client).
        let eff = DisplayPolicy {
            preset: Preset::Workstation,
            ..DisplayPolicy::default()
        }
        .effective();
        let mut positions = BTreeMap::new();
        positions.insert("1".to_string(), Position { x: 0, y: 0 });
        positions.insert("7".to_string(), Position { x: 2560, y: 0 });
        let p = eff.with_manual_layout(
            positions,
            GameSession::Dedicated,
            true,
            true,
            true,
            Some("DP-2".into()),
        );
        // The orthogonal axes (game-session, DDC power-off, PnP disable, EDID lock,
        // capture-monitor pin) are preserved through the transform — arranging displays must not
        // clear an unrelated setting.
        assert_eq!(p.game_session, GameSession::Dedicated);
        assert!(p.ddc_power_off);
        assert!(p.pnp_disable_monitors);
        assert!(p.edid_lock);
        assert_eq!(p.capture_monitor.as_deref(), Some("DP-2"));
        // Preset drops to Custom so the explicit fields (incl. the layout) rule…
        assert_eq!(p.preset, Preset::Custom);
        // …every other behavior axis is preserved verbatim…
        assert_eq!(p.keep_alive, eff.keep_alive);
        assert_eq!(p.topology, eff.topology);
        assert_eq!(p.mode_conflict, eff.mode_conflict);
        assert_eq!(p.identity, eff.identity);
        assert_eq!(p.max_displays, eff.max_displays);
        // …and the arrangement is the manual layout we asked for, surviving the effective round-trip.
        let e2 = p.effective();
        assert_eq!(e2.layout.mode, LayoutMode::Manual);
        let want = Position { x: 2560, y: 0 };
        assert_eq!(e2.layout.positions.get("7"), Some(&want));
    }

    /// The **file** contract, not the PUT contract. `display-settings.json` must survive every axis
    /// this crate has ever added, so a document written by an older host loads with the missing axes
    /// at their defaults. Do NOT read this as "an omitted field means reset": for `PUT
    /// /display/settings` the same shape is a whole-object replace, which is why an omitted
    /// `capture_monitor` currently clears an operator's pin (§13 11.1). Fixing that means an
    /// `Option<T>`-per-axis DTO merged onto `prefs().get()` in the mgmt handler — the wire type gains
    /// a sibling, this behavior stays exactly as asserted here.
    #[test]
    fn serde_defaults_fill_a_partial_document() {
        // A hand-written file with only a couple of fields loads, the rest defaulting.
        let p: DisplayPolicy =
            serde_json::from_str(r#"{ "preset": "custom", "max_displays": 2 }"#).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.keep_alive, KeepAlive::default());
        assert_eq!(p.topology, Topology::Auto);
        assert_eq!(p.version, 1);
        // A file written before the experimental DDC/PnP axes existed defaults them OFF.
        assert!(!p.ddc_power_off);
        assert!(!p.pnp_disable_monitors);
    }

    #[test]
    fn store_roundtrips_and_gates_on_file_presence() {
        let dir = std::env::temp_dir().join(format!("pf-disp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("display-settings.json");
        let _ = std::fs::remove_file(&path);

        let store = DisplayPolicyStore::load_from(path.clone());
        // Unconfigured: get() yields defaults, configured() is None.
        assert!(store.configured().is_none());
        assert_eq!(store.get(), DisplayPolicy::default());

        // After a write the file gates flip to configured.
        let want = DisplayPolicy {
            preset: Preset::SharedDesktop,
            ..DisplayPolicy::default()
        };
        store.set(want.clone()).unwrap();
        assert_eq!(
            store.configured().as_ref().map(|p| p.preset),
            Some(Preset::SharedDesktop)
        );
        assert_eq!(
            store.configured_effective().unwrap().keep_alive,
            KeepAlive::Off
        );

        // A fresh store reading the same path sees the persisted value.
        let reopened = DisplayPolicyStore::load_from(path.clone());
        assert_eq!(reopened.configured().unwrap().preset, Preset::SharedDesktop);

        let _ = std::fs::remove_file(&path);
    }

    /// Guard for §13 11.1: the merge the mgmt PUT needs has to enumerate every axis, and the way that
    /// goes wrong is a NEW field nobody wires in — silently, because no test counts them. Bump this
    /// number only in the same change that teaches the update path about the new axis.
    #[test]
    fn every_policy_axis_is_accounted_for() {
        let v = serde_json::to_value(DisplayPolicy::default()).unwrap();
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            13,
            "a display-policy axis was added or removed: {keys:?} — wire it into the mgmt PUT's \
             per-axis merge (and into `EffectivePolicy` if it is a behavior axis) before bumping this"
        );
    }

    #[test]
    fn sanitize_clamps_an_absurd_linger_to_a_day() {
        // ~136 years of `duration` is a deadline the reaper never reaches and a nonsense
        // `expires_in_ms`; `forever` is how you say "keep it" (and stays releasable).
        let p = DisplayPolicy {
            keep_alive: KeepAlive::Duration { seconds: u32::MAX },
            ..DisplayPolicy::default()
        }
        .sanitized();
        assert_eq!(
            p.keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
        // A window inside the bound, and the window-less variants, pass through untouched.
        for k in [
            KeepAlive::Duration { seconds: 300 },
            KeepAlive::Off,
            KeepAlive::Forever,
        ] {
            let p = DisplayPolicy {
                keep_alive: k,
                ..DisplayPolicy::default()
            }
            .sanitized();
            assert_eq!(p.keep_alive, k);
        }
        // The same bound applies through the custom-preset catalog, which would otherwise be a way
        // to smuggle a window past the direct PUT.
        let mut f = preset_fields(Preset::Default).unwrap();
        f.keep_alive = KeepAlive::Duration { seconds: u32::MAX };
        assert_eq!(
            sanitize_preset_fields(f).keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
    }

    #[test]
    fn sanitize_canonicalizes_layout_keys_and_drops_unusable_ones() {
        let mut positions = BTreeMap::new();
        positions.insert("01".to_string(), Position { x: 10, y: 0 }); // zero-padded: usable
        positions.insert("2".to_string(), Position { x: 20, y: 0 }); // already canonical
        positions.insert("slot3".to_string(), Position { x: 30, y: 0 }); // not a slot id
        positions.insert(" 4".to_string(), Position { x: 40, y: 0 }); // whitespace: not a slot id
        positions.insert("99".to_string(), Position { x: 50, y: 0 }); // no such slot, ever
        positions.insert("0".to_string(), Position { x: 60, y: 0 }); // slots start at 1
        let p = DisplayPolicy {
            layout: Layout {
                mode: LayoutMode::Manual,
                positions,
            },
            ..DisplayPolicy::default()
        }
        .sanitized();
        let got = &p.layout.positions;
        assert_eq!(got.len(), 2, "only the two real slot pins survive: {got:?}");
        // "01" now resolves — `arrange` looks members up by `u32::to_string()`, so before this it was
        // a pin the console showed and no session ever honored.
        assert_eq!(got.get("1"), Some(&Position { x: 10, y: 0 }));
        assert_eq!(got.get("2"), Some(&Position { x: 20, y: 0 }));
    }

    #[test]
    fn canonical_positions_prefers_the_canonical_spelling_over_a_duplicate() {
        // Two spellings of slot 1 in one table: the answer must not depend on BTreeMap order.
        let mut positions = BTreeMap::new();
        positions.insert("01".to_string(), Position { x: 10, y: 0 });
        positions.insert("1".to_string(), Position { x: 11, y: 0 });
        let out = canonical_positions(positions);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("1"), Some(&Position { x: 11, y: 0 }));
    }

    #[test]
    fn a_readable_file_is_sanitized_on_load_not_only_on_write() {
        // A hand-edited settings file gets exactly the treatment a console PUT gets — otherwise the
        // clamps are enforceable only by whoever last used the web UI.
        let doc = br#"{ "version": 1, "max_displays": 999,
                       "keep_alive": { "mode": "duration", "seconds": 4294967295 },
                       "layout": { "mode": "manual", "positions": { "01": { "x": 5, "y": 6 } } } }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, 16);
        assert_eq!(
            p.keep_alive,
            KeepAlive::Duration {
                seconds: MAX_KEEP_ALIVE_SECS
            }
        );
        assert_eq!(p.layout.positions.get("1"), Some(&Position { x: 5, y: 6 }));
    }

    #[test]
    fn one_unreadable_axis_does_not_discard_the_whole_policy() {
        // `topology: "hologram"` is an enum variant this build has never heard of — the whole-document
        // parse fails on it. Before the per-axis salvage that reverted the host to built-in defaults:
        // the operator's capture-monitor pin, their preset, their linger, all silently gone.
        let doc = br#"{ "version": 1, "preset": "hotdesk", "topology": "hologram",
                        "max_displays": 3, "capture_monitor": "DP-2" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc)
            .expect("a single bad axis must not discard the document");
        assert_eq!(p.topology, Topology::Auto, "the bad axis falls to default");
        assert_eq!(p.preset, Preset::Hotdesk, "…and everything else survives");
        assert_eq!(p.max_displays, 3);
        assert_eq!(p.capture_monitor.as_deref(), Some("DP-2"));
    }

    #[test]
    fn a_mistyped_scalar_falls_back_to_its_own_default_not_zero() {
        // `max_displays` has a non-`Default::default()` serde default (4); the salvage must go through
        // the same path, or a typo would leave the host admitting zero displays.
        let doc = br#"{ "max_displays": "four", "preset": "workstation" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, default_max_displays());
        assert_eq!(p.preset, Preset::Workstation);
    }

    #[test]
    fn a_document_that_is_not_a_policy_at_all_is_unconfigured() {
        // Only genuine nonsense falls back to "unconfigured" — and it must, since half-reading a
        // truncated file would be worse than admitting we have nothing.
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{ not json").is_none());
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"[1, 2, 3]").is_none());
        // An empty object IS a policy: every axis defaults.
        assert_eq!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{}"),
            Some(DisplayPolicy::default())
        );
    }

    #[test]
    fn a_future_schema_version_is_still_read() {
        // Rejecting a version we don't know would revert the host to defaults on a downgrade — the
        // exact failure the version field exists to avoid. We read it and log the mismatch.
        let doc = br#"{ "version": 7, "preset": "gaming-rig", "future_axis": { "a": 1 } }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.preset, Preset::GamingRig);
        assert_eq!(
            p.version, CURRENT_VERSION,
            "sanitized back to what we write"
        );
    }

    #[test]
    fn a_missing_file_and_a_present_one_are_different_states() {
        let dir = std::env::temp_dir().join(format!("pf-disp-io-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("display-settings.json");
        let _ = std::fs::remove_file(&path);
        assert!(DisplayPolicyStore::load_from(path.clone())
            .configured()
            .is_none());
        std::fs::write(&path, br#"{"preset":"hotdesk"}"#).unwrap();
        assert_eq!(
            DisplayPolicyStore::load_from(path.clone())
                .configured()
                .unwrap()
                .preset,
            Preset::Hotdesk
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unique_tmp_paths_never_repeat_and_stay_in_the_same_dir() {
        let path = PathBuf::from("/tmp/pf/display-settings.json");
        let a = unique_tmp_path(&path);
        let b = unique_tmp_path(&path);
        assert_ne!(a, b, "a fixed temp name is what two writers collide on");
        assert_eq!(
            a.parent(),
            path.parent(),
            "rename must stay intra-filesystem"
        );
        assert!(a.to_string_lossy().ends_with(".tmp"));
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("display-settings.json."));
    }

    #[test]
    fn one_bad_catalog_entry_costs_only_itself() {
        // The whole-document parse this replaces returned an EMPTY vector here — and the next
        // "create preset" renamed that empty vector over the operator's catalog.
        let doc = br#"[
            { "id": "a", "name": "Keep", "fields": { "keep_alive": { "mode": "forever" },
              "topology": "exclusive", "mode_conflict": "steal", "identity": "per-client",
              "layout": { "mode": "auto-row", "positions": {} }, "max_displays": 2 } },
            { "id": "b", "name": "Broken", "fields": { "topology": "hologram" } },
            { "id": "c", "name": "Also kept", "fields": {} }
        ]"#;
        let read = parse_catalog(doc);
        assert!(read.lossy, "the caller must know an entry was dropped");
        let names: Vec<&str> = read.presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["Keep", "Also kept"]);
        // The entry that omitted every field defaults them (and gets the real `max_displays`
        // default, not 0) — a catalog written before an axis existed still loads.
        let c = &read.presets[1];
        assert_eq!(c.fields.max_displays, default_max_displays());
        assert_eq!(c.fields.keep_alive, KeepAlive::default());
        assert_eq!(c.fields.topology, Topology::Auto);
        assert_eq!(c.game_session, GameSession::Auto);
        // Entries are clamped on read, exactly like a fresh write.
        assert_eq!(read.presets[0].fields.max_displays, 2);
    }

    #[test]
    fn a_clean_catalog_is_not_lossy_and_a_broken_one_is() {
        let clean = parse_catalog(b"[]");
        assert!(clean.presets.is_empty() && !clean.lossy);
        // Not an array at all: nothing recovered, and `lossy` so the CRUD path preserves the file
        // instead of renaming an empty vector over it.
        let junk = parse_catalog(br#"{"presets":[]}"#);
        assert!(junk.presets.is_empty() && junk.lossy);
    }

    #[test]
    fn preset_ids_never_collide_with_the_catalog() {
        let entry = |id: &str| CustomPreset {
            id: id.to_string(),
            name: "Same name".into(),
            fields: preset_fields(Preset::Default).unwrap(),
            game_session: GameSession::Auto,
        };
        // Same name, same nanosecond — the collision a double-clicked Save produces. Before the
        // re-roll both creates got one id and `update_custom_preset` then edited whichever came
        // first; the second create must now step to the next nonce.
        let first = free_preset_id_at(&[], "Same name", 42);
        assert_eq!(first, preset_id("Same name", 42, 0));
        let second = free_preset_id_at(&[entry(&first)], "Same name", 42);
        assert_eq!(second, preset_id("Same name", 42, 1));
        assert_ne!(first, second);
        assert_eq!(first.len(), 12, "12 hex chars, unchanged id shape");
    }

    /// The REQUEST contract of `POST/PUT /display/presets`. `EffectivePolicy` is not a file-only
    /// shape: `#[serde(default)]` on its fields turns `{"fields":{}}` — and every camelCase typo —
    /// into a 201 that stores six axes the operator never chose, and drops all six from `required`
    /// in the generated OpenAPI schema. The catalog's leniency lives in `StoredEffectivePolicy`
    /// instead (see the next test), which is exactly what lets this stay strict.
    #[test]
    fn the_wire_shape_of_an_effective_policy_requires_every_axis() {
        assert!(serde_json::from_str::<EffectivePolicy>("{}").is_err());
        // The console's own body, minus one axis — an omission is a mistake, not a default.
        let almost = r#"{ "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                          "mode_conflict": "steal", "identity": "per-client",
                          "layout": { "mode": "auto-row", "positions": {} } }"#;
        assert!(serde_json::from_str::<EffectivePolicy>(almost).is_err());
        let mut full: serde_json::Value = serde_json::from_str(almost).unwrap();
        full["max_displays"] = serde_json::json!(2);
        assert!(serde_json::from_value::<EffectivePolicy>(full).is_ok());
        // Same strictness through the request body itself.
        assert!(
            serde_json::from_str::<CustomPresetInput>(r#"{ "name": "Kiosk", "fields": {} }"#)
                .is_err()
        );
    }

    /// …and the FILE contract, which is the opposite one: a catalog entry written before an axis
    /// existed still loads, because the read path goes through the lenient private mirror.
    #[test]
    fn a_catalog_entry_may_omit_an_axis_even_though_the_wire_shape_may_not() {
        let doc = br#"[{ "id": "c", "name": "Old", "fields": { "topology": "exclusive" } }]"#;
        let read = parse_catalog(doc);
        assert!(!read.lossy, "an omitted axis is not a lost entry");
        assert_eq!(read.presets.len(), 1);
        let f = &read.presets[0].fields;
        assert_eq!(f.topology, Topology::Exclusive);
        assert_eq!(f.max_displays, default_max_displays(), "not 0");
        assert_eq!(f.keep_alive, KeepAlive::default());
    }

    #[test]
    fn an_unreadable_preset_name_refuses_the_whole_document() {
        // `preset` SELECTS the other axes, so it is not salvageable per-axis: dropping it to its
        // `Custom` default would activate the stale explicit fields the console's whole-object PUT
        // always ships alongside the chosen preset — here `exclusive` + `forever` + `steal`, a
        // policy nobody selected, on a file whose only unreadable byte was the preset name.
        let doc = br#"{ "version": 1, "preset": "kiosk",
                        "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                        "mode_conflict": "steal", "identity": "per-client",
                        "layout": { "mode": "auto-row", "positions": {} }, "max_displays": 4 }"#;
        assert!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none(),
            "a preset name this build cannot read means we do not know what the file asks for"
        );
        // A preset name we DO know still salvages its neighbours (the case above must not have
        // turned the salvage off wholesale).
        let doc = br#"{ "preset": "hotdesk", "topology": "hologram" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.preset, Preset::Hotdesk);
        assert_eq!(p.topology, Topology::Auto);
    }

    #[test]
    fn a_document_we_understood_nothing_of_is_unconfigured_not_default() {
        // `configured() == None` is the gate the "host keeps its historical default" contract rests
        // on, and `DisplayPolicy::default()` is NOT that default: `resolve_slot` is passed
        // `Identity::Shared` on both Linux backends and only consults it while `configured_effective()`
        // is `None`, while the default policy's identity is `PerClient` — so `Some(default)` here
        // renames the KWin output and throws away KDE's stored per-output config.
        let doc = br#"{ "topology": "hologram", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // `version` survives its own probe but selects no behaviour, so it cannot make a document
        // "configured" on its own — nor can a key serde never looked at.
        let doc = br#"{ "version": 1, "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        let doc = br#"{ "note": "mine", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // One survivor IS a configuration — the salvage still does its job.
        let doc = br#"{ "identity": "perclient", "max_displays": 2 }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.identity, Identity::default());
    }

    #[test]
    fn standby_sink_neutralise_is_on_unless_explicitly_kept() {
        // Unset, empty, and the two "off" spellings all mean: neutralise (the measured default).
        assert!(standby_sink_neutralise(None));
        assert!(standby_sink_neutralise(Some("")));
        assert!(standby_sink_neutralise(Some("0")));
        assert!(standby_sink_neutralise(Some("off")));
        assert!(standby_sink_neutralise(Some("OFF")));
        // Anything else is the operator asking to keep the sink alive.
        assert!(!standby_sink_neutralise(Some("1")));
        assert!(!standby_sink_neutralise(Some("keep")));
    }

    #[test]
    fn a_temp_file_is_removed_unless_the_rename_published_it() {
        let dir = std::env::temp_dir().join(format!("pf-disp-tmp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // The write-failed / rename-failed path: every early return must take the temp file with
        // it, because a unique name is never reused and nothing reaps `*.tmp`.
        let leaked = unique_tmp_path(&dir.join("display-settings.json"));
        std::fs::write(&leaked, b"partial").unwrap();
        drop(TmpFile::arm(leaked.clone()));
        assert!(!leaked.exists(), "a failed write must not leave {leaked:?}");
        // The published path: the file is the real one now, and removing it would delete the write.
        let kept = unique_tmp_path(&dir.join("display-settings.json"));
        std::fs::write(&kept, b"published").unwrap();
        TmpFile::arm(kept.clone()).published();
        assert!(kept.exists());
        let _ = std::fs::remove_file(&kept);
        let _ = std::fs::remove_dir(&dir);
    }
}
