//! Virtual-display **management policy** — the user-configurable behavior surface for how virtual
//! displays are created, kept alive, and arranged (design: `design/display-management.md`).
//!
//! This is the pure config layer that sits **above** the per-compositor [`VirtualDisplay`](super)
//! backends: a small set of orthogonal options ([`DisplayPolicy`]) with safe defaults and named
//! [`Preset`]s, persisted to `<config>/display-settings.json` and editable from the web console.
//! The lifecycle/registry that *acts* on this policy lands in later stages; **Stage 0** (this file
//! plus the mgmt endpoints) stands up the surface and wires the two behaviors the existing code can
//! already express — the Windows monitor linger duration and the Linux "make the streamed output
//! the sole desktop" topology — through it.
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
        /// Linger window in seconds.
        seconds: u32,
    },
    /// Keep the display until host shutdown or an explicit release (the `Pinned` lifecycle state).
    /// **Not honored until the display-lifecycle stage** — rejected by the mgmt PUT at Stage 0.
    Forever,
}

impl Default for KeepAlive {
    fn default() -> Self {
        // The historical Windows behavior, made explicit; the Linux backends had no linger and map
        // `Off`/short-duration onto their (nonexistent) keep-alive as a no-op until the lifecycle stage.
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
/// a different mode. Stored at Stage 0; enforced from the mode-conflict admission stage.
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

/// Stable display identity, so desktop environments persist per-display config (KDE scaling). Stored
/// at Stage 0; carriers wired from the identity stage.
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

/// How group members are arranged in the desktop coordinate space. Stored at Stage 0; applied from
/// the multi-monitor stage.
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
    /// Schema version (currently 1) — lets a future field addition migrate rather than reject.
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
    /// EXPERIMENTAL (Windows): DISABLE physical monitors' PnP device nodes for the stream's
    /// duration (persistently, so a standby monitor/TV whose hot-plug events re-arrive stays
    /// disabled) and re-enable them at teardown. Two selectors: the monitors an `Exclusive`
    /// isolate deactivated, plus — in ANY topology — external monitors that are connected but not
    /// part of the desktop (the standby TV that was never active, whose input auto-scan /
    /// instant-on HPD cycling re-probes the link every few seconds). Targets the same
    /// "connected-but-dark head" periodic-stutter class as [`Self::ddc_power_off`], but at the
    /// Windows-reaction level: a disabled devnode's wake events trigger no PnP arrival, no CCD
    /// re-evaluation, no DWM invalidation. A crash-recovery journal re-enables leftovers on host
    /// startup. Orthogonal to `preset` (like `game_session`); `#[serde(default)]` = off.
    #[serde(default)]
    pub pnp_disable_monitors: bool,
}

fn one() -> u32 {
    1
}
fn default_max_displays() -> u32 {
    4
}

impl Default for DisplayPolicy {
    fn default() -> Self {
        // Bit-for-bit today's behavior (the `default` preset expanded), so an unconfigured host reads
        // the same policy the Stage-0 call sites already produce.
        DisplayPolicy {
            version: 1,
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
        }
    }
}

/// The six resolved fields after preset expansion — what the lifecycle/registry and the Stage-0 call
/// sites read, and what the mgmt API echoes as the "currently in force" policy. Pure output of
/// [`DisplayPolicy::effective`].
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

    /// Clamp fields to their valid ranges (called on write). `max_displays` to `1..=16` (the
    /// pf-vdisplay connector ceiling / a sane Linux bound).
    pub fn sanitized(mut self) -> Self {
        self.version = 1;
        self.max_displays = self.max_displays.clamp(1, 16);
        self
    }
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
    ) -> DisplayPolicy {
        DisplayPolicy {
            version: 1,
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
            // Preserve the orthogonal axes (EffectivePolicy doesn't carry them).
            game_session,
            ddc_power_off,
            pnp_disable_monitors,
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
/// JSON path. Mirrors [`crate::gpu::GpuPrefStore`] — private dir, temp-write + atomic rename,
/// in-memory rollback if the disk write fails.
pub struct DisplayPolicyStore {
    path: PathBuf,
    /// `Some` only when a valid `display-settings.json` was loaded / written — the "console has
    /// configured this host" signal that gates whether Stage-0 call sites override their historical
    /// env/default behavior.
    cur: Mutex<Option<DisplayPolicy>>,
}

impl DisplayPolicyStore {
    /// Load from `path`. A missing file ⇒ unconfigured (`None`); a corrupt file ⇒ unconfigured with a
    /// warning (never fail host startup over a settings file).
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<DisplayPolicy>(&bytes) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(path = %path.display(),
                        "display-settings.json unreadable — using built-in defaults: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        DisplayPolicyStore {
            path,
            cur: Mutex::new(cur),
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

    /// Persist + adopt a new policy (sanitized first). The in-memory value changes only if the disk
    /// write succeeds, so a full disk can't leave memory and file disagreeing.
    pub fn set(&self, policy: DisplayPolicy) -> Result<()> {
        let policy = policy.sanitized();
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&policy)?)?;
        std::fs::rename(&tmp, &self.path)?;
        *self.cur.lock().unwrap() = Some(policy);
        Ok(())
    }
}

/// The process-wide display-policy store (config-dir file), loaded once on first access — the same
/// global-accessor shape as [`crate::gpu::prefs`], because display setup happens deep in the
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
/// enforces, so a preset can never carry an out-of-range `max_displays` that a later apply would reject.
fn sanitize_preset_fields(mut fields: EffectivePolicy) -> EffectivePolicy {
    fields.max_displays = fields.max_displays.clamp(1, 16);
    fields
}

/// Load the saved custom presets (empty + non-fatal if the file is absent or malformed — a bad
/// catalog never breaks the console's settings GET).
pub fn load_custom_presets() -> Vec<CustomPreset> {
    match std::fs::read(custom_presets_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "display-presets.json malformed — ignoring custom presets");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// Persist the catalog (private dir, temp-write + atomic rename — the [`DisplayPolicyStore::set`]
/// discipline, so a crash mid-write never truncates it).
fn save_custom_presets(presets: &[CustomPreset]) -> Result<()> {
    let path = custom_presets_path();
    if let Some(dir) = path.parent() {
        pf_paths::create_private_dir(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(presets)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// 12 hex chars from the name + wall-clock nanos — collision-free in practice, no uuid dep (the
/// [`crate::library`] custom-entry id scheme).
fn new_preset_id(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(&Sha256::digest(format!("{name}:{nanos}").as_bytes())[..6])
}

/// Create a custom preset, returning it with its assigned id.
pub fn add_custom_preset(input: CustomPresetInput) -> Result<CustomPreset> {
    let mut presets = load_custom_presets();
    let preset = CustomPreset {
        id: new_preset_id(&input.name),
        name: input.name,
        fields: sanitize_preset_fields(input.fields),
        game_session: input.game_session,
    };
    presets.push(preset.clone());
    save_custom_presets(&presets)?;
    Ok(preset)
}

/// Replace a custom preset's fields (id preserved). `None` ⇒ no preset with that id.
pub fn update_custom_preset(id: &str, input: CustomPresetInput) -> Result<Option<CustomPreset>> {
    let mut presets = load_custom_presets();
    let Some(slot) = presets.iter_mut().find(|p| p.id == id) else {
        return Ok(None);
    };
    slot.name = input.name;
    slot.fields = sanitize_preset_fields(input.fields);
    slot.game_session = input.game_session;
    let updated = slot.clone();
    save_custom_presets(&presets)?;
    Ok(Some(updated))
}

/// Delete a custom preset. `false` ⇒ no preset with that id.
pub fn delete_custom_preset(id: &str) -> Result<bool> {
    let mut presets = load_custom_presets();
    let before = presets.len();
    presets.retain(|p| p.id != id);
    if presets.len() == before {
        return Ok(false);
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
        let p = eff.with_manual_layout(positions, GameSession::Dedicated, true, true);
        // The orthogonal axes (game-session, DDC power-off, PnP disable) are preserved through
        // the transform.
        assert_eq!(p.game_session, GameSession::Dedicated);
        assert!(p.ddc_power_off);
        assert!(p.pnp_disable_monitors);
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

    #[test]
    fn partial_json_fills_defaults() {
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
}
