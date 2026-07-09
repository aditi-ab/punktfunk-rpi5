//! The in-console PIN pairing screen — the couch counterpart of the desktop PairSheet,
//! and the piece that makes a Steam Deck self-sufficient (pairing used to need the
//! Decky plugin or a desktop). The host shows a PIN in its web console; the user types
//! it here (on-screen keyboard, or Steam's keyboard on Deck), the SPAKE2 ceremony runs
//! on the binary's service thread, and success pins the host and pops back to Home.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, PairPhase};
use crate::screens::{Ctx, Outbox};
use crate::theme::{Fonts, DIM, ERROR, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Pin,
    Device,
}

pub(crate) struct PairScreen {
    host_name: String,
    addr: String,
    port: u16,
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
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            pin: String::new(),
            device: device_name.to_string(),
            editing: None,
            busy: false,
            error: None,
        }
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
        let (msg, pulse) = self.list.menu(ev, 3);
        match msg {
            ListMsg::Activate => {
                match self.list.cursor {
                    0 => self.editing = Some(Field::Pin),
                    1 => self.editing = Some(Field::Device),
                    _ if self.can_pair() => {
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
                        // No PIN yet — jump into the PIN field instead of a dead press.
                        self.list.cursor = 0;
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
        fonts.centered(
            canvas,
            "Enter the PIN from the host's web console (Pairing page) or its log.",
            W::Regular,
            13.0 * k,
            DIM,
            cx,
            f64::from(rect.top) + 2.0 * k,
            f64::from(rect.width()) * 0.72,
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
                DIM,
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
        let mut pin = RowSpec::field("PIN", pin_display, "From the host");
        pin.caret = self.editing == Some(Field::Pin);
        let mut device = RowSpec::field(
            "Device name",
            self.device.clone(),
            "How the host lists this device",
        );
        device.caret = self.editing == Some(Field::Device);
        vec![
            pin,
            device,
            RowSpec::action(if self.busy { "Pairing…" } else { "Pair" }, self.can_pair()),
        ]
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
