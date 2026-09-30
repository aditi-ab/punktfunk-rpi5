//! Managed emulators: hermir, opened on this host's prefix. A plugin only asks; an install runs
//! on the operator's click and lands under `<prefix>/<id>/app`, the folder the plugin
//! is then granted, so its launch templates may point inside it.
use std::path::{Path, PathBuf};

use hermir::progress::Quiet;
use hermir::{Hermir, Installed, Options};

/// Where managed emulators live. `%ProgramData%\punktfunk\emulators` on Windows, so the SYSTEM
/// service installs and the player's session runs. The data dir on POSIX: a plugin is granted
/// the emulator's folder, and the runner shares nothing under the config dir.
pub fn prefix() -> PathBuf {
    #[cfg(windows)]
    let base = pf_paths::config_dir();
    #[cfg(not(windows))]
    let base = pf_paths::data_dir();
    base.join("emulators")
}

/// The emulator's own folder: exe, config and saves for a portable one. The path a plugin is
/// granted when its request is allowed, whether the copy lives here or is a Flatpak.
pub fn home_of(id: &str) -> PathBuf {
    prefix().join(id).join("app")
}

pub fn open() -> hermir::Result<Hermir> {
    Hermir::open(Options {
        prefix: Some(prefix()),
        ..Options::default()
    })
}

/// Whether this OS has an install channel for `id`; `NotInCatalog` for an unknown id.
pub fn offered(id: &str) -> hermir::Result<bool> {
    let h = open()?;
    let entry = h
        .catalog()
        .get(id)
        .ok_or_else(|| hermir::Error::NotInCatalog(id.into()))?;
    Ok(entry.channels.get(h.os()).is_some())
}

/// Installs (or reinstalls) `id` and makes its folder exist, so the grant that follows has a
/// directory to land on even when the copy itself is a Flatpak.
pub fn install(id: &str) -> hermir::Result<Installed> {
    let h = open()?;
    let row = h.emulator(id)?.install(&Quiet)?;
    let home = home_of(id);
    std::fs::create_dir_all(&home).map_err(|e| hermir::Error::Io {
        op: "create",
        path: home.clone(),
        source: e,
    })?;
    crate::plugins::open_for_players(&prefix().join(id));
    Ok(row)
}

pub fn remove(id: &str, purge: bool) -> hermir::Result<()> {
    open()?.emulator(id)?.remove(purge)
}

/// RetroArch's cores folder on this machine, from the best copy hermir knows; `None` without
/// a RetroArch. hermir creates it on the first core, so the grant that follows has a folder.
pub fn cores_dir() -> hermir::Result<Option<PathBuf>> {
    let h = open()?;
    Ok(h.emulator("retroarch")?
        .best()?
        .and_then(|i| i.config_root)
        .map(|root| root.join("cores")))
}

/// A libretro core from the buildbot into that folder.
pub fn install_core(core: &str) -> hermir::Result<PathBuf> {
    open()?.install_core(core, &Quiet)
}

/// Every copy of `id` answers its first-run questions and gets `platform`'s firmware from
/// `firmware_dir` (the regular files in it, not below). `platform` is a catalog id or any of
/// its aliases (RomM slug, ES-DE folder, libretro name). Blocking: an installer may run.
pub fn prepare(
    id: &str,
    platform: Option<&str>,
    firmware_dir: Option<&Path>,
) -> hermir::Result<Vec<(String, hermir::Prepared)>> {
    let h = open()?;
    let emulator = h.emulator(id)?;
    let platform = platform.map(|p| {
        h.catalog()
            .platforms()
            .iter()
            .find(|x| x.id == p || x.aliases.values().any(|a| a == p))
            .map_or_else(|| p.to_string(), |x| x.id.clone())
    });
    let platform = platform.as_deref();
    let firmware: Vec<PathBuf> = match firmware_dir {
        None => Vec::new(),
        Some(dir) => std::fs::read_dir(dir)
            .map_err(|e| hermir::Error::Io {
                op: "read",
                path: dir.to_path_buf(),
                source: e,
            })?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .collect(),
    };
    Ok(emulator
        .copies()?
        .iter()
        .map(|copy| {
            (
                copy.exe.to_string(),
                emulator.prepare(copy, platform, &firmware),
            )
        })
        .collect())
}

/// A core name as the buildbot spells it: `snes9x`, `mupen64plus_next`.
pub fn valid_core(core: &str) -> bool {
    !core.is_empty()
        && core.len() <= 64
        && core.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The grant a plugin's request asks for: read-only, on the emulator's folder.
pub fn grant_target(id: &str) -> (PathBuf, bool) {
    (home_of(id), false)
}

/// True when `path` is an emulator home under the prefix.
pub fn is_home(path: &Path) -> bool {
    path.starts_with(prefix())
}
