//! Virtual tablet ("Punktfunk Pen"): a uinput stylus with pressure, tilt, barrel
//! roll, hover distance, eraser, and barrel buttons.
//!
//! Compositors have no virtual-tablet protocol; they consume evdev tablets via
//! libinput and forward `zwp_tablet_v2`. udev's `input_id` builtin classifies
//! `BTN_TOOL_PEN` + `ABS_X/Y` as `ID_INPUT_TABLET`. A stable vendor:product lets
//! compositor mapping rules pin this device.
//!
//! [`PenTracker`](punktfunk_core::quic::PenTracker) feeds [`PenTransition`]s.
//! This file maps them to evdev and groups SYN frames so proximity-enter
//! carries its position in the same frame — libinput otherwise reports a stale
//! point. ioctl numbers and layouts match `gamepad.rs`.
//!
//! Evidence: `design/pen-tablet-input.md`.

use anyhow::{bail, Result};
use punktfunk_core::quic::{PenSample, PenTool, PenTransition, PEN_BARREL1, PEN_BARREL2};
use std::os::fd::{AsRawFd, OwnedFd};

// ioctls (x86_64).
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// Barrel roll on ABS_Z (Wacom Art-Pen). libinput maps min..max onto 0..360°.
const ABS_Z: u16 = 0x02;
const ABS_PRESSURE: u16 = 0x18;
const ABS_DISTANCE: u16 = 0x19;
const ABS_TILT_X: u16 = 0x1a;
const ABS_TILT_Y: u16 = 0x1b;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOOL_RUBBER: u16 = 0x141;
const BTN_TOUCH: u16 = 0x14a;
const BTN_STYLUS: u16 = 0x14b;
const BTN_STYLUS2: u16 = 0x14c;
/// Screen tablet: libinput maps the full ABS range onto the output rect.
const INPUT_PROP_DIRECT: libc::c_int = 0x01;

/// Full-scale wire pressure (u16) → the declared 0..4095 axis.
const PRESSURE_SHIFT: u32 = 4;
/// Wire hover distance (u16, 0xFFFF = unknown) → the declared 0..1023 axis.
const DISTANCE_SHIFT: u32 = 6;
const ABS_RANGE: f32 = 65535.0;

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

fn ioctl_int(fd: i32, req: libc::c_ulong, arg: libc::c_int, what: &str) -> Result<()> {
    // SAFETY: every caller passes a UI_SET_*/UI_DEV_* request whose argument the kernel reads
    // as a plain int; `fd` is a live uinput fd owned by the caller. No memory is handed over.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: *mut T, what: &str) -> Result<()> {
    // SAFETY: every caller passes a pointer to a live, initialized `#[repr(C)]` struct matching
    // the request's expected layout (UI_DEV_SETUP/UI_ABS_SETUP); the kernel reads it during the
    // call and retains nothing.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Evdev key for the in-proximity tool. The tracker re-enters on a tool switch,
/// so this only names the key to release on `ProximityOut`.
fn tool_key(tool: PenTool) -> u16 {
    match tool {
        PenTool::Eraser => BTN_TOOL_RUBBER,
        // Unknown = a newer client's future tool — nearest ink-capable behavior is the pen.
        PenTool::Pen | PenTool::Unknown => BTN_TOOL_PEN,
    }
}

/// Per-session uinput tablet.
pub struct VirtualPen {
    fd: OwnedFd,
    /// In-proximity `BTN_TOOL_*`; the `ProximityOut` release target.
    tool: u16,
    /// Current SYN frame already has a Motion; a second Motion starts a new frame.
    frame_has_motion: bool,
    frame_dirty: bool,
}

impl VirtualPen {
    pub fn create() -> Result<VirtualPen> {
        use std::os::fd::FromRawFd;
        // SAFETY: `c"/dev/uinput"` is a 'static NUL-terminated C string literal; `open` reads it
        // as a path, returns a fresh fd (or -1) and retains nothing.
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
        // SAFETY: `raw >= 0` here, a freshly-opened fd owned nowhere else; `OwnedFd` becomes the
        // unique owner and closes it exactly once on drop.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        ioctl_int(raw, UI_SET_EVBIT, EV_KEY as i32, "UI_SET_EVBIT(EV_KEY)")?;
        ioctl_int(raw, UI_SET_EVBIT, EV_ABS as i32, "UI_SET_EVBIT(EV_ABS)")?;
        for key in [
            BTN_TOOL_PEN,
            BTN_TOOL_RUBBER,
            BTN_TOUCH,
            BTN_STYLUS,
            BTN_STYLUS2,
        ] {
            ioctl_int(raw, UI_SET_KEYBIT, key as i32, "UI_SET_KEYBIT")?;
        }
        ioctl_int(
            raw,
            UI_SET_PROPBIT,
            INPUT_PROP_DIRECT,
            "UI_SET_PROPBIT(DIRECT)",
        )?;

        // 0..65535, resolution 100 units/mm (~655 mm). Zero resolution trips libinput's
        // missing-resolution fixup; the mm figure is unused for pen mapping.
        let pos = AbsInfo {
            minimum: 0,
            maximum: 65535,
            resolution: 100,
            ..Default::default()
        };
        // Degrees from vertical. resolution 57 units/radian ⇒ 1 unit = 1° (Wacom).
        let tilt = AbsInfo {
            minimum: -90,
            maximum: 90,
            resolution: 57,
            ..Default::default()
        };
        for (code, info) in [
            (ABS_X, pos),
            (ABS_Y, pos),
            (
                ABS_PRESSURE,
                AbsInfo {
                    minimum: 0,
                    maximum: 4095,
                    ..Default::default()
                },
            ),
            (
                ABS_DISTANCE,
                AbsInfo {
                    minimum: 0,
                    maximum: 1023,
                    ..Default::default()
                },
            ),
            (ABS_TILT_X, tilt),
            (ABS_TILT_Y, tilt),
            (
                // 0..359: libinput maps the declared range linearly onto 0..360°.
                ABS_Z,
                AbsInfo {
                    minimum: 0,
                    maximum: 359,
                    ..Default::default()
                },
            ),
        ] {
            let mut a = UinputAbsSetup {
                code,
                _pad: 0,
                absinfo: info,
            };
            ioctl_ptr(raw, UI_ABS_SETUP, &mut a, "UI_ABS_SETUP")?;
        }

        // pid.codes VID + "PF" PID so compositor tablet-mapping can target this device.
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x0006, // BUS_VIRTUAL
                vendor: 0x1209,
                product: 0x5046, // "PF"
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let name = b"Punktfunk Pen";
        setup.name[..name.len()].copy_from_slice(name);
        ioctl_ptr(raw, UI_DEV_SETUP, &mut setup, "UI_DEV_SETUP")?;
        ioctl_int(raw, UI_DEV_CREATE, 0, "UI_DEV_CREATE")?;
        tracing::info!("virtual tablet created (Punktfunk Pen, uinput)");

        Ok(VirtualPen {
            fd,
            tool: BTN_TOOL_PEN,
            frame_has_motion: false,
            frame_dirty: false,
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
        // SAFETY: `ev` is a live local `#[repr(C)]` all-integer struct (no padding: timeval=16 +
        // u16 + u16 + i32 = 24), so every byte is initialized; the slice spans exactly `ev`'s
        // bytes and is used immediately below with no concurrent mutation.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &ev as *const _ as *const u8,
                std::mem::size_of::<InputEventRaw>(),
            )
        };
        // Best-effort: a full kernel queue drops the event; the next sample re-syncs axes.
        // SAFETY: `self.fd` stays open for the synchronous call; `write` only reads
        // `bytes.len()` bytes from the still-live local and retains nothing.
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
    }

    fn flush(&mut self) {
        if self.frame_dirty {
            self.emit(EV_SYN, SYN_REPORT, 0);
            self.frame_dirty = false;
            self.frame_has_motion = false;
        }
    }

    fn motion(&mut self, s: &PenSample) {
        self.emit(EV_ABS, ABS_X, (s.x * ABS_RANGE) as i32);
        self.emit(EV_ABS, ABS_Y, (s.y * ABS_RANGE) as i32);
        self.emit(EV_ABS, ABS_PRESSURE, (s.pressure >> PRESSURE_SHIFT) as i32);
        if s.distance != punktfunk_core::quic::PEN_DISTANCE_UNKNOWN {
            self.emit(EV_ABS, ABS_DISTANCE, (s.distance >> DISTANCE_SHIFT) as i32);
        }
        // Polar → tiltX/tiltY. Azimuth clockwise from north: east (90°) is +X, south (180°) is +Y.
        if s.tilt_deg != punktfunk_core::quic::PEN_TILT_UNKNOWN
            && s.azimuth_deg != punktfunk_core::quic::PEN_ANGLE_UNKNOWN
        {
            let az = (s.azimuth_deg as f32).to_radians();
            let tilt = s.tilt_deg as f32;
            self.emit(EV_ABS, ABS_TILT_X, (tilt * az.sin()).round() as i32);
            self.emit(EV_ABS, ABS_TILT_Y, (-tilt * az.cos()).round() as i32);
        }
        if s.roll_deg != punktfunk_core::quic::PEN_ANGLE_UNKNOWN {
            self.emit(EV_ABS, ABS_Z, (s.roll_deg % 360) as i32);
        }
        self.frame_dirty = true;
        self.frame_has_motion = true;
    }

    /// Apply one batch of tracker transitions as SYN frames. Close a frame before
    /// `ProximityIn` (entry must carry its own position) and before a second
    /// `Motion` (consecutive samples are consecutive instants), then close at the
    /// end. `[ProxIn, Motion, TipDown]` is one frame; `[Motion, Motion]` is two.
    pub fn apply_batch(&mut self, transitions: &[PenTransition]) {
        for t in transitions {
            match t {
                PenTransition::ProximityIn { tool } => {
                    self.flush();
                    self.tool = tool_key(*tool);
                    self.emit(EV_KEY, self.tool, 1);
                    self.frame_dirty = true;
                }
                PenTransition::Motion { sample } => {
                    if self.frame_has_motion {
                        self.flush();
                    }
                    self.motion(sample);
                }
                PenTransition::TipDown => {
                    self.emit(EV_KEY, BTN_TOUCH, 1);
                    self.frame_dirty = true;
                }
                PenTransition::ButtonsChanged { pressed, released } => {
                    for (bit, key) in [(PEN_BARREL1, BTN_STYLUS), (PEN_BARREL2, BTN_STYLUS2)] {
                        if pressed & bit != 0 {
                            self.emit(EV_KEY, key, 1);
                            self.frame_dirty = true;
                        }
                        if released & bit != 0 {
                            self.emit(EV_KEY, key, 0);
                            self.frame_dirty = true;
                        }
                    }
                }
                PenTransition::TipUp => {
                    self.emit(EV_KEY, BTN_TOUCH, 0);
                    self.emit(EV_ABS, ABS_PRESSURE, 0);
                    self.frame_dirty = true;
                }
                PenTransition::ProximityOut => {
                    self.emit(EV_KEY, self.tool, 0);
                    self.frame_dirty = true;
                }
            }
        }
        self.flush();
    }
}

impl Drop for VirtualPen {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is still open (OwnedFd closes only after this body returns);
        // UI_DEV_DESTROY takes no pointer argument. Errors are moot on teardown.
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}
