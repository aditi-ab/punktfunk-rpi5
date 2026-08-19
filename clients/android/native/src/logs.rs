//! JNI seam for "Send logs to host": hand Kotlin the client's recent log ring (fed by the
//! [`crate::RingTee`] logcat tee) rendered as one text bundle. The UPLOAD stays on the
//! Kotlin side — its mTLS OkHttp client (`mtlsHttpClient`, the library/art path) already
//! owns HTTPS-to-the-pinned-host on this platform, and `logring::send_to_host`'s ureq
//! agent is deliberately desktop-only. Android-gated (unlike [`crate::wol`]/[`crate::probe`])
//! because `pf-client-core` is an Android-target dependency of this crate.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::EnvUnowned;

/// `NativeBridge.nativeRenderLogs(header): String` — the ring as one text bundle, oldest
/// first, prefixed by `header` (the Kotlin side's identity line) and an eviction note when
/// the ring wrapped. Never empty (the header line is always present); cheap enough for any
/// thread, though the caller is about to do network anyway.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeRenderLogs<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    header: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| {
        let header: String = header.try_to_string(env)?;
        env.new_string(pf_client_core::logring::render(&header))
    })
    .resolve::<LogErrorAndDefault>()
}
