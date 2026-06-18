//! Host→client gamepad feedback pulls (Option B): blocking JNI shims that forward to the connector's
//! rumble (0xCA) / HID-output (0xCD) planes and return one decoded event. Kotlin owns the poll
//! threads + the Android Vibrator/Lights rendering (see `GamepadFeedback.kt`) — no JNI upcalls, no
//! `JavaVM` attach, no cached method ids. Mirrors the audio plane's one-thread-per-plane contract,
//! except the thread lives in Kotlin and we just expose the blocking pull.
//!
//! Not android-gated: `next_rumble`/`next_hidout` are pure-Rust on the `quic` feature, so these
//! compile on the host build too (parity with the input shims in [`crate::session`]).

use crate::session::SessionHandle;
use jni::objects::{JByteBuffer, JObject};
use jni::sys::{jint, jlong};
use jni::JNIEnv;
use punktfunk_core::quic::HidOutput;
use std::time::Duration;

/// Short blocking timeout: long enough not to busy-spin, short enough that the Kotlin poll thread
/// observes its `running=false` flag promptly on teardown.
const PULL_TIMEOUT: Duration = Duration::from_millis(100);

// HID-output kind tags written into the returned ByteBuffer (Kotlin reads them back).
const TAG_LED: u8 = 0x01;
const TAG_PLAYER_LEDS: u8 = 0x02;
const TAG_TRIGGER: u8 = 0x03;

/// `NativeBridge.nativeNextRumble(handle): Long` — block up to ~100 ms for the next rumble update.
/// Returns `(low << 16) | high` (each 0..=0xFFFF; `0` = stop), or `-1` on timeout / session closed.
/// Pad index is dropped (single-pad model). Run from a dedicated Kotlin poll thread.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextRumble(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jlong {
    if handle == 0 {
        return -1;
    }
    // SAFETY: live handle per the nativeConnect/nativeClose contract; next_rumble is &self on the
    // Sync connector — safe alongside the decode/audio/input threads. Kotlin stops these poll
    // threads (and joins them) before nativeClose frees the handle.
    let h = unsafe { &*(handle as *const SessionHandle) };
    match h.client.next_rumble(PULL_TIMEOUT) {
        Ok((_pad, low, high)) => (jlong::from(low) << 16) | jlong::from(high),
        Err(_) => -1, // NoFrame (timeout) or Closed — Kotlin loops on its running flag
    }
}

/// `NativeBridge.nativeNextHidout(handle, buf): Int` — block up to ~100 ms for the next DualSense
/// HID-output event, written into the caller's direct ByteBuffer as `[kind][fields…]`:
///   Led        → `[0x01][r][g][b]`         (len 4)
///   PlayerLeds → `[0x02][bits]`            (len 2)
///   Trigger    → `[0x03][which][effect…]`  (len 2 + effect.len())
/// Returns the byte count written, or `-1` on timeout / session closed / buffer too small.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextHidout(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
    buf: JByteBuffer,
) -> jint {
    if handle == 0 {
        return -1;
    }
    // SAFETY: live handle per the contract; next_hidout is &self on the Sync connector.
    let h = unsafe { &*(handle as *const SessionHandle) };
    let ev = match h.client.next_hidout(PULL_TIMEOUT) {
        Ok(ev) => ev,
        Err(_) => return -1, // timeout or closed — Kotlin loops
    };

    // The caller passes a direct ByteBuffer (allocateDirect) so we write its backing store directly.
    let cap = match env.get_direct_buffer_capacity(&buf) {
        Ok(c) => c,
        Err(_) => return -1,
    };
    let ptr = match env.get_direct_buffer_address(&buf) {
        Ok(p) if !p.is_null() => p,
        _ => return -1,
    };
    // SAFETY: `ptr`/`cap` describe the direct ByteBuffer's backing store, valid for this call.
    let out = unsafe { std::slice::from_raw_parts_mut(ptr, cap) };

    let n = match ev {
        HidOutput::Led { r, g, b, .. } => {
            if cap < 4 {
                return -1;
            }
            out[0] = TAG_LED;
            out[1] = r;
            out[2] = g;
            out[3] = b;
            4
        }
        HidOutput::PlayerLeds { bits, .. } => {
            if cap < 2 {
                return -1;
            }
            out[0] = TAG_PLAYER_LEDS;
            out[1] = bits;
            2
        }
        HidOutput::Trigger { which, effect, .. } => {
            let n = 2 + effect.len();
            if cap < n {
                return -1; // the raw DS5 trigger block is ~11 bytes; Kotlin allocates 64
            }
            out[0] = TAG_TRIGGER;
            out[1] = which;
            out[2..n].copy_from_slice(&effect);
            n
        }
    };
    n as jint
}
