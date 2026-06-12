//! Gamepad capture + feedback over SDL3, on a dedicated thread.
//!
//! Mirrors the Apple client's selection model: exactly one pad is forwarded as pad 0 —
//! the first connected (a pin/auto picker lands with the settings work). SDL3 is the one
//! library with full DualSense fidelity (touchpad/gyro/lightbar/player LEDs/rumble +
//! adaptive triggers via raw effect packets), matching the wire planes; this stage wires
//! buttons/axes out and rumble/lightbar back. Touchpad/motion capture (0xCC) and
//! adaptive-trigger replay (0xCD `Trigger`) are follow-ups on the same loop.
//!
//! This thread also owns the rumble and HID-output pull planes (one consumer per plane).

use punktfunk_core::client::NativeClient;
use punktfunk_core::input::{gamepad as wire, InputEvent, InputKind};
use punktfunk_core::quic::HidOutput;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(
    connector: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("punktfunk-gamepad".into())
        .spawn(move || {
            if let Err(e) = run(&connector, &stop) {
                tracing::warn!(error = %e, "gamepad thread ended — pads disabled");
            }
        })
        .ok()
}

fn send(connector: &NativeClient, kind: InputKind, code: u32, x: i32) {
    let _ = connector.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y: 0,
        flags: 0, // pad index 0 — single-pad model
    });
}

fn button_bit(b: sdl3::gamepad::Button) -> Option<u32> {
    use sdl3::gamepad::Button;
    Some(match b {
        Button::South => wire::BTN_A,
        Button::East => wire::BTN_B,
        Button::West => wire::BTN_X,
        Button::North => wire::BTN_Y,
        Button::Back => wire::BTN_BACK,
        Button::Start => wire::BTN_START,
        Button::Guide => wire::BTN_GUIDE,
        Button::LeftStick => wire::BTN_LS_CLICK,
        Button::RightStick => wire::BTN_RS_CLICK,
        Button::LeftShoulder => wire::BTN_LB,
        Button::RightShoulder => wire::BTN_RB,
        Button::DPadUp => wire::BTN_DPAD_UP,
        Button::DPadDown => wire::BTN_DPAD_DOWN,
        Button::DPadLeft => wire::BTN_DPAD_LEFT,
        Button::DPadRight => wire::BTN_DPAD_RIGHT,
        Button::Touchpad => wire::BTN_TOUCHPAD,
        _ => return None,
    })
}

/// SDL axis → (wire axis id, wire value). SDL sticks are +y = down; the wire (XInput
/// convention) is +y = up. SDL triggers span 0..32767; the wire wants 0..255.
fn axis_value(axis: sdl3::gamepad::Axis, v: i16) -> (u32, i32) {
    use sdl3::gamepad::Axis;
    match axis {
        Axis::LeftX => (wire::AXIS_LS_X, v as i32),
        Axis::LeftY => (wire::AXIS_LS_Y, -(v as i32).max(-32767)),
        Axis::RightX => (wire::AXIS_RS_X, v as i32),
        Axis::RightY => (wire::AXIS_RS_Y, -(v as i32).max(-32767)),
        Axis::TriggerLeft => (wire::AXIS_LT, (v as i32).clamp(0, 32767) >> 7),
        Axis::TriggerRight => (wire::AXIS_RT, (v as i32).clamp(0, 32767) >> 7),
    }
}

fn run(connector: &NativeClient, stop: &AtomicBool) -> Result<(), String> {
    // Off-main-thread + no video subsystem: keep SDL away from signals, poll pads on its
    // own thread.
    sdl3::hint::set("SDL_NO_SIGNAL_HANDLERS", "1");
    sdl3::hint::set("SDL_JOYSTICK_THREAD", "1");
    let sdl = sdl3::init().map_err(|e| e.to_string())?;
    let subsystem = sdl.gamepad().map_err(|e| e.to_string())?;
    let mut pump = sdl.event_pump().map_err(|e| e.to_string())?;

    let mut active: Option<sdl3::gamepad::Gamepad> = None;
    let pad_id = |p: &Option<sdl3::gamepad::Gamepad>| -> Option<u32> {
        p.as_ref().and_then(|p| p.id().ok()).map(|id| id.0)
    };
    // Last sent wire value per axis id — suppress no-op repeats (SDL re-reports).
    let mut last_axis = [i32::MIN; 6];

    while !stop.load(Ordering::SeqCst) {
        while let Some(event) = pump.poll_event() {
            use sdl3::event::Event;
            match event {
                Event::ControllerDeviceAdded { which, .. } => {
                    if active.is_none() {
                        match subsystem.open(sdl3::sys::joystick::SDL_JoystickID(which)) {
                            Ok(pad) => {
                                tracing::info!(
                                    name = pad.name().unwrap_or_default(),
                                    "gamepad attached as pad 0"
                                );
                                active = Some(pad);
                                last_axis = [i32::MIN; 6];
                            }
                            Err(e) => tracing::warn!(error = %e, "gamepad open failed"),
                        }
                    }
                }
                Event::ControllerDeviceRemoved { which, .. } => {
                    if pad_id(&active) == Some(which) {
                        tracing::info!("gamepad detached");
                        active = None;
                    }
                }
                Event::ControllerButtonDown { which, button, .. } => {
                    if pad_id(&active) == Some(which) {
                        if let Some(bit) = button_bit(button) {
                            send(connector, InputKind::GamepadButton, bit, 1);
                        }
                    }
                }
                Event::ControllerButtonUp { which, button, .. } => {
                    if pad_id(&active) == Some(which) {
                        if let Some(bit) = button_bit(button) {
                            send(connector, InputKind::GamepadButton, bit, 0);
                        }
                    }
                }
                Event::ControllerAxisMotion {
                    which, axis, value, ..
                } if pad_id(&active) == Some(which) => {
                    let (id, v) = axis_value(axis, value);
                    if last_axis[id as usize] != v {
                        last_axis[id as usize] = v;
                        send(connector, InputKind::GamepadAxis, id, v);
                    }
                }
                _ => {}
            }
        }

        // Feedback planes (this thread is their single consumer). The host re-sends
        // rumble state periodically, so a generous duration with refresh-on-update is
        // safe — a dropped stop heals within ~500 ms.
        while let Ok((pad, low, high)) = connector.next_rumble(Duration::ZERO) {
            if pad == 0 {
                if let Some(p) = active.as_mut() {
                    let _ = p.set_rumble(low, high, 5_000);
                }
            }
        }
        loop {
            match connector.next_hidout(Duration::ZERO) {
                Ok(HidOutput::Led { pad: 0, r, g, b }) => {
                    if let Some(p) = active.as_mut() {
                        let _ = p.set_led(r, g, b);
                    }
                }
                Ok(HidOutput::PlayerLeds { .. }) => {} // TODO: SDL player-index mapping
                Ok(HidOutput::Trigger { .. }) => {}    // TODO: DS5 effect packet replay
                Ok(_) => {}
                Err(_) => break,
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
