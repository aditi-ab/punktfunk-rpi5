//! The host's forwarded cursor over JNI. Kotlin's `HostCursor` thread pulls the shape on
//! change and the newest per-frame state, and tells the host who draws the pointer. Live only
//! when `CLIENT_CAP_CURSOR` met `HOST_CAP_CURSOR`.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JIntArray, JObject};
use jni::sys::{jboolean, jlong};
use jni::EnvUnowned;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::quic::HOST_CAP_CURSOR;
use std::time::Duration;

use super::{jni_guard, SESSIONS};
use crate::feedback::{idle_pull, PULL_TIMEOUT};

/// `NativeBridge.nativeCursorChannel(handle)` — the host forwards its cursor instead of
/// compositing it. `0` handle → false.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeCursorChannel(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|h| h.client.host_caps() & HOST_CAP_CURSOR != 0)
    })
}

/// `NativeBridge.nativeNextCursorState(handle): IntArray?` — the newest `0xD0` as
/// `[serial, flags, x, y]`, the hotspot in frame pixels. Waits up to 100 ms; `null` when none
/// came, which is all the host sends while it composites.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextCursorState<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(idle_pull(JIntArray::default()));
        };
        let mut st = match h.client.next_cursor_state(PULL_TIMEOUT) {
            Ok(st) => st,
            Err(PunktfunkError::Closed) => return Ok(idle_pull(JIntArray::default())),
            Err(_) => return Ok(JIntArray::default()),
        };
        while let Ok(newer) = h.client.next_cursor_state(Duration::ZERO) {
            st = newer;
        }
        let buf = [st.serial as i32, st.flags.into(), st.x, st.y];
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeNextCursorShape(handle): IntArray?` — one queued shape as
/// `[serial, w, h, hotX, hotY, pixels…]`, the pixels unpremultiplied ARGB for
/// `Bitmap.createBitmap(int[], …)`. Never waits; `null` when the queue is empty.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextCursorShape<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JIntArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JIntArray<'local>> {
        let Some(shape) = SESSIONS
            .get(handle)
            .and_then(|h| h.client.next_cursor_shape(Duration::ZERO).ok())
        else {
            return Ok(JIntArray::default());
        };
        let mut buf = vec![
            shape.serial as i32,
            shape.w.into(),
            shape.h.into(),
            shape.hot_x.into(),
            shape.hot_y.into(),
        ];
        buf.extend(argb(&shape.rgba));
        let arr = env.new_int_array(buf.len())?;
        arr.set_region(env, 0, &buf)?;
        Ok(arr)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetCursorRender(handle, clientDraws)` — `true`: the host leaves the
/// pointer out of the picture and forwards it; `false`: the host composites it. Latest-wins.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetCursorRender(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    client_draws: jboolean,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            let _ = h.client.set_cursor_render(client_draws);
        }
    })
}

/// Straight RGBA8 → the ARGB ints Android's bitmaps take.
fn argb(rgba: &[u8]) -> impl Iterator<Item = i32> + '_ {
    rgba.chunks_exact(4)
        .map(|p| i32::from_be_bytes([p[3], p[0], p[1], p[2]]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn argb_moves_alpha_to_the_top_byte() {
        let px: Vec<i32> = super::argb(&[0x10, 0x20, 0x30, 0xFF, 1, 2, 3, 0]).collect();
        assert_eq!(px, [0xFF10_2030_u32 as i32, 0x0001_0203]);
    }
}
