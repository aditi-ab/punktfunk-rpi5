//! Virtual Steam Controller 2 (Triton, `28DE:1302`) on Windows over the pf_gamepad UMDF shm
//! channel — the Windows analogue of the Linux UHID/usbip Triton backend (`super::steam_controller2`,
//! Linux-only), sharing the whole transport-independent contract in [`crate::triton_proto`].
//!
//! Unlike the Deck backend ([`super::steam_deck_windows`]), this device is NOT re-synthesized
//! from typed wire state: the client captures the physical controller (USB / wired / BLE) and
//! forwards its raw input reports verbatim
//! ([`RichInput::HidReport`](punktfunk_core::quic::RichInput)); the host mirrors them into the
//! section's input slot unchanged (the driver trims each to its declared report-id length before
//! serving it to hidclass). Feedback runs the other way: everything Steam's hidraw consumer
//! writes back — SET_REPORT features (lizard-off / IMU-enable / settings) and `0x80..` haptic
//! OUTPUT reports — comes back on the section's out ring **kind-tagged** (bit 31 of the slot
//! length, [`pf_driver_proto::triton::OUT_FEATURE_BIT`], drained by
//! [`OutputDrain::drain_tagged`]) and is forwarded raw to the client as `HidOutput::HidRaw` for
//! replay on the physical pad — FEATURE frames re-armed via `SET_REPORT(Feature)` / a GATT
//! feature write, OUTPUT frames via the
//! interrupt-OUT endpoint / a GATT characteristic write. Rumble is ALSO parsed out of the
//! untagged OUTPUT plane onto the universal 0xCA plane, so a client's phone-mirror rumble path
//! keeps working even without a raw feed.
//!
//! Transport = the same sealed shared-memory channel + `SwDeviceCreate` devnode shape the Deck
//! backend uses (device-type [`DEVTYPE_TRITON`] instead of the Deck's `DEVTYPE_STEAMDECK`), with
//! one identity delta: the real wired Triton is a **single-interface** USB device — its devnode
//! carries no `MI_` token, and SDL's hidapi claim for `28DE:1302` matches on VID/PID alone, not on
//! `bInterfaceNumber` the way the Deck's claim does — so `usb_mi` is `None` here (bench gate R2:
//! if Steam still won't list the pad, A/B `Some(0)`; the Deck needed `Some(2)` for its
//! multi-interface identity).

use super::dualsense_windows::{
    create_swdevice, publish_input, OutputDrain, SwDeviceProfile, OFF_DEVTYPE, OFF_DRIVER_PROTO,
    OFF_INPUT, OFF_OUT_RING_VER, OFF_PAD_INDEX, SHM_MAGIC, SHM_SIZE,
};
use super::gamepad_raii::{DriverAttach, PadChannel, ProofTransport, SwDevice};
use crate::triton_proto::{
    parse_triton_rumble, serialize_triton_state, triton_serial, TritonState, TRITON_STATE_LEN,
};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use pf_driver_proto::gamepad::DEVTYPE_TRITON;
use punktfunk_core::quic::{HidOutput, RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT};
use std::time::Duration;

/// The hardware id this pad's devnode carries. Must be one `pf_gamepad.inx` declares — a package
/// rename must never touch it.
pub(super) const TRITON_HWID: &str = "pf_triton";

/// A single virtual Steam Controller 2: the `SwDeviceCreate`'d `pf_triton_<index>` devnode plus
/// the sealed shared-memory channel. Dropping it removes the devnode and closes both sections.
/// `pub`: the type appears as `type Pad` in the `PadProto` impl (a public trait).
pub struct TritonWinPad {
    /// Per-session devnode from SwDeviceCreate, when it succeeds (RAII — `SwDeviceClose` on drop).
    _sw: Option<SwDevice>,
    /// The sealed channel: unnamed DATA section (`PadShm`) + bootstrap mailbox + handle delivery.
    channel: PadChannel,
    /// Watches the section's `driver_proto` field and logs attach / never-attached diagnosis.
    attach: DriverAttach,
    /// Synth-mode sequence counter — only advances for the typed fallback report; the raw path
    /// mirrors the physical pad's own report bytes (its own sequence byte included) unchanged.
    seq: u8,
    /// This pad's v2.3 input-seqlock generation — see `publish_input`.
    input_gen: u32,
    /// Output-plane cursor: the section's ring drain, kind-tagged for Triton's FEATURE/OUTPUT
    /// split (see [`OutputDrain::drain_tagged`]).
    drain: OutputDrain,
}

impl TritonWinPad {
    /// Create the sealed channel, stamp `device_type = Triton` FIRST + the pad index + a neutral
    /// `0x42` report + the magic LAST, then spawn the `pf_triton_<index>` devnode.
    fn open(index: u8) -> Result<TritonWinPad> {
        let boot_name = pf_driver_proto::gamepad::pad_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let base = channel.data_base();
        // SAFETY: base points at SHM_SIZE writable bytes; the OFF_* offsets are in range.
        unsafe {
            *base.add(OFF_DEVTYPE) = DEVTYPE_TRITON;
            std::ptr::write_unaligned(base.add(OFF_PAD_INDEX) as *mut u32, index as u32);
            // Ring capability `2` = "this host drains the v2.2 long ring", stamped before the
            // magic so the driver sees it on attach (see the DualSense open path + PadShm docs).
            std::ptr::write_unaligned(base.add(OFF_OUT_RING_VER) as *mut u32, 2);
            std::ptr::write_unaligned(
                base.add(OFF_INPUT) as *mut [u8; 64],
                neutral_triton_report(),
            );
            std::ptr::write_unaligned(base as *mut u32, SHM_MAGIC);
        }
        let inst = format!("pf_triton_{index}");
        let (hsw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &inst,
            container_tag: 0x5046_4453, // "PFDS"
            container_index: index,
            hwid: TRITON_HWID,
            usb_vid_pid: "VID_28DE&PID_1302",
            // The real wired Triton is single-interface — its devnode carries no MI_ token, and
            // SDL's claim for 0x1302 is VID/PID-only. Bench gate R2: if Steam balks, A/B Some(0)
            // here (the Deck needed Some(2) for its multi-interface model).
            usb_mi: None,
            description: "Punktfunk Virtual Steam Controller",
        })?; // Propagate — swallowing latched the slot to a pad with no devnode (see the DS4 twin).
        let (hsw, instance_id) = (Some(hsw), instance_id);
        // The DATA section goes to whoever THIS devnode says is serving it — not to whatever pid
        // the LocalService-writable mailbox names (security-review 2026-07-28).
        channel.bind_devnode(
            index as u32,
            instance_id.clone(),
            ProofTransport::HidFeatureReport,
        );
        let _sw = hsw.map(SwDevice::new);
        // Bounded eager delivery — the driver must read `device_type = Triton` before hidclass
        // asks it for descriptors, or the pad would enumerate with the default DualSense identity.
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(TritonWinPad {
            _sw,
            channel,
            attach: DriverAttach::new(
                TRITON_HWID,
                "pf_gamepad.inf", // one driver package serves every identity
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pf_gamepad-driver.log",
                boot_name,
                instance_id,
            ),
            seq: 0,
            input_gen: 0,
            drain: OutputDrain::new(),
        })
    }

    /// Mirror one report into the section's input slot: the client's raw bytes verbatim in as-is
    /// mode, else a synthesized minimal `0x42` state report from the typed fallback fields —
    /// the same mirroring policy as the Linux backend's `TritonPad::write_state`.
    fn write_state(&mut self, st: &TritonState) {
        let mut r = [0u8; 64];
        if st.raw_len > 0 {
            let len = (st.raw_len as usize).min(st.raw.len()).min(r.len());
            r[..len].copy_from_slice(&st.raw[..len]);
        } else {
            self.seq = self.seq.wrapping_add(1);
            let mut s = [0u8; TRITON_STATE_LEN];
            serialize_triton_state(&mut s, st, self.seq);
            r[..TRITON_STATE_LEN].copy_from_slice(&s);
        }
        // SAFETY: same contract as DeckWinPad::write_state — the v2.3 input_gen seqlock.
        unsafe { publish_input(self.channel.data_base(), &mut self.input_gen, &r) };
    }

    /// Drain Steam's writes: rumble for the universal 0xCA plane (parsed only out of an untagged
    /// OUTPUT report — a FEATURE frame is never a rumble command), everything raw and kind-tagged
    /// for the client's `[0xCD][0x05]` HidRaw plane. Also ticks the sealed-channel delivery and
    /// the driver-attach health watcher. Returns `(rumble, raw reports, resync)`; `resync` is the
    /// drain's ring-overflow flag and must reach `PadFeedback` unchanged — see
    /// `TritonWinProto::service`.
    fn service(&mut self, idx: u8) -> (Option<(u16, u16)>, Vec<HidOutput>, bool) {
        self.channel.pump();
        // SAFETY: base points at SHM_SIZE bytes.
        let proto = unsafe {
            std::ptr::read_unaligned(self.channel.data_base().add(OFF_DRIVER_PROTO) as *const u32)
        };
        self.attach.observe(proto);
        let base = self.channel.data_base();
        let mut rumble = None;
        let mut hidout = Vec::new();
        let resync = self.drain.drain_tagged(base, |bytes, feature| {
            // Windows hidclass pads every output write to `OutputReportByteLength` (64) before
            // the driver rings it, whereas the Linux leg (`steam_controller2`) forwards native
            // lengths (a 0x80 rumble is 10 bytes). The client replays these bytes verbatim over
            // GATT, so trim each OUTPUT frame to its declared per-id wire length here — cross-OS
            // parity; no untested padding reaches the physical controller's firmware. FEATURE
            // frames deliberately stay whole: Steam SETs full-length feature reports, and both
            // the feature machine and the BLE feature replay consume whole frames. (The ring
            // path only delivers non-empty slices; the salvage/legacy paths deliver a fixed
            // 64-byte slice — an empty slice would pass through untrimmed.)
            let bytes = match (feature, bytes.first()) {
                (false, Some(&id)) => {
                    &bytes[..bytes.len().min(pf_driver_proto::triton::out_report_len(id))]
                }
                _ => bytes,
            };
            if !feature {
                if let Some(r) = parse_triton_rumble(bytes) {
                    rumble = Some(r);
                }
            }
            hidout.push(HidOutput::HidRaw {
                pad: idx,
                kind: if feature {
                    HID_RAW_FEATURE
                } else {
                    HID_RAW_OUTPUT
                },
                data: bytes.to_vec(),
            });
        });
        (rumble, hidout, resync)
    }
}

/// A neutral wired-Triton `0x42` state report: report id plus an all-zero 53-byte payload — the
/// Phase-0 canned shape a fresh or unplugged pad (re)starts from, before either a raw feed or the
/// typed fallback publishes anything real.
fn neutral_triton_report() -> [u8; 64] {
    let mut r = [0u8; 64];
    r[0] = 0x42;
    r
}

/// The Windows-Triton half of the shared stateful manager (see [`PadProto`]): the sealed-channel
/// open under the Triton identity, the same [`TritonState`] as-is mirroring + typed-fallback
/// mappers the Linux backend uses, and the kind-tagged feedback poll. Lifecycle (slot table,
/// unplug sweep, heartbeat, rumble dedup) lives in [`UhidManager`].
///
/// `Default` is REQUIRED: `UhidManager::new()` bounds `B: PadProto + Default`
/// ([`crate::uhid_manager`]) — every backend's `Proto` derives it, this one included.
#[derive(Default)]
pub struct TritonWinProto;

impl PadProto for TritonWinProto {
    type Pad = TritonWinPad;
    type State = TritonState;
    const LABEL: &'static str = "Steam Controller 2/Windows";
    const DEVICE: &'static str = "Steam Controller 2";
    const CREATE_HINT: &'static str =
        " (install/repair: punktfunk-host.exe driver install --gamepad)";

    fn open(&mut self, idx: u8) -> Result<TritonWinPad> {
        let p = TritonWinPad::open(idx)?;
        tracing::info!(
            index = idx,
            // The in-driver query-dance answers Steam's feature GET_REPORTs itself (unlike the
            // Linux UHID leg, which must round-trip through user-space); it derives this same
            // serial from the pad index with no host-side plumbing needed. Logged here purely as
            // an on-glass diagnostic breadcrumb — "what serial should Steam be showing".
            serial = %triton_serial(idx),
            "virtual Steam Controller 2 created (Windows UMDF shm channel, as-is raw passthrough)"
        );
        Ok(p)
    }

    fn neutral(&self) -> TritonState {
        TritonState::neutral()
    }

    /// Typed fallback merge. Once raw reports flow (`raw_len > 0`) the frame only refreshes the
    /// typed fields for diagnostics — `write_state` keeps mirroring the raw report. Identical to
    /// the Linux `TritonProto::merge_frame` ("as-is mode is sticky").
    fn merge_frame(
        &self,
        prev: &TritonState,
        f: &punktfunk_core::input::GamepadFrame,
    ) -> TritonState {
        let mut s = TritonState::from_gamepad(
            f.buttons,
            f.ls_x,
            f.ls_y,
            f.rs_x,
            f.rs_y,
            f.left_trigger,
            f.right_trigger,
        );
        // As-is mode is sticky: a typed frame between two raw reports must not flap the pad back
        // to synth mode (the client sends BOTH planes — typed keeps the degrade paths alive).
        s.raw = prev.raw;
        s.raw_len = prev.raw_len;
        s
    }

    fn apply_rich(&self, st: &mut TritonState, rich: RichInput) {
        if let RichInput::HidReport { len, data, .. } = rich {
            let len = (len as usize).min(data.len()).min(st.raw.len());
            if len == 0 {
                return;
            }
            st.raw[..len].copy_from_slice(&data[..len]);
            st.raw_len = len as u8;
        }
        // Touchpad/Motion/TouchpadEx: nothing to fold — the raw feed carries pads + IMU natively,
        // and the synth fallback has no surface for them.
    }

    // `neutralize_gyro` / `clear_rich` stay the no-op defaults — same rationale as the Linux
    // backend: this device never sees a `RichInput::Motion` to go stale, and its motion lives
    // inside an opaque passthrough report the trait has no business reaching into.

    fn write_state(&self, pad: &mut TritonWinPad, st: &TritonState) {
        pad.write_state(st);
    }

    /// Ack + forward Steam's writes: rumble on the universal 0xCA plane, everything raw
    /// (kind-tagged) on the `[0xCD][0x05]` HidRaw plane — mirrors the Linux `TritonProto::service`,
    /// plus forwarding the drain's own resync flag like `DeckWinProto::service` does (the Linux
    /// leg has no ring to overflow, hence its permanent `resync: false`; this backend's ring can,
    /// so hardcoding `false` here would silently swallow the Windows-only overflow signal).
    fn service(&self, pad: &mut TritonWinPad, idx: u8) -> PadFeedback {
        let (rumble, hidout, resync) = pad.service(idx);
        PadFeedback {
            // No trigger motors on this protocol — see `PadFeedback::rumble`.
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout,
            // Rumble-plane liveness: Steam is a hidraw writer here too, so the shared
            // abandoned-rumble force-off applies (the raw 0xCD passthrough plane is unaffected).
            rumble_drove: Some(rumble.is_some()),
            resync,
        }
    }
}

/// All virtual Steam Controller 2 pads of a Windows session — the analogue of the Linux
/// `Triton2Manager`, with the same method surface (via the shared [`UhidManager`]) as the other
/// Windows pad managers.
pub type TritonWindowsManager = UhidManager<TritonWinProto>;
