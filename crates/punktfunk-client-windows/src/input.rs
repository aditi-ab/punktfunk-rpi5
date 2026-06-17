//! Stream input: Win32 low-level keyboard + mouse hooks forwarding to the host while the WinUI
//! window is focused and the pointer is captured.
//!
//! windows-reactor exposes no raw key-down/up or pointer-position/wheel events (only keyboard
//! *accelerators* and pointer button-state), which is insufficient for a game stream. So this
//! drops below XAML to `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, installed on the UI thread when the
//! stream page mounts and removed when it unmounts.
//!
//! **Pointer lock.** While captured the cursor is *locked* the way a game-streaming client locks
//! it (Moonlight/Parsec): the OS cursor is hidden + confined to the window (`ClipCursor`), and
//! every physical move is turned into a **relative** delta (`InputKind::MouseMove`) — we read the
//! offset from the window centre, ship it (scaled screen→host through the Contain-fit factor, with
//! sub-pixel remainder carried so slow drags aren't lost), then warp the cursor back to centre so
//! it never reaches a screen edge. This is why the old absolute path froze: swallowing
//! `WM_MOUSEMOVE` pinned the OS cursor, so `pt` never travelled and the absolute coordinate
//! snapped to one point. Keys carry the native Windows VK directly (the wire contract).
//!
//! **Ctrl+Alt+Shift+Q** toggles capture — releasing the lock hands the cursor back to the local
//! desktop (and re-grabs on the next toggle). Losing foreground also releases the lock so the
//! cursor is never stranded.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::HashSet;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_Q;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, GetClientRect, GetForegroundWindow, SetCursorPos,
    SetWindowsHookExW, ShowCursor, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT,
    LLMHF_INJECTED, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

struct State {
    connector: Arc<NativeClient>,
    mode: Mode,
    /// Our window handle, stored as the raw `isize` so `State` is `Send` (`HWND` is not).
    hwnd: isize,
    /// User intent: forward input to the host (toggled by Ctrl+Alt+Shift+Q).
    captured: bool,
    /// The OS pointer is currently locked (hidden + confined + recentering). Tracks the real
    /// `ClipCursor`/`ShowCursor` state so we engage/disengage exactly once per transition.
    locked: bool,
    /// Lock centre in screen coordinates (the cursor is warped here after every move).
    center_x: i32,
    center_y: i32,
    /// Sub-pixel remainder of the screen→host scale, carried so slow drags aren't truncated away.
    acc_x: f32,
    acc_y: f32,
    /// Modifier state, tracked from the hook's own event stream (see `kbd_proc`).
    ctrl: bool,
    alt: bool,
    shift: bool,
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
    let mut st = State {
        connector,
        mode,
        hwnd: hwnd.0 as isize,
        captured: true,
        locked: false,
        center_x: 0,
        center_y: 0,
        acc_x: 0.0,
        acc_y: 0.0,
        ctrl: false,
        alt: false,
        shift: false,
        held_keys: HashSet::new(),
        held_buttons: HashSet::new(),
    };
    // Lock immediately (the window is foreground at mount, like Moonlight grabbing on stream start).
    set_locked(&mut st, true);
    *STATE.lock().unwrap() = Some(st);
    unsafe {
        let hinst = GetModuleHandleW(None).ok();
        if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd_proc), hinst.map(Into::into), 0) {
            KBD_HOOK.store(h.0 as isize, Ordering::SeqCst);
        }
        if let Ok(h) = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst.map(Into::into), 0) {
            MOUSE_HOOK.store(h.0 as isize, Ordering::SeqCst);
        }
    }
    tracing::info!(
        "stream input hooks installed — pointer locked (Ctrl+Alt+Shift+Q toggles capture)"
    );
}

/// Remove the hooks, release the pointer lock, and flush any held keys/buttons (so nothing
/// sticks down on the host).
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
    if let Some(mut st) = STATE.lock().unwrap().take() {
        set_locked(&mut st, false); // hand the cursor back to the desktop
        flush_held(&mut st);
    }
}

/// Release every held key/button on the host, so nothing sticks down when capture is dropped
/// (toggled off) or the session ends.
fn flush_held(st: &mut State) {
    let c = st.connector.clone();
    for vk in st.held_keys.drain() {
        send(&c, InputKind::KeyUp, vk as u32, 0, 0, 0);
    }
    for b in st.held_buttons.drain() {
        send(&c, InputKind::MouseButtonUp, b, 0, 0, 0);
    }
}

/// Engage or release the pointer lock: confine + hide + recentre on, free + show on off.
/// Guarded so the `ClipCursor`/`ShowCursor` calls stay balanced (one each per transition).
fn set_locked(st: &mut State, on: bool) {
    if on == st.locked {
        return;
    }
    let hwnd = HWND(st.hwnd as *mut _);
    unsafe {
        if on {
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                let mut tl = POINT {
                    x: rc.left,
                    y: rc.top,
                };
                let mut br = POINT {
                    x: rc.right,
                    y: rc.bottom,
                };
                let _ = ClientToScreen(hwnd, &mut tl);
                let _ = ClientToScreen(hwnd, &mut br);
                let clip = RECT {
                    left: tl.x,
                    top: tl.y,
                    right: br.x,
                    bottom: br.y,
                };
                let _ = ClipCursor(Some(&clip as *const RECT));
                st.center_x = (tl.x + br.x) / 2;
                st.center_y = (tl.y + br.y) / 2;
                let _ = SetCursorPos(st.center_x, st.center_y);
            }
            let _ = ShowCursor(false);
            st.acc_x = 0.0;
            st.acc_y = 0.0;
        } else {
            let _ = ClipCursor(None);
            let _ = ShowCursor(true);
        }
    }
    st.locked = on;
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

unsafe extern "system" fn kbd_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        let vk = kb.vkCode as u16;
        let mut guard = STATE.lock().unwrap();
        if let Some(st) = guard.as_mut() {
            // Track modifier state from the hook's own event stream — reliable even while we
            // swallow these keys (GetAsyncKeyState doesn't reflect keys suppressed by our own LL
            // hook, which is why the shortcut never fired). Handles the generic + L/R vk codes.
            match kb.vkCode {
                0x11 | 0xA2 | 0xA3 => st.ctrl = !up,  // (L/R)CONTROL
                0x12 | 0xA4 | 0xA5 => st.alt = !up,   // (L/R)MENU (Alt)
                0x10 | 0xA0 | 0xA1 => st.shift = !up, // (L/R)SHIFT
                _ => {}
            }
            let foreground = unsafe { GetForegroundWindow() }.0 as isize == st.hwnd;
            if foreground {
                // Capture toggle: Ctrl+Alt+Shift+Q (consumed locally, never forwarded).
                if !up && vk == VK_Q.0 && st.ctrl && st.alt && st.shift {
                    let on = !st.captured;
                    st.captured = on;
                    set_locked(st, on); // grab/release the cursor immediately
                    if !on {
                        flush_held(st); // release held keys/buttons so nothing sticks on the host
                    }
                    tracing::info!(captured = on, "capture toggled (Ctrl+Alt+Shift+Q)");
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

/// Client-area size in pixels (for the screen→host relative-motion scale).
fn client_size(hwnd: isize) -> (f32, f32) {
    let mut rc = RECT::default();
    if unsafe { GetClientRect(HWND(hwnd as *mut _), &mut rc) }.is_ok() {
        (
            (rc.right - rc.left).max(1) as f32,
            (rc.bottom - rc.top).max(1) as f32,
        )
    } else {
        (1.0, 1.0)
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let injected = (ms.flags & LLMHF_INJECTED) != 0;
        let mut guard = STATE.lock().unwrap();
        if let Some(st) = guard.as_mut() {
            let foreground = unsafe { GetForegroundWindow() }.0 as isize == st.hwnd;
            let want_lock = st.captured && foreground;
            if want_lock != st.locked {
                set_locked(st, want_lock); // sync to focus changes (e.g. lost foreground)
            }
            if st.locked {
                // Skip the synthetic move our own SetCursorPos recentre generates.
                if injected {
                    return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                }
                let c = st.connector.clone();
                match msg {
                    WM_MOUSEMOVE => {
                        let dx = (ms.pt.x - st.center_x) as f32;
                        let dy = (ms.pt.y - st.center_y) as f32;
                        if dx != 0.0 || dy != 0.0 {
                            // screen px → host px: the Contain-fit display scale's inverse, so the
                            // host cursor tracks the physical mouse 1:1 on screen at any window size.
                            let (ww, wh) = client_size(st.hwnd);
                            let (vw, vh) =
                                (st.mode.width.max(1) as f32, st.mode.height.max(1) as f32);
                            let s = (ww / vw).min(wh / vh).max(0.01);
                            st.acc_x += dx / s;
                            st.acc_y += dy / s;
                            let (hx, hy) = (st.acc_x.trunc() as i32, st.acc_y.trunc() as i32);
                            st.acc_x -= hx as f32;
                            st.acc_y -= hy as f32;
                            if hx != 0 || hy != 0 {
                                send(&c, InputKind::MouseMove, 0, hx, hy, 0);
                            }
                        }
                        let _ = unsafe { SetCursorPos(st.center_x, st.center_y) };
                    }
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
                return LRESULT(1); // swallow inside the locked window
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
