//! Stream input: Win32 low-level keyboard + mouse hooks forwarding to the host while the WinUI
//! window is focused and capture is engaged.
//!
//! windows-reactor exposes no raw key-down/up or pointer-position/wheel events (only keyboard
//! *accelerators* and pointer button-state), which is insufficient for a game stream. So this
//! drops below XAML to `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, installed on the UI thread when the
//! stream page mounts and removed when it unmounts. The `SwapChainPanel` fills the window, so the
//! pointer maps through the window's client rect (Contain-fit into the negotiated mode), and
//! keys carry the native Windows VK directly (the wire contract). While captured, events inside
//! the video area are swallowed (so Alt+Tab / Win etc. reach the host); Ctrl+Alt+Shift+Q toggles
//! capture; clicks outside the client area (the title bar) pass through so the window stays usable.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::HashSet;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_Q, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClientRect, GetForegroundWindow, SetWindowsHookExW, UnhookWindowsHookEx,
    HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYUP,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

struct State {
    connector: Arc<NativeClient>,
    mode: Mode,
    /// Our window handle, stored as the raw `isize` so `State` is `Send` (`HWND` is not).
    hwnd: isize,
    captured: bool,
    held_keys: HashSet<u8>,
    held_buttons: HashSet<u32>,
}

// `State` carries no `!Send` handle (hwnd is an `isize`), so the static is sound. The hook procs
// run on the same UI thread that installs/removes the hooks, so the lock is uncontended.
static STATE: Mutex<Option<State>> = Mutex::new(None);
static KBD_HOOK: AtomicIsize = AtomicIsize::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);

/// Install the hooks for a streaming session. Call from the UI thread once the window is shown.
pub fn install(connector: Arc<NativeClient>, mode: Mode) {
    let hwnd = unsafe { GetForegroundWindow() };
    *STATE.lock().unwrap() = Some(State {
        connector,
        mode,
        hwnd: hwnd.0 as isize,
        captured: true,
        held_keys: HashSet::new(),
        held_buttons: HashSet::new(),
    });
    unsafe {
        let hinst = GetModuleHandleW(None).ok();
        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), hinst.map(Into::into), 0) {
            KBD_HOOK.store(h.0 as isize, Ordering::SeqCst);
        }
        if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst.map(Into::into), 0) {
            MOUSE_HOOK.store(h.0 as isize, Ordering::SeqCst);
        }
    }
    tracing::info!("stream input hooks installed (Ctrl+Alt+Shift+Q toggles capture)");
}

/// Remove the hooks and flush any held keys/buttons (so nothing sticks down on the host).
pub fn uninstall() {
    unsafe {
        let k = KBD_HOOK.swap(0, Ordering::SeqCst);
        if k != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(k as *mut _));
        }
        let m = MOUSE_HOOK.swap(0, Ordering::SeqCst);
        if m != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(m as *mut _));
        }
    }
    if let Some(st) = STATE.lock().unwrap().take() {
        for vk in &st.held_keys {
            send(&st.connector, InputKind::KeyUp, *vk as u32, 0, 0, 0);
        }
        for b in &st.held_buttons {
            send(&st.connector, InputKind::MouseButtonUp, *b, 0, 0, 0);
        }
    }
}

fn send(c: &NativeClient, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    let _ = c.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    (unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000) != 0
}

unsafe extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        let vk = kb.vkCode as u16;
        let mut guard = STATE.lock().unwrap();
        if let Some(st) = guard.as_mut() {
            let foreground = unsafe { GetForegroundWindow() }.0 as isize == st.hwnd;
            if foreground {
                // Capture toggle: Ctrl+Alt+Shift+Q (consumed locally, never forwarded).
                if !up
                    && vk == VK_Q.0
                    && key_down(VK_CONTROL)
                    && key_down(VK_MENU)
                    && key_down(VK_SHIFT)
                {
                    st.captured = !st.captured;
                    return LRESULT(1);
                }
                if st.captured {
                    let v = vk as u8;
                    if up {
                        if st.held_keys.remove(&v) {
                            send(&st.connector, InputKind::KeyUp, v as u32, 0, 0, 0);
                        }
                    } else {
                        st.held_keys.insert(v);
                        send(&st.connector, InputKind::KeyDown, v as u32, 0, 0, 0);
                    }
                    return LRESULT(1); // swallow so it reaches the host, not the local OS
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Map a screen point to video pixels through the client-rect Contain-fit letterbox. Returns
/// `None` when the point is outside the video area (so the title bar / borders stay interactive).
fn map_abs(st: &State, screen: POINT) -> Option<(i32, i32, u32)> {
    let hwnd = HWND(st.hwnd as *mut _);
    let mut p = screen;
    unsafe {
        let _ = ScreenToClient(hwnd, &mut p);
    }
    let mut rc = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
        return None;
    }
    let (ww, wh) = (
        (rc.right - rc.left).max(1) as f64,
        (rc.bottom - rc.top).max(1) as f64,
    );
    if (p.x as f64) < 0.0 || (p.y as f64) < 0.0 || p.x as f64 > ww || p.y as f64 > wh {
        return None;
    }
    let (vw, vh) = (st.mode.width.max(1) as f64, st.mode.height.max(1) as f64);
    let scale = (ww / vw).min(wh / vh);
    let (ox, oy) = ((ww - vw * scale) / 2.0, (wh - vh * scale) / 2.0);
    let px = (((p.x as f64 - ox) / scale).round()).clamp(0.0, vw - 1.0) as i32;
    let py = (((p.y as f64 - oy) / scale).round()).clamp(0.0, vh - 1.0) as i32;
    let flags = (st.mode.width << 16) | (st.mode.height & 0xffff);
    Some((px, py, flags))
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let mut guard = STATE.lock().unwrap();
        if let Some(st) = guard.as_mut() {
            let foreground = unsafe { GetForegroundWindow() }.0 as isize == st.hwnd;
            if st.captured && foreground {
                let Some((px, py, flags)) = map_abs(st, ms.pt) else {
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                };
                let c = st.connector.clone();
                match msg {
                    WM_MOUSEMOVE => send(&c, InputKind::MouseMoveAbs, 0, px, py, flags),
                    WM_LBUTTONDOWN => button(st, 1, true),
                    WM_LBUTTONUP => button(st, 1, false),
                    WM_RBUTTONDOWN => button(st, 3, true),
                    WM_RBUTTONUP => button(st, 3, false),
                    WM_MBUTTONDOWN => button(st, 2, true),
                    WM_MBUTTONUP => button(st, 2, false),
                    WM_XBUTTONDOWN => button(st, 3 + ((ms.mouseData >> 16) as u16 as u32), true),
                    WM_XBUTTONUP => button(st, 3 + ((ms.mouseData >> 16) as u16 as u32), false),
                    WM_MOUSEWHEEL => send(
                        &c,
                        InputKind::MouseScroll,
                        0,
                        (ms.mouseData >> 16) as i16 as i32,
                        0,
                        0,
                    ),
                    WM_MOUSEHWHEEL => send(
                        &c,
                        InputKind::MouseScroll,
                        1,
                        (ms.mouseData >> 16) as i16 as i32,
                        0,
                        0,
                    ),
                    _ => {}
                }
                return LRESULT(1); // swallow inside the video area
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn button(st: &mut State, id: u32, down: bool) {
    let c = st.connector.clone();
    if down {
        st.held_buttons.insert(id);
        send(&c, InputKind::MouseButtonDown, id, 0, 0, 0);
    } else if st.held_buttons.remove(&id) {
        send(&c, InputKind::MouseButtonUp, id, 0, 0, 0);
    }
}
