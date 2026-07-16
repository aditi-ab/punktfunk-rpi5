//! Windows conflicting-host facts: a Toolhelp process snapshot for what's running, the SCM for
//! registered services, and `%ProgramFiles%` for on-disk installs. All best-effort — any failing
//! query (no privilege, API error) yields no evidence rather than aborting startup.

use super::{Evidence, Known};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// Lowercased executable basenames (without `.exe`) of every running process, via a Toolhelp
/// snapshot. `szExeFile` is the module base name (e.g. `sunshine.exe`), not a full path.
pub fn running_processes() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: standard Toolhelp snapshot walk. The snapshot handle is closed on every exit path;
    // `entry` is fully initialized (dwSize set) before Process32FirstW reads it.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        // Zeroed then dwSize set — the canonical Toolhelp init (no reliance on a Default impl for
        // the 260-wide szExeFile array).
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..std::mem::zeroed()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase();
                out.push(name.strip_suffix(".exe").unwrap_or(&name).to_string());
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// SCM service registration + `%ProgramFiles%` install dirs — the "installed" evidence.
pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let mut ev = Vec::new();
    for svc in known.win_services {
        if service_exists(svc) {
            ev.push(Evidence::Service {
                name: (*svc).to_string(),
            });
        }
    }
    for dir in known.win_dirs {
        if let Some(at) = program_files_dir(dir) {
            ev.push(Evidence::Installed { at });
        }
    }
    ev
}

/// True if a service by this name is registered with the SCM (running or stopped). Opening it with
/// `QUERY_STATUS` fails cleanly when it doesn't exist.
fn service_exists(name: &str) -> bool {
    let Ok(mgr) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return false;
    };
    mgr.open_service(name, ServiceAccess::QUERY_STATUS).is_ok()
}

/// The install directory under any of the Program Files roots, if it exists.
fn program_files_dir(dir: &str) -> Option<String> {
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            let p = std::path::Path::new(&base).join(dir);
            if p.is_dir() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}
