//! Windows input injection via `SendInput` (Win32 KeyboardAndMouse) — the Windows analogue of
//! [`super::wlr`]: absolute mouse normalized to the virtual desktop, relative mouse for games,
//! scancode keyboard, scroll, buttons. The client already sends Windows VK codes, so there is no
//! keycode table. Survives UAC/lock desktop switches with Sunshine's retry-on-failure model: the
//! thread stays bound to its desktop and only reattaches (`OpenInputDesktop`/`SetThreadDesktop`) when
//! `SendInput` reports a short write (the input desktop switched) — no per-event reattach overhead.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};
use std::mem::size_of;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    HDESK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    MapVirtualKeyExW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::InputInjector;

const ABS_MAX: f64 = 65535.0; // SendInput absolute coords are 0..65535 over the chosen surface.
const GENERIC_ALL: u32 = 0x1000_0000;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

pub struct SendInputInjector {
    desktop: Option<HDESK>,
}

// SAFETY: `SendInputInjector` holds only an `Option<HDESK>` (a desktop handle). The host creates
// and drives it from a single dedicated injector thread; the handle is opened, rebound, and closed
// on whichever thread owns the value, and the type is not `Sync`, so there is never concurrent
// access. A desktop `HDESK` is not thread-affine for ownership (`CloseDesktop` works from any
// thread; `SetThreadDesktop` rebinds the current thread), so transferring ownership via `Send` is
// sound.
unsafe impl Send for SendInputInjector {}

impl SendInputInjector {
    pub fn open() -> Result<Self> {
        let mut me = Self { desktop: None };
        me.reattach_input_desktop(); // best-effort
        tracing::info!("SendInput injector ready (Win32 KeyboardAndMouse)");
        Ok(me)
    }

    /// Bind this thread to the desktop currently receiving input. UAC / lock screen / Ctrl-Alt-Del
    /// swap the input desktop; `SendInput` silently no-ops unless our thread is on it.
    fn reattach_input_desktop(&mut self) {
        // SAFETY: `OpenInputDesktop`/`SetThreadDesktop`/`CloseDesktop` are FFI calls passed only
        // by-value args (constant desktop flags, a `bool`, an access mask). `OpenInputDesktop`
        // yields an owned `HDESK` only on `Ok`; we then either install it with `SetThreadDesktop`
        // (closing the previously-owned handle exactly once) or close the fresh handle on failure —
        // so every handle is closed exactly once and none is used after close. `SetThreadDesktop`
        // only rebinds this calling thread, which is where the injector runs.
        unsafe {
            match OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            ) {
                Ok(h) => {
                    if SetThreadDesktop(h).is_ok() {
                        if let Some(old) = self.desktop.replace(h) {
                            let _ = CloseDesktop(old);
                        }
                    } else {
                        let _ = CloseDesktop(h);
                    }
                }
                Err(_) => { /* not privileged enough for the secure desktop; stay put */ }
            }
        }
    }

    /// Inject with Sunshine's retry-on-failure model: the thread stays bound to whatever desktop it
    /// last attached to (no per-event `OpenInputDesktop`/`SetThreadDesktop` — two syscalls saved on
    /// every mouse move), and only when `SendInput` reports a short write (0 = the input desktop
    /// switched out from under us, e.g. into UAC/lock) do we reattach to the now-current input desktop
    /// and retry once. This serves both the normal and secure desktops with no steady-state overhead.
    fn send(&mut self, inputs: &[INPUT]) -> Result<()> {
        // SAFETY: `inputs` is a live `&[INPUT]` slice that outlives this synchronous `SendInput`
        // call; `size_of::<INPUT>()` is the exact per-element stride Win32 requires as `cbSize`. The
        // call only reads the array (one event per element) and returns the count injected.
        let n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if n as usize == inputs.len() {
            return Ok(());
        }
        // Short write → the input desktop likely changed. Reattach + retry once.
        self.reattach_input_desktop();
        // SAFETY: same as the first `SendInput` — `inputs` is the identical live slice outliving the
        // call and `cbSize == size_of::<INPUT>()`; only re-issued after reattaching the input desktop.
        let n = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        if n as usize != inputs.len() {
            anyhow::bail!(
                "SendInput injected {n}/{} events (blocked desktop?)",
                inputs.len()
            );
        }
        Ok(())
    }
}

impl Drop for SendInputInjector {
    fn drop(&mut self) {
        if let Some(h) = self.desktop.take() {
            // SAFETY: `h` is the `HDESK` this injector owned (moved out of `self.desktop`);
            // `CloseDesktop` runs once here in `Drop` on that still-valid handle, with no later use —
            // no double close.
            unsafe {
                let _ = CloseDesktop(h);
            }
        }
    }
}

impl InputInjector for SendInputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        // No per-event desktop reattach — `send` reattaches lazily only on a short write (desktop
        // switch). The injector is bound to the input desktop at open() and follows switches on demand.
        match event.kind {
            InputKind::MouseMove => {
                let mi = MOUSEINPUT {
                    dx: event.x,
                    dy: event.y,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send(&[mouse(mi)])
            }
            InputKind::MouseMoveAbs => {
                let w = (event.flags >> 16) & 0xffff;
                let h = event.flags & 0xffff;
                if w == 0 || h == 0 {
                    return Ok(()); // contract: drop zero extent
                }
                let (_vx, _vy, vw, vh) = virtual_desktop_rect();
                // One virtual output spanning the virtual desktop: map client (0..w,0..h) -> 0..65535.
                let cx = (event.x.clamp(0, w as i32)) as f64 / w as f64;
                let cy = (event.y.clamp(0, h as i32)) as f64 / h as f64;
                let ax = (cx * ABS_MAX).round() as i32;
                let ay = (cy * ABS_MAX).round() as i32;
                let _ = (vw, vh); // virtual-desktop rect reserved for multi-output mapping
                let mi = MOUSEINPUT {
                    dx: ax,
                    dy: ay,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send(&[mouse(mi)])
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                let down = event.kind == InputKind::MouseButtonDown;
                let (flag, data) = match event.code {
                    1 => (
                        if down {
                            MOUSEEVENTF_LEFTDOWN
                        } else {
                            MOUSEEVENTF_LEFTUP
                        },
                        0u32,
                    ),
                    2 => (
                        if down {
                            MOUSEEVENTF_MIDDLEDOWN
                        } else {
                            MOUSEEVENTF_MIDDLEUP
                        },
                        0,
                    ),
                    3 => (
                        if down {
                            MOUSEEVENTF_RIGHTDOWN
                        } else {
                            MOUSEEVENTF_RIGHTUP
                        },
                        0,
                    ),
                    4 => (
                        if down {
                            MOUSEEVENTF_XDOWN
                        } else {
                            MOUSEEVENTF_XUP
                        },
                        XBUTTON1,
                    ),
                    5 => (
                        if down {
                            MOUSEEVENTF_XDOWN
                        } else {
                            MOUSEEVENTF_XUP
                        },
                        XBUTTON2,
                    ),
                    _ => return Ok(()),
                };
                let mi = MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send(&[mouse(mi)])
            }
            InputKind::MouseScroll => {
                // GameStream WHEEL_DELTA(120) units. Windows WHEEL positive=up (matches GameStream —
                // no flip, unlike Wayland); HWHEEL positive=right (matches). x is 120-scaled already.
                let horizontal = event.code == 1;
                let mi = MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: event.x as u32, // signed wheel delta reinterpreted as DWORD
                    dwFlags: if horizontal {
                        MOUSEEVENTF_HWHEEL
                    } else {
                        MOUSEEVENTF_WHEEL
                    },
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send(&[mouse(mi)])
            }
            InputKind::KeyDown | InputKind::KeyUp => {
                let down = event.kind == InputKind::KeyDown;
                let vk = (event.code & 0xff) as u16; // client sends Windows VK
                // SAFETY: `MapVirtualKeyExW` is a pure value translation (VK → scancode); all three
                // args are by-value (`u32`, the `MAPVK_VK_TO_VSC_EX` map-type constant, a `None`
                // HKL). It dereferences no pointer and returns a `u32` — FFI-`unsafe` only.
                let sc_ex = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, None) };
                if sc_ex == 0 {
                    return Ok(()); // unmappable -> drop
                }
                let extended = (sc_ex & 0xe000) == 0xe000 || forced_extended(vk);
                let scan = (sc_ex & 0xff) as u16;
                let mut flags = KEYEVENTF_SCANCODE;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                let ki = KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send(&[key(ki)])
            }
            // Gamepad goes through ViGEm (separate backend). Touch: no SendInput equivalent -> no-op.
            InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::TouchDown
            | InputKind::TouchMove
            | InputKind::TouchUp => Ok(()),
        }
    }
}

fn mouse(mi: MOUSEINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    }
}

fn key(ki: KEYBDINPUT) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

fn virtual_desktop_rect() -> (i32, i32, i32, i32) {
    // SAFETY: each `GetSystemMetrics` takes a single by-value `SYSTEM_METRICS_INDEX` constant and
    // returns an `i32`; it dereferences no pointer and has no side effects — FFI-`unsafe` only.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

// VKs Windows wants flagged extended even when the scancode high bits aren't set: the editing
// cluster (Ins/Del/Home/End/PgUp/PgDn = 0x21..0x28, 0x2D, 0x2E), the Win keys (0x5B/0x5C/0x5D),
// RCtrl (0xA3), RAlt (0xA5), Pause (0x90). MAPVK_VK_TO_VSC_EX already encodes E0 for most; this is a
// thin safety net.
fn forced_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x5D | 0xA3 | 0xA5 | 0x90
    )
}
