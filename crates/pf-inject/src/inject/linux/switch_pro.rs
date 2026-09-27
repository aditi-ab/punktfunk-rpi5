//! Virtual Nintendo Switch Pro Controller on `/dev/uhid`, bound by `hid-nintendo`
//! (≥ 5.16). State mapping lives in [`super::switch_proto`], the replies in
//! `pf_driver_proto::switch`; this file is the UHID plumbing that answers the driver's
//! probe from [`UhidManager`]'s `service` pass.
//!
//! `hid-nintendo` is not DualSense's three GET_REPORTs: it runs a blocking probe
//! (`0x80` USB commands, then subcommands for device info, SPI calibration, IMU,
//! vibration, input mode, player lights). Each step must see `0x81`/`0x21` within
//! 1–2 s or the probe aborts and no input devices appear.
//!
//! After bind, LED/rumble writes stall up to 250 ms unless `0x30` reports are
//! flowing — the manager's 8 ms silence heartbeat is that stream. Suspend/resume
//! re-runs the whole init; nothing probe-specific is latched here.

use super::switch_proto::{
    parse_output, player_leds_bits, serialize_report_0x30, SwitchOutput, SwitchState,
    SWITCH_PRODUCT, SWITCH_VENDOR,
};
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::switch as wire;
use punktfunk_core::quic::{HidOutput, RichInput};

/// Virtual Pro Controller on `/dev/uhid`. Drop unbinds `hid-nintendo`.
pub struct SwitchProPad {
    dev: UhidDevice,
    index: u8,
    /// Rolling report timer (byte 1 of every input report).
    timer: u8,
    /// Last written state. Subcommand replies embed this header so probe reports stay coherent.
    state: SwitchState,
}

impl SwitchProPad {
    /// `index` is name/uniq and the virtual MAC. BUS_USB selects hid-nintendo's USB probe.
    pub fn open(index: u8) -> Result<SwitchProPad> {
        let dev = UhidDevice::open(&Create2 {
            name: &format!("Punktfunk Switch Pro Controller {index}"),
            phys: &format!("punktfunk/switchpro/{index}"),
            uniq: &format!("punktfunk-swpro-{index}"),
            rdesc: &wire::RDESC,
            vendor: SWITCH_VENDOR,
            product: SWITCH_PRODUCT,
            version: 0x0200, // bcdDevice 2.00
        })?;
        Ok(SwitchProPad {
            dev,
            index,
            timer: 0,
            state: SwitchState::neutral(),
        })
    }

    pub fn write_state(&mut self, st: &SwitchState) -> Result<()> {
        self.state = *st;
        self.timer = self.timer.wrapping_add(1);
        let r = serialize_report_0x30(st, self.timer);
        self.dev.write_input(&r)
    }

    /// Drain UHID events. Each probe step blocks `hid-nintendo` until answered; call often.
    /// A handshake command or subcommand is answered as the Windows driver does. Every `0x80`
    /// is acked, including no-timeout (0x04): that skips the driver's 2 × 100 ms wait.
    pub fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        let (timer, state, index) = (&mut self.timer, &self.state, self.index);
        self.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => {
                match parse_output(data) {
                    Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                        // No trigger motors on this protocol — see `PadFeedback::rumble`.
                        fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                        // Player lights are the subcommand payload; the reply still acks it.
                        if let (0x30, Some(&arg)) = (id, args.first()) {
                            fb.hidout.push(HidOutput::PlayerLeds {
                                pad,
                                bits: player_leds_bits(arg),
                            });
                        }
                    }
                    Some(SwitchOutput::Rumble(r)) => fb.rumble = Some((r.0, r.1, 0, 0)),
                    Some(SwitchOutput::UsbCmd(_)) | None => {}
                }
                *timer = timer.wrapping_add(1);
                let report = serialize_report_0x30(state, *timer);
                if let Some(reply) = wire::reply(&report, data, index) {
                    let _ = dev.write_input(&reply);
                }
            }
            // hid-nintendo never GET_REPORTs; EIO so a stray request cannot block.
            UhidEvent::GetReport { id, .. } => {
                let _ = dev.reply_get_report(id, None);
            }
            UhidEvent::SetReport(_) => {}
        });
        fb
    }
}

/// Switch Pro [`PadProto`]: UHID open, [`SwitchState`] mappers, probe `service`.
/// Slot table / unplug / heartbeat / dedup live in [`UhidManager`].
pub struct SwitchProProto {
    /// Steam back-grip fold. A Pro Controller has no paddle slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchProProto {
    fn default() -> SwitchProProto {
        SwitchProProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchProProto {
    type Pad = SwitchProPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<SwitchProPad> {
        let p = SwitchProPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (UHID hid-nintendo)"
        );
        Ok(p)
    }

    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        SwitchState::merge_frame(prev, f, buttons)
    }

    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut SwitchProPad, st: &SwitchState) {
        let _ = pad.write_state(st);
    }

    /// Probe conversation + feedback: HD-rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchProPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // hid-nintendo embeds rumble in every command, so a poll that saw rumble is
        // the activity signal. Physical HD-rumble decays faster than the idle window;
        // abandoned-rumble force-off covers a writer that latches a level.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

/// Session Switch Pro pads (`PUNKTFUNK_GAMEPAD=switchpro`, or a Nintendo-family per-pad kind).
pub type SwitchProManager = UhidManager<SwitchProProto>;
