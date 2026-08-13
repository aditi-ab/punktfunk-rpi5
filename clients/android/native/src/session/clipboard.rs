//! Shared-clipboard plane (text-only v1): Kotlin drives the Android `ClipboardManager`, these
//! shims drive [`punktfunk_core::client::NativeClient`]'s clipboard surface.
//!
//! Model (mirrors the desktop clients): opt-in via `nativeClipControl(true)`; local copies are
//! announced lazily as format-list offers (`nativeClipOfferText`) and the bytes cross only when
//! the host pastes (a `fetch` event answered by `nativeClipServeText`); a host copy arrives as an
//! `offer` event, which the Kotlin side fetches eagerly (Android's clipboard has no lazy provider
//! path worth the complexity) and lands in the system clipboard on the `data` event.
//!
//! Events cross to Kotlin as compact strings from the blocking `nativeNextClip` poll (drained on
//! a dedicated thread, same pattern as `nativeNextRumble`):
//! `state:<0|1>` · `offer:<seq>:<has_text 0|1>` · `fetch:<req_id>` · `data:<xfer_id>:<text>` ·
//! `cancel:<id>` · `error:<id>:<code>` · `closed` — null on a poll timeout. Non-text fetch
//! requests are cancelled natively (only text is ever offered, so they shouldn't occur).

use std::time::Duration;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::clipboard::ClipEventCore;
use punktfunk_core::error::PunktfunkError;
use punktfunk_core::quic::{ClipKind, CLIP_FILE_INDEX_NONE, HOST_CAP_CLIPBOARD};

use super::SessionHandle;

/// The portable wire MIME both ends map to their platform text type.
const TEXT_MIME: &str = "text/plain;charset=utf-8";

/// Deref the opaque handle (`0` → `None`).
///
/// SAFETY: live handle per the nativeConnect/nativeClose contract; every method used is `&self`
/// on the `Sync` connector.
fn client(handle: jlong) -> Option<&'static SessionHandle> {
    if handle == 0 {
        return None;
    }
    // SAFETY: see the function docs — the Kotlin side guarantees the handle outlives the call.
    Some(unsafe { &*(handle as *const SessionHandle) })
}

/// `NativeBridge.nativeClipSupported(handle)` — the host advertised `HOST_CAP_CLIPBOARD`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipSupported(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    client(handle).is_some_and(|h| h.client.host_caps() & HOST_CAP_CLIPBOARD != 0)
}

/// `NativeBridge.nativeClipControl(handle, enabled)` — session-level opt-in/out. Nothing
/// clipboard-related happens on either side until an `enabled: true` crosses.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipControl(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    enabled: jboolean,
) {
    if let Some(h) = client(handle) {
        let _ = h.client.clip_control(enabled, 0);
    }
}

/// `NativeBridge.nativeClipOfferText(handle, seq)` — announce "the Android clipboard now holds
/// text" (format list only; bytes cross when the host fetches). `seq` is Kotlin's monotonic
/// counter, newest wins.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipOfferText(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    seq: jint,
) {
    if let Some(h) = client(handle) {
        let _ = h.client.clip_offer(
            seq as u32,
            vec![ClipKind {
                mime: TEXT_MIME.into(),
                size_hint: 0,
            }],
        );
    }
}

/// `NativeBridge.nativeClipFetchText(handle, seq)` — pull the text of the host's offer `seq`.
/// Returns the transfer id echoed on the matching `data:`/`error:` event, or −1.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipFetchText(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    seq: jint,
) -> jint {
    client(handle)
        .and_then(|h| {
            h.client
                .clip_fetch(seq as u32, TEXT_MIME.into(), CLIP_FILE_INDEX_NONE)
                .ok()
        })
        .map_or(-1, |xfer| xfer as jint)
}

/// `NativeBridge.nativeClipServeText(handle, reqId, text)` — answer a `fetch:` event with the
/// clipboard's current text (the host is pasting our offer).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipServeText(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    req_id: jint,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = client(handle) else {
            return Ok(());
        };
        // An unreadable payload still has to answer the host's `fetch:` — leaving it unanswered
        // stalls the paste — so cancel the transfer rather than propagating the error.
        let Ok(s) = text.try_to_string(env) else {
            let _ = h.client.clip_cancel(req_id as u32);
            return Ok(());
        };
        let _ = h.client.clip_serve(req_id as u32, s.into_bytes(), true);
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeClipCancel(handle, id)` — abort a transfer (either direction).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClipCancel(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    id: jint,
) {
    if let Some(h) = client(handle) {
        let _ = h.client.clip_cancel(id as u32);
    }
}

/// `NativeBridge.nativeNextClip(handle)` — block ≤250 ms for the next clipboard event, encoded
/// as a compact string (module docs); null on timeout, `"closed"` once the session is gone.
/// Call from a dedicated poll thread.
///
/// Text payloads ride `data:<xfer_id>:<text>` decoded lossily — safe because the phase-0
/// clipboard task delivers a whole payload in ONE event (`last = true`), so a chunk boundary
/// can never split a UTF-8 sequence.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeNextClip<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    // `JString::default()` is the null reference the old `std::ptr::null_mut()` returned, so the
    // "null on timeout" contract in the doc comment above is unchanged.
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let Some(h) = client(handle) else {
            return Ok(JString::default());
        };
        let msg = match h.client.next_clip(Duration::from_millis(250)) {
            Ok(ClipEventCore::State { enabled, .. }) => format!("state:{}", u8::from(enabled)),
            Ok(ClipEventCore::RemoteOffer { seq, kinds }) => {
                let has_text = kinds.iter().any(|k| k.mime.starts_with("text/plain"));
                format!("offer:{seq}:{}", u8::from(has_text))
            }
            Ok(ClipEventCore::FetchRequest { req_id, mime, .. }) => {
                if mime.starts_with("text/plain") {
                    format!("fetch:{req_id}")
                } else {
                    // We only ever offer text; cancel anything else rather than stall the host.
                    let _ = h.client.clip_cancel(req_id);
                    return Ok(JString::default());
                }
            }
            Ok(ClipEventCore::Data { xfer_id, bytes, .. }) => {
                format!("data:{xfer_id}:{}", String::from_utf8_lossy(&bytes))
            }
            Ok(ClipEventCore::Cancelled { id }) => format!("cancel:{id}"),
            Ok(ClipEventCore::Error { id, code }) => format!("error:{id}:{code}"),
            Err(PunktfunkError::NoFrame) => return Ok(JString::default()),
            Err(_) => "closed".into(),
        };
        env.new_string(msg)
    })
    .resolve::<LogErrorAndDefault>()
}
