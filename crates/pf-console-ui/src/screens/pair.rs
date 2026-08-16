//! The in-console PIN pairing screen — the couch counterpart of the desktop PairSheet,
//! and the piece that makes a Steam Deck self-sufficient (pairing used to need the
//! Decky plugin or a desktop). The host shows a PIN in its web console; the user types
//! it here (on-screen keyboard, or Steam's keyboard on Deck), the SPAKE2 ceremony runs
//! on the binary's service thread, and success pins the host and pops back to Home.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, PairPhase};
use crate::pointer::Pointer;
use crate::screens::{ConnectIntent, Ctx, Outbox};
use crate::theme::{fg, Fonts, EDGE_INSET, ERROR, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Pin,
    Device,
}

/// The ordered actions a pair screen presents. `RequestAccess` leads only when the host
/// has an advertised fingerprint to pin (a discovered host); a manually-typed host with
/// no advert is PIN-only, exactly like the desktop shells.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    RequestAccess,
    Pin,
    Device,
    Pair,
}

pub(crate) struct PairScreen {
    host_name: String,
    addr: String,
    port: u16,
    /// The host's advertised certificate fingerprint (lowercase hex); empty = a manual
    /// entry with no advert → no request-access path.
    fp_hex: String,
    list: MenuList,
    keyboard: Keyboard,
    pin: String,
    device: String,
    editing: Option<Field>,
    /// The ceremony is in flight (mirrors the model's `PairPhase::Busy` immediately so
    /// a second A can't double-submit before the service thread picks the command up).
    busy: bool,
    error: Option<String>,
}

impl PairScreen {
    pub(crate) fn new(host: &HostRow, device_name: &str) -> PairScreen {
        PairScreen {
            host_name: host.name.clone(),
            addr: host.addr.clone(),
            port: host.port,
            fp_hex: host.fp_hex.clone(),
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            pin: String::new(),
            device: device_name.to_string(),
            editing: None,
            busy: false,
            error: None,
        }
    }

    /// Whether the no-PIN "request access" action is offered (host advertises an identity
    /// to pin). Stable across `busy` so the row list never reshuffles mid-ceremony.
    fn can_request(&self) -> bool {
        !self.fp_hex.is_empty()
    }

    /// The ordered roles for the current host — the single source both `rows` (render) and
    /// `menu` (activate dispatch) index, so a cursor never acts on a stale row.
    fn roles(&self) -> Vec<Role> {
        let mut roles = Vec::with_capacity(4);
        if self.can_request() {
            roles.push(Role::RequestAccess);
        }
        roles.push(Role::Pin);
        roles.push(Role::Device);
        roles.push(Role::Pair);
        roles
    }

    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    /// The shell routes the model's pairing phase here (success pops shell-side).
    pub(crate) fn apply_phase(&mut self, phase: &PairPhase) {
        match phase {
            PairPhase::Busy => self.busy = true,
            PairPhase::Failed(msg) => {
                self.busy = false;
                self.error = Some(msg.clone());
            }
            PairPhase::Idle | PairPhase::Paired { .. } => self.busy = false,
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn can_pair(&self) -> bool {
        !self.pin.trim().is_empty() && !self.busy
    }

    fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Pin => &mut self.pin,
            Field::Device => &mut self.device,
        }
    }

    fn charset(f: Field) -> Charset {
        match f {
            Field::Pin => Charset::Digits,
            Field::Device => Charset::Free,
        }
    }

    fn type_char(&mut self, ch: char) -> bool {
        let Some(f) = self.editing else { return false };
        if !permits(Self::charset(f), ch) {
            return false;
        }
        if f == Field::Pin && self.pin.chars().count() >= 8 {
            return false; // PINs are 4 digits today; leave headroom, refuse novels
        }
        self.field_mut(f).push(ch);
        true
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    pub(crate) fn edit_key(&mut self, sc: sdl3::keyboard::Scancode) -> bool {
        use sdl3::keyboard::Scancode as S;
        if self.editing.is_none() {
            return false;
        }
        match sc {
            S::Backspace => {
                let f = self.editing.unwrap();
                self.field_mut(f).pop();
                true
            }
            S::Return | S::KpEnter | S::Escape => {
                self.editing = None;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if self.editing.is_some() {
            if ctx.deck {
                return match ev {
                    MenuEvent::Back | MenuEvent::Confirm => {
                        self.editing = None;
                        Some(MenuPulse::Confirm)
                    }
                    _ => None,
                };
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            return match msg {
                KeyMsg::Type(c) => {
                    if self.type_char(c) {
                        Some(MenuPulse::Move)
                    } else {
                        Some(MenuPulse::Boundary)
                    }
                }
                KeyMsg::Backspace => {
                    let f = self.editing.unwrap();
                    if self.field_mut(f).pop().is_some() {
                        Some(MenuPulse::Move)
                    } else {
                        Some(MenuPulse::Boundary)
                    }
                }
                KeyMsg::Done => {
                    self.editing = None;
                    Some(MenuPulse::Confirm)
                }
                KeyMsg::None => pulse,
            };
        }

        if ev == MenuEvent::Back {
            // Leaving mid-ceremony is fine — success still pins and toasts globally.
            fx.pop();
            return None;
        }
        let roles = self.roles();
        let (msg, pulse) = self.list.menu(ev, roles.len());
        self.activate(msg, pulse, &roles, ctx, fx)
    }

    /// Mouse/touch. The raised keyboard is modal, exactly as on the add-host screen: it
    /// takes what lands on it, and a press outside closes it rather than reaching through.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing.is_some() && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing = None;
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    if let Some(f) = self.editing {
                        self.field_mut(f).pop();
                    }
                }
                KeyMsg::Done => self.editing = None,
                KeyMsg::None => {}
            }
            return true;
        }
        let roles = self.roles();
        let (msg, pulse) = self.list.pointer(p, roles.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &roles, ctx, fx);
        true
    }

    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        roles: &[Role],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match msg {
            ListMsg::Activate => {
                match roles.get(self.list.cursor) {
                    Some(Role::RequestAccess) if !self.busy => {
                        // The no-PIN path: connect and park until the operator approves this
                        // device on the host. The shell shows the approval takeover; on
                        // success the binary persists the host as paired. Leave the pair
                        // screen so a canceled or finished session returns to Home.
                        fx.connect = Some(ConnectIntent {
                            addr: self.addr.clone(),
                            port: self.port,
                            fp_hex: self.fp_hex.clone(),
                            launch: None,
                            title: self.host_name.clone(),
                            request_access: true,
                            profile: None,
                        });
                        fx.pop();
                    }
                    Some(Role::Pin) => self.editing = Some(Field::Pin),
                    Some(Role::Device) => self.editing = Some(Field::Device),
                    Some(Role::Pair) if self.can_pair() => {
                        self.busy = true;
                        self.error = None;
                        fx.cmds.push(ConsoleCmd::Pair {
                            addr: self.addr.clone(),
                            port: self.port,
                            pin: self.pin.trim().to_string(),
                            device_name: if self.device.trim().is_empty() {
                                ctx.device_name.to_string()
                            } else {
                                self.device.trim().to_string()
                            },
                        });
                    }
                    _ => {
                        // Pair with no PIN yet (or a request while busy) — jump into the
                        // PIN field instead of a dead press.
                        if let Some(i) = roles.iter().position(|r| *r == Role::Pin) {
                            self.list.cursor = i;
                        }
                        self.editing = Some(Field::Pin);
                    }
                }
                pulse
            }
            _ => pulse,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing.is_some() {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Cancel"),
        ]
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        let intro = if self.can_request() {
            "Request access and approve this device on the host, or enter the PIN it shows."
        } else {
            "Enter the PIN from the host's web console (Pairing page) or its log."
        };
        fonts.leading(
            canvas,
            intro,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.72 * k,
        );

        let seat = self.keyboard.seat(self.editing.is_some() && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        // A status band under the rows: the ceremony spinner or the error line.
        let status_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
            rect.right,
            rect.bottom - tray_h as f32 - status_h as f32,
        );
        let rows = self.rows();
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.editing.is_none() && !self.busy,
        );

        let status_y = f64::from(rect.bottom) - tray_h - status_h + 6.0 * k;
        if self.busy {
            crate::theme::spinner(canvas, cx - 70.0 * k, status_y + 8.0 * k, 7.0 * k, ctx.t);
            fonts.centered(
                canvas,
                "Pairing… confirm the PIN on the host",
                W::Regular,
                13.0 * k,
                fg(0.55),
                cx + 10.0 * k,
                status_y,
                f64::from(rect.width()) * 0.6,
            );
        } else if let Some(err) = &self.error {
            fonts.centered(
                canvas,
                err,
                W::Regular,
                13.0 * k,
                ERROR,
                cx,
                status_y,
                f64::from(rect.width()) * 0.8,
            );
        }

        if seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat,
                k,
            );
        }
    }

    fn rows(&self) -> Vec<RowSpec> {
        // The PIN renders spaced out (● would hide typos; a PIN is not a secret worth
        // masking — the host shows it on screen anyway).
        let pin_display: String = self
            .pin
            .chars()
            .flat_map(|c| [c, ' '])
            .collect::<String>()
            .trim_end()
            .to_string();
        let has_request = self.can_request();
        self.roles()
            .into_iter()
            .map(|role| match role {
                Role::RequestAccess => {
                    let mut r = RowSpec::action("Request access — approve on the host", !self.busy);
                    r.header = Some("No PIN needed");
                    r
                }
                Role::Pin => {
                    let mut pin = RowSpec::field("PIN", pin_display.clone(), "From the host");
                    pin.caret = self.editing == Some(Field::Pin);
                    // When a request-access path is offered above, head the PIN group so
                    // the two ways to pair read as alternatives.
                    if has_request {
                        pin.header = Some("Or pair with a PIN");
                    }
                    pin
                }
                Role::Device => {
                    let mut device = RowSpec::field(
                        "Device name",
                        self.device.clone(),
                        "How the host lists this device",
                    );
                    device.caret = self.editing == Some(Field::Device);
                    device
                }
                Role::Pair => {
                    RowSpec::action(if self.busy { "Pairing…" } else { "Pair" }, self.can_pair())
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    fn host() -> HostRow {
        HostRow {
            key: "10.0.0.7:9777".into(),
            name: "Tower".into(),
            addr: "10.0.0.7".into(),
            port: 9777,
            fp_hex: String::new(),
            paired: false,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            last_used: None,
            os: String::new(),
            pin: None,
            bound_profile: None,
        }
    }

    #[test]
    fn pair_submits_with_pin_and_device_fallback() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "living-room-deck",
            t: 0.0,
        };
        let mut s = PairScreen::new(&host(), "living-room-deck");
        s.device.clear(); // the user deleted the prefill
        s.editing = Some(Field::Pin);
        s.text_input("1234");
        s.edit_key(sdl3::keyboard::Scancode::Return);
        s.list.cursor = 2;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::Pair { pin, device_name, .. })
                if pin == "1234" && device_name == "living-room-deck"
        ));
        assert!(s.busy);
        // A second A while busy is a no-op (the action row is disabled).
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
    }

    /// A host with an advertised fingerprint offers Request Access as the first row; A on
    /// it raises a request-access connect intent (pinning the advert) and leaves the screen.
    #[test]
    fn request_access_connects_and_leaves() {
        let mut host = host();
        host.fp_hex = "abcd".into();
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            pads: &pads,
            deck: false,
            device_name: "deck",
            t: 0.0,
        };
        let mut s = PairScreen::new(&host, "deck");
        assert_eq!(s.roles().len(), 4, "Request Access + PIN + Device + Pair");
        s.list.cursor = 0; // the Request Access row leads
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        let intent = fx.connect.expect("request-access raises a connect intent");
        assert!(intent.request_access);
        assert_eq!(intent.fp_hex, "abcd");
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }

    /// A manual host (no advert) is PIN-only — the Request Access row never appears.
    #[test]
    fn no_request_access_without_an_advert() {
        let s = PairScreen::new(&host(), "deck"); // host() has an empty fp_hex
        assert!(!s.can_request());
        assert_eq!(s.roles().len(), 3, "PIN + Device + Pair only");
    }

    #[test]
    fn pin_is_digits_only() {
        let mut s = PairScreen::new(&host(), "d");
        s.editing = Some(Field::Pin);
        s.text_input("12ab34");
        assert_eq!(s.pin, "1234");
    }

    #[test]
    fn failure_lands_in_the_error_line() {
        let mut s = PairScreen::new(&host(), "d");
        s.busy = true;
        s.apply_phase(&PairPhase::Failed("Wrong PIN".into()));
        assert!(!s.busy);
        assert_eq!(s.error.as_deref(), Some("Wrong PIN"));
    }
}
