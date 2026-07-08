//! Connect lifecycle + the trust surface: identity mint, connect (TOFU / pinned), close,
//! host-fingerprint read, and the SPAKE2 PIN pairing ceremony.

use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{hex32, jni_guard, parse_hex32, SessionHandle};

/// `NativeBridge.nativeGenerateIdentity(): String` — mint a fresh persistent self-signed identity.
/// Returns `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on failure (logged). Kotlin
/// persists it (Keystore-wrapped) and only calls this again when the store is genuinely empty.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeGenerateIdentity<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
) -> jni::sys::jstring {
    let out = match punktfunk_core::quic::endpoint::generate_identity() {
        Ok((cert, key)) => format!("{cert}\n-----PUNKTFUNK-KEY-----\n{key}"),
        Err(e) => {
            log::error!("nativeGenerateIdentity failed: {e}");
            String::new()
        }
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

/// `NativeBridge.nativeSetLowLatencyMode(enabled)` — apply the user's "Low-latency mode
/// (experimental)" toggle to the process-wide transport defaults, today just DSCP/QoS marking on
/// the media sockets. Must be called BEFORE `nativeConnect` (the tag is applied at socket
/// creation); Kotlin's one connect choke point (`HostConnect.connectToHost`) does. The rest of the
/// toggle rides explicit per-session parameters (`nativeStartVideo` / `nativeStartAudio`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetLowLatencyMode(
    _env: JNIEnv,
    _this: JObject,
    enabled: jboolean,
) {
    punktfunk_core::transport::set_dscp_default(enabled != 0);
}

/// `NativeBridge.nativeConnect(host, port, w, h, hz, certPem, keyPem, pinHex, bitrateKbps,
/// compositorPref, gamepadPref, hdrEnabled, audioChannels, preferredCodec, timeoutMs, launch): Long`.
/// `launch` (empty ⇒ none) is a store-qualified library id to boot straight into a game.
/// `certPem`/`keyPem` empty = anonymous, else presented as the persistent identity. `pinHex` empty
/// = TOFU (read `nativeHostFingerprint` after), else 64-hex SHA-256 to pin the host (mismatch → 0).
/// `bitrateKbps` 0 = host default. `compositorPref`/`gamepadPref` are `CompositorPref`/`GamepadPref`
/// wire bytes (0 = Auto; unknown → Auto). `audioChannels` is the requested surround layout (2/6/8;
/// normalized, anything else → stereo) — the host clamps it and the resolved count drives playback.
/// `preferredCodec` is the soft codec preference wire byte (0 = Auto). `timeoutMs` is the handshake
/// budget: the normal path passes a short value, the no-PIN "request access" path a long one (≥ the
/// host's approval-park window) so a slow operator approval lands on this same parked connection
/// rather than timing the client out first. Returns an opaque handle, or 0 on failure.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    width: jint,
    height: jint,
    refresh_hz: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin_hex: JString<'local>,
    bitrate_kbps: jint,
    compositor_pref: jint,
    gamepad_pref: jint,
    hdr_enabled: jboolean,
    audio_channels: jint,
    video_codecs: jint,
    preferred_codec: jint,
    timeout_ms: jint,
    launch: JString<'local>,
) -> jlong {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(_) => return 0,
    };
    let cert: String = env
        .get_string(&cert_pem)
        .map(Into::into)
        .unwrap_or_default();
    let key: String = env.get_string(&key_pem).map(Into::into).unwrap_or_default();
    let pin_hex: String = env.get_string(&pin_hex).map(Into::into).unwrap_or_default();
    // A store-qualified library id (`steam:<appid>` / `custom:<id>`) to boot straight into a game;
    // null / empty ⇒ None (a plain desktop connect). Rides the Hello as `launch`.
    let launch: Option<String> = env
        .get_string(&launch)
        .map(Into::into)
        .ok()
        .filter(|s: &String| !s.is_empty());

    let identity: Option<(String, String)> = if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some((cert, key))
    };
    let pin: Option<[u8; 32]> = if pin_hex.is_empty() {
        None
    } else {
        match parse_hex32(&pin_hex) {
            Some(fp) => Some(fp),
            None => {
                log::error!("nativeConnect: bad pin hex (len {})", pin_hex.len());
                return 0;
            }
        }
    };
    let mode = Mode {
        width: width as u32,
        height: height as u32,
        refresh_hz: refresh_hz as u32,
    };
    match NativeClient::connect(
        &host,
        port as u16,
        mode,
        CompositorPref::from_u8(compositor_pref.clamp(0, u8::MAX as jint) as u8),
        GamepadPref::from_u8(gamepad_pref.clamp(0, u8::MAX as jint) as u8),
        bitrate_kbps.max(0) as u32, // 0 = host default
        // Advertise 10-bit + HDR ONLY when this device's display can actually present it (Kotlin
        // checks Display.getHdrCapabilities() and passes the result): the host (e.g. Windows) then
        // upgrades to a Main10 / BT.2020 PQ encode. On an SDR display we advertise 0 so the host
        // sends a proper 8-bit BT.709 stream rather than PQ the panel would mis-tone-map. AMediaCodec
        // decodes Main10 from the SPS and the decode loop signals the Surface HDR dataspace + static
        // metadata (see crate::decode).
        if hdr_enabled != 0 {
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR
        } else {
            0
        },
        // Requested surround layout (2 = stereo / 6 = 5.1 / 8 = 7.1). The host clamps to what it can
        // capture and echoes the resolved count in `connector.audio_channels`, which drives the
        // decoder + AAudio layout (read in `crate::audio::AudioPlayback::start`). Anything else
        // normalizes to stereo here.
        punktfunk_core::audio::normalize_channels(audio_channels.clamp(0, u8::MAX as jint) as u8),
        // Codecs this device can decode, ranked on the Kotlin side (`VideoDecoders.decodableCodecBits`:
        // H.264 + HEVC always, AV1 when a real `video/av01` decoder exists — AMediaCodec is
        // mime-driven, see `codec_mime`). Mask to the known bits and fall back to the pre-AV1
        // H.264|HEVC pair on 0 so a bogus value can't advertise nothing and kill the handshake.
        // The host resolves the emitted codec from these + the soft `preferred_codec` and echoes it
        // in `connector.codec`, which drives the mime below.
        {
            let bits = (video_codecs.clamp(0, u8::MAX as jint) as u8)
                & (punktfunk_core::quic::CODEC_H264
                    | punktfunk_core::quic::CODEC_HEVC
                    | punktfunk_core::quic::CODEC_AV1);
            if bits == 0 {
                punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_HEVC
            } else {
                bits
            }
        },
        preferred_codec.clamp(0, u8::MAX as jint) as u8,
        launch,   // a store-qualified library id to boot into a game, or None for the desktop
        pin,      // Some → Crypto on host-fp mismatch
        identity, // owned (cert, key) PEM, or None (anonymous)
        // Handshake budget from Kotlin: ~10 s for a normal connect, ~185 s for "request access"
        // (the host parks the connection until the operator approves the device — see ConnectScreen).
        Duration::from_millis(timeout_ms.max(0) as u64),
    ) {
        Ok(client) => {
            let handle = SessionHandle {
                client: Arc::new(client),
                stats: Arc::new(crate::stats::VideoStats::new()),
                video: Mutex::new(None),
                #[cfg(target_os = "android")]
                audio: Mutex::new(None),
                #[cfg(target_os = "android")]
                mic: Mutex::new(None),
            };
            Box::into_raw(Box::new(handle)) as jlong
        }
        Err(e) => {
            log::error!("nativeConnect to {host}:{port} failed: {e}");
            0
        }
    }
}

/// `NativeBridge.nativeClose(handle)` — drop the session (stops the decode thread, then RAII-tears
/// down the connector). No-op on `0`.
///
/// # Safety contract
/// `handle` must be `0` or a live handle from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect`],
/// closed exactly once and not concurrently with other calls on the same handle (Kotlin owns this).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: per the contract, `handle` is a live `Box<SessionHandle>` pointer.
            unsafe { drop(Box::from_raw(handle as *mut SessionHandle)) };
        }
    })
}

/// `NativeBridge.nativeDisconnectQuit(handle)` — signal a DELIBERATE user quit before `nativeClose`,
/// so the session closes with `QUIT_CLOSE_CODE` and the host tears it down immediately instead of
/// holding the keep-alive linger for a reconnect. Call from an explicit disconnect action only (a
/// plain drop / app-background keeps the linger). The handle is only BORROWED (not freed). No-op on `0`.
///
/// # Safety contract
/// `handle` must be `0` or a live handle from [`Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect`],
/// not freed / closed concurrently with this call (Kotlin still owns it and closes it via `nativeClose`).
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDisconnectQuit(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) {
    jni_guard((), || {
        if handle != 0 {
            // SAFETY: per the contract, `handle` is a live `Box<SessionHandle>` — we only borrow it
            // (no drop), so it stays owned by Kotlin for the later `nativeClose`.
            let sh = unsafe { &*(handle as *const SessionHandle) };
            sh.client.disconnect_quit();
        }
    })
}

/// `NativeBridge.nativeHostFingerprint(handle): String` — the SHA-256 (64-hex) of the cert the host
/// presented on this connection. Valid after a successful `nativeConnect`; Kotlin pins it on a TOFU
/// connect. `""` on a `0` handle.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostFingerprint<'local>(
    env: JNIEnv<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> jni::sys::jstring {
    let out = if handle == 0 {
        String::new()
    } else {
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        hex32(&h.client.host_fingerprint)
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}

/// `NativeBridge.nativeSessionEnded(handle): Boolean` — has the underlying QUIC session ended?
/// `true` once the connection closed (a host suspend / crash / network drop idle-timed it out, or the
/// host closed it) — from then on no more frames arrive and the video sits frozen on its last one.
/// Kotlin's stream watchdog polls this (~1 Hz) to leave a dead stream and return to the menu (where
/// the user can Wake-on-LAN the host) instead of stranding them on a frozen frame. `false` on a `0`
/// handle. Cheap (one atomic load); safe on the UI thread.
#[no_mangle]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSessionEnded(
    _env: JNIEnv,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(0, || {
        if handle == 0 {
            return 0;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        jboolean::from(h.client.is_session_ended())
    })
}

/// `NativeBridge.nativePair(host, port, certPem, keyPem, pin, name): String` — run the SPAKE2 PIN
/// ceremony, presenting our persistent identity. On success returns the host's verified fingerprint
/// (64-hex) to persist + pin; on any failure (wrong PIN / MITM / host reject / unreachable) returns
/// `""` (logged). Blocking — Kotlin calls it off the UI thread.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePair<'local>(
    mut env: JNIEnv<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin: JString<'local>,
    name: JString<'local>,
) -> jni::sys::jstring {
    let g = |e: &mut JNIEnv<'local>, j: &JString<'local>| -> String {
        e.get_string(j).map(Into::into).unwrap_or_default()
    };
    let host = g(&mut env, &host);
    let cert = g(&mut env, &cert_pem);
    let key = g(&mut env, &key_pem);
    let pin = g(&mut env, &pin);
    let name = g(&mut env, &name);

    let out = if host.is_empty() || cert.is_empty() || key.is_empty() {
        log::error!("nativePair: missing host/identity");
        String::new()
    } else {
        match NativeClient::pair(
            &host,
            port as u16,
            (&cert, &key), // borrowed identity
            &pin,
            &name,
            Duration::from_secs(60),
        ) {
            Ok(host_fp) => hex32(&host_fp),
            Err(e) => {
                // Crypto error == wrong PIN / MITM; anything else == transport/host reject.
                log::error!("nativePair to {host}:{port} failed: {e}");
                String::new()
            }
        }
    };
    match env.new_string(out) {
        Ok(s) => s.into_raw(),
        Err(_) => JObject::null().into_raw(),
    }
}
