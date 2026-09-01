//! The self-extractor (D2/D3): a packed exe unpacks runtime + payload into a fresh admin-only
//! dir and re-runs itself from there, where the WinAppSDK DLLs sit beside it. The child gets
//! `PUNKTFUNK_SETUP_ROOT` plus the same argv; the parent waits and returns the child's exit
//! code, so winget and a terminal see one process with one result.
//!
//! The dir is created with an explicit SDDL — owner Administrators, SYSTEM + Administrators
//! only, protected, inherited — so it passes the host's `ensure_admin_only_source` by
//! construction: driver staging and the password temp file are served from it. Cleanup is
//! left to a later run: a process cannot delete the directory it runs from.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use punktfunk_setup::platform::windows::sys;

use crate::{overlay, payload};

pub const ROOT_ENV: &str = "PUNKTFUNK_SETUP_ROOT";

/// `Some(exit)` when this process was the packed outer exe and has run the extracted copy.
/// A plain wizard build (no footer) or the extracted child itself gets `None`.
pub fn relaunch_if_packed() -> Option<ExitCode> {
    if std::env::var_os(ROOT_ENV).is_some() {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    let data = std::fs::read(&exe).ok()?;
    let payload = overlay::extract(&data).ok()?;
    // Console first: the child's AttachConsole(ATTACH_PARENT_PROCESS) binds OURS.
    let console = sys::attach_parent_console();
    Some(match extract_and_run(&exe, &data, payload) {
        Ok(code) => code,
        Err(e) => {
            use std::io::Write;
            if let Some(mut c) = console {
                let _ = writeln!(c, "  xx {e}");
            }
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    })
}

fn extract_and_run(exe: &Path, data: &[u8], payload: &[u8]) -> Result<ExitCode, String> {
    let root = fresh_root()?;
    payload::extract(payload, &root)?;
    // The stub is the bytes before the overlay — the same wizard, minus payload and
    // signature. Written beside the runtime under the launch name, so `unins000.exe` keeps
    // its D6 meaning in the child.
    let stub_len = payload.as_ptr() as usize - data.as_ptr() as usize;
    let name = exe.file_name().ok_or("the running exe has no file name")?;
    let child = root.join("runtime").join(name);
    std::fs::write(&child, &data[..stub_len]).map_err(|e| format!("{}: {e}", child.display()))?;
    let status = std::process::Command::new(&child)
        .args(std::env::args_os().skip(1))
        .env(ROOT_ENV, &root)
        .status()
        .map_err(|e| format!("could not start {}: {e}", child.display()))?;
    Ok(ExitCode::from(
        status.code().unwrap_or(1).clamp(0, 255) as u8
    ))
}

/// `%ProgramData%\punktfunk\setup\<pid>-<hex>`, protected before anything lands in it.
fn fresh_root() -> Result<PathBuf, String> {
    let data = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    let base = data.join("punktfunk").join("setup");
    std::fs::create_dir_all(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    let root = base.join(format!("{}-{}", std::process::id(), sys::random_hex(4)?));
    protected_dir(&root)?;
    Ok(root)
}

/// Owner Administrators; SYSTEM and Administrators full control, inherited; nothing else,
/// and protected from the parent's ACL. Every trustee with write access is privileged, which
/// is exactly what `ensure_admin_only_source` checks.
fn protected_dir(path: &Path) -> Result<(), String> {
    use windows::core::HSTRING;
    use windows::Win32::fileapi::CreateDirectoryW;
    use windows::Win32::sddl::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::winbase::LocalFree;
    use windows::Win32::{HANDLE, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    const SDDL: &str = "O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the SDDL string outlives the call; `sd` is the single LocalAlloc'd descriptor
    // the conversion returns, handed to CreateDirectoryW by pointer and LocalFree'd after.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SDDL),
            SDDL_REVISION_1 as u32,
            &mut sd,
            None,
        )
        .ok()
        .map_err(|e| format!("security descriptor: {e}"))?;
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let created = CreateDirectoryW(&HSTRING::from(path.as_os_str()), Some(&attrs)).ok();
        // HLOCAL is a HANDLE alias at this rev, so the alias cannot construct.
        LocalFree(HANDLE(sd.0));
        created.map_err(|e| format!("create {}: {e}", path.display()))
    }
}
