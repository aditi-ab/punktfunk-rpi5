//! The legs no process can do: SCM stop-and-wait, `.lnk` writing, the env-change broadcast,
//! the Appx presence check, real randomness. `cfg(windows)` implementations; everywhere else
//! an honest error so a cross-OS test that reaches one sees a warning path, never a lie.

#[cfg(windows)]
pub fn stop_service_wait(name: &str) -> Result<(), String> {
    use ::windows::core::HSTRING;
    use ::windows::Win32::System::Services::{
        CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
        SC_MANAGER_CONNECT, SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS,
        SERVICE_STOP, SERVICE_STOPPED,
    };

    // SAFETY: plain SCM handle lifecycle — open manager, open service, control, poll, close
    // both handles on every path. All buffers are stack locals owned by this frame.
    unsafe {
        let scm =
            OpenSCManagerW(None, None, SC_MANAGER_CONNECT).map_err(|e| format!("SCM: {e}"))?;
        let service = match OpenServiceW(
            scm,
            &HSTRING::from(name),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        ) {
            Ok(s) => s,
            Err(_) => {
                // Not installed — nothing to stop, which is the goal state.
                let _ = CloseServiceHandle(scm);
                return Ok(());
            }
        };
        let mut status = SERVICE_STATUS::default();
        let _ = ControlService(service, SERVICE_CONTROL_STOP, &mut status);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let outcome = loop {
            if QueryServiceStatus(service, &mut status).is_err()
                || status.dwCurrentState == SERVICE_STOPPED
            {
                break Ok(());
            }
            if std::time::Instant::now() > deadline {
                break Err(format!("'{name}' did not stop within 30s"));
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        };
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
        outcome
    }
}

#[cfg(not(windows))]
pub fn stop_service_wait(_name: &str) -> Result<(), String> {
    Err("SCM is Windows-only".into())
}

#[cfg(windows)]
pub fn create_shortcut(link: &str, target: &str) -> Result<(), String> {
    use ::windows::core::{Interface, HSTRING};
    use ::windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use ::windows::Win32::UI::Shell::{
        FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellLink,
        KF_FLAG_DEFAULT,
    };

    // SAFETY: COM lifecycle as in `nlm_networks` — init tolerating an initialized thread,
    // smart-pointer interfaces, uninit only when this call did the init. The known-folder
    // path is copied out before the interfaces drop.
    unsafe {
        let inited = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| -> Result<(), String> {
            let resolve = |id, tail: &str| -> Result<String, String> {
                let base = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None)
                    .map_err(|e| format!("known folder: {e}"))?;
                Ok(format!(
                    "{}{tail}",
                    base.to_string().map_err(|e| e.to_string())?
                ))
            };
            let path = if let Some(rest) = link.strip_prefix("<start menu>") {
                resolve(&FOLDERID_Programs, rest)?
            } else if let Some(rest) = link.strip_prefix("<desktop>") {
                resolve(&FOLDERID_Desktop, rest)?
            } else {
                link.to_string()
            };
            let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("ShellLink: {e}"))?;
            sl.SetPath(&HSTRING::from(target))
                .map_err(|e| format!("SetPath: {e}"))?;
            let pf: IPersistFile = sl.cast().map_err(|e| format!("IPersistFile: {e}"))?;
            pf.Save(&HSTRING::from(path.as_str()), true)
                .map_err(|e| format!("Save: {e}"))
        })();
        if inited {
            CoUninitialize();
        }
        result
    }
}

#[cfg(not(windows))]
pub fn create_shortcut(_link: &str, _target: &str) -> Result<(), String> {
    Err("shortcuts are Windows-only".into())
}

#[cfg(windows)]
pub fn broadcast_env_change() -> Result<(), String> {
    use ::windows::core::w;
    use ::windows::Win32::Foundation::{LPARAM, WPARAM};
    use ::windows::Win32::UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    };

    // SAFETY: HWND_BROADCAST with a static wide literal and a bounded timeout; no memory is
    // handed to the receivers beyond the constant string.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            2000,
            None,
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn broadcast_env_change() -> Result<(), String> {
    Err("WM_SETTINGCHANGE is Windows-only".into())
}

/// Is any WindowsAppRuntime 2.x framework package installed for this user?
#[cfg(windows)]
pub fn app_runtime_present() -> bool {
    use ::windows::core::HSTRING;
    use ::windows::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;

    let family = HSTRING::from("Microsoft.WindowsAppRuntime.2_8wekyb3d8bbwe");
    let mut count = 0u32;
    let mut buffer_len = 0u32;
    // SAFETY: the count-query form — null buffers, out-params only; any error means "treat
    // as absent" and the caller falls back to the (idempotent) installer download.
    unsafe {
        let _ = GetPackagesByPackageFamily(&family, &mut count, None, &mut buffer_len, None);
    }
    count > 0
}

#[cfg(not(windows))]
pub fn app_runtime_present() -> bool {
    false
}

/// `n` random bytes as lowercase hex — the web password. Real RNG, never a timestamp hack:
/// this is a credential.
#[cfg(windows)]
pub fn random_hex(n: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| format!("no randomness source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(not(windows))]
pub fn random_hex(_n: usize) -> Result<String, String> {
    Err("password generation runs on Windows".into())
}
