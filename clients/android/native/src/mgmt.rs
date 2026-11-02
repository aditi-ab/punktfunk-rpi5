//! JNI seam for the management API over the stream port (`punktfunk_core::client::mgmt`), so a
//! paired device reaches a host whose TCP 47990 is closed. Like [`crate::wol`] it takes no
//! session handle and links into the host workspace build.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JObject, JString};
use jni::sys::jint;
use jni::EnvUnowned;
use punktfunk_core::client::mgmt;
use std::time::Duration;

/// `NativeBridge.nativeMgmtRequest(host, port, certPem, keyPem, pinHex, method, path,
/// contentType, body, timeoutMs): ByteArray?` — `method path` with `body` over the stream port
/// as this device, refusing any host but `pinHex`. An empty `contentType` sends none. The reply
/// as two bytes of big-endian status, then its body; null when the port carried nothing, so
/// HTTPS may. Blocking — Kotlin runs it off the main thread.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeMgmtRequest<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin_hex: JString<'local>,
    method: JString<'local>,
    path: JString<'local>,
    content_type: JString<'local>,
    body: JByteArray<'local>,
    timeout_ms: jint,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let host: String = host.try_to_string(env)?;
        let cert: String = cert_pem.try_to_string(env)?;
        let key: String = key_pem.try_to_string(env)?;
        let pin_hex: String = pin_hex.try_to_string(env)?;
        let method: String = method.try_to_string(env)?;
        let path: String = path.try_to_string(env)?;
        let content_type: String = content_type.try_to_string(env)?;
        let body = env.convert_byte_array(&body)?;
        let Some(pin) = punktfunk_core::fp::parse_hex32(&pin_hex) else {
            return Ok(JByteArray::default());
        };
        let port = port.clamp(0, u16::MAX as jint) as u16;
        let wait = Duration::from_millis(timeout_ms.max(0) as u64);
        let req = mgmt::Request {
            method: &method,
            path: &path,
            content_type: Some(content_type.as_str()).filter(|c| !c.is_empty()),
            body: &body,
        };
        match mgmt::request(&host, port, (&cert, &key), Some(pin), wait, &req) {
            Ok(r) => {
                let mut out = r.status.to_be_bytes().to_vec();
                out.extend_from_slice(&r.body);
                env.byte_array_from_slice(&out)
            }
            Err(e) => {
                log::debug!("mgmt over the stream port: {e}");
                Ok(JByteArray::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}
