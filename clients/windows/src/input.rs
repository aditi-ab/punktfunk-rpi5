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
//! snapped to one point. Keys carry the **US-positional VK** for the pressed physical key (the
//! punktfunk wire contract shared by every first-party client — see [`scan_to_positional_vk`]):
//! the hook's layout-resolved `vkCode` must NOT go on the wire, or a non-US pair re-maps
//! positions through two layouts (German: y↔z swapped, ü lands on ö).
//!
//! **Capture state machine** (parity with the GTK/Swift clients): capture engages at stream
//! start, **Ctrl+Alt+Shift+Q** releases it (handing the cursor back to the local desktop), and a
//! **click on the stream** re-engages it. Losing foreground also releases the lock so the cursor
//! is never stranded; regaining it while still captured re-locks. When "capture system
//! shortcuts" is off in Settings, Alt+Tab / Alt+Esc / Ctrl+Esc / the Win keys act on the local
//! desktop instead of being forwarded. **Ctrl+Alt+Shift+D disconnects** the session (consumed
//! locally, works captured or released while our window is foreground): it trips the session's
//! stop flag, the pump winds down, and the event loop navigates back to the host list.
//! **Ctrl+Alt+Shift+S** toggles the stats overlay live and **F11** toggles fullscreen — both are
//! client-local shortcuts (consumed, never forwarded), matching the GTK client's stream key set.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::Mode;
use punktfunk_core::input::{InputEvent, InputKind};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_D, VK_F11, VK_Q, VK_S};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, EnumChildWindows, GetClientRect, GetForegroundWindow, SetCursor,
    SetCursorPos, SetWindowsHookExW, ShowCursor, UnhookWindowsHookEx, HC_ACTION, HHOOK,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLMHF_INJECTED, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
    WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

struct State {
    connector: Arc<NativeClient>,
    mode: Mode,
    /// The session's stop flag (Ctrl+Alt+Shift+D trips it; the pump then ends the session).
    stop: Arc<AtomicBool>,
    /// Our window handle, stored as the raw `isize` so `State` is `Send` (`HWND` is not).
    hwnd: isize,
    /// User intent: forward input to the host (toggled by Ctrl+Alt+Shift+Q / click-to-capture).
    captured: bool,
    /// Forward system shortcuts (Alt+Tab, Win, …) to the host; off = they act locally.
    inhibit_shortcuts: bool,
    /// The OS pointer is currently locked (hidden + confined + recentering). Tracks the real
    /// `ClipCursor`/`ShowCursor` state so we engage/disengage exactly once per transition.
    locked: bool,
    /// Lock geometry, captured when the lock engages: the confinement rect (screen coordinates,
    /// also the click-to-capture hit test), its centre (the cursor is warped here after every
    /// move), and the screen→host scale (the Contain-fit display scale's inverse). Stable while
    /// locked — the window can't be moved or resized with the cursor confined inside it.
    clip: RECT,
    center_x: i32,
    center_y: i32,
    scale: f32,
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
/// Mirror of `State::captured` for lock-free reads off the UI thread (the HUD poll).
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Live stats-overlay visibility. Seeded from `Settings::show_stats` at `install`, then toggled by
/// Ctrl+Alt+Shift+S for the session (parity with the GTK client's live `s` toggle); the HUD poll
/// reads it lock-free to drive the overlay.
static HUD_VISIBLE: AtomicBool = AtomicBool::new(false);
/// Whether the pointer lock currently wants the OS cursor hidden. Read lock-free by
/// [`cursor_subclass_proc`] (which runs on the UI thread inside `WM_SETCURSOR`) so it can override
/// WinUI's per-move arrow re-assertion — a one-shot `ShowCursor(false)` alone loses that race
/// because the content island re-sets the arrow every time the pointer moves.
static CURSOR_HIDDEN: AtomicBool = AtomicBool::new(false);
/// Our `SetWindowSubclass` id on the WinUI window + its content-island children (any stable value;
/// scopes the subclass so install/remove target exactly our proc).
const CURSOR_SUBCLASS_ID: usize = 0x7066_6375; // 'pfcu'

/// Whether stream input is currently captured (drives the HUD's release/capture hint).
pub fn is_captured() -> bool {
    CAPTURED.load(Ordering::Relaxed)
}

/// Whether the stats overlay should be shown: the Settings default at stream start, then whatever
/// Ctrl+Alt+Shift+S last set for the session. Read by the HUD poll thread.
pub fn hud_visible() -> bool {
    HUD_VISIBLE.load(Ordering::Relaxed)
}

/// Set the capture intent and engage/release the pointer lock to match.
fn set_captured(st: &mut State, on: bool) {
    st.captured = on;
    CAPTURED.store(on, Ordering::Relaxed);
    set_locked(st, on);
    if !on {
        flush_held(st); // release held keys/buttons so nothing sticks on the host
    }
}

/// Install the hooks for a streaming session. Call from the UI thread once the window is shown.
/// `inhibit_shortcuts` forwards system shortcuts (Alt+Tab, Win, …) to the host; off = local.
/// `show_stats` seeds the stats-overlay visibility that Ctrl+Alt+Shift+S then toggles live.
/// `stop` is the session's stop flag, tripped by the disconnect shortcut.
pub fn install(
    connector: Arc<NativeClient>,
    mode: Mode,
    inhibit_shortcuts: bool,
    show_stats: bool,
    stop: Arc<AtomicBool>,
) {
    HUD_VISIBLE.store(show_stats, Ordering::Relaxed);
    let hwnd = unsafe { GetForegroundWindow() };
    let mut st = State {
        connector,
        mode,
        stop,
        hwnd: hwnd.0 as isize,
        captured: false,
        inhibit_shortcuts,
        locked: false,
        clip: RECT::default(),
        center_x: 0,
        center_y: 0,
        scale: 1.0,
        acc_x: 0.0,
        acc_y: 0.0,
        ctrl: false,
        alt: false,
        shift: false,
        held_keys: HashSet::new(),
        held_buttons: HashSet::new(),
    };
    // Capture immediately (the window is foreground at mount, like Moonlight grabbing on stream
    // start).
    set_captured(&mut st, true);
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
        inhibit_shortcuts,
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
        // Hand the cursor back + flush held state.
        set_captured(&mut st, false);
        // Drop the WM_SETCURSOR subclass so the long-lived app window (reused for the host list
        // once the stream ends) is left pristine — set_captured already cleared CURSOR_HIDDEN.
        remove_cursor_subclass(HWND(st.hwnd as *mut _));
        // Fullscreen is a streaming-only mode: if F11 put us there, drop back to a normal window
        // so the GUI (the host list) is never left borderless-fullscreen after the stream ends.
        exit_fullscreen(HWND(st.hwnd as *mut _));
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

/// Subclass proc on the WinUI window + its content-island children: while the pointer lock wants
/// the cursor hidden ([`CURSOR_HIDDEN`]), answer `WM_SETCURSOR` ourselves with `SetCursor(None)`
/// and return TRUE — halting WinUI's default handling before it re-asserts the arrow. This is what
/// actually keeps the cursor hidden while captured; the sibling `ShowCursor(false)` cannot, because
/// WinUI re-sets the arrow on every pointer move (the content island answers `WM_SETCURSOR` itself,
/// which a low-level mouse hook never sees). When not hidden, we defer to the chain untouched.
unsafe extern "system" fn cursor_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _ref: usize,
) -> LRESULT {
    if msg == WM_SETCURSOR && CURSOR_HIDDEN.load(Ordering::Relaxed) {
        unsafe {
            let _ = SetCursor(None);
        }
        return LRESULT(1); // handled — suppress the framework's arrow re-assertion
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

unsafe extern "system" fn subclass_install_cb(child: HWND, _l: LPARAM) -> BOOL {
    unsafe {
        let _ = SetWindowSubclass(child, Some(cursor_subclass_proc), CURSOR_SUBCLASS_ID, 0);
    }
    BOOL(1) // keep enumerating
}

unsafe extern "system" fn subclass_remove_cb(child: HWND, _l: LPARAM) -> BOOL {
    unsafe {
        let _ = RemoveWindowSubclass(child, Some(cursor_subclass_proc), CURSOR_SUBCLASS_ID);
    }
    BOOL(1)
}

/// Install [`cursor_subclass_proc`] on the top-level WinUI window and every descendant — the video
/// is a composition SwapChainPanel, so the pointer actually sits over WinUI's internal content-
/// island child window, which is the window that receives `WM_SETCURSOR`. `EnumChildWindows`
/// recurses into all descendants, so one pass covers it. Idempotent (re-installing the same
/// id+proc just refreshes it), so it's safe to call on every lock engage.
fn install_cursor_subclass(top: HWND) {
    unsafe {
        let _ = SetWindowSubclass(top, Some(cursor_subclass_proc), CURSOR_SUBCLASS_ID, 0);
        let _ = EnumChildWindows(Some(top), Some(subclass_install_cb), LPARAM(0));
    }
}

/// Remove our subclass from the top-level window and every descendant. Called on teardown so the
/// long-lived app window (reused for the host list after the stream ends) is left pristine.
fn remove_cursor_subclass(top: HWND) {
    unsafe {
        let _ = RemoveWindowSubclass(top, Some(cursor_subclass_proc), CURSOR_SUBCLASS_ID);
        let _ = EnumChildWindows(Some(top), Some(subclass_remove_cb), LPARAM(0));
    }
}

/// Engage or release the pointer lock: confine + hide + recentre on, free + show on off.
/// Guarded so the `ClipCursor`/`ShowCursor` calls stay balanced (one each per transition).
/// Engaging captures the lock geometry (rect, centre, screen→host scale) — see `State::clip`.
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
                st.clip = RECT {
                    left: tl.x,
                    top: tl.y,
                    right: br.x,
                    bottom: br.y,
                };
                let _ = ClipCursor(Some(&st.clip as *const RECT));
                st.center_x = (tl.x + br.x) / 2;
                st.center_y = (tl.y + br.y) / 2;
                // Screen px → host px: the Contain-fit display scale's inverse, so the host
                // cursor tracks the physical mouse 1:1 on screen at any window size.
                let (ww, wh) = ((br.x - tl.x).max(1) as f32, (br.y - tl.y).max(1) as f32);
                let (vw, vh) = (st.mode.width.max(1) as f32, st.mode.height.max(1) as f32);
                st.scale = (ww / vw).min(wh / vh).max(0.01);
                let _ = SetCursorPos(st.center_x, st.center_y);
            }
            // Hide the OS cursor. ShowCursor(false) is the coarse gate; the subclass is what
            // actually holds it hidden against WinUI's per-move arrow re-assertion — see
            // cursor_subclass_proc / install_cursor_subclass.
            let _ = ShowCursor(false);
            CURSOR_HIDDEN.store(true, Ordering::Relaxed);
            install_cursor_subclass(hwnd);
            st.acc_x = 0.0;
            st.acc_y = 0.0;
        } else {
            CURSOR_HIDDEN.store(false, Ordering::Relaxed);
            let _ = ClipCursor(None);
            let _ = ShowCursor(true);
        }
    }
    st.locked = on;
}

/// The pre-fullscreen window placement, saved on entering fullscreen and restored on leaving it.
/// Module-level (not a `toggle_fullscreen`-local static) so the F11 toggle and the stream-stop exit
/// ([`uninstall`]) share the one saved placement, and its presence is also the "are we fullscreen?"
/// flag for [`exit_fullscreen`]. Only ever touched on the UI thread (the hook proc / the stream
/// page's unmount), but a Mutex keeps the static sound + `Sync`.
static SAVED_PLACEMENT: Mutex<Option<windows::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT>> =
    Mutex::new(None);

/// Whether our top-level window is currently borderless-fullscreen. Entering strips
/// `WS_OVERLAPPEDWINDOW`, so its absence is the flag — no extra state beyond [`SAVED_PLACEMENT`].
fn is_fullscreen(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_STYLE, WS_OVERLAPPEDWINDOW,
    };
    let overlapped = WS_OVERLAPPEDWINDOW.0 as isize;
    unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) & overlapped == 0 }
}

/// Enter borderless fullscreen: remember the window placement, drop the frame
/// (`WS_OVERLAPPEDWINDOW`), and size the window to cover the whole monitor. windows-reactor owns
/// the WinUI window but exposes no fullscreen API, so we drive the HWND directly (parity with the
/// GTK client's F11). The SwapChainPanel follows the resulting `WM_SIZE` like any window resize.
fn enter_fullscreen(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowPlacement, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
        SWP_FRAMECHANGED, SWP_NOOWNERZORDER, SWP_NOZORDER, WINDOWPLACEMENT, WS_OVERLAPPEDWINDOW,
    };
    let overlapped = WS_OVERLAPPEDWINDOW.0 as isize;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        if GetWindowPlacement(hwnd, &mut wp).is_ok() && GetMonitorInfoW(mon, &mut mi).as_bool() {
            *SAVED_PLACEMENT.lock().unwrap() = Some(wp);
            SetWindowLongPtrW(hwnd, GWL_STYLE, style & !overlapped);
            let r = mi.rcMonitor;
            let _ = SetWindowPos(
                hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
            );
        }
    }
}

/// Leave borderless fullscreen: restore the frame style and the saved placement. A no-op when we
/// aren't fullscreen (nothing saved), so it's safe to call unconditionally on stream stop.
fn exit_fullscreen(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, GWL_STYLE,
        SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
        WS_OVERLAPPEDWINDOW,
    };
    let Some(wp) = SAVED_PLACEMENT.lock().unwrap().take() else {
        return; // never went fullscreen — nothing to restore
    };
    let overlapped = WS_OVERLAPPEDWINDOW.0 as isize;
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetWindowLongPtrW(hwnd, GWL_STYLE, style | overlapped);
        let _ = SetWindowPlacement(hwnd, &wp);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
        );
    }
}

/// Toggle borderless fullscreen for our top-level window (F11), the classic Win32 dance split into
/// [`enter_fullscreen`] / [`exit_fullscreen`] so the stream-stop path can force windowed too.
fn toggle_fullscreen(hwnd: isize) {
    let hwnd = HWND(hwnd as *mut _);
    if is_fullscreen(hwnd) {
        exit_fullscreen(hwnd);
    } else {
        enter_fullscreen(hwnd);
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

/// System shortcuts that act on the LOCAL desktop when "capture system shortcuts" is off:
/// the Win keys, Alt+Tab, and Alt/Ctrl+Esc.
fn is_system_shortcut(st: &State, vk: u16) -> bool {
    match vk {
        0x5B | 0x5C => true,       // L/R Win
        0x09 => st.alt,            // Alt+Tab
        0x1B => st.alt || st.ctrl, // Alt+Esc / Ctrl+Esc
        _ => false,
    }
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
                    set_captured(st, on);
                    tracing::info!(captured = on, "capture toggled (Ctrl+Alt+Shift+Q)");
                    return LRESULT(1);
                }
                // Disconnect: Ctrl+Alt+Shift+D (consumed locally). Release capture immediately so
                // the cursor is free while the session winds down and the UI navigates home.
                if !up && vk == VK_D.0 && st.ctrl && st.alt && st.shift {
                    set_captured(st, false);
                    // Deliberate user exit → close with QUIT_CLOSE_CODE so the host tears the session
                    // down immediately instead of holding the keep-alive linger for a reconnect.
                    st.connector.disconnect_quit();
                    st.stop.store(true, Ordering::SeqCst);
                    tracing::info!("disconnect requested (Ctrl+Alt+Shift+D)");
                    return LRESULT(1);
                }
                // Toggle the stats overlay: Ctrl+Alt+Shift+S (consumed locally). Seeded from
                // Settings at install; this live toggle overrides it for the session — parity
                // with the GTK client, where `s` flips the OSD without leaving the stream.
                if !up && vk == VK_S.0 && st.ctrl && st.alt && st.shift {
                    let on = !HUD_VISIBLE.load(Ordering::Relaxed);
                    HUD_VISIBLE.store(on, Ordering::Relaxed);
                    tracing::info!(hud = on, "stats overlay toggled (Ctrl+Alt+Shift+S)");
                    return LRESULT(1);
                }
                // Toggle fullscreen: F11 (consumed locally, no modifiers — a client shortcut,
                // never a wire key). Works captured or released. The window resize changes the
                // client rect, so re-lock to recompute the pointer confinement + recentre.
                if !up && vk == VK_F11.0 {
                    toggle_fullscreen(st.hwnd);
                    if st.locked {
                        set_locked(st, false);
                        set_locked(st, true);
                    }
                    tracing::info!("fullscreen toggled (F11)");
                    return LRESULT(1);
                }
                if st.captured {
                    // With shortcut capture off, hand Alt+Tab & co. to the local desktop —
                    // neither forwarded nor swallowed.
                    if !st.inhibit_shortcuts && is_system_shortcut(st, vk) {
                        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
                    }
                    // Wire key: the US-positional VK for this physical key (module docs), derived
                    // from the scancode. `vkCode` is layout-semantic and only passes through for
                    // keys the table doesn't cover — extended keys and everything outside the
                    // typing area, where positional == semantic (plus injected events with
                    // scanCode 0 from remapping tools, best-effort).
                    let ext = (kb.flags.0 & LLKHF_EXTENDED.0) != 0;
                    let v = if ext {
                        vk as u8
                    } else {
                        scan_to_positional_vk(kb.scanCode as u16).unwrap_or(vk as u8)
                    };
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

/// Whether a screen point lies inside the window's CURRENT client area (the click-to-capture
/// hit test — computed fresh per click, since the window can move/resize while released).
fn in_client_area(hwnd: isize, pt: POINT) -> bool {
    let hwnd = HWND(hwnd as *mut _);
    let mut rc = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rc) }.is_err() {
        return false;
    }
    let mut tl = POINT {
        x: rc.left,
        y: rc.top,
    };
    let mut br = POINT {
        x: rc.right,
        y: rc.bottom,
    };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut tl);
        let _ = ClientToScreen(hwnd, &mut br);
    }
    pt.x >= tl.x && pt.x < br.x && pt.y >= tl.y && pt.y < br.y
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
            // Click-to-capture: after a Ctrl+Alt+Shift+Q release, a primary click on the stream
            // re-engages capture. The click is consumed — it starts the grab, it isn't gameplay.
            if !st.captured
                && foreground
                && msg == WM_LBUTTONDOWN
                && !injected
                && in_client_area(st.hwnd, ms.pt)
            {
                set_captured(st, true);
                tracing::info!("capture re-engaged (click on stream)");
                return LRESULT(1);
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
                            st.acc_x += dx / st.scale;
                            st.acc_y += dy / st.scale;
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

/// Set-1 make scancode → US-positional VK for the layout-**variant** typing area (letters, digit
/// row, OEM punctuation, the ISO 102nd key) — the exact inverse of the host injector's positional
/// table and the Windows analogue of the Linux client's `evdev_to_vk`. Keys not listed (F-row,
/// nav cluster, numpad, modifiers — plus every E0-extended key, which the caller filters out)
/// have layout-invariant VKs, so the hook's `vkCode` is already correct for them.
fn scan_to_positional_vk(scan: u16) -> Option<u8> {
    Some(match scan {
        0x02..=0x0A => (scan - 0x02) as u8 + 0x31, // 1..9
        0x0B => 0x30,                              // 0
        0x0C => 0xBD,                              // -_  VK_OEM_MINUS (DE: ß)
        0x0D => 0xBB,                              // =+  VK_OEM_PLUS
        0x10 => 0x51,                              // Q
        0x11 => 0x57,                              // W
        0x12 => 0x45,                              // E
        0x13 => 0x52,                              // R
        0x14 => 0x54,                              // T
        0x15 => 0x59,                              // Y position (QWERTZ: the Z key)
        0x16 => 0x55,                              // U
        0x17 => 0x49,                              // I
        0x18 => 0x4F,                              // O
        0x19 => 0x50,                              // P
        0x1A => 0xDB,                              // [{  VK_OEM_4 (DE: ü)
        0x1B => 0xDD,                              // ]}  VK_OEM_6
        0x1E => 0x41,                              // A
        0x1F => 0x53,                              // S
        0x20 => 0x44,                              // D
        0x21 => 0x46,                              // F
        0x22 => 0x47,                              // G
        0x23 => 0x48,                              // H
        0x24 => 0x4A,                              // J
        0x25 => 0x4B,                              // K
        0x26 => 0x4C,                              // L
        0x27 => 0xBA,                              // ;:  VK_OEM_1 (DE: ö)
        0x28 => 0xDE,                              // '"  VK_OEM_7 (DE: ä)
        0x29 => 0xC0,                              // `~  VK_OEM_3 (DE: ^)
        0x2B => 0xDC,                              // \|  VK_OEM_5
        0x2C => 0x5A,                              // Z position (QWERTZ: the Y key)
        0x2D => 0x58,                              // X
        0x2E => 0x43,                              // C
        0x2F => 0x56,                              // V
        0x30 => 0x42,                              // B
        0x31 => 0x4E,                              // N
        0x32 => 0x4D,                              // M
        0x33 => 0xBC,                              // ,<  VK_OEM_COMMA
        0x34 => 0xBE,                              // .>  VK_OEM_PERIOD
        0x35 => 0xBF,                              // /?  VK_OEM_2
        0x56 => 0xE2,                              // <>| VK_OEM_102 (ISO)
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The German-scramble regression pins: the physical keys a QWERTZ board labels Z/Y/ö/ü must
    /// leave this client as their US-position VKs, regardless of the local layout's vkCode.
    #[test]
    fn positional_pins_for_the_qwertz_scramble() {
        assert_eq!(scan_to_positional_vk(0x15), Some(0x59)); // QWERTZ Z key → VK_Y (US position)
        assert_eq!(scan_to_positional_vk(0x2C), Some(0x5A)); // QWERTZ Y key → VK_Z (US position)
        assert_eq!(scan_to_positional_vk(0x27), Some(0xBA)); // ö key → VK_OEM_1 (US ;: position)
        assert_eq!(scan_to_positional_vk(0x1A), Some(0xDB)); // ü key → VK_OEM_4 (US [{ position)
        assert_eq!(scan_to_positional_vk(0x28), Some(0xDE)); // ä key → VK_OEM_7 (US '" position)
        assert_eq!(scan_to_positional_vk(0x0C), Some(0xBD)); // ß key → VK_OEM_MINUS (US -_ position)
    }

    /// Keys outside the layout-variant typing area stay un-mapped (vkCode passes through).
    #[test]
    fn invariant_keys_fall_through() {
        for scan in [
            0x01u16, 0x0E, 0x0F, 0x1C, 0x1D, 0x2A, 0x36, 0x38, 0x39, 0x3B, 0x45, 0x57,
        ] {
            assert_eq!(scan_to_positional_vk(scan), None, "scan 0x{scan:02X}");
        }
    }

    /// Exactly the 48 typing-area keys are covered (10 digits + 26 letters + 12 OEM), and every
    /// mapping is unique — two physical keys must never collapse onto one wire VK.
    #[test]
    fn table_covers_the_typing_area_bijectively() {
        let mapped: Vec<(u16, u8)> = (0u16..=0xFF)
            .filter_map(|sc| scan_to_positional_vk(sc).map(|vk| (sc, vk)))
            .collect();
        assert_eq!(mapped.len(), 48);
        let mut vks: Vec<u8> = mapped.iter().map(|&(_, vk)| vk).collect();
        vks.sort_unstable();
        vks.dedup();
        assert_eq!(vks.len(), 48, "duplicate wire VK in the positional table");
    }
}
