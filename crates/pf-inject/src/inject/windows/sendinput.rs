//! Windows `SendInput` injection (Win32 KeyboardAndMouse) — analogue of [`super::wlr`]:
//! absolute mouse over the streamed output ([`crate::stream_target`]), relative mouse
//! for games, scancode keyboard, scroll, buttons. Survives UAC/lock by Sunshine's
//! retry-on-failure: the thread stays bound and only reattaches
//! (`OpenInputDesktop`/`SetThreadDesktop`) when `SendInput` reports a short write.
//!
//! Keyboard (`crate::KEY_FLAG_SEMANTIC_VK`): first-party clients send US-positional
//! VKs, resolved through [`positional_vk_to_scan`]. GameStream/Moonlight send
//! layout-semantic VKs, resolved under the foreground app's layout. Never resolve a
//! positional VK through a layout: this thread is the SYSTEM service, whose layout
//! is not the user's — a German host would y↔z / ü-on-ö scramble.

use anyhow::Result;
use punktfunk_core::input::{InputEvent, InputKind};
use std::mem::size_of;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    HDESK,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, MapVirtualKeyExW, SendInput, HKL, INPUT, INPUT_0, INPUT_KEYBOARD,
    INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC_EX, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use super::InputInjector;

const GENERIC_ALL: u32 = 0x1000_0000;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

pub struct SendInputInjector {
    desktop: Option<HDESK>,
    /// PT_TOUCH device, created on the first wire-touch. `None` after a failed
    /// create (pre-1809) — touch then stays a no-op.
    touch: Option<crate::pen::SyntheticTouch>,
    touch_failed: bool,
}

// SAFETY: the only non-`Send` field is `Option<HDESK>` (`SyntheticTouch` is `Send`
// on its own). The host drives this from one dedicated thread; the type is not
// `Sync`. An `HDESK` is not thread-affine for ownership (`CloseDesktop` works from
// any thread; `SetThreadDesktop` rebinds the current thread).
unsafe impl Send for SendInputInjector {}

impl SendInputInjector {
    pub fn open() -> Result<Self> {
        let mut me = Self {
            desktop: None,
            touch: None,
            touch_failed: false,
        };
        me.reattach_input_desktop(); // best-effort
        tracing::info!("SendInput injector ready (Win32 KeyboardAndMouse)");
        Ok(me)
    }

    /// Bind this thread to the desktop currently receiving input. UAC / lock / Ctrl-Alt-Del
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

    /// Inject with Sunshine's retry-on-failure: stay bound to the last desktop, and only
    /// when `SendInput` reports a short write (0 = input desktop switched) reattach and
    /// retry once. No per-event `OpenInputDesktop`/`SetThreadDesktop`.
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
                // Client (0..w,0..h) → STREAMED output rect ([`crate::stream_target`];
                // whole virtual desktop only as fallback) → 0..65535 over the virtual
                // desktop for MOUSEEVENTF_VIRTUALDESK. Mapping over the desktop alone
                // is the Extend-topology offset bug (design/pen-tablet-input.md).
                let cx = (event.x.clamp(0, w as i32)) as f64 / w as f64;
                let cy = (event.y.clamp(0, h as i32)) as f64 / h as f64;
                let px = crate::stream_target::map_normalized(cx, cy);
                let (ax, ay) = crate::stream_target::desktop_px_to_virtualdesk(px);
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
                // GameStream WHEEL_DELTA(120) units. Windows WHEEL positive=up (matches
                // GameStream — no flip, unlike Wayland); HWHEEL positive=right. x is already 120-scaled.
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
                let vk = (event.code & 0xff) as u16;
                let semantic = (event.flags & crate::KEY_FLAG_SEMANTIC_VK) != 0;
                // Positional VKs: US table for the layout-variant typing area; everything
                // else falls through to `MapVirtualKeyExW` (layout-invariant, extended bit).
                // Semantic VKs skip the table and use the foreground app's layout.
                let table = if semantic {
                    None
                } else {
                    positional_vk_to_scan(vk)
                };
                let (scan, extended) = match table {
                    Some(scan) => (scan, forced_extended(vk)), // typing area: never E0-extended
                    None => {
                        let hkl = if semantic { foreground_hkl() } else { None };
                        // SAFETY: `MapVirtualKeyExW` is a pure value translation (VK → scancode);
                        // all three args are by-value (`u32`, the `MAPVK_VK_TO_VSC_EX` map-type
                        // constant, an optional `HKL` handle used only as a lookup key). It
                        // dereferences no pointer and returns a `u32` — FFI-`unsafe` only.
                        let sc_ex = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, hkl) };
                        if sc_ex == 0 {
                            return Ok(()); // unmappable -> drop
                        }
                        (
                            (sc_ex & 0xff) as u16,
                            (sc_ex & 0xe000) == 0xe000 || forced_extended(vk),
                        )
                    }
                };
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
            InputKind::TextInput => {
                // Committed IME text: one Unicode scalar per event, injected as
                // `KEYEVENTF_UNICODE` (wScan = UTF-16 unit, no scancode/layout). An
                // astral-plane scalar is its surrogate pair, each unit down+up in order.
                let Some(ch) = char::from_u32(event.code) else {
                    return Ok(()); // lone surrogate / out of range — drop
                };
                if ch.is_control() {
                    return Ok(()); // control chars ride the VK path (Enter/Backspace/Tab)
                }
                let mut units = [0u16; 2];
                let mut inputs: Vec<INPUT> = Vec::with_capacity(4);
                for &unit in ch.encode_utf16(&mut units).iter() {
                    for flags in [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP] {
                        inputs.push(key(KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: unit,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        }));
                    }
                }
                self.send(&inputs)
            }
            // Gamepad goes through the XUSB backend.
            InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadState
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => Ok(()),
            // Wire touch → PT_TOUCH (design/pen-tablet-input.md). Lazy: a session that
            // never touches never creates one; a pre-1809 create failure latches the no-op.
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {
                if self.touch.is_none() && !self.touch_failed {
                    match crate::pen::SyntheticTouch::create() {
                        Ok(t) => self.touch = Some(t),
                        Err(e) => {
                            self.touch_failed = true;
                            tracing::warn!(
                                error = %format!("{e:#}"),
                                "touch: synthetic pointer unavailable — wire touch stays a no-op"
                            );
                        }
                    }
                }
                if let Some(t) = self.touch.as_mut() {
                    t.apply(event);
                }
                Ok(())
            }
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

/// US-positional VK → set-1 make scancode for the layout-variant typing area (letters,
/// digit row, OEM punctuation, ISO 102nd key). Mirror of Linux `crate::vk_to_evdev` —
/// for these keys the evdev code IS the set-1 scancode. Layout-invariant keys are
/// absent (`MapVirtualKeyExW` resolves them under any layout). Never E0-extended.
fn positional_vk_to_scan(vk: u16) -> Option<u16> {
    Some(match vk {
        0x30 => 0x0B,                    // VK_0
        0x31..=0x39 => vk - 0x31 + 0x02, // VK_1..VK_9 → 0x02..0x0A
        0x41 => 0x1E,                    // A
        0x42 => 0x30,                    // B
        0x43 => 0x2E,                    // C
        0x44 => 0x20,                    // D
        0x45 => 0x12,                    // E
        0x46 => 0x21,                    // F
        0x47 => 0x22,                    // G
        0x48 => 0x23,                    // H
        0x49 => 0x17,                    // I
        0x4A => 0x24,                    // J
        0x4B => 0x25,                    // K
        0x4C => 0x26,                    // L
        0x4D => 0x32,                    // M
        0x4E => 0x31,                    // N
        0x4F => 0x18,                    // O
        0x50 => 0x19,                    // P
        0x51 => 0x10,                    // Q
        0x52 => 0x13,                    // R
        0x53 => 0x1F,                    // S
        0x54 => 0x14,                    // T
        0x55 => 0x16,                    // U
        0x56 => 0x2F,                    // V
        0x57 => 0x11,                    // W
        0x58 => 0x2D,                    // X
        0x59 => 0x15,                    // Y (US position — a QWERTZ host renders it as Z)
        0x5A => 0x2C,                    // Z (US position)
        0xBA => 0x27,                    // VK_OEM_1      ;:  (DE: ö)
        0xBB => 0x0D,                    // VK_OEM_PLUS   =+
        0xBC => 0x33,                    // VK_OEM_COMMA  ,<
        0xBD => 0x0C,                    // VK_OEM_MINUS  -_  (DE: ß)
        0xBE => 0x34,                    // VK_OEM_PERIOD .>
        0xBF => 0x35,                    // VK_OEM_2      /?
        0xC0 => 0x29,                    // VK_OEM_3      `~  (DE: ^)
        0xDB => 0x1A,                    // VK_OEM_4      [{  (DE: ü)
        0xDC => 0x2B,                    // VK_OEM_5      \|
        0xDD => 0x1B,                    // VK_OEM_6      ]}
        0xDE => 0x28,                    // VK_OEM_7      '"  (DE: ä)
        0xE2 => 0x56,                    // VK_OEM_102    <>| (ISO key next to left shift)
        _ => return None,
    })
}

/// Keyboard layout of the thread owning the foreground window — the layout the
/// receiving app will decode our scancodes under. `None` when there is no
/// foreground window (secure desktop) — caller falls back to this thread's layout.
fn foreground_hkl() -> Option<HKL> {
    // SAFETY: three read-only queries. `GetForegroundWindow` takes nothing and returns a possibly
    // null `HWND` (checked). `GetWindowThreadProcessId` reads the window's owning thread id (the
    // process-id out-param is `None`, allowed). `GetKeyboardLayout` maps a thread id to its input
    // locale by value. No pointer we own is dereferenced; a stale/foreign `tid` yields a null HKL,
    // which is filtered.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return None;
        }
        let tid = GetWindowThreadProcessId(hwnd, None);
        let hkl = GetKeyboardLayout(tid);
        (!hkl.is_invalid()).then_some(hkl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The positional table must mirror Linux `vk_to_evdev` exactly — for the typing
    /// area the evdev code IS the set-1 scancode, so a divergence lands the same wire
    /// VK on different physical keys on the two hosts.
    #[test]
    fn positional_table_mirrors_linux_vk_to_evdev() {
        let mut checked = 0;
        for vk in 0x01..=0xFEu16 {
            if let Some(scan) = positional_vk_to_scan(vk) {
                assert_eq!(
                    Some(scan),
                    crate::vk_to_evdev(vk as u8),
                    "vk 0x{vk:02X}: sendinput scancode diverges from vk_to_evdev"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 48, "typing-area coverage changed unexpectedly");
    }

    /// US-position VKs for physical Y/Z/ö/ü must resolve to those positions, not a layout.
    #[test]
    fn positional_pins_for_the_qwertz_scramble() {
        assert_eq!(positional_vk_to_scan(0x59), Some(0x15)); // VK_Y → US-Y position (QWERTZ: Z key)
        assert_eq!(positional_vk_to_scan(0x5A), Some(0x2C)); // VK_Z → US-Z position (QWERTZ: Y key)
        assert_eq!(positional_vk_to_scan(0xBA), Some(0x27)); // VK_OEM_1 → ;: position (QWERTZ: ö)
        assert_eq!(positional_vk_to_scan(0xDB), Some(0x1A)); // VK_OEM_4 → [{ position (QWERTZ: ü)
                                                             // Layout-invariant keys stay out of the table (resolved via MapVirtualKeyExW).
        assert_eq!(positional_vk_to_scan(0x70), None); // VK_F1
        assert_eq!(positional_vk_to_scan(0x0D), None); // VK_RETURN
        assert_eq!(positional_vk_to_scan(0xA0), None); // VK_LSHIFT
    }
}
