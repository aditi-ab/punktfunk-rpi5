//! Windows facts for conflicting-host detection: Toolhelp for running processes,
//! the SCM for registered services, `%ProgramFiles%` for on-disk installs.
//! Best-effort — privilege or API failure yields no evidence, never aborts startup.

use super::{Evidence, Known};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_service::service::{ServiceAccess, ServiceStartType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// Lowercased executable basenames (no `.exe`) from a Toolhelp snapshot.
/// `szExeFile` is the module base name, not a full path.
pub fn running_processes() -> Vec<String> {
    let mut out = Vec::new();
    // SAFETY: the snapshot handle is closed on every exit path; `entry` is fully
    // initialized (`dwSize` set) before Process32FirstW reads it.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        // Zeroed then `dwSize` set. Toolhelp reads `szExeFile` from this; there is no useful Default.
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

pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let mut ev = Vec::new();
    for svc in known.win_services {
        if let Some(autostart) = service_start_type(svc) {
            ev.push(Evidence::Service {
                name: (*svc).to_string(),
                autostart,
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

/// `Some(autostart)` if the SCM has this service, `None` if it does not.
///
/// `autostart` is boot/system/auto only — those come up alone and can take the
/// GameStream ports. Missing `QUERY_CONFIG` reports dormant, not autostart:
/// a false alarm is worse than a miss, and a live host still hits the process scan.
fn service_start_type(name: &str) -> Option<bool> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = mgr
        .open_service(
            name,
            ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
        )
        // Status-only if `QUERY_CONFIG` is denied: still present (dormant), not absent.
        .or_else(|_| mgr.open_service(name, ServiceAccess::QUERY_STATUS))
        .ok()?;
    let autostart = svc.query_config().is_ok_and(|c| {
        matches!(
            c.start_type,
            ServiceStartType::AutoStart
                | ServiceStartType::BootStart
                | ServiceStartType::SystemStart
        )
    });
    Some(autostart)
}

/// Under `ProgramFiles`, `ProgramW6432`, or `ProgramFiles(x86)` — WOW64 vs 32-bit hosts differ.
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
