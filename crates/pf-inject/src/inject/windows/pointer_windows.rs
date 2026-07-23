//! Windows synthetic-pointer injection (design/pen-tablet-input.md §6): a per-session `PT_PEN`
//! device carrying the pen plane's full fidelity — pressure (rescaled to Windows' 0..1024),
//! polar tilt → tiltX/tiltY, barrel roll on `rotation` (0..359 — Windows Ink renders Pencil
//! Pro roll natively), barrel button, eraser (`PEN_FLAG_INVERTED`/`ERASER`), hover — plus a
//! `PT_TOUCH` device that closes the long-standing SendInput touch no-op for wire touches.
//!
//! Both follow Apollo's proven recipe (`design/apollo-comparison.md`): synthetic pointer state
//! goes STALE if not re-injected (~100 ms), so each device runs a small refresh thread that
//! re-asserts the last frame every [`REFRESH_MS`] while the pen is in range / contacts are
//! held — a stationary stylus must not hover-out (and a held finger must not auto-lift) just
//! because no new samples arrived. `CreateSyntheticPointerDevice` needs Win10 1809+;
//! [`crate::pen_supported`] probes it, so older hosts simply never advertise pen.
//!
//! Frame grouping mirrors the Linux uinput backend: a proximity-enter is injected together
//! with its position (never at a stale point), tip edges get their own DOWN/UP frames, and a
//! range-leave is a final frame without `INRANGE`.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it.
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{Context, Result};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, PenTransition, PEN_ANGLE_UNKNOWN, PEN_BARREL1, PEN_TILT_UNKNOWN,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_DOWN, POINTER_FLAG_FIRSTBUTTON,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_NEW, POINTER_FLAG_UP,
    POINTER_FLAG_UPDATE, POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, PEN_FLAG_BARREL, PEN_FLAG_ERASER, PEN_FLAG_INVERTED, PEN_FLAG_NONE,
    PEN_MASK_PRESSURE, PEN_MASK_ROTATION, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, PT_TOUCH,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, TOUCH_FLAG_NONE,
    TOUCH_MASK_NONE,
};

/// Re-inject cadence while state is held (in-range pen / touching contacts). Apollo uses
/// 50 ms against the ~100 ms staleness window; 40 ms keeps two refreshes inside it.
const REFRESH_MS: u64 = 40;

/// Windows pen pressure is 0..1024 (vs the wire's full-scale u16).
const WIN_PEN_PRESSURE_MAX: u32 = 1024;

/// Map a normalized [0,1] coordinate pair onto virtual-desktop pixels — the same surface the
/// SendInput absolute mouse targets, so pen, touch, and pointer all land identically.
fn to_screen(x: f32, y: f32) -> POINT {
    // SAFETY: `GetSystemMetrics` takes a constant index and reads global metrics; no memory in.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    };
    POINT {
        x: vx + (x.clamp(0.0, 1.0) * (vw - 1) as f32) as i32,
        y: vy + (y.clamp(0.0, 1.0) * (vh - 1) as f32) as i32,
    }
}

/// An owned `HSYNTHETICPOINTERDEVICE` (destroyed exactly once on drop).
struct Device(HSYNTHETICPOINTERDEVICE);

// SAFETY: the handle is a plain kernel object identifier with no thread affinity —
// `InjectSyntheticPointerInput`/`DestroySyntheticPointerDevice` are documented callable from
// any thread; ownership transfer/sharing does not alias memory.
unsafe impl Send for Device {}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the device this wrapper uniquely owns; destroyed once here.
        unsafe { DestroySyntheticPointerDevice(self.0) };
    }
}

/// Probe: can this Windows build create a synthetic pen device (Win10 1809+)?
pub fn synthetic_pen_available() -> bool {
    // SAFETY: FFI create with by-value args; on success the returned handle is destroyed
    // immediately by the `Device` wrapper, exactly once.
    match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
        Ok(h) => {
            drop(Device(h));
            true
        }
        Err(_) => false,
    }
}

/// The tracked pen state a refresh tick re-asserts.
#[derive(Default)]
struct PenState {
    in_range: bool,
    touching: bool,
    barrel: bool,
    eraser: bool,
    x: f32,
    y: f32,
    pressure: u16,
    tilt_deg: u8,
    azimuth_deg: u16,
    roll_deg: u16,
}

struct PenShared {
    dev: Device,
    state: PenState,
    /// First injection failure logs at WARN (see `TouchShared::fail_warned`).
    fail_warned: bool,
}

/// One per-session virtual pen (the Windows sibling of the Linux uinput tablet — same
/// [`PenTransition`] consumer API). Wire barrel button 2 has no Windows pen equivalent
/// (one barrel + eraser is the platform model) and is ignored here.
pub struct VirtualPen {
    shared: Arc<Mutex<PenShared>>,
    stop: Arc<AtomicBool>,
    refresher: Option<std::thread::JoinHandle<()>>,
    // Batch-local frame grouping (single-threaded within apply_batch).
    edge_down: bool,
    edge_up: bool,
    is_new: bool,
    frame_dirty: bool,
    frame_has_motion: bool,
}

impl VirtualPen {
    pub fn create() -> Result<VirtualPen> {
        // SAFETY: FFI create with by-value args; the handle's sole owner becomes `Device`.
        let dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .context("CreateSyntheticPointerDevice(PT_PEN) — needs Windows 10 1809+")?;
        let shared = Arc::new(Mutex::new(PenShared {
            dev: Device(dev),
            state: PenState::default(),
            fail_warned: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        // The staleness guard: re-assert the last frame while in range so a stationary pen
        // (native plane: between 100 ms heartbeats; GameStream: indefinitely) never hovers out.
        let refresher = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("pf-pen-refresh".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
                        let s = &mut *shared.lock().unwrap();
                        if s.state.in_range {
                            inject_pen(s, POINTER_FLAG_UPDATE, false);
                        }
                    }
                })
                .context("spawn pen refresh thread")?
        };
        tracing::info!("virtual pen created (Windows synthetic pointer, PT_PEN)");
        Ok(VirtualPen {
            shared,
            stop,
            refresher: Some(refresher),
            edge_down: false,
            edge_up: false,
            is_new: false,
            frame_dirty: false,
            frame_has_motion: false,
        })
    }

    /// Apply one batch of tracker transitions — same grouping contract as the Linux backend:
    /// `[ProxIn, Motion, TipDown]` is ONE frame (entry lands at its position, in contact),
    /// consecutive `Motion`s split, tip edges own their frames, range-leave is a final
    /// no-`INRANGE` frame.
    pub fn apply_batch(&mut self, transitions: &[PenTransition]) {
        for t in transitions {
            match t {
                PenTransition::ProximityIn { tool } => {
                    self.flush();
                    let mut s = self.shared.lock().unwrap();
                    s.state.in_range = true;
                    s.state.eraser = *tool == PenTool::Eraser;
                    drop(s);
                    self.is_new = true;
                    self.frame_dirty = true;
                }
                PenTransition::Motion { sample } => {
                    if self.frame_has_motion {
                        self.flush();
                    }
                    self.set_axes(sample);
                    self.frame_dirty = true;
                    self.frame_has_motion = true;
                }
                PenTransition::TipDown => {
                    self.shared.lock().unwrap().state.touching = true;
                    self.edge_down = true;
                    self.frame_dirty = true;
                }
                PenTransition::ButtonsChanged { pressed, released } => {
                    let mut s = self.shared.lock().unwrap();
                    if pressed & PEN_BARREL1 != 0 {
                        s.state.barrel = true;
                    }
                    if released & PEN_BARREL1 != 0 {
                        s.state.barrel = false;
                    }
                    drop(s);
                    self.frame_dirty = true;
                }
                PenTransition::TipUp => {
                    self.shared.lock().unwrap().state.touching = false;
                    self.edge_up = true;
                    self.frame_dirty = true;
                    self.flush(); // UP owns its frame (still INRANGE)
                }
                PenTransition::ProximityOut => {
                    self.shared.lock().unwrap().state.in_range = false;
                    self.frame_dirty = true;
                    self.flush(); // final frame without INRANGE
                }
            }
        }
        self.flush();
    }

    fn set_axes(&mut self, s: &PenSample) {
        let mut sh = self.shared.lock().unwrap();
        sh.state.x = s.x;
        sh.state.y = s.y;
        sh.state.pressure = s.pressure;
        sh.state.tilt_deg = s.tilt_deg;
        sh.state.azimuth_deg = s.azimuth_deg;
        sh.state.roll_deg = s.roll_deg;
    }

    fn flush(&mut self) {
        if !self.frame_dirty {
            return;
        }
        let edge = if self.edge_down {
            POINTER_FLAG_DOWN
        } else if self.edge_up {
            POINTER_FLAG_UP
        } else {
            POINTER_FLAG_UPDATE
        };
        inject_pen(&mut self.shared.lock().unwrap(), edge, self.is_new);
        self.edge_down = false;
        self.edge_up = false;
        self.is_new = false;
        self.frame_dirty = false;
        self.frame_has_motion = false;
    }
}

impl Drop for VirtualPen {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.refresher.take() {
            let _ = h.join();
        }
        // The device itself dies with `shared` (Device::drop) — Windows releases any held
        // in-range/contact state when the synthetic device is destroyed.
    }
}

/// Build + inject one pen frame from tracked state. `edge` is DOWN/UP/UPDATE;
/// `INRANGE`/`INCONTACT` derive from the state itself.
fn inject_pen(sh: &mut PenShared, edge: POINTER_FLAGS, is_new: bool) {
    let st = &sh.state;
    let mut flags = edge;
    if st.in_range {
        flags |= POINTER_FLAG_INRANGE;
    }
    if st.touching {
        flags |= POINTER_FLAG_INCONTACT | POINTER_FLAG_FIRSTBUTTON;
    }
    if is_new {
        flags |= POINTER_FLAG_NEW;
    }
    let mut pen_flags = PEN_FLAG_NONE;
    if st.barrel {
        pen_flags |= PEN_FLAG_BARREL;
    }
    if st.eraser {
        pen_flags |= PEN_FLAG_INVERTED;
        if st.touching {
            pen_flags |= PEN_FLAG_ERASER;
        }
    }
    let mut pen_mask = PEN_MASK_PRESSURE;
    // Contact needs a nonzero pressure to ink; hover reports 0 (Windows convention).
    let pressure = if st.touching {
        ((st.pressure as u32 * WIN_PEN_PRESSURE_MAX) / u16::MAX as u32).max(1)
    } else {
        0
    };
    let (mut tilt_x, mut tilt_y) = (0i32, 0i32);
    if st.tilt_deg != PEN_TILT_UNKNOWN && st.azimuth_deg != PEN_ANGLE_UNKNOWN {
        let az = (st.azimuth_deg as f32).to_radians();
        let tilt = st.tilt_deg as f32;
        tilt_x = (tilt * az.sin()).round() as i32;
        tilt_y = (-tilt * az.cos()).round() as i32;
        pen_mask |= PEN_MASK_TILT_X | PEN_MASK_TILT_Y;
    }
    let mut rotation = 0u32;
    if st.roll_deg != PEN_ANGLE_UNKNOWN {
        rotation = (st.roll_deg % 360) as u32;
        pen_mask |= PEN_MASK_ROTATION;
    }
    let pt = to_screen(st.x, st.y);
    let info = POINTER_TYPE_INFO {
        r#type: PT_PEN,
        Anonymous: POINTER_TYPE_INFO_0 {
            penInfo: POINTER_PEN_INFO {
                pointerInfo: POINTER_INFO {
                    pointerType: PT_PEN,
                    pointerId: 0,
                    pointerFlags: flags,
                    ptPixelLocation: pt,
                    ptPixelLocationRaw: pt,
                    ..Default::default()
                },
                penFlags: pen_flags,
                penMask: pen_mask,
                pressure,
                rotation,
                tiltX: tilt_x,
                tiltY: tilt_y,
            },
        },
    };
    // SAFETY: `sh.dev.0` is the live device this wrapper owns; the one-element array is a live
    // stack value the call only reads. Best-effort like every injector write — a transient
    // failure (desktop switch) is healed by the next refresh tick re-asserting state.
    if let Err(e) = unsafe { InjectSyntheticPointerInput(sh.dev.0, &[info]) } {
        if !sh.fail_warned {
            sh.fail_warned = true;
            tracing::warn!(error = %e, "pen inject failed");
        } else {
            tracing::trace!(error = %e, "pen inject failed (transient)");
        }
    } else {
        sh.fail_warned = false;
    }
}

/// One live wire-touch contact. `slot` is the SMALL, DENSE pointer id handed to Windows —
/// synthetic-pointer injection rejects arbitrary large ids, and clients (Moonlight's
/// `pointerId` especially) send exactly those, so wire ids compact into the lowest free slot
/// for the contact's lifetime (Apollo's slot-contiguity rule; the on-glass symptom of passing
/// wire ids through was pen working while every touch silently failed to inject).
#[derive(Clone, Copy)]
struct Contact {
    id: u32,
    slot: u32,
    x: f32,
    y: f32,
}

/// Lowest slot not held by a live contact.
fn free_slot(contacts: &[Contact]) -> u32 {
    let mut slot = 0u32;
    while contacts.iter().any(|c| c.slot == slot) {
        slot += 1;
    }
    slot
}

/// Windows can inject at most this many simultaneous synthetic touch contacts.
const MAX_CONTACTS: usize = 10;

struct TouchShared {
    dev: Device,
    contacts: Vec<Contact>,
    /// First injection failure logs at WARN (an on-glass "touch does nothing" must be
    /// visible in the host log); repeats stay at trace.
    fail_warned: bool,
}

/// The `PT_TOUCH` device servicing wire `TouchDown/Move/Up` events — closes the SendInput
/// touch no-op. Contacts are keyed by the wire's finger id; every frame re-injects the FULL
/// active set (the synthetic-pointer contract), and a refresh thread re-asserts held contacts
/// against the ~100 ms staleness auto-lift.
pub struct SyntheticTouch {
    shared: Arc<Mutex<TouchShared>>,
    stop: Arc<AtomicBool>,
    refresher: Option<std::thread::JoinHandle<()>>,
}

impl SyntheticTouch {
    pub fn create() -> Result<SyntheticTouch> {
        // SAFETY: FFI create with by-value args; the handle's sole owner becomes `Device`.
        let dev = unsafe {
            CreateSyntheticPointerDevice(PT_TOUCH, MAX_CONTACTS as u32, POINTER_FEEDBACK_DEFAULT)
        }
        .context("CreateSyntheticPointerDevice(PT_TOUCH) — needs Windows 10 1809+")?;
        let shared = Arc::new(Mutex::new(TouchShared {
            dev: Device(dev),
            contacts: Vec::new(),
            fail_warned: false,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let refresher = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("pf-touch-refresh".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(REFRESH_MS));
                        let s = &mut *shared.lock().unwrap();
                        if !s.contacts.is_empty() {
                            inject_touch_frame(s, None);
                        }
                    }
                })
                .context("spawn touch refresh thread")?
        };
        tracing::info!("virtual touchscreen created (Windows synthetic pointer, PT_TOUCH)");
        Ok(SyntheticTouch {
            shared,
            stop,
            refresher: Some(refresher),
        })
    }

    /// Apply one wire touch event (`code` = finger id, pixel x/y against the
    /// `flags = (w << 16) | h` reference extent, exactly like `MouseMoveAbs`).
    pub fn apply(&mut self, ev: &InputEvent) {
        let (w, h) = ((ev.flags >> 16) as f32, (ev.flags & 0xFFFF) as f32);
        if (w < 1.0 || h < 1.0) && ev.kind != InputKind::TouchUp {
            return; // the documented zero-extent drop, as for MouseMoveAbs
        }
        let (x, y) = (ev.x as f32 / w.max(1.0), ev.y as f32 / h.max(1.0));
        let s = &mut *self.shared.lock().unwrap();
        match ev.kind {
            InputKind::TouchDown => {
                match s.contacts.iter().position(|c| c.id == ev.code) {
                    Some(i) => (s.contacts[i].x, s.contacts[i].y) = (x, y),
                    None if s.contacts.len() < MAX_CONTACTS => {
                        let slot = free_slot(&s.contacts);
                        s.contacts.push(Contact {
                            id: ev.code,
                            slot,
                            x,
                            y,
                        })
                    }
                    None => return, // beyond the platform max — drop, never evict a live finger
                }
                inject_touch_frame(s, Some((ev.code, POINTER_FLAG_DOWN)));
            }
            InputKind::TouchMove => {
                match s.contacts.iter().position(|c| c.id == ev.code) {
                    Some(i) => {
                        (s.contacts[i].x, s.contacts[i].y) = (x, y);
                        inject_touch_frame(s, None);
                    }
                    // A move for an unknown id (its DOWN was dropped/lost): synthesize the
                    // contact so the stroke self-heals, like the pen plane does.
                    None if s.contacts.len() < MAX_CONTACTS => {
                        let slot = free_slot(&s.contacts);
                        s.contacts.push(Contact {
                            id: ev.code,
                            slot,
                            x,
                            y,
                        });
                        inject_touch_frame(s, Some((ev.code, POINTER_FLAG_DOWN)));
                    }
                    None => {}
                }
            }
            InputKind::TouchUp => {
                let Some(idx) = s.contacts.iter().position(|c| c.id == ev.code) else {
                    return;
                };
                // The UP frame still carries the lifting contact (with UP flags), then it
                // leaves the active set.
                inject_touch_frame(s, Some((ev.code, POINTER_FLAG_UP)));
                s.contacts.remove(idx);
            }
            _ => {}
        }
    }
}

impl Drop for SyntheticTouch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.refresher.take() {
            let _ = h.join();
        }
    }
}

/// Inject the full active-contact frame; `edge` marks one contact's DOWN/UP transition (by
/// WIRE id — everyone else is a held UPDATE). Windows sees the compacted `slot` ids only.
fn inject_touch_frame(sh: &mut TouchShared, edge: Option<(u32, POINTER_FLAGS)>) {
    let contacts = &sh.contacts;
    if contacts.is_empty() {
        return;
    }
    let mut frame: Vec<POINTER_TYPE_INFO> = Vec::with_capacity(contacts.len());
    for c in contacts {
        let mut flags = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE;
        if let Some((id, e)) = edge {
            if id == c.id {
                if e == POINTER_FLAG_DOWN {
                    flags = POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN;
                } else if e == POINTER_FLAG_UP {
                    flags = POINTER_FLAG_UP; // contact + range end together
                }
            }
        }
        let pt = to_screen(c.x, c.y);
        frame.push(POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 {
                touchInfo: POINTER_TOUCH_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_TOUCH,
                        pointerId: c.slot,
                        pointerFlags: flags,
                        ptPixelLocation: pt,
                        ptPixelLocationRaw: pt,
                        ..Default::default()
                    },
                    touchFlags: TOUCH_FLAG_NONE,
                    touchMask: TOUCH_MASK_NONE,
                    ..Default::default()
                },
            },
        });
    }
    // SAFETY: `sh.dev.0` is the live owned device; `frame` is a live Vec the call only reads.
    // Best-effort — a transient failure heals on the next event/refresh re-assertion.
    if let Err(e) = unsafe { InjectSyntheticPointerInput(sh.dev.0, &frame) } {
        if !sh.fail_warned {
            sh.fail_warned = true;
            tracing::warn!(error = %e, contacts = frame.len(), "touch inject failed");
        } else {
            tracing::trace!(error = %e, "touch inject failed (transient)");
        }
    } else {
        sh.fail_warned = false;
    }
}
