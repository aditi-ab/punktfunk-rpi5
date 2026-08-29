//! The in-stream quick-action ring on Skia (design/touch-client-overlay.md §2): six
//! translucent discs on a circle under the fingers plus a centre "More" that opens the sheet —
//! the complete catalogue with values. The twist drives the opening frame by frame
//! (`RingInput::Turn`); the `⌃⌥⇧O` chord opens it at the window centre. Glass is an iOS material
//! (D7): here a translucent disc with a hairline. The console has no icon font, so a button
//! carries a SHORT text label, and a shortcut its keycap chord.
//!
//! Contract: nothing is drawn while closed; every pointer event is consumed while open (the
//! scrim); commands go out through [`Ring::take_command`], host actions through the console's
//! own bus ([`Ring::take_cmds`]).

use crate::input::Key;
use crate::model::ConsoleCmd;
use crate::pointer::{Pointer, PointerKind};
use crate::theme::{fill, stroke, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::host_actions::{self, ActionInfo};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{chord_chip, key_vk, OverlayConfig, RingPlatform, SlotId};
use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
use skia_safe::{Canvas, Color4f, Point, RRect, Rect};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Geometry in pixels at 100 % scale (`FrameCtx::scale` multiplies every metric).
const RADIUS: f32 = 120.0;
const SLOT_D: f32 = 56.0;
const CENTRE_D: f32 = 64.0;
const IDLE_CLOSE: Duration = Duration::from_secs(8);
const HINT_LIFE: Duration = Duration::from_secs(2);
/// Button k lags the previous one by this much of the twist, so the ring visibly unwinds.
const SLOT_LAG: f32 = 0.06;
const RES_PRESETS: [(&str, u32, u32); 3] = [
    ("1440p", 2560, 1440),
    ("1080p", 1920, 1080),
    ("720p", 1280, 720),
];
const HZ_PRESETS: [u32; 2] = [120, 60];

/// One button as the ring draws it, and why it is dimmed.
struct Spec {
    id: String,
    label: String,
    short: String,
    enabled: bool,
    reason: String,
    /// Destructive: two presses.
    armed: bool,
    /// A toggle leaves the ring open so the new state is visible (D6).
    toggle: bool,
    state: String,
}

/// A row of the sheet.
#[derive(Clone, PartialEq)]
enum SheetRow {
    Slot(SlotId),
    Resolution,
    Refresh,
}

pub(crate) struct Ring {
    progress: f32,
    committed: bool,
    clockwise: bool,
    centre: (f32, f32),
    /// The drawn opening: the twist's progress until commit, then a spring toward 1.
    shown: f32,
    sheet: bool,
    armed: Option<String>,
    hint: Option<String>,
    hint_at: Instant,
    last_touch: Instant,
    /// The pad's highlight: a slot 0…5, or 6 for the centre (the initial one — `Select+A`
    /// then `A` opens the sheet in two presses). `None` until a pad or key moves it.
    highlight: Option<usize>,
    /// Hit rects as drawn last frame: six slots, then the centre.
    geom: Vec<Rect>,
    sheet_rect: Rect,
    list: MenuList,
    facts: RingFacts,
    cfg: OverlayConfig,
    pending: VecDeque<RingCommand>,
    cmds: Vec<ConsoleCmd>,
}

impl Ring {
    pub(crate) fn new() -> Ring {
        Ring {
            progress: 0.0,
            committed: false,
            clockwise: true,
            centre: (0.0, 0.0),
            shown: 0.0,
            sheet: false,
            armed: None,
            hint: None,
            hint_at: Instant::now(),
            last_touch: Instant::now(),
            highlight: None,
            geom: vec![Rect::new_empty(); 7],
            sheet_rect: Rect::new_empty(),
            list: MenuList::new(),
            facts: RingFacts::default(),
            cfg: OverlayConfig::platform_default(RingPlatform::Desktop),
            pending: VecDeque::new(),
            cmds: Vec::new(),
        }
    }

    pub(crate) fn open(&self) -> bool {
        self.committed || self.progress > 0.0
    }

    /// The session facts, per frame. The ring config is re-parsed only when the blob changes.
    pub(crate) fn set_facts(&mut self, facts: &RingFacts) {
        if facts.overlay_actions != self.facts.overlay_actions {
            self.cfg = OverlayConfig::parse(&facts.overlay_actions, RingPlatform::Desktop);
        }
        self.facts = facts.clone();
    }

    pub(crate) fn input(&mut self, input: RingInput) {
        match input {
            RingInput::Turn {
                progress,
                clockwise,
                x,
                y,
            } => {
                if self.committed {
                    return;
                }
                self.progress = progress;
                self.clockwise = clockwise;
                self.centre = (x, y);
            }
            RingInput::Commit => self.commit(),
            RingInput::Cancel => self.close(),
            RingInput::Toggle { x, y } => {
                if self.committed {
                    self.close();
                } else {
                    self.centre = (x, y);
                    self.commit();
                }
            }
        }
    }

    fn commit(&mut self) {
        self.committed = true;
        self.progress = 1.0;
        self.touch();
    }

    fn close(&mut self) {
        self.committed = false;
        self.progress = 0.0;
        self.shown = 0.0;
        self.sheet = false;
        self.armed = None;
        self.hint = None;
        self.highlight = None;
    }

    fn touch(&mut self) {
        self.last_touch = Instant::now();
    }

    fn say(&mut self, text: String) {
        self.hint = Some(text);
        self.hint_at = Instant::now();
    }

    /// Timeouts: the exit disc's 8 s idle rule (unless the sheet is up), and the 2 s life of
    /// an armed slot or a hint. Once per frame, before the damage key is read.
    pub(crate) fn tick(&mut self) {
        if self.committed && !self.sheet && self.last_touch.elapsed() > IDLE_CLOSE {
            self.close();
        }
        if (self.armed.is_some() || self.hint.is_some()) && self.hint_at.elapsed() > HINT_LIFE {
            self.armed = None;
            self.hint = None;
        }
    }

    pub(crate) fn take_command(&mut self) -> Option<RingCommand> {
        self.pending.pop_front()
    }

    pub(crate) fn take_cmds(&mut self) -> Vec<ConsoleCmd> {
        std::mem::take(&mut self.cmds)
    }

    /// Everything the drawing depends on, folded into one number for the damage gate.
    pub(crate) fn damage(&self) -> u64 {
        if !self.open() {
            return 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        ((self.shown * 200.0) as u32).hash(&mut h);
        self.committed.hash(&mut h);
        self.clockwise.hash(&mut h);
        ((self.centre.0 / 4.0) as i32, (self.centre.1 / 4.0) as i32).hash(&mut h);
        self.sheet.hash(&mut h);
        self.armed.hash(&mut h);
        self.hint.hash(&mut h);
        self.highlight.hash(&mut h);
        self.list.cursor.hash(&mut h);
        self.facts.touch_mode.hash(&mut h);
        self.facts.stats_tier.hash(&mut h);
        self.facts.mic_muted.hash(&mut h);
        self.facts.mode.hash(&mut h);
        h.finish().max(1)
    }

    fn actions(&self) -> Vec<ActionInfo> {
        host_actions::cached(&self.facts.fp_hex)
    }

    fn spec(&self, slot: &SlotId) -> Spec {
        let plain = |id: &str, label: &str, short: &str| Spec {
            id: id.into(),
            label: label.into(),
            short: short.into(),
            enabled: true,
            reason: String::new(),
            armed: false,
            toggle: false,
            state: String::new(),
        };
        let f = &self.facts;
        match slot {
            SlotId::EndStream => Spec {
                armed: true,
                ..plain("end_stream", "End stream", "End")
            },
            SlotId::DisconnectLinger => plain(
                "disconnect_linger",
                "Disconnect, keep the game running",
                "Leave",
            ),
            SlotId::TouchMode => Spec {
                toggle: true,
                state: match f.touch_mode.as_str() {
                    "pointer" => "Direct pointer",
                    "touch" => "Touch passthrough",
                    _ => "Trackpad",
                }
                .into(),
                ..plain(
                    "touch_mode",
                    "Touch mode",
                    match f.touch_mode.as_str() {
                        "pointer" => "Point",
                        "touch" => "Pass",
                        _ => "Track",
                    },
                )
            },
            SlotId::Keyboard => plain("keyboard", "Keyboard", "Keys"),
            SlotId::Stats => Spec {
                toggle: true,
                state: f.stats_tier.clone(),
                ..plain("stats", "Statistics", "Stats")
            },
            SlotId::Mic => Spec {
                enabled: f.mic_available,
                reason: "No microphone is running this session".into(),
                toggle: true,
                state: if f.mic_muted { "Muted" } else { "On" }.into(),
                ..plain(
                    "mic",
                    "Microphone",
                    if f.mic_muted { "Mic ✕" } else { "Mic" },
                )
            },
            SlotId::Pad => Spec {
                enabled: false,
                reason: "The virtual controller is for phones and tablets".into(),
                ..plain("pad", "Virtual controller", "Pad")
            },
            SlotId::SendText => Spec {
                enabled: false,
                reason: "Not on this client yet — use the keyboard".into(),
                ..plain("send_text", "Send text", "Text")
            },
            SlotId::Host(id) => {
                let act = self.actions().into_iter().find(|a| &a.id == id);
                Spec {
                    enabled: act.as_ref().is_some_and(|a| a.available),
                    reason: act
                        .as_ref()
                        .and_then(|a| a.unavailable_reason.clone())
                        .unwrap_or_else(|| "This host does not offer it".into()),
                    armed: act.as_ref().is_none_or(|a| a.danger),
                    ..plain(
                        &format!("host:{id}"),
                        act.as_ref().map_or(id.as_str(), |a| a.label()),
                        "Power",
                    )
                }
            }
            SlotId::Shortcut(id) => {
                let s = self.cfg.shortcut(id);
                let keys: Vec<String> = s.map(|s| s.keys.clone()).unwrap_or_default();
                let chip = chord_chip(&keys);
                Spec {
                    enabled: !keys.is_empty() && keys.iter().all(|k| key_vk(k).is_some()),
                    reason: "A key in this chord is unknown".into(),
                    ..plain(
                        &format!("shortcut:{id}"),
                        s.filter(|s| !s.label.is_empty())
                            .map_or(chip.as_str(), |s| s.label.as_str()),
                        &chip,
                    )
                }
            }
        }
    }

    /// A slot was pressed (ring or sheet): dim ⇒ say why; destructive ⇒ arm, then fire on
    /// the second press; toggles leave the ring open, commands close it.
    fn fire(&mut self, slot: &SlotId) {
        self.touch();
        let s = self.spec(slot);
        if !s.enabled {
            self.armed = None;
            self.say(s.reason);
            return;
        }
        if s.armed && self.armed.as_deref() != Some(&s.id) {
            self.armed = Some(s.id.clone());
            self.say(format!("{}? Press again", s.label));
            return;
        }
        self.armed = None;
        self.hint = None;
        match slot {
            SlotId::EndStream => {
                self.close();
                self.pending.push_back(RingCommand::EndStream);
            }
            SlotId::DisconnectLinger => {
                self.close();
                self.pending.push_back(RingCommand::DisconnectLinger);
            }
            SlotId::TouchMode => self.pending.push_back(RingCommand::CycleTouchMode),
            SlotId::Keyboard => {
                self.close();
                self.pending.push_back(RingCommand::Keyboard);
            }
            SlotId::Stats => self.pending.push_back(RingCommand::CycleStats),
            SlotId::Mic => self.pending.push_back(RingCommand::ToggleMic),
            SlotId::Pad | SlotId::SendText => {}
            SlotId::Host(id) => {
                let f = &self.facts;
                self.cmds.push(ConsoleCmd::HostAction {
                    addr: f.addr.clone(),
                    mgmt: f.mgmt_port,
                    fp_hex: f.fp_hex.clone(),
                    host_name: f.host_name.clone(),
                    action_id: id.clone(),
                    label: s.label.clone(),
                });
                self.close();
            }
            SlotId::Shortcut(id) => {
                if let Some(sc) = self.cfg.shortcut(id) {
                    let keys = sc.keys.clone();
                    self.close();
                    self.pending.push_back(RingCommand::Shortcut(keys));
                }
            }
        }
        if s.toggle {
            // The state shown is the one BEFORE the command lands; the next frame's facts
            // correct the label under the ring.
            self.say(format!("{}: {}", s.label, s.state));
        }
    }

    /// The sheet's rows in the fixed catalogue order (D2).
    fn sheet_rows(&self) -> Vec<SheetRow> {
        let mut rows = vec![
            SheetRow::Slot(SlotId::EndStream),
            SheetRow::Slot(SlotId::DisconnectLinger),
            SheetRow::Resolution,
            SheetRow::Refresh,
            SheetRow::Slot(SlotId::TouchMode),
            SheetRow::Slot(SlotId::Keyboard),
            SheetRow::Slot(SlotId::Stats),
            SheetRow::Slot(SlotId::Mic),
        ];
        rows.extend(
            self.actions()
                .into_iter()
                .filter(ActionInfo::offerable)
                .map(|a| SheetRow::Slot(SlotId::Host(a.id))),
        );
        rows.extend(
            self.cfg
                .shortcuts
                .iter()
                .map(|s| SheetRow::Slot(SlotId::Shortcut(s.id.clone()))),
        );
        rows
    }

    fn res_label(&self) -> String {
        let (w, h, _) = self.facts.mode;
        let (nw, nh, _) = self.facts.native_mode;
        if (w, h) == (nw, nh) {
            format!("Native ({w}×{h})")
        } else {
            RES_PRESETS
                .iter()
                .find(|p| (p.1, p.2) == (w, h))
                .map_or_else(|| format!("{w}×{h}"), |p| p.0.into())
        }
    }

    fn sheet_row_spec(&self, row: &SheetRow) -> RowSpec {
        match row {
            SheetRow::Resolution => RowSpec::field("Resolution", self.res_label(), ""),
            SheetRow::Refresh => RowSpec::field("Refresh", format!("{} Hz", self.facts.mode.2), ""),
            SheetRow::Slot(slot) => {
                let s = self.spec(slot);
                let value = if !s.enabled {
                    s.reason.clone()
                } else if self.armed.as_deref() == Some(&s.id) {
                    "press again".into()
                } else {
                    s.state.clone()
                };
                let mut r = RowSpec::field(s.label, value, "");
                r.enabled = s.enabled;
                r
            }
        }
    }

    /// Left/Right on the resolution rows cycles the presets (native first).
    fn adjust(&mut self, row: &SheetRow, dir: i32) {
        self.touch();
        let (w, h, hz) = self.facts.mode;
        let (nw, nh, nhz) = self.facts.native_mode;
        match row {
            SheetRow::Resolution => {
                let mut options: Vec<(u32, u32)> = vec![(nw, nh)];
                options.extend(RES_PRESETS.iter().map(|p| (p.1, p.2)));
                let i = options.iter().position(|o| *o == (w, h)).unwrap_or(0) as i32;
                let n = options.len() as i32;
                let (rw, rh) = options[((i + dir).rem_euclid(n)) as usize];
                self.pending.push_back(RingCommand::RequestMode {
                    width: rw,
                    height: rh,
                    refresh_hz: hz,
                });
            }
            SheetRow::Refresh => {
                let mut options = vec![nhz];
                options.extend(HZ_PRESETS);
                options.dedup();
                let i = options.iter().position(|o| *o == hz).unwrap_or(0) as i32;
                let n = options.len() as i32;
                let rhz = options[((i + dir).rem_euclid(n)) as usize];
                self.pending.push_back(RingCommand::RequestMode {
                    width: w,
                    height: h,
                    refresh_hz: rhz,
                });
            }
            SheetRow::Slot(_) => {}
        }
    }

    fn sheet_msg(&mut self, msg: ListMsg, rows: &[SheetRow]) {
        let Some(row) = rows.get(self.list.cursor).cloned() else {
            return;
        };
        match msg {
            ListMsg::None => {}
            ListMsg::Adjust(d) => self.adjust(&row, d),
            ListMsg::Activate => match &row {
                SheetRow::Slot(slot) => self.fire(slot),
                SheetRow::Resolution | SheetRow::Refresh => self.adjust(&row, 1),
            },
        }
    }

    /// Pointer input while open; always consumed (the scrim owns the glass).
    pub(crate) fn pointer(&mut self, p: Pointer) -> bool {
        if !self.open() {
            return false;
        }
        match p.kind {
            PointerKind::Back => {
                if self.sheet {
                    self.sheet = false;
                } else {
                    self.close();
                }
            }
            PointerKind::Press if self.sheet => {
                if p.hits(self.sheet_rect) {
                    self.touch();
                    let rows = self.sheet_rows();
                    let (msg, _) = self.list.pointer(p, rows.len());
                    self.sheet_msg(msg, &rows);
                } else {
                    self.sheet = false;
                }
            }
            PointerKind::Press => match p.pick(&self.geom) {
                Some(6) => {
                    self.touch();
                    self.sheet = true;
                    self.list = MenuList::new();
                }
                Some(k) => {
                    if let Some(slot) = self.cfg.ring[k].clone() {
                        self.fire(&slot);
                    }
                }
                None => self.close(),
            },
            PointerKind::Scroll { .. } if self.sheet => {
                let rows = self.sheet_rows();
                let (msg, _) = self.list.pointer(p, rows.len());
                self.sheet_msg(msg, &rows);
            }
            _ => {}
        }
        true
    }

    /// Keyboard while open — the pad's vocabulary on keys: arrows move the highlight, Return
    /// activates, Escape backs out. Always consumed while open.
    pub(crate) fn key(&mut self, key: Key) -> bool {
        if !self.open() {
            return false;
        }
        let ev = match key {
            Key::Escape => MenuEvent::Back,
            Key::Return | Key::Space => MenuEvent::Confirm,
            Key::Up => MenuEvent::Move(MenuDir::Up),
            Key::Down => MenuEvent::Move(MenuDir::Down),
            Key::Left => MenuEvent::Move(MenuDir::Left),
            Key::Right => MenuEvent::Move(MenuDir::Right),
            Key::Y => MenuEvent::Secondary,
            _ => return true,
        };
        self.menu(ev);
        true
    }

    /// The pad while open (design §2.6): Right steps the highlight clockwise, Left
    /// anticlockwise, Up jumps to 12 o'clock, Down to 6, Y returns it to the centre; A fires
    /// the highlight (the centre opens the sheet), B closes (the sheet first). In the sheet,
    /// the list takes the moves and Left/Right adjusts a resolution row.
    pub(crate) fn menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        if !self.open() {
            return None;
        }
        self.touch();
        if self.sheet {
            if ev == MenuEvent::Back {
                self.sheet = false;
                return Some(MenuPulse::Confirm);
            }
            let rows = self.sheet_rows();
            let (msg, pulse) = self.list.menu(ev, rows.len());
            self.sheet_msg(msg, &rows);
            return pulse;
        }
        let h = self.highlight.unwrap_or(6);
        match ev {
            MenuEvent::Move(MenuDir::Right) => {
                self.highlight = Some(if h >= 6 { 0 } else { (h + 1) % 6 });
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Left) => {
                self.highlight = Some(if h >= 6 { 5 } else { (h + 5) % 6 });
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Up) => {
                self.highlight = Some(0);
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Down) => {
                self.highlight = Some(3);
                Some(MenuPulse::Move)
            }
            MenuEvent::Secondary => {
                self.highlight = Some(6);
                Some(MenuPulse::Move)
            }
            MenuEvent::Confirm => {
                if h >= 6 {
                    self.sheet = true;
                    self.list = MenuList::new();
                } else if let Some(slot) = self.cfg.ring[h].clone() {
                    self.fire(&slot);
                } else {
                    return Some(MenuPulse::Boundary);
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                self.close();
                Some(MenuPulse::Confirm)
            }
            _ => None,
        }
    }

    /// Draw the ring (and the sheet) over the stream chrome. `scale` is the chrome scale.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        scale: f32,
        fonts: &Fonts,
        dt: f64,
    ) {
        if !self.open() {
            return;
        }
        // The twist drives the opening; the commit settles toward 1.
        if self.committed {
            let k = 1.0 - (-dt as f32 * 14.0).exp();
            self.shown += (1.0 - self.shown) * k;
            if self.shown > 0.995 {
                self.shown = 1.0;
            }
        } else {
            self.shown = self.progress;
        }
        let shown = self.shown;
        let radius = RADIUS * scale;
        let slot_d = SLOT_D * scale;
        let margin = radius + slot_d / 2.0 + 16.0 * scale;
        let cx = self
            .centre
            .0
            .clamp(margin, (width as f32 - margin).max(margin));
        let cy = self
            .centre
            .1
            .clamp(margin, (height as f32 - margin).max(margin));
        let white = |a: f32| Color4f::new(1.0, 1.0, 1.0, a);

        // The scrim: nothing reaches the stream while the ring is open.
        canvas.draw_rect(
            Rect::from_wh(width as f32, height as f32),
            &fill(Color4f::new(0.0, 0.0, 0.0, 0.18 * shown)),
        );

        let armed = self.armed.clone();
        for k in 0..6 {
            let q = ((shown - k as f32 * SLOT_LAG) / (1.0 - 5.0 * SLOT_LAG)).clamp(0.0, 1.0);
            if q <= 0.0 {
                self.geom[k] = Rect::new_empty();
                continue;
            }
            // Slot k sits at 12, 2, 4… o'clock and travels out along a short spiral that
            // turns the way the hand turns.
            let turn = if self.clockwise { -40.0 } else { 40.0 };
            let deg = -90.0 + 60.0 * k as f32 + (1.0 - q) * turn;
            let (s, c) = deg.to_radians().sin_cos();
            let (x, y) = (cx + radius * q * c, cy + radius * q * s);
            let r = slot_d / 2.0 * (0.6 + 0.4 * q);
            let spec = self.cfg.ring[k].as_ref().map(|slot| self.spec(slot));
            let is_armed = spec
                .as_ref()
                .is_some_and(|s| armed.as_deref() == Some(&s.id));
            draw_disc(
                canvas,
                fonts,
                x,
                y,
                r,
                q,
                scale,
                spec.as_ref(),
                is_armed,
                white,
            );
            if self.highlight == Some(k) {
                canvas.draw_circle(
                    Point::new(x, y),
                    r + 3.0 * scale,
                    &stroke(white(0.8 * q), 2.0 * scale),
                );
            }
            self.geom[k] = Rect::from_xywh(x - r, y - r, 2.0 * r, 2.0 * r);
        }
        // The centre arrives last and opens the sheet.
        let cq = ((shown - 6.0 * SLOT_LAG) / (1.0 - 6.0 * SLOT_LAG)).clamp(0.0, 1.0);
        if cq > 0.0 {
            let r = CENTRE_D * scale / 2.0 * (0.6 + 0.4 * cq);
            let more = Spec {
                id: "more".into(),
                label: "More".into(),
                short: "More".into(),
                enabled: true,
                reason: String::new(),
                armed: false,
                toggle: false,
                state: String::new(),
            };
            draw_disc(
                canvas,
                fonts,
                cx,
                cy,
                r,
                cq,
                scale,
                Some(&more),
                false,
                white,
            );
            if self.highlight == Some(6) {
                canvas.draw_circle(
                    Point::new(cx, cy),
                    r + 3.0 * scale,
                    &stroke(white(0.8 * cq), 2.0 * scale),
                );
            }
            self.geom[6] = Rect::from_xywh(cx - r, cy - r, 2.0 * r, 2.0 * r);
        } else {
            self.geom[6] = Rect::new_empty();
        }
        // The label under the ring: a hint, else the highlighted slot's name (the label a
        // finger would reveal).
        let label = self.hint.clone().or_else(|| match self.highlight {
            Some(6) => Some("More".into()),
            Some(k) => self.cfg.ring[k].as_ref().map(|s| self.spec(s).label),
            None => None,
        });
        if let Some(hint) = &label {
            let size = f64::from(13.0 * scale);
            let tw = fonts.measure(hint, W::Medium, size);
            let (px, py) = (14.0 * scale, 8.0 * scale);
            let (w, h) = (tw + 2.0 * px, size as f32 + 2.0 * py);
            let (x, y) = (cx - w / 2.0, cy + radius + slot_d);
            canvas.draw_rrect(
                RRect::new_rect_xy(Rect::from_xywh(x, y, w, h), h / 2.0, h / 2.0),
                &fill(Color4f::new(0.0, 0.0, 0.0, 0.62)),
            );
            fonts.draw(
                canvas,
                hint,
                f64::from(x + px),
                f64::from(y + py + size as f32 * 0.8),
                W::Medium,
                size,
                white(0.92),
            );
        }
        if self.sheet {
            self.render_sheet(canvas, width, height, scale, fonts, dt);
        } else {
            self.sheet_rect = Rect::new_empty();
        }
    }

    fn render_sheet(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        scale: f32,
        fonts: &Fonts,
        dt: f64,
    ) {
        let rows = self.sheet_rows();
        let specs: Vec<RowSpec> = rows.iter().map(|r| self.sheet_row_spec(r)).collect();
        let k = f64::from(scale);
        let w = (520.0 * scale).min(width as f32 * 0.92);
        let h = ((rows.len() as f32 * 50.0 + 24.0) * scale).min(height as f32 * 0.6);
        let x = (width as f32 - w) / 2.0;
        let y = height as f32 - h - 16.0 * scale;
        let rect = Rect::from_xywh(x, y, w, h);
        canvas.draw_rrect(
            RRect::new_rect_xy(rect, 16.0 * scale, 16.0 * scale),
            &fill(Color4f::new(0.0, 0.0, 0.0, 0.78)),
        );
        canvas.draw_rrect(
            RRect::new_rect_xy(rect, 16.0 * scale, 16.0 * scale),
            &stroke(Color4f::new(1.0, 1.0, 1.0, 0.18), scale),
        );
        let inner = Rect::from_ltrb(
            rect.left + 8.0 * scale,
            rect.top + 12.0 * scale,
            rect.right - 8.0 * scale,
            rect.bottom - 12.0 * scale,
        );
        canvas.save();
        canvas.clip_rect(inner, None, None);
        self.list.render(canvas, inner, &specs, fonts, k, dt, true);
        canvas.restore();
        self.sheet_rect = rect;
    }
}

/// One translucent disc with its short label — the in-stream pill family's surface, round.
#[allow(clippy::too_many_arguments)]
fn draw_disc(
    canvas: &Canvas,
    fonts: &Fonts,
    x: f32,
    y: f32,
    r: f32,
    alpha: f32,
    scale: f32,
    spec: Option<&Spec>,
    armed: bool,
    white: impl Fn(f32) -> Color4f,
) {
    let base = if armed { 0.75 } else { 0.55 };
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &fill(Color4f::new(0.0, 0.0, 0.0, base * alpha)),
    );
    canvas.draw_circle(
        Point::new(x, y),
        r,
        &stroke(white((if armed { 0.6 } else { 0.18 }) * alpha), scale),
    );
    let Some(spec) = spec else { return };
    let color = if armed {
        Color4f::new(1.0, 0.35, 0.35, alpha)
    } else if spec.enabled {
        white(0.95 * alpha)
    } else {
        white(0.35 * alpha)
    };
    let size = f64::from(12.0 * scale * (r / (SLOT_D * scale / 2.0)).clamp(0.6, 1.2));
    let tw = fonts.measure(&spec.short, W::SemiBold, size);
    fonts.draw(
        canvas,
        &spec.short,
        f64::from(x - tw / 2.0),
        f64::from(y + size as f32 * 0.35),
        W::SemiBold,
        size,
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> RingFacts {
        RingFacts {
            mode: (1920, 1080, 60),
            native_mode: (1920, 1080, 60),
            mic_available: true,
            ..RingFacts::default()
        }
    }

    #[test]
    fn a_twist_opens_and_a_lift_short_of_commit_closes() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Turn {
            progress: 0.4,
            clockwise: true,
            x: 300.0,
            y: 300.0,
        });
        assert!(r.open());
        r.input(RingInput::Cancel);
        assert!(!r.open());
        r.input(RingInput::Turn {
            progress: 1.0,
            clockwise: true,
            x: 300.0,
            y: 300.0,
        });
        r.input(RingInput::Commit);
        r.input(RingInput::Cancel); // wound back after a commit
        assert!(!r.open());
    }

    #[test]
    fn end_stream_needs_two_presses_and_a_toggle_keeps_the_ring_open() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.fire(&SlotId::EndStream);
        assert_eq!(r.take_command(), None, "the first press only arms");
        assert_eq!(r.armed.as_deref(), Some("end_stream"));
        r.fire(&SlotId::EndStream);
        assert_eq!(r.take_command(), Some(RingCommand::EndStream));
        assert!(!r.open());

        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        r.fire(&SlotId::Stats);
        assert_eq!(r.take_command(), Some(RingCommand::CycleStats));
        assert!(r.open(), "a toggle leaves the ring open (D6)");
        r.fire(&SlotId::Keyboard);
        assert_eq!(r.take_command(), Some(RingCommand::Keyboard));
        assert!(!r.open(), "a command closes it");
    }

    #[test]
    fn a_dimmed_slot_says_why_and_sends_nothing() {
        let mut r = Ring::new();
        r.set_facts(&RingFacts {
            mic_available: false,
            ..facts()
        });
        r.fire(&SlotId::Mic);
        assert_eq!(r.take_command(), None);
        assert!(r.hint.as_deref().is_some_and(|h| h.contains("microphone")));
    }

    #[test]
    fn the_resolution_row_cycles_presets_from_native() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.adjust(&SheetRow::Resolution, 1);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::RequestMode {
                width: 2560,
                height: 1440,
                refresh_hz: 60
            })
        );
        r.adjust(&SheetRow::Resolution, -1);
        assert_eq!(
            r.take_command(),
            Some(RingCommand::RequestMode {
                width: 1280,
                height: 720,
                refresh_hz: 60
            }),
            "wraps the other way"
        );
    }

    #[test]
    fn the_pad_steps_the_highlight_and_confirms_it() {
        let mut r = Ring::new();
        r.set_facts(&facts());
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert_eq!(r.highlight, None, "centre until moved");
        r.menu(MenuEvent::Move(MenuDir::Right));
        assert_eq!(
            r.highlight,
            Some(0),
            "from the centre, Right lands on 12 o'clock"
        );
        r.menu(MenuEvent::Move(MenuDir::Left));
        assert_eq!(r.highlight, Some(5), "anticlockwise wraps");
        r.menu(MenuEvent::Move(MenuDir::Down));
        assert_eq!(r.highlight, Some(3));
        r.menu(MenuEvent::Secondary);
        assert_eq!(r.highlight, Some(6));
        r.menu(MenuEvent::Confirm);
        assert!(r.sheet, "A on the centre opens the sheet");
        r.menu(MenuEvent::Back);
        assert!(!r.sheet && r.open(), "B leaves the sheet, keeps the ring");
        r.menu(MenuEvent::Move(MenuDir::Up));
        r.menu(MenuEvent::Confirm); // slot 0 of the desktop default = End stream: arms
        assert_eq!(r.armed.as_deref(), Some("end_stream"));
        r.menu(MenuEvent::Back);
        assert!(!r.open(), "B closes the ring");
    }

    #[test]
    fn closed_means_no_damage_and_no_pointer_consumption() {
        let mut r = Ring::new();
        assert_eq!(r.damage(), 0);
        assert!(!r.pointer(Pointer {
            x: 1.0,
            y: 1.0,
            kind: PointerKind::Press
        }));
        r.input(RingInput::Toggle { x: 1.0, y: 1.0 });
        assert_ne!(r.damage(), 0);
        assert!(r.pointer(Pointer {
            x: 1.0,
            y: 1.0,
            kind: PointerKind::Move
        }));
    }
}
