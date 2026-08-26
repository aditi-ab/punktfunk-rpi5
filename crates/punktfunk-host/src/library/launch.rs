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
    /// This entry opens a LAUNCHER, not a game (design D4) — so there is no "the game exited"
    /// moment to detect, and the lease stays untracked no matter what else is known about it.
    /// See [`crate::gamelease::LeaseRequest::launcher`].
    pub launcher: bool,
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
        let command = plugin_recipe(&entry)
            .map(|l| l.command)
            .or_else(|| entry.launch.as_ref().and_then(command_for))?;
        Some(LaunchTarget {
            game,
            launcher: entry.role == GameRole::Launcher,
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
            launcher: entry.role == GameRole::Launcher,
            detect: entry.detect,
            command: None,
        })
    }
}

/// The recipe for a `plugin`-kind entry, asked of the plugin that owns it. `None` for every other
/// kind (without doing any I/O), so both per-OS resolvers can simply try this first.
///
/// This lives beside [`resolve_launch`] / [`launch_title`] rather than inside `command_for` /
/// `windows_launch_for` because it needs the entry's **`provider`** — and that field is the whole
/// authorization story. `provider` is stamped by the host from the reconcile URL
/// (`PUT /library/provider/{provider}`), never taken from the payload, so it is what decides which
/// plugin gets asked. A plugin that plants an entry under someone else's provider only causes that
/// *other* plugin to be asked about a key it never published — which is a 404, not a launch.
///
/// **Blocking**: see [`ask_plugin_launch`]. `resolve_launch`'s async callers hop through
/// `spawn_blocking`; the handshake probe uses [`launch_is_resolvable`], which never asks.
fn plugin_recipe(entry: &GameEntry) -> Option<PluginLaunch> {
    let spec = entry.launch.as_ref()?;
    if spec.kind != "plugin" {
        return None;
    }
    let Some(provider) = entry.provider.as_deref() else {
        // Only a provider reconcile can author this kind, so this is unreachable short of a
        // hand-edited library.json — say so rather than silently doing nothing.
        tracing::warn!(
            id = %entry.id,
            "plugin launch: entry carries no provider, so no plugin can answer for it"
        );
        return None;
    };
    ask_plugin_launch(provider, &spec.value)
}

/// Whether `id` will actually launch something — **without asking a plugin**.
///
/// The handshake needs this one bit to decide dedicated-session routing, and it runs on the async
/// path, so it must not make a blocking call out to a plugin. For a `plugin`-kind entry the cheap
/// answer is "a live plugin is registered under its provider, and the key is well formed"; if that
/// plugin later refuses the ask, the launch fails the same way any unresolvable entry does and the
/// player is left on the session.
#[cfg(not(windows))]
pub fn launch_is_resolvable(id: &str) -> bool {
    let Some(entry) = all_games().into_iter().find(|g| g.id == id) else {
        return false;
    };
    let Some(spec) = entry.launch.as_ref() else {
        return false;
    };
    if spec.kind == "plugin" {
        return valid_plugin_entry_key(&spec.value)
            && entry
                .provider
                .as_deref()
                .is_some_and(|p| crate::mgmt::ui_credential(p).is_some());
    }
    command_for(spec).is_some()
}

/// Map a resolved [`LaunchSpec`] to its shell command (pure — the unit-testable core of
/// [`resolve_launch`], split out so the appid-validation can be tested without a Steam install).
///
/// The `plugin` kind is deliberately absent: its answer comes from another process, so it is
/// resolved by [`plugin_recipe`] before this is reached.
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
        // A launcher entry (D4): open the Steam client itself, in Big Picture or on the desktop.
        // Nested in gamescope this is the SteamOS game-mode shape.
        "steam_ui" => match spec.value.as_str() {
            "bigpicture" => Some("steam -gamepadui".into()),
            "desktop" => Some("steam".into()),
            _ => None,
        },
        // The other launchers' own UIs (D4). The host builds the command — a plugin only names
        // which launcher — so no shell string ever crosses the wire.
        #[cfg(target_os = "linux")]
        "launcher_ui" => match spec.value.as_str() {
            // The same resolution the `heroic` game launches use (native binary, else Flatpak), just
            // without `--no-gui` and without a URI: that opens Heroic's window, which IS the tile.
            "heroic" => heroic_launch_prefix(),
            // Heroic's console mode — its couch front end, the Big Picture of this launcher.
            //
            // It takes TWO flags, which is not obvious and is why this is the host's business and
            // not a plugin's: `--console` only routes the UI to that front end, and `--fullscreen`
            // is what actually fills the screen (Heroic reads them separately —
            // `isCLIConsoleMode` / `isCLIFullscreen`). Neither is a URI: `heroic://` speaks only
            // `ping` and `launch`, so a protocol hand-off cannot reach console mode at all — the
            // same reason Playnite's fullscreen tile spawns its exe directly.
            //
            // Console mode arrived in Heroic 2.21.0. An older Heroic ignores the unknown
            // `--console` and honours `--fullscreen`, so the tile degrades to a fullscreen desktop
            // UI rather than to nothing.
            "heroic-console" => {
                heroic_launch_prefix().map(|p| format!("{p} --console --fullscreen"))
            }
            // Bare `lutris` opens the Lutris window; with a `lutris:rungameid/…` URI it launches a
            // game instead (the `lutris_id` kind above).
            "lutris" => Some("lutris".into()),
            _ => None,
        },
        // Trusted: the command comes from the host's own custom store, never the client.
        "command" => (!spec.value.trim().is_empty()).then(|| spec.value.clone()),
        _ => None,
    }
}

/// A resolved Windows launch: the command line to spawn, the directory to spawn it in, and whether
/// the process that line starts **is** the game.
///
/// [`Self::owns_game`] is the whole reason this is a struct and not a pair. Almost every Windows
/// recipe is a protocol hand-off — `explorer.exe "playnite://…"`, `Steam.exe "steam://…"` — that
/// forwards the request to whichever launcher owns the title and then exits. Its pid is a
/// forwarder's, so that pid's lifetime says nothing about the game's, in either direction:
///
/// * the launcher was already running, so the forwarder quits a second later — read as a `Child`
///   lease, that is the game "exiting" while it is still loading;
/// * the launcher was *not* running, so the process the host started becomes the launcher itself
///   and outlives every game the player then quits — a lease that can never report an exit.
///
/// Only a line that starts the game (or the operator's own command) directly earns its pid a place
/// in [`crate::gamelease::LeaseRequest::spawned`]; a hand-off pid is dropped, and the lease falls
/// back to the title's detect signals, exactly as it did before the pid was carried at all.
#[cfg(windows)]
pub struct WinRecipe {
    /// The full command line to hand to `CreateProcessAsUserW`.
    pub cmdline: String,
    /// The working directory to start it in, when the recipe needs a specific one.
    pub workdir: Option<std::path::PathBuf>,
    /// See the type docs: `false` for a protocol/launcher hand-off.
    pub owns_game: bool,
}

#[cfg(windows)]
impl WinRecipe {
    /// A line that forwards the launch to whoever owns the title and then exits.
    fn handoff(cmdline: String) -> Self {
        Self {
            cmdline,
            workdir: None,
            owns_game: false,
        }
    }

    /// A line that starts the game — or the operator's own command — as its own process.
    fn game(cmdline: String, workdir: Option<std::path::PathBuf>) -> Self {
        Self {
            cmdline,
            workdir,
            owns_game: true,
        }
    }
}

/// What a Windows launch started, as the lease needs to hear it — see [`WinRecipe::owns_game`].
#[cfg(windows)]
pub struct WindowsLaunch {
    /// The pid `CreateProcessAsUserW` handed back.
    pub pid: u32,
    /// Whether that pid is the game's rather than a forwarder's.
    pub owns_game: bool,
}

#[cfg(windows)]
impl WindowsLaunch {
    /// The pid to carry on the lease: `None` when all the host started was a hand-off.
    pub fn tracked_pid(&self) -> Option<u32> {
        self.owns_game.then_some(self.pid)
    }
}

/// Windows: launch a store-qualified library id into the **interactive user session** — the Windows
/// analogue of the Linux gamescope-nested [`resolve_launch`]. The id is resolved against the host's
/// OWN library (the client never sends a command), mapped to a concrete process by
/// [`windows_launch_for`], and spawned via [`crate::interactive::spawn_in_active_session`].
///
/// Wired into the data plane *after* capture is live, so the title renders onto the already-captured
/// desktop and grabs foreground.
///
/// Returns the process it started and whether that process is the game ([`WindowsLaunch`]) — the
/// pid is what the caller hands to [`crate::gamelease::LeaseRequest::spawned`], but only when it
/// belongs to the game. It used to be logged and discarded, and that was the whole of Windows'
/// disadvantage against Linux here: with no `Child` to hold and no pid kept, a title whose provider
/// supplied no detect hint left the lease nothing to watch or signal.
#[cfg(windows)]
pub fn launch_title(id: &str) -> Result<WindowsLaunch> {
    let entry = all_games()
        .into_iter()
        .find(|g| g.id == id)
        .filter(|g| g.launch.is_some())
        .ok_or_else(|| anyhow::anyhow!("no launchable library entry '{id}'"))?;
    let spec = entry.launch.clone().expect("filtered to Some above");
    // A `plugin` entry's recipe comes from the plugin that owns it, and arrives in the same
    // (command line, working dir) shape this path already spawns. `windows_launch_for` has no arm
    // for the kind, so a failed ask falls through to the "no recipe" error below.
    // A plugin publishes a concrete `(command line, working dir)` for its own title, the same shape
    // the operator-typed `command` kind produces — so it is spawned, and tracked, on the same terms.
    let recipe = plugin_recipe(&entry)
        .map(|l| WinRecipe::game(l.command, l.cwd))
        .or_else(|| windows_launch_for(&spec))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "library entry '{id}' has no Windows launch recipe (kind '{}')",
                spec.kind
            )
        })?;
    let WinRecipe {
        cmdline,
        workdir,
        owns_game,
    } = recipe;
    let pid = crate::interactive::spawn_in_active_session(&cmdline, workdir.as_deref())
        .with_context(|| format!("launch '{id}' in the interactive session"))?;
    tracing::info!(
        launch_id = id,
        %cmdline,
        pid,
        owns_game,
        "launched library title in the interactive session"
    );
    Ok(WindowsLaunch { pid, owns_game })
}

/// Windows: map a resolved [`LaunchSpec`] to a `(command line, working dir)` to spawn into the
/// interactive session. Pure + unit-testable. `None` = no Windows recipe for this kind.
///
/// CreateProcessAsUserW does NO shell or protocol resolution, so the URI/flags are handed to a
/// concrete EXE as plain arguments — a (host-derived) URI string can never reach a command interpreter.
///
/// The `plugin` kind is deliberately absent: its answer comes from another process, so it is
/// resolved by [`plugin_recipe`] before this is reached.
#[cfg(windows)]
fn windows_launch_for(spec: &LaunchSpec) -> Option<WinRecipe> {
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
            // Either line is a forwarder: `Steam.exe <uri>` against a running client posts the URI
            // and exits, and against a cold one it *becomes* the client. Neither is the game.
            Some(WinRecipe::handoff(cmdline))
        }
        // A launcher entry (D4): open the Steam client's own UI. Same Steam.exe-then-explorer ladder
        // as `steam_appid`, and the URI is one of exactly two host-owned literals — nothing from the
        // entry is interpolated at all.
        "steam_ui" => {
            let uri = match spec.value.as_str() {
                "bigpicture" => "steam://open/bigpicture",
                "desktop" => "steam://open/main",
                _ => return None,
            };
            let cmdline = match steam_exe() {
                Some(exe) => format!("\"{}\" \"{uri}\"", exe.display()),
                None => format!("explorer.exe \"{uri}\""),
            };
            Some(WinRecipe::handoff(cmdline))
        }
        // Epic: open the (host-built, validated) com.epicgames.launcher:// URI via explorer.exe — a
        // concrete EXE that resolves the registered protocol handler as the user; the URI is a single
        // argv element (no shell, no cmd /c). Same pattern as the steam explorer fallback.
        "epic" => epic_launch_uri(&spec.value)
            .map(|uri| WinRecipe::handoff(format!("explorer.exe \"{uri}\""))),
        // GOG: spawn the game's own exe directly (no Galaxy) — the one store recipe that is NOT a
        // hand-off. The triple comes from the plugin, so `gog_spawn` re-confines it to a GOG install
        // the host finds itself.
        "gog" => gog_spawn(&spec.value).map(|(cmdline, workdir)| WinRecipe::game(cmdline, workdir)),
        // Xbox/Game Pass: activate the UWP/GDK package by its AUMID (<PFN>!<AppId>) via explorer's
        // shell:AppsFolder — which runs in the interactive user session (UWP activation fails as
        // SYSTEM/session-0; spawn_in_active_session uses the user token). Guard the charset (the value
        // is host-derived from MicrosoftGame.config + AppRepository, but belt-and-suspenders).
        "aumid" => valid_aumid(&spec.value).then(|| {
            WinRecipe::handoff(format!("explorer.exe \"shell:AppsFolder\\{}\"", spec.value))
        }),
        // Xbox / Game Pass from a library PLUGIN: `<Identity>!<AppId>`, both read straight out of
        // `MicrosoftGame.config`. The host completes it into the AUMID.
        //
        // This kind exists because of a measured privilege asymmetry (2026-08-06): resolving the
        // PackageFamilyName means enumerating `%ProgramData%\…\AppRepository\Packages`, which is
        // denied to `NT AUTHORITY\LocalService` — the principal the plugin runner runs as — and
        // allowed to the host, which runs as LocalSystem. So the plugin sends what it can read and
        // the host reads the authoritative publisher hash itself, at launch time.
        //
        // Resolving here rather than caching at install time also means a package update that
        // changes the hash cannot leave a stale, unlaunchable tile behind.
        "xbox" => {
            let (identity, app_id) = spec.value.split_once('!')?;
            if !aumid_part(identity) || !aumid_part(app_id) {
                return None;
            }
            let pfn = xbox_pfn(identity)?;
            Some(WinRecipe::handoff(format!(
                "explorer.exe \"shell:AppsFolder\\{pfn}!{app_id}\""
            )))
        }
        // Playnite: open the game through Playnite's own URI handler, which is what actually knows
        // how to start it (Playnite maps the id to whichever store owns the title). explorer.exe
        // resolves the registered protocol as the user — the same pattern as the `epic` kind — and
        // the id is GUID-validated, so the only variable part of the line is 36 hex-and-dash chars.
        //
        // This kind exists because the plugin used to publish `kind: "command"` (a `start ""` shell
        // line). The 2026-08-05 review made `command` operator-only, which refuses a plugin's whole
        // reconcile — so without a typed kind the Playnite plugin cannot publish anything at all.
        "playnite" => valid_playnite_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"playnite://playnite/start/{}\"",
                spec.value
            ))
        }),
        // A launcher entry (D4) on Windows: today that is Playnite's Fullscreen app, spawned
        // directly (its `playnite://` handler opens the DESKTOP app, so no URI can do this). The
        // value is the literal "playnite" — nothing from the entry reaches the command line — and
        // the working directory is Playnite's own install dir, as a .NET app expects.
        "launcher_ui" => match spec.value.as_str() {
            "playnite" => playnite_fullscreen_exe().map(|exe| {
                let dir = exe.parent().map(std::path::Path::to_path_buf);
                WinRecipe::game(format!("\"{}\"", exe.display()), dir)
            }),
            _ => None,
        },
        // Operator-typed custom command (host-owned, never client-set): run it through the shell in the
        // interactive session. `cmd.exe /c` is acceptable here precisely because the value is operator
        // input — the same trust as the operator typing it — not a client-influenced string.
        // `cmd.exe /c <v>` blocks until the operator's command returns, so its pid tracks that
        // command's life — the Windows twin of the Linux child the host holds.
        "command" => {
            let v = spec.value.trim();
            (!v.is_empty()).then(|| WinRecipe::game(format!("cmd.exe /c {v}"), None))
        }
        _ => None,
    }
}

/// Windows: the default Steam install's `steam.exe`, if present. A non-default Steam install dir
/// (registry `Valve\Steam\InstallPath`) isn't covered — the explorer.exe protocol fallback handles
/// that case. Probes the default Program Files dirs, in `ProgramFiles(x86)`-first order.
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

/// Resolve a package's PackageFamilyName by finding its
/// `AppRepository\Packages\<PackageFullName>` dir (machine-wide, SYSTEM-readable) and reducing the
/// full name to `Name_PublisherHash`. This READS the authoritative PFN — never compute the hash.
///
/// **Readable by the host, NOT by the plugin runner.** Measured on 2026-08-06: that directory is
/// `UnauthorizedAccessException` for `NT AUTHORITY\LocalService` (which the runner is), while the
/// host service runs as LocalSystem and enumerates all 348 entries. That asymmetry is the entire
/// reason the `xbox` launch kind exists — a library plugin sends the package Identity it CAN read
/// out of `MicrosoftGame.config`, and this resolves the rest at launch time.
///
/// It lives here rather than beside a scanner because it is **launch** vocabulary: the in-host Xbox
/// scanner that used to share it was removed with the rest of the built-ins, and the plugin that
/// replaced it depends on exactly this resolution step.
#[cfg(windows)]
fn xbox_pfn(identity: &str) -> Option<String> {
    let pkgs = std::path::PathBuf::from(std::env::var_os("ProgramData")?)
        .join("Microsoft")
        .join("Windows")
        .join("AppRepository")
        .join("Packages");
    let prefix = format!("{identity}_");
    for e in std::fs::read_dir(&pkgs).ok()?.flatten() {
        let dn = e.file_name().to_string_lossy().into_owned();
        if dn.starts_with(&prefix) {
            if let Some(pfn) = pfn_from_full(&dn, identity) {
                return Some(pfn);
            }
        }
    }
    None
}

/// PackageFamilyName from a PackageFullName dir name
/// (`Name_Version_Arch_ResourceId_PublisherHash`) → `Name_PublisherHash`. The hash is the last
/// `_`-segment; `Name` is the caller's identity.
#[cfg(windows)]
fn pfn_from_full(dir_name: &str, identity: &str) -> Option<String> {
    let hash = dir_name.rsplit('_').next()?;
    (!hash.is_empty() && hash != dir_name).then(|| format!("{identity}_{hash}"))
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

/// The `steam_ui` launch values (D4) — which Steam UI a launcher entry opens. A closed two-value
/// enum, validated on the way IN (the reconcile payload) as well as on the way out, so an entry can
/// never carry a third value that silently resolves to nothing at launch time.
pub(crate) fn valid_steam_ui(value: &str) -> bool {
    matches!(value, "bigpicture" | "desktop")
}

/// One half of an AUMID (a package family name or an app id): non-empty, and no character that
/// could break out of the `shell:AppsFolder\…` argument. Both halves are host-derived, so this is
/// belt-and-braces — but the `xbox` kind now takes an Identity straight off a plugin's wire, which
/// makes it load-bearing rather than defensive.
pub(crate) fn aumid_part(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A full `<PFN>!<AppId>` AUMID.
pub(crate) fn valid_aumid(value: &str) -> bool {
    value
        .split_once('!')
        .is_some_and(|(pfn, app)| aumid_part(pfn) && aumid_part(app))
}

/// A Playnite game id: the GUID Playnite's own database uses, and the only client-influenced part
/// of a `playnite` launch. Interpolated into a URI handed to explorer.exe, so the charset is
/// validated first — 8-4-4-4-12 lowercase-or-uppercase hex with dashes, nothing else.
pub(crate) fn valid_playnite_id(value: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = value.split('-');
    for want in groups {
        match parts.next() {
            Some(p) if p.len() == want && p.bytes().all(|b| b.is_ascii_hexdigit()) => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// The launcher UIs **this host** can open, as `launcher_ui` values (D4).
///
/// One kind for every launcher but Steam, rather than one kind each. A value names a launcher *UI*,
/// which for most of them is the same thing as naming the launcher — and where it is not, the value
/// says which one: `heroic` opens Heroic's window, `heroic-console` its couch front end, and on
/// Windows `playnite` has always meant Playnite's **Fullscreen** app rather than its desktop one.
/// Steam keeps its own [`valid_steam_ui`] kind because it was the first launcher with two UIs worth
/// opening, and because both of its values are the same length of word — splitting Heroic's two out
/// into a `heroic_ui` kind today would buy symmetry and cost every N-1 host: an unknown *kind*
/// degrades to an unlaunchable tile, but an unknown *value* is a hard 400 that refuses the whole
/// reconcile, so a new value is the shape that has to be gated on `minHost` in the plugin index
/// either way.
///
/// Platform-gated, because a value naming a launcher this OS cannot run is not a tile that merely
/// looks odd — it is one that fails at launch. Validated inbound too, so a plugin gets a 400 it can
/// act on instead of publishing a dead entry.
///
/// **Why a typed kind at all**, when design D4 originally said non-Steam launchers would ride the
/// `command` kind: the 2026-08-05 review made `launch.kind = "command"` operator-only (it is handed
/// to a shell), so a plugin publishing one is refused. A typed kind keeps D1's rule intact — the
/// plugin supplies a validated *value*, the host builds the command — and is the only way a scanner
/// plugin can offer a launcher tile at all.
fn launcher_ui_stores() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        &["heroic", "heroic-console", "lutris"]
    }
    // Playnite's activation is verified (2026-08-06, on the .173 box); Epic, GOG Galaxy and the
    // Xbox app are still unwired — each needs its own verified activation, and an unverified guess
    // would ship a tile that does nothing.
    #[cfg(windows)]
    {
        &["playnite"]
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        &[]
    }
}

/// Is `value` a launcher this host's platform knows about at all?
///
/// The *vocabulary* half of the old `valid_launcher_ui`. A value outside this set is a plugin
/// author's mistake — a typo, or a launcher this OS has no support for — and no amount of
/// installing things on the box will make it resolve, so the reconcile refuses the payload.
pub(crate) fn known_launcher_ui(value: &str) -> bool {
    launcher_ui_stores().contains(&value)
}

/// Can this host open `value`'s launcher **right now**?
///
/// The *environment* half. Deliberately separate from [`known_launcher_ui`], because the two
/// failures are not the same kind of thing and must not get the same answer:
///
/// - an unknown value is a bug in the plugin, and a 400 is the only way its author finds out;
/// - a known value that will not resolve means the launcher simply is not installed here, which is
///   an ordinary fact about the box, not a defect in the payload.
///
/// Conflating them cost a real library: the Playnite plugin publishes one launcher tile alongside
/// every game, so a host that could not resolve Playnite 400'd the whole reconcile and the operator
/// got **no games at all** — the same shape as the unservable-cover bug that
/// [`super::sanitize_art_paths`] was introduced to fix. The tile is dropped now (see
/// [`super::sanitize_launcher_entries`]) and the games sync.
pub(crate) fn resolvable_launcher_ui(value: &str) -> bool {
    if !known_launcher_ui(value) {
        return false;
    }
    #[cfg(windows)]
    if value == "playnite" {
        return playnite_fullscreen_exe().is_some();
    }
    // Same question for both Heroic tiles, and the same answer: they resolve to whatever
    // `heroic_launch_prefix` finds, so when that finds nothing the tile is dead and must not be
    // published. Keeping `~/.config/heroic` around after uninstalling Heroic is enough to reach
    // this — the plugin's `detect` only looks for that directory.
    #[cfg(target_os = "linux")]
    if matches!(value, "heroic" | "heroic-console") {
        return heroic_launch_prefix().is_some();
    }
    true
}

/// Windows: Playnite's **Fullscreen** app, if this host can find it.
///
/// Fullscreen rather than Desktop for two reasons: a launcher tile is opened from a couch over a
/// stream, and — verified on 2026-08-06 — the registered `playnite://` protocol handler points at
/// `Playnite.DesktopApp.exe`, so a URI cannot open fullscreen mode at all. The exe is launched
/// directly, which is also why nothing here is interpolated from the entry: the whole value is the
/// literal `"playnite"`.
///
/// `None` when nothing resolves, which is what drops the tile.
#[cfg(windows)]
fn playnite_fullscreen_exe() -> Option<std::path::PathBuf> {
    const EXE: &str = "Playnite.FullscreenApp.exe";
    playnite_install_dirs()
        .into_iter()
        .map(|dir| dir.join(EXE))
        .find(|p| p.is_file())
}

/// Windows: every directory that might hold a Playnite install, best candidates first.
///
/// **Playnite installs per-user by default, and this host is a LocalSystem service** — which
/// invalidates all three of the obvious lookups, and is why this is not a two-liner:
///
/// - `HKEY_CURRENT_USER` is *SYSTEM's own* hive (`S-1-5-18`), never the person's, so a per-user
///   install is invisible there. Every **loaded** hive under `HKEY_USERS` is read instead: only
///   logged-on users' hives are loaded, which is exactly the set that can be streaming, and it
///   avoids a `WTSQueryUserToken` dance for what is a best-effort probe. Same trade-off
///   [`crate::procscan::steam_running_hint`] makes, for the same reason.
/// - The uninstall subkey is matched by its **`DisplayName`**, not by key name. Playnite ships an
///   Inno Setup installer and Inno registers `<AppId>_is1` — measured on a Windows box where Git
///   and Inno itself appear as `Git_is1` and `Inno Setup 6_is1`. The hardcoded
///   `…\Uninstall\Playnite` this replaced matched nothing on any box.
/// - `%LOCALAPPDATA%` for a SYSTEM service is `C:\Windows\System32\config\systemprofile\AppData\
///   Local`, so the default-install fallback cannot trust the variable — it enumerates the profiles
///   under the users base instead, the same breadth [`super::art::art_roots`] already allows.
///
/// A **portable** Playnite is none of those: it is unzipped wherever the operator wanted it
/// (`D:\Apps\Playnite`), registers no uninstall entry, and is not under any profile. Its one
/// registry trace is the `playnite://` handler Playnite registers for itself
/// ([`playnite_dir_from_uri_handler`]) — the same registration this host's own launch path follows.
///
/// Order matters only as a preference: a registry `InstallLocation` is what the installer actually
/// did, so it is consulted before the conventional path. Every candidate is probed for the exe, so
/// a stale entry costs one `is_file` and nothing else.
#[cfg(windows)]
fn playnite_install_dirs() -> Vec<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    // 64-bit and 32-bit views. HKCU/HKU `Software` is not redirected (only `Software\Classes` is),
    // so the WOW view is a machine-hive concern only.
    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    // Playnite's own `playnite://` registration, in both spellings: bare inside a `…_Classes` hive,
    // and via the `Software\Classes` link everywhere else.
    const URI_COMMAND: &str = r"playnite\shell\open\command";
    const CLASSES_URI_COMMAND: &str = r"Software\Classes\playnite\shell\open\command";

    let mut dirs: Vec<std::path::PathBuf> = Vec::new();

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    playnite_dirs_from_uninstall(&hklm, UNINSTALL, &mut dirs);
    playnite_dirs_from_uninstall(&hklm, UNINSTALL_WOW, &mut dirs);
    playnite_dir_from_uri_handler(&hklm, CLASSES_URI_COMMAND, &mut dirs);

    let users = RegKey::predef(HKEY_USERS);
    for sid in users.enum_keys().flatten() {
        let Ok(hive) = users.open_subkey_with_flags(&sid, KEY_READ) else {
            continue;
        };
        // The `…_Classes` companion hives carry file associations — which is exactly where the
        // `playnite://` handler lives, `HKCU\Software\Classes` BEING that hive — and never uninstall
        // entries. Both spellings are probed rather than reasoned about: the in-hive `Software\Classes`
        // link is a link, and a probe that misses costs one failed `open_subkey`.
        if sid.ends_with("_Classes") {
            playnite_dir_from_uri_handler(&hive, URI_COMMAND, &mut dirs);
            continue;
        }
        playnite_dirs_from_uninstall(&hive, UNINSTALL, &mut dirs);
        playnite_dir_from_uri_handler(&hive, CLASSES_URI_COMMAND, &mut dirs);
    }

    // The conventional per-user location, for every profile on the box — this is where Playnite's
    // own default install lands, and it covers a user whose hive is not currently loaded.
    for profile in windows_user_profiles() {
        push_unique(&mut dirs, profile.join(r"AppData\Local\Playnite"));
    }
    dirs
}

/// Collect `InstallLocation` from every Playnite-looking uninstall entry under `root\path`.
///
/// Matched on `DisplayName` because the key name is the installer's `AppId` (see
/// [`playnite_install_dirs`]). `starts_with` rather than equality so a versioned or suffixed display
/// name still counts; the value is only ever used as a directory to probe for the exe, so a false
/// positive costs one failed `is_file`.
#[cfg(windows)]
fn playnite_dirs_from_uninstall(
    root: &winreg::RegKey,
    path: &str,
    out: &mut Vec<std::path::PathBuf>,
) {
    use winreg::enums::KEY_READ;

    let Ok(uninstall) = root.open_subkey_with_flags(path, KEY_READ) else {
        return;
    };
    for name in uninstall.enum_keys().flatten() {
        let Ok(entry) = uninstall.open_subkey_with_flags(&name, KEY_READ) else {
            continue;
        };
        let display: String = entry.get_value("DisplayName").unwrap_or_default();
        if !display.starts_with("Playnite") {
            continue;
        }
        if let Ok(location) = entry.get_value::<String, _>("InstallLocation") {
            let location = location.trim();
            if !location.is_empty() {
                push_unique(out, std::path::PathBuf::from(location));
            }
        }
    }
}

/// Take the directory of Playnite's registered `playnite://` handler from `root\path`, if there is one.
///
/// This is what finds a **portable** Playnite. It leaves no uninstall entry and lives under no user
/// profile, so every other probe here is blind to it — but Playnite registers its own URI scheme,
/// and that registration is the very one `explorer.exe "playnite://…"` follows when this host starts
/// a Playnite title. If it resolves, this box already opens games with that copy.
#[cfg(windows)]
fn playnite_dir_from_uri_handler(
    root: &winreg::RegKey,
    path: &str,
    out: &mut Vec<std::path::PathBuf>,
) {
    use winreg::enums::KEY_READ;

    let Ok(command) = root
        .open_subkey_with_flags(path, KEY_READ)
        .and_then(|k| k.get_value::<String, _>(""))
    else {
        return;
    };
    if let Some(dir) = exe_from_shell_command(&command)
        .map(std::path::Path::new)
        .and_then(std::path::Path::parent)
        .filter(|d| !d.as_os_str().is_empty())
    {
        push_unique(out, dir.to_path_buf());
    }
}

/// The executable out of a registered shell-open command line:
/// `"D:\Apps\Playnite\Playnite.DesktopApp.exe" --uridata "%1"` → `D:\Apps\Playnite\Playnite.DesktopApp.exe`.
///
/// Quoted form first, because that is what a registrar writes. The cut at the first `.exe` is the
/// fallback for the unquoted spelling, whose path may itself contain spaces and so cannot be split on
/// whitespace. `None` when neither shape matches; the result is only ever a directory to probe for an
/// exe, so a miss costs one `is_file` and nothing else.
#[cfg_attr(not(windows), allow(dead_code))]
fn exe_from_shell_command(command: &str) -> Option<&str> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().filter(|p| !p.is_empty());
    }
    let end = command.to_ascii_lowercase().find(".exe")? + ".exe".len();
    Some(&command[..end])
}

/// Windows: every Playnite root on this box, as an **art** root.
///
/// A portable Playnite keeps its library beside the exe — covers land in
/// `<PlayniteDir>\library\files\…` — so for that layout the install dir IS where the art lives, and
/// the users base can never cover it: the whole point of portable is that it sits wherever the
/// operator put it (`D:\Apps\Playnite` in the report that prompted this). Without it a portable
/// install synced its games and had EVERY cover dropped by the confinement. An installed Playnite
/// keeps the same tree under `%APPDATA%\Playnite`, already inside the users base; naming that
/// directory twice costs one `canonicalize` in [`super::art::art_path_is_confined`].
///
/// Same shape and same reasoning as [`super::art::steam_art_roots`], and it does not widen what the
/// host can be *tricked* into reading: every candidate comes from the host's own registry and
/// filesystem probes, never from the plugin lane that supplies the art path, and the extension,
/// regular-file, magic-byte and config-dir gates all still apply on top.
///
/// The per-user hives these candidates partly come from are writable by that user — which is a bar
/// this host already stands on, and one rung lower here than where it already stood: the same
/// lookup picks the `Playnite.FullscreenApp.exe` a launcher tile SPAWNS. Trusting it to name a
/// directory whose image files may be read is strictly weaker than trusting it to name a program to
/// run.
#[cfg(windows)]
pub(crate) fn playnite_art_roots() -> Vec<std::path::PathBuf> {
    playnite_install_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect()
}

/// Every user profile directory on the box (`C:\Users\*`), minus the shared `Public` pseudo-profile.
///
/// `%PUBLIC%`'s parent is the users base on every supported Windows — the same derivation
/// [`super::art::art_roots`] uses — with `%SystemDrive%\Users` as the fallback when the variable is
/// missing from a service's environment.
#[cfg(windows)]
fn windows_user_profiles() -> Vec<std::path::PathBuf> {
    let base = std::env::var_os("PUBLIC")
        .map(std::path::PathBuf::from)
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .or_else(|| {
            std::env::var_os("SystemDrive").map(|d| std::path::PathBuf::from(d).join("Users"))
        });
    let Some(base) = base else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && !p.ends_with("Public"))
        .collect()
}

/// Push `path` unless an equal one is already there — the candidate lists are a handful of entries,
/// so a linear check beats carrying a set around.
#[cfg(windows)]
fn push_unique(out: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
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

/// Map a `gog` LaunchSpec value — the tab-separated `exe \t args \t workdir` spawn triple a GOG
/// library plugin derives from `goggame-<id>.info` — to a `(command line, working dir)`. GOG games
/// are spawned directly (no Galaxy), so the exe is quoted and the arguments ride verbatim.
///
/// The exe (and the working dir) is re-confined here to an install directory GOG's own registry
/// lists ([`gog_install_dirs`]). The plugin already confines it while parsing the manifest
/// (plugin-kit's `confinedJoin`, the port of the in-host scanner's `confined_join`) — but that runs
/// outside this host's trust boundary: the triple arrives over the provider API, and unchecked this
/// kind is an arbitrary exe-plus-arguments primitive for any lane that may publish an entry
/// (security review 2026-08-25). `None` ⇒ no GOG install owns that exe, so the entry has no recipe
/// and the launch fails the way any unresolvable one does.
#[cfg(windows)]
pub(crate) fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    gog_spawn_in(value, &gog_install_dirs())
}

/// The pure core of [`gog_spawn`] (unit-testable without a GOG install): `installs` is the set of
/// directories the exe and the working dir must sit inside.
#[cfg(windows)]
fn gog_spawn_in(value: &str, installs: &[String]) -> Option<(String, Option<PathBuf>)> {
    let under = |p: &str| installs.iter().any(|dir| path_under(dir, p));
    let mut parts = value.split('\t');
    let exe = parts.next().filter(|s| !s.is_empty())?;
    if !under(exe) {
        tracing::warn!(
            exe,
            "gog launch: the exe is in no GOG install — refusing it"
        );
        return None;
    }
    let args = parts.next().unwrap_or("");
    // An out-of-bounds working dir is dropped rather than refused: it only decides where the
    // (confined) exe starts, and an entry that omits it already spawns without one.
    let workdir = parts.next().filter(|s| under(s)).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
}

/// Windows: every directory GOG's own registry names as an installed game's root
/// (`HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\<productId>\PATH` — GOG is 32-bit, so the WOW view is
/// where it writes). This is the host's OWN enumeration of the store, which is the whole point: it
/// is what a `gog` launch value is checked against, and it is the same key the in-host scanner read
/// before the store moved to a plugin. Empty when GOG isn't installed, which refuses every `gog`
/// launch on that box.
#[cfg(windows)]
fn gog_install_dirs() -> Vec<String> {
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    let Ok(games) =
        RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SOFTWARE\WOW6432Node\GOG.com\Games")
    else {
        return Vec::new();
    };
    games
        .enum_keys()
        .flatten()
        .filter_map(|sub| {
            let path: String = games.open_subkey(&sub).ok()?.get_value("PATH").ok()?;
            let path = path.trim().to_string();
            (!path.is_empty()).then_some(path)
        })
        .collect()
}

/// Whether `path` is `dir` itself or something inside it, compared the way Windows compares paths:
/// case-insensitively, either separator, and with `..` refused outright (it would climb straight
/// back out of the directory the prefix test just accepted — the component the in-host scanner's
/// `confined_join` refused for the same reason). A string test rather than [`Path::starts_with`],
/// which compares components case-SENSITIVELY: the install path a plugin sends need not be spelled
/// the way the registry spells it.
#[cfg(windows)]
fn path_under(dir: &str, path: &str) -> bool {
    let norm = |s: &str| s.replace('/', "\\").trim_end_matches('\\').to_lowercase();
    let (dir, path) = (norm(dir), norm(path));
    if dir.is_empty() || path.split('\\').any(|c| c == "..") {
        return false;
    }
    path == dir
        || path
            .strip_prefix(&dir)
            .is_some_and(|rest| rest.starts_with('\\'))
}

/// Launch a GameStream `apps.json` command (operator-typed, trusted — never client-set) into the
/// interactive Windows user session, AFTER capture is up (the host is SYSTEM). The Linux paths go
/// through the compositor-aware [`launch_session_command`] instead.
#[cfg(windows)]
pub fn launch_gamestream_command(cmd: &str) -> Result<WindowsLaunch> {
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    // cmd.exe /c is fine here: the value is the host operator's own apps.json command, not a
    // client-influenced string (same trust as the custom-store `command` kind).
    let pid = crate::interactive::spawn_in_active_session(&format!("cmd.exe /c {cmd}"), None)
        .context("spawn gamestream command in the interactive session")?;
    tracing::info!(command = %cmd, pid, "gamestream: launched app in the interactive session");
    // `cmd.exe /c` waits for the operator's command, so this pid is the command's own life. Should
    // the command itself be a forwarder that returns at once, the lease's shim window is what reads
    // that as a hand-off rather than as the game exiting.
    Ok(WindowsLaunch {
        pid,
        owns_game: true,
    })
}

/// Launch a library title chosen from the **GameStream `/applist`** (the store-qualified id is carried
/// on the `AppEntry`, resolved from the numeric Moonlight appid) into the interactive Windows user
/// session ([`launch_title`]). The id is resolved against the host's OWN library, so a client can
/// only ever pick an existing title — never inject a command. Linux resolves the id via
/// [`resolve_launch`] and goes through [`launch_session_command`] instead.
#[cfg(windows)]
pub fn launch_gamestream_library(id: &str) -> Result<WindowsLaunch> {
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
/// * **KWin / Mutter** — the host runs inside the user's graphical session (the process env was
///   retargeted at it by `apply_session_env`) and the per-session virtual output is promoted
///   *primary*, so a plain spawn lands the app on the streamed output.
/// * **Hyprland / wlroots (sway)** — those two are EXTEND-only: the streamed head is added *beside*
///   the operator's and nothing promotes it, so a plain spawn lands the app on whichever monitor
///   holds focus — the operator's physical one. This is the case that was reported from the field as
///   "anything from the library opens on my main display instead of the virtual screen". The spawn is
///   therefore preceded by [`crate::vdisplay::focus_streamed_output`], which claims focus for the
///   streamed head so the window Hyprland/sway is about to map goes there. (The backends also focus
///   it at capture bring-up; re-asserting here is what covers the gap between the two — portal
///   handshake, encoder build and first frame all sit in between, and anything that touches focus in
///   that window would otherwise silently put the launch back on a physical head.)
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
    // Claim focus for the streamed head before spawning, so the window the compositor is about to map
    // opens where the client is looking (see the EXTEND note above; a no-op on every other backend).
    // The name comes from the same slot the absolute-input pointer is bound to, so focus and cursor
    // land on one head by construction rather than by two independent guesses.
    if let Some(out) = crate::inject::stream_output() {
        if crate::vdisplay::focus_streamed_output(compositor, &out) {
            tracing::debug!(output = %out, "claimed focus for the streamed head before launching");
        }
    }
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

    /// The `steam_ui` launcher kind (D4): a closed two-value enum, mapped to the Steam client's own
    /// UI on each OS. Nothing from the entry is interpolated — the value only SELECTS between two
    /// host-owned literals — so there is no injection surface at all here.
    #[test]
    fn steam_ui_is_a_closed_two_value_enum() {
        assert!(valid_steam_ui("bigpicture"));
        assert!(valid_steam_ui("desktop"));
        assert!(!valid_steam_ui("gamepadui"));
        assert!(!valid_steam_ui(""));
        assert!(!valid_steam_ui("bigpicture; rm -rf ~"));
    }

    /// The `launcher_ui` kind exists because D4's original plan — non-Steam launchers riding the
    /// `command` kind — stopped being available to plugins when the 2026-08-05 review made
    /// `command` operator-only. A plugin names a launcher; the host builds the command.
    #[test]
    fn launcher_ui_accepts_only_launchers_this_host_can_open() {
        #[cfg(target_os = "linux")]
        {
            assert!(known_launcher_ui("heroic"));
            assert!(known_launcher_ui("heroic-console"));
            assert!(known_launcher_ui("lutris"));
            // Not wired on this OS — outside the vocabulary, so it is refused inbound rather than
            // becoming a tile that does nothing.
            assert!(!known_launcher_ui("gog"));
            // Both Heroic tiles resolve through the same probe, so a box without Heroic drops both
            // rather than publishing one dead tile beside the other.
            assert_eq!(
                resolvable_launcher_ui("heroic"),
                heroic_launch_prefix().is_some()
            );
            assert_eq!(
                resolvable_launcher_ui("heroic-console"),
                heroic_launch_prefix().is_some()
            );
        }
        #[cfg(windows)]
        {
            // Playnite is in the vocabulary unconditionally — whether this particular box has it
            // installed is a separate question, answered by `resolvable_launcher_ui` below. Keeping
            // them separate is the fix for the reconcile that 400'd a whole library over one tile.
            assert!(known_launcher_ui("playnite"));
            assert_eq!(
                resolvable_launcher_ui("playnite"),
                playnite_fullscreen_exe().is_some()
            );
            // The Linux launchers, and the Windows ones whose activation is still unverified
            // (Epic, GOG Galaxy, the Xbox app), stay refused.
            assert!(!known_launcher_ui("heroic"));
            assert!(!known_launcher_ui("gog"));
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            // No launcher UIs are wired on this OS, so every value is refused.
            assert!(!known_launcher_ui("heroic"));
            assert!(!known_launcher_ui("gog"));
        }
        // Junk is outside the vocabulary on every OS, so it never reaches a resolver.
        assert!(!known_launcher_ui(""));
        assert!(!known_launcher_ui("lutris; rm -rf ~"));
        assert!(!resolvable_launcher_ui(""));
        assert!(!resolvable_launcher_ui("lutris; rm -rf ~"));
    }

    /// The `xbox` kind is what a library PLUGIN can publish: the runner's principal cannot read
    /// AppRepository (measured 2026-08-06), so it sends `<Identity>!<AppId>` and the host resolves
    /// the publisher hash. The charset guard is load-bearing here — unlike `aumid`, this value
    /// arrives over the wire.
    #[test]
    fn xbox_value_is_identity_bang_appid_and_charset_guarded() {
        assert!(valid_aumid("Microsoft.Foo!Game"));
        assert!(valid_aumid("A_b-c.d!App"));
        // Both halves must be present and non-empty.
        assert!(!valid_aumid("Microsoft.Foo"));
        assert!(!valid_aumid("!Game"));
        assert!(!valid_aumid("Microsoft.Foo!"));
        assert!(!valid_aumid(""));
        // Nothing that could break out of the `shell:AppsFolder\…` argument.
        assert!(!valid_aumid("Foo\"!Game"));
        assert!(!valid_aumid("Foo!Game\" & calc"));
        assert!(!valid_aumid("Foo\\..\\Bar!Game"));
        assert!(!valid_aumid("Foo Bar!Game"));
    }

    /// The portable-Playnite probe, at the only part of it that can be wrong off-Windows: pulling the
    /// exe out of the registered `playnite://` command line. A miss here is a portable install the
    /// host cannot find — no launcher tile, and (through [`playnite_art_roots`]) every cover dropped.
    #[test]
    fn exe_is_read_out_of_a_registered_shell_command() {
        // What Playnite actually registers, portable install on a second drive.
        assert_eq!(
            exe_from_shell_command(r#""D:\Apps\Playnite\Playnite.DesktopApp.exe" --uridata "%1""#),
            Some(r"D:\Apps\Playnite\Playnite.DesktopApp.exe")
        );
        // Unquoted, with a space in the path — which is why this cannot split on whitespace.
        assert_eq!(
            exe_from_shell_command(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe %1"),
            Some(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe")
        );
        // Case is the registrar's business, not ours.
        assert_eq!(
            exe_from_shell_command(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE"),
            Some(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE")
        );
        // Nothing exe-shaped, and the empty quoted form: no candidate beats a bogus one, because a
        // bogus one would become an allowed art root.
        assert_eq!(
            exe_from_shell_command("rundll32 shell32.dll,Control_RunDLL"),
            None
        );
        assert_eq!(exe_from_shell_command(r#""" %1"#), None);
        assert_eq!(exe_from_shell_command(""), None);
    }

    /// Windows' launcher tile opens Playnite's FULLSCREEN app. Both negatives are the point: the
    /// desktop app is not what a couch tile should open, and the `playnite://` handler cannot be
    /// used because it is registered to the desktop app (verified on .173, 2026-08-06).
    #[cfg(windows)]
    #[test]
    fn playnite_launcher_opens_the_fullscreen_app() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
            })
        };
        // A launcher this host cannot open is refused, whatever the OS.
        assert!(ui("gog").is_none());
        assert!(ui("heroic").is_none());
        assert!(ui("").is_none());

        // The rest only means anything on a box that actually has Playnite.
        let Some(exe) = playnite_fullscreen_exe() else {
            return;
        };
        let r = ui("playnite").expect("resolvable when the exe was found");
        let cmd = &r.cmdline;
        assert!(cmd.contains("Playnite.FullscreenApp.exe"), "{cmd}");
        assert!(!cmd.contains("DesktopApp"), "{cmd}");
        assert!(!cmd.contains("playnite://"), "{cmd}");
        assert_eq!(r.workdir.as_deref(), exe.parent());
        assert!(r.owns_game, "the exe is spawned directly, not forwarded");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_ui_opens_the_launcher_itself() {
        let ui = |v: &str| {
            command_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
            })
        };
        // Bare `lutris` opens the window; the URI form is the `lutris_id` kind and launches a game.
        assert_eq!(ui("lutris").as_deref(), Some("lutris"));
        assert!(!ui("lutris").unwrap().contains("rungameid"));
        // Heroic resolves the same way its game launches do, but with no `--no-gui` and no URI — so
        // the window IS what opens. `None` on a box without Heroic, which is a correct answer.
        if let Some(cmd) = ui("heroic") {
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
            assert!(!cmd.contains("heroic://"), "no game URI: {cmd:?}");
            assert!(
                !cmd.contains("--console"),
                "that is the other tile: {cmd:?}"
            );
        }
        // Console mode needs BOTH flags — `--console` alone routes the UI without filling the
        // screen, which from a couch is the bug this tile exists to avoid. Same prefix as the
        // window tile, so it is `None` on the same boxes.
        assert_eq!(ui("heroic-console").is_some(), ui("heroic").is_some());
        if let Some(cmd) = ui("heroic-console") {
            assert!(cmd.contains("--console"), "{cmd:?}");
            assert!(cmd.contains("--fullscreen"), "{cmd:?}");
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
            // Gamescope spawns by `split_whitespace`, so every token has to stand alone.
            assert!(cmd.split_whitespace().any(|t| t == "--console"), "{cmd:?}");
        }
        assert_eq!(ui("nonsense"), None);
        assert_eq!(ui(""), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn steam_ui_resolves_to_the_client_ui_on_linux() {
        let ui = |v: &str| {
            command_for(&LaunchSpec {
                kind: "steam_ui".into(),
                value: v.into(),
            })
        };
        // Big Picture is the SteamOS game-mode shape; nested in gamescope this is what `--steam`
        // integration is built around.
        assert_eq!(ui("bigpicture").as_deref(), Some("steam -gamepadui"));
        assert_eq!(ui("desktop").as_deref(), Some("steam"));
        assert_eq!(ui("nonsense"), None);
        assert_eq!(ui(""), None);
    }

    #[cfg(windows)]
    #[test]
    fn steam_ui_resolves_to_the_client_ui_on_windows() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "steam_ui".into(),
                value: v.into(),
            })
        };
        let bp = ui("bigpicture").expect("bigpicture recipe");
        assert!(
            bp.cmdline.contains("steam://open/bigpicture"),
            "line was {:?}",
            bp.cmdline
        );
        assert!(bp.workdir.is_none());
        assert!(!bp.owns_game, "a steam:// URI is forwarded to the client");
        let desk = ui("desktop").expect("desktop recipe");
        assert!(
            desk.cmdline.contains("steam://open/main"),
            "line was {:?}",
            desk.cmdline
        );
        assert!(ui("nonsense").is_none());
        assert!(ui("").is_none());
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
        let installs = ["C:\\Games\\W3".to_string(), "C:\\".to_string()];
        let spawn = |v: &str| gog_spawn_in(v, &installs);
        let (cmd, wd) = spawn("C:\\Games\\W3\\witcher3.exe\t--skip\tC:\\Games\\W3").unwrap();
        assert_eq!(cmd, "\"C:\\Games\\W3\\witcher3.exe\" --skip");
        assert_eq!(wd, Some(std::path::PathBuf::from("C:\\Games\\W3")));
        let (cmd2, wd2) = spawn("C:\\g.exe").unwrap();
        assert_eq!(cmd2, "\"C:\\g.exe\"");
        assert!(wd2.is_none());
        assert!(spawn("").is_none());
    }

    /// The `gog` kind is an exe PLUS ARGUMENTS, and the triple reaches the host over the provider
    /// API — so the exe has to be one the host itself found, not one the caller named (security
    /// review 2026-08-25). Without this the kind is `cmd.exe /c <anything>` by another spelling.
    #[cfg(windows)]
    #[test]
    fn gog_spawn_refuses_an_exe_outside_every_gog_install() {
        let installs = ["C:\\Games\\W3".to_string()];
        let spawn = |v: &str| gog_spawn_in(v, &installs);
        assert!(spawn("C:\\Windows\\System32\\cmd.exe\t/c calc\tC:\\Games\\W3").is_none());
        // A sibling whose name merely starts with the install path is not inside it.
        assert!(spawn("C:\\Games\\W3x\\evil.exe").is_none());
        // ...nor is anything that climbs back out of one.
        assert!(spawn("C:\\Games\\W3\\..\\..\\Windows\\System32\\cmd.exe").is_none());
        // No GOG install at all ⇒ nothing is launchable, rather than everything.
        assert!(gog_spawn_in("C:\\Games\\W3\\witcher3.exe", &[]).is_none());
        // Windows paths are case-insensitive and take either separator, so a plugin that spells the
        // install dir differently to the registry still launches its own games.
        assert!(spawn("c:/games/w3/bin/game.exe").is_some());
        // An out-of-bounds working dir costs the working dir, not the launch.
        let (_, wd) = spawn("C:\\Games\\W3\\witcher3.exe\t\tC:\\Windows\\System32").unwrap();
        assert!(wd.is_none());
    }

    /// Moved here with `xbox_pfn` when the built-in scanners were removed: reducing a
    /// PackageFullName to its family name is what the `xbox` launch kind does with the Identity a
    /// de-privileged plugin sends it, so the guard belongs to the launch path now.
    #[cfg(windows)]
    #[test]
    fn pfn_reduces_a_package_full_name_to_its_family() {
        assert_eq!(
            pfn_from_full(
                "Microsoft.624F8B84B80_1.0.0.0_x64__8wekyb3d8bbwe",
                "Microsoft.624F8B84B80"
            )
            .as_deref(),
            Some("Microsoft.624F8B84B80_8wekyb3d8bbwe")
        );
        // No `_` at all → nothing to reduce, and we must not invent a hash.
        assert!(pfn_from_full("NoUnderscore", "NoUnderscore").is_none());
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
        let steam_r = windows_launch_for(&steam).expect("steam recipe");
        let line = &steam_r.cmdline;
        assert!(line.contains("steam://rungameid/570"), "line was {line:?}");
        assert!(steam_r.workdir.is_none());
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
        let cmd_r = windows_launch_for(&cmd).unwrap();
        assert_eq!(cmd_r.cmdline, "cmd.exe /c notepad.exe");
        assert!(
            cmd_r.owns_game,
            "`cmd /c` blocks on the operator's command, so its pid is that command's"
        );
        // Xbox AUMID → explorer shell:AppsFolder activation; a value without '!' is rejected.
        let aumid = LaunchSpec {
            kind: "aumid".into(),
            value: "Microsoft.X_8wekyb3d8bbwe!Game".into(),
        };
        assert_eq!(
            windows_launch_for(&aumid).unwrap().cmdline,
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
