//! Virtual tablet ("Punktfunk Pen"): a uinput stylus device carrying the pen plane's full
//! fidelity — pressure, tilt, barrel roll, hover distance, eraser, barrel buttons
//! (design/pen-tablet-input.md §5).
//!
//! Deliberately a **uinput device, not a compositor-protocol citizen**: no virtual-tablet
//! protocol exists in EI, the RemoteDesktop portal, KWin `fake_input`, or wlroots — while
//! every compositor consumes evdev tablets via libinput and forwards them to apps over
//! `zwp_tablet_v2` with nothing to configure. udev's `input_id` builtin classifies the device
//! from its capabilities (`BTN_TOOL_PEN` + `ABS_X/Y` ⇒ `ID_INPUT_TABLET`), so Krita/GIMP/
//! Xournal++ see a real pen. Output mapping is the compositor's own tablet-mapping default
//! (single output ⇒ correct; multi-monitor pinning is the documented follow-up, which is why
//! the device carries a stable, distinctive identity to key rules on).
//!
//! The consumer feeds it [`PenTransition`]s straight from the core's
//! [`PenTracker`](punktfunk_core::quic::PenTracker); this file only translates transitions to
//! evdev events and groups them into SYN frames so proximity-enter carries its position in the
//! same frame (libinput would otherwise report an entry at a stale point).
//!
//! ioctl numbers/struct layouts mirror `gamepad.rs` (verified against the same kernel
//! generation); each backend file stays self-contained by convention.

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

// input-event-codes.h subset.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// Barrel roll rides ABS_Z (the Wacom Art-Pen rotation convention); libinput normalizes the
/// declared min..max onto its 0..360° rotation axis.
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
/// The pen writes on the display it is mapped to (a "screen tablet"), not a desk pad —
/// libinput then maps the full ABS range onto the output rect, exactly the wire's normalized
/// coordinate contract.
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

/// The active tool's evdev key, one in proximity at a time (Wacom semantics — the core's
/// [`PenTracker`](punktfunk_core::quic::PenTracker) already re-enters proximity on a tool
/// switch, so this only tracks which key to release on `ProximityOut`).
fn tool_key(tool: PenTool) -> u16 {
    match tool {
        PenTool::Eraser => BTN_TOOL_RUBBER,
        // Unknown = a newer client's future tool — nearest ink-capable behavior is the pen.
        PenTool::Pen | PenTool::Unknown => BTN_TOOL_PEN,
    }
}

/// One per-session virtual tablet. Created lazily on the first pen batch (a session that never
/// draws never creates a device), destroyed with the session (Drop → `UI_DEV_DESTROY`).
pub struct VirtualPen {
    fd: OwnedFd,
    /// The tool key currently held in proximity (release target for `ProximityOut`).
    tool: u16,
    /// Whether the current SYN frame already carries a Motion — the frame-split trigger for
    /// consecutive samples in one batch.
    frame_has_motion: bool,
    /// Whether the current SYN frame has any unflushed events.
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

        // Position spans the full u16 range; `resolution` (units/mm) only feeds libinput's mm
        // math (nothing pen-relevant), but tablets without one trip its missing-resolution
        // fixup — 100 declares a plausible ~655 mm drawing surface.
        let pos = AbsInfo {
            minimum: 0,
            maximum: 65535,
            resolution: 100,
            ..Default::default()
        };
        // Tilt in degrees from vertical, per evdev convention; resolution = units/radian (57
        // ⇔ 1 unit = 1°, what the Wacom driver declares).
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
                // Barrel roll: libinput maps the declared range linearly onto 0..360°.
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

        // A stable, distinctive identity (pid.codes open-source VID) so compositor
        // tablet-mapping rules — GNOME's per-`vendor:product` gsettings path, sway
        // `map_to_output` — can target exactly this device.
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
        // Best-effort like the gamepad path: a full kernel queue drops the event; pen samples
        // are state-full, so the next frame re-syncs axes (and the tracker re-syncs state).
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
        // Polar → tiltX/tiltY needs both angles; azimuth clockwise from north, so east (90°)
        // tilts +X and south (180°, toward the user) tilts +Y — the evdev/W3C signs.
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

    /// Apply one decoded batch's transitions (the core tracker's output, in its documented
    /// order), grouping them into SYN frames: a frame closes before a `ProximityIn` (an entry
    /// is a new instant — and must carry its own position, not inherit the stale frame) and
    /// before a second `Motion` (consecutive samples are consecutive instants), plus a final
    /// close. So `[ProxIn, Motion, TipDown]` lands as ONE frame — libinput reports the entry
    /// already at the right point with contact — while a drag batch's `[Motion, Motion]`
    /// stays two.
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
