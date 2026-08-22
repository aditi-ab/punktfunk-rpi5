//! How to **recognize** a launched title's running process(es) — the read-side counterpart to
//! [`super::launch`], which only knows how to *start* one.
//!
//! A launch tells the host what to run; it does not tell it what "the game" looks like once the
//! launcher has handed off. Every store here therefore contributes whatever identifying signals it
//! already has on disk (design §4): a Steam appid, an install directory, a concrete executable, an
//! environment marker. [`crate::procscan`] turns those into live pids, and
//! [`crate::gamelease`] turns pids into a lifetime.
//!
//! The signals are a **union**, not a ladder: any process matching any signal belongs to the game.
//! That is what lets one recipe (`install_dir`) cover the stores that expose nothing else, while a
//! store with something sharper (Steam's launch reaper) still gets the precise answer.
//!
//! `DetectSpec` is **host-internal and never crosses the wire** — it names local filesystem paths,
//! which no client has any business seeing. It rides on [`super::GameEntry`] as a `#[serde(skip)]`
//! field purely so the providers that already read these paths during their scan don't have to be
//! walked a second time.

use super::*;

/// An environment variable a launcher stamps onto the game's process, identifying it.
///
/// Serializable because it is now half of the inbound [`DetectHint`] too (D3) — a library plugin
/// that knows its launcher's marker (Heroic's `HEROIC_APP_NAME`, load-bearing under Proton) has to
/// be able to say so, since after extraction the host no longer reads that launcher's files itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnvMarker {
    /// The variable name (e.g. `HEROIC_GAME_ID`).
    #[schema(example = "HEROIC_APP_NAME")]
    pub key: String,
    /// The exact value to require, when the launcher's value identifies *this* title. `None` matches
    /// the key's mere presence — only safe for launchers that run one game at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// The env-var name charset a hint may carry: `[A-Za-z0-9_]{1,64}`, POSIX-shaped. An out-of-charset
/// key is not a real environment variable, so accepting one could only ever produce a matcher rule
/// that never fires (or, with an absurd length, a needless per-process comparison cost).
pub(crate) fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Longest env-var VALUE a hint may pin. Values are compared against every candidate process's
/// environment, so an unbounded one is a (small) DoS lever and never a legitimate game id.
pub(crate) const MAX_ENV_VALUE: usize = 256;

/// The signals that identify a launched title's process(es). Every field is optional and
/// independent; an all-`None` spec means "this title can't be tracked" (the lease degrades to
/// [`crate::gamelease::LeaseKind::Untracked`] and both lifetime behaviors stay inert for it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetectSpec {
    /// Steam appid, for titles Steam itself installed (never for non-Steam shortcuts, whose reaper
    /// appid semantics differ — those carry an [`exe`](Self::exe) instead). On Linux this is the
    /// sharpest signal available: Steam wraps every launch — native or Proton — in
    /// `reaper SteamLaunch AppId=<appid>`.
    ///
    /// ⚠ That reaper is the *appid's*, not the game's. Steam wraps its **pre-launch** work for a
    /// title in one too — shader pre-caching most visibly — so a launch is a chain of reaper trees
    /// and only the last of them is the game. Reading the first as the game is what dropped a
    /// stream 10 s into a Rocket League launch, mid-shader-compile (field report 2026-08-22); the
    /// shader job is excluded by name in [`crate::procscan`], and [`crate::gamelease`] waits out a
    /// window before believing any of them.
    pub steam_appid: Option<u32>,
    /// A launcher-stamped environment marker.
    pub env_marker: Option<EnvMarker>,
    /// The game's own executable, when a store resolves one exactly.
    pub exe: Option<PathBuf>,
    /// The game's install directory — the universal recipe. A process whose image path (or, for
    /// Proton/Wine, whose command line) sits under this directory is part of the game.
    pub install_dir: Option<PathBuf>,
    /// The game's executable **file name** (`Hades.exe`, `retroarch`), matched case-insensitively
    /// against a process's image name and nothing else.
    ///
    /// The weakest signal here, and the only one an operator supplies by hand
    /// ([`super::DetectHint`]): a bare name says nothing about *which* copy is running. It is offered
    /// because the entries that need it — an emulator launched through a front-end, a game whose
    /// launcher relocates it — often expose nothing sharper, and because "started after this launch"
    /// still bounds it: a copy the player already had open is never adopted.
    pub process_name: Option<String>,
}

impl DetectSpec {
    /// A spec with no signals at all — nothing to track.
    pub fn is_empty(&self) -> bool {
        self.steam_appid.is_none()
            && self.env_marker.is_none()
            && self.exe.is_none()
            && self.install_dir.is_none()
            && self.process_name.is_none()
    }

    /// Just a Steam appid (the manifest path; art/shortcut scanning fills the rest).
    pub fn steam(appid: u32) -> Self {
        Self {
            steam_appid: Some(appid),
            ..Default::default()
        }
    }

    /// Just an install directory — the universal recipe most stores land on.
    pub fn dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            install_dir: Some(dir.into()),
            ..Default::default()
        }
    }

    /// Just a concrete executable.
    pub fn exe(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: Some(exe.into()),
            ..Default::default()
        }
    }

    /// Add an install directory to an existing spec (Steam pairs one with its appid so the Windows
    /// matcher, which has no reaper, still has something to go on).
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.install_dir = Some(dir.into());
        self
    }

    /// Add an environment marker to an existing spec.
    pub fn with_env(mut self, key: impl Into<String>, value: Option<String>) -> Self {
        self.env_marker = Some(EnvMarker {
            key: key.into(),
            value,
        });
        self
    }

    /// Fill in whatever this spec doesn't already know from an operator/provider hint.
    ///
    /// The host's own findings win: a hint is a fallback for a title the scanners couldn't pin down,
    /// never a way to redirect the matcher away from what the store actually reported.
    pub fn or_hint(mut self, hint: &DetectHint) -> Self {
        let from = Self::from(hint);
        self.install_dir = self.install_dir.or(from.install_dir);
        self.exe = self.exe.or(from.exe);
        self.process_name = self.process_name.or(from.process_name);
        // D3: the two store-derived signals are fillable from a hint now that the store may live in
        // a plugin. Same rule as the other three — the host's own finding wins where it has one,
        // which for a provider entry is moot (the host scanned nothing for it).
        self.steam_appid = self.steam_appid.or(from.steam_appid);
        self.env_marker = self.env_marker.or(from.env_marker);
        self
    }
}

/// What an operator (or a provider plugin) can tell the host about recognizing a title — the wire
/// half of [`DetectSpec`], and the only part of it that is ever accepted from outside.
///
/// Deliberately a **subset**: the store-derived signals (a Steam appid, a launcher's environment
/// marker) are things the host discovers for itself and would be meaningless — or dangerous — to take
/// on someone's word. What is left is what a provider genuinely knows and the host cannot guess: where
/// the title is installed, which executable is the game, what the process is called. All three are
/// optional; supplying none is the same as supplying no hint at all.
///
/// Never returned by the catalog API — see the module docs on why detect data does not cross the wire
/// outbound.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DetectHint {
    /// Where the title is installed. Any process running from under this directory is part of the
    /// game — the universal recipe, and the one worth supplying if you supply only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
    /// The game's own executable, as an absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// The executable's file name (`Hades.exe`), when its location isn't fixed. Weakest of the three
    /// — see [`DetectSpec::process_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    /// The Steam appid, for a title Steam itself installed (D3). On Linux this is the **sharpest**
    /// signal that exists — Steam wraps every launch, native or Proton, in
    /// `reaper SteamLaunch AppId=<appid>`, whose lifetime is exactly the game's — so without it a
    /// steam plugin's lease tracking would degrade from reaper-exact to install-dir prefix matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_appid: Option<u32>,
    /// A launcher-stamped environment marker (D3) — see [`EnvMarker`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_marker: Option<EnvMarker>,
}

impl DetectHint {
    /// Whether the hint says anything at all (all-empty is treated as absent).
    pub fn is_empty(&self) -> bool {
        self.trimmed().is_none() && self.steam_appid.is_none() && self.env_marker().is_none()
    }

    /// The env marker, if it is well-formed. A malformed one is dropped rather than rejected, for
    /// the same reason a blank `install_dir` is: hint fields are hand-writable plugin input, and the
    /// matcher must never be handed a rule it can't honour.
    fn env_marker(&self) -> Option<&EnvMarker> {
        self.env_marker
            .as_ref()
            .filter(|m| valid_env_key(&m.key))
            .filter(|m| m.value.as_ref().is_none_or(|v| v.len() <= MAX_ENV_VALUE))
    }

    /// The hint with blank fields dropped, or `None` if nothing is left. Console text inputs and
    /// hand-written plugin payloads both produce `""` for "not set", and an empty install dir would
    /// otherwise match *every* process on the box.
    fn trimmed(&self) -> Option<(Option<&str>, Option<&str>, Option<&str>)> {
        fn f(s: &Option<String>) -> Option<&str> {
            s.as_deref().map(str::trim).filter(|v| !v.is_empty())
        }
        let (dir, exe, name) = (f(&self.install_dir), f(&self.exe), f(&self.process_name));
        (dir.is_some() || exe.is_some() || name.is_some()).then_some((dir, exe, name))
    }
}

/// A provider's hint becomes a spec — the one inbound path into [`DetectSpec`].
impl From<&DetectHint> for DetectSpec {
    fn from(h: &DetectHint) -> Self {
        let (install_dir, exe, process_name) = h.trimmed().unwrap_or((None, None, None));
        Self {
            install_dir: install_dir.map(PathBuf::from),
            exe: exe.map(PathBuf::from),
            process_name: process_name.map(str::to_string),
            steam_appid: h.steam_appid,
            env_marker: h.env_marker().cloned(),
        }
    }
}

/// Derive a detect spec from an operator-typed shell command (the custom store's `command` kind, and
/// the provider-plugin entries that reuse it): if the command's first token is an absolute path to an
/// existing file, that's the game's executable.
///
/// Deliberately conservative — a bare `dolphin-emu --batch` yields nothing (a PATH lookup would guess
/// at which of several installs the launcher will pick, and a wrong exe is worse than none: the lease
/// would call a running game exited). Custom entries are spawned by the host anyway, so their primary
/// tracking is the child process itself; this is only the fallback for a command that shims out.
pub fn spec_from_command(cmd: &str) -> DetectSpec {
    let Some(first) = shell_first_token(cmd) else {
        return DetectSpec::default();
    };
    let p = Path::new(&first);
    if p.is_absolute() && p.is_file() {
        DetectSpec::exe(p)
    } else {
        DetectSpec::default()
    }
}

/// The first token of a shell command, honoring single/double quotes around it (`"/opt/My Game/run"`).
/// Not a shell parser — just enough to recover a quoted absolute path, which is the only case that
/// matters here.
fn shell_first_token(cmd: &str) -> Option<String> {
    let cmd = cmd.trim_start();
    let mut chars = cmd.chars();
    match chars.next()? {
        q @ ('"' | '\'') => {
            let rest: String = chars.collect();
            let end = rest.find(q)?;
            Some(rest[..end].to_string())
        }
        _ => cmd.split_whitespace().next().map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_untrackable() {
        assert!(DetectSpec::default().is_empty());
        assert!(!DetectSpec::steam(570).is_empty());
        assert!(!DetectSpec::dir("/games/x").is_empty());
        assert!(!DetectSpec::exe("/games/x/run").is_empty());
        assert!(!DetectSpec::default()
            .with_env("HEROIC_GAME_ID", Some("abc".into()))
            .is_empty());
    }

    #[test]
    fn builders_compose() {
        let s = DetectSpec::steam(570).with_dir("/games/dota");
        assert_eq!(s.steam_appid, Some(570));
        assert_eq!(s.install_dir.as_deref(), Some(Path::new("/games/dota")));
        let h = DetectSpec::dir("/games/quail").with_env("HEROIC_GAME_ID", Some("Quail".into()));
        assert_eq!(
            h.env_marker,
            Some(EnvMarker {
                key: "HEROIC_GAME_ID".into(),
                value: Some("Quail".into())
            })
        );
    }

    #[test]
    fn command_spec_only_trusts_an_absolute_existing_file() {
        // A PATH-relative command is not guessed at.
        assert!(spec_from_command("dolphin-emu --batch").is_empty());
        // An absolute path that doesn't exist is not asserted either.
        assert!(spec_from_command("/nope/not/here --x").is_empty());
        // A real absolute file (this test binary) is picked up, quoted or bare.
        let me = std::env::current_exe().expect("current exe");
        let bare = format!("{} --flag", me.display());
        assert_eq!(spec_from_command(&bare).exe.as_deref(), Some(me.as_path()));
        let quoted = format!("\"{}\" --flag", me.display());
        assert_eq!(
            spec_from_command(&quoted).exe.as_deref(),
            Some(me.as_path())
        );
        assert!(spec_from_command("   ").is_empty());
    }

    /// A hint is operator/plugin input, so the blank-field case is the norm, not an edge: a console
    /// form and a hand-written plugin payload both send `""` for "not set". An empty install dir that
    /// reached the matcher would prefix-match every process on the box — and this feature can end
    /// processes.
    #[test]
    fn a_blank_hint_says_nothing() {
        assert!(DetectHint::default().is_empty());
        let blank = DetectHint {
            install_dir: Some("".into()),
            exe: Some("   ".into()),
            process_name: Some("\t".into()),
            ..Default::default()
        };
        assert!(blank.is_empty());
        assert!(DetectSpec::from(&blank).is_empty(), "nothing to match on");

        let hint = DetectHint {
            install_dir: Some("  /games/quail  ".into()),
            exe: None,
            process_name: Some("quail".into()),
            ..Default::default()
        };
        assert!(!hint.is_empty());
        let spec = DetectSpec::from(&hint);
        assert_eq!(spec.install_dir.as_deref(), Some(Path::new("/games/quail")));
        assert_eq!(spec.process_name.as_deref(), Some("quail"));
        assert_eq!(spec.exe, None);
    }

    /// The host's own findings outrank a hint. A provider that guessed wrong (or a stale export)
    /// must not be able to point the matcher — and therefore the termination ladder — at something
    /// other than what the store itself reported.
    #[test]
    fn a_hint_only_fills_gaps() {
        let found = DetectSpec::dir("/games/real");
        let hint = DetectHint {
            install_dir: Some("/games/wrong".into()),
            exe: Some("/games/real/run".into()),
            process_name: None,
            ..Default::default()
        };
        let merged = found.or_hint(&hint);
        assert_eq!(
            merged.install_dir.as_deref(),
            Some(Path::new("/games/real")),
            "the store's own answer stands"
        );
        assert_eq!(
            merged.exe.as_deref(),
            Some(Path::new("/games/real/run")),
            "but a field the store had nothing for is filled in"
        );
        // Nothing found + nothing hinted stays untrackable.
        assert!(DetectSpec::default()
            .or_hint(&DetectHint::default())
            .is_empty());
    }

    /// D3: the two store-derived signals now ride the hint, because after extraction the host no
    /// longer reads Steam's or Heroic's files itself. Without them a plugin's lease tracking would
    /// silently degrade — reaper-exact to dir-prefix on Linux Steam, and gone entirely for Heroic
    /// under Proton, where the env marker is the only thing that works.
    #[test]
    fn a_hint_can_carry_the_store_derived_signals() {
        let hint = DetectHint {
            steam_appid: Some(440),
            env_marker: Some(EnvMarker {
                key: "HEROIC_APP_NAME".into(),
                value: Some("Quail".into()),
            }),
            ..Default::default()
        };
        assert!(!hint.is_empty(), "either field alone is a real hint");
        let spec = DetectSpec::from(&hint);
        assert_eq!(spec.steam_appid, Some(440));
        assert_eq!(spec.env_marker.as_ref().unwrap().key, "HEROIC_APP_NAME");

        // A steam_appid on its own is enough to be trackable.
        let only_appid = DetectHint {
            steam_appid: Some(620),
            ..Default::default()
        };
        assert!(!only_appid.is_empty());
        assert!(!DetectSpec::from(&only_appid).is_empty());

        // The host's own finding still wins where it has one (unchanged rule).
        let found = DetectSpec::steam(70);
        assert_eq!(found.or_hint(&hint).steam_appid, Some(70));
        // …but a field the host had nothing for is filled in.
        assert_eq!(
            DetectSpec::dir("/games/x")
                .or_hint(&hint)
                .env_marker
                .unwrap()
                .key,
            "HEROIC_APP_NAME"
        );
    }

    /// A malformed marker is DROPPED, not honoured — same posture as a blank `install_dir`. The
    /// matcher must never be handed a rule it cannot evaluate, and these values reach a code path
    /// that can end processes.
    #[test]
    fn a_malformed_env_marker_says_nothing() {
        let bad = |key: &str, value: Option<String>| DetectHint {
            env_marker: Some(EnvMarker {
                key: key.into(),
                value,
            }),
            ..Default::default()
        };
        assert!(bad("", None).is_empty());
        assert!(bad("HAS-DASH", None).is_empty(), "not a POSIX env name");
        assert!(bad("HAS SPACE", None).is_empty());
        assert!(bad(&"K".repeat(65), None).is_empty(), "over the key cap");
        assert!(
            bad("K", Some("v".repeat(MAX_ENV_VALUE + 1))).is_empty(),
            "over the value cap"
        );
        // …and a well-formed one at exactly the caps is kept.
        assert!(!bad(&"K".repeat(64), Some("v".repeat(MAX_ENV_VALUE))).is_empty());
        assert!(DetectSpec::from(&bad("HAS-DASH", None))
            .env_marker
            .is_none());
    }

    #[test]
    fn first_token_handles_quotes_and_spaces() {
        assert_eq!(
            shell_first_token("\"/opt/My Game/run\" -w").as_deref(),
            Some("/opt/My Game/run")
        );
        assert_eq!(
            shell_first_token("'/opt/My Game/run'").as_deref(),
            Some("/opt/My Game/run")
        );
        assert_eq!(shell_first_token("  plain --x").as_deref(), Some("plain"));
        assert_eq!(shell_first_token("").as_deref(), None);
        // An unterminated quote yields nothing rather than a bogus token.
        assert_eq!(shell_first_token("\"/opt/oops").as_deref(), None);
    }
}
