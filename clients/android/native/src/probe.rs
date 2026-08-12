//! JNI seam for the reachability probe: a bounded, trust-agnostic QUIC handshake to a saved host
//! (`punktfunk_core::client::NativeClient::probe`). Like [`crate::wol`] it takes no session handle
//! and links into the host workspace build (pure `jni` + `punktfunk_core`). Kotlin calls it
//! periodically, on a background dispatcher, to light the "online" pip for saved hosts that never
//! advertise on mDNS (reached over Tailscale / VPN / another subnet) — the display-side companion
//! to the dial-first connect fix.

use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint};
use jni::JNIEnv;
use punktfunk_core::client::NativeClient;
use std::time::Duration;

/// `NativeBridge.nativeProbe(host, port, timeoutMs): Boolean` — true if `host:port` completed a
/// QUIC handshake within `timeoutMs`. No pin/identity presented (trust-agnostic), mDNS-independent.
/// Blocking (builds its own runtime) — Kotlin runs it on `Dispatchers.IO`, never the main thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeProbe<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    timeout_ms: jint,
) -> jboolean {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let port = port.clamp(0, u16::MAX as jint) as u16;
    let timeout = Duration::from_millis(timeout_ms.max(0) as u64);
    if NativeClient::probe(&host, port, timeout) {
        1
    } else {
        0
    }
}
