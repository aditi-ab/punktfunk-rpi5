//! Linux conflicting-host facts: `/proc` for running processes, the standard systemd unit dirs +
//! flatpak app dirs + `PATH` for install markers. All best-effort and dependency-free (no
//! subprocess spawns) — a missing `/proc` or an unreadable dir simply yields no evidence.

use super::{Evidence, Known};
use std::path::Path;

/// Lowercased basenames of every process whose `/proc/<pid>/comm` we can read. `comm` is the
/// kernel's 15-char command name — every host we match on (sunshine, apollo, vibeshine, vibepollo,
/// luminalshine) fits within that, so no `/proc/<pid>/exe` readlink is needed.
pub fn running_processes() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        // Only numeric (pid) directories.
        if !entry
            .file_name()
            .to_str()
            .map(|n| n.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            out.push(comm.trim().to_ascii_lowercase());
        }
    }
    out
}

/// systemd unit registration + flatpak + `PATH` binary presence — the "installed" evidence.
pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let mut ev = Vec::new();

    // systemd units, system + per-user, in the dirs systemd actually reads.
    let home = std::env::var_os("HOME");
    let mut unit_dirs: Vec<String> = vec![
        "/etc/systemd/system".into(),
        "/run/systemd/system".into(),
        "/usr/lib/systemd/system".into(),
        "/lib/systemd/system".into(),
        "/etc/systemd/user".into(),
        "/usr/lib/systemd/user".into(),
    ];
    if let Some(h) = &home {
        unit_dirs.push(format!("{}/.config/systemd/user", h.to_string_lossy()));
    }
    for unit in known.linux_units {
        let file = format!("{unit}.service");
        if unit_dirs.iter().any(|d| Path::new(d).join(&file).exists()) {
            ev.push(Evidence::Service { name: file });
        }
    }

    // flatpak app installs (system + per-user).
    let mut flatpak_roots: Vec<String> = vec!["/var/lib/flatpak/app".into()];
    if let Some(h) = &home {
        flatpak_roots.push(format!("{}/.local/share/flatpak/app", h.to_string_lossy()));
    }
    for id in known.flatpaks {
        if flatpak_roots.iter().any(|r| Path::new(r).join(id).exists()) {
            ev.push(Evidence::Installed {
                at: format!("flatpak {id}"),
            });
        }
    }

    // A matching binary on PATH (covers manual / package installs the unit/flatpak checks miss).
    let path = std::env::var_os("PATH");
    for bin in known.processes {
        if let Some(found) = find_on_path(bin, path.as_deref()) {
            ev.push(Evidence::Installed { at: found });
        }
    }

    ev
}

fn find_on_path(bin: &str, path: Option<&std::ffi::OsStr>) -> Option<String> {
    let dirs = path.map(std::env::split_paths).into_iter().flatten();
    // Always also probe the common bindirs, even if PATH is unset/narrow (e.g. a service context).
    let extra = ["/usr/bin", "/usr/local/bin", "/bin", "/usr/games"]
        .into_iter()
        .map(std::path::PathBuf::from);
    for dir in dirs.chain(extra) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}
