//! Virtual Sony DualSense **Edge** on Windows via the UMDF minidriver — the Edge sibling of
//! [`super::dualsense_windows`]. Same transport ([`DsWinPad`]: a per-session `SwDeviceCreate`
//! devnode + the sealed shared-memory channel), same report codec ([`super::dualsense_proto`]);
//! the host stamps `device_type = 2` so the one UMDF driver serves the Edge descriptor /
//! `VID_054C&PID_0DF2` attributes, and the wire back-grip bits map onto the Edge's native
//! `buttons[2]` slots instead of the fold/drop policy — a client's Deck grips / Elite paddles
//! reach games as real buttons. Feedback is identical to the plain DualSense (rumble arrives with
//! the vibration-v2 flag, which [`parse_ds_output`](super::dualsense_proto::parse_ds_output)
//! already handles).

use super::dualsense_proto::{edge_paddle_bits, DsState, DS_TOUCH_H, DS_TOUCH_W};
use super::dualsense_windows::{DsWinPad, WinDsIdentity};
use crate::inject::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::quic::RichInput;

/// The Windows-Edge half of the shared stateful manager (see [`PadProto`]): the shared
/// [`DsWinPad`] transport under the Edge identity, with the Edge paddle mapping in `merge_frame`.
/// No remap config — every wire paddle has a native slot.
#[derive(Default)]
pub struct DsEdgeWinProto;

impl PadProto for DsEdgeWinProto {
    type Pad = DsWinPad;
    type State = DsState;
    const LABEL: &'static str = "DualSense Edge/Windows";
    const DEVICE: &'static str = "DualSense Edge";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<DsWinPad> {
        let p = DsWinPad::open(idx, &WinDsIdentity::dualsense_edge())?;
        tracing::info!(
            index = idx,
            "virtual DualSense Edge created (Windows UMDF shm channel)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving the rich-plane fields — like the
    /// plain DualSense, EXCEPT the wire paddles land on the Edge's own `buttons[2]` bits
    /// (rebuilt from every button frame, so no extra persistence).
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let mut s = DsState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.buttons[2] |= edge_paddle_bits(f.buttons);
        s.touch = prev.touch;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.touch_click = prev.touch_click;
        s
    }

    /// The shared DualSense-family mapping (dualsense_proto::DsState::apply_rich): Steam dual pads
    /// split the one touchpad left/right, pad clicks ride touch_click.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS_TOUCH_W, DS_TOUCH_H);
    }

    fn write_state(&self, pad: &mut DsWinPad, st: &DsState) {
        pad.write_state(st);
    }

    /// Poll the section for a game's feedback: motor rumble on the universal 0xCA plane, the rich
    /// lightbar/player-LED/trigger events on the 0xCD plane.
    fn service(&self, pad: &mut DsWinPad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            rumble: fb.rumble,
            hidout: fb.hidout,
            game_drove: Some(fb.fresh),
        }
    }
}

/// All virtual DualSense Edge pads of a session — the Windows analogue of
/// [`DualSenseEdgeManager`](crate::inject::dualsense::DualSenseEdgeManager), with the same method
/// surface (via the shared [`UhidManager`]) as the other Windows pad managers.
pub type DualSenseEdgeWindowsManager = UhidManager<DsEdgeWinProto>;
