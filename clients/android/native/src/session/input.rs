//! Input plane: Kotlin capture → `NativeClient::send_input`.
//!
//! All shims are `&self` on the `Sync` connector (send_input is a non-blocking datagram push), safe
//! from the Kotlin UI thread. NOT android-gated — send_input exists on the host build too, so these
//! compile everywhere (parity with nativeConnect/nativeClose). The wire codes are the GameStream
//! conventions: buttons 1=left/2=middle/3=right/4=X1/5=X2; scroll axis 0=vertical/1=horizontal,
//! signed 120-unit delta, +=up/right; keys are Windows VK (mapped from KEYCODE_* on the Kotlin side).

use jni::objects::JObject;
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use punktfunk_core::input::{InputEvent, InputKind};

use super::SessionHandle;

/// Shared shim body: guard against a `0` handle, deref, and push one [`InputEvent`].
fn send_event(handle: jlong, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_input is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

/// `NativeBridge.nativeSendPointerMove(handle, dx, dy)` — relative mouse motion (screen +y down).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerMove(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    dx: jint,
    dy: jint,
) {
    send_event(handle, InputKind::MouseMove, 0, dx, dy, 0);
}

/// `NativeBridge.nativeSendPointerAbs(handle, x, y, surfaceWidth, surfaceHeight)` — absolute cursor
/// position: the host moves the pointer to `x`/`y` in a `surfaceWidth`×`surfaceHeight` pixel space,
/// normalizing against the size packed into `flags` as `(w << 16) | h` and mapping into the output
/// region (it drops the event if that size is zero). This is the touch "direct pointing" path — the
/// cursor jumps to the finger — and matches the Apple client's absolute touch forwarding.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerAbs(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    x: jint,
    y: jint,
    surface_width: jint,
    surface_height: jint,
) {
    let w = (surface_width.max(0) as u32) & 0xffff;
    let ht = (surface_height.max(0) as u32) & 0xffff;
    send_event(handle, InputKind::MouseMoveAbs, 0, x, y, (w << 16) | ht);
}

/// `NativeBridge.nativeSendPointerButton(handle, button, down)` — one button transition.
/// `button`: GameStream id (1=left, 2=middle, 3=right, 4=X1, 5=X2). `down`: 1=press, 0=release.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerButton(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    button: jint,
    down: jboolean,
) {
    let kind = if down != 0 {
        InputKind::MouseButtonDown
    } else {
        InputKind::MouseButtonUp
    };
    send_event(handle, kind, button as u32, 0, 0, 0);
}

/// `NativeBridge.nativeSendScroll(handle, axis, delta)` — one scroll step. `axis`: 0=vertical,
/// 1=horizontal. `delta`: signed, WHEEL_DELTA(120)-scaled, +=up/right.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendScroll(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    axis: jint,
    delta: jint,
) {
    send_event(handle, InputKind::MouseScroll, axis as u32, delta, 0, 0);
}

/// `NativeBridge.nativeSendTouch(handle, id, kind, x, y, surfaceWidth, surfaceHeight)` — one REAL
/// touchscreen transition (`kind`: 0=down 1=move 2=up), for the touch-passthrough input mode. `id`
/// distinguishes fingers (reusable after up); coordinates are pixels on the client's touch
/// surface, whose size rides in `flags` so the host can rescale into the output (identical
/// packing to MouseMoveAbs). On up only the id matters. The host injects a real touch contact
/// (libei touchscreen / wlroots / SendInput).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendTouch(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    id: jint,
    kind: jint,
    x: jint,
    y: jint,
    surface_width: jint,
    surface_height: jint,
) {
    let kind = match kind {
        0 => InputKind::TouchDown,
        1 => InputKind::TouchMove,
        _ => InputKind::TouchUp,
    };
    let w = (surface_width.max(0) as u32) & 0xffff;
    let h = (surface_height.max(0) as u32) & 0xffff;
    send_event(handle, kind, id as u32, x, y, (w << 16) | h);
}

/// `NativeBridge.nativeSendKey(handle, vk, down, mods)` — one key transition. `vk`: Windows
/// Virtual-Key code (0 = unmapped → dropped). `down`: 1=press, 0=release. `mods`: VK modifier
/// bitmask (0 for now — the host folds modifiers from the L/R modifier key events themselves).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendKey(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    vk: jint,
    down: jboolean,
    mods: jint,
) {
    if vk == 0 {
        return;
    }
    let kind = if down != 0 {
        InputKind::KeyDown
    } else {
        InputKind::KeyUp
    };
    send_event(handle, kind, vk as u32, 0, 0, mods as u32);
}

// ---- Gamepad: Kotlin captures (KeyEvent/MotionEvent) → NativeClient::send_input ---------------
// Multi-pad model: each physical controller is forwarded on its own wire pad index (0..15), carried
// in the low byte of `flags` on every per-pad event — the Kotlin side (`GamepadRouter`) assigns a
// stable lowest-free index per Android device and threads it here. Buttons carry the gamepad::BTN_*
// bit in `code` and pressed/released in `x` (1/0); axes carry the gamepad::AXIS_* id in `code` and
// the value in `x` (sticks i16 −32768..32767, +y = up; triggers 0..255). The host accumulates the
// incremental events per pad into a matching virtual device. The core input task folds these into
// the seq'd GamepadState snapshots (keyed on this same `flags` index) and owns the per-pad seq — so
// the only thing this layer must get right is the index. Wire contract: input.rs::gamepad. A single
// controller lands on index 0, so its wire is byte-identical to the old single-pad path.

/// `NativeBridge.nativeSendGamepadButton(handle, bit, down, pad)` — one gamepad button transition on
/// wire pad index `pad`. `bit`: a `gamepad::BTN_*` bit (e.g. BTN_A = 0x1000). `down`: 1=press,
/// 0=release. `pad`: wire pad index 0..15 (rides `flags`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadButton(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    bit: jint,
    down: jboolean,
    pad: jint,
) {
    send_event(
        handle,
        InputKind::GamepadButton,
        bit as u32,
        i32::from(down != 0),
        0,
        pad as u32,
    );
}

/// `NativeBridge.nativeSendGamepadAxis(handle, axisId, value, pad)` — one gamepad axis update on wire
/// pad index `pad`. `axisId`: a `gamepad::AXIS_*` id (LS_X=0..RT=5). `value`: stick i16
/// (−32768..32767, +y=up) or trigger 0..255. `pad`: wire pad index 0..15 (rides `flags`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadAxis(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    axis_id: jint,
    value: jint,
    pad: jint,
) {
    send_event(
        handle,
        InputKind::GamepadAxis,
        axis_id as u32,
        value,
        0,
        pad as u32,
    );
}

/// `NativeBridge.nativeSendGamepadArrival(handle, pref, pad)` — declare the controller KIND presented
/// on wire pad index `pad` so the host builds a matching virtual device (mixed types — pad 0 a
/// DualSense, pad 1 an Xbox pad). `pref`: the `GamepadPref` wire byte (rides `code`). `pad`: wire pad
/// index 0..15 (rides `flags`). Sent ONCE when a pad opens, BEFORE any of its input; the core re-sends
/// it a few times against datagram loss, and an older host ignores the unknown tag (that pad then uses
/// the session-default kind from the handshake — the pre-existing single-pad behaviour on pad 0).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadArrival(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    pref: jint,
    pad: jint,
) {
    send_event(
        handle,
        InputKind::GamepadArrival,
        pref as u32,
        0,
        0,
        pad as u32,
    );
}

/// `NativeBridge.nativeSendGamepadRemove(handle, pad)` — signal that wire pad index `pad` was
/// unplugged so the host tears its virtual device down. `pad` (rides `flags`) is the only field; the
/// core stamps the per-pad seq (in the snapshot seq space, so a reordered snapshot can't resurrect the
/// pad) and arms a re-send burst against datagram loss. An older host ignores the unknown tag.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadRemove(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    pad: jint,
) {
    send_event(handle, InputKind::GamepadRemove, 0, 0, 0, pad as u32);
}
