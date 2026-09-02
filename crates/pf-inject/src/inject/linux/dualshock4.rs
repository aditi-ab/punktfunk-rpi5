//! Virtual DualShock 4 via `/dev/uhid`.
//!
//! `hid-playstation` binds VID `054C` / PID `09CC` (Linux ≥ 6.2). Input is report
//! `0x01`; OUTPUT `0x05` is rumble (0xCA) and lightbar (`HidOutput::Led`, 0xCD).
//! There are no adaptive triggers, player LEDs, or mute.
//!
//! Codec, feature blobs, and GET_REPORT answers live in [`super::dualshock4_proto`]
//! (shared with the Windows UMDF backend). This file is the UHID transport, the
//! report descriptor, and the handshake. Pin with the tests here and
//! `crates/pf-inject/tests/motion_contract.rs`.

use super::dualsense_proto::DsState;
use super::dualshock4_proto::{
    ds4_pairing_reply, parse_ds4_output, serialize_state, Ds4Feedback, DS4_FEATURE_CALIBRATION,
    DS4_FEATURE_FIRMWARE, DS4_INPUT_REPORT_LEN, DS4_PRODUCT, DS4_TOUCH_H, DS4_TOUCH_W, DS4_VENDOR,
};
use crate::sensor_clock::SensorClock;
use crate::uhid_abi::{
    put_cstr, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2, UHID_DESTROY, UHID_EVENT_SIZE,
    UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2, UHID_OUTPUT, UHID_PATH, UHID_SET_REPORT,
    UHID_SET_REPORT_REPLY,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

/// HID core answers GET_REPORT only for ids declared here: `0x01` in, `0x05` out,
/// `0x02`/`0x12`/`0xa3` feature. Bind is VID/PID, not this blob.
#[rustfmt::skip]
const DS4_RDESC: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01, 0x09, 0x30, 0x09, 0x31,
    0x09, 0x32, 0x09, 0x35, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95,
    0x04, 0x81, 0x02, 0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x35, 0x00, 0x46,
    0x3B, 0x01, 0x65, 0x14, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42, 0x65, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0E, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x0E, 0x81, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x20, 0x75, 0x06, 0x95,
    0x01, 0x15, 0x00, 0x25, 0x7F, 0x81, 0x02, 0x05, 0x01, 0x09, 0x33, 0x09,
    0x34, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x02, 0x81, 0x02,
    0x06, 0x00, 0xFF, 0x09, 0x21, 0x95, 0x36, 0x81, 0x02, 0x85, 0x05, 0x09,
    0x22, 0x95, 0x1F, 0x91, 0x02, 0x85, 0x04, 0x09, 0x23, 0x95, 0x24, 0xB1,
    0x02, 0x85, 0x02, 0x09, 0x24, 0x95, 0x24, 0xB1, 0x02, 0x85, 0x08, 0x09,
    0x25, 0x95, 0x03, 0xB1, 0x02, 0x85, 0x10, 0x09, 0x26, 0x95, 0x04, 0xB1,
    0x02, 0x85, 0x11, 0x09, 0x27, 0x95, 0x02, 0xB1, 0x02, 0x85, 0x12, 0x06,
    0x02, 0xFF, 0x09, 0x21, 0x95, 0x0F, 0xB1, 0x02, 0x85, 0x13, 0x09, 0x22,
    0x95, 0x16, 0xB1, 0x02, 0x85, 0x14, 0x06, 0x05, 0xFF, 0x09, 0x20, 0x95,
    0x10, 0xB1, 0x02, 0x85, 0x15, 0x09, 0x21, 0x95, 0x2C, 0xB1, 0x02, 0x06,
    0x80, 0xFF, 0x85, 0x80, 0x09, 0x20, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x81,
    0x09, 0x21, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x82, 0x09, 0x22, 0x95, 0x05,
    0xB1, 0x02, 0x85, 0x83, 0x09, 0x23, 0x95, 0x01, 0xB1, 0x02, 0x85, 0x84,
    0x09, 0x24, 0x95, 0x04, 0xB1, 0x02, 0x85, 0x85, 0x09, 0x25, 0x95, 0x06,
    0xB1, 0x02, 0x85, 0x86, 0x09, 0x26, 0x95, 0x06, 0xB1, 0x02, 0x85, 0x87,
    0x09, 0x27, 0x95, 0x23, 0xB1, 0x02, 0x85, 0x88, 0x09, 0x28, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0x89, 0x09, 0x29, 0x95, 0x02, 0xB1, 0x02, 0x85, 0x90,
    0x09, 0x30, 0x95, 0x05, 0xB1, 0x02, 0x85, 0x91, 0x09, 0x31, 0x95, 0x03,
    0xB1, 0x02, 0x85, 0x92, 0x09, 0x32, 0x95, 0x03, 0xB1, 0x02, 0x85, 0x93,
    0x09, 0x33, 0x95, 0x0C, 0xB1, 0x02, 0x85, 0x94, 0x09, 0x34, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xA0, 0x09, 0x40, 0x95, 0x06, 0xB1, 0x02, 0x85, 0xA1,
    0x09, 0x41, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xA2, 0x09, 0x42, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xA3, 0x09, 0x43, 0x95, 0x30, 0xB1, 0x02, 0x85, 0xA4,
    0x09, 0x44, 0x95, 0x0D, 0xB1, 0x02, 0x85, 0xF0, 0x09, 0x47, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xF1, 0x09, 0x48, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xF2,
    0x09, 0x49, 0x95, 0x0F, 0xB1, 0x02, 0x85, 0xA7, 0x09, 0x4A, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xA8, 0x09, 0x4B, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xA9,
    0x09, 0x4C, 0x95, 0x08, 0xB1, 0x02, 0x85, 0xAA, 0x09, 0x4E, 0x95, 0x01,
    0xB1, 0x02, 0x85, 0xAB, 0x09, 0x4F, 0x95, 0x39, 0xB1, 0x02, 0x85, 0xAC,
    0x09, 0x50, 0x95, 0x39, 0xB1, 0x02, 0x85, 0xAD, 0x09, 0x51, 0x95, 0x0B,
    0xB1, 0x02, 0x85, 0xAE, 0x09, 0x52, 0x95, 0x01, 0xB1, 0x02, 0x85, 0xAF,
    0x09, 0x53, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xB0, 0x09, 0x54, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xE0, 0x09, 0x57, 0x95, 0x02, 0xB1, 0x02, 0x85, 0xB3,
    0x09, 0x55, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xB4, 0x09, 0x55, 0x95, 0x3F,
    0xB1, 0x02, 0x85, 0xB5, 0x09, 0x56, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xD0,
    0x09, 0x58, 0x95, 0x3F, 0xB1, 0x02, 0x85, 0xD4, 0x09, 0x59, 0x95, 0x3F,
    0xB1, 0x02, 0xC0,
];

/// Drop sends `UHID_DESTROY` and unbinds `hid-playstation`.
pub struct DualShock4Pad {
    fd: File,
    counter: u8,
    clock: SensorClock,
}

impl DualShock4Pad {
    /// `index` is only the name/uniq suffix, not a HID slot.
    pub fn open(index: u8) -> Result<DualShock4Pad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut ds = DualShock4Pad {
            fd,
            counter: 0,
            clock: SensorClock::dualshock4(),
        };
        ds.send_create2(index).context("UHID_CREATE2 DualShock4")?;
        Ok(ds)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // uhid_create2_req at 4: name[128] phys[64] uniq[64] rd_size bus vid pid version country rd_data.
        put_cstr(&mut ev, 4, 128, &format!("Punktfunk DualShock 4 {index}"));
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/dualshock4/{index}"));

        // uniq is cosmetic; hid-playstation keys uniqueness off the pairing-report MAC.
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-ds4-{index}"));
        ev[260..262].copy_from_slice(&(DS4_RDESC.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes());
        ev[264..268].copy_from_slice(&(DS4_VENDOR as u32).to_ne_bytes());
        ev[268..272].copy_from_slice(&(DS4_PRODUCT as u32).to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0100u32.to_ne_bytes());
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes());
        ev[280..280 + DS4_RDESC.len()].copy_from_slice(DS4_RDESC);
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    pub fn write_state(&mut self, st: &DsState) -> Result<()> {
        self.counter = self.counter.wrapping_add(1);
        let ts = self.clock.ds4_ticks(Instant::now());
        let mut r = [0u8; DS4_INPUT_REPORT_LEN];
        serialize_state(&mut r, st, self.counter, ts);

        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        // uhid_input2_req: size u16 at 4, data at 6.
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes());
        ev[6..6 + r.len()].copy_from_slice(&r);
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Pairing GET_REPORT (`0x12`) must be answered during `hid-playstation` bind or no
    /// input nodes appear. Call right after [`open`].
    pub fn service(&mut self, pad: u8) -> Ds4Feedback {
        let mut fb = Ds4Feedback::default();
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => {
                    // uhid_output_req: data[4096] at [4..4100], size u16 at [4100..4102].
                    let size = u16::from_ne_bytes([ev[4100], ev[4101]]) as usize;
                    let end = 4 + size.min(HID_MAX_DESCRIPTOR_SIZE);
                    parse_ds4_output(&ev[4..end], &mut fb);
                }
                UHID_GET_REPORT => {
                    // uhid_get_report_req: id u32 [4..8], rnum u8 [8].
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let pairing = ds4_pairing_reply(pad);
                    let data: &[u8] = match ev[8] {
                        0x12 => &pairing,
                        0x02 => DS4_FEATURE_CALIBRATION,
                        0xA3 => DS4_FEATURE_FIRMWARE,
                        _ => &[],
                    };
                    let _ = self.reply_get_report(id, data);
                }
                UHID_SET_REPORT => {
                    // Ack SET_REPORT (err=0): kernel waits 5 s otherwise. DS4 feedback is OUTPUT, not SET_REPORT.
                    let id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_set_report(id);
                }
                _ => {}
            }
        }
        fb
    }

    fn reply_get_report(&mut self, id: u32, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12], data [12..].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        let err: u16 = if data.is_empty() { 5 } else { 0 }; // EIO if unknown report
        ev[8..10].copy_from_slice(&err.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[12..12 + data.len()].copy_from_slice(data);
        self.fd
            .write_all(&ev)
            .context("write UHID_GET_REPORT_REPLY")?;
        Ok(())
    }

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        // uhid_set_report_reply_req: id u32 [4..8], err u16 [8..10].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&0u16.to_ne_bytes());
        self.fd
            .write_all(&ev)
            .context("write UHID_SET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for DualShock4Pad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// Slot table, heartbeat, and `HidoutDedup` live in [`UhidManager`]. The kernel restamps
/// the lightbar on every OUTPUT (including rumble-only); `Led` is compared to the last
/// forwarded value and re-armed on create/unplug.
pub struct Ds4LinuxProto {
    /// Steam back-grip fold. DS4 has no paddle HID slot; `PUNKTFUNK_STEAM_REMAP=paddles=…`, default drop.
    remap: crate::steam_remap::RemapConfig,
}

impl Default for Ds4LinuxProto {
    fn default() -> Ds4LinuxProto {
        Ds4LinuxProto {
            remap: crate::steam_remap::RemapConfig::from_env(),
        }
    }
}

impl PadProto for Ds4LinuxProto {
    type Pad = DualShock4Pad;
    type State = DsState;
    const LABEL: &'static str = "DualShock 4";
    const DEVICE: &'static str = "DualShock 4";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<DualShock4Pad> {
        let p = DualShock4Pad::open(idx)?;
        tracing::info!(
            index = idx,
            "virtual DualShock 4 created (UHID hid-playstation)"
        );
        Ok(p)
    }

    fn neutral(&self) -> DsState {
        DsState::neutral()
    }

    /// Keep prev touch/motion/click — they arrive on the rich plane, not this button frame.
    fn merge_frame(&self, prev: &DsState, f: &punktfunk_core::input::GamepadFrame) -> DsState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = DsState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.touch = prev.touch;
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s.touch_click = prev.touch_click;
        s
    }

    /// Steam dual pads split the one touchpad left/right; clicks ride `touch_click`.
    fn apply_rich(&self, st: &mut DsState, rich: RichInput) {
        st.apply_rich(rich, DS4_TOUCH_W, DS4_TOUCH_H);
    }

    fn neutralize_gyro(&self, st: &mut DsState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut DsState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut DualShock4Pad, st: &DsState) {
        let _ = pad.write_state(st);
    }

    /// Rumble on 0xCA, lightbar as 0xCD `Led`. No player LEDs or adaptive triggers.
    fn service(&self, pad: &mut DualShock4Pad, idx: u8) -> PadFeedback {
        let fb = pad.service(idx);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: fb.rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: fb
                .led
                .map(|(r, g, b)| HidOutput::Led { pad: idx, r, g, b })
                .into_iter()
                .collect(),
            // Arms abandoned-rumble force-off. `parse_ds4_output` sets rumble only when flag0 bit0 is on.
            rumble_drove: Some(fb.rumble.is_some()),
            resync: false,
        }
    }
}

/// `PUNKTFUNK_GAMEPAD=ps4`. [`UhidManager`] heartbeats report `0x01` through input silence;
/// `hid-playstation`/SDL treat a multi-second gap as unplug.
pub type DualShock4Manager = UhidManager<Ds4LinuxProto>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dualshock4_proto::DS4_FEATURE_PAIRING;

    // Codec tests live in `dualshock4_proto`. This module pins UHID-side feature-report shapes.

    #[test]
    fn feature_report_shapes() {
        assert_eq!(DS4_FEATURE_PAIRING.len(), 16);
        assert_eq!(DS4_FEATURE_PAIRING[0], 0x12);
        assert_eq!(DS4_FEATURE_CALIBRATION.len(), 37);
        assert_eq!(DS4_FEATURE_CALIBRATION[0], 0x02);
        assert_eq!(DS4_FEATURE_FIRMWARE.len(), 49);
        assert_eq!(DS4_FEATURE_FIRMWARE[0], 0xA3);
    }

    /// Pairing MAC low octet is per-pad. SDL/Steam dedup controllers by that serial.
    #[test]
    fn pairing_reply_mac_is_per_pad() {
        assert_eq!(ds4_pairing_reply(0).as_slice(), DS4_FEATURE_PAIRING);
        let (a, b) = (ds4_pairing_reply(1), ds4_pairing_reply(2));
        assert_eq!(a[0], 0x12);
        assert_eq!(a[1], DS4_FEATURE_PAIRING[1].wrapping_add(1));
        assert_eq!(b[1], DS4_FEATURE_PAIRING[1].wrapping_add(2));
        assert_eq!(a[2..], b[2..]);
    }
}
