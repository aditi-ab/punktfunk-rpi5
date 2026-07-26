//! Title launch: resolve a library id / raw command into an executable command line (per-store +
//! per-OS), and the gamescope-session launch helpers. Split out of the `library` facade (plan §W5).

use super::custom::valid_steam_appid;
#[cfg(target_os = "linux")]
use super::heroic::heroic_command;
use super::*;
#[cfg(windows)]
use super::{epic::epic_launch_uri, gog::gog_spawn};

/// Everything a session needs about the title it is launching, resolved in **one** library scan:
/// what to run, what to call it, and how to recognize it once it is running.
///
/// Enumerating the library touches every installed store's on-disk metadata, so the launch path
/// resolves this once at handshake time and threads it into the data plane rather than looking the
/// same id up again per use.
pub struct LaunchTarget {
    /// Identity for the status surface and the `game.*` events.
    pub game: crate::gamelease::GameRef,
    /// How to recognize the running game ([`DetectSpec`]); empty when the store offers nothing.
    pub detect: DetectSpec,
    /// The resolved shell command. `Some` on Linux (where the host runs it); `None` on Windows,
    /// which launches by library id through the interactive-session spawner instead.
    pub command: Option<String>,
}

/// Resolve a store-qualified library id (as sent by a client in `Hello::launch`, or carried on a
/// GameStream catalog entry) against the host's **own** library — so a client can only pick an
/// existing title, never inject a command. `None` = unknown id, or — on Linux — a title with no
/// runnable recipe.
///
/// This is the single lookup, shared by both planes: the client sends only an id, and everything the
/// session does with the title afterwards comes from what the host itself knows about it.
///
/// **Linux**: the resolved command is run by the host (nested into a per-session gamescope, or
/// spawned into the live session). **Windows** has no gamescope to nest into and resolves the
/// concrete process at launch time instead, via [`launch_title`].
pub fn resolve_launch(id: &str) -> Option<LaunchTarget> {
    let entry = all_games().into_iter().find(|g| g.id == id)?;
    let game = crate::gamelease::GameRef {
        id: Some(entry.id.clone()),
        store: Some(entry.store.clone()),
        title: entry.title.clone(),
    };
    #[cfg(not(windows))]
    {
        // Linux runs the command itself, so a title without one has nothing to launch — same answer
        // (and same warning path) as before this resolution existed.
        let command = entry.launch.as_ref().and_then(command_for)?;
        Some(LaunchTarget {
            game,
            detect: entry.detect,
            command: Some(command),
        })
    }
    #[cfg(windows)]
    {
        // Windows resolves the concrete process at launch time (`launch_title`), which is also where
        // a missing recipe is reported — so an entry with no Windows recipe still yields a target and
        // the existing warning fires there.
        Some(LaunchTarget {
            game,
            detect: entry.detect,
            command: None,
        })
    }
}

/// Map a resolved [`LaunchSpec`] to its shell command (pure — the unit-testable core of
/// [`resolve_launch`], split out so the appid-validation can be tested without a Steam install).
///
/// - `steam_appid` → `steam steam://rungameid/<appid>` (appid validated as digits).
/// - `command` → the stored command verbatim. This string comes from the host's own custom store
///   (added by the host operator via the admin UI), never from the client, so it is trusted.
#[cfg(not(windows))]
fn command_for(spec: &LaunchSpec) -> Option<String> {
    match spec.kind.as_str() {
        "steam_appid" => valid_steam_appid(&spec.value)
            .then(|| format!("steam steam://rungameid/{}", spec.value)),
        // Lutris: a digits-only pga.db game id (same guard as steam_appid) → its run URI.
        #[cfg(target_os = "linux")]
        "lutris_id" => (!spec.value.is_empty() && spec.value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| format!("lutris lutris:rungameid/{}", spec.value)),
        // Heroic: `<runner>:<appName>` → the validated heroic://launch command (see heroic_command).
        #[cfg(target_os = "linux")]
        "heroic" => heroic_command(&spec.value),
        // Trusted: the command comes from the host's own custom store, never the client.
        "command" => (!spec.value.trim().is_empty()).then(|| spec.value.clone()),
        _ => None,
    }
}

/// Windows: launch a store-qualified library id into the **interactive user session** — the Windows
/// analogue of the Linux gamescope-nested [`resolve_launch`]. The id is resolved against the host's
/// OWN library (the client never sends a command), mapped to a concrete process by
/// [`windows_launch_for`], and spawned via [`crate::interactive::spawn_in_active_session`].
///
/// Wired into the data plane *after* capture is live, so the title renders onto the already-captured
/// desktop and grabs foreground.
#[cfg(windows)]
pub fn launch_title(id: &str) -> Result<()> {
    let spec = all_games()
        .into_iter()
        .find(|g| g.id == id)
        .and_then(|g| g.launch)
        .ok_or_else(|| anyhow::anyhow!("no launchable library entry '{id}'"))?;
    let (cmdline, workdir) = windows_launch_for(&spec).ok_or_else(|| {
        anyhow::anyhow!(
            "library entry '{id}' has no Windows launch recipe (kind '{}')",
            spec.kind
        )
    })?;
    let pid = crate::interactive::spawn_in_active_session(&cmdline, workdir.as_deref())
        .with_context(|| format!("launch '{id}' in the interactive session"))?;
    tracing::info!(launch_id = id, %cmdline, pid, "launched library title in the interactive session");
    Ok(())
}

/// Windows: map a resolved [`LaunchSpec`] to a `(command line, working dir)` to spawn into the
/// interactive session. Pure + unit-testable. `None` = no Windows recipe for this kind.
///
/// CreateProcessAsUserW does NO shell or protocol resolution, so the URI/flags are handed to a
/// concrete EXE as plain arguments — a (host-derived) URI string can never reach a command interpreter.
#[cfg(windows)]
fn windows_launch_for(spec: &LaunchSpec) -> Option<(String, Option<std::path::PathBuf>)> {
    match spec.kind.as_str() {
        "steam_appid" => {
            if !valid_steam_appid(&spec.value) {
                return None;
            }
            let uri = format!("steam://rungameid/{}", spec.value);
            // Prefer launching Steam.exe with the URI as an argument; fall back to explorer.exe, which
            // resolves the steam:// handler from the user hive. (The appid is digits-validated, so the
            // only variable part of the line is a number either way.)
            let cmdline = match steam_exe() {
                Some(exe) => format!("\"{}\" \"{uri}\"", exe.display()),
                None => format!("explorer.exe \"{uri}\""),
            };
            Some((cmdline, None))
        }
        // Epic: open the (host-built, validated) com.epicgames.launcher:// URI via explorer.exe — a
        // concrete EXE that resolves the registered protocol handler as the user; the URI is a single
        // argv element (no shell, no cmd /c). Same pattern as the steam explorer fallback.
        "epic" => epic_launch_uri(&spec.value).map(|uri| (format!("explorer.exe \"{uri}\""), None)),
        // GOG: spawn the resolved game exe directly (host-derived from goggame-<id>.info), no Galaxy.
        "gog" => gog_spawn(&spec.value),
        // Xbox/Game Pass: activate the UWP/GDK package by its AUMID (<PFN>!<AppId>) via explorer's
        // shell:AppsFolder — which runs in the interactive user session (UWP activation fails as
        // SYSTEM/session-0; spawn_in_active_session uses the user token). Guard the charset (the value
        // is host-derived from MicrosoftGame.config + AppRepository, but belt-and-suspenders).
        "aumid" => {
            let valid = spec.value.split_once('!').is_some_and(|(pfn, app)| {
                let part = |s: &str| {
                    !s.is_empty()
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                };
                part(pfn) && part(app)
            });
            valid.then(|| {
                (
                    format!("explorer.exe \"shell:AppsFolder\\{}\"", spec.value),
                    None,
                )
            })
        }
        // Operator-typed custom command (host-owned, never client-set): run it through the shell in the
        // interactive session. `cmd.exe /c` is acceptable here precisely because the value is operator
        // input — the same trust as the operator typing it — not a client-influenced string.
        "command" => {
            let v = spec.value.trim();
            (!v.is_empty()).then(|| (format!("cmd.exe /c {v}"), None))
        }
        _ => None,
    }
}

/// Windows: the default Steam install's `steam.exe`, if present. A non-default Steam install dir
/// (registry `Valve\Steam\InstallPath`) isn't covered — the explorer.exe protocol fallback handles
/// that case. Mirrors [`steam_roots`]' "default Program Files dirs" approach.
#[cfg(windows)]
fn steam_exe() -> Option<std::path::PathBuf> {
    for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
        if let Some(pf) = std::env::var_os(var) {
            let p = std::path::PathBuf::from(pf).join("Steam").join("steam.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Launch a GameStream `apps.json` command (operator-typed, trusted — never client-set) into the
/// interactive Windows user session, AFTER capture is up (the host is SYSTEM). The Linux paths go
/// through the compositor-aware [`launch_session_command`] instead.
#[cfg(windows)]
pub fn launch_gamestream_command(cmd: &str) -> Result<()> {
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    // cmd.exe /c is fine here: the value is the host operator's own apps.json command, not a
    // client-influenced string (same trust as the custom-store `command` kind).
    let pid = crate::interactive::spawn_in_active_session(&format!("cmd.exe /c {cmd}"), None)
        .context("spawn gamestream command in the interactive session")?;
    tracing::info!(command = %cmd, pid, "gamestream: launched app in the interactive session");
    Ok(())
}

/// Launch a library title chosen from the **GameStream `/applist`** (the store-qualified id is carried
/// on the `AppEntry`, resolved from the numeric Moonlight appid) into the interactive Windows user
/// session ([`launch_title`]). The id is resolved against the host's OWN library, so a client can
/// only ever pick an existing title — never inject a command. Linux resolves the id via
/// [`resolve_launch`] and goes through [`launch_session_command`] instead.
#[cfg(windows)]
pub fn launch_gamestream_library(id: &str) -> Result<()> {
    launch_title(id)
}

/// The child a session launch produced.
///
/// Handed back to the caller so the game's lifetime can be tracked
/// (design/session-game-lifetime.md) instead of the process being forgotten the moment it starts.
#[cfg(target_os = "linux")]
pub struct SpawnedLaunch {
    pub child: std::process::Child,
    /// Whether the child leads its own process group — true for the plain session spawn in [`launch_session_command`], which
    /// deliberately creates one so the whole wrapper tree can be signalled as a unit. False for the
    /// gamescope-session spawn, which shares the host's group (see
    /// [`crate::gamelease::OwnedChild::group_leader`]: a non-leader must never be signalled by
    /// negative pid).
    pub group_leader: bool,
}

/// Launch a resolved shell command into the **live Linux session** for the session's compositor —
/// the one launch entry point shared by the native (punktfunk/1) and GameStream planes, called
/// AFTER capture is up so the app renders onto the streamed output. The command is host-resolved
/// (a library id via [`resolve_launch`], or an operator-typed apps.json/custom command) — never a
/// client-sent string. Best-effort by contract: a failure leaves the user on the (streamed)
/// desktop/session rather than tearing the stream down.
///
/// * **KWin / Mutter / wlroots** — the host runs inside the user's graphical session (the process
///   env was retargeted at it by `apply_session_env`, and the per-session virtual output is
///   promoted primary), so a plain spawn lands the app on the streamed output.
/// * **gamescope (managed / SteamOS / attach)** — the app must open *inside* the running gamescope
///   session: spawned with the session's own `DISPLAY`/Wayland env
///   ([`crate::vdisplay::launch_into_gamescope_session`]). A `steam steam://…` command additionally
///   forwards over the running Steam instance's own pipe, so the dominant Steam case is
///   env-independent.
/// * **gamescope (bare spawn)** — not routed here: the command was nested into the fresh gamescope
///   via `set_launch_command` (the caller gates on `vdisplay::launch_is_nested`).
#[cfg(target_os = "linux")]
pub fn launch_session_command(
    compositor: crate::vdisplay::Compositor,
    cmd: &str,
) -> Result<SpawnedLaunch> {
    use std::os::unix::process::CommandExt;
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    let (child, group_leader) = match compositor {
        crate::vdisplay::Compositor::Gamescope => {
            (crate::vdisplay::launch_into_gamescope_session(cmd)?, false)
        }
        _ => (
            std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                // Its own process group, so ending this game later signals the shell *and* the game
                // it exec'd or forked — the whole tree the host started — and nothing else. Also
                // detaches it from any signal the host's own group receives.
                .process_group(0)
                .spawn()
                .context("spawn launch command")?,
            true,
        ),
    };
    tracing::info!(
        command = %cmd,
        pid = child.id(),
        compositor = compositor.id(),
        "launched app into the live session"
    );
    Ok(SpawnedLaunch {
        child,
        group_leader,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn launch_command_resolves_and_guards() {
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
        };
        assert_eq!(
            command_for(&steam).as_deref(),
            Some("steam steam://rungameid/570")
        );
        // A non-numeric "appid" (e.g. a client trying to inject) is rejected, never interpolated.
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570; rm -rf ~".into(),
        };
        assert_eq!(command_for(&evil), None);
        // Custom commands (from the host's own store) pass through verbatim.
        let custom = LaunchSpec {
            kind: "command".into(),
            value: "dolphin-emu --batch".into(),
        };
        assert_eq!(command_for(&custom).as_deref(), Some("dolphin-emu --batch"));
        // Empty / unknown kinds → no command.
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "command".into(),
                value: "  ".into()
            }),
            None
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "wat".into(),
                value: "x".into()
            }),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn command_for_lutris_and_heroic_guards() {
        // Lutris: digits → its run URI; a non-numeric id (injection attempt) is rejected.
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42".into()
            })
            .as_deref(),
            Some("lutris lutris:rungameid/42")
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42; rm -rf ~".into()
            }),
            None
        );
        // Heroic guards (independent of whether Heroic is installed): bad runner / appName → None.
        assert_eq!(heroic_command("badrunner:Quail"), None);
        assert_eq!(heroic_command("legendary:bad name"), None);
        assert_eq!(heroic_command("nile:"), None);
        // When Heroic IS resolvable (a dev box), a valid id yields the launch URI; on CI (no Heroic)
        // it's None — assert the URI shape only when a launcher prefix exists.
        if let Some(cmd) = heroic_command("legendary:Quail-1.2_x") {
            assert!(cmd.contains("heroic://launch?appName=Quail-1.2_x&runner=legendary"));
            assert!(cmd.contains("--no-gui"));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_for_maps_and_guards() {
        // Steam: a digits-only appid → a steam:// URI line (via Steam.exe or explorer.exe, depending
        // on the box) with no working dir.
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
        };
        let (line, wd) = windows_launch_for(&steam).expect("steam recipe");
        assert!(line.contains("steam://rungameid/570"), "line was {line:?}");
        assert!(wd.is_none());
        // A non-numeric "appid" (a client trying to inject) is rejected, never interpolated.
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570\" & calc".into(),
        };
        assert!(windows_launch_for(&evil).is_none());
        // Operator command → cmd /c passthrough (trusted host input).
        let cmd = LaunchSpec {
            kind: "command".into(),
            value: "notepad.exe".into(),
        };
        assert_eq!(
            windows_launch_for(&cmd).unwrap().0,
            "cmd.exe /c notepad.exe"
        );
        // Xbox AUMID → explorer shell:AppsFolder activation; a value without '!' is rejected.
        let aumid = LaunchSpec {
            kind: "aumid".into(),
            value: "Microsoft.X_8wekyb3d8bbwe!Game".into(),
        };
        assert_eq!(
            windows_launch_for(&aumid).unwrap().0,
            "explorer.exe \"shell:AppsFolder\\Microsoft.X_8wekyb3d8bbwe!Game\""
        );
        assert!(windows_launch_for(&LaunchSpec {
            kind: "aumid".into(),
            value: "no-bang".into()
        })
        .is_none());
        // Empty / unknown kinds → no recipe.
        assert!(windows_launch_for(&LaunchSpec {
            kind: "command".into(),
            value: "  ".into()
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "wat".into(),
            value: "x".into()
        })
        .is_none());
    }
}
