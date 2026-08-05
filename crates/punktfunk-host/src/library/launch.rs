//! Title launch: resolve a library id / raw command into an executable command line (per-store +
//! per-OS), and the gamescope-session launch helpers. Split out of the `library` facade (plan §W5).
//!
//! This module owns the **whole launch side** of the library: the `kind` vocabulary, its per-kind
//! charset validators, and the per-OS resolvers. That split is deliberate and load-bearing — the
//! scanner modules beside it do *enumeration only*, so they can be lifted out into library plugins
//! without taking any launch logic with them (design/library-scanner-plugins.md D1: a client sends
//! only an entry id and the host resolves the [`LaunchSpec`] it holds, which stays true whether the
//! entry was enumerated in-process or reconciled in by a plugin).

use super::*;

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

// ------------------------------------------------------- per-kind launch values (host-owned ABI)
//
// Each helper below turns a store's launch VALUE — the only part a scanner (or, after extraction, a
// library plugin) supplies — into the URI/command line the host actually runs. They live here rather
// than beside the enumeration that produces the value because the host keeps owning URI construction
// and spawning no matter where the enumeration came from (D1). Every one of them is total and
// validating: an unparseable or hostile value yields `None`, never a partially-interpolated command.

/// A digits-only Steam appid: the sole client-influenced part of a Steam launch, validated before it
/// is interpolated into any command / URI (so a client-sent id can never carry shell or URI syntax).
/// Cross-platform — used by the Linux shell mapping ([`command_for`]) and the Windows spawn mapping
/// ([`windows_launch_for`]).
///
/// Also accepts the 64-bit non-Steam-shortcut game id ([`shortcut_gameid`]), which is likewise
/// digits — the two share the `steam_appid` kind precisely because `rungameid` takes either.
pub(crate) fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// The 64-bit game id `steam://rungameid/` needs to launch a non-Steam shortcut: high dword = the
/// 32-bit shortcut appid, low dword = the shortcut marker `0x0200_0000`. (Handing `rungameid` the
/// bare 32-bit appid does not launch a shortcut — it must be this composed id.)
pub(crate) fn shortcut_gameid(appid: u32) -> u64 {
    ((appid as u64) << 32) | 0x0200_0000
}

/// Map a `heroic` LaunchSpec value (`<runner>:<appName>`) to the Heroic launch command, run nested in
/// gamescope. The host owns this mapping; the client only ever sends the id. CAVEAT: Heroic is a
/// single-instance Electron app — in a fresh per-session gamescope it boots, launches the game (which
/// renders into that gamescope) and stays hidden via `--no-gui`; but if a Heroic GUI is ALREADY
/// running on the box, the spawned process forwards the URI and exits, which would tear the session
/// down. The validated path is the fresh-session case; needs live confirmation on a box with Heroic.
#[cfg(target_os = "linux")]
pub(crate) fn heroic_command(value: &str) -> Option<String> {
    let (runner, app) = value.split_once(':')?;
    if !matches!(runner, "legendary" | "gog" | "nile") {
        return None;
    }
    // appName charset (Epic alnum, GOG digits, Amazon alnum) — keep the URI a single safe token.
    if app.is_empty()
        || !app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    let prefix = heroic_launch_prefix()?;
    // No quotes: gamescope spawns the app by `split_whitespace()`, and the URI has no spaces (appName
    // is validated above) so it stays a single argv token; `&` is fine (exec'd, not shell-parsed).
    Some(format!(
        "{prefix} --no-gui heroic://launch?appName={app}&runner={runner}"
    ))
}

/// How to invoke Heroic: the native `heroic` binary if on `PATH`, else the Flatpak app if its data
/// root is present. `None` ⇒ Heroic not found, so no launch command.
#[cfg(target_os = "linux")]
fn heroic_launch_prefix() -> Option<String> {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("heroic").is_file()));
    if on_path {
        return Some("heroic".into());
    }
    let flatpak = std::env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|h| h.join(".var/app/com.heroicgameslauncher.hgl").is_dir());
    flatpak.then(|| "flatpak run com.heroicgameslauncher.hgl".into())
}

/// Map an `epic` LaunchSpec value to the Epic Games Launcher URI. The value is either the full
/// `<namespace>:<catalogItemId>:<appName>` triple (what the manifests carry) or a bare `appName`;
/// every part is charset-checked so the URI stays one safe argv token.
#[cfg(windows)]
pub(crate) fn epic_launch_uri(value: &str) -> Option<String> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    let inner = match value.split(':').collect::<Vec<_>>().as_slice() {
        [ns, cat, app] if ok(ns) && ok(cat) && ok(app) => format!("{ns}%3A{cat}%3A{app}"),
        [app] if ok(app) => (*app).to_string(),
        _ => return None,
    };
    Some(format!(
        "com.epicgames.launcher://apps/{inner}?action=launch&silent=true"
    ))
}

/// Map a `gog` LaunchSpec value — the tab-separated `exe \t args \t workdir` spawn triple the scanner
/// derived from `goggame-<id>.info` — to a `(command line, working dir)`. GOG games are spawned
/// directly (no Galaxy), so the exe is quoted and the arguments ride verbatim.
#[cfg(windows)]
pub(crate) fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    let mut parts = value.split('\t');
    let exe = parts.next().filter(|s| !s.is_empty())?;
    let args = parts.next().unwrap_or("");
    let workdir = parts.next().filter(|s| !s.is_empty()).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
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

    #[test]
    fn steam_appid_validation_accepts_appids_and_shortcut_gameids() {
        assert!(valid_steam_appid("570"));
        // The 64-bit shortcut game id shares the `steam_appid` kind — `rungameid` takes either.
        assert!(valid_steam_appid(
            &shortcut_gameid(2_456_789_012).to_string()
        ));
        assert!(!valid_steam_appid(""));
        assert!(!valid_steam_appid("570; rm -rf ~"));
        assert!(!valid_steam_appid("-1"));
    }

    /// Moved here with `shortcut_gameid` (WP1.1): the composed id is launch vocabulary, not
    /// enumeration — the scanner only supplies the 32-bit appid it read out of `shortcuts.vdf`.
    #[test]
    fn shortcut_gameid_composes_appid_and_marker() {
        let id = shortcut_gameid(0x8000_0000);
        assert_eq!(id >> 32, 0x8000_0000, "high dword is the shortcut appid");
        assert_eq!(id & 0xFFFF_FFFF, 0x0200_0000, "low dword is the marker");
    }

    #[cfg(windows)]
    #[test]
    fn epic_launch_uri_triple_bare_and_guard() {
        assert_eq!(
            epic_launch_uri("fn:abc:Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/fn%3Aabc%3AFortnite?action=launch&silent=true")
        );
        assert_eq!(
            epic_launch_uri("Fortnite").as_deref(),
            Some("com.epicgames.launcher://apps/Fortnite?action=launch&silent=true")
        );
        assert!(epic_launch_uri("bad part:x:y").is_none()); // a space → rejected
        assert!(epic_launch_uri("").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn gog_spawn_parses_and_guards() {
        let (cmd, wd) = gog_spawn("C:\\Games\\W3\\witcher3.exe\t--skip\tC:\\Games\\W3").unwrap();
        assert_eq!(cmd, "\"C:\\Games\\W3\\witcher3.exe\" --skip");
        assert_eq!(wd, Some(std::path::PathBuf::from("C:\\Games\\W3")));
        let (cmd2, wd2) = gog_spawn("C:\\g.exe").unwrap();
        assert_eq!(cmd2, "\"C:\\g.exe\"");
        assert!(wd2.is_none());
        assert!(gog_spawn("").is_none());
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
