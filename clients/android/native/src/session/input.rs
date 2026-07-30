//! Input plane: Kotlin capture → `NativeClient::send_input`.
//!
//! All shims are `&self` on the `Sync` connector (send_input is a non-blocking datagram push), safe
//! from the Kotlin UI thread. NOT android-gated — send_input exists on the host build too, so these
//! compile everywhere (parity with nativeConnect/nativeClose). The wire codes are the GameStream
//! conventions: buttons 1=left/2=middle/3=right/4=X1/5=X2; scroll axis 0=vertical/1=horizontal,
//! signed 120-unit delta, +=up/right; keys are Windows VK (mapped from KEYCODE_* on the Kotlin side).

use jni::objects::{JByteBuffer, JFloatArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, RichInput, HID_REPORT_MAX, HOST_CAP_PEN, HOST_CAP_TEXT_INPUT,
    PEN_ANGLE_UNKNOWN, PEN_BATCH_MAX, PEN_DISTANCE_UNKNOWN, PEN_TILT_UNKNOWN,
};

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

/// `NativeBridge.nativeTextInputSupported(handle)` — whether the host advertised
/// `HOST_CAP_TEXT_INPUT` (its inject backend types committed text), so the Kotlin side can pick
/// the real IME `InputConnection` over the TYPE_NULL raw-key fallback. `0` handle → false.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeTextInputSupported(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; host_caps is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    u8::from(h.client.host_caps() & HOST_CAP_TEXT_INPUT != 0)
}

/// `NativeBridge.nativeHostSupportsPen(handle)` — the host advertised `HOST_CAP_PEN`, so the
/// Kotlin side splits stylus pointers out of the touch path onto the pen plane
/// (design/pen-tablet-input.md §7). `0` handle → false.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostSupportsPen(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    if handle == 0 {
        return 0;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; host_caps is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    u8::from(h.client.host_caps() & HOST_CAP_PEN != 0)
}

/// Floats per sample in the `nativeSendPen` flat array.
const PEN_JNI_STRIDE: usize = 10;

/// `NativeBridge.nativeSendPen(handle, samples, count)` — one stylus batch of STATE-FULL
/// samples, `count` × [`PEN_JNI_STRIDE`] floats, oldest first:
/// `[state, tool, x, y, pressure, distance, tilt_deg, azimuth_deg, roll_deg, dt_us]`.
/// `state` = the wire `PEN_*` bits; `tool` 0=pen 1=eraser; `x`/`y`/`pressure`/`distance`
/// normalized 0..1; `distance`/`tilt_deg`/`azimuth_deg`/`roll_deg` < 0 = unknown. Call only
/// against a [`nativeHostSupportsPen`] host; the client heartbeats the last sample ≤100 ms
/// while in range (Kotlin side — see `StylusStream`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPen(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
    samples: JFloatArray,
    count: jint,
) {
    if handle == 0 || count <= 0 {
        return;
    }
    let count = (count as usize).min(PEN_BATCH_MAX);
    let mut buf = [0f32; PEN_BATCH_MAX * PEN_JNI_STRIDE];
    let flat = &mut buf[..count * PEN_JNI_STRIDE];
    if env.get_float_array_region(&samples, 0, flat).is_err() {
        return; // short array — a bridge bug, never worth a crash on the input path
    }
    let mut batch = [PenSample::default(); PEN_BATCH_MAX];
    for (slot, s) in batch.iter_mut().zip(flat.chunks_exact(PEN_JNI_STRIDE)) {
        if !s[2].is_finite() || !s[3].is_finite() {
            return; // never forward a NaN coordinate
        }
        *slot = PenSample {
            state: s[0] as u8,
            tool: if s[1] as u8 == 1 {
                PenTool::Eraser
            } else {
                PenTool::Pen
            },
            x: s[2].clamp(0.0, 1.0),
            y: s[3].clamp(0.0, 1.0),
            pressure: (s[4].clamp(0.0, 1.0) * 65535.0) as u16,
            distance: if s[5] < 0.0 {
                PEN_DISTANCE_UNKNOWN
            } else {
                (s[5].clamp(0.0, 1.0) * 65534.0) as u16
            },
            tilt_deg: if s[6] < 0.0 {
                PEN_TILT_UNKNOWN
            } else {
                (s[6].clamp(0.0, 90.0)) as u8
            },
            azimuth_deg: if s[7] < 0.0 {
                PEN_ANGLE_UNKNOWN
            } else {
                (s[7] as u16) % 360
            },
            roll_deg: if s[8] < 0.0 {
                PEN_ANGLE_UNKNOWN
            } else {
                (s[8] as u16) % 360
            },
            dt_us: s[9].clamp(0.0, 65535.0) as u16,
        };
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_pen is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_pen(&batch[..count]);
}

/// `NativeBridge.nativeSendText(handle, text)` — committed IME text, one `TextInput` event per
/// Unicode scalar (`code` = the scalar; multi-char commits are consecutive events in order).
/// Control characters are skipped — Enter/Backspace/Tab ride the VK key path. Call only when
/// [`Java_io_unom_punktfunk_kit_NativeBridge_nativeTextInputSupported`] returned true.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendText(
    mut env: JNIEnv,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    if handle == 0 {
        return;
    }
    let Ok(s) = env.get_string(&text) else {
        return;
    };
    for ch in String::from(s).chars().filter(|c| !c.is_control()) {
        send_event(handle, InputKind::TextInput, ch as u32, 0, 0, 0);
    }
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

/// `NativeBridge.nativeSendPadHidReport(handle, pad, buf, len)` — one raw HID input report from a
/// client-captured controller (the as-is Steam Controller 2 passthrough), forwarded verbatim on
/// the rich-input plane (`RichInput::HidReport`, 0xCC). `buf` is a DIRECT ByteBuffer whose first
/// `len` bytes are the report, id byte first (`0x42`/`0x45`/`0x47` state, `0x43` battery, …);
/// `len` is clamped to the 64-byte wire body. Called from the capture thread at the controller's
/// own report rate (~250–500 Hz) — the direct-buffer read avoids a JNI array copy per report.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadHidReport(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
    pad: jint,
    buf: JByteBuffer,
    len: jint,
) {
    if handle == 0 || len <= 0 {
        return;
    }
    let cap = match env.get_direct_buffer_capacity(&buf) {
        Ok(c) => c,
        Err(_) => return,
    };
    let ptr = match env.get_direct_buffer_address(&buf) {
        Ok(p) if !p.is_null() => p,
        _ => return,
    };
    let n = (len as usize).min(cap).min(HID_REPORT_MAX);
    let mut data = [0u8; HID_REPORT_MAX];
    // SAFETY: `ptr`/`cap` describe the direct ByteBuffer's backing store, valid for this call;
    // `n` is bounded by both the buffer capacity and the fixed wire body.
    data[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(ptr, n) });
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_rich_input is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_rich_input(RichInput::HidReport {
        pad: (pad as u32 & 0xF) as u8,
        len: n as u8,
        data,
    });
}

/// `NativeBridge.nativeSendPadTouch(handle, pad, finger, active, x, y)` — one touchpad contact
/// from a client-captured controller (the Sony USB capture), forwarded on the rich-input plane
/// (`RichInput::Touchpad`, 0xCC). `finger`: contact slot 0/1; `x`/`y`: normalized 0..=65535 in
/// SCREEN convention (+y down — the wire's fixed meaning); `active` 0 lifts the finger. The
/// host's DualSense-family backends scale onto the virtual pad's touch surface. On-change only —
/// the capture diffs, the host holds per-slot state.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadTouch(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    pad: jint,
    finger: jint,
    active: jboolean,
    x: jint,
    y: jint,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_rich_input is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_rich_input(RichInput::Touchpad {
        pad: (pad as u32 & 0xF) as u8,
        finger: (finger as u32 & 0x1) as u8,
        active: active != 0,
        x: (x as i64).clamp(0, 65535) as u16,
        y: (y as i64).clamp(0, 65535) as u16,
    });
}

/// `NativeBridge.nativeSendPadMotion(handle, pad, gp, gy, gr, ax, ay, az)` — one motion sample
/// from a client-captured controller (`RichInput::Motion`, 0xCC): gyro pitch/yaw/roll + accel,
/// raw signed-16 values in the pad's own units, passed straight into the host's virtual
/// DualSense report (the wire is a unit passthrough). Called from the capture thread at the
/// controller's report rate.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadMotion(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    pad: jint,
    gyro_pitch: jint,
    gyro_yaw: jint,
    gyro_roll: jint,
    accel_x: jint,
    accel_y: jint,
    accel_z: jint,
) {
    if handle == 0 {
        return;
    }
    let c = |v: jint| (v as i64).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
    // SAFETY: live handle per the nativeConnect/nativeClose contract; send_rich_input is &self.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let _ = h.client.send_rich_input(RichInput::Motion {
        pad: (pad as u32 & 0xF) as u8,
        gyro: [c(gyro_pitch), c(gyro_yaw), c(gyro_roll)],
        accel: [c(accel_x), c(accel_y), c(accel_z)],
    });
}
