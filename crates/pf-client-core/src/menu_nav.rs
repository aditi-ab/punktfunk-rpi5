//! The controller **menu** vocabulary and its synthesizer, portable: [`MenuEvent`]s (what a
//! console screen consumes), [`MenuNav`] (raw pad sample → events: edge-triggered buttons,
//! snapshot-on-entry, stick/D-pad direction with initial-delay auto-repeat) and the pad
//! descriptors the console's controller chip renders ([`PadInfo`], [`PadBattery`]).
//!
//! Split out of `gamepad` (the SDL3 service, desktop-only) so the Skia console shell can run
//! on Android, where Kotlin captures the raw pad and hands the same [`MenuSample`]s over JNI.
//! One synthesizer means one navigation feel on every platform — the repeat cadence, the
//! dead zone and the hysteresis are decided here and nowhere else.
//!
//! Apple `GamepadMenuInput` parity is the reference for the numbers.

use punktfunk_core::config::GamepadPref;
use std::time::{Duration, Instant};

/// Stick deflection below this is ignored for menu navigation (0.5 of full scale — Apple
/// `GamepadMenuInput` parity; menus want deliberate flicks, not drift).
pub const MENU_DEADZONE: u16 = 16384;
/// A held direction starts auto-repeating after this initial delay…
pub const MENU_REPEAT_DELAY: Duration = Duration::from_millis(380);
/// …and then repeats at this cadence until released or changed.
pub const MENU_REPEAT_INTERVAL: Duration = Duration::from_millis(160);
/// Once a stick direction is ENGAGED it stays engaged until its own axis falls back below
/// this (0.3 of full scale) — the release threshold of the hysteresis both the Apple and
/// Android console inputs had to grow on glass. Without it a right flick that leaves the
/// dead zone slightly diagonal reads UP-then-RIGHT (two moves for one gesture) and a held
/// stick that wobbles across the dominant-axis boundary jitters between two directions.
pub const MENU_RELEASE: u16 = 9830;
// The hysteresis is only a hysteresis if the release point sits below the engage point; a
// future "tune the dead zone" edit that crosses them would silently turn this back into the
// jittery stateless resolver — so the compiler holds the line.
const _: () = assert!(MENU_RELEASE > 0 && MENU_RELEASE < MENU_DEADZONE);

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
    /// The left stick's angle as one of the ring's six 60° sectors (design §2.6, D12): fired
    /// when the sector changes — `Some(k)` for slot `k`, `None` when the stick returns to
    /// neutral. Only the ring reads it; every list keeps stepping on `Move`.
    Sector(Option<u8>),
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
pub struct MenuSample {
    /// a, b, x, y, l1, r1 — the order [`MenuNav::poll`] maps to events.
    pub buttons: [bool; 6],
    /// Left stick, SDL convention (+y = down).
    pub lx: i16,
    pub ly: i16,
    /// up, down, left, right.
    pub dpad: [bool; 4],
}

/// The pure menu-input state machine (no SDL types — unit-tested below). Port of the
/// Swift client's `GamepadMenuInput`: the poll after a [`reset`](Self::reset) adopts the
/// currently-held buttons and direction WITHOUT firing, so a press that crossed a screen
/// handoff (the B that closed a stream, a held A on mode entry) must be released before
/// it can act; buttons fire on the rising edge only.
pub struct MenuNav {
    /// Adopt the next sample silently (set on mode entry / stream detach / pad change).
    snapshot_pending: bool,
    /// Previous button states, [`MenuSample::buttons`] order.
    was: [bool; 6],
    dir: Option<MenuDir>,
    /// When `dir` engaged — start of the initial-repeat delay.
    dir_since: Instant,
    last_repeat: Instant,
    /// The ring sector the stick last resolved to (see [`ring_sector`]).
    sector: Option<u8>,
    /// `sector` went out as an event. A sector the snapshot adopted never did, so its release
    /// stays silent too — the ring never engaged that stick.
    sector_announced: bool,
}

/// A sector, once engaged, keeps the stick until the angle is this far past its 30° edge —
/// a stick resting on the boundary between two slots would otherwise flicker between them.
pub const SECTOR_OVERLAP_DEG: f32 = 5.0;

/// The ring slot the left stick points at, given the sector already engaged: past the
/// deadzone (by magnitude — a diagonal counts) the angle falls into one of six 60° sectors
/// centred on the slots (slot `k` sits at `-90° + 60°·k`, 12 o'clock first, clockwise). An
/// engaged sector holds until the stick drops under [`MENU_RELEASE`] (then `None`) or the
/// angle leaves it by [`SECTOR_OVERLAP_DEG`]. SDL sticks are +y = down.
pub fn ring_sector(lx: i16, ly: i16, current: Option<u8>) -> Option<u8> {
    let (x, y) = (f32::from(lx), f32::from(ly));
    let mag = x.hypot(y);
    let floor = if current.is_some() {
        f32::from(MENU_RELEASE)
    } else {
        f32::from(MENU_DEADZONE)
    };
    if mag <= floor {
        return None;
    }
    // Degrees clockwise from 12 o'clock, so slot k's centre is at 60·k.
    let deg = (y.atan2(x).to_degrees() + 90.0).rem_euclid(360.0);
    if let Some(k) = current {
        let off = (deg - 60.0 * f32::from(k) + 180.0).rem_euclid(360.0) - 180.0;
        if off.abs() <= 30.0 + SECTOR_OVERLAP_DEG {
            return Some(k);
        }
    }
    Some((((deg + 30.0) / 60.0) as u8) % 6)
}

impl Default for MenuNav {
    fn default() -> Self {
        Self::new()
    }
}

impl MenuNav {
    pub fn new() -> MenuNav {
        MenuNav {
            snapshot_pending: true,
            was: [false; 6],
            dir: None,
            dir_since: Instant::now(),
            last_repeat: Instant::now(),
            sector: None,
            sector_announced: false,
        }
    }

    /// Arm the snapshot: the next poll adopts held state without firing.
    pub fn reset(&mut self) {
        self.snapshot_pending = true;
        self.dir = None;
        self.sector = None;
        self.sector_announced = false;
    }

    /// Direction from the left stick (dominant axis wins past the deadzone), falling back
    /// to the discrete dpad. SDL sticks are +y = down.
    pub fn resolve_dir(s: &MenuSample) -> Option<MenuDir> {
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

    /// Does the STICK still hold `d` past the release threshold? The hysteresis half of
    /// direction resolution: an engaged direction is only re-resolved once its own axis has
    /// let go, so the dominant-axis rule never flips a direction mid-gesture.
    fn stick_holds(s: &MenuSample, d: MenuDir) -> bool {
        match d {
            MenuDir::Left => s.lx < -(MENU_RELEASE as i16),
            MenuDir::Right => s.lx > MENU_RELEASE as i16,
            MenuDir::Up => s.ly < -(MENU_RELEASE as i16),
            MenuDir::Down => s.ly > MENU_RELEASE as i16,
        }
    }

    /// The direction this sample resolves to GIVEN what is already engaged: a stick-held
    /// direction persists until its own axis releases (see [`MENU_RELEASE`]); otherwise the
    /// stateless [`Self::resolve_dir`] answers.
    fn resolve_engaged(&self, s: &MenuSample) -> Option<MenuDir> {
        match self.dir {
            Some(d) if Self::stick_holds(s, d) => Some(d),
            _ => Self::resolve_dir(s),
        }
    }

    pub fn poll(&mut self, s: &MenuSample, now: Instant, out: &mut Vec<MenuEvent>) {
        let dir = self.resolve_engaged(s);
        let sector = ring_sector(s.lx, s.ly, self.sector);
        if self.snapshot_pending {
            self.snapshot_pending = false;
            self.was = s.buttons;
            self.dir = dir;
            self.dir_since = now;
            self.last_repeat = now;
            self.sector = sector;
            self.sector_announced = false;
            return;
        }
        // The sector goes out BEFORE the buttons and the four-way move of the same sample, so
        // the ring has engaged the stick by the time the move that would step it arrives, and
        // an A in the same sample lands on the slot the stick points at.
        if sector != self.sector {
            self.sector = sector;
            if sector.is_some() || self.sector_announced {
                out.push(MenuEvent::Sector(sector));
            }
            self.sector_announced = sector.is_some();
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
    /// The identity line the console's controllers screen shows under the name —
    /// `VID:PID · gamepad · dpad`. Support's first question when a pad "doesn't work" is
    /// whether the OS enumerated the pad or the adapter in front of it, and the name alone
    /// never answers that. Written by whoever enumerated the device; empty is "nothing more
    /// to say", never an error.
    pub detail: String,
    /// Actually forwarded to the host: a real, non-virtual controller the OS classifies as a
    /// GAMEPAD. A joystick-only node — an adapter that enumerates as a bare joystick, a
    /// DualSense's motion-sensor sibling — is listed and NOT forwarded, which is the single
    /// most common cause of "my pad is connected and nothing happens".
    pub forwarded: bool,
    /// The device reports a rumble motor. `false` is what turns the controllers screen's
    /// rumble test into the sentence explaining why host rumble will be silent on this pad.
    pub rumble: bool,
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
        // Release — silently, the adopted sector included — then press again → now it
        // fires, the ring's sector first so an A in the same sample lands on the slot the
        // stick points at.
        assert!(events(&mut nav, &sample(), t + Duration::from_millis(20)).is_empty());
        assert_eq!(
            events(&mut nav, &held, t + Duration::from_millis(30)),
            vec![
                MenuEvent::Sector(Some(2)),
                MenuEvent::Confirm,
                MenuEvent::Move(MenuDir::Right)
            ]
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
        // The ring's sector rides ahead of every stick move (3 o'clock is the edge between
        // slots 1 and 2, 9 o'clock between 4 and 5).
        assert_eq!(
            events(&mut nav, &right, t + Duration::from_millis(10)),
            vec![MenuEvent::Sector(Some(2)), MenuEvent::Move(MenuDir::Right)]
        );
        assert_eq!(
            events(&mut nav, &left, t + Duration::from_millis(20)),
            vec![MenuEvent::Sector(Some(5)), MenuEvent::Move(MenuDir::Left)]
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
    fn the_stick_angle_picks_a_ring_sector_with_hysteresis() {
        // Neutral, and the deadzone by magnitude: a diagonal just past it counts.
        assert_eq!(ring_sector(0, 0, None), None);
        assert_eq!(
            ring_sector(12000, 12000, None),
            Some(2),
            "16 971 > 16 384, 45° = slot 2"
        );
        assert_eq!(ring_sector(11000, 11000, None), None, "15 556 < 16 384");
        // The six centres, 12 o'clock first, clockwise (SDL +y = down).
        assert_eq!(ring_sector(0, -30000, None), Some(0));
        assert_eq!(ring_sector(26000, -15000, None), Some(1));
        assert_eq!(ring_sector(26000, 15000, None), Some(2));
        assert_eq!(ring_sector(0, 30000, None), Some(3));
        assert_eq!(ring_sector(-26000, 15000, None), Some(4));
        assert_eq!(ring_sector(-26000, -15000, None), Some(5));
        // 28° off slot 0 toward slot 1 is still slot 0 …
        let (x, y) = (14084, -26489); // 30 000 at 28°
        assert_eq!(ring_sector(x, y, None), Some(0));
        // … and 32° past the edge stays with an engaged slot 0, flips fresh to slot 1.
        let (x, y) = (15897, -25441); // 30 000 at 32°
        assert_eq!(
            ring_sector(x, y, Some(0)),
            Some(0),
            "5° of overlap holds it"
        );
        assert_eq!(ring_sector(x, y, None), Some(1));
        assert_eq!(
            ring_sector(x, y, Some(5)),
            Some(1),
            "36° from slot 5's edge lets go"
        );
        // An engaged sector releases only under MENU_RELEASE, not at the deadzone.
        assert_eq!(ring_sector(0, -12000, Some(0)), Some(0));
        assert_eq!(ring_sector(0, -9000, Some(0)), None);
    }

    #[test]
    fn the_sector_goes_out_before_the_move_and_only_on_change() {
        let mut nav = MenuNav::new();
        let t0 = Instant::now();
        assert!(events(&mut nav, &sample(), t0).is_empty(), "snapshot");
        let mut s = sample();
        s.lx = 26000;
        s.ly = 15000;
        assert_eq!(
            events(&mut nav, &s, t0),
            vec![MenuEvent::Sector(Some(2)), MenuEvent::Move(MenuDir::Right)]
        );
        assert!(
            events(&mut nav, &s, t0).is_empty(),
            "held: no repeat yet, no new sector"
        );
        s.ly = -15000;
        assert_eq!(events(&mut nav, &s, t0), vec![MenuEvent::Sector(Some(1))]);
        assert_eq!(
            events(&mut nav, &sample(), t0),
            vec![MenuEvent::Sector(None)],
            "neutral releases the sector"
        );
    }

    #[test]
    fn engaged_direction_holds_until_its_own_axis_releases() {
        // The on-glass "random jumps": a stick engaged DOWN drifts right past the
        // dominant-axis boundary while ly is still well above the release threshold. The
        // stateless resolver would say RIGHT; the engaged direction must hold.
        let mut nav = MenuNav::new();
        let t = Instant::now();
        events(&mut nav, &sample(), t);
        let mut s = sample();
        s.ly = 25_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(10)),
            vec![MenuEvent::Sector(Some(3)), MenuEvent::Move(MenuDir::Down)]
        );
        s.lx = 30_000;
        s.ly = 12_000; // above MENU_RELEASE (9830), below MENU_DEADZONE
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(20)),
            vec![MenuEvent::Sector(Some(2))],
            "the dominant-axis flip fired RIGHT while DOWN was still engaged; the ring's \
             sector, which has no axis to hold, followed the angle to 4 o'clock"
        );
        // Only once ly lets go does the stick re-resolve — to RIGHT, immediately.
        s.ly = 5_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(30)),
            vec![MenuEvent::Move(MenuDir::Right)]
        );
        // And a clean reversal on the same axis still fires at once (no hold across sign).
        s.lx = -30_000;
        assert_eq!(
            events(&mut nav, &s, t + Duration::from_millis(40)),
            vec![MenuEvent::Sector(Some(4)), MenuEvent::Move(MenuDir::Left)]
        );
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
