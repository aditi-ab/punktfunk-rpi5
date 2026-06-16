//! Force-composed-flip overlay (Windows) — make the secure (Winlogon: UAC / lock / login) desktop
//! capturable by Desktop Duplication.
//!
//! The secure desktop's dialog/wallpaper presents via **fullscreen independent-flip / MPO**: it scans
//! out directly, bypassing DWM composition, so `IDXGIOutputDuplication::AcquireNextFrame` returns
//! `DXGI_ERROR_ACCESS_LOST` (born-lost) — there is no composed frame to hand out (the client sees
//! black). Independent-flip requires the presenting app to own the ENTIRE output: putting ANY other
//! visible window on that output disqualifies it, forcing DWM to **composite**, which DDA can then
//! capture. So we keep a tiny, click-through, near-invisible **topmost layered window** alive on the
//! *current input desktop* (which is Winlogon while the secure desktop is up). On a desktop switch the
//! window is orphaned, so a dedicated thread tracks the input desktop and recreates it there.
//!
//! This is the non-input alternative to "tap a key to wake the lock screen": it needs no SendInput and
//! no system-wide registry change (it does NOT disable MPO globally — it only nudges OUR output to
//! composed while a session is live). Effectiveness can be build/driver-dependent; gated by
//! `PUNKTFUNK_FORCE_COMPOSED` (default ON; set =0 to disable).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, UOI_NAME,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST,
    LWA_ALPHA, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    WS_POPUP,
};

/// A running force-composed-flip overlay. Drop signals the thread to tear down its window + exit.
pub struct ForceComposedFlip {
    stop: Arc<AtomicBool>,
}

impl ForceComposedFlip {
    /// Start the overlay (no-op + `None` if disabled via `PUNKTFUNK_FORCE_COMPOSED=0`).
    pub fn start() -> Option<Self> {
        if std::env::var("PUNKTFUNK_FORCE_COMPOSED").as_deref() == Ok("0") {
            tracing::info!("force-composed-flip overlay disabled (PUNKTFUNK_FORCE_COMPOSED=0)");
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        std::thread::Builder::new()
            .name("composed-flip".into())
            .spawn(move || unsafe { run(st) })
            .ok()?;
        tracing::info!("force-composed-flip overlay started (Winlogon-aware)");
        Some(ForceComposedFlip { stop })
    }
}

impl Drop for ForceComposedFlip {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Read the current input-desktop name (e.g. "Default" / "Winlogon"); `None` if it can't be read.
unsafe fn input_desktop_name() -> Option<String> {
    let desk = OpenInputDesktop(
        DESKTOP_CONTROL_FLAGS(0),
        false,
        DESKTOP_ACCESS_FLAGS(0x0001),
    )
    .ok()?;
    let mut buf = [0u16; 64];
    let mut needed = 0u32;
    let ok = GetUserObjectInformationW(
        windows::Win32::Foundation::HANDLE(desk.0),
        UOI_NAME,
        Some(buf.as_mut_ptr() as *mut _),
        (buf.len() * 2) as u32,
        Some(&mut needed),
    )
    .is_ok();
    let _ = CloseDesktop(desk);
    if !ok {
        return None;
    }
    Some(
        String::from_utf16_lossy(&buf)
            .trim_end_matches('\u{0}')
            .to_string(),
    )
}

/// Create the tiny topmost layered click-through window on the CURRENT thread's desktop. Caller must
/// have `SetThreadDesktop`'d to the target input desktop first.
unsafe fn make_overlay() -> Option<HWND> {
    let hinst = GetModuleHandleW(None).ok()?;
    let class = w!("PunktfunkComposedFlip");
    // RegisterClassW is idempotent-ish: a second register for the same name fails harmlessly; we
    // ignore the result and rely on the class existing. (One process, so it registers once.)
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: hinst.into(),
        lpszClassName: class,
        ..Default::default()
    };
    let atom = RegisterClassW(&wc);
    if atom == 0 {
        let e = windows::Win32::Foundation::GetLastError();
        // 1410 = ERROR_CLASS_ALREADY_EXISTS is fine (re-register after a desktop switch).
        if e.0 != 1410 {
            tracing::warn!(err = e.0, "force-composed-flip: RegisterClassW failed");
        }
    }
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
        class,
        w!(""),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        Some(hinst.into()),
        None,
    ) {
        Ok(h) => h,
        Err(e) => {
            let le = windows::Win32::Foundation::GetLastError();
            tracing::warn!(err = %format!("{e:?}"), last = le.0,
                "force-composed-flip: CreateWindowExW failed");
            return None;
        }
    };
    // alpha=1: technically visible (so it disqualifies independent-flip) but imperceptible.
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 1, LWA_ALPHA);
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    Some(hwnd)
}

unsafe fn run(stop: Arc<AtomicBool>) {
    let mut cur_desktop: Option<String> = None;
    let mut hwnd: Option<HWND> = None;
    let mut ticks: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        // Follow the input desktop: if it changed (Default↔Winlogon), re-attach this thread and
        // recreate the window there (a window is bound to the desktop it was created on).
        let name = input_desktop_name();
        if name != cur_desktop {
            if let Some(h) = hwnd.take() {
                let _ = DestroyWindow(h);
            }
            if let Ok(desk) = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(0x1000_0000), // GENERIC_ALL (incl. DESKTOP_CREATEWINDOW=0x0002)
            ) {
                if SetThreadDesktop(desk).is_ok() {
                    hwnd = make_overlay();
                    tracing::info!(desktop = ?name, created = hwnd.is_some(),
                        "force-composed-flip: overlay (re)created on input desktop");
                }
                // Leak `desk` while it's the thread desktop (closing the current thread desktop is UB).
            }
            cur_desktop = name;
        }
        // Re-assert topmost periodically (other windows on the secure desktop can push us down) and
        // pump our message queue so the window stays responsive/composited.
        if let Some(h) = hwnd {
            let _ = SetWindowPos(
                h,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(h), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        ticks = ticks.wrapping_add(1);
        let _ = ticks;
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if let Some(h) = hwnd.take() {
        let _ = DestroyWindow(h);
    }
    tracing::info!("force-composed-flip overlay stopped");
}
