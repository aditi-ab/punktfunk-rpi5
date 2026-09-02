//! Virtual gamepads via `/dev/uinput`, cloning the kernel `xpad` identity so SDL/Steam/Proton
//! match their built-in mapping with no extra config. One [`VirtualPad`] per attached client
//! controller; [`GamepadManager`] applies decoded
//! [`GamepadFrame`](punktfunk_core::input::GamepadFrame)s.
//!
//! Rumble is the reverse path on the same fd: the game uploads FF effects
//! (`EV_UINPUT`/`UI_FF_UPLOAD` → `UI_BEGIN/END_FF_UPLOAD`) and plays them with `EV_FF`.
//! [`GamepadManager::pump_rumble`] must run every tick — a game's `EVIOCSFF` BLOCKS until
//! we answer `UI_END_FF_UPLOAD`. Mixdown is `(low, high)` for the host to send back.
//!
//! Ioctl numbers and struct layouts match `<linux/uinput.h>` on x86_64 (see the `size_of`
//! asserts). `/dev/uinput` needs the udev rule and `input` group
//! (`scripts/60-punktfunk.rules`).

use crate::pad_slots::PadSlots;
use anyhow::{bail, Result};
use punktfunk_core::input::{gamepad, GamepadFrame, MAX_PADS};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

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
const BTN_NORTH: u16 = 0x133; // X
const BTN_WEST: u16 = 0x134; // Y
const BTN_TL: u16 = 0x136;
const BTN_TR: u16 = 0x137;
const BTN_SELECT: u16 = 0x13a;
const BTN_START: u16 = 0x13b;
const BTN_MODE: u16 = 0x13c;
const BTN_THUMBL: u16 = 0x13d;
const BTN_THUMBR: u16 = 0x13e;
// xpad Elite paddles (SDL/Steam Input). PADDLE1/2/3/4 = R4/L4/R5/L5.
const BTN_TRIGGER_HAPPY5: u16 = 0x2c4;
const BTN_TRIGGER_HAPPY6: u16 = 0x2c5;
const BTN_TRIGGER_HAPPY7: u16 = 0x2c6;
const BTN_TRIGGER_HAPPY8: u16 = 0x2c7;

/// `(GameStream button bit, evdev key code)`. D-pad is HAT axes, not keys.
const BUTTON_MAP: [(u32, u16); 15] = [
    (gamepad::BTN_A, BTN_SOUTH),
    (gamepad::BTN_B, BTN_EAST),
    (gamepad::BTN_X, BTN_NORTH),
    (gamepad::BTN_Y, BTN_WEST),
    (gamepad::BTN_LB, BTN_TL),
    (gamepad::BTN_RB, BTN_TR),
    (gamepad::BTN_BACK, BTN_SELECT),
    (gamepad::BTN_START, BTN_START),
    (gamepad::BTN_GUIDE, BTN_MODE),
    (gamepad::BTN_LS_CLICK, BTN_THUMBL),
    (gamepad::BTN_RS_CLICK, BTN_THUMBR),
    (gamepad::BTN_PADDLE1, BTN_TRIGGER_HAPPY5),
    (gamepad::BTN_PADDLE2, BTN_TRIGGER_HAPPY6),
    (gamepad::BTN_PADDLE3, BTN_TRIGGER_HAPPY7),
    (gamepad::BTN_PADDLE4, BTN_TRIGGER_HAPPY8),
];

/// USB identity the virtual pad presents. SDL/Steam/Proton key the mapping off
/// `bustype/vendor/product/version` (+ name); games pick glyphs from it. Axis/button
/// layout is XInput either way — One/Series only changes glyphs. Impulse-trigger rumble
/// is not in evdev `FF_RUMBLE`.
#[derive(Clone, Copy)]
pub struct PadIdentity {
    vendor: u16,
    product: u16,
    version: u16,
    name: &'static [u8],
    log: &'static str,
}

impl PadIdentity {
    /// Kernel `xpad` table entry `045e:028e`. SDL/Steam map it with no extra config.
    pub const fn xbox360() -> PadIdentity {
        PadIdentity {
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
            name: b"Microsoft X-Box 360 pad",
            log: "X-Box 360 pad",
        }
    }

    /// Kernel `xpad` table entry `045e:02ea`. One/Series glyphs; XInput-identical otherwise.
    pub const fn xbox_one() -> PadIdentity {
        PadIdentity {
            vendor: 0x045e,
            product: 0x02ea,
            version: 0x0408,
            name: b"Microsoft X-Box One S pad",
            log: "X-Box One S pad",
        }
    }
}

impl Default for PadIdentity {
    fn default() -> PadIdentity {
        PadIdentity::xbox360()
    }
}

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

// x86_64 `<linux/uinput.h>` layouts.
const _: () = {
    assert!(std::mem::size_of::<UinputSetup>() == 92);
    assert!(std::mem::size_of::<UinputAbsSetup>() == 28);
    assert!(std::mem::size_of::<InputEventRaw>() == 24);
    assert!(std::mem::size_of::<FfEffect>() == 48);
    assert!(std::mem::size_of::<UinputFfUpload>() == 104);
    assert!(std::mem::size_of::<UinputFfErase>() == 12);
};

fn ioctl_int(fd: i32, req: libc::c_ulong, arg: libc::c_int, what: &str) -> Result<()> {
    // SAFETY: callers pass UI_SET_EVBIT/KEYBIT/FFBIT/UI_DEV_CREATE/UI_DEV_DESTROY — integer
    // ioctls whose third arg the kernel takes BY VALUE, so nothing is dereferenced through
    // `arg`. `fd` is the live `/dev/uinput` fd; a stale fd returns EBADF, not UB.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: *mut T, what: &str) -> Result<()> {
    // SAFETY: `fd` is the caller's live `/dev/uinput` fd. Call sites pass `&mut x` for a
    // uniquely-borrowed `#[repr(C)]` `T` whose size matches the request (`UI_DEV_SETUP`
    // 0x405c_5503 → 0x5c=92; `UI_ABS_SETUP` → 0x1c=28; FF upload/erase → 0x68/0x0c — pinned
    // by the `size_of` asserts). The kernel copies that many bytes; the `&mut` lives for
    // the whole synchronous call.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Played-effect window: `replay.delay` of silence, then `replay.length` of rumble.
#[derive(Clone, Copy)]
struct Playback {
    /// `play + replay.delay`. Armed but silent until then.
    starts: Instant,
    /// When it stops, or `None` for replay length 0 (until explicitly stopped).
    ends: Option<Instant>,
}

struct Effect {
    strong: u16,
    weak: u16,
    playing: Option<Playback>,
    replay_ms: u16,
    /// Silence after play. `replay.length` runs from the end of this delay, so the delay
    /// shifts the window instead of eating into it.
    delay_ms: u16,
}

impl Effect {
    /// Silent for `replay.delay`, then `replay.length` of rumble (length 0 = until stopped).
    /// Length is measured from the end of the delay, not from the play command.
    fn window(&self, at: Instant) -> Playback {
        let starts = at + Duration::from_millis(self.delay_ms as u64);
        Playback {
            starts,
            ends: (self.replay_ms > 0)
                .then(|| starts + Duration::from_millis(self.replay_ms as u64)),
        }
    }
}

/// Game-side FF table and mixdown (finite-replay expiry + abandoned infinite force-off).
/// Split from [`VirtualPad`] so the policy is testable without a uinput fd.
struct FfState {
    effects: HashMap<i16, Effect>,
    gain: u32,
    /// Last `(low, high)` reported, to dedup.
    last_mix: (u16, u16),
    /// Last upload/erase/play/stop/gain. An infinite-replay effect still playing past the
    /// idle window against this was abandoned — kernel auto-erase only runs on fd close.
    /// Finite effects keep their declared deadline. SDL re-plays held rumble every ~2 s.
    last_activity: Instant,
}

impl FfState {
    fn new() -> FfState {
        FfState {
            effects: HashMap::new(),
            gain: 0xFFFF,
            last_mix: (0, 0),
            last_activity: Instant::now(),
        }
    }

    fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// `Some` only when mixed `(low, high)` changed since last call.
    fn mix(&mut self, now: Instant, idle: Option<Duration>) -> Option<(u16, u16)> {
        let quiet_since = |t: Instant| idle.is_some_and(|d| now.duration_since(t) >= d);
        let plane_stale = quiet_since(self.last_activity);
        let (mut strong, mut weak) = (0u32, 0u32);
        for e in self.effects.values_mut() {
            let Some(p) = e.playing else { continue };
            // Still inside `replay.delay`: armed, silent, not a candidate for expiry or the
            // abandoned-effect force-off — it has not had its turn yet.
            if now < p.starts {
                continue;
            }
            match p.ends {
                Some(d) if now >= d => e.playing = None,
                // Infinite-replay, no FF traffic for `idle`. Kernel auto-erase only runs on
                // fd close. Require audible-for-`idle` too: play is last_activity, so a delay
                // longer than idle would die on its first contributing tick.
                None if plane_stale && quiet_since(p.starts) => {
                    tracing::info!(
                        strong = e.strong,
                        weak = e.weak,
                        "rumble: stale infinite FF effect (game stopped driving the pad) — forcing off"
                    );
                    e.playing = None;
                }
                _ => {
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

pub struct VirtualPad {
    fd: OwnedFd,
    ff: FfState,
}

impl VirtualPad {
    pub fn create(index: usize, identity: PadIdentity) -> Result<VirtualPad> {
        use std::os::fd::FromRawFd;
        // SAFETY: `c"/dev/uinput"` is a 'static NUL-terminated C string; `as_ptr()` is a
        // valid path the kernel only reads. `open` returns a fresh fd (or -1) and retains
        // nothing; no Rust memory is handed over except that 'static path.
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
        // SAFETY: `raw >= 0` (the `< 0` branch already bailed). The fd is freshly opened
        // and not stored elsewhere. `OwnedFd` becomes the unique owner and closes it once.
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

        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x0003, // BUS_USB
                vendor: identity.vendor,
                product: identity.product,
                version: identity.version,
            },
            name: [0; 80],
            ff_effects_max: 16, // must be > 0 or FF uploads are never delivered
        };
        let name = identity.name;
        setup.name[..name.len()].copy_from_slice(name);
        ioctl_ptr(raw, UI_DEV_SETUP, &mut setup, "UI_DEV_SETUP")?;
        ioctl_int(raw, UI_DEV_CREATE, 0, "UI_DEV_CREATE")?;
        tracing::info!(
            index,
            pad = identity.log,
            "virtual gamepad created (uinput)"
        );

        Ok(VirtualPad {
            fd,
            ff: FfState::new(),
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
        // Best-effort: a full kernel queue drops the event; the next frame re-syncs state.
        // SAFETY: `self.fd` is the live uinput `OwnedFd` (borrowed via `as_raw_fd`).
        // `write` READS `size_of::<InputEventRaw>()` initialized bytes from local `ev`
        // (`#[repr(C)]` all-integer, no padding, size 24) and retains nothing past return.
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &ev as *const _ as *const libc::c_void,
                std::mem::size_of::<InputEventRaw>(),
            )
        };
    }

    pub fn apply(&mut self, f: &GamepadFrame) {
        // Absolute state every frame, not XOR edges: `emit` is best-effort, so a dropped
        // edge would stick until that button toggles again. Kernel input drops an EV_KEY
        // that already matches device state (BTN_* does not autorepeat).
        for (bit, key) in BUTTON_MAP {
            self.emit(EV_KEY, key, ((f.buttons & bit) != 0) as i32);
        }

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

    /// Non-blocking FF protocol on this pad's fd. `Some` when mixed `(low, high)` changed.
    fn pump_ff(&mut self) -> Option<(u16, u16)> {
        let raw = self.fd.as_raw_fd();
        let mut buf = [0u8; std::mem::size_of::<InputEventRaw>()];
        loop {
            // SAFETY: `raw` is the live non-blocking uinput fd. `buf` is a local
            // `[u8; size_of::<InputEventRaw>()]`; `read` writes at most `buf.len()` bytes.
            // The buffer outlives this synchronous call and is borrowed uniquely.
            let n = unsafe { libc::read(raw, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n != buf.len() as isize {
                break; // EAGAIN / short read — queue drained
            }
            // SAFETY: `buf` is exactly `size_of::<InputEventRaw>()` bytes and fully written by
            // the `read` above. `read_unaligned` because `[u8]` is 1-aligned and `InputEventRaw`
            // needs 8 (`timeval`); a plain `ptr::read` would be UB.
            let ev: InputEventRaw =
                unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const InputEventRaw) };
            match (ev.type_, ev.code) {
                (EV_UINPUT, UI_FF_UPLOAD) => {
                    self.ff.note_activity();
                    // SAFETY: `UinputFfUpload` is `#[repr(C)]` over integers and two `FfEffect`s
                    // (integers + `[u8; 32]`); all-zero is valid for every field (no
                    // bool/NonZero/enum/reference niche). `request_id` is set below; the ioctl
                    // fills the rest.
                    let mut up: UinputFfUpload = unsafe { std::mem::zeroed() };
                    up.request_id = ev.value as u32;
                    if ioctl_ptr(raw, UI_BEGIN_FF_UPLOAD, &mut up, "UI_BEGIN_FF_UPLOAD").is_ok() {
                        let e = up.effect;
                        // ff-core assigns a slot before uinput sees the request. A local
                        // counter would fight the kernel's id space.
                        debug_assert!(e.id >= 0, "uinput handed us an unassigned FF effect id");
                        if e.type_ == FF_RUMBLE {
                            let strong = u16::from_ne_bytes([e.u[0], e.u[1]]);
                            let weak = u16::from_ne_bytes([e.u[2], e.u[3]]);
                            let slot = self.ff.effects.entry(e.id).or_insert(Effect {
                                strong: 0,
                                weak: 0,
                                playing: None,
                                replay_ms: 0,
                                delay_ms: 0,
                            });
                            slot.strong = strong;
                            slot.weak = weak;
                            slot.replay_ms = e.replay_length;
                            slot.delay_ms = e.replay_delay;
                        }
                        up.effect.id = e.id; // hand the assigned slot back to the kernel
                        up.retval = 0;
                        let _ = ioctl_ptr(raw, UI_END_FF_UPLOAD, &mut up, "UI_END_FF_UPLOAD");
                    }
                }
                (EV_UINPUT, UI_FF_ERASE) => {
                    self.ff.note_activity();
                    // SAFETY: `UinputFfErase` is `#[repr(C)]` over three integer fields; all-zero
                    // is valid for each. `request_id` is set below; the ioctl fills `effect_id`.
                    let mut er: UinputFfErase = unsafe { std::mem::zeroed() };
                    er.request_id = ev.value as u32;
                    if ioctl_ptr(raw, UI_BEGIN_FF_ERASE, &mut er, "UI_BEGIN_FF_ERASE").is_ok() {
                        self.ff.effects.remove(&(er.effect_id as i16));
                        er.retval = 0;
                        let _ = ioctl_ptr(raw, UI_END_FF_ERASE, &mut er, "UI_END_FF_ERASE");
                    }
                }
                (EV_FF, FF_GAIN) => {
                    self.ff.note_activity();
                    self.ff.gain = (ev.value as u32).min(0xFFFF);
                }
                (EV_FF, code) => {
                    self.ff.note_activity();
                    if let Some(e) = self.ff.effects.get_mut(&(code as i16)) {
                        e.playing = (ev.value != 0).then(|| e.window(Instant::now()));
                    }
                }
                _ => {}
            }
        }

        self.ff
            .mix(Instant::now(), crate::uhid_manager::rumble_idle_timeout())
    }
}

impl Drop for VirtualPad {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is still live here (`OwnedFd` closes only after this `drop`
        // returns). UI_DEV_DESTROY takes 0 BY VALUE, so nothing is dereferenced.
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}

/// Evdev holds last-known state kernel-side, so this rides [`PadSlots`] with no extra
/// vec or heartbeat.
pub struct GamepadManager {
    slots: PadSlots<VirtualPad>,
    /// Shared by every pad in the session.
    identity: PadIdentity,
}

impl Default for GamepadManager {
    fn default() -> GamepadManager {
        GamepadManager::new()
    }
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        GamepadManager::with_identity(PadIdentity::xbox360())
    }

    pub fn with_identity(identity: PadIdentity) -> GamepadManager {
        GamepadManager {
            slots: PadSlots::new(identity.log, "gamepad", ""),
            identity,
        }
    }

    pub fn handle(&mut self, ev: &punktfunk_core::input::GamepadEvent) {
        use punktfunk_core::input::GamepadEvent;
        match ev {
            GamepadEvent::Arrival { index, kind, .. } => {
                tracing::info!(index, kind, "controller arrival ({})", self.slots.label());
                self.ensure(*index as usize);
            }
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                if idx >= MAX_PADS {
                    return;
                }
                // Drop any allocated pad whose mask bit cleared. No per-index sibling
                // state to reset — the pads mix rumble internally.
                self.slots.sweep(f.active_mask);
                if f.active_mask & (1 << idx) == 0 {
                    return; // this event WAS the unplug
                }
                self.ensure(idx);
                if let Some(pad) = self.slots.get_mut(idx) {
                    pad.apply(f);
                }
            }
        }
    }

    fn ensure(&mut self, idx: usize) {
        let identity = self.identity;
        // `VirtualPad::create` logs its own success line (it knows the identity + transport).
        self.slots
            .ensure(idx, |i| VirtualPad::create(i as usize, identity));
    }

    /// Service every pad's FF protocol. `send(index, low, high, left_trigger, right_trigger)`
    /// runs when mixed rumble changed. Call every tick: games block in `EVIOCSFF` until answered.
    /// Trigger levels are always 0: `FF_RUMBLE` is `{strong, weak}` with no third field.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16, u16, u16)) {
        // Reap an unplug whose removal frame only armed the grace — that frame is sent once,
        // so without this the uinput node outlives the controller. The swept mask is unused:
        // this manager has no per-index sibling state.
        self.slots.reap();
        for (i, pad) in self.slots.iter_mut() {
            if let Some((low, high)) = pad.pump_ff() {
                send(i as u16, low, high, 0, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn find_ff_node(name: &str) -> Option<String> {
        let s = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
        let mut cur = String::new();
        let mut node = None;
        for line in s.lines() {
            if let Some(n) = line.strip_prefix("N: Name=") {
                cur = n.trim_matches('"').to_string();
            } else if let Some(h) = line.strip_prefix("H: Handlers=") {
                if cur.contains(name) {
                    node = h
                        .split_whitespace()
                        .find(|t| t.starts_with("event"))
                        .map(|ev| format!("/dev/input/{ev}"));
                }
            } else if line.starts_with("B: FF=")
                && cur.contains(name)
                && node.is_some()
                && !line.trim_end().ends_with("FF=0")
            {
                return node;
            }
        }
        node
    }

    /// Upload + play an `FF_RUMBLE`. Returns the OPEN fd (close erases the process's effects)
    /// and the kernel-assigned id. `EVIOCSFF` BLOCKS until the uinput owner answers
    /// `UI_FF_UPLOAD` — the caller must not be the thread running [`VirtualPad::pump_ff`].
    fn evdev_rumble(node: &str, strong: u16, weak: u16) -> std::io::Result<(std::fs::File, i16)> {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)?;
        let mut eff = [0u8; 48]; // struct ff_effect; union (rumble magnitudes) at offset 16
        eff[0..2].copy_from_slice(&FF_RUMBLE.to_ne_bytes());
        eff[2..4].copy_from_slice(&(-1i16).to_ne_bytes()); // id: kernel assigns
        eff[10..12].copy_from_slice(&5000u16.to_ne_bytes()); // replay.length ms
        eff[16..18].copy_from_slice(&strong.to_ne_bytes());
        eff[18..20].copy_from_slice(&weak.to_ne_bytes());
        // EVIOCSFF = _IOW('E', 0x80, struct ff_effect)
        let req: libc::c_ulong = (1 << 30) | (48 << 16) | (0x45 << 8) | 0x80;
        // SAFETY: EVIOCSFF reads/writes the 48-byte `ff_effect` behind `f`; `eff` is
        // exactly `sizeof(struct ff_effect)` and outlives the synchronous call.
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, eff.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let id = i16::from_ne_bytes([eff[2], eff[3]]);
        let mut ev = [0u8; 24]; // struct input_event: timeval 16, type u16, code u16, value s32
        ev[16..18].copy_from_slice(&EV_FF.to_ne_bytes());
        ev[18..20].copy_from_slice(&(id as u16).to_ne_bytes());
        ev[20..24].copy_from_slice(&1i32.to_ne_bytes()); // play
        f.write_all(&ev)?;
        Ok((f, id))
    }

    #[test]
    #[ignore = "creates a real /dev/uinput device; needs the input group"]
    fn ff_upload_reaches_pump_and_stops_on_erase() {
        let mut pad = VirtualPad::create(0, PadIdentity::xbox360()).expect("create uinput pad");
        std::thread::sleep(Duration::from_millis(700)); // let udev settle the node
        let node = find_ff_node("Microsoft X-Box 360 pad").expect("no X-Box 360 evdev node");
        let game = std::thread::spawn(move || {
            let r = evdev_rumble(&node, 0xC000, 0x4000);
            std::thread::sleep(Duration::from_millis(1200)); // hold the effect, then erase
            r.expect("EVIOCSFF/play (fd held meanwhile)");
        });
        let start = Instant::now();
        let mut seen = Vec::new();
        while start.elapsed() < Duration::from_millis(2500) {
            if let Some(mix) = pad.pump_ff() {
                seen.push(mix);
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        game.join().unwrap();
        // Requested magnitudes scaled by the 0xFFFF default gain (>> 16).
        assert!(
            seen.contains(&(0xBFFF, 0x3FFF)),
            "evdev FF rumble never surfaced through pump_ff: {seen:?}"
        );
        assert_eq!(
            seen.last(),
            Some(&(0, 0)),
            "erase-on-close never produced a stop mix: {seen:?}"
        );
    }
}

#[cfg(test)]
mod ff_state_tests {
    use super::*;

    /// The default idle window the shared hatch resolves to when the env is unset.
    const IDLE: Option<Duration> = Some(Duration::from_millis(2500));

    /// `gain` is 0xFFFF (not a true 1.0 multiplier), so a magnitude loses 1 LSB in the mixdown.
    fn scaled(v: u16) -> u16 {
        ((v as u32 * 0xFFFF) >> 16) as u16
    }

    fn ff_with(effect: Effect) -> FfState {
        let mut ff = FfState::new();
        ff.effects.insert(0, effect);
        ff
    }

    /// Playing from `at`, no delay, until explicitly stopped.
    fn playing(at: Instant) -> Option<Playback> {
        Some(Playback {
            starts: at,
            ends: None,
        })
    }

    fn playing_for(at: Instant, len: Duration) -> Option<Playback> {
        Some(Playback {
            starts: at,
            ends: Some(at + len),
        })
    }

    #[test]
    fn abandoned_infinite_effect_is_forced_off_after_idle_window() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            // Playing since before the window: abandoned means audible AND unattended.
            playing: playing(now - Duration::from_millis(2600)),
            replay_ms: 0,
            delay_ms: 0,
        });
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
        assert_eq!(ff.mix(now, IDLE), None); // unchanged level; still playing
        // FF plane quiet past the idle window: cut, exactly once.
        ff.last_activity = now - Duration::from_millis(2600);
        assert_eq!(ff.mix(now, IDLE), Some((0, 0)));
        assert_eq!(ff.mix(now, IDLE), None); // already off — no repeat
    }

    #[test]
    fn finite_effect_honors_its_replay_deadline_not_the_idle_window() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x4000,
            weak: 0,
            playing: playing_for(now, Duration::from_secs(10)),
            replay_ms: 10_000,
            delay_ms: 0,
        });
        // FF plane long stale, but a finite replay is the contract — keep playing.
        ff.last_activity = now - Duration::from_secs(60);
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x4000), 0)));
        // Expires at its own deadline, not the idle window.
        assert_eq!(ff.mix(now + Duration::from_secs(11), IDLE), Some((0, 0)));
    }

    #[test]
    fn replay_after_cut_rearms_the_effect() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: playing(now - Duration::from_millis(3000)),
            replay_ms: 0,
            delay_ms: 0,
        });
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
        ff.last_activity = now - Duration::from_millis(3000);
        assert_eq!(ff.mix(now, IDLE), Some((0, 0)));
        ff.last_activity = now;
        ff.effects.get_mut(&0).unwrap().playing = playing(now);
        assert_eq!(ff.mix(now, IDLE), Some((scaled(0x8000), 0)));
    }

    #[test]
    fn replay_delay_holds_the_effect_off_then_gives_it_its_full_length() {
        let now = Instant::now();
        let starts = now + Duration::from_millis(500);
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: Some(Playback {
                starts,
                ends: Some(starts + Duration::from_secs(1)),
            }),
            replay_ms: 1000,
            delay_ms: 500,
        });
        // Inside the delay: armed but silent.
        assert_eq!(ff.mix(now, IDLE), None);
        assert_eq!(ff.mix(now + Duration::from_millis(499), IDLE), None);
        // Delay elapsed: it plays.
        assert_eq!(
            ff.mix(now + Duration::from_millis(501), IDLE),
            Some((scaled(0x8000), 0))
        );
        // Still playing at 1400 ms — full second FROM the delay, not from play.
        assert_eq!(ff.mix(now + Duration::from_millis(1400), IDLE), None);
        // Ends at delay + length, not at length.
        assert_eq!(
            ff.mix(now + Duration::from_millis(1600), IDLE),
            Some((0, 0))
        );
    }

    /// Playback window from the uploaded fields. Separate from `mix` because the `EV_FF`
    /// handler that calls [`Effect::window`] needs a live uinput fd; a mix-only test would
    /// pass with the delay ignored.
    #[test]
    fn window_offsets_the_whole_playback_by_replay_delay() {
        let at = Instant::now();

        let delayed = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 1000,
            delay_ms: 500,
        };
        let w = delayed.window(at);
        assert_eq!(
            w.starts,
            at + Duration::from_millis(500),
            "delay defers the start"
        );
        assert_eq!(
            w.ends,
            Some(at + Duration::from_millis(1500)),
            "length runs from the END of the delay, so the effect keeps its full second"
        );

        let plain = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 1000,
            delay_ms: 0,
        };
        let w = plain.window(at);
        assert_eq!(w.starts, at);
        assert_eq!(w.ends, Some(at + Duration::from_millis(1000)));

        // Length 0 = until stopped, but the delay still applies.
        let infinite = Effect {
            strong: 0,
            weak: 0,
            playing: None,
            replay_ms: 0,
            delay_ms: 250,
        };
        let w = infinite.window(at);
        assert_eq!(w.starts, at + Duration::from_millis(250));
        assert_eq!(w.ends, None);
    }

    /// A delayed effect is not "abandoned" while it is still waiting: it has not had its
    /// turn, and the idle window can be shorter than a legitimate delay.
    #[test]
    fn a_waiting_effect_is_not_cut_by_the_idle_watchdog() {
        let now = Instant::now();
        let starts = now + Duration::from_secs(5);
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: Some(Playback { starts, ends: None }),
            replay_ms: 0,
            delay_ms: 5000,
        });
        ff.last_activity = now - Duration::from_secs(60); // long stale
        assert_eq!(ff.mix(now, IDLE), None); // silent, but not cut
        // Plays once the delay elapses.
        assert_eq!(
            ff.mix(now + Duration::from_millis(5001), IDLE),
            Some((scaled(0x8000), 0))
        );
    }

    #[test]
    fn disabled_watchdog_never_cuts() {
        let now = Instant::now();
        let mut ff = ff_with(Effect {
            strong: 0x8000,
            weak: 0,
            playing: playing(now),
            replay_ms: 0,
            delay_ms: 0,
        });
        ff.last_activity = now - Duration::from_secs(600);
        assert_eq!(ff.mix(now, None), Some((scaled(0x8000), 0)));
    }
}
