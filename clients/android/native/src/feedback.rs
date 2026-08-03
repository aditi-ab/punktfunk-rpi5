//! Host→client gamepad feedback pulls (Option B): blocking JNI shims that forward to the connector's
//! rumble (0xCA) / HID-output (0xCD) planes and return one decoded event. Kotlin owns the poll
//! threads + the Android Vibrator/Lights rendering (see `GamepadFeedback.kt`) — no JNI upcalls, no
//! `JavaVM` attach, no cached method ids. Mirrors the audio plane's one-thread-per-plane contract,
//! except the thread lives in Kotlin and we just expose the blocking pull.
//!
//! Not android-gated: `next_rumble`/`next_hidout` are pure-Rust on the `quic` feature, so these
//! compile on the host build too (parity with the input shims in [`crate::session`]).

use crate::session::{jni_guard, SessionHandle};
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
const TAG_HID_RAW: u8 = 0x05;

/// `NativeBridge.nativeNextRumble(handle): Long` — block up to ~100 ms for the next EFFECTIVE
/// rumble command from the core's shared policy engine (`design/rumble-root-fix.md` §D). The
/// engine owns ALL rumble policy — v2 lease expiry, legacy-host staleness (a uniform 1 s, ending
/// the old 60 s Android exposure), connection-close drain zeros — so Kotlin applies commands
/// verbatim: `(0, 0)` = cancel now, non-zero = one-shot at this level.
///
/// Returns a packed positive long: bits 49..52 = wire `pad` index (0..15), bits 32..47 = the
/// command's `backstop_ms` (≤ 5000 — the one-shot duration, i.e. the hardware net under a stalled
/// poll thread; the engine emits explicit zeros at every policy stop, so it is never the stop
/// mechanism), bits 16..31 = `low`, bits 0..15 = `high` (0..=0xFFFF). `-1` on timeout / session
/// closed (all packed values are positive, so `-1` stays unambiguous). Kotlin routes the command
/// back to the controller holding that wire `pad` index (multi-pad rumble). Run from a Kotlin
/// poll thread.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextRumble(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jlong {
    // Runs on a Kotlin poll thread, so a panic here would abort the process; guard the boundary.
    jni_guard(-1, || {
        if handle == 0 {
            return -1;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract; next_rumble_command is
        // &self on the Sync connector — safe alongside the decode/audio/input threads. Kotlin
        // stops these poll threads (and joins them — unbounded) before nativeClose frees the
        // handle.
        let h = unsafe { &*(handle as *const SessionHandle) };
        match h.client.next_rumble_command(PULL_TIMEOUT) {
            // A pad whose coils are ACTIVELY being driven by the 0xD1 haptics stream must not see
            // wire rumble: `DsDevice` sets `valid_flag0` bit 1 (`HAPTICS_SELECT`) on every rumble
            // write, and that bit disables the audio-haptics path — so one replayed command would
            // mute the coils the stream is driving. Gating on *arrival of haptics frames* rather
            // than on "a stream is open" is what keeps a rumble-only title working: it renders no
            // haptics audio, so the host emits nothing on 0xD1 and the pad keeps its rumble.
            // Dropping it here rather than in Kotlin keeps the rule next to the reason.
            Ok(cmd) if crate::pad_audio::haptics_owns_coils((cmd.pad & 0xF) as u8) => -1,
            Ok(cmd) => {
                (jlong::from(cmd.pad & 0xF) << 49)
                    | (jlong::from(cmd.backstop_ms.min(0xFFFF) as u16) << 32)
                    | (jlong::from(cmd.low) << 16)
                    | jlong::from(cmd.high)
            }
            Err(_) => -1, // NoFrame (timeout) or Closed — Kotlin loops on its running flag
        }
    })
}

/// `NativeBridge.nativeNextHidout(handle, buf): Int` — block up to ~100 ms for the next DualSense
/// HID-output event, written into the caller's direct ByteBuffer as `[pad][kind][fields…]` (the
/// leading `pad` is the wire pad index the event is addressed to, so Kotlin routes it to that
/// controller — multi-pad HID feedback):
///   Led        → `[pad][0x01][r][g][b]`         (len 5)
///   PlayerLeds → `[pad][0x02][bits]`            (len 3)
///   Trigger    → `[pad][0x03][which][effect…]`  (len 3 + effect.len())
/// Returns the byte count written, or `-1` on timeout / session closed / buffer too small.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextHidout(
    env: JNIEnv,
    _this: JObject,
    handle: jlong,
    buf: JByteBuffer,
) -> jint {
    // Runs on a Kotlin poll thread, so a panic here would abort the process; guard the boundary.
    jni_guard(-1, || {
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

        // out[0] = wire pad index; out[1] = kind tag; the rest is the per-kind payload.
        let n = match ev {
            HidOutput::Led { pad, r, g, b } => {
                if cap < 5 {
                    return -1;
                }
                out[0] = pad;
                out[1] = TAG_LED;
                out[2] = r;
                out[3] = g;
                out[4] = b;
                5
            }
            HidOutput::PlayerLeds { pad, bits } => {
                if cap < 3 {
                    return -1;
                }
                out[0] = pad;
                out[1] = TAG_PLAYER_LEDS;
                out[2] = bits;
                3
            }
            HidOutput::Trigger { pad, which, effect } => {
                let n = 3 + effect.len();
                if cap < n {
                    return -1; // the raw DS5 trigger block is ~11 bytes; Kotlin allocates 64
                }
                out[0] = pad;
                out[1] = TAG_TRIGGER;
                out[2] = which;
                out[3..n].copy_from_slice(&effect);
                n
            }
            HidOutput::TrackpadHaptic { .. } => {
                // Steam Controller trackpad-coil haptics — no Android equivalent; drop it (motor
                // rumble already rides the universal 0xCA plane).
                return -1;
            }
            HidOutput::HidRaw { pad, kind, data } => {
                // As-is SC2 passthrough: the host's hidraw consumer (Steam) wrote this report to
                // the virtual pad; Kotlin replays it verbatim on the physical controller.
                // `[pad][0x05][kind][report…]` — kind 0 = output report, 1 = feature report.
                let n = 3 + data.len();
                if cap < n {
                    return -1; // reports are ≤ 64 bytes; Kotlin allocates 128
                }
                out[0] = pad;
                out[1] = TAG_HID_RAW;
                out[2] = kind;
                out[3..n].copy_from_slice(&data);
                n
            }
            HidOutput::AudioCtl { .. } => {
                // DS5 pad-audio routing/volumes — no Android replay path yet (the 0xD1 sample
                // plane isn't rendered here either); drop it like TrackpadHaptic.
                return -1;
            }
        };
        n as jint
    })
}
