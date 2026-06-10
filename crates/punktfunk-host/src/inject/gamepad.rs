//! Virtual gamepads via `/dev/uinput`, cloning the kernel `xpad` identity ("Microsoft X-Box
//! 360 pad", `045e:028e`) so SDL/Steam/Proton match their built-in mapping with zero
//! configuration — exactly what Sunshine emulates. One [`VirtualPad`] per attached client
//! controller, managed by [`GamepadManager`] from decoded
//! [`GamepadFrame`](crate::gamestream::gamepad::GamepadFrame)s.
//!
//! Rumble flows the *other* way on the same fd: games upload force-feedback effects
//! (`EV_UINPUT`/`UI_FF_UPLOAD` → `UI_BEGIN/END_FF_UPLOAD` ioctls) and trigger them with
//! `EV_FF` writes; [`GamepadManager::pump_rumble`] services that protocol non-blockingly
//! (the control thread calls it every tick) and reports mixed `(low, high)` motor levels for
//! the host to send to the client. Note: a game's `EVIOCSFF` ioctl BLOCKS until we answer
//! `UI_END_FF_UPLOAD`, so the pump must run regularly.
//!
//! All ioctl numbers/struct layouts below were verified against this generation's
//! `<linux/uinput.h>` on x86_64. `/dev/uinput` needs a udev rule + `input` group membership
//! (see `scripts/60-punktfunk.rules`); creation fails with a clear error otherwise.

use crate::gamestream::gamepad::{self, GamepadFrame, MAX_PADS};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Instant;

// ioctls (x86_64).
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_FFBIT: libc::c_ulong = 0x4004_556b;
const UI_BEGIN_FF_UPLOAD: libc::c_ulong = 0xc068_55c8;
const UI_END_FF_UPLOAD: libc::c_ulong = 0x4068_55c9;
const UI_BEGIN_FF_ERASE: libc::c_ulong = 0xc00c_55ca;
const UI_END_FF_ERASE: libc::c_ulong = 0x400c_55cb;

// Event types/codes.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_FF: u16 = 0x15;
const EV_UINPUT: u16 = 0x0101;
const SYN_REPORT: u16 = 0;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
const FF_RUMBLE: u16 = 0x50;
const FF_GAIN: u16 = 0x60;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RY: u16 = 0x04;
const ABS_RZ: u16 = 0x05;
const ABS_HAT0X: u16 = 0x10;
const ABS_HAT0Y: u16 = 0x11;

const BTN_SOUTH: u16 = 0x130; // A
const BTN_EAST: u16 = 0x131; // B
const BTN_NORTH: u16 = 0x133; // X (kernel calls it BTN_NORTH/BTN_X)
const BTN_WEST: u16 = 0x134; // Y
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;

/// `(GameStream button bit, evdev key code)` — D-pad is emitted as HAT axes instead.
const BUTTON_MAP: [(u32, u16); 11] = [
    (gamepad::BTN_A, BTN_SOUTH),
    (gamepad::BTN_B, BTN_EAST),
    (gamepad::BTN_X, BTN_NORTH),
    (gamepad::BTN_Y, BTN_WEST),
    (gamepad::BTN_LB, BTN_TL),
    (gamepad::BTN_RB, BTN_TR),
    (gamepad::BTN_BACK, BTN_SELECT),
    (gamepad::BTN_START, BTN_START),
    (gamepad::BTN_GUIDE, BTN_MODE),
    (gamepad::BTN_LS_CLK, BTN_THUMBL),
    (gamepad::BTN_RS_CLK, BTN_THUMBR),
];

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEventRaw {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

/// `struct ff_effect` (48 bytes; the union starts 8-aligned at offset 16).
#[repr(C)]
#[derive(Clone, Copy)]
struct FfEffect {
    type_: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    _pad: u16,
    /// Union; for `FF_RUMBLE`: `u16 strong_magnitude` at [0..2], `u16 weak_magnitude` at [2..4].
    u: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputFfUpload {
    request_id: u32,
    retval: i32,
    effect: FfEffect,
    old: FfEffect,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UinputFfErase {
    request_id: u32,
    retval: i32,
    effect_id: u32,
}

// Layouts verified by compiling a probe against this generation's <linux/uinput.h> (x86_64).
const _: () = {
    assert!(std::mem::size_of::<UinputSetup>() == 92);
    assert!(std::mem::size_of::<UinputAbsSetup>() == 28);
    assert!(std::mem::size_of::<InputEventRaw>() == 24);
    assert!(std::mem::size_of::<FfEffect>() == 48);
    assert!(std::mem::size_of::<UinputFfUpload>() == 104);
    assert!(std::mem::size_of::<UinputFfErase>() == 12);
};

fn ioctl_int(fd: i32, req: libc::c_ulong, arg: libc::c_int, what: &str) -> Result<()> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: *mut T, what: &str) -> Result<()> {
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// One FF effect a game uploaded: rumble magnitudes + playback state.
struct Effect {
    strong: u16,
    weak: u16,
    /// `Some(deadline)` while playing (replay length 0 = until stopped).
    playing: Option<Option<Instant>>,
    replay_ms: u16,
}

/// One virtual X-Box-360 pad backed by a uinput device.
pub struct VirtualPad {
    fd: OwnedFd,
    prev_buttons: u32,
    effects: HashMap<i16, Effect>,
    next_effect_id: i16,
    gain: u32,
    /// Last `(low, high)` reported, to dedup.
    last_mix: (u16, u16),
}

impl VirtualPad {
    pub fn create(index: usize) -> Result<VirtualPad> {
        use std::os::fd::FromRawFd;
        let raw = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            bail!(
                "open /dev/uinput: {} (install the udev rule granting the 'input' group access \
                 — see scripts/60-punktfunk.rules — and add the user to the 'input' group)",
                std::io::Error::last_os_error()
            );
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        ioctl_int(raw, UI_SET_EVBIT, EV_KEY as i32, "UI_SET_EVBIT(EV_KEY)")?;
        ioctl_int(raw, UI_SET_EVBIT, EV_ABS as i32, "UI_SET_EVBIT(EV_ABS)")?;
        ioctl_int(raw, UI_SET_EVBIT, EV_FF as i32, "UI_SET_EVBIT(EV_FF)")?;
        for (_, key) in BUTTON_MAP {
            ioctl_int(raw, UI_SET_KEYBIT, key as i32, "UI_SET_KEYBIT")?;
        }
        ioctl_int(
            raw,
            UI_SET_FFBIT,
            FF_RUMBLE as i32,
            "UI_SET_FFBIT(FF_RUMBLE)",
        )?;
        ioctl_int(raw, UI_SET_FFBIT, FF_GAIN as i32, "UI_SET_FFBIT(FF_GAIN)")?;

        let stick = AbsInfo {
            minimum: -32768,
            maximum: 32767,
            fuzz: 16,
            flat: 128,
            ..Default::default()
        };
        let trigger = AbsInfo {
            minimum: 0,
            maximum: 255,
            ..Default::default()
        };
        let hat = AbsInfo {
            minimum: -1,
            maximum: 1,
            ..Default::default()
        };
        for (code, info) in [
            (ABS_X, stick),
            (ABS_Y, stick),
            (ABS_RX, stick),
            (ABS_RY, stick),
            (ABS_Z, trigger),
            (ABS_RZ, trigger),
            (ABS_HAT0X, hat),
            (ABS_HAT0Y, hat),
        ] {
            let mut a = UinputAbsSetup {
                code,
                _pad: 0,
                absinfo: info,
            };
            ioctl_ptr(raw, UI_ABS_SETUP, &mut a, "UI_ABS_SETUP")?;
        }

        // The xpad identity: SDL keys its built-in mapping off bustype/vendor/product/version.
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x0003, // BUS_USB
                vendor: 0x045e,
                product: 0x028e,
                version: 0x0110,
            },
            name: [0; 80],
            ff_effects_max: 16, // must be > 0 or FF uploads are never delivered
        };
        let name = b"Microsoft X-Box 360 pad";
        setup.name[..name.len()].copy_from_slice(name);
        ioctl_ptr(raw, UI_DEV_SETUP, &mut setup, "UI_DEV_SETUP")?;
        ioctl_int(raw, UI_DEV_CREATE, 0, "UI_DEV_CREATE")?;
        tracing::info!(index, "virtual gamepad created (X-Box 360 pad via uinput)");

        Ok(VirtualPad {
            fd,
            prev_buttons: 0,
            effects: HashMap::new(),
            next_effect_id: 0,
            gain: 0xFFFF,
            last_mix: (0, 0),
        })
    }

    fn emit(&self, type_: u16, code: u16, value: i32) {
        let ev = InputEventRaw {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const _ as *const u8,
                std::mem::size_of::<InputEventRaw>(),
            )
        };
        // Best-effort: a full kernel queue drops the event; the next frame re-syncs state.
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
    }

    /// Apply one decoded frame: button transitions, axes, D-pad hat, one SYN_REPORT.
    pub fn apply(&mut self, f: &GamepadFrame) {
        let changed = self.prev_buttons ^ f.buttons;
        for (bit, key) in BUTTON_MAP {
            if changed & bit != 0 {
                self.emit(EV_KEY, key, ((f.buttons & bit) != 0) as i32);
            }
        }
        self.prev_buttons = f.buttons;

        // Moonlight: +Y = up; evdev: +Y = down → negate (i32 math avoids -(-32768) overflow).
        self.emit(EV_ABS, ABS_X, f.ls_x as i32);
        self.emit(EV_ABS, ABS_Y, -(f.ls_y as i32));
        self.emit(EV_ABS, ABS_RX, f.rs_x as i32);
        self.emit(EV_ABS, ABS_RY, -(f.rs_y as i32));
        self.emit(EV_ABS, ABS_Z, f.left_trigger as i32);
        self.emit(EV_ABS, ABS_RZ, f.right_trigger as i32);
        let hat_x = ((f.buttons & gamepad::BTN_DPAD_RIGHT != 0) as i32)
            - ((f.buttons & gamepad::BTN_DPAD_LEFT != 0) as i32);
        let hat_y = ((f.buttons & gamepad::BTN_DPAD_DOWN != 0) as i32)
            - ((f.buttons & gamepad::BTN_DPAD_UP != 0) as i32);
        self.emit(EV_ABS, ABS_HAT0X, hat_x);
        self.emit(EV_ABS, ABS_HAT0Y, hat_y);
        self.emit(EV_SYN, SYN_REPORT, 0);
    }

    /// Service the FF protocol on this pad's fd (non-blocking). Returns the new mixed
    /// `(low, high)` motor levels if they changed since last call.
    fn pump_ff(&mut self) -> Option<(u16, u16)> {
        let raw = self.fd.as_raw_fd();
        let mut buf = [0u8; std::mem::size_of::<InputEventRaw>()];
        loop {
            let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n != buf.len() as isize {
                break; // EAGAIN / short read — queue drained
            }
            let ev: InputEventRaw = unsafe { std::ptr::read(buf.as_ptr() as *const _) };
            match (ev.type_, ev.code) {
                (EV_UINPUT, UI_FF_UPLOAD) => {
                    let mut up: UinputFfUpload = unsafe { std::mem::zeroed() };
                    up.request_id = ev.value as u32;
                    if ioctl_ptr(raw, UI_BEGIN_FF_UPLOAD, &mut up, "UI_BEGIN_FF_UPLOAD").is_ok() {
                        let mut e = up.effect;
                        if e.id == -1 {
                            e.id = self.next_effect_id;
                            self.next_effect_id = self.next_effect_id.wrapping_add(1);
                        }
                        if e.type_ == FF_RUMBLE {
                            let strong = u16::from_ne_bytes([e.u[0], e.u[1]]);
                            let weak = u16::from_ne_bytes([e.u[2], e.u[3]]);
                            let slot = self.effects.entry(e.id).or_insert(Effect {
                                strong: 0,
                                weak: 0,
                                playing: None,
                                replay_ms: 0,
                            });
                            slot.strong = strong;
                            slot.weak = weak;
                            slot.replay_ms = e.replay_length;
                        }
                        up.effect.id = e.id; // hand the assigned slot back to the kernel
                        up.retval = 0;
                        let _ = ioctl_ptr(raw, UI_END_FF_UPLOAD, &mut up, "UI_END_FF_UPLOAD");
                    }
                }
                (EV_UINPUT, UI_FF_ERASE) => {
                    let mut er: UinputFfErase = unsafe { std::mem::zeroed() };
                    er.request_id = ev.value as u32;
                    if ioctl_ptr(raw, UI_BEGIN_FF_ERASE, &mut er, "UI_BEGIN_FF_ERASE").is_ok() {
                        self.effects.remove(&(er.effect_id as i16));
                        er.retval = 0;
                        let _ = ioctl_ptr(raw, UI_END_FF_ERASE, &mut er, "UI_END_FF_ERASE");
                    }
                }
                (EV_FF, FF_GAIN) => self.gain = (ev.value as u32).min(0xFFFF),
                (EV_FF, code) => {
                    if let Some(e) = self.effects.get_mut(&(code as i16)) {
                        e.playing = if ev.value != 0 {
                            Some((e.replay_ms > 0).then(|| {
                                Instant::now()
                                    + std::time::Duration::from_millis(e.replay_ms as u64)
                            }))
                        } else {
                            None
                        };
                    }
                }
                _ => {}
            }
        }

        // Mix: sum playing effects (expiring finished ones), scale by gain.
        let now = Instant::now();
        let (mut strong, mut weak) = (0u32, 0u32);
        for e in self.effects.values_mut() {
            if let Some(deadline) = e.playing {
                if deadline.is_some_and(|d| now >= d) {
                    e.playing = None;
                } else {
                    strong = strong.saturating_add(e.strong as u32);
                    weak = weak.saturating_add(e.weak as u32);
                }
            }
        }
        // Linux FF: strong = low-frequency (big) motor, weak = high-frequency motor.
        let low = ((strong.min(0xFFFF) * self.gain) >> 16) as u16;
        let high = ((weak.min(0xFFFF) * self.gain) >> 16) as u16;
        (self.last_mix != (low, high)).then(|| {
            self.last_mix = (low, high);
            (low, high)
        })
    }
}

impl Drop for VirtualPad {
    fn drop(&mut self) {
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}

/// All virtual pads of a session, driven from decoded controller events.
#[derive(Default)]
pub struct GamepadManager {
    pads: Vec<Option<VirtualPad>>,
    /// Pad creation failed (e.g. /dev/uinput permissions) — warn once, drop events.
    broken: bool,
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        GamepadManager {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            broken: false,
        }
    }

    /// Handle one decoded controller event (create/destroy by mask, then apply state).
    pub fn handle(&mut self, ev: &crate::gamestream::gamepad::GamepadEvent) {
        use crate::gamestream::gamepad::GamepadEvent;
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival");
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Unplugs: drop any allocated pad whose mask bit cleared.
                for (i, slot) in self.pads.iter_mut().enumerate() {
                    if slot.is_some() && f.active_mask & (1 << i) == 0 {
                        tracing::info!(index = i, "controller unplugged");
                        *slot = None;
                    }
                }
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                if let Some(pad) = self.pads[idx].as_mut() {
                    pad.apply(f);
                }
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        if idx >= MAX_PADS || self.pads[idx].is_some() || self.broken {
            return;
        }
        match VirtualPad::create(idx) {
            Ok(p) => self.pads[idx] = Some(p),
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "virtual gamepad creation failed — controller input disabled");
                self.broken = true;
            }
        }
    }

    /// Service every pad's FF protocol; `send(index, low, high)` is invoked for each pad whose
    /// mixed rumble level changed. Call frequently (games block in `EVIOCSFF` until answered).
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16)) {
        for (i, slot) in self.pads.iter_mut().enumerate() {
            if let Some(pad) = slot {
                if let Some((low, high)) = pad.pump_ff() {
                    send(i as u16, low, high);
                }
            }
        }
    }
}
