//! App-lifetime gamepad service over SDL3 (mirrors the Swift client's `GamepadManager` +
//! `GamepadCapture`/`GamepadFeedback`).
//!
//! One worker thread owns SDL for the process lifetime: it tracks connected pads for the
//! Settings UI (metadata only — see below), selects the ONE controller forwarded as pad 0
//! (the user pin — persisted in Settings by stable `vid:pid:name` key — else the most
//! recently connected real pad; Steam Input's virtual pad is skipped), and — while a
//! session is attached — forwards buttons/axes, DualSense touchpad contacts and motion
//! samples (0xCC), and renders feedback: rumble, lightbar via SDL, and on a real DualSense
//! the raw effects packet (adaptive-trigger blocks replayed verbatim, player LEDs). Held
//! state is zeroed on the wire when the active pad switches or the session detaches, so
//! nothing sticks down.
//!
//! **Idle means hands off the hardware.** Outside an attached session the worker never
//! opens a device and keeps SDL's Valve HIDAPI drivers disabled ([`set_valve_hidapi`]):
//! the Steam Deck driver clears the built-in controller's "lizard mode" (trackpad-mouse,
//! clicky pads) the moment the device *enumerates* and keeps feeding that watchdog — so an
//! idle host-list window would kill the Deck's system input. The pad list for Settings is
//! built from SDL's ID-based metadata getters, which need no open.
//!
//! This thread is also the single consumer of the rumble and HID-output pull planes.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::{HidOutput, RichInput};
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
    pub name: String,
    /// Stable identity (`vid:pid:name`) for pinning across restarts — SDL instance ids are
    /// per-run, so [`Settings::forward_pad`](crate::trust::Settings) persists this instead.
    pub key: String,
    /// The virtual pad "Automatic" resolves to for this physical controller (so the host creates a
    /// matching pad: DualSense → DualSense, DS4 → DualShock 4, Xbox One/Series → Xbox One, anything
    /// else → Xbox 360). Drives [`GamepadService::auto_pref`] and the rich-feedback render path.
    pub pref: GamepadPref,
    /// Steam Input's emulated pad ("Steam Virtual Gamepad", Valve 28de:11ff). It shadows the
    /// physical controller and has no sensors/touchpad, so auto-selection skips it while a real
    /// pad is connected — otherwise gyro silently dies on Bazzite/Deck game mode.
    pub steam_virtual: bool,
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

/// Enable/disable SDL's Valve HIDAPI drivers at runtime. The Steam Deck driver sends
/// `ID_CLEAR_DIGITAL_MAPPINGS` + `TRACKPAD_NONE` in `InitDevice` — at *enumeration*, before
/// any open — and its `UpdateDevice` keeps feeding the firmware's lizard-mode watchdog
/// (`SDL_hidapi_steamdeck.c`), so a Deck's built-in trackpad-mouse dies for the whole
/// system while the driver merely runs. These drivers therefore run ONLY while a session
/// is attached (input is captured then anyway, and streaming wants the paddles, both
/// trackpads, and gyro first-class). SDL3 applies the hint changes live: disabling detaches
/// the driver and the firmware watchdog restores lizard mode within seconds.
///
/// On a Deck in Game Mode, Steam Input still holds the device — the user must disable
/// Steam Input for this app (see the Decky UX); on a desktop client (or a Deck with Steam
/// Input off) the in-session enable just works.
fn set_valve_hidapi(enabled: bool) {
    let v = if enabled { "1" } else { "0" };
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAMDECK", v);
    sdl3::hint::set("SDL_JOYSTICK_HIDAPI_STEAM", v);
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

/// Best-effort "this machine is a Steam Deck". The Gaming-Mode env short-circuits; desktop
/// mode falls back to DMI (Valve board, Jupiter = LCD / Galileo = OLED — readable inside the
/// flatpak sandbox). Cached: the answer can't change while we run.
pub fn is_steam_deck() -> bool {
    static DECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DECK.get_or_init(|| {
        if std::env::var_os("SteamDeck").is_some() {
            return true;
        }
        let dmi = |f: &str| std::fs::read_to_string(format!("/sys/class/dmi/id/{f}"));
        dmi("board_vendor").is_ok_and(|v| v.trim() == "Valve")
            && dmi("product_name").is_ok_and(|p| matches!(p.trim(), "Jupiter" | "Galileo"))
    })
}

enum Ctl {
    Attach(Arc<NativeClient>),
    Detach,
    Pin(Option<String>),
}

#[derive(Clone)]
pub struct GamepadService {
    pads: Arc<Mutex<Vec<PadInfo>>>,
    active: Arc<Mutex<Option<PadInfo>>>,
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
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (p, a) = (pads.clone(), active.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-gamepad".into())
            .spawn(move || {
                if let Err(e) = run(&p, &a, &ctl_rx, &escape_tx, &disconnect_tx) {
                    tracing::warn!(error = %e, "gamepad service ended — pads disabled");
                }
            })
        {
            tracing::warn!(error = %e, "gamepad service failed to start");
        }
        GamepadService {
            pads,
            active,
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

    /// Pin the forwarded controller by stable key (`PadInfo::key`) — `None` = automatic.
    /// The pin persists as `Settings::forward_pad` (the UI's source of truth) and survives
    /// the pad disconnecting: it re-applies the moment a matching controller shows up again.
    pub fn set_pinned(&self, key: Option<String>) {
        let _ = self.ctl.send(Ctl::Pin(key));
    }

    pub fn attach(&self, connector: Arc<NativeClient>) {
        let _ = self.ctl.send(Ctl::Attach(connector));
    }

    pub fn detach(&self) {
        let _ = self.ctl.send(Ctl::Detach);
    }

    /// What "Automatic" resolves to right now — the virtual pad matching the physical one
    /// (Swift parity); no pad connected leaves the host's own default.
    ///
    /// **Steam Deck special case:** this is read at session start, *before* attach — but the
    /// Deck's built-in controller is only enumerable with its real 28DE:1205 identity while
    /// the Valve HIDAPI drivers run, and those are enabled on attach only (see
    /// [`set_valve_hidapi`]); with Steam Input on, SDL sees nothing but Steam's virtual
    /// X360 pad anyway. Both cases used to fall through to Xbox 360. On a Deck, a virtual
    /// pad (or no pad at all) means the physical controller behind it IS the built-in one —
    /// resolve to the Steam Deck virtual pad so the paddles/trackpads/gyro have somewhere
    /// to land. A real external controller still wins (it's the one that gets forwarded).
    pub fn auto_pref(&self) -> GamepadPref {
        match self.active() {
            Some(p) if !p.steam_virtual => p.pref,
            _ if is_steam_deck() => GamepadPref::SteamDeck,
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

struct Worker<'a> {
    subsystem: sdl3::GamepadSubsystem,
    /// UI-facing state (the `GamepadService` accessors): pad list, active pad, pin.
    pads_out: &'a Mutex<Vec<PadInfo>>,
    active_out: &'a Mutex<Option<PadInfo>>,
    /// The ONE device held open — the active pad while a session is attached, `None`
    /// otherwise. Opening is what grabs the hardware (SDL's HIDAPI drivers take the
    /// hidraw device away from the system), so idle keeps this empty; see the module doc.
    open: Option<(u32, sdl3::gamepad::Gamepad)>,
    /// Connected pad ids in connection order (metadata only, no device open); the most
    /// recently connected is the auto selection.
    order: Vec<u32>,
    /// Stable key of the user-pinned controller (persisted in Settings) — matched against
    /// connected pads, so it survives restarts and disconnects.
    pinned: Option<String>,
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

impl Worker<'_> {
    fn active_id(&self) -> Option<u32> {
        // The pin matches by stable key (most recently connected wins if two identical pads
        // share one); an unmatched pin falls through to automatic without being cleared.
        if let Some(key) = &self.pinned {
            if let Some(id) = self
                .order
                .iter()
                .rev()
                .copied()
                .find(|&id| self.pad_info(id).is_some_and(|p| &p.key == key))
            {
                return Some(id);
            }
        }
        // Automatic: the most recently connected pad — but never Steam Input's virtual pad
        // while a real controller is present (see `PadInfo::steam_virtual`).
        self.order
            .iter()
            .rev()
            .copied()
            .find(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
            .or_else(|| self.order.last().copied())
    }

    /// Pad metadata from SDL's ID-based getters — deliberately NO device open (see the
    /// module doc; an open would grab the hardware).
    fn pad_info(&self, id: u32) -> Option<PadInfo> {
        if !self.order.contains(&id) {
            return None;
        }
        let jid = sdl3::sys::joystick::SDL_JoystickID(id);
        let mut pref = pref_for_type(self.subsystem.type_for_id(jid));
        let (vid, pid) = (
            self.subsystem.vendor_for_id(jid).unwrap_or(0),
            self.subsystem.product_for_id(jid).unwrap_or(0),
        );
        // There is no SDL gamepad type for the Steam Deck / Steam Controller, so detect Valve by
        // VID/PID (Deck 0x1205, SC wired 0x1102, SC dongle 0x1142) — the host then builds the virtual
        // hid-steam pad with the back grips + dual trackpads and the right glyph identity.
        if vid == 0x28DE && matches!(pid, 0x1205 | 0x1102 | 0x1142) {
            pref = GamepadPref::SteamDeck;
        }
        let name = self
            .subsystem
            .name_for_id(jid)
            .unwrap_or_else(|_| "Controller".into());
        Some(PadInfo {
            key: format!("{vid:04x}:{pid:04x}:{name}"),
            steam_virtual: (vid == 0x28DE && pid == 0x11FF)
                || name.starts_with("Steam Virtual Gamepad"),
            name,
            pref,
        })
    }

    /// Hold exactly the right device: the active pad while a session is attached, nothing
    /// otherwise. The single place that decides to open (= grab) hardware; dropping the
    /// old handle closes it (`SDL_CloseGamepad`) — on a Deck the firmware watchdog then
    /// restores lizard mode.
    fn sync_open(&mut self) {
        let want = if self.attached.is_some() {
            self.active_id()
        } else {
            None
        };
        if self.open.as_ref().map(|(id, _)| *id) == want {
            return;
        }
        self.open = None;
        let Some(id) = want else { return };
        match self.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(id)) {
            Ok(pad) => {
                self.open = Some((id, pad));
                self.set_sensors(true);
            }
            Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
        }
    }

    /// React to anything that may have moved the active-pad selection (hotplug, pin
    /// change): flush held wire state if it did, then re-sync the opened device and the
    /// UI-facing snapshot.
    fn refresh_active(&mut self, before: Option<u32>) {
        if self.active_id() != before {
            self.flush_held();
        }
        self.sync_open();
        self.publish();
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
        if let Some((_, pad)) = self.open.as_mut() {
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
            .open
            .as_ref()
            .filter(|(id, _)| *id == which)
            .map(|(_, p)| p.touchpads_count() >= 2)
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

    /// Publish the pad list, active pad, and pin to the UI-facing mutexes.
    fn publish(&self) {
        let mut list: Vec<PadInfo> = self
            .order
            .iter()
            .filter_map(|&id| self.pad_info(id))
            .collect();
        list.reverse(); // most recent first — the Settings list order
        *self.pads_out.lock().unwrap() = list;
        *self.active_out.lock().unwrap() = self.active_id().and_then(|id| self.pad_info(id));
    }

    /// Apply queued control-plane messages from the UI thread. Returns false when the
    /// app side is gone and the worker should exit.
    fn drain_ctl(&mut self, ctl: &Receiver<Ctl>) -> bool {
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Attach(c)) => {
                    self.attached = Some(c);
                    self.last_axis = [i32::MIN; 6];
                    self.reset_chord(); // every session starts un-latched (Attach doesn't flush)
                                        // The Valve HIDAPI drivers run only in-session (see set_valve_hidapi);
                                        // enabling them re-enumerates a Deck's built-in pad with paddles/
                                        // trackpads/gyro first-class — sync_open follows the churn events.
                    set_valve_hidapi(true);
                    self.sync_open();
                }
                Ok(Ctl::Detach) => {
                    self.flush_held();
                    self.attached = None;
                    self.sync_open(); // closes the held device
                    set_valve_hidapi(false);
                }
                Ok(Ctl::Pin(key)) => {
                    let before = self.active_id();
                    self.pinned = key;
                    self.refresh_active(before);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false, // app gone
            }
        }
    }

    /// Route one SDL event: pad hotplug bookkeeping, and — while a session is attached —
    /// buttons/axes/touchpads/motion of the active pad onto the wire.
    fn handle_event(&mut self, event: sdl3::event::Event) {
        use sdl3::event::Event;
        let active = self.active_id();
        match event {
            Event::ControllerDeviceAdded { which, .. } => {
                if !self.order.contains(&which) {
                    self.order.push(which);
                    if let Some(p) = self.pad_info(which) {
                        tracing::info!(name = p.name, "gamepad attached");
                    }
                    self.refresh_active(active);
                }
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                if self.order.contains(&which) {
                    self.order.retain(|&id| id != which);
                    if self.open.as_ref().map(|(id, _)| *id) == Some(which) {
                        self.open = None; // the device is gone; drop our handle
                    }
                    tracing::info!("gamepad detached");
                    self.refresh_active(active);
                }
            }
            Event::ControllerButtonDown { which, button, .. } if active == Some(which) => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                if let Some(bit) = button_bit(button) {
                    self.held_buttons.push(bit);
                    send(&c, InputKind::GamepadButton, bit, 1);
                    self.maybe_fire_escape();
                }
            }
            Event::ControllerButtonUp { which, button, .. } if active == Some(which) => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                if let Some(bit) = button_bit(button) {
                    self.held_buttons.retain(|&b| b != bit);
                    send(&c, InputKind::GamepadButton, bit, 0);
                    self.rearm_escape();
                }
            }
            Event::ControllerAxisMotion {
                which, axis, value, ..
            } if active == Some(which) => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let (id, v) = axis_value(axis, value);
                if self.last_axis[id as usize] != v {
                    self.last_axis[id as usize] = v;
                    send(&c, InputKind::GamepadAxis, id, v);
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
            } if active == Some(which) && self.attached.is_some() => {
                self.forward_touch(which, touchpad as u32, finger as u8, x, y, true);
            }
            Event::ControllerTouchpadUp {
                which,
                touchpad,
                finger,
                x,
                y,
                ..
            } if active == Some(which) && self.attached.is_some() => {
                self.forward_touch(which, touchpad as u32, finger as u8, x, y, false);
            }
            // Motion: accel events update the cache; each gyro event ships a sample
            // (the DualSense reports both at ~250 Hz). Scale convention shared with
            // the Swift client — sign/scale derived, not yet live-verified.
            Event::ControllerSensorUpdated {
                which,
                sensor,
                data,
                ..
            } if active == Some(which) => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                use sdl3::sensor::SensorType;
                match sensor {
                    SensorType::Accelerometer => {
                        for (i, v) in data.iter().enumerate() {
                            self.last_accel[i] =
                                (v / G * ACCEL_LSB_PER_G).clamp(-32768.0, 32767.0) as i16;
                        }
                    }
                    SensorType::Gyroscope => {
                        let mut gyro = [0i16; 3];
                        for (i, v) in data.iter().enumerate() {
                            gyro[i] = (v * GYRO_LSB_PER_RAD_S).clamp(-32768.0, 32767.0) as i16;
                        }
                        let _ = c.send_rich_input(RichInput::Motion {
                            pad: 0,
                            gyro,
                            accel: self.last_accel,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Drain and render the feedback planes — rumble plus HID output (lightbar /
    /// player LEDs / adaptive triggers) — on the active pad; this thread is their single
    /// consumer. The host re-sends rumble state periodically, so a generous duration with
    /// refresh-on-update is safe — a dropped stop heals within ~500 ms.
    fn render_feedback(&mut self) {
        let Some(connector) = self.attached.clone() else {
            return;
        };
        while let Ok((pad, low, high)) = connector.next_rumble(Duration::ZERO) {
            if pad == 0 {
                if let Some((_, p)) = self.open.as_mut() {
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
            let is_ds = self
                .open
                .as_ref()
                .and_then(|(id, _)| self.pad_info(*id))
                .is_some_and(|p| p.is_dualsense());
            let Some((_, pad)) = self.open.as_mut() else {
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
}

fn run(
    pads_out: &Mutex<Vec<PadInfo>>,
    active_out: &Mutex<Option<PadInfo>>,
    ctl: &Receiver<Ctl>,
    escape_tx: &async_channel::Sender<()>,
    disconnect_tx: &async_channel::Sender<()>,
) -> Result<(), String> {
    // Off-main-thread + no video subsystem: keep SDL away from signals, poll pads on its
    // own thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    // The Valve HIDAPI drivers start DISABLED (SDL defaults the Deck one ON, and its mere
    // enumeration kills the Deck's trackpad-mouse system-wide — see set_valve_hidapi);
    // they are enabled for the duration of an attached session only.
    set_valve_hidapi(false);
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;

    let mut w = Worker {
        subsystem,
        pads_out,
        active_out,
        open: None,
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

    loop {
        // Control plane from the UI thread.
        if !w.drain_ctl(ctl) {
            return Ok(());
        }

        // Block in SDL's own event wait instead of a fixed-interval sleep+poll: input
        // events are handled the moment they arrive (the old 2 ms sleep added up to 2 ms
        // per event), while the timeout bounds the polled work below — ctl messages,
        // rumble/HID feedback, and the escape-chord hold check all run once per wakeup,
        // so their worst case is one timeout (~10 ms attached, imperceptible for
        // haptics; DISCONNECT_HOLD is 1500 ms, so 10 ms hold-check granularity is far
        // inside tolerance). Idle (no session) wakes lazily at 30 ms for hotplug + ctl.
        let timeout = Duration::from_millis(if w.attached.is_some() { 10 } else { 30 });
        if let Some(event) = pump.wait_event_timeout(timeout) {
            w.handle_event(event);
            // Drain whatever else queued while we were waiting or handling.
            while let Some(event) = pump.poll_event() {
                w.handle_event(event);
            }
        }

        // Escalate a held escape chord to a disconnect (polled — the hold completes with no
        // new button events; the chord itself is only detected while a session is attached).
        w.maybe_fire_disconnect();

        w.render_feedback();
    }
}
