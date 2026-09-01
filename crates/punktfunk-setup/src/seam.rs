//! The two injection seams every other module reads the world through.
//!
//! `BasePaths` owns every filesystem root a probe may touch; `CommandRunner` owns every
//! process spawn. Nothing else in the crate may call `std::process::Command::new` — the
//! crate-local `clippy.toml` denies it, and `SystemRunner` carries the single `#[allow]`.
//!
//! They are what make `--demo` on a Mac and the whole test suite honest: swap both for the
//! fake pair and the engine cannot reach the machine it is running on, by construction rather
//! than by remembering to check a flag. See `design/installer-v2.md` D3 and §7.
//!
//! The `PUNKTFUNK_INSTALL_OS_RELEASE` / `_ETC` env twins the sh installer grew are honoured
//! here so installer-smoke's fake boxes keep working against the binary unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Every filesystem root a probe is allowed to read.
///
/// `etc_root` is a *prefix*, not the directory itself: the sh script reads
/// `"$ETC/etc/apt/..."`, so an empty override means the real `/etc`.
#[derive(Debug, Clone)]
pub struct BasePaths {
    pub os_release: PathBuf,
    pub etc_root: PathBuf,
    pub sys: PathBuf,
    pub run: PathBuf,
    pub config: PathBuf,
    pub home: PathBuf,
}

impl BasePaths {
    /// The real box, honouring the two test seams the sh installer already exposed.
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/root"), PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let etc_root = std::env::var_os("PUNKTFUNK_INSTALL_ETC")
            .map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let os_release = std::env::var_os("PUNKTFUNK_INSTALL_OS_RELEASE")
            .map_or_else(|| PathBuf::from("/etc/os-release"), PathBuf::from);
        Self {
            os_release,
            etc_root,
            sys: "/sys".into(),
            run: "/run".into(),
            config,
            home,
        }
    }

    /// A tree rooted at `root` — the shape every Facts test builds in a tempdir.
    pub fn rooted(root: &Path) -> Self {
        Self {
            os_release: root.join("etc/os-release"),
            etc_root: root.to_path_buf(),
            sys: root.join("sys"),
            run: root.join("run"),
            config: root.join("config"),
            home: root.join("home"),
        }
    }

    /// `/etc/<rel>` under the configured root.
    pub fn etc(&self, rel: &str) -> PathBuf {
        self.etc_root.join("etc").join(rel)
    }

    /// The host's env file. Its path is reported verbatim in dry-run output.
    pub fn host_env(&self) -> PathBuf {
        self.config.join("punktfunk/host.env")
    }

    pub fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// The process environment, snapshotted once so probes stay hermetic.
///
/// Reading `std::env` directly inside `facts` would make the seat probe untestable: Rust 2024
/// makes `set_var` unsafe, so a test cannot stage `DISPLAY` around a parallel suite.
#[derive(Debug, Default, Clone)]
pub struct Env(pub HashMap<String, String>);

impl Env {
    pub fn from_env() -> Self {
        Self(std::env::vars().collect())
    }

    /// Only the values a test actually stages; everything else reads as unset.
    pub fn of(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// How a spawned command gets its stdin.
///
/// `Tty` is the package manager's own confirmation prompt reaching the user; without a
/// terminal it must be `/dev/null` rather than the script's own stdin under `curl | sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    Tty,
    Null,
}

/// The outcome of a probe spawn. `None` means the binary was not found at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// A snippet that could not start, or exited non-zero. It carries no message on purpose: the
/// caller owns the failure text, because the sh installer's die line is part of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunFailed;

/// Every process spawn in the crate goes through here.
pub trait CommandRunner {
    fn run_shell(&self, cmd: &str, stdin: Stdin) -> Result<(), RunFailed>;

    /// Spawn a probe and capture it. `None` when the program is not on `PATH`.
    fn probe(&self, program: &str, args: &[&str]) -> Option<Output>;

    /// `command -v <program>` — is it on `PATH`?
    fn which(&self, program: &str) -> bool;

    /// First line of `<program> <args…>` stdout, trimmed, when it exited 0.
    fn first_line(&self, program: &str, args: &[&str]) -> Option<String> {
        let out = self.probe(program, args)?;
        if !out.ok() {
            return None;
        }
        out.stdout.lines().next().map(|l| l.trim().to_string())
    }
}

/// The one implementation that touches the machine.
pub struct SystemRunner {
    /// Prepended to `PATH` for the root-without-sudo shim (`design/installer-v2.md` §4).
    pub path_prefix: Option<PathBuf>,
    /// Exported into every spawn. The plan's commands carry a literal `"$USER"`, so it has to
    /// be set even where the installer was invoked without one.
    pub exports: Vec<(String, String)>,
}

impl SystemRunner {
    pub fn new() -> Self {
        Self {
            path_prefix: None,
            exports: Vec::new(),
        }
    }

    // The single sanctioned `Command::new` in the crate; everything else routes through the
    // trait so demo mode and the tests cannot be bypassed by accident.
    #[allow(clippy::disallowed_methods)]
    fn command(&self, program: &str) -> std::process::Command {
        let mut c = std::process::Command::new(program);
        for (key, value) in &self.exports {
            c.env(key, value);
        }
        if let Some(prefix) = &self.path_prefix {
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut parts = vec![prefix.clone()];
            parts.extend(std::env::split_paths(&existing));
            if let Ok(joined) = std::env::join_paths(parts) {
                c.env("PATH", joined);
            }
        }
        c
    }
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for SystemRunner {
    fn run_shell(&self, cmd: &str, stdin: Stdin) -> Result<(), RunFailed> {
        let source = match stdin {
            Stdin::Tty => std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok(),
            Stdin::Null => None,
        };
        let mut c = self.command("sh");
        c.arg("-ec").arg(cmd);
        match source {
            Some(tty) => c.stdin(std::process::Stdio::from(tty)),
            None => c.stdin(std::process::Stdio::null()),
        };
        match c.status() {
            Ok(s) if s.success() => Ok(()),
            _ => Err(RunFailed),
        }
    }

    fn probe(&self, program: &str, args: &[&str]) -> Option<Output> {
        let out = self
            .command(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        Some(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn which(&self, program: &str) -> bool {
        self.probe(
            "sh",
            &["-c", &format!("command -v {program} >/dev/null 2>&1")],
        )
        .is_some_and(|o| o.ok())
    }
}

/// A runner that answers from a script and spawns nothing.
///
/// Keyed by the whole command line (`"systemctl is-active sunshine.service"`), so a test says
/// what the box answers and nothing else can leak in: an unscripted probe returns `None`,
/// which is the same answer as "that binary is not installed".
#[derive(Debug, Default, Clone)]
pub struct FakeRunner {
    pub answers: HashMap<String, Output>,
    pub on_path: Vec<String>,
    /// Every `run_shell` snippet, in order — what the exec tests assert against.
    pub ran: std::cell::RefCell<Vec<String>>,
    /// Snippets that must fail, matched as a substring.
    pub fail_matching: Option<String>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripted answer for a probe, e.g. `answer("punktfunk-host detect-conflicts", 1, "")`.
    pub fn answer(mut self, line: &str, code: i32, stdout: &str) -> Self {
        self.answers.insert(
            line.to_string(),
            Output {
                code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        self.on_path
            .push(line.split_whitespace().next().unwrap_or(line).to_string());
        self
    }

    pub fn with_path(mut self, program: &str) -> Self {
        self.on_path.push(program.to_string());
        self
    }
}

impl CommandRunner for FakeRunner {
    fn run_shell(&self, cmd: &str, _stdin: Stdin) -> Result<(), RunFailed> {
        self.ran.borrow_mut().push(cmd.to_string());
        match &self.fail_matching {
            Some(needle) if cmd.contains(needle.as_str()) => Err(RunFailed),
            _ => Ok(()),
        }
    }

    fn probe(&self, program: &str, args: &[&str]) -> Option<Output> {
        let mut line = String::from(program);
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        if let Some(out) = self.answers.get(&line) {
            return Some(out.clone());
        }
        // An unscripted probe of a program that IS on PATH exits non-zero rather than
        // vanishing: "installed but says no" and "not installed" are different answers.
        if self.on_path.iter().any(|p| p == program) {
            return Some(Output {
                code: 1,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        None
    }

    fn which(&self, program: &str) -> bool {
        self.on_path.iter().any(|p| p == program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etc_root_is_a_prefix_not_the_directory() {
        let p = BasePaths::rooted(Path::new("/tmp/box"));
        assert_eq!(
            p.etc("apt/sources.list.d/punktfunk.list").to_str().unwrap(),
            "/tmp/box/etc/apt/sources.list.d/punktfunk.list"
        );
    }

    #[test]
    fn an_unscripted_probe_of_a_missing_program_is_none() {
        let r = FakeRunner::new().with_path("systemctl");
        assert!(r.probe("nvidia-smi", &[]).is_none());
        assert_eq!(r.probe("systemctl", &["is-active", "x"]).unwrap().code, 1);
    }

    #[test]
    fn fake_runner_records_every_snippet_it_was_handed() {
        let r = FakeRunner::new();
        r.run_shell("sudo apt update", Stdin::Null).unwrap();
        assert_eq!(r.ran.borrow().as_slice(), ["sudo apt update"]);
    }
}
