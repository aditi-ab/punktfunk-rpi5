//! What an installed plugin declares about itself: the paths it reads, whether it needs the
//! network, and the commands the host may run on its behalf.
//!
//! The manifest is a `punktfunk` block in the package's own `package.json`, so it is part of the
//! reviewed, hash-pinned tarball rather than something a running plugin can choose. It is the only
//! source of a plugin-owned command: the host builds every argv from a template here, with the
//! parameters validated per launch, instead of running a string the plugin answers with.
//!
//! Pin: `manifest_tests` below, and `library::launch::exec` for the launch half.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One argv template the host may run for this plugin.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ExecTemplate {
    /// A bare program name resolved from `PATH`, or an absolute path under a declared root.
    pub exe: String,
    /// argv after the program. `{param}` is replaced by a validated value, never by a shell.
    #[serde(default)]
    pub args: Vec<String>,
    /// Parameter name → what a value may look like. Anything not listed cannot be passed.
    #[serde(default)]
    pub params: BTreeMap<String, ParamKind>,
    /// Working directory, if the program needs one. Absolute, under a declared root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The shapes a template parameter may take. Deliberately a closed set of character classes
/// rather than plugin-supplied patterns: a regex from a package is a validator nobody reviewed.
///
/// No value may begin with `-`: it becomes one argv element, and a program that reads it as a
/// flag is the one way a validated value still changes what runs.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    /// Printable text — a game or bottle name, which legitimately carries punctuation. Safe
    /// because it is quoted into exactly one argv element, never parsed as syntax.
    Name,
    /// `[A-Za-z0-9._-]`, for ids and core names.
    Id,
    /// ASCII digits.
    Digits,
    /// An absolute path, checked against the plugin's declared roots at launch.
    Path,
    /// Extra argv elements — the operator's own flags for this program, each its own element.
    /// The one kind that may start with `-`, because flags are what it is for, and the one that
    /// may be given more than once. The program itself still comes from the template.
    Args,
}

/// A plugin's `punktfunk` block.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginManifest {
    /// Must be 1. A later schema is refused rather than guessed at.
    #[serde(default)]
    pub schema: u32,
    /// The id the plugin registers under (`definePlugin({ name })`), which is also its provider
    /// id in the library. How a launch finds the templates it may use.
    #[serde(default)]
    pub id: String,
    /// Paths the plugin reads. Absolute, or `~`-rooted.
    #[serde(default)]
    pub reads: Vec<String>,
    /// Paths it also writes.
    #[serde(default)]
    pub writes: Vec<String>,
    /// Whether it needs to reach the network at all.
    #[serde(default)]
    pub network: bool,
    /// Templates the host may run, by name.
    #[serde(default)]
    pub exec: BTreeMap<String, ExecTemplate>,
}

impl PluginManifest {
    /// What the manifest itself declares: expanded `reads` + `writes`, without operator grants.
    pub fn declared_roots(&self) -> Vec<PathBuf> {
        self.reads
            .iter()
            .chain(self.writes.iter())
            .filter_map(|p| expand_home(p))
            .collect()
    }

    /// Every root this plugin may reach: what it declared, plus what the operator granted it.
    ///
    /// A package cannot know where someone keeps their ROMs or installs their games, so the
    /// grants are how those paths become usable without the package asking for the whole disk.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.declared_roots()
            .into_iter()
            .chain(granted_roots(&self.id))
            .collect()
    }

    /// Is `candidate` inside one of the declared roots? Lexical on a normalized path: a `..`
    /// segment is refused outright rather than resolved, so this needs no filesystem.
    pub fn confines(&self, candidate: &Path) -> bool {
        if !candidate.is_absolute()
            || candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return false;
        }
        self.roots().iter().any(|root| candidate.starts_with(root))
    }
}

fn expand_home(p: &str) -> Option<PathBuf> {
    let rest = match p.strip_prefix("~/") {
        Some(rest) => rest,
        None => return Some(PathBuf::from(p)),
    };
    Some(home_dir()?.join(rest))
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Does `value` match `kind`? Length is capped here too: every value ends up as one argv element.
pub fn param_ok(kind: ParamKind, value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_control) {
        return false;
    }
    if kind == ParamKind::Args {
        return value.len() <= 256;
    }
    if value.starts_with('-') {
        return false;
    }
    match kind {
        ParamKind::Name => value.len() <= 128,
        ParamKind::Id => {
            value.len() <= 128
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }
        ParamKind::Digits => value.len() <= 32 && value.bytes().all(|b| b.is_ascii_digit()),
        ParamKind::Path => value.len() <= 4096 && Path::new(value).is_absolute(),
        // Handled above, before the leading-dash refusal.
        ParamKind::Args => true,
    }
}

/// [`granted_roots`] against a chosen store dir, so a test never opens the real config.
/// `~/` spells in a v1 grant expand through [`expand_home`], the same rule declared roots
/// follow.
pub(crate) fn granted_roots_in(id: &str, config_dir: PathBuf) -> Vec<PathBuf> {
    crate::plugins::access::AccessStore::open(config_dir)
        .grants_for(id)
        .into_iter()
        .filter_map(|g| expand_home(&g.path))
        .collect()
}

/// Extra roots the operator granted a plugin, by id: `<config>/plugin-grants.json` via
/// [`crate::plugins::access::AccessStore`].
///
/// Written by `punktfunk-host plugins grant` or an operator's `allow` decision, never by a
/// plugin — the file is the operator's answer to "this package may also reach here", so
/// nothing in the plugin lane may edit it.
pub fn granted_roots(id: &str) -> Vec<PathBuf> {
    granted_roots_in(id, pf_paths::config_dir())
}

/// The plugin install root: `<config>/plugins/node_modules`.
fn install_root() -> PathBuf {
    pf_paths::config_dir().join("plugins").join("node_modules")
}

/// Every installed plugin's manifest, keyed by the id it declares.
///
/// Read fresh: installs and updates land between launches, and the whole scan is a handful of
/// small files. A package with no `punktfunk` block, or one on a schema this host does not know,
/// contributes nothing — it simply has no templates the host will run.
pub fn installed() -> BTreeMap<String, PluginManifest> {
    let mut out = BTreeMap::new();
    for dir in package_dirs(&install_root()) {
        let Some(manifest) = read_package(&dir) else {
            continue;
        };
        if manifest.schema != 1 || manifest.id.is_empty() {
            tracing::warn!(
                package = %dir.display(),
                schema = manifest.schema,
                "plugin manifest: unusable (schema must be 1 and id must be set) — no host-run commands for it"
            );
            continue;
        }
        out.insert(manifest.id.clone(), manifest);
    }
    out
}

/// The manifest declaring `id`, if one is installed.
pub fn for_provider(id: &str) -> Option<PluginManifest> {
    installed().remove(id)
}

/// `node_modules/<name>` plus `node_modules/@scope/<name>`.
fn package_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let scoped = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('@'));
        if scoped {
            out.extend(
                std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter_map(|e| {
                        let p = e.path();
                        p.is_dir().then_some(p)
                    }),
            );
        } else {
            out.push(path);
        }
    }
    out
}

#[derive(Deserialize)]
struct PackageJson {
    #[serde(default)]
    punktfunk: Option<PluginManifest>,
}

fn read_package(dir: &Path) -> Option<PluginManifest> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    match serde_json::from_str::<PackageJson>(&text) {
        Ok(pkg) => pkg.punktfunk,
        Err(e) => {
            tracing::warn!(package = %dir.display(), error = %e, "plugin manifest: unreadable");
            None
        }
    }
}

#[cfg(test)]
mod manifest_tests {
    use super::*;

    fn manifest(reads: &[&str]) -> PluginManifest {
        PluginManifest {
            schema: 1,
            // An id no grants file in a test environment carries.
            id: "demo-no-grants".into(),
            reads: reads.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn params_take_only_their_own_shape() {
        assert!(param_ok(ParamKind::Name, "Hollow Knight: Silksong"));
        assert!(param_ok(ParamKind::Name, "Sam & Max #2"));
        // Shell syntax is harmless in one quoted argv element; a newline or a leading dash is not.
        assert!(param_ok(ParamKind::Name, "a; rm -rf /"));
        assert!(!param_ok(ParamKind::Name, "two\nlines"));
        assert!(!param_ok(ParamKind::Name, "--output=/etc/passwd"));
        assert!(!param_ok(ParamKind::Path, "-/games/x.sfc"));
        assert!(param_ok(ParamKind::Id, "snes9x_libretro"));
        assert!(!param_ok(ParamKind::Id, "../etc/passwd"));
        assert!(param_ok(ParamKind::Digits, "440"));
        assert!(!param_ok(ParamKind::Digits, "44 0"));
        assert!(param_ok(ParamKind::Args, "--fullscreen"));
        assert!(!param_ok(ParamKind::Args, "--x\ny"));
        assert!(param_ok(ParamKind::Path, "/games/rom.sfc"));
        assert!(!param_ok(ParamKind::Path, "rom.sfc"));
        assert!(!param_ok(ParamKind::Path, ""));
    }

    #[test]
    fn confinement_refuses_traversal_and_foreign_roots() {
        let m = manifest(&["/games", "/opt/emu"]);
        assert!(m.confines(Path::new("/games/snes/rom.sfc")));
        assert!(m.confines(Path::new("/opt/emu/cores/x.so")));
        assert!(!m.confines(Path::new("/etc/shadow")));
        assert!(!m.confines(Path::new("/games/../etc/shadow")));
        assert!(!m.confines(Path::new("relative/path")));
    }

    #[test]
    fn expand_home_roots_only_a_tilde_prefix() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~/legacy"), Some(home.join("legacy")));
            assert_eq!(expand_home("/abs/path"), Some(PathBuf::from("/abs/path")));
        }
    }

    #[test]
    fn a_package_block_round_trips() {
        let json = r#"{"name":"@punktfunk/plugin-bottles","punktfunk":{
            "schema":1,"id":"bottles","reads":["~/.local/share/bottles"],"network":false,
            "exec":{"run":{"exe":"flatpak","args":["run","com.usebottles.bottles","-b","{bottle}"],
                           "params":{"bottle":"name"}}}}}"#;
        let m: PluginManifest = serde_json::from_str::<PackageJson>(json)
            .unwrap()
            .punktfunk
            .unwrap();
        assert_eq!(m.id, "bottles");
        assert_eq!(m.exec["run"].exe, "flatpak");
        assert_eq!(m.exec["run"].params["bottle"], ParamKind::Name);
    }
}
