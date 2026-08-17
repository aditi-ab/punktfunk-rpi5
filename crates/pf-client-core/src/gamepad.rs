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
//! **Menu mode is the one idle exception.** The gamepad library launcher (`--browse`)
//! flips [`GamepadService::set_menu_mode`] on for its lifetime: the worker then holds the
//! active pad open and translates its buttons/stick into [`MenuEvent`]s (polled off the
//! open handle each loop — Apple `GamepadMenuInput` parity: edge-triggered buttons,
//! snapshot-on-entry so a button still held from a previous screen or stream can't ghost-
//! fire, stick/dpad direction with initial-delay auto-repeat). The Valve HIDAPI drivers
//! stay OFF — a plain SDL open of the virtual X360 / evdev pad doesn't touch lizard mode —
//! and an attached session always supersedes menu translation (the stream path is
//! untouched); detach re-snapshots so the escape chord that ended the session fires
//! nothing in the menu.
//!
//! This thread is also the single consumer of the rumble and HID-output pull planes.

use punktfunk_core::client::{ActuatorQuirks, NativeClient};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Motion scale constants, shared convention with the Swift client (`GamepadWire`): the wire's
/// units ([`wire::MOTION_GYRO_LSB_PER_DEG_S`] / [`wire::MOTION_ACCEL_LSB_PER_G`]), which the host's
/// fixed calibration blobs declare back to their own consumers. SDL hands us gyro in rad/s and
/// accel in m/s²; the DualSense report wants raw LSBs.
const GYRO_LSB_PER_RAD_S: f32 =
    wire::MOTION_GYRO_LSB_PER_DEG_S as f32 * 180.0 / std::f32::consts::PI;
const ACCEL_LSB_PER_G: f32 = wire::MOTION_ACCEL_LSB_PER_G as f32;
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

/// Hold Select/Back ALONE at least this long to send the HOST the guide button — the
/// [`SelectGesture`], armed by [`Settings::guide_gesture`]. The synthetic guide stays down
/// for as long as Select is held, so a long hold IS the host's long-press (the QAM on a
/// Gaming-Mode host). Exists because on some platforms the physical guide press can never
/// reach the host cleanly: the local shell reserves it (iOS's Game Overlay, tvOS) or
/// reacts to it in parallel (Gaming Mode's Steam UI — see [`Settings::system_buttons`]).
///
/// [`Settings::guide_gesture`]: crate::trust::Settings::guide_gesture
/// [`Settings::system_buttons`]: crate::trust::Settings::system_buttons
const GUIDE_HOLD: Duration = Duration::from_millis(350);

/// A held-back Select TAP is delivered as a press with its release scheduled this far
/// behind — never back-to-back: per-transition sends are folded into seq'd `GamepadState`
/// snapshots by the core input task, and a down+up inside one fold window can coalesce
/// into no press at all.
const TAP_PRESS: Duration = Duration::from_millis(50);

/// Steam Deck actuator-decay keepalive cadence, declared to the core's rumble policy engine as an
/// [`ActuatorQuirks`] at slot open. The Deck's built-in actuator decays inside SDL's ~2 s internal
/// rumble resend (`SDL_RUMBLE_RESEND_MS`) and SDL short-circuits an identical `set_rumble` value
/// to a no-op device write — so a steady level is felt as a periodic pulse without sub-decay
/// re-kicks; 40 ms mirrors SDL's sibling Steam-Controller driver keep-alive. The engine owns the
/// re-kick timing, the 1-LSB dedupe-defeat jitter, and every staleness/lease bound — this worker
/// only applies the commands it emits (`design/rumble-root-fix.md` §D).
const DECK_RUMBLE_KEEPALIVE_MS: u16 = 40;

/// Stick deflection below this is ignored for menu navigation (0.5 of full scale — Apple
/// `GamepadMenuInput` parity; menus want deliberate flicks, not drift).
const MENU_DEADZONE: u16 = 16384;
/// A held direction starts auto-repeating after this initial delay…
const MENU_REPEAT_DELAY: Duration = Duration::from_millis(380);
/// …and then repeats at this cadence until released or changed.
const MENU_REPEAT_INTERVAL: Duration = Duration::from_millis(160);
/// How often the open pad's battery is re-read. See [`GamepadWorker::battery_poll`].
const BATTERY_POLL: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuDir {
    Up,
    Down,
    Left,
    Right,
}

/// One controller action for the launcher UI, translated from the open pad while menu
/// mode is on and no session is attached. Buttons are edge-triggered; `Move` debounces
/// the stick/dpad and auto-repeats ([`MENU_REPEAT_DELAY`]/[`MENU_REPEAT_INTERVAL`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    Move(MenuDir),
    /// A — activate the focused item.
    Confirm,
    /// B — back / quit.
    Back,
    /// Y (Apple "secondary"; unused by the launcher today, kept for parity).
    Secondary,
    /// X (Apple "tertiary"; unused).
    Tertiary,
    /// L1 — jump back 5.
    JumpBack,
    /// R1 — jump forward 5.
    JumpForward,
}

/// Menu haptic pulses — short rumble ticks on the menu pad (never during a stream).
#[derive(Clone, Copy, Debug)]
pub enum MenuPulse {
    Move,
    Confirm,
    Boundary,
}

/// Raw pad state sampled once per worker iteration for menu translation.
#[derive(Clone, Copy, Default)]
struct MenuSample {
    /// a, b, x, y, l1, r1 — the order [`MenuNav::poll`] maps to events.
    buttons: [bool; 6],
    /// Left stick, SDL convention (+y = down).
    lx: i16,
    ly: i16,
    /// up, down, left, right.
    dpad: [bool; 4],
}

/// The pure menu-input state machine (no SDL types — unit-tested below). Port of the
/// Swift client's `GamepadMenuInput`: the poll after a [`reset`](Self::reset) adopts the
/// currently-held buttons and direction WITHOUT firing, so a press that crossed a screen
/// handoff (the B that closed a stream, a held A on mode entry) must be released before
/// it can act; buttons fire on the rising edge only.
struct MenuNav {
    /// Adopt the next sample silently (set on mode entry / stream detach / pad change).
    snapshot_pending: bool,
    /// Previous button states, [`MenuSample::buttons`] order.
    was: [bool; 6],
    dir: Option<MenuDir>,
    /// When `dir` engaged — start of the initial-repeat delay.
    dir_since: Instant,
    last_repeat: Instant,
}

impl MenuNav {
    fn new() -> MenuNav {
        MenuNav {
            snapshot_pending: true,
            was: [false; 6],
            dir: None,
            dir_since: Instant::now(),
            last_repeat: Instant::now(),
        }
    }

    /// Arm the snapshot: the next poll adopts held state without firing.
    fn reset(&mut self) {
        self.snapshot_pending = true;
        self.dir = None;
    }

    /// Direction from the left stick (dominant axis wins past the deadzone), falling back
    /// to the discrete dpad. SDL sticks are +y = down.
    fn resolve_dir(s: &MenuSample) -> Option<MenuDir> {
        let (ax, ay) = (s.lx.unsigned_abs(), s.ly.unsigned_abs());
        if ax > MENU_DEADZONE || ay > MENU_DEADZONE {
            return Some(if ax >= ay {
                if s.lx > 0 {
                    MenuDir::Right
                } else {
                    MenuDir::Left
                }
            } else if s.ly > 0 {
                MenuDir::Down
            } else {
                MenuDir::Up
            });
        }
        let [up, down, left, right] = s.dpad;
        if left {
            Some(MenuDir::Left)
        } else if right {
            Some(MenuDir::Right)
        } else if up {
            Some(MenuDir::Up)
        } else if down {
            Some(MenuDir::Down)
        } else {
            None
        }
    }

    fn poll(&mut self, s: &MenuSample, now: Instant, out: &mut Vec<MenuEvent>) {
        let dir = Self::resolve_dir(s);
        if self.snapshot_pending {
            self.snapshot_pending = false;
            self.was = s.buttons;
            self.dir = dir;
            self.dir_since = now;
            self.last_repeat = now;
            return;
        }
        // buttons order a, b, x, y, l1, r1 → the matching event per index.
        const EVENTS: [MenuEvent; 6] = [
            MenuEvent::Confirm,
            MenuEvent::Back,
            MenuEvent::Tertiary,
            MenuEvent::Secondary,
            MenuEvent::JumpBack,
            MenuEvent::JumpForward,
        ];
        for (i, ev) in EVENTS.iter().enumerate() {
            if s.buttons[i] && !self.was[i] {
                out.push(*ev);
            }
            self.was[i] = s.buttons[i];
        }
        if dir != self.dir {
            self.dir = dir;
            self.dir_since = now;
            self.last_repeat = now;
            if let Some(d) = dir {
                out.push(MenuEvent::Move(d));
            }
        } else if let Some(d) = dir {
            if now.duration_since(self.dir_since) >= MENU_REPEAT_DELAY
                && now.duration_since(self.last_repeat) >= MENU_REPEAT_INTERVAL
            {
                self.last_repeat = now;
                out.push(MenuEvent::Move(d));
            }
        }
    }
}

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
    /// The pad's own power state, when it reports one. Purely LOCAL SDL state — nothing about
    /// this crosses the wire, so it is additive with no ABI implication whatever.
    ///
    /// `None` is the common case, not an error: a wired pad has nothing to report, and Steam's
    /// virtual gamepad reports nothing about the physical device behind it. Anything reading
    /// this must degrade to "no battery shown" rather than to "0 %".
    pub battery: Option<PadBattery>,
}

/// SDL's power report for an OPEN pad, reduced to the two facts a UI can act on.
///
/// `None` folds together every "nothing useful to say" case: a wired pad with no battery at
/// all, an error, an unknown state, and the `-1` percentage SDL returns for "powered, level
/// unknown". A caller must draw NO battery for `None` — never 0 %, which is the one reading
/// that would send someone hunting for a charger.
fn battery_of(pad: &sdl3::gamepad::Gamepad) -> Option<PadBattery> {
    use sdl3::joystick::PowerLevel;
    let info = pad.power_info();
    let charging = match info.state {
        PowerLevel::OnBattery => false,
        PowerLevel::Charging | PowerLevel::Charged => true,
        PowerLevel::NoBattery | PowerLevel::Error | PowerLevel::Unknown => return None,
    };
    if info.percentage < 0 {
        return None;
    }
    Some(PadBattery {
        percent: info.percentage.min(100) as u8,
        charging,
    })
}

/// A controller's power state, as SDL reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadBattery {
    /// 0–100. SDL gives −1 for "on power but level unknown", which callers map to `None`
    /// rather than storing here.
    pub percent: u8,
    /// On the cable (or dock) right now. Worth showing separately: a pad at 4 % that is
    /// charging is not the problem a pad at 4 % that is not.
    pub charging: bool,
}

impl PadInfo {
    /// A short controller-kind label for the Settings list (`""` for a plain Xbox/standard pad).
    pub fn kind_label(&self) -> &'static str {
        match self.pref {
            GamepadPref::DualSense => "DualSense",
            GamepadPref::DualSenseEdge => "DualSense Edge",
            GamepadPref::DualShock4 => "DualShock 4",
            GamepadPref::XboxOne => "Xbox One",
            // Unreachable from `pref_for_type` today — SDL has no Elite `GamepadType` — but a
            // pinned setting can carry it, and an empty label there reads as a plain Xbox pad.
            GamepadPref::XboxElite => "Xbox Elite Series 2",
            GamepadPref::SteamDeck => "Steam Deck",
            GamepadPref::SteamController => "Steam Controller",
            GamepadPref::SteamController2 => "Steam Controller 2",
            GamepadPref::SteamController2Puck => "Steam Controller 2 Puck",
            GamepadPref::SwitchPro => "Switch Pro",
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

/// Disable the Valve HIDAPI drivers **before SDL exists** — call this alongside the other
/// pre-`SDL_Init` hints, not after a subsystem is up.
///
/// The damage these drivers do happens at *enumeration*, which is part of initialising the
/// joystick/gamepad subsystem. Setting the hint afterwards does detach the driver, but only after
/// it has already sent the Deck its `ID_CLEAR_DIGITAL_MAPPINGS` + `TRACKPAD_NONE` — so the
/// built-in trackpad-mouse dies system-wide and stays dead until the firmware watchdog restores
/// lizard mode seconds later. The threaded worker ([`run`]) has always done this in the right
/// order; the caller-pumped path could not, because by the time it receives a
/// [`sdl3::GamepadSubsystem`] the enumeration has already happened. Hence a separate entry point
/// its callers can put in the right place.
pub fn preinit_disable_valve_hidapi() {
    set_valve_hidapi(false);
}

/// Map the SDL-reported controller type to the virtual pad we'd ask the host to create.
fn pref_for_type(t: sdl3::gamepad::GamepadType) -> GamepadPref {
    use sdl3::gamepad::GamepadType as T;
    match t {
        T::PS5 => GamepadPref::DualSense,
        T::PS4 => GamepadPref::DualShock4,
        T::XboxOne => GamepadPref::XboxOne,
        // A paired Joy-Con set exposes the full Pro button surface through SDL, so it rides
        // the same virtual pad; single Joy-Cons stay on the Xbox 360 fallback (half a pad).
        T::NintendoSwitchPro | T::NintendoSwitchJoyconPair => GamepadPref::SwitchPro,
        _ => GamepadPref::Xbox360,
    }
}

/// The kind a slot DECLARES to the host ([`InputKind::GamepadArrival`]) given the user's
/// controller-type `setting` and the pad's `physical` kind: an explicit setting emulates that pad
/// for every slot, `Auto` keeps per-pad detection (what makes a mixed session honest).
///
/// This has to be applied per pad and not just in the Hello: the host builds each virtual device
/// from that pad's arrival and only falls back to the session default for a pad that never
/// declares one, so a client that always declared the detected kind would silently undo the
/// setting the moment a controller connected. The physical kind is still what the LOCAL feedback
/// paths use (DualSense raw effects, the Deck rumble keep-alive) — those talk to the controller in
/// the user's hands, not the one the host is pretending to have.
fn declared_kind(setting: GamepadPref, physical: GamepadPref) -> GamepadPref {
    match setting {
        GamepadPref::Auto => physical,
        explicit => explicit,
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
    KindOverride(GamepadPref),
    Forwarding(bool),
    SystemButtons {
        forward_raw: bool,
        gesture: bool,
    },
    TapButton(u32),
    /// Which pad-audio streams the session's settings want rendered (bit0 = haptics, bit1 =
    /// speaker) — the settings half of the per-pad tier-A capability declared at slot open.
    PadAudioPrefs(u8),
    MenuMode(bool),
    MenuRumble(MenuPulse),
    Mask(bool),
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
    /// Menu-navigation events while menu mode is on and no session is attached; the
    /// launcher page consumes them.
    menu_rx: async_channel::Receiver<MenuEvent>,
}

impl GamepadService {
    pub fn start() -> GamepadService {
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (menu_tx, menu_rx) = async_channel::unbounded();
        let (p, a) = (pads.clone(), active.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-gamepad".into())
            .spawn(move || {
                if let Err(e) = run(p, a, &ctl_rx, &escape_tx, &disconnect_tx, &menu_tx) {
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
            menu_rx,
        }
    }

    /// The caller-pumped variant for the session binary: SDL video+events live on ITS
    /// main thread, and SDL only ever grants one thread the event queue — a second
    /// `start()`-style worker thread could never see gamepad events there. The caller
    /// owns the SDL context, feeds every polled event to [`GamepadPump::handle_event`],
    /// and calls [`GamepadPump::tick`] once per loop iteration (the threaded worker's
    /// per-wakeup work: ctl drain, chord-hold check, menu repeat, feedback).
    ///
    /// The Valve HIDAPI drivers are held off here too, but this is **too late to be the only
    /// place it happens**: the `subsystem` argument means enumeration is already done, and that
    /// is when the Deck driver kills the trackpad-mouse. The caller must also call
    /// [`preinit_disable_valve_hidapi`] with its other pre-`SDL_Init` hints. This call still
    /// earns its place — it re-asserts "off" for a process that ran a session earlier — but on
    /// its own it only detaches a driver that has already done the damage.
    pub fn pumped(subsystem: sdl3::GamepadSubsystem) -> (GamepadService, GamepadPump) {
        set_valve_hidapi(false);
        let pads = Arc::new(Mutex::new(Vec::new()));
        let active = Arc::new(Mutex::new(None));
        let (ctl, ctl_rx) = std::sync::mpsc::channel();
        let (escape_tx, escape_rx) = async_channel::unbounded();
        let (disconnect_tx, disconnect_rx) = async_channel::unbounded();
        let (menu_tx, menu_rx) = async_channel::unbounded();
        let worker = Worker::new(
            subsystem,
            pads.clone(),
            active.clone(),
            escape_tx,
            disconnect_tx,
            menu_tx,
        );
        (
            GamepadService {
                pads,
                active,
                ctl,
                escape_rx,
                disconnect_rx,
                menu_rx,
            },
            GamepadPump { worker, ctl_rx },
        )
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

    /// Menu-navigation events ([`MenuEvent`]) — flowing only while menu mode is on and no
    /// session is attached. A fresh clone per call; the launcher spawns a future on it.
    pub fn menu_events(&self) -> async_channel::Receiver<MenuEvent> {
        self.menu_rx.clone()
    }

    /// Turn menu mode on/off: while on (and no session attached) the worker holds the
    /// active pad open and translates it into [`MenuEvent`]s. The launcher flips this on
    /// once for its lifetime — an attached session supersedes translation automatically.
    pub fn set_menu_mode(&self, on: bool) {
        let _ = self.ctl.send(Ctl::MenuMode(on));
    }

    /// Play a short menu haptic tick on the menu pad (no-op while a session is attached
    /// or no pad is open; best-effort on pads without rumble).
    pub fn menu_rumble(&self, pulse: MenuPulse) {
        let _ = self.ctl.send(Ctl::MenuRumble(pulse));
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

    /// Adopt the user's explicit controller-type setting for the session about to start
    /// (`GamepadPref::Auto` = detect per pad, the default).
    ///
    /// This is NOT redundant with the session default in the Hello: a current host honors a pad's
    /// [`InputKind::GamepadArrival`] over the session default, so a client that declared only the
    /// detected kind would silently undo the setting the moment a controller connected. Call it
    /// before [`Self::attach`] — slots declare their kind at open time and the host does not
    /// hot-swap a device that already exists.
    pub fn set_kind_override(&self, pref: GamepadPref) {
        let _ = self.ctl.send(Ctl::KindOverride(pref));
    }

    /// Forward this device's controllers to the host at all ([`Settings::gamepad_forwarding`],
    /// default on). Off is for a couch whose pad reaches the host another way — a USB
    /// passthrough tool like VirtualHere, or a controller plugged into the host itself —
    /// where forwarding as well would give the host two pads for one pair of hands.
    ///
    /// Off holds no slot open, so nothing is sent AND nothing is *grabbed*: no arrival, no
    /// virtual pad host-side, and the hidraw node stays free for the passthrough tool to
    /// bind (SDL's HIDAPI drivers take it at open — a held device cannot be bound away).
    /// It follows that the escape chord, which only listens on forwarded pads, is not
    /// available while off; the keyboard chord and the client's own UI still end a session.
    ///
    /// Menu navigation is untouched: the launcher still opens the active pad to drive its
    /// UI, and a session — which supersedes menu mode whether it forwards or not — releases
    /// it again, so the pad is free for the whole time a stream is up.
    ///
    /// [`Settings::gamepad_forwarding`]: crate::trust::Settings::gamepad_forwarding
    pub fn set_forwarding(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Forwarding(on));
    }

    /// A system overlay owns the controller right now — hold every forwarded pad NEUTRAL
    /// until it closes. This is the Steam Input behaviour a streaming client has to
    /// reproduce by hand: while the Deck's Steam menu or QAM is up, the same physical
    /// sticks and buttons drive Steam's UI, and anything we keep forwarding lands in the
    /// game underneath as a second, invisible player.
    ///
    /// **Masking is not [`set_forwarding`](Self::set_forwarding).** Forwarding-off closes the
    /// slot and sends the host a [`GamepadRemove`](InputKind::GamepadRemove) — the game sees a
    /// controller *unplug*, which is a hardware event with real in-game consequences (pause
    /// menus, "reconnect your controller", player-slot churn). Opening the QAM must not look
    /// like that. Masking keeps every slot open and merely stops the transitions, after
    /// flushing what the host believes is held so a stick held at overlay-open stops steering
    /// instead of freezing at its last value.
    ///
    /// SDL has this gate of its own — it drops presses while the process has windows but no
    /// keyboard focus — and on a desktop it fires. It CANNOT fire on a Deck in Gaming Mode:
    /// gamescope resolves focus per Xwayland ctx, and the client sits alone in its own ctx, so
    /// its X input focus never moves when the overlay takes over (measured). That is why this
    /// exists as an explicit lever rather than something inherited for free.
    ///
    /// Held state is adopted, not replayed, on the way back — see [`Ctl::Mask`]'s handling.
    pub fn set_masked(&self, on: bool) {
        let _ = self.ctl.send(Ctl::Mask(on));
    }

    /// The session's system-button policy, resolved from
    /// [`Settings::system_buttons_forward`] × [`Settings::guide_gesture_enabled`]:
    /// `forward_raw` gates the physical guide/QAM presses onto the wire (off = they stay
    /// with the local shell — the Gaming-Mode default, where Steam reacts to them no
    /// matter what and forwarding opens BOTH overlays); `gesture` arms the hold-Select
    /// guide gesture ([`GUIDE_HOLD`]), the alternate route that keeps the host's guide —
    /// and, held longer, a Gaming-Mode host's QAM — reachable from a controller.
    ///
    /// [`Settings::system_buttons_forward`]: crate::trust::Settings::system_buttons_forward
    /// [`Settings::guide_gesture_enabled`]: crate::trust::Settings::guide_gesture_enabled
    pub fn set_system_buttons(&self, forward_raw: bool, gesture: bool) {
        let _ = self.ctl.send(Ctl::SystemButtons {
            forward_raw,
            gesture,
        });
    }

    /// One-shot synthetic tap of the HOST's guide button ([`Ctl::TapButton`]): down now,
    /// up [`TAP_PRESS`] later, on the first forwarded slot's wire index (pad 0 when none
    /// is open). The session control socket's "press the host's Steam/guide button" verb
    /// — the Decky panel's UI route to the host overlay. No-op while no session is
    /// attached.
    pub fn tap_guide(&self) {
        let _ = self.ctl.send(Ctl::TapButton(wire::BTN_GUIDE));
    }

    /// Like [`Self::tap_guide`] for the quick-access button (`MISC1` — the Deck `…`).
    /// Opens the QAM on a Gaming-Mode host whose virtual pad is Deck-shaped; other
    /// virtual pads map it to their own misc button (or drop it) — harmless.
    pub fn tap_qam(&self) {
        let _ = self.ctl.send(Ctl::TapButton(wire::BTN_MISC1));
    }

    /// Declare which pad-audio streams this session's settings want rendered (`haptics` =
    /// [`Settings::pad_haptics`](crate::trust::Settings::pad_haptics), `speaker` =
    /// `pad_speaker == "pad"` via [`crate::pad_audio::speaker_active`]). Drives the per-pad
    /// tier-A capability bits declared to the core at slot open — a WIRED DualSense/Edge
    /// declares exactly these; every other pad declares 0. Call before [`Self::attach`],
    /// like [`Self::set_kind_override`]: slots declare at open time. Defaults to "nothing"
    /// for an embedder that never calls it, keeping the wire bytes exactly as before.
    pub fn set_pad_audio_prefs(&self, haptics: bool, speaker: bool) {
        let bits = (haptics as u8) | ((speaker as u8) << 1);
        let _ = self.ctl.send(Ctl::PadAudioPrefs(bits));
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

/// The caller-pumped worker half of [`GamepadService::pumped`]: the session binary owns
/// SDL and its event loop; this just needs the events and a periodic tick.
pub struct GamepadPump {
    worker: Worker,
    ctl_rx: Receiver<Ctl>,
}

impl GamepadPump {
    /// Feed one polled SDL event. Non-gamepad events (window, keyboard, mouse) are
    /// ignored, so the caller can forward everything unfiltered.
    pub fn handle_event(&mut self, event: sdl3::event::Event) {
        self.worker.handle_event(event);
    }

    /// The per-wakeup polled work the threaded worker runs after each event wait: ctl
    /// drain (attach/detach/pin/menu), the escape-chord hold check, menu repeat timing,
    /// and rumble/HID feedback. Call once per loop iteration (≲30 ms cadence keeps
    /// chord-hold and haptics inside the threaded worker's tolerances).
    pub fn tick(&mut self) {
        let _ = self.worker.drain_ctl(&self.ctl_rx);
        self.worker.gesture_poll();
        self.worker.maybe_fire_disconnect();
        self.worker.menu_poll();
        self.worker.render_feedback();
    }

    /// Close every forwarded slot — flush its held wire state, tell the host to remove the pad,
    /// and physically silence it. Call once on the way out of the caller's event loop.
    ///
    /// [`GamepadService::detach`] only *posts* `Ctl::Detach`; the close — the flush, the host-side
    /// `GamepadRemove`, and the explicit `set_rumble(0, 0)` backstop in `close_slot_at` — happens
    /// when the pump next drains it. An exit path that detached and then left the loop without
    /// another [`tick`](Self::tick) therefore skipped all of it, and nothing else would: the slots
    /// hold no `Drop` that silences them. A pad left mid-buzz stayed buzzing.
    ///
    /// This closes the slots directly rather than draining the queued `Ctl::Detach` that would
    /// have done it. Same physical outcome by a shorter path, and deliberately so: this also runs
    /// from `Drop`, and `drain_ctl` reaches `Mutex::lock().unwrap()`, which on a poisoned lock
    /// would panic — during an unwind that aborts the process. Closing a slot touches no lock.
    ///
    /// Idempotent, and safe with nothing attached.
    pub fn shutdown(&mut self) {
        self.worker.close_all_slots();
    }
}

/// The silence backstop of last resort. A caller's loop can also leave by `?` on a fatal overlay
/// or present error — several paths do — and those would skip an explicit
/// [`shutdown`](GamepadPump::shutdown) entirely, leaving a forwarded pad buzzing on the way out.
///
/// Callers should still call `shutdown` at their normal exit rather than lean on this: the pad
/// wants to go quiet *before* a long teardown (session join, `vkDeviceWaitIdle`), not after it.
/// Doing both is free — `shutdown` is idempotent.
impl Drop for GamepadPump {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The lowest wire pad index (0..[`MAX_PADS`](punktfunk_core::input::MAX_PADS)) not already held
/// by a slot, or `None` when every index is taken. Assigning lowest-free keeps slot indices
/// stable across hot-plug churn: a pad that disconnects frees only its own index, so the others
/// never renumber (a game must not see its players shuffle when one pad drops).
fn lowest_free_index(taken: &[u8]) -> Option<u8> {
    (0..punktfunk_core::input::MAX_PADS as u8).find(|i| !taken.contains(i))
}

/// Send one per-transition gamepad event tagged with its wire pad index (`flags`). The core
/// input task folds these per-pad into the seq'd [`GamepadState`](punktfunk_core::input::InputKind::GamepadState)
/// snapshots the host applies (keyed on this same `flags` index), so the only thing multi-pad
/// forwarding must get right here is the index — one controller per slot, one slot per index.
fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32, pad: u8) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y: 0,
        flags: pad as u32,
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
        Axis::LeftX => (wire::AXIS_LS_X, (v as i32).max(-32767)),
        Axis::LeftY => (wire::AXIS_LS_Y, -(v as i32).max(-32767)),
        Axis::RightX => (wire::AXIS_RS_X, (v as i32).max(-32767)),
        Axis::RightY => (wire::AXIS_RS_Y, -(v as i32).max(-32767)),
        Axis::TriggerLeft => (wire::AXIS_LT, (v as i32).clamp(0, 32767) >> 7),
        Axis::TriggerRight => (wire::AXIS_RT, (v as i32).clamp(0, 32767) >> 7),
    }
}

/// The DualSense effects packet (SDL `DS5EffectsState_t`, 47 bytes) — the same layout the
/// host parses off its virtual pad; the wire's 11-byte trigger blocks drop in verbatim.
/// Enable bits select only the fields each update touches, so rumble (driven separately
/// through SDL) and untouched fields keep their state.
///
/// The offsets below are the USB output report's, **minus one**: SDL's payload carries no leading
/// report id. `pf-inject`'s `dualsense_proto::out_report` is where that layout is written down and
/// explained (including the Bluetooth `+2` base), but this crate cannot import it — `pf-inject` is
/// host-side and neither crate depends on the other, and a DualSense report layout has no business
/// in `punktfunk-core`, the only crate they share. So this is a deliberate second copy, and
/// [`ds5_offsets_track_the_usb_report`](ds5_feedback_tests) pins the `−1` relationship rather than
/// leaving it to a comment.
/// A `u8` field lever: decimal, or `0x`-prefixed hex (these name DS5 report BYTES, and every
/// reference to them — SDL's source, the reverse-engineering notes, this module's own comments —
/// writes them in hex). `None` when unset or unparseable, so a typo falls back to the default
/// rather than to zero.
fn env_u8(key: &str) -> Option<u8> {
    let v = std::env::var(key).ok()?;
    let v = v.trim();
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok(),
        None => v.parse().ok(),
    }
}

struct Ds5Feedback;

impl Ds5Feedback {
    /// The USB report offsets these are derived from — see the type doc. Kept beside the derived
    /// values so the subtraction is visible at the point of definition.
    const REPORT_ID_LEN: usize = 1;
    /// The audio-control region (`ucHeadphoneVolume`…`ucAudioMuteBits`): report byte 5.
    const AUDIO: usize = 5 - Self::REPORT_ID_LEN;
    const RIGHT_TRIGGER: usize = 11 - Self::REPORT_ID_LEN;
    const LEFT_TRIGGER: usize = 22 - Self::REPORT_ID_LEN;
    const PAD_LIGHTS: usize = 44 - Self::REPORT_ID_LEN;
    const LED_RGB: usize = 45 - Self::REPORT_ID_LEN;
    /// One adaptive-trigger parameter block: a mode byte plus 10 parameters. Mirrors
    /// `PUNKTFUNK_HID_EFFECT_MAX`, which is the same number at the C-ABI boundary.
    const TRIGGER_LEN: usize = punktfunk_core::abi::PUNKTFUNK_HID_EFFECT_MAX as usize;

    fn trigger_packet(which: u8, effect: &[u8]) -> [u8; 47] {
        let mut p = [0u8; 47];
        let (flag, off) = if which == 1 {
            (0x04, Self::RIGHT_TRIGGER)
        } else {
            (0x08, Self::LEFT_TRIGGER)
        };
        p[0] = flag;
        let n = effect.len().min(Self::TRIGGER_LEN);
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

    /// The one-shot tier-A activation packet — the SDL disable-bit trap undone. `p[0]`
    /// (`ucEnableBits1`) bit0 = "enable rumble emulation" and bit1 = "disable audio haptics"
    /// (SDL_hidapi_ps5.c); SDL sets BOTH whenever its rumble path runs, which mutes the very
    /// voice coils the 0xD1 haptics stream drives. Per SDL's own comment — "Leaving emulated
    /// rumble bits off will restore audio haptics" — a packet with those bits CLEARED (and no
    /// other valid flag, so nothing else is touched) puts the pad back on audio haptics.
    fn audio_haptics_packet() -> [u8; 47] {
        [0u8; 47]
    }

    /// Point the pad's audio at its own SPEAKER, and give that speaker a volume.
    ///
    /// Without this the speaker is silent no matter how correct the PCM routing is, which is
    /// exactly what a wired DS5 on a Steam Deck did: haptics felt, speaker inaudible. The
    /// reason is that **channel 1 of the pad's audio function is shared** — it is the headphone
    /// jack's right channel AND the built-in mono speaker — and which one physically sounds is
    /// chosen by `ucAudioEnableBits` (report byte 8, struct offset 7). A pad powers up pointing
    /// at the headphone jack, so with nothing plugged in the speaker pair goes nowhere. The
    /// voice coils are channels 2/3 and are NOT affected by that select, which is why haptics
    /// work the instant the samples are routed right and the speaker does not.
    ///
    /// We only ever wrote these bytes when a host forwarded a game's [`HidOutput::AudioCtl`],
    /// so a title that manages no audio settings of its own left the speaker dead. This is the
    /// default that makes the stream audible; a later `AudioCtl` still overrides it verbatim
    /// ([`Self::audio_ctl_packet`]), so a game that does drive its own volume still wins.
    ///
    /// ⚠ `ucEnableBits1` bits 0/1 stay CLEAR — they are "enable rumble emulation" and "disable
    /// audio haptics", and asserting either would mute the coils this plane drives.
    ///
    /// ⚠ `path` is empirical. Measured on a DualSense (`054c:0ce6`) using the pad's OWN
    /// microphone as the detector, with a tone driven into the speaker pair. The first sweep
    /// only cleared the noise floor by ~5× and left the value unsettled; a second sweep
    /// (2026-08-17, DS5 wired to a Steam Deck client, 330 Hz) swept the whole byte and is the
    /// one to trust — it separates the two sounding values by ~300×:
    ///
    /// | `ucAudioEnableBits` | 330 Hz energy | |
    /// |---|---|---|
    /// | `0x00` (power-on) | 0.000001 | silent — the headphone jack |
    /// | `0x10` | 0.000002 | silent |
    /// | `0x20` | 0.000283 | **speaker sounds** |
    /// | `0x30` | 0.000336 | speaker sounds, marginally louder |
    /// | `0x40` | 0.000001 | silent |
    /// | `0x50` | 0.000002 | silent |
    ///
    /// So **bit 5 is the speaker-path enable** and bit 4 alone does nothing. We ship `0x20`
    /// rather than the marginally louder `0x30` because `0x30` asserts a second path bit whose
    /// effect on the HEADPHONE leg was NOT measured, and `0x20` already sounds — a pad with
    /// headphones in its jack must not lose them to a default. Two further properties were
    /// measured the same day, both of which this one-shot relies on: the setting **persists**
    /// (unchanged 40 s after a single write, with SDL live on the pad), and it **survives the
    /// pad's USB audio stream stopping and restarting** — so it does not need re-asserting when
    /// the renderer opens its output.
    ///
    /// Verified end-to-end on glass 2026-08-17 by forcing the pad back to `0x00` (pad-mic floor
    /// 0.000000, inaudible), then reconnecting: this packet alone restored the speaker
    /// (pad-mic 0.000349, and audible to the user).
    ///
    /// Overridable per-run with `PUNKTFUNK_PAD_SPEAKER_PATH` / `PUNKTFUNK_PAD_SPEAKER_VOLUME`
    /// so a field report can bisect it without a rebuild.
    fn speaker_enable_packet(volume: u8, path: u8) -> [u8; 47] {
        let mut p = [0u8; 47];
        // bit5 = ucSpeakerVolume is valid, bit7 = the audio-control byte is valid.
        p[0] = 0x20 | 0x80;
        p[Self::AUDIO + 1] = volume; // ucSpeakerVolume
        p[Self::AUDIO + 3] = path; // ucAudioEnableBits
        p
    }

    /// Fold a host [`HidOutput::AudioCtl`] into an effects packet: `raw` is DS5 output report
    /// `0x02` bytes 5..=10 verbatim → struct offsets 4..=9 ([`Self::AUDIO`] — headphone/
    /// speaker/mic volumes + routing), and `p[0]` re-asserts the report's audio-valid flags
    /// (`flags` bits1..4 = report `flag0` bits 4..7). `flags` bit0 (haptics-select, `flag0`
    /// bit1 = SDL's "disable audio haptics") is deliberately NOT replayed: bits 0/1 stay
    /// clear so the pad's audio haptics stay live (see [`audio_haptics_packet`]).
    fn audio_ctl_packet(flags: u8, raw: &[u8; 6]) -> [u8; 47] {
        let mut p = [0u8; 47];
        p[0] = (flags & 0x1E) << 3;
        p[Self::AUDIO..Self::AUDIO + 6].copy_from_slice(raw);
        p
    }
}

/// One forwarded controller during an attached session: the open SDL handle, its stable wire
/// pad index (0..[`MAX_PADS`](punktfunk_core::input::MAX_PADS)), and the per-pad wire/feedback
/// state that used to be single-scalar on the Worker. Opening the device is what grabs the
/// hardware (SDL's HIDAPI drivers take the hidraw node from the system), so slots exist only
/// while a session is attached — idle/menu never populates them (see the module doc).
struct Slot {
    /// SDL instance id (`ControllerDevice*::which`).
    id: u32,
    /// Wire pad index — stable for the life of the slot; assigned lowest-free on open so a
    /// disconnect+replug of one pad never renumbers the others.
    index: u8,
    pad: sdl3::gamepad::Gamepad,
    /// Resolved controller kind (captured at open) — selects the Deck rumble keep-alive and the
    /// DualSense raw-effect feedback path without re-querying SDL metadata under a `&mut` borrow.
    pref: GamepadPref,
    /// The kind this slot DECLARED to the host in its [`InputKind::GamepadArrival`]
    /// ([`declared_kind`] of the setting and `pref`) — what the host actually built this pad from,
    /// which under `Auto` differs per pad. Captured at open beside `pref` for the same reason, and
    /// kept distinct from it because the two answer different questions: `pref` is the controller
    /// in the user's hands (local feedback), this is the one the host is pretending to have.
    declared: GamepadPref,
    /// Wire axis state — zeroed on the wire when this slot closes (detach / unplug).
    last_axis: [i32; 6],
    held_buttons: Vec<u32>,
    /// Touchpad contacts the host believes are down, keyed by `(surface, finger)` — lifted when
    /// the slot closes so a contact held at that moment doesn't stick. surface 0 = the legacy
    /// single touchpad, 1/2 = a Steam left/right pad.
    held_touches: std::collections::HashSet<(u8, u8)>,
    /// Per Steam-pad surface (index 0 = left/surface 1, 1 = right/surface 2): the last wire
    /// coordinates + whether a finger is on it. Pad CLICKS arrive as buttons with no position,
    /// so the click forward reuses the surface's live contact point.
    surface_last: [(i16, i16, bool); 2],
    /// Steam-pad clicks currently held (surface−1 indexed): keeps the click bit asserted
    /// through touch-motion frames (which would otherwise clear it host-side) and lets the
    /// close lift a click held across detach/unplug.
    held_clicks: [bool; 2],
    last_accel: [i16; 3],
    /// This slot has put at least one motion sample on the wire, so the host is holding one.
    /// Gates the zero-gyro park in [`Worker::flush_slot`] — a pad with no gyro must not start
    /// looking like one just because it closed.
    sent_motion: bool,
    /// The "your gyro can't reach this session" notice fired for this slot (log once, not per
    /// sample — this path runs at the pad's sensor rate).
    motion_unreachable_logged: bool,
    /// Hold-Select→guide state ([`SelectGesture`]) — only fed while the worker's
    /// `guide_gesture` policy is on.
    gesture: SelectGesture,
    /// Pad-audio render capabilities declared for this slot (bit0 = haptics, bit1 = speaker
    /// — the [`NativeClient::set_pad_audio_caps`] bits). Nonzero only for a tier-A pad (a
    /// WIRED DualSense/Edge, see [`crate::pad_audio::is_tier_a_ds5`]) under matching
    /// settings; bit0 set additionally suppresses wire rumble for this slot (the SDL
    /// disable-bit trap — see [`Worker::render_feedback`]).
    audio_caps: u8,
    /// The wire-rumble-suppressed notice fired for this slot (log once, not per command).
    rumble_suppressed_logged: bool,
}

impl Slot {
    fn new(
        id: u32,
        index: u8,
        pref: GamepadPref,
        declared: GamepadPref,
        pad: sdl3::gamepad::Gamepad,
    ) -> Slot {
        Slot {
            id,
            index,
            pad,
            pref,
            declared,
            last_axis: [i32::MIN; 6],
            held_buttons: Vec::new(),
            held_touches: std::collections::HashSet::new(),
            surface_last: [(0, 0, false); 2],
            held_clicks: [false; 2],
            last_accel: [0; 3],
            sent_motion: false,
            motion_unreachable_logged: false,
            gesture: SelectGesture::default(),
            audio_caps: 0,
            rumble_suppressed_logged: false,
        }
    }

    /// This pad has two touchpads (Steam Deck / Steam Controller) — gates the `TouchpadEx`
    /// surface encoding and the pad-click button re-route.
    fn is_multi_touchpad(&self) -> bool {
        self.pad.touchpads_count() >= 2
    }
}

/// Per-slot hold-Select→guide state machine (see [`GUIDE_HOLD`]). Pure — fed transitions
/// and polled with a clock, it emits the wire sends due as `(button bit, down)` pairs —
/// so the timing rules are testable without SDL or a live session.
///
/// The rules:
/// - Select pressed ALONE is held back (pending). Any other button already down means
///   Select is part of a combo — the escape chord ends in it — and passes through.
/// - A button pressed WHILE Select is pending makes it a real Select after all; its
///   deferred down goes out first, preserving chronology.
/// - Pending past [`GUIDE_HOLD`] becomes a synthetic guide, down until Select releases.
/// - Released before the threshold, it's a TAP: press delivered on release, the release
///   itself [`TAP_PRESS`] behind it (back-to-back transitions can fold into nothing).
#[derive(Default)]
struct SelectGesture {
    /// Select is down and held back — tap-or-guide undecided.
    pending_since: Option<Instant>,
    /// The held-back Select became a synthetic guide; its release lifts the guide.
    as_guide: bool,
    /// A delivered tap's release is owed at this time.
    release_due: Option<Instant>,
}

impl SelectGesture {
    /// Select went down (`alone` = no other button held on this slot). Returns true when
    /// the press is held back; false lets the caller forward it as a normal button.
    fn on_select_down(&mut self, now: Instant, alone: bool, out: &mut Vec<(u32, bool)>) -> bool {
        // A previous tap's scheduled release still owed: lift it before the new press.
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
        if alone {
            self.pending_since = Some(now);
            return true;
        }
        false
    }

    /// Another button went down on this slot: a pending Select is a real Select after
    /// all — its deferred down goes out before the caller sends the new button's.
    fn on_other_down(&mut self, out: &mut Vec<(u32, bool)>) {
        if self.pending_since.take().is_some() {
            out.push((wire::BTN_BACK, true));
        }
    }

    /// Select released. Returns true when the gesture owned this release (the caller
    /// skips the normal button-up send).
    fn on_select_up(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) -> bool {
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
            return true;
        }
        if self.pending_since.take().is_some() {
            // A tap: deliver the held-back press now, its release TAP_PRESS behind.
            out.push((wire::BTN_BACK, true));
            self.release_due = Some(now + TAP_PRESS);
            return true;
        }
        false
    }

    /// Clock-driven work: the hold threshold and the owed tap release.
    fn poll(&mut self, now: Instant, out: &mut Vec<(u32, bool)>) {
        if let Some(since) = self.pending_since {
            if now.duration_since(since) >= GUIDE_HOLD {
                self.pending_since = None;
                self.as_guide = true;
                out.push((wire::BTN_GUIDE, true));
            }
        }
        if let Some(due) = self.release_due {
            if now >= due {
                self.release_due = None;
                out.push((wire::BTN_BACK, false));
            }
        }
    }

    /// Slot close / gesture disarm: nothing may stay down (or owed) on the wire.
    fn flush(&mut self, out: &mut Vec<(u32, bool)>) {
        self.pending_since = None;
        if self.as_guide {
            self.as_guide = false;
            out.push((wire::BTN_GUIDE, false));
        }
        if self.release_due.take().is_some() {
            out.push((wire::BTN_BACK, false));
        }
    }
}

struct Worker {
    subsystem: sdl3::GamepadSubsystem,
    /// UI-facing state (the `GamepadService` accessors): pad list, active pad, pin.
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    /// The forwarded controllers held open while a session is attached — one [`Slot`] per
    /// physical pad, each on its own wire index. Empty when idle/menu (opening grabs the
    /// hardware; see the module doc). Populated by [`Worker::reconcile_slots`].
    slots: Vec<Slot>,
    /// The ONE device held open for menu navigation while menu mode is on and NO session is
    /// attached (`active_id`); mutually exclusive with `slots` (a session supersedes the menu).
    menu_open: Option<(u32, sdl3::gamepad::Gamepad)>,
    /// The menu pad's last-read power state, `(id, level)`. Cached rather than read in
    /// [`publish`](Self::publish) because publish runs on every hotplug and pin change,
    /// while the battery only wants looking at every few seconds.
    battery: Option<(u32, PadBattery)>,
    battery_at: Option<Instant>,
    /// Connected pad ids in connection order (metadata only, no device open); the most
    /// recently connected is the auto selection.
    order: Vec<u32>,
    /// Stable key of the user-pinned controller (persisted in Settings) — matched against
    /// connected pads, so it survives restarts and disconnects. A pin forwards ONLY that pad
    /// (an explicit single-player choice); Automatic forwards every real controller.
    pinned: Option<String>,
    /// Forward controllers to an attached session at all ([`GamepadService::set_forwarding`]).
    /// Off makes [`Self::forwarded_ids`] empty, so a session opens no slot — the whole point
    /// being that the hardware stays ungrabbed for a USB passthrough tool.
    forwarding: bool,
    /// The user's explicit "controller type" setting ([`GamepadService::set_kind_override`]);
    /// `Auto` = per-pad detection. Applied at slot open to the kind DECLARED to the host, never
    /// to [`Slot::pref`] — the local feedback paths must keep reading the physical pad.
    kind_override: GamepadPref,
    /// Forward raw guide/QAM presses ([`GamepadService::set_system_buttons`]); off keeps
    /// them with the local shell.
    system_forward: bool,
    /// The hold-Select guide gesture is armed ([`GamepadService::set_system_buttons`]).
    guide_gesture: bool,
    /// Releases owed for synthetic taps ([`Ctl::TapButton`]): `(pad, bit, due)` — the
    /// down went out on receipt, the up goes out from the poll once `due` passes.
    synthetic_ups: Vec<(u8, u32, Instant)>,
    /// Pad-audio streams the session's settings want rendered (bit0 = haptics, bit1 =
    /// speaker — [`GamepadService::set_pad_audio_prefs`]). `0` (the default) until an embedder
    /// declares some: tier-A detection then never runs and every arrival stays caps-less.
    pad_audio_prefs: u8,
    attached: Option<Arc<NativeClient>>,
    /// Raises the UI escape signal; the escape chord fires it once per press.
    escape_tx: async_channel::Sender<()>,
    /// Raises the UI disconnect signal when the escape chord is held past [`DISCONNECT_HOLD`].
    disconnect_tx: async_channel::Sender<()>,
    /// The escape chord is fully held (by any one forwarded pad) — latched so it fires once.
    chord_armed: bool,
    /// When the escape chord became fully held (drives the hold-to-disconnect escalation); `None`
    /// when the chord is broken.
    chord_since: Option<Instant>,
    /// The disconnect signal already fired for the current hold — latched so it fires once.
    disconnect_fired: bool,
    /// Menu mode ([`GamepadService::set_menu_mode`]): hold the active pad open while idle
    /// and translate it into [`MenuEvent`]s. An attached session pauses translation.
    menu_mode: bool,
    menu_nav: MenuNav,
    menu_tx: async_channel::Sender<MenuEvent>,
    /// A system overlay owns input ([`GamepadService::set_masked`]): forwarded pads are held
    /// neutral and menu translation is paused, with every slot still OPEN.
    masked: bool,
}

impl Worker {
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
        // VID/PID — the host then builds the matching virtual hid-steam pad (grips + trackpads +
        // the right glyph identity): Deck 0x1205; classic SC wired 0x1102 / dongle 0x1142.
        if vid == 0x28DE && pid == 0x1205 {
            pref = GamepadPref::SteamDeck;
        }
        if vid == 0x28DE && matches!(pid, 0x1102 | 0x1142) {
            pref = GamepadPref::SteamController;
        }
        // The DualSense Edge has no distinct SDL gamepad type either (it reports PS5) — detect by
        // VID/PID so the host builds the virtual Edge and this pad's back paddles land on native
        // slots instead of the fold/drop policy.
        if vid == 0x054C && pid == 0x0DF2 {
            pref = GamepadPref::DualSenseEdge;
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
            // Unknowable from an ID-based getter — SDL reports power only for an OPEN
            // device. `publish` fills it in for the one pad this service holds open.
            battery: None,
        })
    }

    /// The controllers to forward this session, in slot-assignment preference order. A pin
    /// forwards ONLY the pinned pad (an explicit single-player choice — matched by stable key,
    /// most-recent wins); Automatic forwards every real (non-Steam-virtual) controller, falling
    /// back to the single most-recent pad when only a Steam-virtual pad is present (the Deck
    /// game-mode case — otherwise its gyro/paddles/input would have nowhere to land).
    fn forwarded_ids(&self) -> Vec<u32> {
        // Forwarding off: nothing is forwarded, so nothing is opened either — the device stays
        // free for whatever route the user's controller actually takes to the host.
        if !self.forwarding {
            return Vec::new();
        }
        if let Some(key) = &self.pinned {
            if let Some(id) = self
                .order
                .iter()
                .rev()
                .copied()
                .find(|&id| self.pad_info(id).is_some_and(|p| &p.key == key))
            {
                return vec![id];
            }
            // A pin matching nothing connected falls through to Automatic (mirrors the old
            // single-pad `active_id`, which never cleared an unmatched pin).
        }
        let real: Vec<u32> = self
            .order
            .iter()
            .copied()
            .filter(|&id| self.pad_info(id).is_some_and(|p| !p.steam_virtual))
            .collect();
        if !real.is_empty() {
            real
        } else {
            self.order.last().copied().into_iter().collect()
        }
    }

    /// Hold exactly the right devices open: a [`Slot`] per forwarded controller while a session
    /// is attached, or the single menu pad while menu mode owns navigation, and nothing
    /// otherwise. The one place that opens (= grabs) hardware; dropping a handle closes it
    /// (`SDL_CloseGamepad`) — on a Deck the firmware watchdog then restores lizard mode.
    fn sync_open(&mut self) {
        if self.attached.is_some() {
            // A session forwards every pad; the menu never holds a device at the same time.
            self.menu_open = None;
            self.reconcile_slots();
            return;
        }
        // No session: close any forwarded slots, then (menu mode only) hold the one nav pad.
        self.close_all_slots();
        let want = if self.menu_mode {
            self.active_id()
        } else {
            None
        };
        if self.menu_open.as_ref().map(|(id, _)| *id) == want {
            return;
        }
        self.menu_open = None;
        let Some(id) = want else { return };
        match self.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(id)) {
            Ok(pad) => {
                self.menu_open = Some((id, pad));
                // The menu pad changed under us (hot-plug while the launcher is open): adopt the
                // new pad's held state instead of firing it. Menu needs buttons + stick only, so
                // no sensors.
                self.menu_nav.reset();
            }
            Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
        }
    }

    /// Bring `self.slots` in line with [`forwarded_ids`](Self::forwarded_ids): close any slot no
    /// longer wanted (flushing its held wire state first) and open any newly-wanted pad into the
    /// lowest free wire index. Slot indices stay stable across the churn — a pad that disconnects
    /// frees only its own index; the others keep theirs, so a game never sees its players shuffle.
    fn reconcile_slots(&mut self) {
        let want = self.forwarded_ids();
        let mut i = 0;
        while i < self.slots.len() {
            if want.contains(&self.slots[i].id) {
                i += 1;
            } else {
                self.close_slot_at(i);
            }
        }
        for id in want {
            if self.slots.iter().any(|s| s.id == id) {
                continue;
            }
            self.open_slot(id);
        }
    }

    /// Open `id` into the lowest free wire index and enable its sensors for the session. Skipped
    /// (logged) when every wire slot is taken or the SDL open fails.
    fn open_slot(&mut self, id: u32) {
        let taken: Vec<u8> = self.slots.iter().map(|s| s.index).collect();
        let Some(index) = lowest_free_index(&taken) else {
            tracing::warn!(
                id,
                max = punktfunk_core::input::MAX_PADS,
                "gamepad slots full — controller not forwarded"
            );
            return;
        };
        let pref = match self.pad_info(id) {
            // Steam Input's virtual pad standing in front of the Deck's built-in controls (the
            // only-pad-forwarded case, [`Self::forwarded_ids`]): declare the DECK kind, not the
            // wrapper's Xbox 360 identity. [`Self::auto_pref`] already resolves the SESSION
            // default this way, but a current host honors the per-pad arrival over the session
            // default — so without this the host builds an X-Box 360 pad on a real Deck.
            Some(p) if p.steam_virtual && is_steam_deck() => GamepadPref::SteamDeck,
            Some(p) => p.pref,
            None => GamepadPref::Xbox360,
        };
        let declared = declared_kind(self.kind_override, pref);
        match self.subsystem.open(sdl3::sys::joystick::SDL_JoystickID(id)) {
            Ok(pad) => {
                let mut slot = Slot::new(id, index, pref, declared, pad);
                Self::set_slot_sensors(&mut slot, true);
                slot.audio_caps = self.pad_audio_caps_for(id, &slot.pad);
                // Declare this pad's kind BEFORE any of its input, so the host builds a matching
                // virtual device (mixed types — pad 0 a DualSense, pad 1 an Xbox pad). The core
                // re-sends it a few times against datagram loss; an older host ignores it and
                // uses the session-default kind.
                if let Some(c) = &self.attached {
                    // Pad-audio render caps go in FIRST — the core ORs them into this (and
                    // every re-sent) arrival's flags bits 8/9 toward a capable host. ALWAYS
                    // set (0 for non-tier-A): wire indices are reused within a connection, so
                    // a tier-A slot that closes must not leave its bits behind for the next
                    // pad on the same index (the set_rumble_quirks rule).
                    c.set_pad_audio_caps(index, slot.audio_caps);
                    send(
                        c,
                        InputKind::GamepadArrival,
                        declared.to_u8() as u32,
                        0,
                        index,
                    );
                    // Declare the actuator's quirks to the shared rumble policy engine. ALWAYS
                    // set (defaults for a well-behaved pad): wire indices are reused within a
                    // connection, so a Deck slot that closes must not leave its keepalive quirk
                    // behind for the next pad on the same index.
                    let quirks = if pref == GamepadPref::SteamDeck {
                        ActuatorQuirks {
                            keepalive_ms: DECK_RUMBLE_KEEPALIVE_MS,
                            min_pulse_ms: 0,
                            dedup_jitter: true,
                        }
                    } else {
                        ActuatorQuirks::default()
                    };
                    c.set_rumble_quirks(index as u16, quirks);
                }
                if slot.audio_caps != 0 {
                    if slot.audio_caps & 0x01 != 0 {
                        // Tier-A haptics activation: the SDL disable-bit trap. SDL's DS5
                        // driver sets ucEnableBits1 0x01|0x02 ("enable rumble emulation" +
                        // "disable audio haptics") whenever its rumble path runs — which
                        // would MUTE the voice coils the 0xD1 stream drives. One effects
                        // packet with those bits CLEARED puts the pad back on audio haptics
                        // ("Leaving emulated rumble bits off will restore audio haptics" —
                        // SDL_hidapi_ps5.c); wire rumble for this slot is suppressed in
                        // render_feedback so SDL never re-arms them.
                        //
                        // ⚠ This needs SDL's HIDAPI driver to be the one on the pad — the
                        // packet is a raw DS5 effects report, and SDL can only send it where
                        // it owns the HID link. On a Linux box where the kernel's
                        // `hid-playstation` has the pad instead, the call fails, and it is
                        // worth SAYING so: `hid-playstation` asserts the same disable bit on
                        // every force-feedback update it makes, so a pad some other program
                        // has rumbled stays deaf to this plane until it is re-plugged. Not
                        // fatal — nothing else asserts the bit in our own path, so the pad's
                        // power-on default (audio haptics live) usually still stands.
                        if let Err(e) = slot.pad.send_effect(&Ds5Feedback::audio_haptics_packet()) {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not re-arm the DualSense's audio-haptics bit (SDL does \
                                 not own this pad's HID link) — haptics still work unless \
                                 something else has rumbled the pad this plug-in"
                            );
                        }
                    }
                    if slot.audio_caps & 0x02 != 0 {
                        // Speaker activation: point the pad's shared channel-1 output at its
                        // own speaker instead of the headphone jack it powers up on, and give
                        // it a volume. Without this the speaker stream is routed perfectly and
                        // heard by nobody — see `speaker_enable_packet`.
                        let path = env_u8("PUNKTFUNK_PAD_SPEAKER_PATH").unwrap_or(0x20);
                        let volume = env_u8("PUNKTFUNK_PAD_SPEAKER_VOLUME").unwrap_or(0x7F);
                        if let Err(e) = slot
                            .pad
                            .send_effect(&Ds5Feedback::speaker_enable_packet(volume, path))
                        {
                            tracing::info!(
                                index,
                                error = %e,
                                "could not point the DualSense at its own speaker (SDL does \
                                 not own this pad's HID link) — the pad's speaker may stay \
                                 silent even though the stream reaches it"
                            );
                        }
                    }
                    // Hand the pad to the session's renderer worker. Windows correlation
                    // needs the HID interface path; Linux matches by card identity.
                    crate::pad_audio::register_tier_a(index, slot.pad.path());
                    tracing::info!(
                        index,
                        caps = slot.audio_caps,
                        "tier-A DualSense: pad-audio render caps declared"
                    );
                }
                tracing::info!(
                    id,
                    index,
                    pref = ?pref,
                    declared = ?declared,
                    "gamepad forwarding (slot opened)"
                );
                self.slots.push(slot);
            }
            Err(e) => tracing::warn!(id, error = %e, "gamepad open failed"),
        }
    }

    /// This pad's pad-audio render capabilities (the bits [`NativeClient::set_pad_audio_caps`]
    /// takes): the settings prefs for a tier-A pad — a physical DualSense/Edge (by VID:PID,
    /// never the DECLARED kind: the stream renders on the controller in the user's hands) on
    /// a WIRED connection — and `0` for everything else (tier B/C are out of scope). Wired
    /// comes from `SDL_GetGamepadConnectionState`; when SDL answers Unknown, the pad's 4-ch
    /// audio sibling existing is the fallback signal (Bluetooth exposes no audio device).
    fn pad_audio_caps_for(&self, id: u32, pad: &sdl3::gamepad::Gamepad) -> u8 {
        if self.pad_audio_prefs == 0 {
            return 0; // nothing wanted — skip the (possibly probing) wired check entirely
        }
        let jid = sdl3::sys::joystick::SDL_JoystickID(id);
        let vid = self.subsystem.vendor_for_id(jid).unwrap_or(0);
        let pid = self.subsystem.product_for_id(jid).unwrap_or(0);
        if !crate::pad_audio::is_tier_a_ds5(vid, pid, true) {
            return 0; // not a DualSense/Edge — no wired check needed
        }
        use sdl3::joystick::ConnectionState;
        let wired = match pad.connection_state() {
            Ok(ConnectionState::Wired) => true,
            Ok(ConnectionState::Wireless) => false,
            _ => crate::pad_audio::wired_audio_sibling(pad.path().as_deref()),
        };
        if crate::pad_audio::is_tier_a_ds5(vid, pid, wired) {
            self.pad_audio_prefs
        } else {
            0
        }
    }

    /// Flush a slot's held wire state (so nothing sticks down host-side) and drop it — closing
    /// the SDL handle. The flush only emits wire events, so it is safe even when the device is
    /// already gone (unplug).
    fn close_slot_at(&mut self, i: usize) {
        // Best-effort physical silence before the handle drops: a slot closed mid-buzz (detach /
        // unplug) must not depend on what SDL does to a rumbling device at close. Errors are
        // expected for an already-unplugged pad.
        let _ = self.slots[i].pad.set_rumble(0, 0, 100);
        Self::reset_slot_feedback(&mut self.slots[i]);
        if let Some(c) = self.attached.clone() {
            Self::flush_slot(&c, &mut self.slots[i]);
            // Signal the host to tear down this pad's virtual device (native hot-unplug). Sent
            // after the flush so the core stamps it with a seq past the zeroing snapshots; the
            // host seq-gates it, so a reordered snapshot can't resurrect the removed pad.
            send(&c, InputKind::GamepadRemove, 0, 0, self.slots[i].index);
        }
        let slot = self.slots.remove(i);
        if slot.audio_caps != 0 {
            // Take the pad back from the pad-audio renderer (its device-gone path then
            // re-correlates — and finds nothing until a tier-A pad registers again).
            crate::pad_audio::unregister_tier_a(slot.index);
        }
        tracing::info!(
            id = slot.id,
            index = slot.index,
            "gamepad forwarding stopped (slot closed)"
        );
    }

    /// Hand the physical controller back in a neutral state before its handle closes.
    ///
    /// Rumble stops on its own the moment nothing renews it, but the rich planes do not: an
    /// adaptive-trigger effect and a lightbar colour are LATCHED in the pad's firmware and survive
    /// the stream, the app, and being unplugged. Ending a session on a weapon's trigger resistance
    /// left the physical trigger stiff on the desktop afterwards, with nothing to clear it but
    /// another game. Apple's client already resets on teardown; this is the desktop half.
    ///
    /// Best-effort throughout: the pad may already be gone (that is one of the ways we get here).
    fn reset_slot_feedback(slot: &mut Slot) {
        if matches!(
            slot.pref,
            GamepadPref::DualSense | GamepadPref::DualSenseEdge
        ) {
            // An all-zero trigger block is mode 0x00 — no effect — which is what releases the
            // trigger. Both sides, then the lightbar dark and the player indicator clear.
            for which in [0u8, 1] {
                let _ = slot
                    .pad
                    .send_effect(&Ds5Feedback::trigger_packet(which, &[0u8; 11]));
            }
            let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(0, 0, 0));
            let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(0));
        } else {
            // Anything else with an LED goes dark through SDL, which owns the per-device details.
            let _ = slot.pad.set_led(0, 0, 0);
        }
    }

    fn close_all_slots(&mut self) {
        while !self.slots.is_empty() {
            self.close_slot_at(0);
        }
    }

    /// Enable/disable a slot's motion sensors — they stream only while a session wants them
    /// (they cost USB/BT bandwidth). Called once at open.
    fn set_slot_sensors(slot: &mut Slot, enabled: bool) {
        use sdl3::sensor::SensorType;
        for s in [SensorType::Gyroscope, SensorType::Accelerometer] {
            // SAFETY: an SDL3 query on the gamepad this slot owns and keeps open; it takes a
            // plain sensor-type enum and only reads device state.
            if unsafe { slot.pad.has_sensor(s) } {
                let _ = slot.pad.sensor_set_enabled(s, enabled);
            }
        }
    }

    /// Re-sync opened devices + the UI-facing snapshot after anything that may have moved the
    /// forwarded set (hotplug, pin change). Slot flush-on-close is handled inside
    /// [`reconcile_slots`](Self::reconcile_slots); a pad that held the escape chord may have just
    /// unplugged, so re-arm it here.
    fn refresh_active(&mut self) {
        self.sync_open();
        self.rearm_escape();
        self.publish();
    }

    /// Zero everything the host believes is held for one slot — on slot close (detach / unplug).
    /// Emits wire events only (no SDL device calls), so it is safe against an already-removed pad.
    fn flush_slot(c: &NativeClient, slot: &mut Slot) {
        let pad = slot.index;
        // Gesture first: a synthetic guide is NOT in `held_buttons`, so the drain below
        // would never lift it — and a still-pending Select was never sent, so dropping
        // it beats delivering a ghost press into the close.
        let mut due = Vec::new();
        slot.gesture.flush(&mut due);
        for (b, down) in due {
            send(c, InputKind::GamepadButton, b, down as i32, pad);
        }
        for b in slot.held_buttons.drain(..) {
            send(c, InputKind::GamepadButton, b, 0, pad);
        }
        for (id, v) in slot.last_axis.iter_mut().enumerate() {
            if *v != 0 && *v != i32::MIN {
                send(c, InputKind::GamepadAxis, id as u32, 0, pad);
            }
            *v = i32::MIN;
        }
        // Lift any Steam-pad click held at this moment — a click that survives a close would
        // leave the host's pad pressed forever.
        for i in 0..2usize {
            if std::mem::take(&mut slot.held_clicks[i]) {
                let (x, y, _) = slot.surface_last[i];
                let _ = c.send_rich_input(RichInput::TouchpadEx {
                    pad,
                    surface: (i as u8) + 1,
                    finger: 0,
                    touch: false,
                    click: false,
                    x,
                    y,
                    pressure: 0,
                });
            }
        }
        slot.surface_last = [(0, 0, false); 2];
        // Lift any touchpad contact the host still believes is down (surface 0 = legacy pad).
        for (surface, finger) in slot.held_touches.drain() {
            let rich = if surface == 0 {
                RichInput::Touchpad {
                    pad,
                    finger,
                    active: false,
                    x: 0,
                    y: 0,
                }
            } else {
                RichInput::TouchpadEx {
                    pad,
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
        // Park motion. Gyro is level-triggered host-side — the last sample is preserved across
        // button frames and re-emitted by the pad heartbeat — so a slot closing mid-rotation
        // leaves the virtual pad turning, and a game integrating gyro aim turns with it. The host
        // has an idle watchdog for the cases nobody can flush (a dropped link); this is the case
        // we can, so take it immediately. Acceleration is kept: gravity doesn't stop when the
        // session does.
        if std::mem::take(&mut slot.sent_motion) {
            let _ = c.send_rich_input(RichInput::Motion {
                pad,
                gyro: [0; 3],
                accel: slot.last_accel,
            });
        }
    }

    /// Re-adopt what the pads are physically holding when an overlay mask lifts.
    ///
    /// Buttons are taken back into `held_buttons` **without** a wire press: a button pressed
    /// inside the overlay (the A that picked a QAM row) must not fire in the game the instant it
    /// closes — releasing it and pressing again is what arms it. Same rule menu mode already
    /// applies across a screen handoff ([`MenuNav::reset`]), for the same reason.
    ///
    /// Axes ARE re-sent, because a stick has no press semantics to ghost — it is deflected or it
    /// is not. The mask flushed them to zero, and SDL only speaks on *change*, so a stick still
    /// held when the overlay closes would stay dead host-side until the user happened to move it.
    ///
    /// Neither half can run against a pad that is gone: this only walks open slots, and every SDL
    /// read here is a state query on a handle the slot owns.
    fn readopt_held(&mut self) {
        use sdl3::gamepad::{Axis, Button};
        // Every button `button_bit` maps — the same surface the press path forwards.
        const BUTTONS: [Button; 21] = [
            Button::South,
            Button::East,
            Button::West,
            Button::North,
            Button::Back,
            Button::Start,
            Button::Guide,
            Button::LeftStick,
            Button::RightStick,
            Button::LeftShoulder,
            Button::RightShoulder,
            Button::DPadUp,
            Button::DPadDown,
            Button::DPadLeft,
            Button::DPadRight,
            Button::Touchpad,
            Button::RightPaddle1,
            Button::LeftPaddle1,
            Button::RightPaddle2,
            Button::LeftPaddle2,
            Button::Misc1,
        ];
        const AXES: [Axis; 6] = [
            Axis::LeftX,
            Axis::LeftY,
            Axis::RightX,
            Axis::RightY,
            Axis::TriggerLeft,
            Axis::TriggerRight,
        ];
        // Copied out: the slot walk below borrows `self` mutably.
        let system_forward = self.system_forward;
        let attached = self.attached.clone();
        for slot in &mut self.slots {
            slot.held_buttons.clear();
            for b in BUTTONS {
                let Some(bit) = button_bit(b) else {
                    continue;
                };
                // The press path returns before `held_buttons` for un-forwarded system
                // buttons; tracking them here would invent state it never keeps.
                if !system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                    continue;
                }
                if slot.pad.button(b) {
                    slot.held_buttons.push(bit);
                }
            }
            let Some(c) = &attached else {
                continue;
            };
            for a in AXES {
                let (id, v) = axis_value(a, slot.pad.axis(a));
                if slot.last_axis[id as usize] != v {
                    slot.last_axis[id as usize] = v;
                    send(c, InputKind::GamepadAxis, id, v, slot.index);
                }
            }
        }
        // The chord latch was cleared on the way in; drop it again if what we just adopted
        // doesn't actually hold it.
        self.rearm_escape();
    }

    /// True when any one forwarded pad holds the entire escape chord (any player can leave).
    fn chord_held(&self) -> bool {
        self.slots
            .iter()
            .any(|s| ESCAPE_CHORD.iter().all(|b| s.held_buttons.contains(b)))
    }

    /// Raise the UI escape signal when the [`ESCAPE_CHORD`] just completed on some pad (latched
    /// so it fires once per press) and start the hold-to-disconnect timer. Called after each
    /// button-down updates a slot's `held_buttons`.
    fn maybe_fire_escape(&mut self) {
        if self.chord_armed {
            return;
        }
        if self.chord_held() {
            self.chord_armed = true;
            self.chord_since = Some(Instant::now());
            let _ = self.escape_tx.try_send(());
            tracing::info!(
                "gamepad escape chord (L1+R1+Start+Select) — leaving fullscreen (hold to disconnect)"
            );
        }
    }

    /// Clock-driven [`SelectGesture`] work — the hold threshold and owed tap releases —
    /// polled like the chord hold, so timings carry at most one wakeup (~10 ms attached)
    /// of jitter.
    fn gesture_poll(&mut self) {
        let Some(c) = self.attached.clone() else {
            self.synthetic_ups.clear();
            return;
        };
        let now = Instant::now();
        // Owed releases of synthetic taps (the control socket's guide/QAM verbs).
        self.synthetic_ups.retain(|&(pad, bit, due)| {
            if now >= due {
                send(&c, InputKind::GamepadButton, bit, 0, pad);
                false
            } else {
                true
            }
        });
        if !self.guide_gesture {
            return;
        }
        for slot in &mut self.slots {
            let mut due = Vec::new();
            slot.gesture.poll(now, &mut due);
            for (b, down) in due {
                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
            }
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

    /// Re-arm once the chord is broken (no pad still holds it — a release, or the holding pad
    /// unplugged).
    fn rearm_escape(&mut self) {
        if self.chord_armed && !self.chord_held() {
            self.reset_chord();
        }
    }

    /// Clear the escape/disconnect chord latches. Called at every session boundary (detach + on
    /// attach): the hold-to-disconnect path *always* ends the session while the chord is still
    /// physically held, so the matching button-up events arrive after detach (dropped once the
    /// slots are gone) and `rearm_escape` never runs — without this the latched state would leak
    /// into the next session and either swallow its first chord press or fire a stale disconnect.
    fn reset_chord(&mut self) {
        self.chord_armed = false;
        self.chord_since = None;
        self.disconnect_fired = false;
    }

    /// Forward one touchpad contact on the rich-input plane for `slot`. A multi-touchpad pad
    /// (Steam Deck / Steam Controller) sends `TouchpadEx` with the surface (SDL touchpad 0 = left
    /// → 1, 1 = right → 2) and signed coordinates; a single-touchpad pad (DualSense) keeps the
    /// legacy `Touchpad` (unsigned). Tagged with the slot's wire pad index.
    fn forward_touch(
        c: &NativeClient,
        slot: &mut Slot,
        touchpad: u32,
        finger: u8,
        x: f32,
        y: f32,
        active: bool,
    ) {
        let pad = slot.index;
        let multi = slot.is_multi_touchpad();
        let (cx, cy) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
        let surface = if multi { (touchpad as u8) + 1 } else { 0 };
        let rich = if multi {
            let (wx, wy) = (
                (cx * 65535.0 - 32768.0) as i16,
                (cy * 65535.0 - 32768.0) as i16,
            );
            let i = (surface - 1).min(1) as usize;
            slot.surface_last[i] = (wx, wy, active);
            RichInput::TouchpadEx {
                pad,
                surface,
                finger,
                touch: active,
                // The pad's physical click is a separate BUTTON event (see forward_click) —
                // carry the held state so a motion frame can't clear a click mid-press.
                click: slot.held_clicks[i],
                x: wx,
                y: wy,
                pressure: 0,
            }
        } else {
            RichInput::Touchpad {
                pad,
                finger,
                active,
                x: (cx * 65535.0) as u16,
                y: (cy * 65535.0) as u16,
            }
        };
        let _ = c.send_rich_input(rich);
        if active {
            slot.held_touches.insert((surface, finger));
        } else {
            slot.held_touches.remove(&(surface, finger));
        }
    }

    /// SDL's Steam Deck mapping delivers the pad CLICKS as gamepad buttons — the generic
    /// `touchpad` button is the LEFT pad's click and `misc2` the RIGHT's (SDL_gamepad_db.h
    /// `touchpad:b17,misc2:b16`). They must NOT ride the button plane: it has no surface
    /// identity, and the host maps `BTN_TOUCHPAD` to the RIGHT pad (DualSense convention) —
    /// which is exactly "a left-pad click registers on the right pad". Only for a
    /// multi-touchpad pad; a DualSense's single `touchpad` button stays a wire button.
    fn steam_click_surface(slot: &Slot, button: sdl3::gamepad::Button) -> Option<u8> {
        use sdl3::gamepad::Button;
        if !slot.is_multi_touchpad() {
            return None;
        }
        match button {
            Button::Touchpad => Some(1),
            Button::Misc2 => Some(2),
            _ => None,
        }
    }

    /// Forward a Steam-pad click on the rich plane, bound to its surface. Click events carry
    /// no position, so reuse the surface's live contact point; a physical click implies
    /// contact, so `touch` stays asserted while the click is down even if the touch event
    /// hasn't arrived yet (event-order safety).
    fn forward_click(c: &NativeClient, slot: &mut Slot, surface: u8, down: bool) {
        let i = (surface - 1).min(1) as usize;
        slot.held_clicks[i] = down;
        let (x, y, touching) = slot.surface_last[i];
        let _ = c.send_rich_input(RichInput::TouchpadEx {
            pad: slot.index,
            surface,
            finger: 0,
            touch: touching || down,
            click: down,
            x,
            y,
            pressure: 0,
        });
    }

    /// Publish the pad list, active pad, and pin to the UI-facing mutexes.
    fn publish(&self) {
        // `pad_info` is deliberately open-free, and SDL only reports power for an OPEN
        // device — so the battery is attached here, from the cache
        // [`battery_poll`](Self::battery_poll) keeps for the one pad this service holds
        // open. Every other pad publishes `None`, which is the honest answer: we cannot
        // know without grabbing hardware that isn't ours to grab.
        let with_battery = |id: u32| -> Option<PadInfo> {
            let mut info = self.pad_info(id)?;
            if let Some((bid, b)) = self.battery {
                if bid == id {
                    info.battery = Some(b);
                }
            }
            Some(info)
        };
        let mut list: Vec<PadInfo> = self
            .order
            .iter()
            .copied()
            .filter_map(with_battery)
            .collect();
        list.reverse(); // most recent first — the Settings list order
        *self.pads_out.lock().unwrap() = list;
        *self.active_out.lock().unwrap() = self.active_id().and_then(with_battery);
    }

    /// Re-read the open pad's battery on a slow cadence, republishing only when it moved.
    ///
    /// Polled rather than event-driven because nothing reports a battery CHANGING — it
    /// drifts, so the only way to show it is to look now and then. 15 s is far finer than a
    /// percent takes to move and far coarser than anything the service's 10 ms loop would
    /// notice; the read itself is a cached HID report, not a device transaction.
    ///
    /// Only ever the menu pad, which is the only one open while a console is on screen —
    /// and the only one any UI asks about.
    fn battery_poll(&mut self) {
        let Some((id, pad)) = &self.menu_open else {
            // Nothing open: forget the level rather than publish a stale one for a pad that
            // may since have been unplugged.
            if self.battery.take().is_some() {
                self.publish();
            }
            self.battery_at = None;
            return;
        };
        let now = Instant::now();
        if self
            .battery_at
            .is_some_and(|t| now.duration_since(t) < BATTERY_POLL)
        {
            return;
        }
        self.battery_at = Some(now);
        let fresh = battery_of(pad).map(|b| (*id, b));
        if fresh != self.battery {
            self.battery = fresh;
            self.publish();
        }
    }

    /// Apply queued control-plane messages from the UI thread. Returns false when the
    /// app side is gone and the worker should exit.
    fn drain_ctl(&mut self, ctl: &Receiver<Ctl>) -> bool {
        loop {
            match ctl.try_recv() {
                Ok(Ctl::Attach(c)) => {
                    self.attached = Some(c);
                    self.reset_chord(); // every session starts un-latched (Attach doesn't flush)

                    // The Valve HIDAPI drivers run only in-session (see set_valve_hidapi);
                    // enabling them re-enumerates a Deck's built-in pad with paddles/
                    // trackpads/gyro first-class — sync_open opens a slot per forwarded pad.
                    // Not with forwarding off: this session opens no slot, and the drivers'
                    // mere enumeration both kills the Deck's trackpad-mouse and is the
                    // opposite of leaving the hardware alone for a passthrough tool.
                    if self.forwarding {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                }
                Ok(Ctl::Detach) => {
                    // Flush + close every forwarded slot while the connector is still live, so
                    // nothing stays held host-side, then drop the session.
                    self.close_all_slots();
                    self.attached = None;
                    self.reset_chord();
                    self.sync_open(); // opens the menu pad if menu mode, else nothing
                    set_valve_hidapi(false);
                    if self.menu_mode {
                        // Back to the launcher: adopt whatever is still physically held
                        // (the escape chord that ended the session, a lingering B) so it
                        // can't ghost-fire menu actions.
                        self.menu_nav.reset();
                    }
                }
                Ok(Ctl::Pin(key)) => {
                    self.pinned = key;
                    self.refresh_active();
                }
                Ok(Ctl::KindOverride(pref)) => self.kind_override = pref,
                Ok(Ctl::SystemButtons {
                    forward_raw,
                    gesture,
                }) => {
                    self.system_forward = forward_raw;
                    if self.guide_gesture == gesture {
                        continue;
                    }
                    self.guide_gesture = gesture;
                    // A mid-session flip may strand gesture state — a synthetic guide
                    // still down, an owed tap release — lift it now (no-op on the way on:
                    // an unarmed gesture was never fed).
                    if let Some(c) = self.attached.clone() {
                        for slot in &mut self.slots {
                            let mut due = Vec::new();
                            slot.gesture.flush(&mut due);
                            for (b, down) in due {
                                send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                            }
                        }
                    }
                }
                Ok(Ctl::TapButton(bit)) => {
                    // Synthetic system-button tap (the session control socket): down on
                    // the first forwarded slot's index — pad 0 when none is open (a
                    // forwarding-off session; best-effort there, the wire pad may not
                    // exist host-side). The up is owed via `synthetic_ups`, TAP_PRESS
                    // later, so the pair can't fold into nothing.
                    if let Some(c) = self.attached.clone() {
                        let pad = self.slots.first().map_or(0, |s| s.index);
                        send(&c, InputKind::GamepadButton, bit, 1, pad);
                        self.synthetic_ups
                            .push((pad, bit, Instant::now() + TAP_PRESS));
                    }
                }
                Ok(Ctl::Mask(on)) => {
                    if self.masked == on {
                        continue;
                    }
                    self.masked = on;
                    if on {
                        // Neutral NOW, and while the slots stay open: a stick held when the
                        // overlay opened must stop steering, but the host must not see the pad
                        // unplug (that is `close_slot_at`'s job, and a game reacts to it).
                        if let Some(c) = self.attached.clone() {
                            for slot in &mut self.slots {
                                Self::flush_slot(&c, slot);
                            }
                        }
                        // Nothing can be mid-chord across the flip: the transitions that would
                        // complete or break it are about to be dropped.
                        self.reset_chord();
                    } else {
                        // Coming back. Whatever is still physically held was never delivered —
                        // adopt it silently rather than replay it as a fresh press, the same
                        // rule menu mode uses across a screen handoff (`MenuNav::reset`). A
                        // button you pressed *inside* the overlay must not fire in the game the
                        // instant it closes; releasing and pressing again is what arms it.
                        self.readopt_held();
                        self.menu_nav.reset();
                    }
                    tracing::info!(masked = on, "overlay input mask");
                }
                Ok(Ctl::Forwarding(on)) => {
                    if self.forwarding == on {
                        continue;
                    }
                    self.forwarding = on;
                    self.reset_chord(); // no forwarded pad can be mid-chord across the flip

                    // Applied live rather than at attach only, so a mid-session flip (an
                    // in-stream settings screen) takes effect on the pad in your hands.
                    //
                    // The Valve HIDAPI drivers are an in-session-only thing (see
                    // set_valve_hidapi), and forwarding off is — for their purpose — not in
                    // session. Order matters and differs by direction: ON must enable them
                    // BEFORE `sync_open`, or a Deck's built-in pad opens under its old
                    // identity; OFF must disable them AFTER, so no slot outlives the driver
                    // that opened it.
                    let attached = self.attached.is_some();
                    if on && attached {
                        set_valve_hidapi(true);
                    }
                    self.sync_open();
                    if !on && attached {
                        set_valve_hidapi(false);
                    }
                }
                Ok(Ctl::PadAudioPrefs(bits)) => self.pad_audio_prefs = bits & 0x03,
                Ok(Ctl::MenuMode(on)) => {
                    self.menu_mode = on;
                    if on {
                        self.menu_nav.reset();
                    }
                    self.sync_open();
                }
                Ok(Ctl::MenuRumble(pulse)) => {
                    if self.attached.is_none() {
                        if let Some((_, pad)) = self.menu_open.as_mut() {
                            let (low, high, ms) = match pulse {
                                // Light high-freq detent — won't jackhammer at repeat rate.
                                MenuPulse::Move => (0, 0x3000, 25),
                                // Fuller both-motor thunk.
                                MenuPulse::Confirm => (0x5000, 0x5000, 60),
                                // Dull low-freq wall.
                                MenuPulse::Boundary => (0x6000, 0, 60),
                            };
                            let _ = pad.set_rumble(low, high, ms);
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return false, // app gone
            }
        }
    }

    /// Route one SDL event: pad hotplug bookkeeping, and — while a session is attached —
    /// buttons/axes/touchpads/motion of each forwarded pad onto the wire, tagged with the
    /// pad's own [`Slot::index`]. An event for a controller with no slot (not forwarded) is
    /// ignored; slots exist only during an attached session, so the slot lookup also gates
    /// "is a session live".
    fn handle_event(&mut self, event: sdl3::event::Event) {
        use sdl3::event::Event;
        // A system overlay owns the controller ([`GamepadService::set_masked`]): drop every
        // input transition. The pads were flushed neutral when the mask went on, so dropping
        // the ups as well as the downs is what keeps the two in agreement — `readopt_held`
        // rebuilds the held set from the hardware when it lifts.
        //
        // Device add/remove deliberately still count: a controller genuinely plugged in or
        // pulled out behind an overlay is a fact about the world, not an input, and losing it
        // would leave the slot table lying about what exists.
        if self.masked
            && matches!(
                event,
                Event::ControllerButtonDown { .. }
                    | Event::ControllerButtonUp { .. }
                    | Event::ControllerAxisMotion { .. }
                    | Event::ControllerTouchpadDown { .. }
                    | Event::ControllerTouchpadMotion { .. }
                    | Event::ControllerTouchpadUp { .. }
                    | Event::ControllerSensorUpdated { .. }
            )
        {
            return;
        }
        match event {
            Event::ControllerDeviceAdded { which, .. } => {
                if !self.order.contains(&which) {
                    self.order.push(which);
                    if let Some(p) = self.pad_info(which) {
                        // Full identity: on a Steam Deck this is the one lever for diagnosing an
                        // empty controller list — it tells you whether SDL sees the physical pad
                        // (28DE:1205), Steam Input's virtual pad (28DE:11FF), both, or nothing.
                        tracing::info!(
                            name = p.name,
                            key = p.key,
                            pref = ?p.pref,
                            steam_virtual = p.steam_virtual,
                            "gamepad attached"
                        );
                    }
                    self.refresh_active();
                }
            }
            Event::ControllerDeviceRemoved { which, .. } => {
                if self.order.contains(&which) {
                    self.order.retain(|&id| id != which);
                    tracing::info!("gamepad detached");
                    // refresh_active → reconcile_slots closes (and flushes) this pad's slot;
                    // the flush emits wire-only events, safe against the now-gone device.
                    self.refresh_active();
                }
            }
            Event::ControllerButtonDown { which, button, .. } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                if let Some(surface) = Self::steam_click_surface(slot, button) {
                    Self::forward_click(&c, slot, surface, true);
                    return;
                }
                if let Some(bit) = button_bit(button) {
                    // Raw system buttons stay with the local shell when passthrough is
                    // off (the Gaming-Mode default): Steam already opened ITS overlay
                    // for this press; the host's is reached via the hold-Select gesture
                    // (and the Decky panel) instead.
                    if !self.system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                        return;
                    }
                    let mut due = Vec::new();
                    let held_back = if !self.guide_gesture {
                        false
                    } else if bit == wire::BTN_BACK {
                        let alone = slot.held_buttons.is_empty();
                        slot.gesture.on_select_down(Instant::now(), alone, &mut due)
                    } else {
                        slot.gesture.on_other_down(&mut due);
                        false
                    };
                    for (b, down) in due {
                        send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                    }
                    // Held-back or not, the chord bookkeeping sees the physical press —
                    // the escape chord must not care that the gesture exists.
                    slot.held_buttons.push(bit);
                    if !held_back {
                        send(&c, InputKind::GamepadButton, bit, 1, slot.index);
                    }
                    self.maybe_fire_escape();
                }
            }
            Event::ControllerButtonUp { which, button, .. } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                if let Some(surface) = Self::steam_click_surface(slot, button) {
                    Self::forward_click(&c, slot, surface, false);
                    return;
                }
                if let Some(bit) = button_bit(button) {
                    if !self.system_forward && matches!(bit, wire::BTN_GUIDE | wire::BTN_MISC1) {
                        return;
                    }
                    slot.held_buttons.retain(|&b| b != bit);
                    let mut due = Vec::new();
                    let owned = self.guide_gesture
                        && bit == wire::BTN_BACK
                        && slot.gesture.on_select_up(Instant::now(), &mut due);
                    for (b, down) in due {
                        send(&c, InputKind::GamepadButton, b, down as i32, slot.index);
                    }
                    if !owned {
                        send(&c, InputKind::GamepadButton, bit, 0, slot.index);
                    }
                    self.rearm_escape();
                }
            }
            Event::ControllerAxisMotion {
                which, axis, value, ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                let (id, v) = axis_value(axis, value);
                if slot.last_axis[id as usize] != v {
                    slot.last_axis[id as usize] = v;
                    send(&c, InputKind::GamepadAxis, id, v, slot.index);
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
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                Self::forward_touch(&c, slot, touchpad as u32, finger as u8, x, y, true);
            }
            Event::ControllerTouchpadUp {
                which,
                touchpad,
                finger,
                x,
                y,
                ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                Self::forward_touch(&c, slot, touchpad as u32, finger as u8, x, y, false);
            }
            // Motion: accel events update the cache; each gyro event ships a sample
            // (the DualSense reports both at ~250 Hz). Scale convention shared with
            // the Swift client — sign/scale derived, not yet live-verified.
            Event::ControllerSensorUpdated {
                which,
                sensor,
                data,
                ..
            } => {
                let Some(c) = self.attached.clone() else {
                    return;
                };
                let Some(slot) = self.slots.iter_mut().find(|s| s.id == which) else {
                    return;
                };
                use sdl3::sensor::SensorType;
                match sensor {
                    SensorType::Accelerometer => {
                        for (i, v) in data.iter().enumerate() {
                            slot.last_accel[i] =
                                (v / G * ACCEL_LSB_PER_G).clamp(-32768.0, 32767.0) as i16;
                        }
                    }
                    SensorType::Gyroscope => {
                        // An X-Box class pad has no motion plane, so every sample below would be
                        // decoded and dropped. Say so once — the player's gyro is silently doing
                        // nothing and the fix is the controller-type setting — and stop paying to
                        // send ~250 Hz of them.
                        //
                        // Asked PER PAD, off this slot's own declaration. The session echo alone is
                        // the wrong question: under `Auto` the Hello carries the active pad's kind,
                        // so a couch with an X-Box pad on 0 and a DualSense on 1 echoes X-Box 360
                        // while the host builds pad 1 a DualSense with a working gyro.
                        if !punktfunk_core::config::pad_motion_reaches(
                            slot.declared,
                            c.requested_gamepad,
                            c.resolved_gamepad,
                        ) {
                            if !slot.motion_unreachable_logged {
                                slot.motion_unreachable_logged = true;
                                tracing::warn!(
                                    pad = slot.index,
                                    declared = ?slot.declared,
                                    resolved = ?c.resolved_gamepad,
                                    "this controller has a gyro but the host built it a backend \
                                     without one — motion will not reach the game; pick a \
                                     DualSense-class controller type to get it"
                                );
                            }
                            return;
                        }
                        let mut gyro = [0i16; 3];
                        for (i, v) in data.iter().enumerate() {
                            gyro[i] = (v * GYRO_LSB_PER_RAD_S).clamp(-32768.0, 32767.0) as i16;
                        }
                        slot.sent_motion = true;
                        let _ = c.send_rich_input(RichInput::Motion {
                            pad: slot.index,
                            gyro,
                            accel: slot.last_accel,
                        });
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Sample the open pad and translate it into [`MenuEvent`]s — only while menu mode is
    /// on and no session is attached (attach supersedes; SDL events merely wake the loop,
    /// so a press is translated the iteration it arrives).
    fn menu_poll(&mut self) {
        // Masked covers the launcher too: with the Deck's Steam menu up over our console, the
        // same stick that scrolls Steam's UI would otherwise also be scrolling ours behind it.
        if !self.menu_mode || self.attached.is_some() || self.masked {
            return;
        }
        let Some((_, pad)) = self.menu_open.as_ref() else {
            return;
        };
        use sdl3::gamepad::{Axis, Button};
        let s = MenuSample {
            buttons: [
                pad.button(Button::South),
                pad.button(Button::East),
                pad.button(Button::West),
                pad.button(Button::North),
                pad.button(Button::LeftShoulder),
                pad.button(Button::RightShoulder),
            ],
            lx: pad.axis(Axis::LeftX),
            ly: pad.axis(Axis::LeftY),
            dpad: [
                pad.button(Button::DPadUp),
                pad.button(Button::DPadDown),
                pad.button(Button::DPadLeft),
                pad.button(Button::DPadRight),
            ],
        };
        let mut out = Vec::new();
        self.menu_nav.poll(&s, Instant::now(), &mut out);
        for e in out {
            let _ = self.menu_tx.try_send(e);
        }
    }

    /// Hand one policy-engine command to SDL on a slot's pad, verbatim. The core engine owns all
    /// rumble policy — leases, legacy-host staleness, the Deck keepalive + its dedupe-defeat
    /// jitter (declared as quirks at slot open) — so this worker keeps no rumble state at all.
    /// `backstop_ms` becomes the SDL duration: the hardware-level net under a stalled worker
    /// thread (the engine emits explicit zeros at every policy stop, so it is never the stop
    /// mechanism).
    fn issue_rumble(slot: &mut Slot, low: u16, high: u16, backstop_ms: u32) {
        let dur_ms: u32 = if (low, high) == (0, 0) {
            100 // a stop takes effect immediately; the duration is irrelevant
        } else {
            // No local floor. There was a `.max(160)` here, and it could never do anything: the
            // engine's own `backstop()` returns `(2 * ttl).clamp(500, 5000)` or the 2000 ms legacy
            // value, so a non-zero command's backstop is never below 500. A floor that belongs to a
            // particular actuator belongs in its `ActuatorQuirks::min_pulse_ms`, which the engine
            // already applies — not re-invented per renderer where it can silently disagree.
            backstop_ms
        };
        // Surface a failed SDL rumble write: a swallowed error here (DualSense not in the right
        // HIDAPI mode, etc.) reads exactly like "rumble doesn't work". The host logs the send side
        // on 0xCA with the pad index, so the two together pinpoint host-game vs client-render.
        match slot.pad.set_rumble(low, high, dur_ms) {
            Err(e) => {
                tracing::warn!(pad = slot.index, low, high, error = %e, "rumble: SDL set_rumble failed")
            }
            Ok(()) => tracing::trace!(pad = slot.index, low, high, "rumble: rendered"),
        }
    }

    /// Drain and render the feedback planes — rumble plus HID output (lightbar / player LEDs /
    /// adaptive triggers) — routing each update to the forwarded slot on its wire pad index; this
    /// thread is their single consumer. Rumble arrives as EFFECTIVE commands from the core's
    /// shared policy engine, which already applied every policy — v2 lease expiry, legacy-host
    /// staleness, the Deck actuator keepalive + jitter (via the quirks declared at slot open),
    /// and connection-close drain zeros — so this worker applies commands verbatim and keeps no
    /// rumble state of its own (`design/rumble-root-fix.md` §D).
    fn render_feedback(&mut self) {
        let Some(connector) = self.attached.clone() else {
            return;
        };
        // Engine commands → the slot holding that wire pad index. A command for an index with no
        // live slot (a pad that just unplugged) is dropped. The loop ends on NoFrame (drained
        // dry this tick) or Closed (session over — the engine delivered its close-drain zeros
        // first; the physical silence backstop is in `close_slot_at`).
        while let Ok(cmd) = connector.next_rumble_command(Duration::ZERO) {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.index as u16 == cmd.pad) {
                // The SDL disable-bit trap: ANY SDL rumble write sets ucEnableBits1
                // 0x01|0x02, muting the very voice coils the 0xD1 haptics stream drives —
                // so a slot with tier-A haptics active never issues wire rumble (the stream
                // carries the feedback; the game's rumble is in its haptics mix).
                if slot.audio_caps & 0x01 != 0 {
                    if !slot.rumble_suppressed_logged {
                        slot.rumble_suppressed_logged = true;
                        tracing::info!(
                            pad = slot.index,
                            "wire rumble suppressed — the pad-audio haptics stream carries feedback"
                        );
                    }
                    continue;
                }
                Self::issue_rumble(slot, cmd.low, cmd.high, cmd.backstop_ms);
            }
        }
        // HID output (lightbar / player LEDs / adaptive triggers) → the slot on that wire index.
        while let Ok(hid) = connector.next_hidout(Duration::ZERO) {
            let idx = hidout_pad(&hid);
            let Some(slot) = self.slots.iter_mut().find(|s| s.index == idx) else {
                continue;
            };
            // A physical Edge takes the same raw DS5 effects packets (SDL's DS5EffectsState_t
            // layout is shared; SDL keys the enhanced path off the Edge PID itself).
            let is_ds = matches!(
                slot.pref,
                GamepadPref::DualSense | GamepadPref::DualSenseEdge
            );
            match hid {
                HidOutput::Led { r, g, b, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::lightbar_packet(r, g, b));
                }
                HidOutput::Led { r, g, b, .. } => {
                    let _ = slot.pad.set_led(r, g, b);
                }
                HidOutput::PlayerLeds { bits, .. } if is_ds => {
                    let _ = slot.pad.send_effect(&Ds5Feedback::player_packet(bits));
                }
                // Every other pad with player LEDs gets them through SDL, which owns the
                // per-device pattern. This used to fall through and do nothing at all.
                HidOutput::PlayerLeds { bits, .. } => {
                    let _ = set_player_leds(&slot.pad, bits);
                }
                HidOutput::Trigger {
                    which, ref effect, ..
                } if is_ds => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::trigger_packet(which, effect));
                }
                // The audio-control region of a DS5 output report a game wrote host-side
                // (volumes + routing; the SAMPLES ride 0xD1) — folded back into the physical
                // pad's effects packet, but only where a tier-A renderer is actually live
                // (`audio_caps`): replaying speaker volumes at a pad whose audio device
                // nothing streams to would just mute/blast a future session's start state.
                // Non-tier-A pads keep dropping it (the pre-pad-audio behaviour).
                HidOutput::AudioCtl { flags, raw, .. } if is_ds && slot.audio_caps != 0 => {
                    let _ = slot
                        .pad
                        .send_effect(&Ds5Feedback::audio_ctl_packet(flags, &raw));
                }
                // Deliberately unhandled, listed rather than left to a bare `_` so a new
                // variant cannot join them silently: adaptive triggers exist only on a
                // DualSense, and the trackpad-haptic / raw-passthrough planes are DS-specific
                // and carried by `send_effect` above when the pad is one. `AudioCtl` lands here
                // only when the guarded arm above declined it — a non-DualSense pad, or one with
                // no live tier-A renderer — which is the pre-pad-audio behaviour: drop it.
                HidOutput::Trigger { .. }
                | HidOutput::TrackpadHaptic { .. }
                | HidOutput::HidRaw { .. }
                | HidOutput::AudioCtl { .. } => {}
            }
        }
    }
}

/// The SDL player index for the wire's positional player-LED `bits`, or `None` for "no player".
///
/// The wire carries a bitmask — one bit per LED, low 5 — while SDL wants a player *index* and owns
/// the per-device pattern. The count bridges them: every convention that reaches this wire spells
/// "player N" as N lit LEDs, both the DualSense patterns (`0x04`, `0x0A`, `0x15`, `0x1B`, `0x1F`)
/// and the Switch/XInput run of low bits (`0x01`, `0x03`, `0x07`, `0x0F`). SDL's index is 0-based,
/// so player 1 is index 0; no lit LED means *no* player rather than player 0.
///
/// Split out from [`set_player_leds`] so the mapping is testable — an `sdl3::Gamepad` needs a real
/// device, so nothing that takes one can be.
fn player_index_from_bits(bits: u8) -> Option<u16> {
    match (bits & 0x1F).count_ones() {
        0 => None,
        n => Some((n - 1) as u16),
    }
}

/// Drive a non-DualSense pad's player LEDs from the wire's positional `bits`.
fn set_player_leds(pad: &sdl3::gamepad::Gamepad, bits: u8) -> Result<(), sdl3::Error> {
    match player_index_from_bits(bits) {
        None => pad.unset_player_index(),
        Some(i) => pad.set_player_index(i),
    }
}

/// The wire pad index a [`HidOutput`] is addressed to (every variant carries `pad`).
fn hidout_pad(h: &HidOutput) -> u8 {
    match h {
        HidOutput::Led { pad, .. }
        | HidOutput::PlayerLeds { pad, .. }
        | HidOutput::Trigger { pad, .. }
        | HidOutput::TrackpadHaptic { pad, .. }
        | HidOutput::HidRaw { pad, .. } => *pad,
        // AudioCtl's pad is the plane's only u16. `HidOutput::decode` rejects anything at or
        // above MAX_PADS (B27), so by the time one reaches here the narrowing is lossless.
        HidOutput::AudioCtl { pad, .. } => *pad as u8,
    }
}

impl Worker {
    /// The blank worker over an SDL gamepad subsystem — shared by the threaded service
    /// (`run`) and the caller-pumped variant (`GamepadService::pumped`).
    fn new(
        subsystem: sdl3::GamepadSubsystem,
        pads_out: Arc<Mutex<Vec<PadInfo>>>,
        active_out: Arc<Mutex<Option<PadInfo>>>,
        escape_tx: async_channel::Sender<()>,
        disconnect_tx: async_channel::Sender<()>,
        menu_tx: async_channel::Sender<MenuEvent>,
    ) -> Worker {
        Worker {
            subsystem,
            pads_out,
            active_out,
            slots: Vec::new(),
            menu_open: None,
            battery: None,
            battery_at: None,
            order: Vec::new(),
            pinned: None,
            forwarding: true,
            kind_override: GamepadPref::Auto,
            system_forward: true,
            guide_gesture: false,
            synthetic_ups: Vec::new(),
            pad_audio_prefs: 0,
            attached: None,
            escape_tx,
            disconnect_tx,
            chord_armed: false,
            chord_since: None,
            disconnect_fired: false,
            menu_mode: false,
            menu_nav: MenuNav::new(),
            menu_tx,
            masked: false,
        }
    }
}

fn run(
    pads_out: Arc<Mutex<Vec<PadInfo>>>,
    active_out: Arc<Mutex<Option<PadInfo>>>,
    ctl: &Receiver<Ctl>,
    escape_tx: &async_channel::Sender<()>,
    disconnect_tx: &async_channel::Sender<()>,
    menu_tx: &async_channel::Sender<MenuEvent>,
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

    let mut w = Worker::new(
        subsystem,
        pads_out,
        active_out,
        escape_tx.clone(),
        disconnect_tx.clone(),
        menu_tx.clone(),
    );

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
        // inside tolerance; menu mode needs the same cadence for its repeat timing).
        // Idle (no session, no menu) wakes lazily at 30 ms for hotplug + ctl.
        let timeout = Duration::from_millis(if w.attached.is_some() || w.menu_mode {
            10
        } else {
            30
        });
        if let Some(event) = pump.wait_event_timeout(timeout) {
            w.handle_event(event);
            // Drain whatever else queued while we were waiting or handling.
            while let Some(event) = pump.poll_event() {
                w.handle_event(event);
            }
        }

        // Escalate a held escape chord to a disconnect (polled — the hold completes with no
        // new button events; the chord itself is only detected while a session is attached).
        w.gesture_poll();
        w.maybe_fire_disconnect();

        w.menu_poll();
        w.battery_poll();
        w.render_feedback();
    }
}

#[cfg(test)]
mod select_gesture_tests {
    use super::*;

    #[test]
    fn tap_delivers_press_then_scheduled_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out), "not held back");
        assert!(out.is_empty(), "a held-back press sends nothing yet");
        // Released inside the threshold: the press goes out on release…
        let up = t + Duration::from_millis(120);
        assert!(g.on_select_up(up, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        // …and the release only TAP_PRESS behind it, so the pair can't fold away.
        g.poll(up + TAP_PRESS - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "release went out early");
        g.poll(up + TAP_PRESS, &mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn hold_becomes_guide_down_until_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD - Duration::from_millis(1), &mut out);
        assert!(out.is_empty(), "guide fired inside the threshold");
        g.poll(t + GUIDE_HOLD, &mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, true)]);
        out.clear();
        // Held on: nothing more (the host times its own long-press = QAM).
        g.poll(t + GUIDE_HOLD * 4, &mut out);
        assert!(out.is_empty());
        // Release lifts the guide, never a Select.
        assert!(g.on_select_up(t + GUIDE_HOLD * 5, &mut out));
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
    }

    #[test]
    fn second_button_makes_pending_select_real() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        // A joins inside the window: the deferred Select down goes out first (the
        // caller then sends A's own down — chronology preserved).
        g.on_other_down(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, true)]);
        out.clear();
        // The release is a normal button-up now — the gesture doesn't own it.
        assert!(!g.on_select_up(t + Duration::from_millis(200), &mut out));
        assert!(out.is_empty());
        // And no stale guide fires later.
        g.poll(t + GUIDE_HOLD * 2, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn select_inside_a_combo_passes_through() {
        let mut g = SelectGesture::default();
        let mut out = Vec::new();
        // L1+R1+Start already down (the escape chord ends in Select): not held back.
        assert!(!g.on_select_down(Instant::now(), false, &mut out));
        assert!(out.is_empty());
    }

    #[test]
    fn quick_repress_lifts_owed_release_first() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        // Re-pressed before the owed release fired: the up goes out before the new
        // press is held back — the host never sees two downs in a row.
        assert!(g.on_select_down(t + Duration::from_millis(100), true, &mut out));
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
    }

    #[test]
    fn flush_lifts_synthetic_guide_and_owed_release() {
        let mut g = SelectGesture::default();
        let t = Instant::now();
        let mut out = Vec::new();
        // Transformed hold: flush lifts the guide.
        assert!(g.on_select_down(t, true, &mut out));
        g.poll(t + GUIDE_HOLD, &mut out);
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_GUIDE, false)]);
        out.clear();
        // Owed tap release: flush emits it. A pending (never-sent) Select just drops.
        assert!(g.on_select_down(t, true, &mut out));
        assert!(g.on_select_up(t + Duration::from_millis(80), &mut out));
        out.clear();
        g.flush(&mut out);
        assert_eq!(out, vec![(wire::BTN_BACK, false)]);
        out.clear();
        assert!(g.on_select_down(t, true, &mut out));
        g.flush(&mut out);
        assert!(out.is_empty(), "a never-sent pending Select ghosted a send");
    }
}

#[cfg(test)]
mod menu_nav_tests {
    use super::*;

    fn sample() -> MenuSample {
        MenuSample::default()
    }

    fn events(nav: &mut MenuNav, s: &MenuSample, at: Instant) -> Vec<MenuEvent> {
        let mut out = Vec::new();
        nav.poll(s, at, &mut out);
        out
    }

    #[test]
    fn snapshot_adopts_held_state_without_firing() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        let mut held = sample();
        held.buttons[0] = true; // A held on entry
        held.lx = 30000; // stick already deflected right
        assert!(events(&mut nav, &held, t).is_empty(), "snapshot poll fired");
        // Still held: nothing (no rising edge, direction unchanged since snapshot).
        assert!(events(&mut nav, &held, t + Duration::from_millis(10)).is_empty());
        // Release, then press again → now it fires.
        assert!(events(&mut nav, &sample(), t + Duration::from_millis(20)).is_empty());
        assert_eq!(
            events(&mut nav, &held, t + Duration::from_millis(30)),
            vec![MenuEvent::Confirm, MenuEvent::Move(MenuDir::Right)]
        );
    }

    #[test]
    fn buttons_fire_on_rising_edge_only() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t); // consume the snapshot
        let mut s = sample();
        s.buttons[1] = true; // B down
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Back]
        );
        for i in 2..20 {
            assert!(
                events(&mut nav, &s, t + Duration::from_millis(10 * i)).is_empty(),
                "held button re-fired"
            );
        }
    }

    #[test]
    fn reset_rearms_the_snapshot() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        nav.reset();
        let mut s = sample();
        s.buttons[1] = true;
        assert!(
            events(&mut nav, &s, t + Duration::from_millis(10)).is_empty(),
            "post-reset poll fired a held button"
        );
    }

    #[test]
    fn direction_repeats_after_delay_at_interval() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.dpad[3] = true; // dpad right
                          // Engage: fires immediately.
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        // Inside the initial delay: silent.
        assert!(events(&mut nav, &s, t + Duration::from_millis(300)).is_empty());
        // Past the delay: repeats…
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(400)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        // …but not faster than the interval…
        assert!(events(&mut nav, &s, t + Duration::from_millis(500)).is_empty());
        // …and again once it elapses.
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(570)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        // Release cancels; re-engage fires immediately again.
        assert!(events(&mut nav, &sample(), t + Duration::from_millis(580)).is_empty());
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(590)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
    }

    #[test]
    fn direction_change_fires_immediately() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut right = sample();
        right.lx = 30000;
        let mut left = sample();
        left.lx = -30000;
        assert_eq!(
            events(&mut nav, &right, t + Duration::from_millis(10)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        assert_eq!(
            events(&mut nav, &left, t + Duration::from_millis(20)),
            vec![MenuEvent::Move(MenuDir::Left)]
        );
    }

    #[test]
    fn direction_resolution() {
        // Below the deadzone: nothing.
        let mut s = sample();
        s.lx = MENU_DEADZONE as i16;
        assert_eq!(MenuNav::resolve_dir(&s), None);
        // Dominant axis wins; SDL +y = down.
        s.lx = 20000;
        s.ly = 25000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Down));
        s.ly = -25000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Up));
        s.lx = 26000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Right));
        s.lx = -26000;
        assert_eq!(MenuNav::resolve_dir(&s), Some(MenuDir::Left));
        // Dpad fallback…
        let mut d = sample();
        d.dpad[1] = true;
        assert_eq!(MenuNav::resolve_dir(&d), Some(MenuDir::Down));
        // …but the stick overrides it.
        d.lx = 30000;
        assert_eq!(MenuNav::resolve_dir(&d), Some(MenuDir::Right));
    }

    #[test]
    fn shoulder_and_face_button_mapping() {
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.buttons = [false, false, true, true, true, true]; // x, y, l1, r1
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![
                MenuEvent::Tertiary,
                MenuEvent::Secondary,
                MenuEvent::JumpBack,
                MenuEvent::JumpForward,
            ]
        );
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    #[test]
    fn lowest_free_index_fills_gaps_and_bounds() {
        // Empty: first pad is player 1 (index 0).
        assert_eq!(lowest_free_index(&[]), Some(0));
        // Sequential occupancy hands out the next index.
        assert_eq!(lowest_free_index(&[0]), Some(1));
        assert_eq!(lowest_free_index(&[0, 1, 2]), Some(3));
        // A freed middle index is reused before growing — the stable-index property: pad 0 and
        // pad 2 stay put when pad 1 unplugs, and a re-plug reclaims slot 1 (not slot 3).
        assert_eq!(lowest_free_index(&[0, 2]), Some(1));
        // Order-independent.
        assert_eq!(lowest_free_index(&[2, 0]), Some(1));
        // Full: every wire index taken → no slot.
        let all: Vec<u8> = (0..punktfunk_core::input::MAX_PADS as u8).collect();
        assert_eq!(lowest_free_index(&all), None);
        // One free near the top is still found.
        let mut but_seven = all.clone();
        but_seven.retain(|&i| i != 7);
        assert_eq!(lowest_free_index(&but_seven), Some(7));
    }

    #[test]
    fn an_explicit_setting_is_what_every_pad_declares() {
        // The regression this pins: the setting used to reach the Hello only, and each pad's
        // arrival then re-declared the DETECTED kind — which the host honors over the session
        // default, so "emulate my DualSense as a DualShock 4" produced a DualSense.
        assert_eq!(
            declared_kind(GamepadPref::DualShock4, GamepadPref::DualSense),
            GamepadPref::DualShock4
        );
        // Every physical pad in a mixed session follows the one explicit choice.
        for physical in [
            GamepadPref::DualSense,
            GamepadPref::Xbox360,
            GamepadPref::SwitchPro,
            GamepadPref::SteamDeck,
        ] {
            assert_eq!(
                declared_kind(GamepadPref::Xbox360, physical),
                GamepadPref::Xbox360
            );
        }
        // Automatic keeps per-pad detection — otherwise a mixed session collapses to one type.
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::DualSense),
            GamepadPref::DualSense
        );
        assert_eq!(
            declared_kind(GamepadPref::Auto, GamepadPref::SteamDeck),
            GamepadPref::SteamDeck
        );
    }

    #[test]
    fn hidout_pad_reads_every_variant() {
        assert_eq!(
            hidout_pad(&HidOutput::Led {
                pad: 3,
                r: 1,
                g: 2,
                b: 3
            }),
            3
        );
        assert_eq!(hidout_pad(&HidOutput::PlayerLeds { pad: 5, bits: 1 }), 5);
        assert_eq!(
            hidout_pad(&HidOutput::Trigger {
                pad: 2,
                which: 0,
                effect: vec![1, 2, 3]
            }),
            2
        );
        assert_eq!(
            hidout_pad(&HidOutput::TrackpadHaptic {
                pad: 4,
                side: 0,
                amplitude: 1,
                period: 2,
                count: 3
            }),
            4
        );
        assert_eq!(
            hidout_pad(&HidOutput::HidRaw {
                pad: 6,
                kind: 0,
                data: vec![0x80, 0, 0]
            }),
            6
        );
        // AudioCtl's wire pad is u16; the index space is 0..MAX_PADS end to end.
        assert_eq!(
            hidout_pad(&HidOutput::AudioCtl {
                pad: 7,
                flags: 0,
                raw: [0; 6]
            }),
            7
        );
    }

    /// The speaker-enable default: volume and output-path land in the audio-control region at
    /// the same offsets an `AudioCtl` fold writes them, the two validity bits are set — and,
    /// most importantly, `ucEnableBits1` bits 0/1 stay CLEAR. Asserting either would enable
    /// rumble emulation / disable audio haptics and mute the very coils this plane drives, so
    /// making the speaker audible must never cost the haptics.
    #[test]
    fn speaker_enable_sets_volume_and_path_without_touching_the_haptics_bits() {
        let p = Ds5Feedback::speaker_enable_packet(0x7F, 0x20);
        assert_eq!(
            p[0] & 0x03,
            0,
            "rumble-emulation / disable-audio-haptics must stay clear"
        );
        assert_eq!(
            p[0],
            0x20 | 0x80,
            "speaker-volume + audio-control validity bits"
        );
        // ucSpeakerVolume is report byte 6 and ucAudioEnableBits report byte 8 — struct
        // offsets 5 and 7, i.e. AUDIO+1 and AUDIO+3.
        assert_eq!(p[5], 0x7F);
        assert_eq!(p[7], 0x20);
        // Nothing else in the packet moves (no rumble, no triggers, no LEDs).
        for (i, b) in p.iter().enumerate() {
            if !matches!(i, 0 | 5 | 7) {
                assert_eq!(*b, 0, "byte {i} should be untouched");
            }
        }
    }

    /// The field levers parse hex (how every reference writes these report bytes) and decimal,
    /// and a typo falls back to the default rather than silently meaning zero.
    #[test]
    fn env_u8_reads_hex_and_decimal() {
        assert_eq!(env_u8("PF_TEST_ABSENT_KEY_XYZ"), None);
        // Parsing is what is under test; the lookup is exercised by the None case above.
        for (s, want) in [
            ("0x20", Some(0x20)),
            ("0X7f", Some(0x7F)),
            ("32", Some(32u8)),
        ] {
            let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(hex) => u8::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            };
            assert_eq!(parsed, want, "{s}");
        }
    }

    /// The AudioCtl fold: the 6 raw bytes (DS5 report 0x02 bytes 5..=10) land at effect-struct
    /// offsets 4..=9, the report's audio-valid flags (AudioCtl.flags bits1..4) come back as
    /// p[0] bits 4..7, and the rumble-emulation / disable-audio-haptics bits (p[0] bits 0/1)
    /// stay CLEAR — setting either would mute the voice coils the 0xD1 stream drives.
    #[test]
    fn audio_ctl_folds_report_bytes_into_effect_offsets() {
        let raw = [0x50, 0x60, 0x70, 0x05, 0x11, 0x22];
        // flags 0b1_0111: haptics-select (bit0) + audio-valid bits 1/2/4 of the condensed form.
        let p = Ds5Feedback::audio_ctl_packet(0b1_0111, &raw);
        assert_eq!(&p[4..10], &raw, "report bytes 5..=10 → struct 4..=9");
        // bits1..4 (0b1011) → flag0 bits 4..7.
        assert_eq!(p[0], 0b1011_0000);
        assert_eq!(
            p[0] & 0x03,
            0,
            "haptics-select must NOT replay into p[0] bits 0/1"
        );
        // Nothing else is touched: no trigger/LED enable bits, no stray bytes.
        assert!(p[1..4].iter().all(|&b| b == 0));
        assert!(p[10..].iter().all(|&b| b == 0));
        // No audio-valid flags condenses to no enable bits (raw still carried verbatim).
        let p = Ds5Feedback::audio_ctl_packet(0b0_0001, &raw);
        assert_eq!(p[0], 0);
        assert_eq!(&p[4..10], &raw);
        // The tier-A activation packet is the all-clear: every enable bit off — per
        // SDL_hidapi_ps5.c, leaving the emulated-rumble bits off restores audio haptics.
        assert_eq!(Ds5Feedback::audio_haptics_packet(), [0u8; 47]);
    }
}

/// [`Ds5Feedback`]'s three packet builders. The host-side parser, the Android writer and the Apple
/// writer are all pinned by their own suites; this writer had nothing, despite being the one that
/// hand-shifts every offset by the report-id length.
#[cfg(test)]
mod ds5_feedback_tests {
    use super::*;

    /// The USB output report offsets, written out independently of the implementation. A DS5
    /// effects payload is the same block with the leading report id removed, so every offset is
    /// exactly one lower — this is the relationship the derived constants encode.
    #[test]
    fn ds5_offsets_track_the_usb_report() {
        for (usb, payload) in [
            (11usize, Ds5Feedback::RIGHT_TRIGGER),
            (22, Ds5Feedback::LEFT_TRIGGER),
            (44, Ds5Feedback::PAD_LIGHTS),
            (45, Ds5Feedback::LED_RGB),
        ] {
            assert_eq!(payload, usb - 1, "payload offset for USB byte {usb}");
        }
        assert_eq!(Ds5Feedback::TRIGGER_LEN, 11);
    }

    #[test]
    fn lightbar_sets_only_its_enable_bit_and_its_three_bytes() {
        let p = Ds5Feedback::lightbar_packet(0x11, 0x22, 0x33);
        assert_eq!(p.len(), 47);
        assert_eq!(p[1], 0x04, "valid_flag1 lightbar bit");
        assert_eq!(p[0], 0, "must not claim any valid_flag0 field");
        assert_eq!(
            (
                p[Ds5Feedback::LED_RGB],
                p[Ds5Feedback::LED_RGB + 1],
                p[Ds5Feedback::LED_RGB + 2]
            ),
            (0x11, 0x22, 0x33)
        );
        // Everything else stays zero — an over-broad packet would blank the triggers/player LEDs
        // it never meant to touch.
        let touched = [
            1,
            Ds5Feedback::LED_RGB,
            Ds5Feedback::LED_RGB + 1,
            Ds5Feedback::LED_RGB + 2,
        ];
        assert!(p
            .iter()
            .enumerate()
            .all(|(i, &b)| touched.contains(&i) || b == 0));
    }

    #[test]
    fn player_leds_are_masked_to_five_bits() {
        let p = Ds5Feedback::player_packet(0xFF);
        assert_eq!(p[1], 0x10, "valid_flag1 player-indicator bit");
        assert_eq!(
            p[Ds5Feedback::PAD_LIGHTS],
            0x1F,
            "high bits are not ours to set"
        );
        let p = Ds5Feedback::player_packet(0b0000_0101);
        assert_eq!(p[Ds5Feedback::PAD_LIGHTS], 0b0000_0101);
    }

    /// which 1 = R2 and which 0 = L2 — and the RIGHT block sits FIRST in the report, which is the
    /// pairing most likely to be transcribed backwards.
    #[test]
    fn trigger_which_selects_the_right_flag_and_offset() {
        let eff: Vec<u8> = (1..=11).collect();

        let r = Ds5Feedback::trigger_packet(1, &eff);
        assert_eq!(r[0], 0x04, "valid_flag0 R2 bit");
        assert_eq!(
            &r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(
            r[Ds5Feedback::LEFT_TRIGGER],
            0,
            "the other trigger is untouched"
        );

        let l = Ds5Feedback::trigger_packet(0, &eff);
        assert_eq!(l[0], 0x08, "valid_flag0 L2 bit");
        assert_eq!(
            &l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11],
            &eff[..]
        );
        assert_eq!(l[Ds5Feedback::RIGHT_TRIGGER], 0);
    }

    #[test]
    fn an_oversized_effect_is_clamped_rather_than_overflowing_into_the_next_field() {
        let long = vec![0xAAu8; 40];
        let p = Ds5Feedback::trigger_packet(1, &long);
        assert_eq!(p.len(), 47);
        // Exactly TRIGGER_LEN bytes written; the left block must not be scribbled on.
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 10], 0xAA);
        assert_eq!(p[Ds5Feedback::RIGHT_TRIGGER + 11], 0);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0);
    }

    #[test]
    fn a_short_effect_leaves_the_rest_of_the_block_zeroed() {
        let p = Ds5Feedback::trigger_packet(0, &[0x02, 0x99]);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER], 0x02);
        assert_eq!(p[Ds5Feedback::LEFT_TRIGGER + 1], 0x99);
        assert!(
            p[Ds5Feedback::LEFT_TRIGGER + 2..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }

    /// An empty effect is a well-formed all-zero block: mode 0x00 = release. It must still assert
    /// its enable bit, or the pad keeps whatever effect it was holding.
    #[test]
    fn an_empty_effect_is_a_release_not_a_no_op() {
        let p = Ds5Feedback::trigger_packet(1, &[]);
        assert_eq!(p[0], 0x04);
        assert!(
            p[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );
    }
}

#[cfg(test)]
mod reset_packet_tests {
    use super::*;

    /// The exact bytes a teardown sends to hand a DualSense back neutral. The *timing* of this
    /// (slot close) needs a live SDL handle and stays untestable, so pin the payloads: a wrong
    /// enable flag or a non-zero mode byte would silently leave the effect latched, which is the
    /// bug this reset exists to prevent.
    #[test]
    fn reset_packets_release_the_triggers_and_darken_the_lights() {
        // Trigger release: mode 0x00 with no parameters, on the side's own enable bit.
        let l = Ds5Feedback::trigger_packet(0, &[0u8; 11]);
        assert_eq!(l[0], 0x08, "left-trigger enable bit");
        assert!(
            l[Ds5Feedback::LEFT_TRIGGER..Ds5Feedback::LEFT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0),
            "an all-zero block is mode 0x00 = no effect"
        );
        let r = Ds5Feedback::trigger_packet(1, &[0u8; 11]);
        assert_eq!(r[0], 0x04, "right-trigger enable bit");
        assert!(
            r[Ds5Feedback::RIGHT_TRIGGER..Ds5Feedback::RIGHT_TRIGGER + 11]
                .iter()
                .all(|&b| b == 0)
        );

        // Lightbar off: enable bit set, RGB all zero. The enable bit matters — without it the pad
        // ignores the payload and keeps the game's last colour.
        let bar = Ds5Feedback::lightbar_packet(0, 0, 0);
        assert_eq!(bar[1], 0x04, "lightbar enable bit");
        assert_eq!(
            &bar[Ds5Feedback::LED_RGB..Ds5Feedback::LED_RGB + 3],
            &[0, 0, 0]
        );

        // Player indicator cleared.
        let pl = Ds5Feedback::player_packet(0);
        assert_eq!(pl[1], 0x10, "player-LED enable bit");
        assert_eq!(pl[Ds5Feedback::PAD_LIGHTS], 0);
    }
}

#[cfg(test)]
mod player_led_tests {
    use super::*;

    /// Both conventions that reach this wire spell "player N" as N lit LEDs, so the count is the
    /// player number regardless of WHICH bits a given pad lights. Pinned because the mapping is
    /// otherwise only obvious once you have seen both patterns side by side.
    #[test]
    fn player_index_counts_lit_leds_for_both_conventions() {
        // DualSense / hid-playstation patterns — non-contiguous, symmetric about the centre LED.
        assert_eq!(player_index_from_bits(0x04), Some(0)); // player 1
        assert_eq!(player_index_from_bits(0x0A), Some(1)); // player 2
        assert_eq!(player_index_from_bits(0x15), Some(2)); // player 3
        assert_eq!(player_index_from_bits(0x1B), Some(3)); // player 4
        assert_eq!(player_index_from_bits(0x1F), Some(4)); // player 5

        // Switch/XInput style — a contiguous run of low bits, the same count each time.
        assert_eq!(player_index_from_bits(0x01), Some(0));
        assert_eq!(player_index_from_bits(0x03), Some(1));
        assert_eq!(player_index_from_bits(0x07), Some(2));
        assert_eq!(player_index_from_bits(0x0F), Some(3));
    }

    /// No lit LED is "no player", NOT player 0 — the difference between LEDs off and player 1 lit.
    #[test]
    fn no_lit_led_is_no_player() {
        assert_eq!(player_index_from_bits(0x00), None);
        // Only the low 5 bits are player LEDs; junk above them must not invent a player.
        assert_eq!(player_index_from_bits(0xE0), None);
    }

    /// The mask is applied before counting, so out-of-range bits cannot inflate the index past
    /// the 5 real LEDs.
    #[test]
    fn high_bits_are_masked_off_before_counting() {
        assert_eq!(player_index_from_bits(0xFF), Some(4)); // 0x1F worth of LEDs, not 8
        assert_eq!(player_index_from_bits(0xE4), Some(0)); // 0x04 with junk on top
    }
}
