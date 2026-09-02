//! Resolve a library id or operator command into the per-OS line the host runs.
//!
//! Scanners enumerate only. This module owns the `kind` vocabulary, charset
//! validators, and resolvers so a plugin lift does not take launch with it.
//! A client sends an entry id; the host holds the [`LaunchSpec`]
//! (design/library-scanner-plugins.md D1).
//!
//! Linux: the host runs the resolved shell command (nested gamescope or live
//! session). Windows: [`launch_title`] uses the signed-in user of the host's
//! WTS session.

use super::*;

/// Library enumeration hits every store's on-disk metadata, so handshake
/// resolves this once and threads it through the data plane.
pub struct LaunchTarget {
    pub game: crate::gamelease::GameRef,
    /// Launcher tile (design D4): no game-exit to detect; the lease stays untracked.
    /// See [`crate::gamelease::LeaseRequest::launcher`].
    pub launcher: bool,
    pub detect: DetectSpec,
    /// Linux: the host-run shell command. Windows: always `None`; spawn is by library id.
    pub command: Option<String>,
}

/// Map a store-qualified library id to a [`LaunchTarget`] from the host's library.
/// `None` = unknown id, or on Linux a title with no runnable recipe.
///
/// Shared by both planes. Linux runs the command (nested gamescope or live
/// session). Windows has no nest: [`launch_title`] resolves the process later.
pub fn resolve_launch(id: &str) -> Option<LaunchTarget> {
    let entry = all_games().into_iter().find(|g| g.id == id)?;
    let game = crate::gamelease::GameRef {
        id: Some(entry.id.clone()),
        store: Some(entry.store.clone()),
        title: entry.title.clone(),
    };
    #[cfg(not(windows))]
    {
        // No command ⇒ nothing to launch; same `None` as an unknown id.
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
        // Recipe is resolved in `launch_title`; a missing one still yields a target here.
        Some(LaunchTarget {
            game,
            launcher: entry.role == GameRole::Launcher,
            detect: entry.detect,
            command: None,
        })
    }
}

/// Recipe for a `plugin`-kind entry from the plugin that owns it. `None` for
/// every other kind, with no I/O, so both OS resolvers try this first.
///
/// Lives here rather than in `command_for` / `windows_launch_for` because it
/// needs `provider` (stamped from `PUT /library/provider/{provider}`). A plant
/// under someone else's provider 404s at that other plugin, not a launch.
///
/// Blocking: [`ask_plugin_launch`]. Async callers use `spawn_blocking`; the
/// handshake probe is [`launch_is_resolvable`], which never asks.
fn plugin_recipe(entry: &GameEntry) -> Option<PluginLaunch> {
    let spec = entry.launch.as_ref()?;
    if spec.kind != "plugin" {
        return None;
    }
    let Some(provider) = entry.provider.as_deref() else {
        // Unreachable unless library.json was hand-edited; warn rather than silent None.
        tracing::warn!(
            id = %entry.id,
            "plugin launch: entry carries no provider, so no plugin can answer for it"
        );
        return None;
    };
    ask_plugin_launch(provider, &spec.value)
}

/// Cheap "will this launch" bit for handshake routing. Async path: never asks
/// a plugin. For `plugin` kind, a live provider plus a well-formed key is
/// enough; a later refuse fails like any other unresolvable entry.
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

/// Pure map from [`LaunchSpec`] to a shell command. `plugin` is absent: that
/// answer is another process, resolved by [`plugin_recipe`] first.
#[cfg(not(windows))]
fn command_for(spec: &LaunchSpec) -> Option<String> {
    match spec.kind.as_str() {
        "steam_appid" => valid_steam_appid(&spec.value)
            .then(|| format!("steam steam://rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "lutris_id" => (!spec.value.is_empty() && spec.value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| format!("lutris lutris:rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "heroic" => heroic_command(&spec.value),
        // Steam client UI (design D4). Nested in gamescope this is SteamOS game-mode.
        "steam_ui" => match spec.value.as_str() {
            "bigpicture" => Some("steam -gamepadui".into()),
            "desktop" => Some("steam".into()),
            _ => None,
        },
        // Other launcher UIs (design D4). Host builds the command; the plugin names the launcher.
        #[cfg(target_os = "linux")]
        "launcher_ui" => match spec.value.as_str() {
            // Same prefix as a game launch, minus `--no-gui` and URI: the window is the tile.
            "heroic" => heroic_launch_prefix(),
            // Both flags: `--console` routes the UI; `--fullscreen` fills the screen.
            // `heroic://` has only ping/launch, so a URI cannot open console mode.
            // Heroic < 2.21.0 ignores `--console` and still honours `--fullscreen`.
            "heroic-console" => {
                heroic_launch_prefix().map(|p| format!("{p} --console --fullscreen"))
            }
            // Bare `lutris` opens the window; a `lutris:rungameid/…` URI would launch a game.
            "lutris" => Some("lutris".into()),
            _ => None,
        },
        "command" => (!spec.value.trim().is_empty()).then(|| spec.value.clone()),
        _ => None,
    }
}

/// Command line, working dir, and whether the started process **is** the game.
///
/// Almost every Windows recipe is a protocol hand-off (`steam://`, `playnite://`)
/// whose pid is a forwarder's: already-running launcher → pid dies while the
/// game still loads; cold launcher → pid becomes the launcher and never exits.
/// Only a direct game (or operator) start goes on
/// [`crate::gamelease::LeaseRequest::spawned`]; a hand-off pid is dropped and
/// the lease uses detect signals.
#[cfg(windows)]
pub struct WinRecipe {
    pub cmdline: String,
    pub workdir: Option<std::path::PathBuf>,
    /// `false` for a protocol/launcher hand-off (see type docs).
    pub owns_game: bool,
}

#[cfg(windows)]
impl WinRecipe {
    fn handoff(cmdline: String) -> Self {
        Self {
            cmdline,
            workdir: None,
            owns_game: false,
        }
    }

    fn game(cmdline: String, workdir: Option<std::path::PathBuf>) -> Self {
        Self {
            cmdline,
            workdir,
            owns_game: true,
        }
    }
}

/// Pid from a Windows launch, plus whether it is the game's ([`WinRecipe::owns_game`]).
#[cfg(windows)]
pub struct WindowsLaunch {
    pub pid: u32,
    /// Whether this pid is the game, not a protocol forwarder.
    pub owns_game: bool,
}

#[cfg(windows)]
impl WindowsLaunch {
    /// Lease pid: `None` when the host only started a hand-off.
    pub fn tracked_pid(&self) -> Option<u32> {
        self.owns_game.then_some(self.pid)
    }
}

/// Launches a store-qualified library id as the user of the host's WTS session.
///
/// [`windows_launch_for`] maps the recipe and
/// [`crate::interactive::spawn_as_current_session_user`] starts it after capture
/// is live. The returned PID enters
/// [`crate::gamelease::LeaseRequest::spawned`] only when it owns the game rather
/// than a protocol hand-off.
#[cfg(windows)]
pub fn launch_title(id: &str) -> Result<WindowsLaunch> {
    let entry = all_games()
        .into_iter()
        .find(|g| g.id == id)
        .filter(|g| g.launch.is_some())
        .ok_or_else(|| anyhow::anyhow!("no launchable library entry '{id}'"))?;
    let spec = entry.launch.clone().expect("filtered to Some above");
    // Plugin recipe is the same (cmdline, cwd) shape; `windows_launch_for` has
    // no `plugin` arm, so a failed ask falls through to "no recipe" below.
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
    let pid = crate::interactive::spawn_as_current_session_user(&cmdline, workdir.as_deref())
        .with_context(|| format!("launch '{id}' as the current WTS session user"))?;
    tracing::info!(
        launch_id = id,
        %cmdline,
        pid,
        owns_game,
        "launched library title as the current WTS session user"
    );
    Ok(WindowsLaunch { pid, owns_game })
}

/// Pure map from [`LaunchSpec`] to a spawn recipe. `None` = no Windows recipe.
///
/// CreateProcessAsUserW does no shell or protocol resolution: URI/flags go to
/// a concrete EXE as argv. `plugin` is absent; [`plugin_recipe`] resolves it.
#[cfg(windows)]
fn windows_launch_for(spec: &LaunchSpec) -> Option<WinRecipe> {
    match spec.kind.as_str() {
        "steam_appid" => {
            if !valid_steam_appid(&spec.value) {
                return None;
            }
            let uri = format!("steam://rungameid/{}", spec.value);
            // Steam.exe + URI, else explorer.exe for the steam:// user-hive handler.
            let cmdline = match steam_exe() {
                Some(exe) => format!("\"{}\" \"{uri}\"", exe.display()),
                None => format!("explorer.exe \"{uri}\""),
            };
            // Forwarder either way: running Steam posts the URI and exits; cold Steam *is* the client.
            Some(WinRecipe::handoff(cmdline))
        }
        // Steam client UI (design D4). Same Steam.exe-then-explorer ladder as `steam_appid`.
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
        // explorer.exe + Epic URI: one argv, no shell. Same pattern as the Steam fallback.
        "epic" => epic_launch_uri(&spec.value)
            .map(|uri| WinRecipe::handoff(format!("explorer.exe \"{uri}\""))),
        // Direct exe spawn (not a Galaxy hand-off). `gog_spawn` re-confines the plugin triple.
        "gog" => gog_spawn(&spec.value).map(|(cmdline, workdir)| WinRecipe::game(cmdline, workdir)),
        // shell:AppsFolder AUMID. UWP activation fails as SYSTEM/session-0; spawn uses the user token.
        "aumid" => valid_aumid(&spec.value).then(|| {
            WinRecipe::handoff(format!("explorer.exe \"shell:AppsFolder\\{}\"", spec.value))
        }),
        // Plugin sends `<Identity>!<AppId>` from MicrosoftGame.config. AppRepository
        // is LocalSystem-only (LocalService cannot enumerate it), so the host
        // completes the PFN at launch — a cached hash would stale on package update.
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
        // explorer.exe + playnite:// — Playnite maps the id to the owning store.
        // Typed kind: `command` is operator-only, so a plugin cannot publish one.
        "playnite" => valid_playnite_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"playnite://playnite/start/{}\"",
                spec.value
            ))
        }),
        // Ubisoft Connect and Amazon Games register a protocol handler and nothing else;
        // explorer.exe resolves it as the user, the `epic` shape. Ids are charset-guarded.
        "uplay" => valid_uplay_id(&spec.value).then(|| {
            WinRecipe::handoff(format!("explorer.exe \"uplay://launch/{}/0\"", spec.value))
        }),
        "amazon" => valid_amazon_id(&spec.value).then(|| {
            WinRecipe::handoff(format!(
                "explorer.exe \"amazon-games://play/{}\"",
                spec.value
            ))
        }),
        // `battlenet://<code>` only opens the game's page; `--exec="launch <code>"` on the
        // client's exe starts it. No exe found refuses the launch rather than opening a page.
        "battlenet" => {
            if !valid_battlenet_code(&spec.value) {
                return None;
            }
            let exe = battlenet_exe()?;
            Some(WinRecipe::handoff(format!(
                "\"{}\" --exec=\"launch {}\"",
                exe.display(),
                spec.value
            )))
        }
        // Playnite Fullscreen (design D4). `playnite://` opens the desktop app, so spawn the exe.
        // Workdir is the install dir; a .NET app expects it.
        "launcher_ui" => match spec.value.as_str() {
            "playnite" => playnite_fullscreen_exe().map(|exe| {
                let dir = exe.parent().map(std::path::Path::to_path_buf);
                WinRecipe::game(format!("\"{}\"", exe.display()), dir)
            }),
            _ => None,
        },
        // Operator command. `cmd.exe /c` blocks until it returns, so the pid is that command's life.
        "command" => {
            let v = spec.value.trim();
            (!v.is_empty()).then(|| WinRecipe::game(format!("cmd.exe /c {v}"), None))
        }
        _ => None,
    }
}

/// Default `steam.exe` only. Non-default installs use the explorer.exe protocol
/// fallback. Probes Program Files, `ProgramFiles(x86)` first.
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

/// The Battle.net client's exe: the machine-wide `battlenet://` handler registration first
/// (it follows a non-default install), else the default under Program Files (x86).
#[cfg(windows)]
fn battlenet_exe() -> Option<std::path::PathBuf> {
    use winreg::enums::{HKEY_CLASSES_ROOT, KEY_READ};
    use winreg::RegKey;

    let registered = RegKey::predef(HKEY_CLASSES_ROOT)
        .open_subkey_with_flags(r"battlenet\shell\open\command", KEY_READ)
        .and_then(|k| k.get_value::<String, _>(""))
        .ok()
        .and_then(|c| exe_from_shell_command(&c).map(std::path::PathBuf::from))
        .filter(|p| p.is_file());
    if registered.is_some() {
        return registered;
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(pf) = std::env::var_os(var) {
            let p = std::path::PathBuf::from(pf)
                .join("Battle.net")
                .join("Battle.net.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// PackageFamilyName from `AppRepository\Packages\<PackageFullName>`:
/// `Name_PublisherHash`. Read the hash; never compute it.
///
/// LocalSystem can enumerate that dir; the plugin runner (LocalService) cannot.
/// The plugin sends Identity from MicrosoftGame.config; this completes the PFN.
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

/// `Name_Version_Arch_ResourceId_PublisherHash` → `Name_PublisherHash`.
/// Hash is the last `_`-segment; `Name` is the caller's identity.
#[cfg(windows)]
fn pfn_from_full(dir_name: &str, identity: &str) -> Option<String> {
    let hash = dir_name.rsplit('_').next()?;
    (!hash.is_empty() && hash != dir_name).then(|| format!("{identity}_{hash}"))
}

// Per-kind launch values. Scanners supply the VALUE; the host builds the URI.
// Lives here so URI construction stays with launch (design D1). Unparseable
// or hostile → `None`, never a partial command.

/// Digits-only Steam appid (or 64-bit [`shortcut_gameid`]). Shared kind because
/// `rungameid` takes either. Used by [`command_for`] and [`windows_launch_for`].
pub(crate) fn valid_steam_appid(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
}

/// 64-bit `rungameid` for a non-Steam shortcut: high dword = 32-bit appid,
/// low dword = marker `0x0200_0000`. The bare 32-bit appid does not launch it.
pub(crate) fn shortcut_gameid(appid: u32) -> u64 {
    ((appid as u64) << 32) | 0x0200_0000
}

/// `steam_ui` values (design D4). Closed set, validated inbound and outbound
/// so a third value cannot sit in the library and resolve to nothing at launch.
pub(crate) fn valid_steam_ui(value: &str) -> bool {
    matches!(value, "bigpicture" | "desktop")
}

/// One AUMID half: non-empty, charset that cannot break `shell:AppsFolder\…`.
/// Load-bearing for `xbox`, whose Identity arrives from a plugin.
pub(crate) fn aumid_part(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn valid_aumid(value: &str) -> bool {
    value
        .split_once('!')
        .is_some_and(|(pfn, app)| aumid_part(pfn) && aumid_part(app))
}

/// Playnite GUID, interpolated into an explorer.exe URI: 8-4-4-4-12 hex + dashes.
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

/// Ubisoft Connect game id, interpolated into `uplay://launch/<id>/0`: digits only.
pub(crate) fn valid_uplay_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 16 && value.bytes().all(|b| b.is_ascii_digit())
}

/// Amazon Games product id (`amzn1.adg.product.<uuid>`), interpolated into
/// `amazon-games://play/<id>`: alphanumerics, `.`, `_`, `-`.
pub(crate) fn valid_amazon_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Battle.net launch code (`WTCG`, `Pro`, `wow_classic`), handed to the client as
/// `--exec="launch <code>"`: word characters, case kept — the client matches exactly.
pub(crate) fn valid_battlenet_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `launcher_ui` values this OS can open (design D4). One kind; a value names
/// a UI (`heroic` vs `heroic-console`; Windows `playnite` is Fullscreen).
///
/// Unknown *kind* is an unlaunchable tile; unknown *value* is a 400 that
/// refuses the whole reconcile. Platform-gated so a plugin gets 400 inbound
/// instead of a tile that fails at launch. `command` is operator-only, so a
/// plugin cannot publish a launcher without this kind.
fn launcher_ui_stores() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        &["heroic", "heroic-console", "lutris"]
    }
    // Playnite only. Unverified Epic/GOG/Xbox activation would ship a dead tile.
    #[cfg(windows)]
    {
        &["playnite"]
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        &[]
    }
}

/// Vocabulary: is `value` a launcher this OS knows? A miss is a plugin bug;
/// installing software will not fix it, so reconcile refuses the payload.
pub(crate) fn known_launcher_ui(value: &str) -> bool {
    launcher_ui_stores().contains(&value)
}

/// Environment: can this box open `value` *now*? Separate from
/// [`known_launcher_ui`]: unknown value is a plugin bug (400); known-but-missing
/// is an ordinary fact. Conflating them 400'd whole libraries over one tile
/// ([`super::sanitize_launcher_entries`] drops the tile; games still sync).
pub(crate) fn resolvable_launcher_ui(value: &str) -> bool {
    if !known_launcher_ui(value) {
        return false;
    }
    #[cfg(windows)]
    if value == "playnite" {
        return playnite_fullscreen_exe().is_some();
    }
    // Both Heroic tiles share `heroic_launch_prefix`. Plugin `detect` is only
    // `~/.config/heroic`, which can survive uninstall — so probe the binary.
    #[cfg(target_os = "linux")]
    if matches!(value, "heroic" | "heroic-console") {
        return heroic_launch_prefix().is_some();
    }
    true
}

/// Playnite Fullscreen exe, if found. `playnite://` is registered to the
/// desktop app, so a URI cannot open fullscreen. `None` drops the tile.
#[cfg(windows)]
fn playnite_fullscreen_exe() -> Option<std::path::PathBuf> {
    const EXE: &str = "Playnite.FullscreenApp.exe";
    playnite_install_dirs()
        .into_iter()
        .map(|dir| dir.join(EXE))
        .find(|p| p.is_file())
}

/// Candidate Playnite install dirs, best first. LocalSystem invalidates the
/// obvious lookups:
///
/// - HKCU is SYSTEM's hive (`S-1-5-18`); read loaded `HKEY_USERS` instead
///   (logged-on streamers; same trade-off as [`crate::procscan::steam_running_hint`]).
/// - Match uninstall by `DisplayName`. Inno registers `<AppId>_is1`, not `Playnite`.
/// - `%LOCALAPPDATA%` is SYSTEM's profile; enumerate users-base profiles instead.
///
/// Portable installs leave only the `playnite://` handler
/// ([`playnite_dir_from_uri_handler`]). Registry `InstallLocation` before
/// conventional paths; each candidate is an `is_file` probe.
#[cfg(windows)]
fn playnite_install_dirs() -> Vec<std::path::PathBuf> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, HKEY_USERS, KEY_READ};
    use winreg::RegKey;

    // 64- and 32-bit views. HKCU/HKU `Software` is not redirected; WOW is HKLM-only.
    const UNINSTALL: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall";
    const UNINSTALL_WOW: &str = r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    // `playnite://` command: bare inside a `…_Classes` hive, `Software\Classes` elsewhere.
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
        // `_Classes` hives hold associations (`playnite://`), never uninstall keys.
        // Probe both spellings; a miss is one failed `open_subkey`.
        if sid.ends_with("_Classes") {
            playnite_dir_from_uri_handler(&hive, URI_COMMAND, &mut dirs);
            continue;
        }
        playnite_dirs_from_uninstall(&hive, UNINSTALL, &mut dirs);
        playnite_dir_from_uri_handler(&hive, CLASSES_URI_COMMAND, &mut dirs);
    }

    // Default per-user path, including profiles whose hive is not loaded.
    for profile in windows_user_profiles() {
        push_unique(&mut dirs, profile.join(r"AppData\Local\Playnite"));
    }
    dirs
}

/// `InstallLocation` from Playnite-looking uninstall entries under `root\path`.
/// Match `DisplayName` (`starts_with`) because the key is Inno's `<AppId>_is1`.
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

/// Directory of the registered `playnite://` handler. Finds portable Playnite
/// (no uninstall key, not under a profile). Same registration the launch path uses.
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

/// Exe from a shell-open command. Quoted form first (what a registrar writes).
/// Unquoted fallback cuts at `.exe` — the path may contain spaces.
#[cfg_attr(not(windows), allow(dead_code))]
fn exe_from_shell_command(command: &str) -> Option<&str> {
    let command = command.trim();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().filter(|p| !p.is_empty());
    }
    let end = command.to_ascii_lowercase().find(".exe")? + ".exe".len();
    Some(&command[..end])
}

/// Playnite install dirs as art roots. Portable keeps covers beside the exe
/// (`<PlayniteDir>\library\files\…`); the users base cannot see that tree.
/// Same shape as [`super::art::steam_art_roots`]. Candidates come from host
/// registry/fs probes, not the plugin lane that supplies the art path.
#[cfg(windows)]
pub(crate) fn playnite_art_roots() -> Vec<std::path::PathBuf> {
    playnite_install_dirs()
        .into_iter()
        .filter(|d| d.is_dir())
        .collect()
}

/// User profiles (`C:\Users\*`), minus `Public`. Users base is `%PUBLIC%`'s
/// parent ([`super::art::art_roots`]); `%SystemDrive%\Users` if the var is missing.
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

/// Dedup. Candidate lists are tiny; a linear scan beats a set.
#[cfg(windows)]
fn push_unique(out: &mut Vec<std::path::PathBuf>, path: std::path::PathBuf) {
    if !out.contains(&path) {
        out.push(path);
    }
}

/// `<runner>:<appName>` → Heroic command, nested in gamescope.
///
/// Heroic is single-instance Electron. Fresh gamescope: boot, launch, stay
/// hidden (`--no-gui`). An already-running GUI forwards the URI and exits,
/// which would tear the session — validated only for the fresh-session case.
#[cfg(target_os = "linux")]
pub(crate) fn heroic_command(value: &str) -> Option<String> {
    let (runner, app) = value.split_once(':')?;
    if !matches!(runner, "legendary" | "gog" | "nile") {
        return None;
    }
    // appName charset: keep the URI a single token (Epic/Amazon alnum, GOG digits).
    if app.is_empty()
        || !app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    let prefix = heroic_launch_prefix()?;
    // No quotes: gamescope splits on whitespace. URI has no spaces; `&` is exec'd, not a shell.
    Some(format!(
        "{prefix} --no-gui heroic://launch?appName={app}&runner={runner}"
    ))
}

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

/// Epic URI from a `<namespace>:<catalogItemId>:<appName>` triple or a bare
/// `appName`. Charset-checked so the URI stays one argv token.
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

/// GOG `exe \t args \t workdir` triple → `(cmdline, workdir)`. Direct spawn
/// (no Galaxy). Re-confine exe and workdir to [`gog_install_dirs`]: the plugin
/// already confined at parse, but the triple arrives over the provider API.
/// `None` if no GOG install owns the exe.
#[cfg(windows)]
pub(crate) fn gog_spawn(value: &str) -> Option<(String, Option<PathBuf>)> {
    gog_spawn_in(value, &gog_install_dirs())
}

/// Pure [`gog_spawn`]: `installs` is the set the exe and workdir must sit inside.
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
    // Out-of-bounds workdir is dropped, not refused: it only sets cwd of a confined exe.
    let workdir = parts.next().filter(|s| under(s)).map(PathBuf::from);
    let cmdline = if args.trim().is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {args}")
    };
    Some((cmdline, workdir))
}

/// GOG install roots from `HKLM\SOFTWARE\WOW6432Node\GOG.com\Games\<id>\PATH`.
/// GOG is 32-bit, so it writes the WOW view. Empty ⇒ every `gog` launch refuses.
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

/// Windows path containment: case-insensitive, either separator, `..` refused
/// (it climbs out of the prefix the test just accepted). String compare, not
/// [`Path::starts_with`], which is case-sensitive — plugin spelling may differ.
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

/// Launches an operator `apps.json` command as the host's WTS session user.
///
/// Capture is already live and the host remains SYSTEM. Linux instead uses
/// compositor-aware [`launch_session_command`].
#[cfg(windows)]
pub fn launch_gamestream_command(cmd: &str) -> Result<WindowsLaunch> {
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    let pid = crate::interactive::spawn_as_current_session_user(&format!("cmd.exe /c {cmd}"), None)
        .context("spawn gamestream command as the current WTS session user")?;
    tracing::info!(command = %cmd, pid, "gamestream: launched app as the current WTS session user");
    // `cmd.exe /c` waits, so its PID tracks the command; forwarders remain the lease shim's concern.
    Ok(WindowsLaunch {
        pid,
        owns_game: true,
    })
}

/// Launches a GameStream `/applist` title through [`launch_title`] in this host's
/// WTS session. Linux uses [`resolve_launch`] then [`launch_session_command`].
#[cfg(windows)]
pub fn launch_gamestream_library(id: &str) -> Result<WindowsLaunch> {
    launch_title(id)
}

/// Child from a session launch, for lifetime tracking
/// (design/session-game-lifetime.md).
#[cfg(target_os = "linux")]
pub struct SpawnedLaunch {
    pub child: std::process::Child,
    /// Own process group (plain session spawn) vs host group (gamescope).
    /// A non-leader must never be signalled by negative pid
    /// ([`crate::gamelease::OwnedChild::group_leader`]).
    pub group_leader: bool,
}

/// Host-resolved command into the live Linux session, after capture is up.
/// Shared by native and GameStream. Best-effort: failure leaves the user on
/// the streamed desktop rather than tearing the stream down.
///
/// * **KWin / Mutter** — session env already retargeted, virtual output is
///   primary; a plain spawn lands on the stream.
/// * **Hyprland / wlroots (sway)** — EXTEND-only: the streamed head sits
///   beside the operator's. [`crate::vdisplay::focus_streamed_output`] claims
///   it here; capture also focused it, but portal handshake / encoder / first
///   frame sit in between and can steal focus back.
/// * **gamescope (managed / SteamOS / attach)** — spawn inside the running
///   session ([`crate::vdisplay::launch_into_gamescope_session`]). `steam
///   steam://…` also forwards over Steam's pipe.
/// * **gamescope (bare spawn)** — not here: nested via `set_launch_command`
///   (`vdisplay::launch_is_nested`).
#[cfg(target_os = "linux")]
pub fn launch_session_command(
    compositor: crate::vdisplay::Compositor,
    cmd: &str,
) -> Result<SpawnedLaunch> {
    use std::os::unix::process::CommandExt;
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    // Focus the streamed head first (no-op off EXTEND). Same slot as the
    // absolute-input pointer, so focus and cursor share one head.
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
                // Own process group: later teardown signals the shell and its
                // children, and not the host's group.
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
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570; rm -rf ~".into(),
        };
        assert_eq!(command_for(&evil), None);
        let custom = LaunchSpec {
            kind: "command".into(),
            value: "dolphin-emu --batch".into(),
        };
        assert_eq!(command_for(&custom).as_deref(), Some("dolphin-emu --batch"));
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
        assert_eq!(heroic_command("badrunner:Quail"), None);
        assert_eq!(heroic_command("legendary:bad name"), None);
        assert_eq!(heroic_command("nile:"), None);
        // Prefix exists only on boxes with Heroic; assert URI shape only then.
        if let Some(cmd) = heroic_command("legendary:Quail-1.2_x") {
            assert!(cmd.contains("heroic://launch?appName=Quail-1.2_x&runner=legendary"));
            assert!(cmd.contains("--no-gui"));
        }
    }

    #[test]
    fn steam_ui_is_a_closed_two_value_enum() {
        assert!(valid_steam_ui("bigpicture"));
        assert!(valid_steam_ui("desktop"));
        assert!(!valid_steam_ui("gamepadui"));
        assert!(!valid_steam_ui(""));
        assert!(!valid_steam_ui("bigpicture; rm -rf ~"));
    }

    #[test]
    fn launcher_ui_accepts_only_launchers_this_host_can_open() {
        #[cfg(target_os = "linux")]
        {
            assert!(known_launcher_ui("heroic"));
            assert!(known_launcher_ui("heroic-console"));
            assert!(known_launcher_ui("lutris"));
            assert!(!known_launcher_ui("gog"));
            // Both Heroic tiles share one probe; a miss drops both, not one dead tile.
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
            // Vocabulary vs installed: separate so a missing Playnite cannot 400 the library.
            assert!(known_launcher_ui("playnite"));
            assert_eq!(
                resolvable_launcher_ui("playnite"),
                playnite_fullscreen_exe().is_some()
            );
            assert!(!known_launcher_ui("heroic"));
            assert!(!known_launcher_ui("gog"));
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            assert!(!known_launcher_ui("heroic"));
            assert!(!known_launcher_ui("gog"));
        }
        assert!(!known_launcher_ui(""));
        assert!(!known_launcher_ui("lutris; rm -rf ~"));
        assert!(!resolvable_launcher_ui(""));
        assert!(!resolvable_launcher_ui("lutris; rm -rf ~"));
    }

    /// `xbox` is plugin-facing: Identity over the wire, host completes the PFN.
    /// Charset is load-bearing here; `aumid` is host-derived.
    #[test]
    fn xbox_value_is_identity_bang_appid_and_charset_guarded() {
        assert!(valid_aumid("Microsoft.Foo!Game"));
        assert!(valid_aumid("A_b-c.d!App"));
        assert!(!valid_aumid("Microsoft.Foo"));
        assert!(!valid_aumid("!Game"));
        assert!(!valid_aumid("Microsoft.Foo!"));
        assert!(!valid_aumid(""));
        assert!(!valid_aumid("Foo\"!Game"));
        assert!(!valid_aumid("Foo!Game\" & calc"));
        assert!(!valid_aumid("Foo\\..\\Bar!Game"));
        assert!(!valid_aumid("Foo Bar!Game"));
    }

    /// Parse `playnite://` command lines (portable probe, works off-Windows).
    /// A miss is a portable install the host cannot find — no tile, no covers.
    #[test]
    fn exe_is_read_out_of_a_registered_shell_command() {
        assert_eq!(
            exe_from_shell_command(r#""D:\Apps\Playnite\Playnite.DesktopApp.exe" --uridata "%1""#),
            Some(r"D:\Apps\Playnite\Playnite.DesktopApp.exe")
        );
        // Unquoted path with spaces: cannot split on whitespace.
        assert_eq!(
            exe_from_shell_command(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe %1"),
            Some(r"C:\Program Files\Playnite\Playnite.DesktopApp.exe")
        );
        assert_eq!(
            exe_from_shell_command(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE"),
            Some(r"D:\Apps\Playnite\PLAYNITE.DESKTOPAPP.EXE")
        );
        // No exe shape / empty quotes: a bogus candidate would become an art root.
        assert_eq!(
            exe_from_shell_command("rundll32 shell32.dll,Control_RunDLL"),
            None
        );
        assert_eq!(exe_from_shell_command(r#""" %1"#), None);
        assert_eq!(exe_from_shell_command(""), None);
    }

    /// Launcher tile is Fullscreen. `playnite://` is registered to the desktop app.
    #[cfg(windows)]
    #[test]
    fn playnite_launcher_opens_the_fullscreen_app() {
        let ui = |v: &str| {
            windows_launch_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
            })
        };
        assert!(ui("gog").is_none());
        assert!(ui("heroic").is_none());
        assert!(ui("").is_none());

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
        // Bare `lutris` opens the window; the URI form is `lutris_id` and launches a game.
        assert_eq!(ui("lutris").as_deref(), Some("lutris"));
        assert!(!ui("lutris").unwrap().contains("rungameid"));
        // Same prefix as a game launch, no `--no-gui` / URI. `None` without Heroic is correct.
        if let Some(cmd) = ui("heroic") {
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
            assert!(!cmd.contains("heroic://"), "no game URI: {cmd:?}");
            assert!(
                !cmd.contains("--console"),
                "that is the other tile: {cmd:?}"
            );
        }
        // Both flags: `--console` alone does not fill the screen. Same prefix as `heroic`.
        assert_eq!(ui("heroic-console").is_some(), ui("heroic").is_some());
        if let Some(cmd) = ui("heroic-console") {
            assert!(cmd.contains("--console"), "{cmd:?}");
            assert!(cmd.contains("--fullscreen"), "{cmd:?}");
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
            // Gamescope spawns by `split_whitespace`, so every token must stand alone.
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
        // Nested in gamescope this is the SteamOS `--steam` game-mode shape.
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
        assert!(valid_steam_appid(
            &shortcut_gameid(2_456_789_012).to_string()
        ));
        assert!(!valid_steam_appid(""));
        assert!(!valid_steam_appid("570; rm -rf ~"));
        assert!(!valid_steam_appid("-1"));
    }

    /// Launch vocabulary, not enumeration: the scanner supplies the 32-bit appid only.
    #[test]
    fn shortcut_gameid_composes_appid_and_marker() {
        let id = shortcut_gameid(0x8000_0000);
        assert_eq!(id >> 32, 0x8000_0000, "high dword is the shortcut appid");
        assert_eq!(id & 0xFFFF_FFFF, 0x0200_0000, "low dword is the marker");
    }

    #[test]
    fn store_ids_are_charset_guarded() {
        assert!(valid_uplay_id("5595"));
        assert!(!valid_uplay_id(""));
        assert!(!valid_uplay_id("5595/0"));
        assert!(valid_amazon_id(
            "amzn1.adg.product.1f2a3b4c-5d6e-7f80-9a1b-2c3d4e5f6a7b"
        ));
        assert!(!valid_amazon_id("amzn1.adg.product.x\" & calc"));
        assert!(!valid_amazon_id(""));
        assert!(valid_battlenet_code("WTCG"));
        assert!(valid_battlenet_code("wow_classic"));
        assert!(!valid_battlenet_code("Pro\" & calc"));
        assert!(!valid_battlenet_code(""));
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
        assert!(epic_launch_uri("bad part:x:y").is_none());
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

    /// Triple arrives over the provider API: exe must sit in a host-found GOG install.
    #[cfg(windows)]
    #[test]
    fn gog_spawn_refuses_an_exe_outside_every_gog_install() {
        let installs = ["C:\\Games\\W3".to_string()];
        let spawn = |v: &str| gog_spawn_in(v, &installs);
        assert!(spawn("C:\\Windows\\System32\\cmd.exe\t/c calc\tC:\\Games\\W3").is_none());
        // Prefix match is not containment: a sibling path must not pass.
        assert!(spawn("C:\\Games\\W3x\\evil.exe").is_none());
        assert!(spawn("C:\\Games\\W3\\..\\..\\Windows\\System32\\cmd.exe").is_none());
        // Empty install set ⇒ nothing is launchable, rather than everything.
        assert!(gog_spawn_in("C:\\Games\\W3\\witcher3.exe", &[]).is_none());
        // Case and separator: plugin spelling may differ from the registry.
        assert!(spawn("c:/games/w3/bin/game.exe").is_some());
        // Out-of-bounds workdir costs the workdir, not the launch.
        let (_, wd) = spawn("C:\\Games\\W3\\witcher3.exe\t\tC:\\Windows\\System32").unwrap();
        assert!(wd.is_none());
    }

    /// Family name from PackageFullName: the `xbox` kind's Identity completion.
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
        // No `_` → nothing to reduce; must not invent a hash.
        assert!(pfn_from_full("NoUnderscore", "NoUnderscore").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_launch_for_maps_and_guards() {
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
        };
        let steam_r = windows_launch_for(&steam).expect("steam recipe");
        let line = &steam_r.cmdline;
        assert!(line.contains("steam://rungameid/570"), "line was {line:?}");
        assert!(steam_r.workdir.is_none());
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570\" & calc".into(),
        };
        assert!(windows_launch_for(&evil).is_none());
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
        let uplay = windows_launch_for(&LaunchSpec {
            kind: "uplay".into(),
            value: "5595".into(),
        })
        .unwrap();
        assert_eq!(uplay.cmdline, "explorer.exe \"uplay://launch/5595/0\"");
        assert!(!uplay.owns_game, "a protocol hand-off is not the game");
        let amazon = windows_launch_for(&LaunchSpec {
            kind: "amazon".into(),
            value: "amzn1.adg.product.abc-123".into(),
        })
        .unwrap();
        assert_eq!(
            amazon.cmdline,
            "explorer.exe \"amazon-games://play/amzn1.adg.product.abc-123\""
        );
        assert!(windows_launch_for(&LaunchSpec {
            kind: "uplay".into(),
            value: "5595\" & calc".into()
        })
        .is_none());
        assert!(windows_launch_for(&LaunchSpec {
            kind: "battlenet".into(),
            value: "WTCG\" & calc".into()
        })
        .is_none());
    }
}
