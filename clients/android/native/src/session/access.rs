//! The session's access level over JNI — the Android leg of `design/per-client-access.md` §7.
//!
//! One poll shim: Kotlin reads `[grants, remainingSecs, updateSeq]` ~1 Hz (alongside its
//! session-ended watchdog) instead of holding a blocking event thread — access news is a
//! console edit or an expiry warning, a handful per session, and every gate the mask drives
//! re-checks within a second anyway. The connector already folds each mid-session
//! [`punktfunk_core::quic::AccessUpdate`] latest-wins into its live grants/deadline slots;
//! the seq counter here only exists so the poller can tell a FRESH update arrived (the host's
//! T−5 m / T−1 m warnings owe a toast) without diffing state that a warning doesn't change.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JIntArray, JObject};
use jni::sys::jlong;
use jni::EnvUnowned;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::SessionHandle;

/// `NativeBridge.nativeAccessState(handle): IntArray?` — the live access state as
/// `[grants, remainingSecs, updateSeq]`; `null` on a `0` handle. `grants` is the
/// `GRANT_GAMEPAD`-family bitmask, seeded from the Welcome's advert (an old host reads as
/// `GRANT_ALL` — full control, today's behavior); `remainingSecs` counts down to the access
/// deadline on the CLIENT's clock (`0` = permanent, clamped to ≥ 1 once a deadline exists so
/// the sentinel can never be reached by counting); `updateSeq` increments once per
/// `AccessUpdate` drained from the connector's event plane. Not android-gated — pure `jni` +
/// connector reads, so it links on the host build too. Cheap; safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeAccessState<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        if handle == 0 {
            return Ok(JIntArray::default());
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        // Drain the event plane into the seq counter. The connector's grants/deadline slots
        // are already the latest-wins fold when an event lands — the events carry no state
        // this read doesn't get below, they are purely the "something arrived" cue. Zero
        // timeout: this is the UI thread's poll, it must never park.
        while h.client.next_access_update(Duration::ZERO).is_ok() {
            h.access_seq.fetch_add(1, Ordering::Relaxed);
        }
        let remaining: u64 = match h.client.access_deadline_unix() {
            None => 0, // permanent
            Some(deadline) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                // ≥ 1 once a deadline exists: 0 is the "permanent" sentinel, and a deadline
                // already past with the session still up (the host's typed close is in
                // flight) must keep reading as "about to end", never flip to "forever".
                deadline.saturating_sub(now).max(1)
            }
        };
        let buf: [i32; 3] = [
            h.client.access_grants() as i32,
            remaining.min(i32::MAX as u64) as i32,
            h.access_seq.load(Ordering::Relaxed) as i32,
        ];
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}
