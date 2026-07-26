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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvMarker {
    /// The variable name (e.g. `HEROIC_GAME_ID`).
    pub key: String,
    /// The exact value to require, when the launcher's value identifies *this* title. `None` matches
    /// the key's mere presence — only safe for launchers that run one game at a time.
    pub value: Option<String>,
}

/// The signals that identify a launched title's process(es). Every field is optional and
/// independent; an all-`None` spec means "this title can't be tracked" (the lease degrades to
/// [`crate::gamelease::LeaseKind::Untracked`] and both lifetime behaviors stay inert for it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetectSpec {
    /// Steam appid, for titles Steam itself installed (never for non-Steam shortcuts, whose reaper
    /// appid semantics differ — those carry an [`exe`](Self::exe) instead). On Linux this is the
    /// sharpest signal available: Steam wraps every launch — native or Proton — in
    /// `reaper SteamLaunch AppId=<appid>`, whose lifetime is exactly the game's.
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
}

impl DetectHint {
    /// Whether the hint says anything at all (all-empty is treated as absent).
    pub fn is_empty(&self) -> bool {
        self.trimmed().is_none()
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
        let Some((install_dir, exe, process_name)) = h.trimmed() else {
            return Self::default();
        };
        Self {
            install_dir: install_dir.map(PathBuf::from),
            exe: exe.map(PathBuf::from),
            process_name: process_name.map(str::to_string),
            ..Default::default()
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
        };
        assert!(blank.is_empty());
        assert!(DetectSpec::from(&blank).is_empty(), "nothing to match on");

        let hint = DetectHint {
            install_dir: Some("  /games/quail  ".into()),
            exe: None,
            process_name: Some("quail".into()),
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
