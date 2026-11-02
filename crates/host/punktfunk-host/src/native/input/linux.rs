//! The Linux virtual-pad managers behind [`PadBackends`](super::PadBackends): UHID/usbip, one
//! per identity, and the uinput Xbox One and Elite. Xbox360 over uinput stays in `Pads`.

use super::*;
use crate::inject::eightbitdo_proto::Model as EightBitDo;
use crate::inject::switch2_proto::Model as Switch2;

/// In pump and heartbeat order.
pub(super) const MANAGERS: &[ManagerRow] = &[
    (GamepadPref::XboxOne, || {
        boxed(crate::inject::gamepad::GamepadManager::with_identity(
            crate::inject::gamepad::PadIdentity::xbox_one(),
        ))
    }),
    (GamepadPref::XboxElite, || {
        boxed(crate::inject::gamepad::GamepadManager::with_identity(
            crate::inject::gamepad::PadIdentity::elite2(),
        ))
    }),
    (GamepadPref::DualSense, || {
        boxed(crate::inject::dualsense::DualSenseManager::new())
    }),
    (GamepadPref::DualSenseEdge, || {
        boxed(crate::inject::dualsense::DualSenseEdgeManager::new())
    }),
    (GamepadPref::DualShock4, || {
        boxed(crate::inject::dualshock4::DualShock4Manager::new())
    }),
    (GamepadPref::SteamDeck, || {
        boxed(crate::inject::steam_controller::SteamControllerManager::new())
    }),
    (GamepadPref::SwitchPro, || {
        boxed(crate::inject::switch_pro::SwitchProManager::new())
    }),
    (GamepadPref::SteamController, || {
        boxed(crate::inject::steam_controller::SteamCtrlManager::new())
    }),
    (GamepadPref::SteamController2, || {
        boxed(pf_inject::steam_controller2::Triton2Manager::new())
    }),
    (GamepadPref::SteamController2Puck, || {
        boxed(
            crate::inject::steam_controller2::Triton2Manager::with_backend(
                crate::inject::steam_controller2::TritonProto::puck(),
            ),
        )
    }),
    (GamepadPref::EightBitDoUltimate2, || {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Ultimate2))
    }),
    (GamepadPref::EightBitDoPro2, || {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Pro2))
    }),
    (GamepadPref::EightBitDoPro3, || {
        boxed(crate::inject::eightbitdo::manager(EightBitDo::Pro3))
    }),
    (GamepadPref::HoripadSteam, || {
        boxed(crate::inject::hori_steam::HoriManager::new())
    }),
    (GamepadPref::JoyConPair, || {
        boxed(crate::inject::switch_pro::JoyConPairManager::new())
    }),
    (GamepadPref::Switch2Pro, || {
        boxed(crate::inject::switch2_usbip::manager(Switch2::Pro))
    }),
    (GamepadPref::Switch2GameCube, || {
        boxed(crate::inject::switch2_usbip::manager(Switch2::GameCube))
    }),
];
