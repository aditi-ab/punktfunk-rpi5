//! The bandwidth speed test over an established session.
//!
//! The host bursts filler over the real data plane — the same path a stream uses, so the answer is
//! about the link the stream will actually take rather than about some generic throughput. It is
//! deliberately *measure-only*: which layer a measured bitrate belongs in (the global default, or a
//! host's bound profile) is a decision the UI makes with the user
//! (design/client-settings-profiles.md §5.3), never one this shim makes for them.
//!
//! Two calls: start, then poll. Both are cheap and non-blocking, so Kotlin can drive them from a
//! coroutine on the main thread the way it polls the stats HUD.

use super::{jni_guard, SessionHandle};
use jni::objects::JObject;
use jni::sys::{jboolean, jdoubleArray, jint, jlong};
use jni::JNIEnv;

/// The `DoubleArray` [`Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult`] returns. Kept in
/// one place because Kotlin indexes it positionally; see the Kotlin doc for the field order.
const PROBE_RESULT_LEN: usize = 6;

/// `NativeBridge.nativeSpeedTest(handle, targetKbps, durationMs): Boolean` — ask the host to burst
/// filler at `targetKbps` of goodput for `durationMs` (each clamped host-side to ≤ 3 Gbps / ≤ 5 s),
/// **briefly pausing video**. Non-blocking: poll
/// [`Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult`] until its `done` element is 1.
/// Starting a probe resets any prior measurement. `false` on a `0` handle or a closed session.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSpeedTest(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
    target_kbps: jint,
    duration_ms: jint,
) -> jboolean {
    jni_guard(0, || {
        if handle == 0 {
            return 0;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        let target = target_kbps.clamp(0, i32::MAX) as u32;
        let duration = duration_ms.clamp(0, i32::MAX) as u32;
        match h.client.request_probe(target, duration) {
            Ok(()) => 1,
            Err(e) => {
                log::warn!("speed test: could not ask the host to probe: {e:?}");
                0
            }
        }
    })
}

/// `NativeBridge.nativeProbeResult(handle): DoubleArray?` — the current measurement, partial until
/// `[0] == 1`. Safe to poll; before any probe it reports zeros. `null` on a `0` handle.
///
/// Layout (doubles so one array carries both the counts and the percentages):
/// `[done, throughputKbps, lossPct, hostDropPct, elapsedMs, recvBytes]`.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeProbeResult<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jdoubleArray {
    jni_guard(JObject::null().into_raw(), || {
        if handle == 0 {
            return JObject::null().into_raw();
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        let r = h.client.probe_result();
        let values: [f64; PROBE_RESULT_LEN] = [
            f64::from(u8::from(r.done)),
            f64::from(r.throughput_kbps),
            f64::from(r.loss_pct),
            f64::from(r.host_drop_pct),
            f64::from(r.elapsed_ms),
            r.recv_bytes as f64,
        ];
        match env.new_double_array(PROBE_RESULT_LEN as i32) {
            Ok(arr) => {
                if env.set_double_array_region(&arr, 0, &values).is_err() {
                    return JObject::null().into_raw();
                }
                arr.into_raw()
            }
            Err(_) => JObject::null().into_raw(),
        }
    })
}
