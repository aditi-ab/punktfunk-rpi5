//! The controller "Add Host" screen — three field rows (name / address / port) plus the
//! Add action. A on a field opens the on-screen keyboard in a bottom tray (on a Steam
//! Deck it opens SDL text input instead — Steam's own keyboard types, ours never
//! draws), B peels one layer: keyboard first, then the screen. Field edits are live;
//! hardware keyboards type straight into the focused field through SDL text input.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::screens::{Ctx, Outbox};
use crate::theme::{Fonts, DIM, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Name,
    Address,
    Port,
}

const FIELDS: [Field; 3] = [Field::Name, Field::Address, Field::Port];

pub(crate) struct AddHostScreen {
    list: MenuList,
    keyboard: Keyboard,
    name: String,
    address: String,
    port: String,
    editing: Option<Field>,
}

impl AddHostScreen {
    pub(crate) fn new() -> AddHostScreen {
        AddHostScreen {
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            name: String::new(),
            address: String::new(),
            port: "9777".into(),
            editing: None,
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn can_add(&self) -> bool {
        !self.address.trim().is_empty() && self.port.parse::<u16>().is_ok_and(|p| p > 0)
    }

    fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Name => &mut self.name,
            Field::Address => &mut self.address,
            Field::Port => &mut self.port,
        }
    }

    fn charset(f: Field) -> Charset {
        match f {
            Field::Name => Charset::Free,
            Field::Address => Charset::Hostname,
            Field::Port => Charset::Digits,
        }
    }

    /// Apply one typed character to the editing field (charset + the port's 5-digit
    /// cap). Returns false when refused — the boundary thud.
    fn type_char(&mut self, ch: char) -> bool {
        let Some(f) = self.editing else { return false };
        if !permits(Self::charset(f), ch) {
            return false;
        }
        if f == Field::Port && self.field_mut(f).chars().count() >= 5 {
            return false;
        }
        self.field_mut(f).push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        let Some(f) = self.editing else { return false };
        self.field_mut(f).pop().is_some()
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
                self.backspace();
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
        if let Some(_field) = self.editing {
            if ctx.deck {
                // Steam's keyboard owns typing; the pad only closes the field.
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
                    if self.backspace() {
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
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, FIELDS.len() + 1);
        match msg {
            ListMsg::Activate => {
                if self.list.cursor < FIELDS.len() {
                    self.editing = Some(FIELDS[self.list.cursor]);
                } else if self.can_add() {
                    fx.cmds.push(ConsoleCmd::SaveHost {
                        name: self.name.trim().to_string(),
                        addr: self.address.trim().to_string(),
                        port: self.port.parse().unwrap_or(9777),
                    });
                    fx.toast = Some(format!("Added {}", self.address.trim()));
                    fx.pop();
                } else {
                    // Not addable yet — jump to what's missing instead of a dead press.
                    self.list.cursor = 1; // the address row
                    self.editing = Some(Field::Address);
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
            "Hosts on this network appear automatically — add one by address for everything else.",
            W::Regular,
            13.0 * k,
            DIM,
            cx,
            f64::from(rect.top) + 2.0 * k,
            f64::from(rect.width()) * 0.72,
        );

        // While the keyboard tray is up (never on Deck) the rows squeeze above it.
        let seat = self.keyboard.seat(self.editing.is_some() && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
            rect.right,
            rect.bottom - tray_h as f32,
        );
        let rows = self.rows();
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.editing.is_none(),
        );
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
        let field_row = |label: &str, value: &str, placeholder: &str, f: Field| {
            let mut row = RowSpec::field(label, value.to_string(), placeholder);
            row.caret = self.editing == Some(f);
            row
        };
        vec![
            field_row(
                "Name",
                &self.name,
                "Optional — e.g. Living Room",
                Field::Name,
            ),
            field_row("Address", &self.address, "IP or hostname", Field::Address),
            field_row("Port", &self.port, "9777", Field::Port),
            RowSpec::action("Add Host", self.can_add()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::Nav;
    use pf_client_core::trust::Settings;

    fn ctx<'a>(
        settings: &'a mut Settings,
        pads: &'a [pf_client_core::gamepad::PadInfo],
        library: &'a crate::library::LibraryShared,
        deck: bool,
    ) -> Ctx<'a> {
        Ctx {
            hosts: &[],
            library,
            settings,
            pads,
            deck,
            device_name: "t",
            t: 0.0,
        }
    }

    #[test]
    fn end_to_end_add_flow() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = ctx(&mut settings, &[], &library, false);
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();

        // A on "Add Host" while the address is empty jumps into the address field.
        s.list.cursor = 3;
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert_eq!(s.editing, Some(Field::Address));
        assert_eq!(s.list.cursor, 1);

        // Hardware keys / Steam keyboard commit text; spaces are refused for addresses.
        s.text_input("deck tower.local");
        assert_eq!(s.address, "decktower.local");
        s.edit_key(sdl3::keyboard::Scancode::Backspace);
        assert_eq!(s.address, "decktower.loca");
        s.text_input("l");
        s.edit_key(sdl3::keyboard::Scancode::Return);
        assert!(s.editing.is_none());

        // Now Add saves, toasts, and pops.
        s.list.cursor = 3;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::SaveHost { addr, port: 9777, .. }) if addr == "decktower.local"
        ));
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    #[test]
    fn port_caps_at_five_digits() {
        let mut s = AddHostScreen::new();
        s.editing = Some(Field::Port);
        s.port.clear();
        s.text_input("123456789");
        assert_eq!(s.port, "12345");
        assert!(!s.type_char('x'), "digits only");
    }

    #[test]
    fn deck_mode_never_uses_the_grid() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = ctx(&mut settings, &[], &library, true);
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();
        s.list.cursor = 1;
        s.menu(MenuEvent::Confirm, &mut c, &mut fx); // open the address field
        assert!(s.editing());
        // Moves don't drive a key grid on Deck; B closes the field.
        assert!(s
            .menu(
                MenuEvent::Move(pf_client_core::gamepad::MenuDir::Right),
                &mut c,
                &mut fx
            )
            .is_none());
        s.menu(MenuEvent::Back, &mut c, &mut fx);
        assert!(!s.editing());
        assert!(fx.nav.is_none(), "B closed the field, not the screen");
    }
}
