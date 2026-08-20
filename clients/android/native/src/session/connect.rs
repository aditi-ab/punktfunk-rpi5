//! Connect lifecycle + the trust surface: identity mint, connect (TOFU / pinned), close,
//! host-fingerprint read, and the SPAKE2 PIN pairing ceremony.

use jni::errors::LogErrorAndDefault;
use jni::objects::{JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{hex32, jni_guard, lock_recover, parse_hex32, SessionHandle};

/// Machine token of the most recent `nativeConnect`/`nativePair` failure, taken (and cleared)
/// by `nativeTakeLastError` so Kotlin can render a cause-specific message instead of the old
/// catch-all "wrong PIN, or the host isn't armed" (which blamed the PIN for dead network paths
/// — the moko0878-class support threads). The app runs one attempt at a time, so one slot
/// suffices; a stale token is harmless (it is taken immediately after the failed call).
static LAST_ERROR: Mutex<String> = Mutex::new(String::new());

/// Stable token for a failed pair/connect cause, matched by Kotlin (`ConnectErrors.kt`):
/// a typed host rejection yields its `RejectReason::as_str()` token ("not-armed", "denied",
/// "approval-timeout", …); transport-level causes map to "crypto" / "timeout" / "io" / "error".
fn note_error(e: &punktfunk_core::error::PunktfunkError) {
    use punktfunk_core::error::PunktfunkError as E;
    let token = match e {
        E::Rejected(r) => r.as_str(),
        E::Crypto => "crypto",
        E::Timeout => "timeout",
        E::Io(_) => "io",
        _ => "error",
    };
    *LAST_ERROR.lock().unwrap() = token.to_string();
}

/// `NativeBridge.nativeTakeLastError(): String` — the machine token of the most recent failed
/// `nativeConnect`/`nativePair`, cleared on read (`""` when none). Call right after a `0`
/// handle / `""` fingerprint.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeTakeLastError<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    let token = std::mem::take(&mut *lock_recover(&LAST_ERROR));
    env.with_env(|env| env.new_string(token))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeGenerateIdentity(): String` — mint a fresh persistent self-signed identity.
/// Returns `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on failure (logged). Kotlin
/// persists it (Keystore-wrapped) and only calls this again when the store is genuinely empty.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeGenerateIdentity<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JString<'local> {
    let out = match punktfunk_core::quic::endpoint::generate_identity() {
        Ok((cert, key)) => format!("{cert}\n-----PUNKTFUNK-KEY-----\n{key}"),
        Err(e) => {
            log::error!("nativeGenerateIdentity failed: {e}");
            String::new()
        }
    };
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetLowLatencyMode(enabled)` — apply the user's "Low-latency mode
/// (experimental)" toggle to the process-wide transport defaults, today just DSCP/QoS marking on
/// the media sockets. Must be called BEFORE `nativeConnect` (the tag is applied at socket
/// creation); Kotlin's one connect choke point (`HostConnect.connectToHost`) does. The rest of the
/// toggle rides explicit per-session parameters (`nativeStartVideo` / `nativeStartAudio`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetLowLatencyMode(
    _env: EnvUnowned,
    _this: JObject,
    enabled: jboolean,
) {
    punktfunk_core::transport::set_dscp_default(enabled);
}

/// `debug.punktfunk.force_parts` = 1: arm slice-progressive parts delivery even when the
/// Kotlin `FEATURE_PartialFrame` probe said no — the rebuild-free on-glass experiment for a
/// decoder that may accept `BUFFER_FLAG_PARTIAL_FRAME` without declaring the feature (the NP3's
/// c2.qti decoders declare nothing). Android-only; everywhere else the probe verdict stands.
#[cfg(target_os = "android")]
fn force_parts_sysprop() -> bool {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe {
        libc::__system_property_get(
            c"debug.punktfunk.force_parts".as_ptr(),
            buf.as_mut_ptr().cast(),
        )
    };
    n > 0 && std::str::from_utf8(&buf[..n as usize]).unwrap_or("").trim() == "1"
}

#[cfg(not(target_os = "android"))]
fn force_parts_sysprop() -> bool {
    false
}

/// The rates this session may ask for when the one the user chose will not open, best first.
///
/// **Down the requested rate's own FAMILY, then the 48 kHz floor.** The two families
/// ([`punktfunk_core::audio::pcm::rate_is_supported`]) are 44.1 / 88.2 / 176.4 kHz and 48 / 96 kHz,
/// and within a family the lower rates are the same material at half the samples — a 176.4 kHz
/// interface that will not open is overwhelmingly likely to be an 88.2 or 44.1 kHz one, and asking
/// there next is asking for the rate the endpoint most plausibly runs at.
///
/// **48 kHz terminates every ladder, including the 44.1 family's**, and that crossing is deliberate
/// rather than an oversight. It is the rate every Android output grants, it is the rate this
/// protocol has always run, and the alternative to a 48 kHz *lossless* session is a 48 kHz *Opus*
/// one — the same rate with a lossy stage added. Nothing is resampled by this decision: the host
/// captures at the rate it answers with, or declines (§8.2/§8.3), so a 44.1 kHz-locked endpoint
/// answers a 48 kHz request with Opus rather than with a quiet upsample.
///
/// The requested rate is the first rung, so an openable rate is asked for unchanged and a default
/// session's ladder is one rung long.
fn rate_fallback_ladder(rate_hz: u32) -> &'static [u32] {
    const HZ48: u32 = punktfunk_core::audio::SAMPLE_RATE_HZ;
    match rate_hz {
        176_400 => &[176_400, 88_200, 44_100, HZ48],
        88_200 => &[88_200, 44_100, HZ48],
        44_100 => &[44_100, HZ48],
        96_000 => &[96_000, HZ48],
        // 48 kHz itself, and — via the `rate_is_supported` guard in the caller — nothing else.
        _ => &[HZ48],
    }
}

/// Resolve the audio format this `Hello` should ASK for, from what Kotlin's setting requested —
/// after proving this device can actually open it.
///
/// This is `design/hi-res-audio.md` §7's rule made mechanical: *"a client that cannot open a
/// 96 kHz output must not set `CLIENT_CAP_AUDIO_HIRES`"*. It has to happen here, before the
/// handshake, because after it there is no recovery: AAudio grants an explicitly-requested rate or
/// fails the open (it never substitutes), the host does not renegotiate the plane mid-session
/// (§6), and the only ways to play a wire of one rate through a stream of another are the wrong
/// speed or a resampler nobody asked for — which §9 forbids in as many words ("say so and fall
/// back, not resample quietly"). So the fall back happens where falling back is still free: in the
/// request.
///
/// **Every rung above the floor is probed, and admitting the 44.1 kHz family made that matter
/// more, not less.** When the ladder was 96 → 48 there was one uncertain rate; now there are four,
/// and their odds are nothing alike — 44 100 Hz is granted by very nearly every Android output,
/// 176 400 Hz by very nearly none, and 88 200 Hz by whatever the HAL happens to think. None of
/// that is inferable from the number, so [`crate::audio::output_rate_is_openable`] opens a stream
/// and reads back what it was granted, once per rung, until one holds.
///
/// Dropping the RATE keeps the depth, so a device that refuses the rate still gets a 24-bit
/// lossless session rather than being pushed all the way back to Opus — the depth is where the
/// plane earns its bandwidth anyway (and it is the half that is audible at all: §12).
///
/// The 48 kHz floor is never probed. It is universally supported, and the DEPTH never reaches
/// AAudio at all (the device is opened as f32 on both planes — see `crate::audio`), so there is
/// nothing about 16-vs-24-bit for a probe to discover. An ordinary session therefore opens no
/// stream here and pays nothing.
///
/// # ⚠⚠ "Not asking" is `(0, 0)`, and it is NOT `(48 000, 16)`
///
/// Core's `advertised_client_caps` sets `CLIENT_CAP_AUDIO_HIRES` when **either field is non-zero**
/// — it keys on *the caller specified a format*, not on *the format differs from the default*, and
/// deliberately: 48 kHz/16-bit is the cheapest lossless rung as well as the legacy pair, so a
/// "differs from the default" rule would make it the one rung on the ladder nobody could ask for.
///
/// So returning the legacy-looking `(48 000, 16)` for a user who chose **Standard (Opus)** does not
/// mean "no request" — it advertises the capability, the host's gate accepts 48 kHz/16-bit as a
/// perfectly supported format, and the host then silently gives that user the lossless `0xD3` plane
/// at 1.5 Mbps in place of 256 kbps of Opus. This returned exactly that pair until it was caught by
/// comparing all four clients; the desktop client and every pre-v24 `punktfunk_connect_ex*` send
/// `(0, 0)`, and so does this now.
///
/// ⚠⚠ The reach of that slip grew on 2026-08-17: `PUNKTFUNK_AUDIO_HIRES` went default-ON, so where
/// this used to need a host whose operator had opted in, it now lands on every host that has not
/// deliberately opted out.
///
/// `(0, 0)` is also what keeps the `Hello` byte-identical to a legacy one, because the wire encodes
/// an explicit 48 000/16 the same as absent — the difference lives entirely in the capability bit.
fn resolve_requested_audio_format(rate_hz: u32, bits: u8, channels: u8) -> (u32, u8) {
    const HZ48: u32 = punktfunk_core::audio::SAMPLE_RATE_HZ;
    /// "This session did not ask for the lossless plane" — see the ⚠⚠ section above for why this
    /// is a pair of zeroes and not the legacy 48 000/16.
    const UNSPECIFIED: (u32, u8) = (0, 0);
    // A format core would not carry — including Kotlin's `0`/`0` for the Opus setting — asks for
    // nothing, rather than being an error: the request is a preference, and an unrecognized one
    // must not block a connect. The rate set comes from core rather than being re-expressed here,
    // so the host's gate and every client's request validation cannot drift apart.
    if !punktfunk_core::audio::pcm::depth_is_supported(bits)
        || !punktfunk_core::audio::pcm::rate_is_supported(rate_hz)
    {
        return UNSPECIFIED;
    }
    let granted = rate_fallback_ladder(rate_hz)
        .iter()
        .copied()
        // `HZ48` short-circuits the probe rather than being trusted after one: it is the ladder's
        // floor, so a probe there could only turn a working session into no lossless session at
        // all — and it is the rate a failed probe would have fallen back TO.
        .find(|&hz| hz == HZ48 || audio_rate_is_openable(hz, channels))
        // Unreachable while every ladder ends at `HZ48`; the belt is here so a future rung added
        // above the floor cannot silently produce an unrequestable format.
        .unwrap_or(HZ48);
    if granted != rate_hz {
        log::warn!(
            "audio: this device will not open a {rate_hz} Hz output, so the session asks for {granted} Hz / {bits}-bit instead — the wire is only ever offered a format this client has proved it can play"
        );
    }
    (granted, bits)
}

#[cfg(target_os = "android")]
fn audio_rate_is_openable(rate_hz: u32, channels: u8) -> bool {
    crate::audio::output_rate_is_openable(rate_hz, channels)
}

/// Off-device (the host `cargo build --workspace` leg, where there is no AAudio at all): nothing
/// can be proved, so nothing is claimed. The caller falls back to the legacy rate, which is the
/// safe answer for a build that never runs on a phone anyway.
#[cfg(not(target_os = "android"))]
fn audio_rate_is_openable(_rate_hz: u32, _channels: u8) -> bool {
    false
}

/// `NativeBridge.nativeConnect(host, port, w, h, hz, certPem, keyPem, pinHex, bitrateKbps,
/// compositorPref, gamepadPref, hdrEnabled, audioChannels, audioRateHz, audioBits, preferredCodec,
/// timeoutMs, launch, deviceName): Long`.
/// `launch` (empty ⇒ none) is a store-qualified library id to boot straight into a game.
/// `deviceName` (empty ⇒ none) rides the Hello as `name` — what the host's pending-approval list
/// and trust store show for this device (Kotlin passes `Build.MODEL`, its `nativePair` convention).
/// `certPem`/`keyPem` empty = anonymous, else presented as the persistent identity. `pinHex` empty
/// = TOFU (read `nativeHostFingerprint` after), else 64-hex SHA-256 to pin the host (mismatch → 0).
/// `bitrateKbps` 0 = host default. `compositorPref`/`gamepadPref` are `CompositorPref`/`GamepadPref`
/// wire bytes (0 = Auto; unknown → Auto). `audioChannels` is the requested surround layout (2/6/8;
/// normalized, anything else → stereo) — the host clamps it and the resolved count drives playback.
/// `audioRateHz`/`audioBits` are the audio FORMAT asked for. **`0`/`0` — and anything unrecognized
/// — is "did not ask", the legacy Opus request every build has made**; any pair core can carry asks
/// the host for the lossless `0xD3` plane, `48000`/`16` INCLUDED (that is the cheapest lossless
/// rung, not a spelling of "default" — see [`resolve_requested_audio_format`], which is where
/// getting this backwards silently upgraded every Opus session). Only a request; the host's gate may
/// answer Opus regardless, and this device may not be able to open the rate at all, which is what
/// [`resolve_requested_audio_format`] settles HERE rather than letting the session negotiate a wire
/// it cannot play.
/// `preferredCodec` is the soft codec preference wire byte (0 = Auto). `timeoutMs` is the handshake
/// budget: the normal path passes a short value, the no-PIN "request access" path a long one (≥ the
/// host's approval-park window) so a slow operator approval lands on this same parked connection
/// rather than timing the client out first. Returns an opaque handle, or 0 on failure.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeConnect<'local>(
    mut env: EnvUnowned<'local>,
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
    multi_slice_ok: jboolean,
    frame_parts_ok: jboolean,
    audio_channels: jint,
    audio_rate_hz: jint,
    audio_bits: jint,
    video_codecs: jint,
    preferred_codec: jint,
    timeout_ms: jint,
    launch: JString<'local>,
    device_name: JString<'local>,
    pad_audio_ok: jboolean,
) -> jlong {
    // Every JNI string this method needs, read up front in the one `Env` scope jni 0.22 grants a
    // native method; everything below is pure Rust over owned `String`s. `None` = the mandatory
    // `host` could not be read, which is the old `Err(_) => return 0` arm.
    type ConnectStrings = Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )>;
    let strings: ConnectStrings = env
        .with_env(|env| -> jni::errors::Result<ConnectStrings> {
            let Ok(host) = host.try_to_string(env) else {
                return Ok(None);
            };
            let cert: String = cert_pem.try_to_string(env).unwrap_or_default();
            let key: String = key_pem.try_to_string(env).unwrap_or_default();
            let pin_hex: String = pin_hex.try_to_string(env).unwrap_or_default();
            // A store-qualified library id (`steam:<appid>` / `custom:<id>`) to boot straight into a
            // game; null / empty ⇒ None (a plain desktop connect). Rides the Hello as `launch`.
            let launch: Option<String> = launch
                .try_to_string(env)
                .ok()
                .filter(|s: &String| !s.is_empty());
            // The host's approval-list / trust-store label for this device; null / blank ⇒ None (the
            // host falls back to its fingerprint-derived "device abcd1234" placeholder).
            let device_name: Option<String> = device_name
                .try_to_string(env)
                .ok()
                .map(|s: String| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Ok(Some((host, cert, key, pin_hex, launch, device_name)))
        })
        .resolve::<LogErrorAndDefault>();
    let Some((host, cert, key, pin_hex, launch, device_name)) = strings else {
        return 0;
    };

    let identity: Option<(String, String)> = if cert.is_empty() || key.is_empty() {
        None
    } else {
        Some((cert, key))
    };
    // Slice-progressive parts, by decoder truth (Kotlin's FEATURE_PartialFrame probe) — with a
    // sysprop escape hatch for the on-glass science question the probe can't answer: does the
    // decoder ACTUALLY choke on BUFFER_FLAG_PARTIAL_FRAME input, or does it merely not declare
    // the feature? (`adb shell setprop debug.punktfunk.force_parts 1` + stream restart; a codec
    // that can't take parts errors recoverably and the reanchor gate + keyframe path recovers.)
    let force_parts = force_parts_sysprop();
    let frame_parts = frame_parts_ok || force_parts;
    // The connect-time capability readout (`adb logcat -s pf.caps`): the P2 slice pipeline is
    // inert client-side unless BOTH probes pass — this line is the one place that says which.
    log::info!(
        target: "pf.caps",
        "decoder caps: multi_slice={} partial_frame={}{} hdr={} codec_bits={:#x}",
        multi_slice_ok,
        frame_parts_ok,
        if force_parts { " (FORCED by sysprop)" } else { "" },
        hdr_enabled,
        video_codecs,
    );
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
    // Requested surround layout (2 = stereo / 6 = 5.1 / 8 = 7.1); anything else is stereo. The
    // host clamps it and echoes the resolved count in `connector.audio_channels`, which drives the
    // decoder + AAudio layout (read in `crate::audio::AudioPlayback::start`).
    let audio_channels =
        punktfunk_core::audio::normalize_channels(audio_channels.clamp(0, u8::MAX as jint) as u8);
    // The audio format, downgraded to something this device has PROVED it can open before the
    // `Hello` carries it — see `resolve_requested_audio_format` for why it cannot wait until
    // playback. `clamp` first: a negative jint from a corrupted setting must not wrap into a
    // plausible rate.
    let (audio_rate_hz, audio_bits) = resolve_requested_audio_format(
        audio_rate_hz.max(0) as u32,
        audio_bits.clamp(0, u8::MAX as jint) as u8,
        audio_channels,
    );
    match NativeClient::connect_with_audio_format(
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
        // 10-bit/HDR by panel truth (above) + multi-slice by DECODER truth: Kotlin probes every
        // decoder this device would use (`VideoDecoders.multiSliceTolerant` — Amlogic wedges the
        // whole device on multi-slice AUs, the 0.17.0 field regression) and only then may the
        // host default to >1 slice per frame (its sub-frame readback / the P2 slice pipeline).
        (if hdr_enabled {
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR
        } else {
            0
        }) | (if multi_slice_ok {
            punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE
        } else {
            0
        }),
        audio_channels,
        // The audio format this session ASKS for (resolved above). A non-default pair is what
        // makes core set `CLIENT_CAP_AUDIO_HIRES` in the `Hello` — capable AND the user turned it
        // on, the `VIDEO_CAP_444` precedent — and it is answered by the host re-formatting the
        // wire, so it must never be advertised on a device that cannot open the output. The host
        // may still decline; `connector.audio_codec`/`audio_sample_rate_hz`/`audio_bits` are what
        // actually happened, and `crate::audio` opens the device from those, never from these.
        audio_rate_hz,
        audio_bits,
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
        // No display-volume forwarding from Android yet (the panel tone-maps PQ itself via the
        // Surface dataspace + static metadata) — the host keeps its virtual-display EDID defaults.
        None,
        // No CLIENT_CAP_CURSOR: this client does not render the host cursor locally (no
        // shape/state planes in the jni surface) — advertising it would stream cursor-less.
        // CLIENT_CAP_PHASE_LOCK is honest: the async decode loop's presenter feeds
        // report_phase (advisory in v1 — the host arms on report receipt — but the Hello
        // should say what the client does).
        // CLIENT_CAP_PAD_AUDIO is the SESSION-level negotiation, separate from the per-pad
        // arrival bits: without it the host never sets HOST_CAP_PAD_AUDIO and never emits 0xD1,
        // so declaring a pad's render caps later would have nothing to gate. Gated on the
        // settings so a user with pad audio off does not make the host provision endpoints.
        punktfunk_core::quic::CLIENT_CAP_PHASE_LOCK
            | if pad_audio_ok {
                punktfunk_core::quic::CLIENT_CAP_PAD_AUDIO
            } else {
                0
            },
        // Slice-progressive delivery, by decoder truth (Kotlin probes FEATURE_PartialFrame on
        // every decoder this device would use; `debug.punktfunk.force_parts` overrides for the
        // on-glass experiment): AU prefixes then arrive as `Frame::part` pieces and the decode
        // loop feeds them with BUFFER_FLAG_PARTIAL_FRAME.
        frame_parts,
        launch, // a store-qualified library id to boot into a game, or None for the desktop
        device_name, // Kotlin's Build.MODEL — the host's approval-list / trust-store label
        pin,    // Some → Crypto on host-fp mismatch
        identity, // owned (cert, key) PEM, or None (anonymous)
        // Handshake budget from Kotlin: ~10 s for a normal connect, ~185 s for "request access"
        // (the host parks the connection until the operator approves the device — see ConnectScreen).
        Duration::from_millis(timeout_ms.max(0) as u64),
        // The Kotlin side cancels by dropping the result (`Dial.cancelled`), not by aborting
        // the dial — its connect runs on a pool thread, so a parked one costs a thread, not a
        // stuck UI. Wire a flag through here if that ever stops being true.
        None,
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
                #[cfg(target_os = "android")]
                pad_audio: Mutex::new(None),
                // A fresh session is never muted (mute is per-session UI state, not a setting).
                mic_muted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                access_seq: std::sync::atomic::AtomicU32::new(0),
                // Reported by Kotlin at `surfaceCreated` and on every resize after it.
                surface_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            };
            Box::into_raw(Box::new(handle)) as jlong
        }
        Err(e) => {
            log::error!("nativeConnect to {host}:{port} failed: {e}");
            note_error(&e);
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeClose(
    _env: EnvUnowned,
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeDisconnectQuit(
    _env: EnvUnowned,
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
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostFingerprint<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    handle: jlong,
) -> JString<'local> {
    let out = if handle == 0 {
        String::new()
    } else {
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        hex32(&h.client.host_fingerprint)
    };
    env.with_env(|env| env.new_string(out))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSessionEnded(handle): Boolean` — has the underlying QUIC session ended?
/// `true` once the connection closed (a host suspend / crash / network drop idle-timed it out, or the
/// host closed it) — from then on no more frames arrive and the video sits frozen on its last one.
/// Kotlin's stream watchdog polls this (~1 Hz) to leave a dead stream and return to the menu (where
/// the user can Wake-on-LAN the host) instead of stranding them on a frozen frame. `false` on a `0`
/// handle. Cheap (one atomic load); safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSessionEnded(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        if handle == 0 {
            return false;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        h.client.is_session_ended()
    })
}

/// `NativeBridge.nativeEndReason(handle): Int` — WHY the session ended, as a
/// `punktfunk_core::client::PunktfunkEndReason` byte (Kotlin mirrors it in `SessionEndReason`).
///
/// Companion to `nativeSessionEnded`, which only says THAT it ended. Kotlin's watchdog needs both:
/// the flag to leave a dead stream, and this to decide what — if anything — to tell the user. A
/// player quitting their game and a host dropping off the network both end the session, and until
/// this existed the watchdog worded them identically ("the host may be asleep"), which is wrong for
/// every deliberate ending. `0` (NONE) on a `0` handle or before the session ends. Cheap (one
/// atomic load); safe on the UI thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeEndReason(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jint {
    jni_guard(0, || {
        if handle == 0 {
            return 0;
        }
        // SAFETY: live handle per the nativeConnect/nativeClose contract.
        let h = unsafe { &*(handle as *const SessionHandle) };
        h.client.end_reason() as jint
    })
}

/// `NativeBridge.nativePair(host, port, certPem, keyPem, pin, name): String` — run the SPAKE2 PIN
/// ceremony, presenting our persistent identity. On success returns the host's verified fingerprint
/// (64-hex) to persist + pin; on any failure (wrong PIN / MITM / host reject / unreachable) returns
/// `""` (logged). Blocking — Kotlin calls it off the UI thread.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePair<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    host: JString<'local>,
    port: jint,
    cert_pem: JString<'local>,
    key_pem: JString<'local>,
    pin: JString<'local>,
    name: JString<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let g = |e: &jni::Env<'local>, j: &JString<'local>| -> String {
            j.try_to_string(e).unwrap_or_default()
        };
        let host = g(env, &host);
        let cert = g(env, &cert_pem);
        let key = g(env, &key_pem);
        let pin = g(env, &pin);
        let name = g(env, &name);

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
                    // The token lets Kotlin say WHICH (`nativeTakeLastError`).
                    log::error!("nativePair to {host}:{port} failed: {e}");
                    note_error(&e);
                    String::new()
                }
            }
        };
        env.new_string(out)
    })
    .resolve::<LogErrorAndDefault>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::audio::pcm::{BITS_16, BITS_24};
    use punktfunk_core::audio::SAMPLE_RATE_HZ;

    /// The rule this leg exists to enforce: the `Hello` never asks for an audio format this device
    /// has not proved it can open, because after the handshake there is no way back — AAudio grants
    /// an explicit rate or fails the open, the host does not renegotiate the plane mid-session, and
    /// playing a 96 kHz wire through a 48 kHz stream is not a fallback, it is the wrong audio.
    ///
    /// Off-device (this test's target) `audio_rate_is_openable` answers `false` for everything, so
    /// what is pinned here is the DOWNGRADE, which is the half that has to be right: a device that
    /// cannot do the rate still gets a lossless session at 48 kHz rather than being pushed all the
    /// way back to Opus, and the depth — the thing lossless is actually for — survives.
    #[test]
    fn an_unopenable_rate_is_downgraded_before_the_hello_and_keeps_its_depth() {
        // 48 kHz/16-bit is the cheapest LOSSLESS rung, not a way of spelling "default" — asking
        // for it explicitly passes through and probes nothing. What a default session sends is
        // `(0, 0)`, pinned in `an_opus_session_asks_for_nothing_and_a_lossless_one_asks_for_
        // something`, and conflating the two is what silently upgraded every Opus user.
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_16, 2),
            (SAMPLE_RATE_HZ, BITS_16)
        );
        // 48 kHz is never probed, so 48/24 lossless survives even where nothing can be opened.
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_24, 2),
            (SAMPLE_RATE_HZ, BITS_24)
        );
        // Every rung above the floor IS probed, is refused here, and lands on 48 kHz with the
        // depth intact — including the whole 44.1 kHz family, which this pass admitted. AAudio
        // never substitutes a rate, so a device that would not grant 176 400 Hz and was asked for
        // it anyway is silence, not a slower session.
        for rate in [44_100u32, 88_200, 96_000, 176_400] {
            assert_eq!(
                resolve_requested_audio_format(rate, BITS_24, 2),
                (SAMPLE_RATE_HZ, BITS_24),
                "{rate} Hz should have fallen to 48 kHz and kept 24-bit"
            );
        }
        // Surround asks exactly as stereo does. The lossless plane was stereo-only while a
        // surround frame did not fit a datagram; the frame ladder is channel-aware, the host
        // decides, and this leg's only job is to prove the OUTPUT opens (at the layout it will
        // actually be opened with).
        assert_eq!(
            resolve_requested_audio_format(SAMPLE_RATE_HZ, BITS_24, 6),
            (SAMPLE_RATE_HZ, BITS_24)
        );
    }

    /// A settings string, a profile written by a newer build, or a corrupted preference must never
    /// reach the wire as a format the plane cannot carry — and must never block a connect either.
    /// Every one resolves to the "did not ask" sentinel, which every host can answer.
    ///
    /// The rate set is `pcm::rate_is_supported`'s, not a second copy of it: 44 100 Hz used to sit
    /// in this table because §4.1's integer samples-per-millisecond arithmetic could not express
    /// it, and the day that stopped being true a locally re-expressed set would have kept refusing
    /// it with a stale reason.
    #[test]
    fn an_unrepresentable_request_falls_back_instead_of_failing() {
        for (rate, bits) in [
            (0, 0),               // Kotlin's Opus setting, and its "unset"
            (22_050, BITS_24),    // below the ladder — a rate this protocol never negotiates
            (192_000, BITS_24),   // out by §3's scope decision, not by any arithmetic
            (384_000, BITS_24),   // above anything anyone has asked for
            (SAMPLE_RATE_HZ, 32), // 32-bit float is deliberately not on the wire
            (SAMPLE_RATE_HZ, 8),  // not a depth this plane carries
            (176_400, 32),        // a carried rate cannot rescue an uncarried depth
        ] {
            assert_eq!(
                resolve_requested_audio_format(rate, bits, 2),
                (0, 0),
                "{rate} Hz / {bits}-bit should have asked for nothing at all"
            );
        }
    }

    /// ⚠⚠ **The one that decides whether a user who chose Opus is quietly given 1.5 Mbps of PCM.**
    ///
    /// Core's `advertised_client_caps` sets `CLIENT_CAP_AUDIO_HIRES` when **either** field of the
    /// pair below is non-zero. It keys on "a format was specified" rather than "the format differs
    /// from the default", and deliberately: 48 kHz/16-bit is the legacy pair AND the cheapest
    /// lossless rung, so the other rule would make that rung the one nobody could request.
    ///
    /// The consequence is that `(48 000, 16)` is not a way of saying "default" — it is a request,
    /// the host's gate accepts it as a supported format, and the user who chose Standard gets the
    /// lossless `0xD3` plane instead of Opus on every host that has not opted out of it (which,
    /// since the host gate went default-ON on 2026-08-17, is all of them). Nothing surfaces it: the
    /// settings screen shows what was asked for, and a granted
    /// session and a declined one look identical from there. This function returned that pair until
    /// all four clients were compared against each other.
    ///
    /// The rule is restated here rather than called, because it is private to core; core's own
    /// tests pin the other half of it. What this test owns is that **this** client never hands it a
    /// non-zero pair unless the user asked for one.
    #[test]
    fn an_opus_session_asks_for_nothing_and_a_lossless_one_asks_for_something() {
        // Core's rule, verbatim: `audio_rate_hz != 0 || audio_bits != 0`.
        let asks_for_hires = |(rate_hz, bits): (u32, u8)| rate_hz != 0 || bits != 0;

        assert!(
            !asks_for_hires(resolve_requested_audio_format(0, 0, 2)),
            "the default session must not advertise the capability"
        );
        // Surround changes nothing about it: the plane is negotiated by format, not by layout.
        assert!(!asks_for_hires(resolve_requested_audio_format(0, 0, 8)));

        // And every rung the settings screen offers does ask — otherwise the setting is inert.
        // The 48 kHz rows resolve unprobed; the rest fall to 48 kHz off-device (see above) and
        // still ask, because the fallback keeps the DEPTH and a 24-bit request is a real one.
        for rate in [44_100u32, SAMPLE_RATE_HZ, 88_200, 96_000, 176_400] {
            let wire = resolve_requested_audio_format(rate, BITS_24, 2);
            assert!(
                asks_for_hires(wire),
                "{rate} Hz / 24-bit resolved to {wire:?}, which asks for nothing"
            );
            assert_eq!(wire.1, BITS_24, "the depth must survive every fallback");
        }
    }

    /// The fallback ladder's shape, which decides what a device that refuses the user's rate is
    /// asked for next — and which is the only place this client makes a quality choice on the
    /// user's behalf, so it is worth pinning rather than reading.
    #[test]
    fn the_rate_ladder_descends_its_own_family_and_ends_at_the_48_khz_floor() {
        for rate in [44_100u32, SAMPLE_RATE_HZ, 88_200, 96_000, 176_400] {
            let ladder = rate_fallback_ladder(rate);
            assert_eq!(ladder[0], rate, "{rate} Hz must ask for itself first");
            assert_eq!(
                ladder.last().copied(),
                Some(SAMPLE_RATE_HZ),
                "{rate} Hz must end at the floor every Android output grants"
            );
            for &rung in ladder {
                assert!(
                    punktfunk_core::audio::pcm::rate_is_supported(rung),
                    "{rung} Hz is on {rate} Hz's ladder but is not a rate the plane carries"
                );
            }
            // Strictly descending, so a fallback is never an upgrade in cost — except for the
            // 48 kHz floor itself, which is above 44 100 and is the crossing the ladder makes on
            // purpose (see `rate_fallback_ladder`).
            for w in ladder.windows(2) {
                assert!(
                    w[1] < w[0] || w[1] == SAMPLE_RATE_HZ,
                    "{rate} Hz's ladder goes up at {:?}",
                    w
                );
            }
        }
        // The 44.1 family stays in the 44.1 family for as long as it can: a 176.4 kHz endpoint
        // that will not open is far likelier to be an 88.2 or 44.1 kHz one than a 96 kHz one.
        assert_eq!(
            rate_fallback_ladder(176_400),
            &[176_400, 88_200, 44_100, SAMPLE_RATE_HZ]
        );
        assert_eq!(rate_fallback_ladder(96_000), &[96_000, SAMPLE_RATE_HZ]);
        // A default session's ladder is one rung, so it opens no probe stream at all.
        assert_eq!(rate_fallback_ladder(SAMPLE_RATE_HZ), &[SAMPLE_RATE_HZ]);
    }
}
