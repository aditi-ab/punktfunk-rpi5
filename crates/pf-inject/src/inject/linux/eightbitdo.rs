//! Virtual 8BitDo pads in their own HID mode via `/dev/uhid`.
//!
//! No kernel driver claims `2DC8:6003/6009/6012`, so `hid-generic` binds and SDL's and Steam's
//! `8bitdo` driver read hidraw — the hidraw rule in `60-punktfunk.rules` lets the seat user in.
//! Codec, descriptors and the feature reply live in [`super::eightbitdo_proto`]; this file is the
//! UHID transport.

use super::eightbitdo_proto::{
    caps_reply, parse_rumble, EightBitDoState, Model, FEATURE_CAPS, VENDOR,
};
use crate::sensor_clock::SensorClock;
use crate::uhid_abi::{Create2, UhidDevice, UhidEvent, BUS_BLUETOOTH, BUS_USB};
use crate::uhid_manager::{PadFeedback, PadProto, UhidManager};
use anyhow::Result;
use punktfunk_core::input::GamepadFrame;
use punktfunk_core::quic::RichInput;
use std::time::{Duration, Instant};

/// Drop destroys the device.
pub struct EightBitDoPad {
    dev: UhidDevice,
    clock: SensorClock,
}

impl EightBitDoPad {
    pub fn open(model: Model, index: u8) -> Result<EightBitDoPad> {
        let dev = UhidDevice::open(&Create2 {
            bus: if model.bluetooth() {
                BUS_BLUETOOTH
            } else {
                BUS_USB
            },
            name: model.name(),
            phys: &format!("punktfunk/8bitdo/{index}"),
            uniq: &format!("punktfunk-8bitdo-{index}"),
            rdesc: model.rdesc(),
            vendor: VENDOR as u32,
            product: model.product() as u32,
            version: 0x0100,
        })?;
        Ok(EightBitDoPad {
            dev,
            clock: SensorClock::micros(),
        })
    }
}

/// One manager per model: the model fixes the identity, the face-button swap and the pacing.
pub struct EightBitDoProto {
    model: Model,
}

impl EightBitDoProto {
    pub fn new(model: Model) -> EightBitDoProto {
        EightBitDoProto { model }
    }
}

impl PadProto for EightBitDoProto {
    type Pad = EightBitDoPad;
    type State = EightBitDoState;
    const LABEL: &'static str = "8BitDo";
    const DEVICE: &'static str = "8BitDo";
    const CREATE_HINT: &'static str = "";

    fn open(&mut self, idx: u8) -> Result<EightBitDoPad> {
        let p = EightBitDoPad::open(self.model, idx)?;
        tracing::info!(
            index = idx,
            model = self.model.name(),
            "virtual 8BitDo created (UHID hid-generic)"
        );
        Ok(p)
    }

    fn merge_frame(&self, prev: &EightBitDoState, f: &GamepadFrame) -> EightBitDoState {
        EightBitDoState::merge_frame(self.model, prev, f)
    }

    fn apply_rich(&self, st: &mut EightBitDoState, rich: RichInput) {
        st.apply_rich(rich);
    }

    fn write_state(&self, pad: &mut EightBitDoPad, st: &EightBitDoState) {
        let clock = self
            .model
            .timestamps()
            .then(|| pad.clock.ticks(Instant::now()) as u32);
        let _ = pad.dev.write_input(&st.serialize(clock));
    }

    /// Rumble on 0xCA; the Pro models' capability feature answered from [`caps_reply`].
    fn service(&self, pad: &mut EightBitDoPad, idx: u8) -> PadFeedback {
        let answers_caps = self.model.timestamps();
        let mut rumble = None;
        pad.dev.poll(|dev, ev| match ev {
            UhidEvent::Output(data) => {
                if let Some(r) = parse_rumble(data) {
                    rumble = Some(r);
                }
            }
            UhidEvent::GetReport { id, rnum } => {
                let caps = caps_reply(idx);
                let data = (answers_caps && rnum == FEATURE_CAPS).then_some(&caps[..]);
                let _ = dev.reply_get_report(id, data);
            }
            UhidEvent::SetReport(_) => {}
        });
        PadFeedback {
            rumble: rumble.map(|(low, high)| (low, high, 0, 0)),
            hidout: Vec::new(),
            // Every `0x05` names both motors, so each one drives the rumble plane.
            rumble_drove: Some(rumble.is_some()),
            resync: false,
        }
    }

    fn report_period(&self) -> Option<Duration> {
        self.model.report_period()
    }
}

pub type EightBitDoManager = UhidManager<EightBitDoProto>;

/// A manager for one model's pads.
pub fn manager(model: Model) -> EightBitDoManager {
    UhidManager::with_backend(EightBitDoProto::new(model))
}
