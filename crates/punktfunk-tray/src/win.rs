//! Windows tray: a hidden top-level window + `Shell_NotifyIconW`, fed by the status poller.
//!
//! The host service (`PunktfunkHost`, LocalSystem) supervises from session 0 and its `serve`
//! child runs as SYSTEM — neither can own a per-user tray icon, so this is a separate small
//! process the installer puts in the HKLM `Run` key (one instance per interactive session,
//! enforced by a `Local\` mutex). Start/Stop/Restart open one UAC consent prompt each
//! (`ShellExecuteW "runas"` on `punktfunk-host.exe service …`) — service control is deliberately
//! left admin-gated rather than DACL-opened to every local user.

use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, FindWindowW, GetCursorPos, GetMessageW, LoadIconW, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    SetMenuDefaultItem, TrackPopupMenuEx, TranslateMessage, HICON, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, SW_HIDE, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_ENDSESSION, WM_NULL, WNDCLASSW,
    WS_OVERLAPPED,
};

use crate::status::{Poller, TrayStatus};

/// Keyboard "select" on the icon (Enter/Space) — `NIN_SELECT | NINF_KEY`; the windows crate
/// exports only NIN_SELECT.
const NIN_KEYSELECT: u32 = NIN_SELECT | 0x1;

/// Posted by the poller thread when the status changed (never touch TLS on the UI thread).
const WMAPP_STATUS: u32 = WM_APP + 2;
/// The notify-icon callback message (NOTIFYICON_VERSION_4 semantics).
const WMAPP_NOTIFYCALLBACK: u32 = WM_APP + 1;

// Menu command ids (WM_COMMAND LOWORD(wParam)).
const IDM_HEADER: usize = 0x0100; // disabled status line
const IDM_OPEN_WEB: usize = 0x0101;
const IDM_START: usize = 0x0102;
const IDM_STOP: usize = 0x0103;
const IDM_RESTART: usize = 0x0104;
const IDM_LOGS: usize = 0x0105;
const IDM_EXIT: usize = 0x0106;
const IDM_PAIRING: usize = 0x0107;

/// Icon resource ordinals (embedded by build.rs).
fn icon_ordinal(status: &TrayStatus) -> u16 {
    match status {
        TrayStatus::Running(_) if status.is_streaming() => 5,
        TrayStatus::Running(_) => 2,
        TrayStatus::Stopped | TrayStatus::NotInstalled => 3,
        TrayStatus::Error(_) => 4,
        TrayStatus::Starting | TrayStatus::Degraded => 6,
    }
}

/// Global tray state — a tray has exactly one window and one wndproc, which cannot carry a
/// closure environment, so the state lives in a `OnceLock` set before window creation.
struct App {
    hwnd: AtomicIsize,
    status: Mutex<TrayStatus>,
    poller: OnceLock<Poller>,
    /// `TaskbarCreated` broadcast id — Explorer restarted, re-add the icon.
    taskbar_created: u32,
    /// `punktfunk-host.exe` next to this exe (the installer lays both in `{app}`).
    host_exe: Option<std::path::PathBuf>,
    /// The installer bundled the web console (detected via `{app}\web\web-run.cmd`).
    web_console: bool,
    web_port: u16,
}

static APP: OnceLock<App> = OnceLock::new();

fn app() -> &'static App {
    APP.get().expect("APP initialized before window creation")
}

fn to_wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

/// Best-effort log for a windows-subsystem process (no stderr): `%LOCALAPPDATA%\punktfunk\tray.log`.
fn log(msg: &str) {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let dir = std::path::PathBuf::from(base).join("punktfunk");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("tray.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

pub fn run(args: crate::Args) -> anyhow::Result<()> {
    let _ = args.autostart; // Linux-only flag, accepted for a uniform command line
    if args.quit {
        return quit_existing();
    }

    // One tray per session: `Local\` scopes the mutex to this logon session, so fast-user-switched
    // sessions each keep their own icon. Handle deliberately leaked (held for the process life).
    // SAFETY: CreateMutexW with a valid nul-terminated name and no security attributes; the
    // returned handle is never closed (process-lifetime singleton guard).
    let already = unsafe {
        match CreateMutexW(None, false, w!("Local\\PunktfunkTray")) {
            Ok(_) => GetLastError() == ERROR_ALREADY_EXISTS,
            Err(_) => false, // can't tell — carry on rather than losing the icon
        }
    };
    if already {
        return Ok(());
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let host_exe = exe_dir
        .as_ref()
        .map(|d| d.join("punktfunk-host.exe"))
        .filter(|p| p.exists());
    let web_console = exe_dir
        .as_ref()
        .is_some_and(|d| d.join("web").join("web-run.cmd").exists());

    // SAFETY: RegisterWindowMessageW with a static nul-terminated literal.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    APP.set(App {
        hwnd: AtomicIsize::new(0),
        status: Mutex::new(TrayStatus::Stopped),
        poller: OnceLock::new(),
        taskbar_created,
        host_exe,
        web_console,
        web_port: args.web_port,
    })
    .ok()
    .expect("run() is called once");

    // Hidden top-level window (NOT message-only — those never receive the TaskbarCreated
    // broadcast, which is how the icon survives an Explorer restart).
    // SAFETY: standard window-class registration + creation; the class name literal outlives the
    // call, wndproc is a valid extern "system" fn, and the window is created on this thread which
    // then runs the message loop.
    let hwnd = unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: w!("PunktfunkTrayWindow"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            anyhow::bail!("RegisterClassW failed: {:?}", GetLastError());
        }
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("PunktfunkTrayWindow"),
            w!("punktfunk tray"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };
    app().hwnd.store(hwnd.0 as isize, Ordering::SeqCst);

    // First NIM_ADD retried across the logon race (the taskbar may not exist yet at sign-in).
    let mut added = false;
    for _ in 0..10 {
        if update_icon(hwnd, true) {
            added = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if !added {
        log("Shell_NotifyIconW(NIM_ADD) kept failing — no taskbar?");
    }

    // The poller owns all network/SCM I/O; it only posts a message here.
    let poller = Poller::spawn(
        args.mgmt_addr.clone(),
        args.mgmt_port,
        Box::new(move |st| {
            *app().status.lock().unwrap() = st;
            let hwnd = HWND(app().hwnd.load(Ordering::SeqCst) as *mut _);
            // SAFETY: PostMessageW is documented thread-safe; a stale/destroyed hwnd fails
            // harmlessly with an error we ignore.
            unsafe {
                let _ = PostMessageW(Some(hwnd), WMAPP_STATUS, WPARAM(0), LPARAM(0));
            }
        }),
    );
    let _ = app().poller.set(poller);

    // SAFETY: classic message pump on the window's owning thread.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// `--quit`: ask a running instance (this session) to exit — used by the uninstaller before file
/// deletion. High-IL callers may message a medium-IL window (UIPI blocks only low→high).
fn quit_existing() -> anyhow::Result<()> {
    // SAFETY: FindWindowW/PostMessageW on a class-name literal; both fail harmlessly when no
    // instance is running.
    unsafe {
        if let Ok(hwnd) = FindWindowW(w!("PunktfunkTrayWindow"), PCWSTR::null()) {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    Ok(())
}

/// Build/refresh the notify icon from the current status. Returns false when the shell rejected
/// the call (no taskbar yet).
fn update_icon(hwnd: HWND, add: bool) -> bool {
    let status = app().status.lock().unwrap().clone();
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WMAPP_NOTIFYCALLBACK,
        ..Default::default()
    };
    // SAFETY: LoadIconW by ordinal from this exe's embedded resources (build.rs); the ordinal is
    // one of the ids compiled in, and a failure falls back to a null icon rather than UB.
    nid.hIcon = unsafe {
        LoadIconW(
            Some(GetModuleHandleW(None).unwrap_or_default().into()),
            PCWSTR(icon_ordinal(&status) as usize as *const u16),
        )
    }
    .unwrap_or(HICON(std::ptr::null_mut()));
    // Tooltip: truncate to the szTip capacity (127 UTF-16 units + nul).
    let tip = to_wide(&status.headline());
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    // SAFETY: nid is fully initialized with a correct cbSize; NIM_* calls only read it.
    unsafe {
        if add {
            if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                return false;
            }
            let mut v = nid;
            v.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &v);
            true
        } else {
            if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
                // Icon vanished (Explorer crash we missed) — re-add.
                return update_icon(hwnd, true);
            }
            true
        }
    }
}

/// The right-click menu, rebuilt from the live status each time.
fn show_menu(hwnd: HWND) {
    let status = app().status.lock().unwrap().clone();
    let running = matches!(
        status,
        TrayStatus::Running(_) | TrayStatus::Starting | TrayStatus::Degraded
    );
    let startable = matches!(status, TrayStatus::Stopped | TrayStatus::Error(_));
    let can_control = app().host_exe.is_some();

    // SAFETY: menu handle created and destroyed here; AppendMenuW copies the item strings, whose
    // wide buffers outlive each call. TrackPopupMenuEx requires the foreground quirk handled
    // below (SetForegroundWindow before, WM_NULL after) per the Shell_NotifyIcon docs.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let add = |id: usize, text: &str, grayed: bool| {
            let wide = to_wide(text);
            let flags = if grayed {
                MF_STRING | MF_GRAYED
            } else {
                MF_STRING
            };
            let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
        };
        add(IDM_HEADER, &status.headline(), true);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        if app().web_console {
            add(IDM_OPEN_WEB, "Open web console", false);
            let _ = SetMenuDefaultItem(menu, IDM_OPEN_WEB as u32, 0);
            if status.pairing_attention() {
                add(IDM_PAIRING, "Approve pairing request…", false);
            }
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        if can_control {
            if startable {
                add(IDM_START, "Start host", false);
            }
            if running {
                add(IDM_STOP, "Stop host", false);
                add(IDM_RESTART, "Restart host", false);
            } else if matches!(status, TrayStatus::Error(_)) {
                add(IDM_RESTART, "Restart host", false);
            }
        }
        add(IDM_LOGS, "Open logs folder", false);
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        add(IDM_EXIT, "Exit tray", false);

        let mut pt = Default::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenuEx(
            menu,
            (TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            pt.x,
            pt.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// `ShellExecuteW` "open" on a URL / folder.
fn shell_open(hwnd: HWND, target: &str) {
    let wide = to_wide(target);
    // SAFETY: all strings nul-terminated and live across the call.
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("open"),
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// One UAC prompt per service action: relaunch the host exe elevated with `service <verb>`.
/// A declined prompt (ERROR_CANCELLED) is deliberately ignored.
fn elevate_service(hwnd: HWND, verb: &str) {
    let Some(exe) = app().host_exe.as_ref() else {
        return;
    };
    let exe_w = to_wide(&exe.to_string_lossy());
    let params = to_wide(&format!("service {verb}"));
    // SAFETY: nul-terminated strings live across the call; "runas" spawns the elevated child
    // (hidden console — the tray re-polls for the outcome instead of scraping its output).
    unsafe {
        ShellExecuteW(
            Some(hwnd),
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_HIDE,
        );
    }
    if let Some(p) = app().poller.get() {
        p.poke();
    }
}

fn open_web_console(hwnd: HWND) {
    shell_open(hwnd, &format!("https://localhost:{}", app().web_port));
}

fn open_logs(hwnd: HWND) {
    let Some(base) = std::env::var_os("ProgramData") else {
        return;
    };
    let dir = std::path::PathBuf::from(base)
        .join("punktfunk")
        .join("logs");
    shell_open(hwnd, &dir.to_string_lossy());
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let Some(app) = APP.get() else {
        // SAFETY: pass-through for messages arriving before APP is set (CreateWindowExW sends
        // WM_NCCREATE/WM_CREATE synchronously — APP is set before that, but stay defensive).
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WMAPP_STATUS => {
            update_icon(hwnd, false);
            LRESULT(0)
        }
        WMAPP_NOTIFYCALLBACK => {
            // NOTIFYICON_VERSION_4: LOWORD(lParam) is the event.
            match (lparam.0 as u32) & 0xffff {
                WM_CONTEXTMENU => show_menu(hwnd),
                x if x == NIN_SELECT || x == NIN_KEYSELECT => {
                    if app.web_console {
                        open_web_console(hwnd);
                    } else {
                        show_menu(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wparam.0) & 0xffff {
                IDM_OPEN_WEB => open_web_console(hwnd),
                IDM_PAIRING => open_web_console(hwnd),
                IDM_START => elevate_service(hwnd, "start"),
                IDM_STOP => elevate_service(hwnd, "stop"),
                IDM_RESTART => elevate_service(hwnd, "restart"),
                IDM_LOGS => open_logs(hwnd),
                // SAFETY: DestroyWindow on the wndproc's own window/thread.
                IDM_EXIT => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_ENDSESSION => {
            // SAFETY: as above — triggers WM_DESTROY below.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            // SAFETY: minimal, correctly sized nid; NIM_DELETE only reads hWnd/uID.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        m if m == app.taskbar_created => {
            // Explorer restarted — the icon is gone; add it back.
            update_icon(hwnd, true);
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
