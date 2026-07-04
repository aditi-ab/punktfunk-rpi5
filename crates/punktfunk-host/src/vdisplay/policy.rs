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
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
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

    /// Persist + adopt a new policy (sanitized first). The in-memory value changes only if the disk
    /// write succeeds, so a full disk can't leave memory and file disagreeing.
    pub fn set(&self, policy: DisplayPolicy) -> Result<()> {
        let policy = policy.sanitized();
        if let Some(dir) = self.path.parent() {
            crate::gamestream::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        crate::gamestream::write_secret_file(&tmp, &serde_json::to_vec_pretty(&policy)?)?;
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
        DisplayPolicyStore::load_from(crate::gamestream::config_dir().join("display-settings.json"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn partial_json_fills_defaults() {
        // A hand-written file with only a couple of fields loads, the rest defaulting.
        let p: DisplayPolicy =
            serde_json::from_str(r#"{ "preset": "custom", "max_displays": 2 }"#).unwrap();
        assert_eq!(p.max_displays, 2);
        assert_eq!(p.keep_alive, KeepAlive::default());
        assert_eq!(p.topology, Topology::Auto);
        assert_eq!(p.version, 1);
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
