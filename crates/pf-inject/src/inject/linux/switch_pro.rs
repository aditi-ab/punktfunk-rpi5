//! Virtual Nintendo Switch Pro Controller via UHID — bound by the kernel's `hid-nintendo`
//! (≥ 5.16), so a Nintendo-family client pad gets correct glyphs + positional layout, live
//! gyro/accel, and HD-rumble feedback, instead of folding to the Xbox 360 pad (mirrored A/B
//! + X/Y, no motion).
//!
//! Unlike `hid-playstation` (whose init is three GET_REPORTs), `hid-nintendo` runs a real
//! PROBE CONVERSATION against the device: the `0x80`-family USB commands, then ~a dozen
//! subcommands (device info, SPI-flash calibration reads, IMU/vibration enable, input mode,
//! player lights) — each a blocking send that must see its reply (input report `0x81`/`0x21`)
//! within 1–2 s or probe aborts and NO input devices appear. The whole codec + the canned
//! replies live in [`super::switch_proto`]; this module is the `/dev/uhid` plumbing that
//! answers them from the [`UhidManager`]'s frequent `service` pass (the same cadence that
//! already completes the DualSense handshake).
//!
//! Post-probe, the driver stalls every LED/rumble write for up to 250 ms unless input reports
//! are flowing — the shared manager's 8 ms silence heartbeat provides exactly that steady
//! `0x30` stream. On host suspend/resume the driver re-runs the whole init; the service pass
//! answers it identically (nothing probe-specific is latched).

use super::switch_proto::{
    build_subcmd_reply, build_usb_ack, device_info_payload, parse_output, player_leds_bits,
    serialize_report_0x30, spi_flash_read, switch_mac, SwitchOutput, SwitchState, PROCON_RDESC,
    SWITCH_PRODUCT, SWITCH_REPORT_LEN, SWITCH_VENDOR,
};
use crate::uhid_abi::{
    put_cstr, BUS_USB, HID_MAX_DESCRIPTOR_SIZE, UHID_CREATE2, UHID_DESTROY, UHID_EVENT_SIZE,
    UHID_GET_REPORT, UHID_GET_REPORT_REPLY, UHID_INPUT2, UHID_OUTPUT, UHID_PATH,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::{Context, Result};
use punktfunk_core::quic::{HidOutput, RichInput};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

/// A virtual Pro Controller backed by `/dev/uhid`. Dropping it destroys the device (the kernel
/// tears down the bound `hid-nintendo` interface).
pub struct SwitchProPad {
    fd: File,
    index: u8,
    /// Rolling report timer (byte 1 of every input report).
    timer: u8,
    /// The last written state — subcommand replies embed the current input-state header, so the
    /// probe conversation always reports coherent (neutral, at first) controller state.
    state: SwitchState,
}

impl SwitchProPad {
    /// Create the UHID Pro Controller for pad `index` (used for the name/uniq + the virtual MAC).
    pub fn open(index: u8) -> Result<SwitchProPad> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut pad = SwitchProPad {
            fd,
            index,
            timer: 0,
            state: SwitchState::neutral(),
        };
        pad.send_create2(index).context("UHID_CREATE2 Switch Pro")?;
        Ok(pad)
    }

    fn send_create2(&mut self, index: u8) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // union (uhid_create2_req) starts at byte 4.
        put_cstr(
            &mut ev,
            4,
            128,
            &format!("Punktfunk Switch Pro Controller {index}"),
        ); // name[128]
        put_cstr(&mut ev, 132, 64, &format!("punktfunk/switchpro/{index}")); // phys[64]
        put_cstr(&mut ev, 196, 64, &format!("punktfunk-swpro-{index}")); // uniq[64]
        ev[260..262].copy_from_slice(&(PROCON_RDESC.len() as u16).to_ne_bytes()); // rd_size
        ev[262..264].copy_from_slice(&BUS_USB.to_ne_bytes()); // bus (selects the driver's USB init path)
        ev[264..268].copy_from_slice(&SWITCH_VENDOR.to_ne_bytes());
        ev[268..272].copy_from_slice(&SWITCH_PRODUCT.to_ne_bytes());
        ev[272..276].copy_from_slice(&0x0200u32.to_ne_bytes()); // version (bcdDevice 2.00)
        ev[276..280].copy_from_slice(&0u32.to_ne_bytes()); // country
        ev[280..280 + PROCON_RDESC.len()].copy_from_slice(PROCON_RDESC); // rd_data
        self.fd.write_all(&ev).context("write UHID_CREATE2")?;
        Ok(())
    }

    /// Write one full input report to the kernel (UHID_INPUT2).
    fn write_report(&mut self, r: &[u8; SWITCH_REPORT_LEN]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        ev[4..6].copy_from_slice(&(r.len() as u16).to_ne_bytes()); // input2.size
        ev[6..6 + r.len()].copy_from_slice(r); // input2.data
        self.fd.write_all(&ev).context("write UHID_INPUT2")?;
        Ok(())
    }

    /// Serialize the state into the standard `0x30` report and stream it.
    pub fn write_state(&mut self, st: &SwitchState) -> Result<()> {
        self.state = *st;
        self.timer = self.timer.wrapping_add(1);
        let r = serialize_report_0x30(st, self.timer);
        self.write_report(&r)
    }

    /// Answer one subcommand from the driver with its canned `0x21` reply.
    fn answer_subcmd(&mut self, id: u8, args: &[u8]) {
        self.timer = self.timer.wrapping_add(1);
        let st = self.state;
        let reply = match id {
            // Device info — the fatal one (probe aborts without it): type = Pro Controller +
            // this pad's virtual MAC. Real hardware acks it with 0x82.
            0x02 => build_subcmd_reply(
                &st,
                self.timer,
                0x82,
                id,
                &device_info_payload(&switch_mac(self.index)),
            ),
            // SPI flash read: echoed addr + len + the canned calibration bytes. An unmapped
            // range answers zeroes (echoed header, zero data) — the driver then warns and uses
            // its defaults instead of stalling through 2 × 1 s timeouts.
            0x10 => {
                let addr = args
                    .get(..4)
                    .map(|a| u32::from_le_bytes([a[0], a[1], a[2], a[3]]))
                    .unwrap_or(0);
                let len = args.get(4).copied().unwrap_or(0);
                let payload = spi_flash_read(addr, len).unwrap_or_else(|| {
                    tracing::debug!(
                        addr = format!("{addr:#x}"),
                        len,
                        "unmapped SPI read — zero fill"
                    );
                    let mut p = Vec::with_capacity(5 + len as usize);
                    p.extend_from_slice(&addr.to_le_bytes());
                    p.push(len);
                    p.extend(std::iter::repeat_n(0u8, len as usize));
                    p
                });
                build_subcmd_reply(&st, self.timer, 0x90, id, &payload)
            }
            // Everything else the driver sends (input mode 0x03, IMU 0x40, vibration 0x48,
            // player lights 0x30, home light 0x38, …) just needs the ack + echoed id.
            _ => build_subcmd_reply(&st, self.timer, 0x80, id, &[]),
        };
        let _ = self.write_report(&reply);
    }

    /// Service the device, non-blocking: answer the driver's probe conversation (USB commands +
    /// subcommands) and surface a game's rumble / player-lights feedback for pad `pad`. Call
    /// frequently — each probe step blocks the driver until answered.
    pub fn service(&mut self, pad: u8) -> PadFeedback {
        let mut fb = PadFeedback::default();
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
                    match parse_output(&ev[4..end]) {
                        Some(SwitchOutput::UsbCmd(cmd)) => {
                            // Ack every 0x80 command, incl. no-timeout (0x04) — the driver
                            // ignores that ack but replying skips its 2 × 100 ms wait.
                            let _ = self.write_report(&build_usb_ack(cmd));
                        }
                        Some(SwitchOutput::Subcmd { id, args, rumble }) => {
                            // No trigger motors on this protocol — see `PadFeedback::rumble`.
                            fb.rumble = Some((rumble.0, rumble.1, 0, 0));
                            if id == 0x30 {
                                // Player lights ride the subcommand itself; still ack it.
                                if let Some(&arg) = args.first() {
                                    fb.hidout.push(HidOutput::PlayerLeds {
                                        pad,
                                        bits: player_leds_bits(arg),
                                    });
                                }
                            }
                            self.answer_subcmd(id, &args);
                        }
                        Some(SwitchOutput::Rumble(r)) => fb.rumble = Some((r.0, r.1, 0, 0)),
                        None => {}
                    }
                }
                UHID_GET_REPORT => {
                    // hid-nintendo never GET_REPORTs; answer EIO so nothing ever blocks on us.
                    let req_id = u32::from_ne_bytes([ev[4], ev[5], ev[6], ev[7]]);
                    let _ = self.reply_get_report_err(req_id);
                }
                _ => {} // Start/Stop/Open/Close/SetReport — ignore
            }
        }
        fb
    }

    fn reply_get_report_err(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        ev[8..10].copy_from_slice(&5u16.to_ne_bytes()); // EIO
        self.fd
            .write_all(&ev)
            .context("write UHID_GET_REPORT_REPLY")?;
        Ok(())
    }
}

impl Drop for SwitchProPad {
    fn drop(&mut self) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

/// The Switch-Pro-specific half of the shared stateful manager (see [`PadProto`]): UHID
/// transport open, the [`SwitchState`] mappers, and the probe-conversation service pass.
/// Lifecycle (slot table, unplug sweep, heartbeat, dedup) lives in [`UhidManager`].
pub struct SwitchProProto {
    /// Fallback policy for the Steam back grips a client may send (a Pro Controller has no
    /// back-button slot). `PUNKTFUNK_STEAM_REMAP=paddles=…`; default drop.
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

    fn neutral(&self) -> SwitchState {
        SwitchState::neutral()
    }

    /// Merge buttons/sticks/triggers from the frame, preserving motion (it arrives on the rich
    /// plane and must survive a button-only frame). Paddles fold via the configured policy.
    fn merge_frame(
        &self,
        prev: &SwitchState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> SwitchState {
        let buttons = crate::steam_remap::fold_paddles(f.buttons, self.remap.paddles);
        let mut s = SwitchState::from_gamepad(
            buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        s.gyro = prev.gyro;
        s.accel = prev.accel;
        s
    }

    /// Motion lands on the IMU sample frames; a Pro Controller has no touchpad, so touch events
    /// are dropped (the client folds trackpads into stick/mouse modes itself).
    fn apply_rich(&self, st: &mut SwitchState, rich: RichInput) {
        if let RichInput::Motion { gyro, accel, .. } = rich {
            st.apply_motion(gyro, accel);
        }
    }

    fn neutralize_gyro(&self, st: &mut SwitchState) -> bool {
        st.neutralize_gyro()
    }

    fn clear_rich(&self, st: &mut SwitchState) {
        st.clear_rich();
    }

    fn write_state(&self, pad: &mut SwitchProPad, st: &SwitchState) {
        let _ = pad.write_state(st);
    }

    /// Answer the driver's probe conversation (it blocks `hid-nintendo` init until every step is
    /// answered — call frequently) and surface a game's feedback: HD-rumble amplitude on the
    /// universal 0xCA plane, player lights on the 0xCD plane.
    fn service(&self, pad: &mut SwitchProPad, idx: u8) -> PadFeedback {
        let mut fb = pad.service(idx);
        // Rumble-plane liveness: hid-nintendo embeds rumble data in every command it sends, so a
        // poll that surfaced rumble is the activity signal; a writer that goes silent on a latched
        // level is cut by the shared abandoned-rumble force-off (a physical Joy-Con/Pro's
        // HD-rumble decays on its own far faster than the idle window).
        fb.rumble_drove = Some(fb.rumble.is_some());
        fb
    }
}

/// All virtual Switch Pro Controllers of a session — `PUNKTFUNK_GAMEPAD=switchpro`, or the
/// per-pad kind a client declares for a Nintendo-family physical pad.
pub type SwitchProManager = UhidManager<SwitchProProto>;
