//! Windows virtual gamepad via ViGEmBus — the analogue of the Linux uinput Xbox-360 pad.
//! One virtual Xbox 360 controller per client pad index. GameStream/Moonlight already uses the
//! XInput button/stick/trigger conventions (low 16 button bits, sticks −32768..32767 +Y up,
//! triggers 0..255), so the mapping is ~1:1.
//!
//! Needs the ViGEmBus driver installed (like SudoVDA for the display); absent → gamepad is disabled
//! and the session continues without it. Rumble back-channel: TODO (ViGEm notification API).

use crate::gamestream::gamepad::GamepadEvent;
use std::collections::HashMap;
use std::sync::Arc;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

pub struct GamepadManager {
    client: Option<Arc<Client>>,
    pads: HashMap<u8, Xbox360Wired<Arc<Client>>>,
}

impl GamepadManager {
    pub fn new() -> GamepadManager {
        let client = match Client::connect() {
            Ok(c) => {
                tracing::info!("ViGEmBus connected (virtual Xbox 360 gamepads)");
                Some(Arc::new(c))
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:?}"),
                    "ViGEmBus unavailable — gamepad disabled (install ViGEmBus)"
                );
                None
            }
        };
        GamepadManager {
            client,
            pads: HashMap::new(),
        }
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let GamepadEvent::State(f) = ev else {
            return; // Arrival metadata — the pad is created lazily on the first State
        };
        let target = self.pads.entry(f.index.max(0) as u8).or_insert_with(|| {
            let mut t = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            let _ = t.plugin();
            let _ = t.wait_ready();
            t
        });
        let gp = XGamepad {
            buttons: XButtons {
                raw: (f.buttons & 0xffff) as u16,
            },
            left_trigger: f.left_trigger,
            right_trigger: f.right_trigger,
            thumb_lx: f.ls_x,
            thumb_ly: f.ls_y,
            thumb_rx: f.rs_x,
            thumb_ry: f.rs_y,
        };
        let _ = target.update(&gp);
    }

    pub fn pump_rumble(&mut self, _send: impl FnMut(u16, u16, u16)) {
        // TODO: wire the ViGEm rumble notification back-channel (Xbox360Wired::request_notification).
    }
}
