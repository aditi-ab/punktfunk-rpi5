//! The Windows virtual-pad managers behind [`PadBackends`](super::PadBackends): UMDF HID, one
//! per identity. Xbox360 over XUSB stays in `Pads`.

use super::*;
use crate::inject::eightbitdo_proto::Model as EightBitDo;
use crate::inject::xbox_windows::{XboxWinProto, XboxWindowsManager};
use crate::native::gamepad::windows_xbox_hid;

/// In pump and heartbeat order.
pub(super) const MANAGERS: &[ManagerRow] = &[
    (GamepadPref::SteamController2, || {
        boxed(pf_inject::triton_windows::TritonWindowsManager::new())
    }),
    (GamepadPref::DualSense, || {
        boxed(crate::inject::dualsense_windows::DualSenseWindowsManager::new())
    }),
    // HID Xbox, one identity per manager, unless `windows_xbox_hid` picks XUSB: then
    // `degrade_xbox_identity` has folded One and Elite to Xbox360, which falls through to the
    // XUSB pad in `Pads`. Never both: two devices for one wire pad are two controllers.
    (GamepadPref::Xbox360, || {
        windows_xbox_hid()
            .then(XboxWindowsManager::new)
            .and_then(boxed)
    }),
    (GamepadPref::XboxOne, || {
        windows_xbox_hid()
            .then(|| XboxWindowsManager::with_backend(XboxWinProto::one_s()))
            .and_then(boxed)
    }),
    (GamepadPref::XboxElite, || {
        windows_xbox_hid()
            .then(|| XboxWindowsManager::with_backend(XboxWinProto::elite()))
            .and_then(boxed)
    }),
    (GamepadPref::DualSenseEdge, || {
        boxed(crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new())
    }),
    (GamepadPref::DualShock4, || {
        boxed(crate::inject::dualshock4_windows::DualShock4WindowsManager::new())
    }),
    (GamepadPref::SteamDeck, || {
        boxed(crate::inject::steam_deck_windows::SteamDeckWindowsManager::new())
    }),
    (GamepadPref::SteamController, || {
        boxed(crate::inject::steam_controller_windows::SteamControllerWindowsManager::new())
    }),
    (GamepadPref::SwitchPro, || {
        boxed(crate::inject::switch_pro_windows::SwitchProWindowsManager::new())
    }),
    (GamepadPref::EightBitDoUltimate2, || {
        boxed(crate::inject::eightbitdo_windows::manager(
            EightBitDo::Ultimate2,
        ))
    }),
    (GamepadPref::EightBitDoPro2, || {
        boxed(crate::inject::eightbitdo_windows::manager(EightBitDo::Pro2))
    }),
    (GamepadPref::EightBitDoPro3, || {
        boxed(crate::inject::eightbitdo_windows::manager(EightBitDo::Pro3))
    }),
    (GamepadPref::HoripadSteam, || {
        boxed(crate::inject::hori_windows::HoriWindowsManager::new())
    }),
    (GamepadPref::JoyConPair, || {
        boxed(crate::inject::switch_pro_windows::JoyConWindowsManager::new())
    }),
];
