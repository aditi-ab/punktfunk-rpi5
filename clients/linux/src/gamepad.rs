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
use std::time::{Duration, Instant};

/// Motion scale constants, shared convention with the Swift client (`GamepadWire`):
/// derived from hid-playstation's math over the host's fixed calibration blob. SDL hands
/// us gyro in rad/s and accel in m/s²; the DualSense report wants raw LSBs.
const GYRO_LSB_PER_RAD_S: f32 = 20.0 * 180.0 / std::f32::consts::PI;
const ACCEL_LSB_PER_G: f32 = 10_000.0;
const G: f32 = 9.80665;

/// The controller "escape" chord (Moonlight convention): L1 + R1 + Start + Select held
/// together. Intercepted by the client to leave fullscreen + release input capture — the
/// Deck has no F11 key and fullscreen hides the window chrome, so with a controller this
/// is the only way out. Four simultaneous buttons that no game uses as a deliberate
/// combo, so it can't be triggered by normal play. Still forwarded to the host (the user
/// is leaving anyway); we only also raise the escape signal.
///
/// **Escalation:** a quick press leaves fullscreen / releases capture; *holding* the same
/// chord for [`DISCONNECT_HOLD`] ends the session. Deliberately NOT the Steam / QAM buttons —
/// those are the marquee pass-through controls that now reach the host's game-mode UI.
const ESCAPE_CHORD: [u32; 4] = [wire::BTN_LB, wire::BTN_RB, wire::BTN_START, wire::BTN_BACK];

/// Hold the [`ESCAPE_CHORD`] at least this long to disconnect (escalates the leave-fullscreen press).
const DISCONNECT_HOLD: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug)]
pub struct PadInfo {
    pub id: u32,
    pub name: String,
    /// The virtual pad "Automatic" resolves to for this physical controller (so the host creates a
    /// matching pad: DualSense → DualSense, DS4 → DualShock 4, Xbox One/Series → Xbox One, anything
    /// else → Xbox 360). Drives [`GamepadService::auto_pref`] and the rich-feedback render path.
    pub pref: GamepadPref,
}

impl PadInfo {
    /// True for a real DualSense — the only pad whose lightbar / player-LED / adaptive-trigger
    /// feedback we replay as raw DS5 HID effect packets (a DS4 uses SDL's generic `set_led`).
    fn is_dualsense(&self) -> bool {
        self.pref == GamepadPref::DualSense
    }

    /// A short controller-kind label for the Settings list (`""` for a plain Xbox/standard pad).
    pub fn kind_label(&self) -> &'static str {
        match self.pref {
            GamepadPref::DualSense => "DualSense",
            GamepadPref::DualShock4 => "DualShock 4",
            GamepadPref::XboxOne => "Xbox One",
            GamepadPref::SteamDeck => "Steam Deck",
            _ => "",
        }
    }
}

/// Map the SDL-reported controller type to the virtual pad we'd ask the host to create.
fn pref_for_type(t: sdl3::gamepad::GamepadType) -> GamepadPref {
    use sdl3::gamepad::GamepadType as T;
    match t {
        T::PS5 => GamepadPref::DualSense,
        T::PS4 => GamepadPref::DualShock4,
        T::XboxOne => GamepadPref::XboxOne,
        _ => GamepadPref::Xbox360,
    }
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
    /// Fires once per press of the [`ESCAPE_CHORD`]; the stream page consumes it to leave
    /// fullscreen + release capture.
    escape_rx: async_channel::Receiver<()>,
    /// Fires once when the [`ESCAPE_CHORD`] is held past [`DISCONNECT_HOLD`]; the stream page
    /// consumes it to end the session (the controller equivalent of Ctrl+Alt+Shift+D).
    disconnect_rx: async_channel::Receiver<()>,
}

impl GamepadService {
    pub fn start() -> GamepadService {
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let pinned = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (p, a, pin) = (pads.clone(), active.clone(), pinned.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-gamepad".into())
            .spawn(move || {
                if let Err(e) = run(&p, &a, &pin, &ctl_rx, &escape_tx, &disconnect_tx) {
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
            escape_rx,
            disconnect_rx,
        }
    }

    /// A receiver that yields one `()` each time the controller escape chord is pressed.
    /// A fresh clone per call (shared mpmc channel); the stream page spawns a future on it.
    pub fn escape_events(&self) -> async_channel::Receiver<()> {
        self.escape_rx.clone()
    }

    /// A receiver that yields one `()` when the escape chord is held past [`DISCONNECT_HOLD`]
    /// (controller disconnect). A fresh clone per call; the stream page spawns a future on it.
    pub fn disconnect_events(&self) -> async_channel::Receiver<()> {
        self.disconnect_rx.clone()
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
            Some(p) => p.pref,
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
        // Back grips / paddles (Steam Deck L4/L5/R4/R5, Xbox Elite P1–P4) + the misc/Share button.
        // PADDLE1/2/3/4 = R4/L4/R5/L5 (see the host `input::gamepad`).
        Button::RightPaddle1 => wire::BTN_PADDLE1,
        Button::LeftPaddle1 => wire::BTN_PADDLE2,
        Button::RightPaddle2 => wire::BTN_PADDLE3,
        Button::LeftPaddle2 => wire::BTN_PADDLE4,
        Button::Misc1 => wire::BTN_MISC1,
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
    /// Touchpad contacts the host believes are down, keyed by `(surface, finger)` — lifted on pad
    /// switch / detach so a contact held at that moment doesn't stick. surface 0 = the legacy single
    /// touchpad, 1/2 = a Steam left/right pad.
    held_touches: std::collections::HashSet<(u8, u8)>,
    last_accel: [i16; 3],
    /// Raises the UI escape signal; the escape chord fires it once per press.
    escape_tx: async_channel::Sender<()>,
    /// Raises the UI disconnect signal when the escape chord is held past [`DISCONNECT_HOLD`].
    disconnect_tx: async_channel::Sender<()>,
    /// The escape chord is fully held — latched so it fires once, not every poll.
    chord_armed: bool,
    /// When the escape chord became fully held (drives the hold-to-disconnect escalation); `None`
    /// when the chord is broken.
    chord_since: Option<Instant>,
    /// The disconnect signal already fired for the current hold — latched so it fires once.
    disconnect_fired: bool,
}

impl Worker {
    fn active_id(&self) -> Option<u32> {
        self.pinned
            .filter(|id| self.opened.contains_key(id))
            .or_else(|| self.order.last().copied())
    }

    fn pad_info(&self, id: u32) -> Option<PadInfo> {
        let pad = self.opened.get(&id)?;
        let mut pref = pref_for_type(
            self.subsystem
                .type_for_id(sdl3::sys::joystick::SDL_JoystickID(id)),
        );
        // There is no SDL gamepad type for the Steam Deck / Steam Controller, so detect Valve by
        // VID/PID (Deck 0x1205, SC wired 0x1102, SC dongle 0x1142) — the host then builds the virtual
        // hid-steam pad with the back grips + dual trackpads and the right glyph identity.
        if pad.vendor_id() == Some(0x28DE)
            && matches!(pad.product_id(), Some(0x1205 | 0x1102 | 0x1142))
        {
            pref = GamepadPref::SteamDeck;
        }
        Some(PadInfo {
            id,
            name: pad.name().unwrap_or_else(|| "Controller".into()),
            pref,
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
            // Lift any touchpad contact the host still believes is down (surface 0 = legacy pad).
            for (surface, finger) in self.held_touches.drain() {
                let rich = if surface == 0 {
                    RichInput::Touchpad {
                        pad: 0,
                        finger,
                        active: false,
                        x: 0,
                        y: 0,
                    }
                } else {
                    RichInput::TouchpadEx {
                        pad: 0,
                        surface,
                        finger,
                        touch: false,
                        click: false,
                        x: 0,
                        y: 0,
                        pressure: 0,
                    }
                };
                let _ = c.send_rich_input(rich);
            }
        } else {
            self.held_buttons.clear();
            self.last_axis = [i32::MIN; 6];
            self.held_touches.clear();
        }
        // A held chord doesn't survive a flush (detach / pad-switch) — clear its latches too.
        self.reset_chord();
    }

    /// Raise the UI escape signal when the [`ESCAPE_CHORD`] just completed (latched so it
    /// fires once per press) and start the hold-to-disconnect timer. Called after each
    /// button-down updates `held_buttons`.
    fn maybe_fire_escape(&mut self) {
        if self.chord_armed {
            return;
        }
        if ESCAPE_CHORD.iter().all(|b| self.held_buttons.contains(b)) {
            self.chord_armed = true;
            self.chord_since = Some(Instant::now());
            let _ = self.escape_tx.try_send(());
            tracing::info!(
                "gamepad escape chord (L1+R1+Start+Select) — leaving fullscreen (hold to disconnect)"
            );
        }
    }

    /// Fire the disconnect signal once the escape chord has been continuously held past
    /// [`DISCONNECT_HOLD`]. Polled from the main loop so the hold completes without new events.
    fn maybe_fire_disconnect(&mut self) {
        if self.disconnect_fired {
            return;
        }
        if let Some(since) = self.chord_since {
            if since.elapsed() >= DISCONNECT_HOLD {
                self.disconnect_fired = true;
                let _ = self.disconnect_tx.try_send(());
                tracing::info!("gamepad escape chord held — disconnecting");
            }
        }
    }

    /// Re-arm once the chord is broken (any of its buttons released).
    fn rearm_escape(&mut self) {
        if self.chord_armed && !ESCAPE_CHORD.iter().all(|b| self.held_buttons.contains(b)) {
            self.reset_chord();
        }
    }

    /// Clear the escape/disconnect chord latches. Called at every session boundary
    /// ([`flush_held`](Self::flush_held) on detach/pad-switch + on attach): the hold-to-disconnect
    /// path *always* ends the session while the chord is still physically held, so the matching
    /// button-up events arrive after detach (dropped by the `attached` guard) and `rearm_escape`
    /// never runs — without this the latched state would leak into the next session and either
    /// swallow its first chord press or fire a stale disconnect on connect.
    fn reset_chord(&mut self) {
        self.chord_armed = false;
        self.chord_since = None;
        self.disconnect_fired = false;
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

    /// Forward one touchpad contact on the rich-input plane. A multi-touchpad pad (Steam Deck / Steam
    /// Controller) sends `TouchpadEx` with the surface (SDL touchpad 0 = left → 1, 1 = right → 2) and
    /// signed coordinates; a single-touchpad pad (DualSense) keeps the legacy `Touchpad` (unsigned).
    fn forward_touch(
        &mut self,
        which: u32,
        touchpad: u32,
        finger: u8,
        x: f32,
        y: f32,
        active: bool,
    ) {
        let Some(c) = self.attached.as_ref() else {
            return;
        };
        let multi = self
            .opened
            .get(&which)
            .map(|p| p.touchpads_count() >= 2)
            .unwrap_or(false);
        let (cx, cy) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let surface = if multi { (touchpad as u8) + 1 } else { 0 };
        let rich = if multi {
            RichInput::TouchpadEx {
                pad: 0,
                surface,
                finger,
                touch: active,
                click: false,
                x: (cx * 65535.0 - 32768.0) as i16,
                y: (cy * 65535.0 - 32768.0) as i16,
                pressure: 0,
            }
        } else {
            RichInput::Touchpad {
                pad: 0,
                finger,
                active,
                x: (cx * 65535.0) as u16,
                y: (cy * 65535.0) as u16,
            }
        };
        let _ = c.send_rich_input(rich);
        if active {
            self.held_touches.insert((surface, finger));
        } else {
            self.held_touches.remove(&(surface, finger));
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(
    pads_out: &Mutex<Vec<PadInfo>>,
    active_out: &Mutex<Option<PadInfo>>,
    pinned_out: &Mutex<Option<u32>>,
    ctl: &Receiver<Ctl>,
    escape_tx: &async_channel::Sender<()>,
    disconnect_tx: &async_channel::Sender<()>,
) -> Result<(), String> {
    // Off-main-thread + no video subsystem: keep SDL away from signals, poll pads on its
    // own thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // Let SDL's HIDAPI drivers open Valve Steam Controller / Steam Deck devices directly, so the
    // paddles, both trackpads, and gyro arrive as first-class SDL gamepad inputs. On a Deck in Game
    // Mode, Steam Input still holds the device — the user must disable Steam Input for this app (see
    // the Decky UX); on a desktop client (or a Deck with Steam Input off) the hints just work.
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAMDECK", "1");
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAM", "1");
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
        held_touches: std::collections::HashSet::new(),
        last_accel: [0; 3],
        escape_tx: escape_tx.clone(),
        disconnect_tx: disconnect_tx.clone(),
        chord_armed: false,
        chord_since: None,
        disconnect_fired: false,
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
                    w.reset_chord(); // every session starts un-latched (Attach doesn't flush)
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
                        w.maybe_fire_escape();
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
                        w.rearm_escape();
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
                // Touchpad contacts → the rich-input plane. One pad (DualSense) keeps the legacy
                // `Touchpad`; two pads (Steam Deck / Steam Controller) send `TouchpadEx` per surface.
                Event::ControllerTouchpadDown {
                    which,
                    touchpad,
                    finger,
                    x,
                    y,
                    ..
                }
                | Event::ControllerTouchpadMotion {
                    which,
                    touchpad,
                    finger,
                    x,
                    y,
                    ..
                } if active == Some(which) && w.attached.is_some() => {
                    w.forward_touch(which, touchpad as u32, finger as u8, x, y, true);
                }
                Event::ControllerTouchpadUp {
                    which,
                    touchpad,
                    finger,
                    x,
                    y,
                    ..
                } if active == Some(which) && w.attached.is_some() => {
                    w.forward_touch(which, touchpad as u32, finger as u8, x, y, false);
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

        // Escalate a held escape chord to a disconnect (polled — the hold completes with no
        // new button events; the chord itself is only detected while a session is attached).
        w.maybe_fire_disconnect();

        // Feedback planes (this thread is their single consumer). The host re-sends
        // rumble state periodically, so a generous duration with refresh-on-update is
        // safe — a dropped stop heals within ~500 ms.
        if let Some(connector) = w.attached.clone() {
            while let Ok((pad, low, high)) = connector.next_rumble(Duration::ZERO) {
                if pad == 0 {
                    if let Some(p) = w.active_id().and_then(|id| w.opened.get_mut(&id)) {
                        // Surface a failed SDL rumble write: a swallowed error here (DualSense not in
                        // the right HIDAPI mode, etc.) reads exactly like "rumble doesn't work". The
                        // host logs the send side on 0xCA, so the two together pinpoint host-game vs
                        // client-render.
                        if let Err(e) = p.set_rumble(low, high, 5_000) {
                            tracing::warn!(low, high, error = %e, "rumble: SDL set_rumble failed");
                        } else {
                            tracing::debug!(low, high, "rumble: rendered");
                        }
                    } else {
                        tracing::debug!(low, high, "rumble: received but no active pad to render");
                    }
                }
            }
            while let Ok(hid) = connector.next_hidout(Duration::ZERO) {
                let Some(id) = w.active_id() else { continue };
                let is_ds = w.pad_info(id).is_some_and(|p| p.is_dualsense());
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
