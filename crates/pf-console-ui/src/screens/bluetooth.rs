//! Bluetooth devices and pairing use the native console widgets.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{KeyMsg, Keyboard, ListMsg, MenuList, RowSpec};
use pf_client_core::bluetooth::{self, Command, Device, PromptKind, Snapshot};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Power,
    Scan,
    Device(String),
    Connect(String),
    Disconnect(String),
    Pair(String),
    Forget(String),
    ConfirmForget(String),
    Cancel,
    Input,
    Accept,
    Reject,
    ClearError,
    Empty,
}

pub(crate) struct BluetoothScreen {
    list: MenuList,
    rows: Vec<Row>,
    selected: Option<String>,
    forget: Option<String>,
    prompt_id: Option<u64>,
    keyboard: Keyboard,
    input: String,
    editing: bool,
    passkey: bool,
}

impl BluetoothScreen {
    pub(crate) fn new() -> Self {
        Self {
            list: MenuList::new(),
            rows: Vec::new(),
            selected: None,
            forget: None,
            prompt_id: None,
            keyboard: Keyboard::new(),
            input: String::new(),
            editing: false,
            passkey: false,
        }
    }

    fn sync(&mut self, state: &Snapshot) {
        let id = state.prompt.as_ref().map(|p| p.id);
        if self.prompt_id != id {
            self.prompt_id = id;
            self.input.clear();
            self.editing = false;
            self.list.cursor = 0;
            self.rows.clear();
        }
        self.passkey = state
            .prompt
            .as_ref()
            .is_some_and(|p| matches!(p.kind, PromptKind::Passkey));
        if self
            .selected
            .as_ref()
            .is_some_and(|path| !state.devices.iter().any(|d| &d.path == path))
        {
            self.selected = None;
            self.forget = None;
        }
        let next = self.roles(state);
        let focused = self.rows.get(self.list.cursor);
        self.list.cursor = focused
            .and_then(|r| next.iter().position(|n| n == r))
            .unwrap_or(0);
        self.rows = next;
    }

    fn roles(&self, state: &Snapshot) -> Vec<Row> {
        if let Some(prompt) = &state.prompt {
            return match prompt.kind {
                PromptKind::Pin | PromptKind::Passkey => vec![Row::Input, Row::Accept, Row::Reject],
                PromptKind::Display { .. } => vec![Row::Reject],
                _ => vec![Row::Reject, Row::Accept],
            };
        }
        if let Some(path) = &self.forget {
            return vec![Row::Cancel, Row::ConfirmForget(path.clone())];
        }
        if let Some(device) = self
            .selected
            .as_ref()
            .and_then(|path| state.devices.iter().find(|d| &d.path == path))
        {
            let mut rows = vec![if device.connected {
                Row::Disconnect(device.path.clone())
            } else if device.paired {
                Row::Connect(device.path.clone())
            } else {
                Row::Pair(device.path.clone())
            }];
            if device.paired {
                rows.push(Row::Forget(device.path.clone()));
            }
            if state.busy && !device.paired {
                rows.push(Row::Reject);
            }
            rows.push(Row::Cancel);
            if state.error.is_some() {
                rows.push(Row::ClearError);
            }
            return rows;
        }
        let mut rows = vec![Row::Power, Row::Scan];
        rows.extend(state.devices.iter().map(|d| Row::Device(d.path.clone())));
        if state.devices.is_empty() {
            rows.push(Row::Empty);
        }
        if state.error.is_some() {
            rows.push(Row::ClearError);
        }
        rows
    }

    fn back(&mut self, state: &Snapshot, fx: &mut Outbox) {
        if state.prompt.is_some() {
            bluetooth::submit(reject_prompt(state));
        } else if self.forget.take().is_none() && self.selected.take().is_none() {
            bluetooth::submit(Command::CancelPairing);
            bluetooth::submit(Command::Scan(false));
            fx.pop();
        }
        self.rows.clear();
        self.list.cursor = 0;
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let state = bluetooth::snapshot();
        self.sync(&state);
        if self.editing {
            if ctx.deck {
                if matches!(ev, MenuEvent::Back | MenuEvent::Confirm) {
                    self.editing = false;
                }
                return None;
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            return self.key_msg(msg).or(pulse);
        }
        if ev == MenuEvent::Back {
            self.back(&state, fx);
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.rows.len());
        self.activate(msg, pulse, &state)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        let state = bluetooth::snapshot();
        self.sync(&state);
        if self.editing && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing = false;
                }
                return true;
            }
            let (msg, _) = self.keyboard.pointer(p);
            self.key_msg(msg);
            return true;
        }
        let (msg, pulse) = self.list.pointer(p, self.rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &state);
        true
    }

    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        state: &Snapshot,
    ) -> Option<MenuPulse> {
        if matches!(msg, ListMsg::Adjust(_)) {
            return Some(MenuPulse::Boundary);
        }
        if !matches!(msg, ListMsg::Activate) {
            return pulse;
        }
        let Some(row) = self.rows.get(self.list.cursor).cloned() else {
            return pulse;
        };
        if !self.spec(&row, state).enabled {
            return Some(MenuPulse::Boundary);
        }
        let command = match row {
            Row::Power => Some(Command::Power(!state.powered)),
            Row::Scan => Some(Command::Scan(!state.discovering)),
            Row::Device(path) => {
                self.selected = Some(path);
                None
            }
            Row::Connect(path) => Some(Command::Connect(path)),
            Row::Disconnect(path) => Some(Command::Disconnect(path)),
            Row::Pair(path) => Some(Command::Pair(path)),
            Row::Forget(path) => {
                self.forget = Some(path);
                None
            }
            Row::ConfirmForget(path) => {
                self.forget = None;
                self.selected = None;
                Some(Command::Forget(path))
            }
            Row::Cancel => {
                if self.forget.take().is_none() {
                    self.selected = None;
                }
                None
            }
            Row::Input => {
                self.editing = true;
                return pulse;
            }
            Row::Accept => state.prompt.as_ref().map(|p| Command::Reply {
                id: p.id,
                value: Some(if matches!(p.kind, PromptKind::Pin | PromptKind::Passkey) {
                    self.input.clone()
                } else {
                    "yes".into()
                }),
            }),
            Row::Reject => Some(reject_prompt(state)),
            Row::ClearError => Some(Command::ClearError),
            Row::Empty => None,
        };
        if let Some(command) = command {
            bluetooth::submit(command);
        }
        self.rows.clear();
        self.list.cursor = 0;
        pulse
    }

    fn type_char(&mut self, ch: char) -> bool {
        if !self.editing
            || !ch.is_ascii()
            || ch.is_control()
            || (self.passkey && !ch.is_ascii_digit())
            || self.input.len() + ch.len_utf8() > if self.passkey { 6 } else { 16 }
        {
            return false;
        }
        self.input.push(ch);
        true
    }

    fn key_msg(&mut self, msg: KeyMsg) -> Option<MenuPulse> {
        match msg {
            KeyMsg::Type(ch) => Some(if self.type_char(ch) {
                MenuPulse::Move
            } else {
                MenuPulse::Boundary
            }),
            KeyMsg::Backspace => {
                self.input.pop();
                Some(MenuPulse::Move)
            }
            KeyMsg::Done => {
                self.editing = false;
                Some(MenuPulse::Confirm)
            }
            KeyMsg::None => None,
        }
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }
    pub(crate) fn editing(&self) -> bool {
        self.editing
    }
    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
        use crate::input::Key;
        if !self.editing {
            return false;
        }
        match key {
            Key::Backspace => {
                self.input.pop();
                true
            }
            Key::Return | Key::Escape => {
                self.editing = false;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(
                HintKey::Confirm,
                if self.editing { "Type" } else { "Choose" },
            ),
            Hint::new(HintKey::Back, if self.editing { "Done" } else { "Back" }),
        ]
    }

    fn spec(&self, row: &Row, state: &Snapshot) -> RowSpec {
        let ready = state.available && !state.busy;
        match row {
            Row::Power => RowSpec {
                value: Some(if state.powered { "On" } else { "Off" }.into()),
                ..RowSpec::action("Bluetooth", ready)
            },
            Row::Scan => RowSpec::action(
                if state.discovering {
                    "Stop searching"
                } else {
                    "Search for devices"
                },
                ready && state.powered,
            ),
            Row::Device(path) => state.devices.iter().find(|d| &d.path == path).map_or_else(
                || RowSpec::action("Device unavailable", false),
                |d| RowSpec {
                    value: Some(device_status(d)),
                    ..RowSpec::action(&d.name, true)
                },
            ),
            Row::Connect(_) => RowSpec::action("Connect", ready && state.powered),
            Row::Disconnect(_) => RowSpec::action("Disconnect", ready),
            Row::Pair(_) => RowSpec::action("Pair and connect", ready && state.powered),
            Row::Forget(_) => RowSpec::action("Forget device…", ready),
            Row::ConfirmForget(_) => RowSpec::action("Forget device", ready),
            Row::Cancel => RowSpec::action("Back", true),
            Row::Input => RowSpec {
                caret: self.editing,
                ..RowSpec::field(
                    if self.passkey { "Passkey" } else { "PIN" },
                    self.input.clone(),
                    "Enter the code from your device",
                )
            },
            Row::Accept => RowSpec::action(
                "Accept",
                state.prompt.as_ref().is_some_and(|p| {
                    !matches!(p.kind, PromptKind::Pin | PromptKind::Passkey)
                        || !self.input.is_empty()
                }),
            ),
            Row::Reject => RowSpec::action("Cancel pairing", true),
            Row::ClearError => RowSpec::action("Dismiss error", true),
            Row::Empty => RowSpec::action(
                if state.discovering {
                    "Searching… put your device in pairing mode"
                } else {
                    "No devices found"
                },
                false,
            ),
        }
    }

    fn detail(&self, state: &Snapshot) -> String {
        if let Some(p) = &state.prompt {
            return match &p.kind {
                PromptKind::Confirm { passkey } => {
                    format!("Does code {passkey:06} match on {}?", p.device)
                }
                PromptKind::Pin | PromptKind::Passkey => {
                    format!("Enter the pairing code for {}.", p.device)
                }
                PromptKind::Display { code } => format!(
                    "Enter {code} on {}, then press Enter.", p.device
                ),
                PromptKind::Authorize => format!("Allow {} to connect?", p.device),
            };
        }
        if self.forget.is_some() {
            return "Forget this device? You will need to pair it again.".into();
        }
        if let Some(error) = &state.error {
            return error.clone();
        }
        if !state.available {
            return "Bluetooth is unavailable. Check your adapter.".into();
        }
        if state.busy {
            return "Working… confirm any pairing request on your device.".into();
        }
        if let Some(d) = self
            .selected
            .as_ref()
            .and_then(|path| state.devices.iter().find(|d| &d.path == path))
        {
            return format!("{} · {} · {}", d.name, d.address, device_status(d));
        }
        "Put your device in pairing mode, then search for it here.".into()
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
        let state = bluetooth::snapshot();
        self.sync(&state);
        let seat = self.keyboard.seat(self.editing && !ctx.deck, dt);
        let tray_h = (Keyboard::tray_height() + 12.0) * k * seat;
        let specs: Vec<_> = self.rows.iter().map(|r| self.spec(r, &state)).collect();
        self.list.render(
            canvas,
            Rect::from_ltrb(
                rect.left,
                rect.top + (48.0 * k) as f32,
                rect.right,
                rect.bottom - tray_h as f32,
            ),
            &specs,
            fonts,
            k,
            dt,
            !self.editing,
        );
        fonts.centered(
            canvas,
            &self.detail(&state),
            W::Regular,
            13.0 * k,
            fg(0.65),
            f64::from(rect.center_x()),
            f64::from(rect.top) + 6.0 * k,
            f64::from(rect.width()) * 0.85,
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
}

impl Drop for BluetoothScreen {
    fn drop(&mut self) {
        bluetooth::submit(Command::CancelPairing);
        bluetooth::submit(Command::Scan(false));
    }
}

fn device_status(device: &Device) -> String {
    let status = if device.connected {
        "Connected"
    } else if device.paired {
        "Paired"
    } else {
        "Not paired"
    };
    match device.battery {
        Some(percent) => format!("{status} · {percent}%"),
        None => status.into(),
    }
}

fn reject_prompt(state: &Snapshot) -> Command {
    match &state.prompt {
        Some(prompt) if !matches!(prompt.kind, PromptKind::Display { .. }) => Command::Reply {
            id: prompt.id,
            value: None,
        },
        _ => Command::CancelPairing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Snapshot {
        Snapshot {
            available: true,
            powered: true,
            discovering: false,
            busy: false,
            devices: vec![],
            prompt: None,
            error: None,
        }
    }

    fn device(path: &str) -> Device {
        Device {
            path: path.into(),
            name: path.into(),
            address: "00:11:22:33:44:55".into(),
            paired: true,
            connected: false,
            trusted: true,
            battery: Some(75),
        }
    }

    #[test]
    fn discovery_keeps_focus_on_the_same_device() {
        let mut screen = BluetoothScreen::new();
        let mut state = state();
        state.devices = vec![device("a"), device("b")];
        screen.sync(&state);
        screen.list.cursor = 3;
        state.devices.insert(0, device("c"));
        screen.sync(&state);
        assert_eq!(screen.rows[screen.list.cursor], Row::Device("b".into()));
        state.devices.retain(|d| d.path != "b");
        screen.sync(&state);
        assert_eq!(screen.rows[screen.list.cursor], Row::Power);
    }

    #[test]
    fn forgetting_requires_a_separate_confirmation() {
        let mut screen = BluetoothScreen::new();
        let mut state = state();
        state.devices = vec![device("a")];
        screen.selected = Some("a".into());
        screen.sync(&state);
        screen.list.cursor = 1;
        screen.activate(ListMsg::Activate, None, &state);
        assert_eq!(screen.forget.as_deref(), Some("a"));
        screen.sync(&state);
        assert_eq!(screen.rows[screen.list.cursor], Row::Cancel);
        assert_eq!(screen.rows[1], Row::ConfirmForget("a".into()));
    }

    #[test]
    fn passkey_input_only_accepts_six_digits() {
        let mut screen = BluetoothScreen::new();
        screen.editing = true;
        screen.passkey = true;
        screen.text_input("a12b345678");
        assert_eq!(screen.input, "123456");
        assert!(screen.edit_key(crate::input::Key::Backspace));
        assert_eq!(screen.input, "12345");
    }

    #[test]
    fn new_prompt_clears_previous_code_and_defaults_to_reject() {
        let mut screen = BluetoothScreen::new();
        screen.input = "secret".into();
        screen.editing = true;
        let mut state = state();
        state.prompt = Some(pf_client_core::bluetooth::Prompt {
            id: 9,
            device: "Pad".into(),
            kind: PromptKind::Confirm { passkey: 1234 },
        });
        screen.sync(&state);
        assert!(screen.input.is_empty());
        assert!(!screen.editing);
        assert_eq!(screen.rows[screen.list.cursor], Row::Reject);
    }

    #[test]
    fn display_code_cancellation_reaches_the_pairing_operation() {
        let mut state = state();
        state.prompt = Some(pf_client_core::bluetooth::Prompt {
            id: 10,
            device: "Keyboard".into(),
            kind: PromptKind::Display {
                code: "123456".into(),
            },
        });
        assert!(matches!(reject_prompt(&state), Command::CancelPairing));
    }

    #[test]
    fn pin_rejects_characters_bluez_cannot_accept() {
        let mut screen = BluetoothScreen::new();
        screen.editing = true;
        screen.text_input("abé12\n");
        assert_eq!(screen.input, "ab12");
    }
}
