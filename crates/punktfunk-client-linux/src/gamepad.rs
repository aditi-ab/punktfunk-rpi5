//! App-lifetime gamepad service over SDL3 (mirrors the Swift client's `GamepadManager` +
//! `GamepadCapture`/`GamepadFeedback`).
//!
//! One worker thread owns SDL for the process lifetime: it tracks connected pads for the
//! Settings UI, selects the ONE controller forwarded as pad 0 (user pin, else the most
//! recently connected), and — while a session is attached — forwards buttons/axes,
//! DualSense touchpad contacts and motion samples (0xCC), and renders feedback: rumble on
//! every pad, lightbar via SDL, and on a real DualSense the raw effects packet
//! (adaptive-trigger blocks replayed verbatim, player LEDs). Held state is zeroed on the
//! wire when the active pad switches or the session detaches, so nothing sticks down.
//!
//! This thread is also the single consumer of the rumble and HID-output pull planes.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Motion scale constants, shared convention with the Swift client (`GamepadWire`):
/// derived from hid-playstation's math over the host's fixed calibration blob. SDL hands
/// us gyro in rad/s and accel in m/s²; the DualSense report wants raw LSBs.
const GYRO_LSB_PER_RAD_S: f32 = 20.0 * 180.0 / std::f32::consts::PI;
const ACCEL_LSB_PER_G: f32 = 10_000.0;
const G: f32 = 9.80665;

#[derive(Clone, Debug)]
pub struct PadInfo {
    pub id: u32,
    pub name: String,
    pub is_dualsense: bool,
}

enum Ctl {
    Attach(Arc<NativeClient>),
    Detach,
    Pin(Option<u32>),
}

#[derive(Clone)]
pub struct GamepadService {
    pads: Arc<Mutex<Vec<PadInfo>>>,
    active: Arc<Mutex<Option<PadInfo>>>,
    pinned: Arc<Mutex<Option<u32>>>,
    ctl: Sender<Ctl>,
}

impl GamepadService {
    pub fn start() -> GamepadService {
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let pinned = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (p, a, pin) = (pads.clone(), active.clone(), pinned.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-gamepad".into())
            .spawn(move || {
                if let Err(e) = run(&p, &a, &pin, &ctl_rx) {
                    tracing::warn!(error = %e, "gamepad service ended — pads disabled");
                }
            })
        {
            tracing::warn!(error = %e, "gamepad service failed to start");
        }
        GamepadService {
            pads,
            active,
            pinned,
            ctl,
        }
    }

    pub fn pads(&self) -> Vec<PadInfo> {
        self.pads.lock().unwrap().clone()
    }

    pub fn active(&self) -> Option<PadInfo> {
        self.active.lock().unwrap().clone()
    }

    pub fn pinned(&self) -> Option<u32> {
        *self.pinned.lock().unwrap()
    }

    pub fn set_pinned(&self, id: Option<u32>) {
        let _ = self.ctl.send(Ctl::Pin(id));
    }

    pub fn attach(&self, connector: Arc<NativeClient>) {
        let _ = self.ctl.send(Ctl::Attach(connector));
    }

    pub fn detach(&self) {
        let _ = self.ctl.send(Ctl::Detach);
    }

    /// What "Automatic" resolves to right now — the virtual pad matching the physical one
    /// (Swift parity); no pad connected leaves the host's own default.
    pub fn auto_pref(&self) -> GamepadPref {
        match self.active() {
            Some(p) if p.is_dualsense => GamepadPref::DualSense,
            Some(_) => GamepadPref::Xbox360,
            None => GamepadPref::Auto,
        }
    }
}

fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y: 0,
        flags: 0, // pad index 0 — single-pad model
    });
}

fn button_bit(b: sdl3::gamepad::Button) -> Option<u32> {
    use sdl3::gamepad::Button;
    Some(match b {
        Button::South => wire::BTN_A,
        Button::East => wire::BTN_B,
        Button::West => wire::BTN_X,
        Button::North => wire::BTN_Y,
        Button::Back => wire::BTN_BACK,
        Button::Start => wire::BTN_START,
        Button::Guide => wire::BTN_GUIDE,
        Button::LeftStick => wire::BTN_LS_CLICK,
        Button::RightStick => wire::BTN_RS_CLICK,
        Button::LeftShoulder => wire::BTN_LB,
        Button::RightShoulder => wire::BTN_RB,
        Button::DPadUp => wire::BTN_DPAD_UP,
        Button::DPadDown => wire::BTN_DPAD_DOWN,
        Button::DPadLeft => wire::BTN_DPAD_LEFT,
        Button::DPadRight => wire::BTN_DPAD_RIGHT,
        Button::Touchpad => wire::BTN_TOUCHPAD,
        _ => return None,
    })
}

/// SDL axis → (wire axis id, wire value). SDL sticks are +y = down; the wire (XInput
/// convention) is +y = up. SDL triggers span 0..32767; the wire wants 0..255.
fn axis_value(axis: sdl3::gamepad::Axis, v: i16) -> (u32, i32) {
    use sdl3::gamepad::Axis;
    match axis {
        Axis::LeftX => (wire::AXIS_LS_X, v as i32),
        Axis::LeftY => (wire::AXIS_LS_Y, -(v as i32).max(-32767)),
        Axis::RightX => (wire::AXIS_RS_X, v as i32),
        Axis::RightY => (wire::AXIS_RS_Y, -(v as i32).max(-32767)),
        Axis::TriggerLeft => (wire::AXIS_LT, (v as i32).clamp(0, 32767) >> 7),
        Axis::TriggerRight => (wire::AXIS_RT, (v as i32).clamp(0, 32767) >> 7),
    }
}

/// The DualSense effects packet (SDL `DS5EffectsState_t`, 47 bytes) — the same layout the
/// host parses off its virtual pad; the wire's 11-byte trigger blocks drop in verbatim.
/// Enable bits select only the fields each update touches, so rumble (driven separately
/// through SDL) and untouched fields keep their state.
#[derive(Default)]
struct Ds5Feedback;

impl Ds5Feedback {
    const RIGHT_TRIGGER: usize = 10;
    const LEFT_TRIGGER: usize = 21;
    const PAD_LIGHTS: usize = 43;
    const LED_RGB: usize = 44;

    fn trigger_packet(which: u8, effect: &[u8]) -> [u8; 47] {
        let mut p = [0u8; 47];
        let (flag, off) = if which == 1 {
            (0x04, Self::RIGHT_TRIGGER)
        } else {
            (0x08, Self::LEFT_TRIGGER)
        };
        p[0] = flag;
        let n = effect.len().min(11);
        p[off..off + n].copy_from_slice(&effect[..n]);
        p
    }

    fn lightbar_packet(r: u8, g: u8, b: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x04; // lightbar enable
        p[Self::LED_RGB] = r;
        p[Self::LED_RGB + 1] = g;
        p[Self::LED_RGB + 2] = b;
        p
    }

    fn player_packet(bits: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[1] = 0x10; // player-LED enable
        p[Self::PAD_LIGHTS] = bits & 0x1F;
        p
    }
}

struct Worker {
    subsystem: sdl3::GamepadSubsystem,
    opened: HashMap<u32, sdl3::gamepad::Gamepad>,
    /// Connection order; the most recently connected is the auto selection.
    order: Vec<u32>,
    pinned: Option<u32>,
    attached: Option<Arc<NativeClient>>,
    /// Wire state of the active pad — zeroed on the wire at switch/detach.
    last_axis: [i32; 6],
    held_buttons: Vec<u32>,
    last_accel: [i16; 3],
}

impl Worker {
    fn active_id(&self) -> Option<u32> {
        self.pinned
            .filter(|id| self.opened.contains_key(id))
            .or_else(|| self.order.last().copied())
    }

    fn pad_info(&self, id: u32) -> Option<PadInfo> {
        let pad = self.opened.get(&id)?;
        Some(PadInfo {
            id,
            name: pad.name().unwrap_or_else(|| "Controller".into()),
            is_dualsense: matches!(
                self.subsystem
                    .type_for_id(sdl3::sys::joystick::SDL_JoystickID(id)),
                sdl3::gamepad::GamepadType::PS5
            ),
        })
    }

    /// Zero everything the host believes is held — on pad switch and detach.
    fn flush_held(&mut self) {
        if let Some(c) = &self.attached {
            for b in self.held_buttons.drain(..) {
                send(c, InputKind::GamepadButton, b, 0);
            }
            for (id, v) in self.last_axis.iter_mut().enumerate() {
                if *v != 0 && *v != i32::MIN {
                    send(c, InputKind::GamepadAxis, id as u32, 0);
                }
                *v = i32::MIN;
            }
        } else {
            self.held_buttons.clear();
            self.last_axis = [i32::MIN; 6];
        }
    }

    /// Sensors stream only while a session wants them (they cost USB/BT bandwidth).
    fn set_sensors(&mut self, enabled: bool) {
        let Some(id) = self.active_id() else { return };
        if let Some(pad) = self.opened.get_mut(&id) {
            use sdl3::sensor::SensorType;
            for s in [SensorType::Gyroscope, SensorType::Accelerometer] {
                if unsafe { pad.has_sensor(s) } {
                    let _ = pad.sensor_set_enabled(s, enabled);
                }
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(
    pads_out: &Mutex<Vec<PadInfo>>,
    active_out: &Mutex<Option<PadInfo>>,
    pinned_out: &Mutex<Option<u32>>,
    ctl: &Receiver<Ctl>,
) -> Result<(), String> {
    // Off-main-thread + no video subsystem: keep SDL away from signals, poll pads on its
    // own thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;

    let mut w = Worker {
        subsystem,
        opened: HashMap::new(),
        order: Vec::new(),
        pinned: None,
        attached: None,
        last_axis: [i32::MIN; 6],
        held_buttons: Vec::new(),
        last_accel: [0; 3],
    };

    let publish = |w: &Worker| {
        let mut list: Vec<PadInfo> = w.order.iter().filter_map(|&id| w.pad_info(id)).collect();
        list.reverse(); // most recent first — the Settings list order
        *pads_out.lock().unwrap() = list;
        *active_out.lock().unwrap() = w.active_id().and_then(|id| w.pad_info(id));
        *pinned_out.lock().unwrap() = w.pinned;
    };

    loop {
        // Control plane from the UI thread.
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Attach(c)) => {
                    w.attached = Some(c);
                    w.last_axis = [i32::MIN; 6];
                    w.set_sensors(true);
                }
                Ok(Ctl::Detach) => {
                    w.flush_held();
                    w.set_sensors(false);
                    w.attached = None;
                }
                Ok(Ctl::Pin(id)) => {
                    let before = w.active_id();
                    w.pinned = id;
                    if w.active_id() != before {
                        w.flush_held();
                        if w.attached.is_some() {
                            w.set_sensors(true);
                        }
                    }
                    publish(&w);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()), // app gone
            }
        }

        while let Some(event) = pump.poll_event() {
            use sdl3::event::Event;
            let active = w.active_id();
            match event {
                Event::ControllerDeviceAdded { which, .. } => {
                    if !w.opened.contains_key(&which) {
                        match w.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(which)) {
                            Ok(pad) => {
                                tracing::info!(
                                    name = pad.name().unwrap_or_default(),
                                    "gamepad attached"
                                );
                                w.opened.insert(which, pad);
                                w.order.push(which);
                                if w.attached.is_some() && w.active_id() == Some(which) {
                                    w.set_sensors(true);
                                }
                                publish(&w);
                            }
                            Err(e) => tracing::warn!(error = %e, "gamepad open failed"),
                        }
                    }
                }
                Event::ControllerDeviceRemoved { which, .. } => {
                    if w.opened.remove(&which).is_some() {
                        w.order.retain(|&id| id != which);
                        if active == Some(which) {
                            w.flush_held();
                        }
                        tracing::info!("gamepad detached");
                        publish(&w);
                    }
                }
                Event::ControllerButtonDown { which, button, .. }
                    if active == Some(which) && w.attached.is_some() =>
                {
                    if let Some(bit) = button_bit(button) {
                        w.held_buttons.push(bit);
                        send(
                            w.attached.as_ref().unwrap(),
                            InputKind::GamepadButton,
                            bit,
                            1,
                        );
                    }
                }
                Event::ControllerButtonUp { which, button, .. }
                    if active == Some(which) && w.attached.is_some() =>
                {
                    if let Some(bit) = button_bit(button) {
                        w.held_buttons.retain(|&b| b != bit);
                        send(
                            w.attached.as_ref().unwrap(),
                            InputKind::GamepadButton,
                            bit,
                            0,
                        );
                    }
                }
                Event::ControllerAxisMotion {
                    which, axis, value, ..
                } if active == Some(which) && w.attached.is_some() => {
                    let (id, v) = axis_value(axis, value);
                    if w.last_axis[id as usize] != v {
                        w.last_axis[id as usize] = v;
                        send(w.attached.as_ref().unwrap(), InputKind::GamepadAxis, id, v);
                    }
                }
                // DualSense touchpad → the rich-input plane, normalized 0..=65535.
                Event::ControllerTouchpadDown {
                    which,
                    finger,
                    x,
                    y,
                    ..
                }
                | Event::ControllerTouchpadMotion {
                    which,
                    finger,
                    x,
                    y,
                    ..
                } if active == Some(which) && w.attached.is_some() => {
                    let _ = w
                        .attached
                        .as_ref()
                        .unwrap()
                        .send_rich_input(RichInput::Touchpad {
                            pad: 0,
                            finger: finger as u8,
                            active: true,
                            x: (x.clamp(0.0, 1.0) * 65535.0) as u16,
                            y: (y.clamp(0.0, 1.0) * 65535.0) as u16,
                        });
                }
                Event::ControllerTouchpadUp {
                    which,
                    finger,
                    x,
                    y,
                    ..
                } if active == Some(which) && w.attached.is_some() => {
                    let _ = w
                        .attached
                        .as_ref()
                        .unwrap()
                        .send_rich_input(RichInput::Touchpad {
                            pad: 0,
                            finger: finger as u8,
                            active: false,
                            x: (x.clamp(0.0, 1.0) * 65535.0) as u16,
                            y: (y.clamp(0.0, 1.0) * 65535.0) as u16,
                        });
                }
                // Motion: accel events update the cache; each gyro event ships a sample
                // (the DualSense reports both at ~250 Hz). Scale convention shared with
                // the Swift client — sign/scale derived, not yet live-verified.
                Event::ControllerSensorUpdated {
                    which,
                    sensor,
                    data,
                    ..
                } if active == Some(which) && w.attached.is_some() => {
                    use sdl3::sensor::SensorType;
                    match sensor {
                        SensorType::Accelerometer => {
                            for (i, v) in data.iter().enumerate() {
                                w.last_accel[i] =
                                    (v / G * ACCEL_LSB_PER_G).clamp(-32768.0, 32767.0) as i16;
                            }
                        }
                        SensorType::Gyroscope => {
                            let mut gyro = [0i16; 3];
                            for (i, v) in data.iter().enumerate() {
                                gyro[i] = (v * GYRO_LSB_PER_RAD_S).clamp(-32768.0, 32767.0) as i16;
                            }
                            let _ =
                                w.attached
                                    .as_ref()
                                    .unwrap()
                                    .send_rich_input(RichInput::Motion {
                                        pad: 0,
                                        gyro,
                                        accel: w.last_accel,
                                    });
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }

        // Feedback planes (this thread is their single consumer). The host re-sends
        // rumble state periodically, so a generous duration with refresh-on-update is
        // safe — a dropped stop heals within ~500 ms.
        if let Some(connector) = w.attached.clone() {
            while let Ok((pad, low, high)) = connector.next_rumble(Duration::ZERO) {
                if pad == 0 {
                    if let Some(p) = w.active_id().and_then(|id| w.opened.get_mut(&id)) {
                        let _ = p.set_rumble(low, high, 5_000);
                    }
                }
            }
            while let Ok(hid) = connector.next_hidout(Duration::ZERO) {
                let Some(id) = w.active_id() else { continue };
                let is_ds = w.pad_info(id).is_some_and(|p| p.is_dualsense);
                let Some(pad) = w.opened.get_mut(&id) else {
                    continue;
                };
                match hid {
                    HidOutput::Led { pad: 0, r, g, b } if is_ds => {
                        let _ = pad.send_effect(&Ds5Feedback::lightbar_packet(r, g, b));
                    }
                    HidOutput::Led { pad: 0, r, g, b } => {
                        let _ = pad.set_led(r, g, b);
                    }
                    HidOutput::PlayerLeds { pad: 0, bits } if is_ds => {
                        let _ = pad.send_effect(&Ds5Feedback::player_packet(bits));
                    }
                    HidOutput::Trigger {
                        pad: 0,
                        which,
                        ref effect,
                    } if is_ds => {
                        let _ = pad.send_effect(&Ds5Feedback::trigger_packet(which, effect));
                    }
                    _ => {}
                }
            }
        }

        std::thread::sleep(Duration::from_millis(if w.attached.is_some() {
            2
        } else {
            30
        }));
    }
}
