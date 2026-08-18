//! "Connected controllers" — everything the client can see about the attached pads, and the
//! handful of actions only the platform can perform on them. Reached from the settings
//! list's Controller tab.
//!
//! This exists for exactly one support case: a pad "doesn't work". Adapters and BT-to-USB
//! dongles often enumerate with a different identity than the physical pad, or not as a
//! gamepad at all, and only devices the OS classifies as a gamepad are forwarded — so the
//! screen's real content is the identity line under each name, not the name.
//!
//! It was a Compose screen the Android host drew OVER the console (the D7 platform-screen
//! mechanism) until 2026-08. Drawing it here instead is what lets the console keep its own
//! input on the page; what genuinely cannot move — the USB and Bluetooth grant dialogs, a
//! rumble pulse on a real `InputDevice` — stays with the host and is asked for by
//! [`ConsoleCmd::PadAction`].
//
// ponytail: the Compose screen's live input test (button grid + axis bars, entered with A,
// left by holding B) did NOT move here — the console only receives the aggregated
// `MenuSample` (6 buttons, lx/ly, dpad), nowhere near a per-device axis/trigger readout,
// and the hold-to-exit gesture has no home in the edge-triggered MenuEvent grammar. The
// touch Controllers screen keeps the full test, so the feature exists on-device; add it
// here by widening the pad-sample bridge with a per-device payload while the test is open.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::platform::Platform;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

/// Work on a controller that only the HOST can do — every one of these needs a permission
/// dialog or a real device handle, neither of which exists on this side of the bridge.
/// Ordered as they are listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PadAction {
    /// Pulse the focused pad's motor (the "is rumble even wired up" test).
    Rumble,
    /// `BLUETOOTH_CONNECT`, without which a BLE-paired Steam Controller 2 is invisible —
    /// not "detected and idle", absent, which is why the row is offered rather than hidden
    /// behind a detection that cannot run.
    Sc2Bluetooth,
    /// USB access for a wired or Puck-dongle Steam Controller 2.
    Sc2Usb,
    /// USB access for a wired Sony pad (DualSense, Edge, DualShock 4).
    DsUsb,
    /// The DualSense pad-audio self test: can this phone drive the pad's audio endpoint at
    /// all. Deliberately reachable with no stream running — it exists to rule the pad out
    /// when a session misbehaves, and gating it behind a session would make it depend on
    /// the very thing under suspicion.
    DsHaptics,
}

impl PadAction {
    /// The stable id the host matches on (crosses JNI inside [`ConsoleCmd::PadAction`]).
    pub(crate) fn id(self) -> &'static str {
        match self {
            PadAction::Rumble => "rumble",
            PadAction::Sc2Bluetooth => "sc2_bluetooth",
            PadAction::Sc2Usb => "sc2_usb",
            PadAction::DsUsb => "ds_usb",
            PadAction::DsHaptics => "ds_haptics",
        }
    }
}

/// The passthrough rows, in list order. Platform-gated as one union exactly like the
/// settings row table (`settings::row_on`): the desktop captures nothing over raw USB and
/// asks for no grants, so it has no such rows — never a control that changes nothing.
const PASSTHROUGH: [(PadAction, &str, &str); 4] = [
    (
        PadAction::Sc2Bluetooth,
        "Steam Controller 2 over Bluetooth",
        "Grant",
    ),
    (PadAction::Sc2Usb, "Steam Controller 2 over USB", "Grant"),
    (PadAction::DsUsb, "DualSense / DualShock over USB", "Grant"),
    (PadAction::DsHaptics, "DualSense haptics self-test", "Test"),
];

/// One line in the list. Pads first, then whatever the platform can be asked to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    /// An index into [`Ctx::pads`].
    Pad(usize),
    /// No pads at all — an inert row, so the list is never empty and the cursor always has
    /// something to sit on while the passthrough rows below it stay reachable.
    NoPads,
    /// An index into [`PASSTHROUGH`].
    Passthrough(usize),
}

fn rows_for(ctx: &Ctx) -> Vec<Row> {
    let mut rows: Vec<Row> = if ctx.pads.is_empty() {
        vec![Row::NoPads]
    } else {
        (0..ctx.pads.len()).map(Row::Pad).collect()
    };
    if ctx.platform == Platform::Android {
        rows.extend((0..PASSTHROUGH.len()).map(Row::Passthrough));
    }
    rows
}

pub(crate) struct ControllersScreen {
    list: MenuList,
}

impl ControllersScreen {
    pub(crate) fn new() -> ControllersScreen {
        ControllersScreen {
            list: MenuList::new(),
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let rows = rows_for(ctx);
        let (msg, pulse) = self.list.menu(ev, rows.len());
        self.activate(msg, pulse, &rows, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let rows = rows_for(ctx);
        let (msg, pulse) = self.list.pointer(p, rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &rows, ctx, fx);
        true
    }

    /// One list message against the focused row — shared by the pad path and the pointer's,
    /// so a click and an A press can never drift apart.
    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        rows: &[Row],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(&focused) = rows.get(self.list.cursor) else {
            return pulse;
        };
        // Nothing here steps: every row is a button or a statement.
        if matches!(msg, ListMsg::Adjust(_)) {
            return Some(MenuPulse::Boundary);
        }
        if !matches!(msg, ListMsg::Activate) {
            return pulse;
        }
        let (action, pad_key) = match focused {
            Row::NoPads => return Some(MenuPulse::Boundary),
            Row::Pad(i) => {
                // A pad with no motor has nothing to test; say so with the thud rather than
                // sending a command the host would silently drop.
                if !ctx.pads[i].rumble {
                    return Some(MenuPulse::Boundary);
                }
                (PadAction::Rumble, ctx.pads[i].key.clone())
            }
            // The grants are about a device the pad list cannot name (an SC2 in lizard mode
            // is no input device at all), so they carry no key.
            Row::Passthrough(i) => (PASSTHROUGH[i].0, String::new()),
        };
        fx.cmds.push(ConsoleCmd::PadAction {
            action: action.id().to_string(),
            pad_key,
        });
        pulse
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let rows = rows_for(ctx);
        let confirm = match rows.get(self.list.cursor) {
            Some(Row::Pad(i)) if ctx.pads[*i].rumble => Some("Test rumble"),
            Some(Row::Passthrough(i)) => Some(match PASSTHROUGH[*i].0 {
                PadAction::DsHaptics => "Test haptics",
                _ => "Grant access",
            }),
            _ => None,
        };
        let mut hints = Vec::new();
        if let Some(label) = confirm {
            hints.push(Hint::new(HintKey::Confirm, label));
        }
        hints.push(Hint::new(HintKey::Back, "Done"));
        hints
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
        // The focused row's explainer takes a reserved band under the list — the settings
        // screen's shape, and here it is the whole point: the identity of the device is the
        // support answer, and it is far too long to live on the row.
        let detail_h = 34.0 * k;
        let rows = rows_for(ctx);
        let specs: Vec<RowSpec> = rows.iter().map(|r| spec(*r, ctx)).collect();
        self.list.render(
            canvas,
            Rect::from_ltrb(
                rect.left,
                rect.top,
                rect.right,
                rect.bottom - detail_h as f32,
            ),
            &specs,
            fonts,
            k,
            dt,
            true,
        );
        let detail = rows
            .get(self.list.cursor)
            .map_or_else(String::new, |r| detail(*r, ctx));
        fonts.centered(
            canvas,
            &detail,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

fn spec(row: Row, ctx: &Ctx) -> RowSpec {
    match row {
        Row::NoPads => RowSpec {
            header: Some("Gamepads"),
            ..RowSpec::action("No controller detected", false)
        },
        Row::Pad(i) => {
            let pad = &ctx.pads[i];
            RowSpec {
                header: (i == 0).then_some("Gamepads"),
                label: pad.name.clone(),
                value: Some(
                    if pad.rumble {
                        "Test rumble"
                    } else {
                        "No rumble"
                    }
                    .into(),
                ),
                value_dim: !pad.rumble,
                caret: false,
                adjustable: false,
                enabled: pad.rumble,
            }
        }
        Row::Passthrough(i) => {
            let (_, label, verb) = PASSTHROUGH[i];
            RowSpec {
                header: (i == 0).then_some("Passthrough"),
                label: label.into(),
                value: Some(verb.into()),
                value_dim: false,
                caret: false,
                adjustable: false,
                enabled: true,
            }
        }
    }
}

/// The band under the list: what this row is, in one sentence.
fn detail(row: Row, ctx: &Ctx) -> String {
    match row {
        Row::NoPads => "Punktfunk only forwards devices the system classifies as a gamepad or \
                        joystick — a pad behind an adapter or hub may enumerate with the \
                        adapter's identity, or not at all."
            .into(),
        Row::Pad(i) => pad_detail(&ctx.pads[i]),
        Row::Passthrough(i) => match PASSTHROUGH[i].0 {
            PadAction::Sc2Bluetooth => {
                "A Steam Controller 2 paired over Bluetooth cannot be detected at all without \
                 Bluetooth access. Wired and Puck-dongle controllers need no permission."
                    .into()
            }
            PadAction::Sc2Usb => {
                "A wired or Puck-dongle Steam Controller 2 needs USB access to be captured; \
                 until then it stays in its built-in keyboard/mouse mode."
                    .into()
            }
            PadAction::DsUsb => {
                "A wired DualSense or DualShock 4 needs USB access to be captured — with it, \
                 streams drive rumble, adaptive triggers, lightbar and gyro directly."
                    .into()
            }
            PadAction::DsHaptics => {
                "Play a short tone through a wired DualSense's audio endpoint, to tell a pad \
                 that cannot do haptics from a stream that is not sending them."
                    .into()
            }
            // Not offered as a passthrough row — the pads carry it.
            PadAction::Rumble => String::new(),
        },
    }
}

/// A pad's identity line: what the OS enumerated, whether it is forwarded, what the host
/// will build for it, and its charge if it reports one.
fn pad_detail(pad: &PadInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !pad.detail.is_empty() {
        parts.push(pad.detail.clone());
    }
    if !pad.forwarded {
        parts.push("not forwarded — not classified as a gamepad".into());
    }
    let kind = pad.kind_label();
    parts.push(format!(
        "streams as {}",
        if kind.is_empty() { "Xbox 360" } else { kind }
    ));
    if let Some(b) = pad.battery {
        parts.push(if b.charging {
            format!("battery {} %, charging", b.percent)
        } else {
            format!("battery {} %", b.percent)
        });
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;
    use punktfunk_core::config::GamepadPref;

    fn pad(name: &str, rumble: bool) -> PadInfo {
        PadInfo {
            name: name.into(),
            key: format!("054c:0ce6:{name}"),
            pref: GamepadPref::DualSense,
            steam_virtual: false,
            battery: None,
            detail: "054C:0CE6 · gamepad".into(),
            forwarded: true,
            rumble,
        }
    }

    fn drive(
        screen: &mut ControllersScreen,
        platform: Platform,
        pads: &[PadInfo],
        ev: MenuEvent,
    ) -> (Outbox, Option<MenuPulse>) {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform,
            pads,
            deck: false,
            device_name: "t",
            t: 0.0,
        };
        let mut fx = Outbox::default();
        let pulse = screen.menu(ev, &mut ctx, &mut fx);
        (fx, pulse)
    }

    #[test]
    fn a_on_a_pad_asks_the_host_for_a_rumble_pulse() {
        let pads = [pad("DualSense", true)];
        let mut s = ControllersScreen::new();
        let (fx, _) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "rumble".into(),
                pad_key: "054c:0ce6:DualSense".into(),
            }]
        );
    }

    #[test]
    fn a_pad_with_no_motor_thuds_instead_of_sending_a_pulse() {
        let pads = [pad("Adapter", false)];
        let mut s = ControllersScreen::new();
        let (fx, pulse) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }

    #[test]
    fn the_grant_rows_are_androids_alone_and_carry_no_pad_key() {
        // Desktop: pads and nothing else — it asks for no grants and captures nothing raw.
        let pads = [pad("DualSense", true)];
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let ctx = |platform, settings: &mut Settings| Ctx {
            hosts: &[],
            library: &library,
            settings,
            store: crate::store::file_store(),
            platform,
            pads: &pads,
            deck: false,
            device_name: "t",
            t: 0.0,
        };
        assert_eq!(rows_for(&ctx(Platform::Desktop, &mut settings)).len(), 1);
        assert_eq!(
            rows_for(&ctx(Platform::Android, &mut settings)).len(),
            1 + PASSTHROUGH.len()
        );

        // Down onto the first grant row, then A.
        let mut s = ControllersScreen::new();
        drive(
            &mut s,
            Platform::Android,
            &pads,
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Down),
        );
        let (fx, _) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "sc2_bluetooth".into(),
                pad_key: String::new(),
            }]
        );
    }

    #[test]
    fn with_no_pads_the_list_still_has_the_grants_under_an_inert_row() {
        let mut s = ControllersScreen::new();
        let (fx, pulse) = drive(&mut s, Platform::Android, &[], MenuEvent::Confirm);
        assert!(fx.cmds.is_empty(), "the empty-state row does nothing");
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }
}
