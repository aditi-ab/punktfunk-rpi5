//! Virtual-display management policy: create, keep-alive, and arrange.
//!
//! Pure config above the per-compositor [`VirtualDisplay`](super) backends.
//! [`DisplayPolicy`] and named [`Preset`]s persist to
//! `<config>/display-settings.json`; the web console edits them.
//!
//! Precedence matches GPU preference (console > env pin > default): a valid
//! `display-settings.json` wins; if it is absent,
//! [`DisplayPolicyStore::configured`] is `None` and call sites keep their
//! env/default path. The store is re-read on acquire/teardown, so a console
//! write applies on the next connect without a host restart.
//!
//! Evidence: `design/display-management.md`. Tests cover preset expansion,
//! [`DisplayPolicy::effective`], and [`KeepAlive`] linger. The store follows
//! `gpu.rs`: private dir, temp-write + atomic rename, in-memory rollback
//! on a failed write.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

/// Linger after the last client detaches. Tagged on `mode` so the web form
/// and OpenAPI stay `{"mode":"off"}` / `{"mode":"duration","seconds":N}` /
/// `{"mode":"forever"}`. On gamescope's bare spawn this also keeps the
/// nested session and its game.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum KeepAlive {
    Off,
    /// Linger `seconds` after the last session leaves; a reconnect inside
    /// the window reuses the display.
    Duration {
        /// Linger seconds, clamped to `0..=86400` on write. Longer is
        /// `forever` in practice; unclamped `u32` is ~136 years and a
        /// nonsense `expires_in_ms`.
        seconds: u32,
    },
    /// Until host shutdown or `POST /display/release` (force-releases
    /// `Pinned` like `Lingering`). The `gaming-rig` preset selects this.
    Forever,
}

impl Default for KeepAlive {
    fn default() -> Self {
        // 10 s covers a client's own reconnect (mode change, network blip)
        // without leaving a leftover virtual display if they walk away.
        KeepAlive::Duration { seconds: 10 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Linger {
    Immediate,
    For(Duration),
    /// Never auto-tear-down (`Pinned`).
    Forever,
}

impl KeepAlive {
    pub fn linger(self) -> Linger {
        match self {
            KeepAlive::Off => Linger::Immediate,
            KeepAlive::Duration { seconds } => Linger::For(Duration::from_secs(seconds as u64)),
            KeepAlive::Forever => Linger::Forever,
        }
    }
}

/// Host topology while managed virtual displays are up.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Resolved at acquire ([`super::effective_topology`]): exclusive on
    /// Windows and auto-detected Linux desktop; extend under an explicit
    /// `PUNKTFUNK_COMPOSITOR` pin.
    #[default]
    Auto,
    /// Add the virtual display(s); leave physical outputs alone.
    Extend,
    /// Group's primary virtual display becomes OS primary; physicals stay on.
    Primary,
    /// Only managed virtual displays stay enabled; physicals restore on teardown.
    Exclusive,
}

/// Admission when a new client asks for a different mode than the live
/// display. [`super::admission`] runs this before Welcome so `reject` is a
/// handshake error, not a half-built session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModeConflict {
    /// Own virtual display on the same desktop.
    #[default]
    Separate,
    /// Stop existing sessions, reconfigure, serve the new client.
    Steal,
    /// Admit the new client at the live display's mode.
    Join,
    Reject,
}

/// Stable identity so DEs persist per-display config (KDE scaling). Carried
/// as Windows EDID serial + IddCx connector index, KWin per-slot output
/// name, and the host-persisted Mutter scale map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Identity {
    Shared,
    #[default]
    PerClient,
    /// One identity per (client, resolution). Distinct scaling per mode
    /// burns slot ids.
    PerClientMode,
}

/// Arrangement in desktop space. Computed only in `layout::arrange`:
/// `/display/state` and (Linux, KWin only) position apply both consume it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutMode {
    /// Left-to-right in acquire order, top-aligned.
    #[default]
    AutoRow,
    /// Per-identity-slot offsets from [`Layout::positions`].
    Manual,
}

/// Desktop-space offset (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Position {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Layout {
    #[serde(default)]
    pub mode: LayoutMode,
    /// Canonical decimal identity-slot ids (`"1"`..`"15"`) — the exact
    /// string `arrange` looks up. [`DisplayPolicy::sanitized`] maps `"01"`
    /// → `"1"` and drops non-ids; a key that never matches is a pin the
    /// console still shows while every session auto-rows past it.
    #[serde(default)]
    pub positions: BTreeMap<String, Position>,
}

/// How a session that launches a game is served
/// (`design/gamemode-and-dedicated-sessions.md`). Top-level
/// [`DisplayPolicy`] field, not part of [`EffectivePolicy`], so a preset
/// never clobbers it. Linux-only in effect.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameSession {
    /// Launch rides the box's current session (managed Steam, bare spawn,
    /// or the live desktop on KWin/Mutter/wlroots).
    #[default]
    Auto,
    /// Own headless gamescope at the client's mode, game only. Degrades to
    /// `auto` when gamescope is missing.
    Dedicated,
}

/// Named bundle of the fields below. `Custom` uses the explicit fields;
/// any other preset ignores them and expands ([`DisplayPolicy::effective`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    #[default]
    Custom,
    Default,
    /// Headless/couch: displays + game survive disconnects; next client takes over.
    GamingRig,
    /// Physical desktop: never blank real monitors, never linger virtuals.
    SharedDesktop,
    /// One user at a time with fast reattach; a second user is refused.
    Hotdesk,
    /// Multi-monitor: manual arrangement, per-client identity, exclusive.
    Workstation,
}

/// File + mgmt GET/PUT shape. When [`preset`](Self::preset) is not
/// [`Preset::Custom`], explicit fields are ignored; [`effective`](Self::effective)
/// resolves both to [`EffectivePolicy`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DisplayPolicy {
    /// Schema version. Unknown versions load best-effort
    /// ([`DisplayPolicyStore::load_from`] warns) and write pins current.
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
    /// Simultaneous live virtual displays. Clamped to `1..=16` (connector ceiling).
    #[serde(default = "default_max_displays")]
    pub max_displays: u32,
    /// Game-launch routing. Orthogonal to `preset`; `#[serde(default)]` is
    /// `Auto` so older `display-settings.json` files stay untouched.
    #[serde(default)]
    pub game_session: GameSession,
    /// Windows: DDC/CI panel off (VCP 0xD6) before Exclusive isolate, on at
    /// restore. Cuts standby auto-input-scan / DP link churn on a dark
    /// physical. Best-effort; no DDC/CI → skip. Orthogonal to `preset`; default off.
    #[serde(default)]
    pub ddc_power_off: bool,
    /// Windows: disable the operator's physical monitor PnP nodes for the
    /// stream and re-enable at teardown. Persistent so a re-HPD stays off.
    /// Opt-in. Inactive externals are default-on via
    /// [`standby_sink_neutralise`]; this flag still implies that. A crash
    /// journal re-enables leftovers. Orthogonal to `preset`; default off.
    #[serde(default)]
    pub pnp_disable_monitors: bool,
    /// Windows/AMD: pin connector EDID emulation while streaming
    /// (`pf_win_display::adl_emul`). Locked at first Exclusive isolate
    /// before physicals deactivate (awake sink answers the live-EDID read),
    /// unlocked at last-member teardown, crash-journaled. Inert without
    /// `atiadlxx.dll`. Orthogonal to `preset`; default off.
    #[serde(default)]
    pub edid_lock: bool,
    /// Stream this physical connector (`DP-1`, `HDMI-A-2`) instead of a
    /// virtual display; `None` is the virtual path. Host-wide, orthogonal
    /// to `preset`. `PUNKTFUNK_CAPTURE_MONITOR` overrides it so `host.env`
    /// can pin without a console write undoing it.
    #[serde(default)]
    pub capture_monitor: Option<String>,
}

/// Schema this host writes. Other versions still load (fields default) but
/// the mismatch is logged — silent treat-as-ours skips a migration.
const CURRENT_VERSION: u32 = 1;

/// Cap on `KeepAlive::Duration.seconds` (24 h). Longer is `forever`;
/// unclamped `u32` is a deadline the reaper never reaches. `Forever` stays
/// releasable via `POST /display/release`.
const MAX_KEEP_ALIVE_SECS: u32 = 24 * 60 * 60;

/// Highest identity-slot id (`identity` `MAX_ID`). Mirrored: the slot table
/// is private. A key above this can never match and is dropped on write.
const MAX_IDENTITY_SLOT: u32 = 15;

fn one() -> u32 {
    1
}
fn default_max_displays() -> u32 {
    4
}

impl Default for DisplayPolicy {
    fn default() -> Self {
        // Bit-for-bit the `default` preset, so an unconfigured host matches
        // the un-overridden call sites.
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

/// The six axes after preset expansion. What lifecycle/registry read, and
/// what mgmt echoes as in-force.
///
/// Every field is required on the wire. This type is also
/// [`CustomPresetInput::fields`] (`POST/PUT /display/presets`) and a
/// response member three times. `#[serde(default)]` would turn
/// `{"name":"Kiosk","fields":{}}` into a 201 storing six unchosen axes and
/// make all six optional in OpenAPI. Catalog tolerance for older entries
/// lives on the read path only: [`StoredEffectivePolicy`].
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
    pub fn effective(&self) -> EffectivePolicy {
        if let Some(mut e) = preset_fields(self.preset) {
            // A preset fixes the six axes; workstation still honors an
            // explicit positions table (data, not behavior).
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

    /// Clamp on write and on load. `max_displays` to `1..=16` (connector
    /// ceiling), linger to `MAX_KEEP_ALIVE_SECS`, layout keys to slot ids.
    pub fn sanitized(mut self) -> Self {
        self.version = CURRENT_VERSION;
        self.max_displays = self.max_displays.clamp(1, 16);
        self.keep_alive = clamp_keep_alive(self.keep_alive);
        self.layout.positions = canonical_positions(std::mem::take(&mut self.layout.positions));
        // A cleared picker sends `""`; that is "no pin", not a monitor named empty.
        self.capture_monitor = self
            .capture_monitor
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        self
    }
}

/// Shared by [`DisplayPolicy::sanitized`] and [`sanitize_preset_fields`] so
/// a custom preset cannot smuggle a window a direct PUT would refuse.
fn clamp_keep_alive(keep_alive: KeepAlive) -> KeepAlive {
    match keep_alive {
        KeepAlive::Duration { seconds } if seconds > MAX_KEEP_ALIVE_SECS => KeepAlive::Duration {
            seconds: MAX_KEEP_ALIVE_SECS,
        },
        other => other,
    }
}

/// Re-key manual layout pins to canonical slot ids; drop what can never match.
///
/// `layout::arrange` looks up `u32::to_string()`, so `"01"` / `"slot1"` /
/// `"99"` round-trip through GET and then silently no-op. Parse here so
/// `"01"` works and junk is logged at write time. A canonical key wins over
/// a duplicate spelling, independent of `BTreeMap` order.
fn canonical_positions(positions: BTreeMap<String, Position>) -> BTreeMap<String, Position> {
    use std::collections::btree_map::Entry;
    let mut out: BTreeMap<String, Position> = BTreeMap::new();
    for (key, pos) in positions {
        let id = key.parse::<u32>().ok().filter(|id| {
            // Slot table only hands out 1..=MAX_ID; a pin outside cannot exist.
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
    /// Persistable `Custom` policy with this behavior and a **manual**
    /// layout. `/display/layout` uses this so arranging stays orthogonal
    /// to the other axes. `Custom` so the new layout is not ignored.
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
            // EffectivePolicy does not carry these. Dropping any would mean
            // "saving a layout cleared my capture pin / game-session / …".
            game_session,
            ddc_power_off,
            pnp_disable_monitors,
            edid_lock,
            capture_monitor,
        }
    }
}

/// Named-preset expansion; `None` for [`Preset::Custom`]. `presets_match_doc`
/// guards the table.
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

/// Loaded file (or `None` if absent). Same discipline as
/// `pf_gpu::GpuPrefStore`: private dir, temp-write + atomic rename,
/// in-memory rollback if the disk write fails.
pub struct DisplayPolicyStore {
    path: PathBuf,
    /// `Some` only after a valid file load/write — the gate that lets call
    /// sites override env/default behavior.
    cur: Mutex<Option<DisplayPolicy>>,
    /// Serializes the write: serialize → temp-write → rename → publish to
    /// `cur`. Without it two concurrent PUTs can rename in one order and
    /// publish in the other. Held *around* `cur` so `get` never waits on disk.
    write: Mutex<()>,
}

impl DisplayPolicyStore {
    /// Missing file ⇒ unconfigured. Corrupt ⇒ per-axis salvage. Unreadable
    /// ⇒ unconfigured + warn (never fail host startup).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => Self::parse(&path, &bytes),
            // Exists-but-unreadable (EACCES, EIO) is not unconfigured: folding
            // it into silent `None` reverted the console to defaults with no log.
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

    /// Parse with salvage. Split from [`Self::load_from`] so recovery is unit-tested.
    ///
    /// Three layers: (1) strict parse for console-written files; (2) version
    /// check so a future document is announced, not silently treated as ours;
    /// (3) per-axis salvage — a member that fails alone is dropped, the rest
    /// survive. Dropping one axis is smaller than reverting the whole file.
    ///
    /// Two things salvage must not do:
    ///
    /// * `preset` is the selector, not an axis. Its default is `Custom`, which
    ///   would hand governance to leftover explicit fields (`exclusive` +
    ///   `forever` + `steal`). An unreadable preset name refuses the file.
    /// * Salvaging *nothing* must not report configured. `configured()` is the
    ///   console-has-configured gate; `DisplayPolicy::default()` is `PerClient`
    ///   identity, while unconfigured Linux uses `Shared`. Judge "nothing" on
    ///   the result (dropped something and landed on default), not surviving
    ///   keys — serde ignores unknowns and `version` selects no behaviour.
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
                // Probe one member at a time: a one-key document parses iff
                // that value is valid. No hand-maintained field list.
                let probe = |key: &str, member: &serde_json::Value| -> bool {
                    let one = serde_json::Value::Object(
                        std::iter::once((key.to_string(), member.clone())).collect(),
                    );
                    serde_json::from_value::<DisplayPolicy>(one).is_ok()
                };
                // Selector is all-or-nothing (see this function's doc).
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
                    // Dropped everything → not configured. Test the result,
                    // not the key list (unknown members and `version` survive
                    // without teaching us an axis). `Some(default)` would
                    // flip Linux identity Shared → PerClient.
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

    /// Stored policy, or [`DisplayPolicy::default`] when unconfigured (mgmt GET).
    pub fn get(&self) -> DisplayPolicy {
        self.cur.lock().unwrap().clone().unwrap_or_default()
    }

    /// Console-configured policy, or `None` if no file. `None` ⇒ leave
    /// historical env/default behavior.
    pub fn configured(&self) -> Option<DisplayPolicy> {
        self.cur.lock().unwrap().clone()
    }

    pub fn configured_effective(&self) -> Option<EffectivePolicy> {
        self.configured().map(|p| p.effective())
    }

    /// Game-session routing, read off the stored policy (default `Auto`).
    /// A preset selection never resets it.
    pub fn game_session(&self) -> GameSession {
        self.get().game_session
    }

    pub fn ddc_power_off(&self) -> bool {
        self.get().ddc_power_off
    }

    pub fn pnp_disable_monitors(&self) -> bool {
        self.get().pnp_disable_monitors
    }

    pub fn edid_lock(&self) -> bool {
        self.get().edid_lock
    }

    /// Neutralise connected-but-inactive external sinks for the stream.
    /// Default on ([`standby_sink_neutralise`]). Operator displays are the
    /// opt-in [`Self::pnp_disable_monitors`] selector, not this one.
    pub fn standby_sink_neutralise(&self) -> bool {
        standby_sink_neutralise(std::env::var("PUNKTFUNK_STANDBY_SINK_KEEP").ok().as_deref())
            || self.get().pnp_disable_monitors
    }

    /// Persist + adopt. Memory changes only after the disk write; the
    /// whole transaction holds [`Self::write`].
    pub fn set(&self, policy: DisplayPolicy) -> Result<()> {
        let policy = policy.sanitized();
        let _tx = self.write.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(&policy)?;
        // Arm before the write: `write_secret_file` creates+truncates first,
        // so ENOSPC/EIO can leave the unique temp on disk. Nothing reaps
        // `*.tmp` — a second host process may be using its own in-flight name.
        let tmp = TmpFile::arm(unique_tmp_path(&self.path));
        pf_paths::write_secret_file(tmp.path(), &bytes)?;
        std::fs::rename(tmp.path(), &self.path)?;
        tmp.published();
        *self.cur.lock().unwrap() = Some(policy);
        Ok(())
    }
}

/// Temp path no other writer shares: `<name>.<pid>.<n>.tmp`. A fixed
/// `.json.tmp` interleaves two host processes into one rename.
fn unique_tmp_path(path: &std::path::Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Owns a [`unique_tmp_path`] until rename; deletes on every early exit.
/// Unique names have no self-heal (a fixed `.json.tmp` was overwritten).
/// Crash between write and rename needs a reaper the caller cannot own.
struct TmpFile(Option<PathBuf>);

impl TmpFile {
    fn arm(path: PathBuf) -> Self {
        TmpFile(Some(path))
    }
    fn path(&self) -> &std::path::Path {
        self.0.as_deref().expect("disarmed only by consuming self")
    }
    /// Rename succeeded: the path is the real file now; do not remove it.
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

/// Process-wide store, loaded once. Same global-accessor shape as
/// `pf_gpu::prefs`: display setup sits deep in capture/vdisplay with no
/// app state threaded through.
pub fn prefs() -> &'static DisplayPolicyStore {
    static STORE: OnceLock<DisplayPolicyStore> = OnceLock::new();
    STORE.get_or_init(|| {
        DisplayPolicyStore::load_from(pf_paths::config_dir().join("display-settings.json"))
    })
}

/// Operator-named bundle of the six axes plus game-session, stored in
/// `<config>/display-presets.json`. Applying one writes a `Custom`
/// [`DisplayPolicy`] via `PUT /display/settings`. Editing the catalog never
/// mutates the running policy; re-apply to adopt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CustomPreset {
    /// Host-assigned, stable for the life of the entry.
    pub id: String,
    pub name: String,
    pub fields: EffectivePolicy,
    /// Unlike a built-in preset, applying a custom preset sets this axis.
    #[serde(default)]
    pub game_session: GameSession,
}

/// Create/replace body. No `id` — the host owns it.
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

/// Same bounds as [`DisplayPolicy::sanitized`], so a later apply cannot
/// smuggle an out-of-range window past the direct PUT.
fn sanitize_preset_fields(mut fields: EffectivePolicy) -> EffectivePolicy {
    fields.max_displays = fields.max_displays.clamp(1, 16);
    fields.keep_alive = clamp_keep_alive(fields.keep_alive);
    fields.layout.positions = canonical_positions(std::mem::take(&mut fields.layout.positions));
    fields
}

/// Recovered entries plus whether anything was lost. CRUD checks `lossy`
/// before overwrite so a save that would drop entries preserves the original.
struct CatalogRead {
    presets: Vec<CustomPreset>,
    lossy: bool,
}

/// Lenient catalog-read shape: every axis defaulted.
///
/// Persist needs this (older entries, hand-dropped keys). The mgmt API
/// must stay strict: `#[serde(default)]` on [`EffectivePolicy`] turned
/// `{"name":"Kiosk","fields":{}}` into a 201. Private, `Deserialize`-only,
/// reached only from [`parse_catalog`]. Gain a field when
/// [`EffectivePolicy`] does; the `From` is exhaustive so a forgotten axis
/// is a compile error.
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

/// Lenient catalog-read shape. `id`/`name` stay required — without them
/// it is not a preset; the entry-wise skip keeps the rest.
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

/// Parse **entry-wise**. Pure so recovery is unit-tested.
///
/// Whole-document `from_slice::<Vec<CustomPreset>>` made one bad entry
/// empty the catalog; the next create then renamed that empty vector over
/// the file. A bad entry costs itself; `lossy` tells the caller the disk
/// still holds more than we understood.
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
        // Keep id/name for the log even when the body is unreadable —
        // "entry 3" is useless next to a console list of names.
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

/// `Ok` for absent (empty) and for readable-possibly-lossy. `Err` only when
/// the file exists and the OS refused it — writing back what we could read
/// (nothing) would erase an unreachable catalog.
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

/// Copy aside to `display-presets.json.bad` before a non-round-trip save.
/// Copy, not rename: a crash still leaves a catalog at the live path.
/// Best-effort — failing to preserve must not fail the operator write.
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

/// Serializes catalog read → mutate → save. Without it concurrent add +
/// delete each write back what they loaded and one edit vanishes.
static CATALOG_LOCK: Mutex<()> = Mutex::new(());

/// Persist the catalog (private dir, temp-write + atomic rename). Callers
/// hold [`CATALOG_LOCK`].
fn save_custom_presets(presets: &[CustomPreset]) -> Result<()> {
    let path = custom_presets_path();
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(presets)?;
    // Same guard as `DisplayPolicyStore::set`: a failed write leaves the
    // unique temp on disk.
    let tmp = TmpFile::arm(unique_tmp_path(&path));
    pf_paths::write_secret_file(tmp.path(), &bytes)?;
    std::fs::rename(tmp.path(), &path)?;
    tmp.published();
    Ok(())
}

/// Load saved custom presets. Absent/unreadable ⇒ empty, non-fatal; the
/// file is left untouched.
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

/// Neutralise a connected-but-inactive external sink while streaming,
/// unless `PUNKTFUNK_STANDBY_SINK_KEEP` is set (any value but `0`/`off`/empty).
///
/// Default-on: standby HPD/DDC on an unused TV stalls the virtual head.
/// Operator displays are out of scope — this selector only sees external
/// physicals in no topology (`monitor_devnode::disable_connected_inactive`).
pub fn standby_sink_neutralise(opt_out: Option<&str>) -> bool {
    !matches!(opt_out, Some(v) if !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("off"))
}

/// 12 hex chars from name + wall-clock nanos + `nonce` (host `library`
/// custom-entry scheme). Name+nanos is not unique: two creates in one tick
/// hash identically, then `update_custom_preset` edits whichever landed first.
fn preset_id(name: &str, nanos: u128, nonce: u64) -> String {
    hex::encode(&Sha256::digest(format!("{name}:{nanos}:{nonce}").as_bytes())[..6])
}

/// First id not already in `presets`, re-rolling nonce against one clock
/// read. 48 bits is collision-free only if something checks; re-reading
/// the clock per attempt is untestable luck.
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

/// Replace fields; id preserved. `None` ⇒ no such id.
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

/// Delete. `false` ⇒ no such id.
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

        // Catalog written before `game_session` still loads (defaults to `Auto`).
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
        let p = DisplayPolicy {
            preset: Preset::Custom,
            keep_alive: KeepAlive::Off,
            topology: Topology::Extend,
            ..DisplayPolicy::default()
        };
        assert_eq!(p.effective().keep_alive, KeepAlive::Off);
        assert_eq!(p.effective().topology, Topology::Extend);

        let p = DisplayPolicy {
            preset: Preset::GamingRig,
            keep_alive: KeepAlive::Off,
            topology: Topology::Extend,
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
                mode: LayoutMode::AutoRow, // workstation forces Manual regardless
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
        // Arranging must not clear orthogonal pins (game-session, capture, …).
        assert_eq!(p.game_session, GameSession::Dedicated);
        assert!(p.ddc_power_off);
        assert!(p.pnp_disable_monitors);
        assert!(p.edid_lock);
        assert_eq!(p.capture_monitor.as_deref(), Some("DP-2"));
        assert_eq!(p.preset, Preset::Custom);
        assert_eq!(p.keep_alive, eff.keep_alive);
        assert_eq!(p.topology, eff.topology);
        assert_eq!(p.mode_conflict, eff.mode_conflict);
        assert_eq!(p.identity, eff.identity);
        assert_eq!(p.max_displays, eff.max_displays);
        let e2 = p.effective();
        assert_eq!(e2.layout.mode, LayoutMode::Manual);
        let want = Position { x: 2560, y: 0 };
        assert_eq!(e2.layout.positions.get("7"), Some(&want));
    }

    /// File contract, not PUT. Older hosts' documents load with missing
    /// axes at defaults. PUT `/display/settings` is a whole-object replace,
    /// so omitting `capture_monitor` currently clears the pin. A merge DTO
    /// belongs in the mgmt handler; this type stays as asserted here.
    #[test]
    fn serde_defaults_fill_a_partial_document() {
        let p: DisplayPolicy =
            serde_json::from_str(r#"{ "preset": "custom", "max_displays": 2 }"#).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.keep_alive, KeepAlive::default());
        assert_eq!(p.topology, Topology::Auto);
        assert_eq!(p.version, 1);
        // Files from before the experimental axes default them off.
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
        assert!(store.configured().is_none());
        assert_eq!(store.get(), DisplayPolicy::default());

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

        let reopened = DisplayPolicyStore::load_from(path.clone());
        assert_eq!(reopened.configured().unwrap().preset, Preset::SharedDesktop);

        let _ = std::fs::remove_file(&path);
    }

    /// The mgmt PUT merge must enumerate every axis. Bump this count only
    /// in the same change that wires the new field.
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
        // Unclamped `u32` is a deadline the reaper never reaches.
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
        // Same bound on the catalog, which would otherwise smuggle past PUT.
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
        positions.insert("01".to_string(), Position { x: 10, y: 0 }); // zero-padded
        positions.insert("2".to_string(), Position { x: 20, y: 0 });
        positions.insert("slot3".to_string(), Position { x: 30, y: 0 }); // not a slot id
        positions.insert(" 4".to_string(), Position { x: 40, y: 0 }); // whitespace
        positions.insert("99".to_string(), Position { x: 50, y: 0 }); // no such slot
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
        // `"01"` must resolve: `arrange` looks up `u32::to_string()`.
        assert_eq!(got.get("1"), Some(&Position { x: 10, y: 0 }));
        assert_eq!(got.get("2"), Some(&Position { x: 20, y: 0 }));
    }

    #[test]
    fn canonical_positions_prefers_the_canonical_spelling_over_a_duplicate() {
        // Two spellings of slot 1: must not depend on BTreeMap order.
        let mut positions = BTreeMap::new();
        positions.insert("01".to_string(), Position { x: 10, y: 0 });
        positions.insert("1".to_string(), Position { x: 11, y: 0 });
        let out = canonical_positions(positions);
        assert_eq!(out.len(), 1);
        assert_eq!(out.get("1"), Some(&Position { x: 11, y: 0 }));
    }

    #[test]
    fn a_readable_file_is_sanitized_on_load_not_only_on_write() {
        // Load sanitizes like a console PUT, so a hand-edit cannot skip clamps.
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
        // Unknown enum variant: drop that axis, keep the rest (pin, preset, linger).
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
        // `max_displays` serde-defaults to 4, not 0; a typo must not admit zero displays.
        let doc = br#"{ "max_displays": "four", "preset": "workstation" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, default_max_displays());
        assert_eq!(p.preset, Preset::Workstation);
    }

    #[test]
    fn a_document_that_is_not_a_policy_at_all_is_unconfigured() {
        // Truncated/non-object → unconfigured. Half-reading would be worse.
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{ not json").is_none());
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"[1, 2, 3]").is_none());
        // Empty object is a policy: every axis defaults.
        assert_eq!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), b"{}"),
            Some(DisplayPolicy::default())
        );
    }

    #[test]
    fn a_future_schema_version_is_still_read() {
        // Unknown version still loads; rejecting it would revert on downgrade.
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
        // One bad entry must not empty the catalog (the next create would then overwrite it).
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
        // Omitted fields default (real `max_displays` default, not 0).
        let c = &read.presets[1];
        assert_eq!(c.fields.max_displays, default_max_displays());
        assert_eq!(c.fields.keep_alive, KeepAlive::default());
        assert_eq!(c.fields.topology, Topology::Auto);
        assert_eq!(c.game_session, GameSession::Auto);
        assert_eq!(read.presets[0].fields.max_displays, 2);
    }

    #[test]
    fn a_clean_catalog_is_not_lossy_and_a_broken_one_is() {
        let clean = parse_catalog(b"[]");
        assert!(clean.presets.is_empty() && !clean.lossy);
        // Not an array: nothing recovered, `lossy` so CRUD preserves the file.
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
        // Same name, same nanosecond: second create must take the next nonce.
        let first = free_preset_id_at(&[], "Same name", 42);
        assert_eq!(first, preset_id("Same name", 42, 0));
        let second = free_preset_id_at(&[entry(&first)], "Same name", 42);
        assert_eq!(second, preset_id("Same name", 42, 1));
        assert_ne!(first, second);
        assert_eq!(first.len(), 12, "12 hex chars, unchanged id shape");
    }

    /// Request contract: `#[serde(default)]` on [`EffectivePolicy`] would
    /// turn `{"fields":{}}` into a 201 and drop axes from OpenAPI `required`.
    /// Catalog leniency is [`StoredEffectivePolicy`].
    #[test]
    fn the_wire_shape_of_an_effective_policy_requires_every_axis() {
        assert!(serde_json::from_str::<EffectivePolicy>("{}").is_err());
        // Omitting one axis is a mistake, not a default.
        let almost = r#"{ "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                          "mode_conflict": "steal", "identity": "per-client",
                          "layout": { "mode": "auto-row", "positions": {} } }"#;
        assert!(serde_json::from_str::<EffectivePolicy>(almost).is_err());
        let mut full: serde_json::Value = serde_json::from_str(almost).unwrap();
        full["max_displays"] = serde_json::json!(2);
        assert!(serde_json::from_value::<EffectivePolicy>(full).is_ok());
        assert!(
            serde_json::from_str::<CustomPresetInput>(r#"{ "name": "Kiosk", "fields": {} }"#)
                .is_err()
        );
    }

    /// File contract is the opposite: an older catalog entry still loads.
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
        // `preset` selects the other axes; salvaging it to `Custom` would
        // activate leftover explicit fields nobody chose.
        let doc = br#"{ "version": 1, "preset": "kiosk",
                        "keep_alive": { "mode": "forever" }, "topology": "exclusive",
                        "mode_conflict": "steal", "identity": "per-client",
                        "layout": { "mode": "auto-row", "positions": {} }, "max_displays": 4 }"#;
        assert!(
            DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none(),
            "a preset name this build cannot read means we do not know what the file asks for"
        );
        // A known preset still salvages its neighbours.
        let doc = br#"{ "preset": "hotdesk", "topology": "hologram" }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.preset, Preset::Hotdesk);
        assert_eq!(p.topology, Topology::Auto);
    }

    #[test]
    fn a_document_we_understood_nothing_of_is_unconfigured_not_default() {
        // `None` is the historical-default gate. `Some(default)` is PerClient
        // identity; unconfigured Linux is Shared — that rename drops KDE config.
        let doc = br#"{ "topology": "hologram", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // `version` and unknown keys are not axes; they cannot mark configured.
        let doc = br#"{ "version": 1, "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        let doc = br#"{ "note": "mine", "identity": "perclient" }"#;
        assert!(DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).is_none());
        // One real axis is a configuration.
        let doc = br#"{ "identity": "perclient", "max_displays": 2 }"#;
        let p = DisplayPolicyStore::parse(std::path::Path::new("t.json"), doc).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.identity, Identity::default());
    }

    #[test]
    fn standby_sink_neutralise_is_on_unless_explicitly_kept() {
        // Unset, empty, and the two "off" spellings all mean: neutralise.
        assert!(standby_sink_neutralise(None));
        assert!(standby_sink_neutralise(Some("")));
        assert!(standby_sink_neutralise(Some("0")));
        assert!(standby_sink_neutralise(Some("off")));
        assert!(standby_sink_neutralise(Some("OFF")));
        // Anything else is the operator asking to keep the sink.
        assert!(!standby_sink_neutralise(Some("1")));
        assert!(!standby_sink_neutralise(Some("keep")));
    }

    #[test]
    fn a_temp_file_is_removed_unless_the_rename_published_it() {
        let dir = std::env::temp_dir().join(format!("pf-disp-tmp-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Failed write/rename must take the unique temp with it; nothing reaps `*.tmp`.
        let leaked = unique_tmp_path(&dir.join("display-settings.json"));
        std::fs::write(&leaked, b"partial").unwrap();
        drop(TmpFile::arm(leaked.clone()));
        assert!(!leaked.exists(), "a failed write must not leave {leaked:?}");
        // After publish the file is the real one; removing it would delete the write.
        let kept = unique_tmp_path(&dir.join("display-settings.json"));
        std::fs::write(&kept, b"published").unwrap();
        TmpFile::arm(kept.clone()).published();
        assert!(kept.exists());
        let _ = std::fs::remove_file(&kept);
        let _ = std::fs::remove_dir(&dir);
    }
}
