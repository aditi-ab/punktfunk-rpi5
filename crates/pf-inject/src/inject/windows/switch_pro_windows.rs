//! Virtual Switch Pro Controller on Windows via the UMDF minidriver: device type 8,
//! `VID_057E&PID_2009`, hardware id `pf_switchpro`. Same sealed channel as
//! [`super::dualsense_windows`]; state mapping is [`super::switch_proto`].
//!
//! The driver answers the `0x80` / `0x01` handshake itself (`pf_driver_proto::switch::reply`)
//! and serves the host's latest `0x30` every 8 ms. Rumble and player lights come back through
//! the output ring. Pin: `dualsense_windows::tests::hwid_matches_inf`.

use super::gamepad_raii::SwDeviceProfile;
use super::pad_shm::ShmPad;
use super::switch_proto::{
    parse_output, player_leds_bits, serialize_report_0x30, SwitchOutput, SwitchState,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_SWITCH_PRO;
use pf_driver_proto::switch as wire;
use punktfunk_core::quic::{HidOutput, RichInput};

/// INF hardware id. A package rename must not change this (`hwid_matches_inf`).
pub(super) const SWITCH_HWID: &str = "pf_switchpro";

/// Drop closes the `pf_swpro_<index>` devnode. `pub` because it is `PadProto::Pad`.
pub struct SwitchWinPad {
    shm: ShmPad,
}

impl SwitchWinPad {
    fn open(index: u8) -> Result<SwitchWinPad> {
        let shm = ShmPad::open(
            index,
            DEVTYPE_SWITCH_PRO,
            &wire::neutral_report(),
            &SwDeviceProfile {
                instance: &format!("pf_swpro_{index}"),
                container_tag: 0x5046_5357, // "PFSW"
                container_index: index,
                hwid: SWITCH_HWID,
                usb_vid_pid: Some("VID_057E&PID_2009"),
                // A wired Pro Controller is a single-interface HID device.
                usb_mi: None,
                bluetooth: false,
                description: "Punktfunk Virtual Pro Controller",
                enumerator: "VID_057E&PID_2009",
            },
        )?;
        Ok(SwitchWinPad { shm })
    }

    /// The driver stamps the timer byte per served report, so the host's is left at zero.
    fn write_state(&mut self, st: &SwitchState) {
        self.shm.publish(&serialize_report_0x30(st, 0));
    }

    /// Rumble from every `0x01` / `0x10`, player lights from subcommand `0x30`, oldest first.
    fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
        fb.resync = self.shm.poll(|bytes, _| match parse_output(bytes) {
            Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                if let (0x30, Some(&arg)) = (id, args.first()) {
                    fb.hidout.push(HidOutput::PlayerLeds {
                        pad,
                        bits: player_leds_bits(arg),
                    });
                }
            }
            Some(SwitchOutput::Rumble(r)) => fb.rumble = Some((r.0, r.1, 0, 0)),
            Some(SwitchOutput::UsbCmd(_)) | None => {}
        });
        fb
    }
}

/// Slot table, unplug, heartbeat, and `HidoutDedup` live in [`UhidManager`].
pub struct SwitchWinProto {
    /// Steam back-grip fold. A Pro Controller has no paddle slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for SwitchWinProto {
    fn default() -> SwitchWinProto {
        SwitchWinProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for SwitchWinProto {
    type Pad = SwitchWinPad;
    type State = SwitchState;
    const LABEL: &'static str = "Switch Pro/Windows";
    const DEVICE: &'static str = "Switch Pro Controller";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<SwitchWinPad> {
        let p = SwitchWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual Switch Pro Controller created (Windows UMDF shm channel)"
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

    fn write_state(&self, pad: &mut SwitchWinPad, st: &SwitchState) {
        pad.write_state(st);
    }

    /// HD rumble on 0xCA, player lights on 0xCD.
    fn service(&self, pad: &mut SwitchWinPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // Every subcommand carries rumble, so a poll that saw rumble is the activity signal.
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

pub type SwitchProWindowsManager = UhidManager<SwitchWinProto>;
