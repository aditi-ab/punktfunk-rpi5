package io.unom.punktfunk

import android.content.Context
import android.util.Log
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.security.ClientIdentity
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** Handshake budget for a normal / library-launch connect (not the long request-access park). */
const val CONNECT_TIMEOUT_MS = 10_000

/**
 * The one place [NativeBridge.nativeConnect] is assembled — shared by [ConnectScreen] and the library
 * launcher ([LibraryScreen]). Derives the mode / HDR / gamepad settings the host needs from
 * [settings]. [pinHex] is the pinned fingerprint (empty ⇒ TOFU). [launch] is a store-qualified library
 * id (`steam:<appid>` / `custom:<id>`) to boot straight into a game, or `null` for the desktop.
 * Returns the session handle, or `0` on failure. Call off the main thread.
 */
suspend fun connectToHost(
    context: Context,
    settings: Settings,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    launch: String?,
    timeoutMs: Int = CONNECT_TIMEOUT_MS,
): Long {
    // Advertise HDR only when the user enabled it AND this device's display can present it (else the
    // host sends a proper SDR stream rather than PQ the panel would mis-tone-map).
    val (baseW, baseH, hz) = settings.effectiveMode(context)
    // Render scale: ask the host for `chosen mode × scale` (even + codec-clamped) — > 1 supersamples
    // (the compositor downscales the larger decoded frame to the SurfaceView), < 1 renders under
    // native. 1.0 leaves the resolved mode untouched.
    val (w, h) = RenderScale.apply(
        baseW, baseH, settings.renderScale, RenderScale.maxDimension(settings.codec)
    )
    val hdrEnabled = settings.hdrEnabled && displaySupportsHdr(context)
    // "Automatic" resolves to a concrete pad type from the connected controller's VID/PID.
    val gamepadPref = Gamepad.resolvePref(settings.gamepad)
    // The requested audio format as the two Hello fields. Stereo only — a lossless surround frame
    // does not fit one QUIC datagram at the default MTU and this plane is never fragmented, so the
    // host would decline; asking anyway would only spend a probe and a decline line. (The settings
    // screen hides the picker on 5.1/7.1 for the same reason, but a profile can still carry a
    // lossless choice into a surround session, and this is where the two settings meet.)
    val (audioRateHz, audioBits) =
        if (settings.audioChannels == 2) settings.audioFormatWire() else 48_000 to 16
    return withContext(Dispatchers.IO) {
        // Transport-level half of "Low-latency mode (experimental)" (DSCP marking on the media
        // sockets) — must be applied before connect, since sockets are tagged at creation.
        NativeBridge.nativeSetLowLatencyMode(settings.lowLatencyMode)
        val multiSlice = VideoDecoders.multiSliceTolerant()
        val partialFrame = VideoDecoders.partialFrameCapable()
        // Slice-progressive delivery: decoder truth AND the async decode loop — the legacy
        // sync loop feeds whole AUs only, so parts must never arrive when it is selected.
        val frameParts = settings.lowLatencyMode && partialFrame
        val codecBits = VideoDecoders.decodableCodecBits()
        // Automatic codec (P5, measured NP3 ↔ RTX 4090): AV1 beat HEVC by ~1.2 ms end-to-end at
        // identical conditions, so under "Automatic" this device prefers AV1 when it hardware-
        // decodes it (the AV1 bit is only ever set for a real, non-blocked hardware decoder) AND
        // it lacks FEATURE_PartialFrame — a partial-frame device keeps HEVC, whose slice overlap
        // AV1 cannot ride (AV1 has no slices; the host's chunked poll never arms). The host
        // honors the preference only inside the probed shared codec set, so an AV1-less encoder
        // still resolves HEVC. An explicit user choice always wins unchanged.
        val preferredCodec = settings.preferredCodec().takeIf { it != 0 }
            ?: if (codecBits and 4 != 0 && !partialFrame) 4 else 0
        // The connect-time capability readout (`adb logcat -s pf.caps`): the P2 slice pipeline
        // is client-inert unless BOTH probes pass — this line says which decoder failed one.
        Log.i(
            "pf.caps",
            VideoDecoders.capsReport() +
                " → multiSlice=$multiSlice parts=$frameParts prefer=$preferredCodec" +
                " (lowLatency=${settings.lowLatencyMode})",
        )
        NativeBridge.nativeConnect(
            host, port, w, h, hz,
            identity.certPem, identity.privateKeyPem, pinHex,
            settings.bitrateKbps, settings.compositor, gamepadPref,
            hdrEnabled, multiSlice,
            frameParts,
            settings.audioChannels,
            // The audio format this session asks for. Only ever a request: the host's five-condition
            // gate may resolve it back to Opus, and the native side downgrades it first if AAudio on
            // this device will not open the rate — a rate the wire has committed to cannot be
            // rescued afterwards, so the fallback has to happen before the Hello.
            audioRateHz, audioBits,
            // What this device can decode (H.264|HEVC always, AV1 when a real decoder exists) +
            // the soft codec preference (user choice, or the Automatic AV1 rule above) — the
            // host resolves the emitted codec from both.
            codecBits, preferredCodec, timeoutMs,
            launch,
            // The host's approval-list / trust-store label for this device — the same
            // user-set device name the pairing dialogs offer for nativePair.
            deviceName(context),
            // Tier-A pad audio: ask for the 0xD1 plane only when a setting would render it, so a
            // user with it off does not make the host provision endpoints it will never feed.
            settings.padHaptics || settings.padSpeaker,
        )
    }
}
