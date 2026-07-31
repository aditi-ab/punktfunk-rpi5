package io.unom.punktfunk

import android.content.Context
import android.os.Build
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
    return withContext(Dispatchers.IO) {
        // Transport-level half of "Low-latency mode (experimental)" (DSCP marking on the media
        // sockets) — must be applied before connect, since sockets are tagged at creation.
        NativeBridge.nativeSetLowLatencyMode(settings.lowLatencyMode)
        NativeBridge.nativeConnect(
            host, port, w, h, hz,
            identity.certPem, identity.privateKeyPem, pinHex,
            settings.bitrateKbps, settings.compositor, gamepadPref,
            hdrEnabled, VideoDecoders.multiSliceTolerant(),
            // Slice-progressive delivery: decoder truth AND the async decode loop — the legacy
            // sync loop feeds whole AUs only, so parts must never arrive when it is selected.
            settings.lowLatencyMode && VideoDecoders.partialFrameCapable(),
            settings.audioChannels,
            // What this device can decode (H.264|HEVC always, AV1 when a real decoder exists) +
            // the user's soft codec preference — the host resolves the emitted codec from both.
            VideoDecoders.decodableCodecBits(), settings.preferredCodec(), timeoutMs,
            launch,
            // The host's approval-list / trust-store label for this device — the same
            // Build.MODEL convention the pairing dialogs use for nativePair.
            Build.MODEL ?: "Android",
        )
    }
}
