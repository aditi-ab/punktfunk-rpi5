//! Windows virtual gamepad via ViGEmBus — the analogue of the Linux uinput Xbox-360 pad.
//! One virtual Xbox 360 controller per client pad index. GameStream/Moonlight already uses the
//! XInput button/stick/trigger conventions (low 16 button bits, sticks −32768..32767 +Y up,
//! triggers 0..255), so the mapping is ~1:1.
//!
//! Needs the ViGEmBus driver installed (like SudoVDA for the display); absent → gamepad is disabled
//! and the session continues without it. Rumble flows back the *other* way: a game on the host writes
//! force-feedback to the virtual pad, ViGEm's notification API delivers it on a background thread,
//! and [`GamepadManager::pump_rumble`] relays level changes to the client (the universal 0xCA plane),
//! mirroring the Linux `EV_FF` read path.

use crate::gamestream::gamepad::GamepadEvent;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

/// A plugged virtual pad plus its rumble back-channel. The notification thread stores the latest
/// motor levels into `rumble` (packed `large << 8 | small`, both 0..255); [`GamepadManager::pump_rumble`]
/// reads it and emits level changes. Dropping `target` aborts the outstanding notification request,
/// so the thread's `poll` returns an error and it exits on its own — we detach it (per ViGEm's docs,
/// dropping the `JoinHandle` does not stop the thread, but the target-drop abort does).
struct PadEntry {
    target: Xbox360Wired<Arc<Client>>,
    rumble: Arc<AtomicU32>,
    last_emitted: u32,
    _notif_thread: Option<JoinHandle<()>>,
}

pub struct GamepadManager {
    client: Option<Arc<Client>>,
    pads: HashMap<u8, PadEntry>,
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

    /// Lazily plug pad `index` on its first event, arming the rumble notification thread. Returns
    /// `None` if ViGEmBus is unavailable or the pad failed to plug.
    fn ensure_pad(&mut self, index: u8) -> Option<&mut PadEntry> {
        if !self.pads.contains_key(&index) {
            let client = self.client.clone()?;
            let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
            if let Err(e) = target.plugin() {
                tracing::warn!(error = format!("{e:?}"), "ViGEm pad plugin failed");
                return None;
            }
            let _ = target.wait_ready();
            // Arm the force-feedback back-channel: a background thread writes each notification's
            // motor levels into the shared atomic; the input thread drains changes via pump_rumble.
            let rumble = Arc::new(AtomicU32::new(0));
            let notif_thread = match target.request_notification() {
                Ok(req) => {
                    let sink = rumble.clone();
                    Some(req.spawn_thread(move |_req, n| {
                        sink.store(
                            ((n.large_motor as u32) << 8) | n.small_motor as u32,
                            Ordering::Relaxed,
                        );
                    }))
                }
                Err(e) => {
                    tracing::warn!(
                        error = format!("{e:?}"),
                        "ViGEm rumble notification unavailable — pad runs without force feedback"
                    );
                    None
                }
            };
            self.pads.insert(
                index,
                PadEntry {
                    target,
                    rumble,
                    last_emitted: 0,
                    _notif_thread: notif_thread,
                },
            );
        }
        self.pads.get_mut(&index)
    }

    pub fn handle(&mut self, ev: &GamepadEvent) {
        let GamepadEvent::State(f) = ev else {
            return; // Arrival metadata — the pad is created lazily on the first State
        };
        let Some(entry) = self.ensure_pad(f.index.max(0) as u8) else {
            return;
        };
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
        let _ = entry.target.update(&gp);
    }

    /// Relay any changed rumble level to the client. The notification thread keeps `rumble` current;
    /// we emit only on change (the input thread re-sends the steady state every 500 ms to heal drops).
    /// ViGEm motors are 0..255; the wire carries 0..65535, so scale by 257 (255 → 65535). `large`
    /// (low-frequency) maps to the universal datagram's `low`, `small` (high-frequency) to `high`.
    pub fn pump_rumble(&mut self, mut send: impl FnMut(u16, u16, u16)) {
        for (idx, entry) in self.pads.iter_mut() {
            let packed = entry.rumble.load(Ordering::Relaxed);
            if packed != entry.last_emitted {
                entry.last_emitted = packed;
                let large = ((packed >> 8) & 0xff) as u16;
                let small = (packed & 0xff) as u16;
                send(*idx as u16, large * 257, small * 257);
            }
        }
    }
}
