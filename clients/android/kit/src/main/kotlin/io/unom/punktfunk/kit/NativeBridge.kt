package io.unom.punktfunk.kit

/**
 * The single JNI seam to `libpunktfunk_android.so` (the Rust-heavy client core).
 *
 * Symbols are implemented in `clients/android/native`. This object is intentionally thin —
 * all protocol logic lives in Rust (`punktfunk-core` + the connector); Kotlin only marshals.
 */
object NativeBridge {
    init {
        System.loadLibrary("punktfunk_android")
    }

    /** punktfunk-core C-ABI version. A successful call proves the native library is linked. */
    external fun abiVersion(): Int

    /** punktfunk-core crate version string. */
    external fun coreVersion(): String

    /**
     * Mint a fresh persistent self-signed identity, returned as
     * `"<certPem>\n-----PUNKTFUNK-KEY-----\n<keyPem>"`, or `""` on error. Kotlin persists it
     * (Keystore-wrapped via `IdentityStore`) and only calls this again when the store is empty.
     */
    external fun nativeGenerateIdentity(): String

    /**
     * Connect, presenting [certPem]/[keyPem] (both empty = anonymous) and pinning [pinHex] (empty =
     * trust-on-first-use — read [nativeHostFingerprint] after; else 64-hex host SHA-256, mismatch →
     * `0`). [width]/[height]/[refreshHz] are the requested virtual-output mode (the host streams at
     * exactly this); [bitrateKbps] 0 = host default; [compositorPref]/[gamepadPref] are the
     * `CompositorPref`/`GamepadPref` wire bytes (0 = Auto). Returns an opaque session handle, or `0`
     * on failure. Pair with exactly one [nativeClose].
     */
    external fun nativeConnect(
        host: String,
        port: Int,
        width: Int,
        height: Int,
        refreshHz: Int,
        certPem: String,
        keyPem: String,
        pinHex: String,
        bitrateKbps: Int,
        compositorPref: Int,
        gamepadPref: Int,
        hdrEnabled: Boolean,
    ): Long

    /** 64-hex SHA-256 of the cert the host presented on [handle]; valid after a successful connect. */
    external fun nativeHostFingerprint(handle: Long): String

    /**
     * Run the SPAKE2 PIN ceremony, presenting [certPem]/[keyPem]. Returns the host's verified
     * fingerprint (64-hex) to persist + pin, or `""` on failure (wrong PIN / MITM / unreachable).
     * Blocking — call off the main thread.
     */
    external fun nativePair(
        host: String,
        port: Int,
        certPem: String,
        keyPem: String,
        pin: String,
        name: String,
    ): String

    /** Tear down a session handle returned by [nativeConnect]. No-op on `0`. */
    external fun nativeClose(handle: Long)

    /**
     * Start the HEVC decode thread rendering onto [surface] (a SurfaceView's surface). Decode runs
     * entirely in Rust (NDK AMediaCodec → ANativeWindow) — no per-frame JNI. No-op if already started.
     */
    external fun nativeStartVideo(handle: Long, surface: android.view.Surface)

    /** Stop + join the decode thread without closing the session. No-op on `0`. */
    external fun nativeStopVideo(handle: Long)

    /**
     * Drain ~1 s of live decode stats for the on-stream HUD, or `null` when no decode thread runs.
     * Returns 10 doubles:
     * `[fps, mbps, latP50Ms, latP95Ms, latValid, skewCorrected, width, height, refreshHz, framesDropped]`
     * (the two flags are 1.0/0.0). Poll ~1 Hz; each call resets the measurement window.
     */
    external fun nativeVideoStats(handle: Long): DoubleArray?

    /**
     * Start host→client audio: Opus decode → jitter ring → AAudio (LowLatency), all in Rust. No-op
     * if already started. Best-effort — a failure leaves video streaming.
     */
    external fun nativeStartAudio(handle: Long)

    /** Stop + join the audio thread and close AAudio, without closing the session. No-op on `0`. */
    external fun nativeStopAudio(handle: Long)

    /**
     * Start mic uplink: AAudio input → Opus (48 kHz stereo, 20 ms) → host (`send_mic` / 0xCB), all in
     * Rust. No-op if already running. The caller MUST hold RECORD_AUDIO; otherwise the AAudio input
     * stream fails to open and the rest of the session keeps streaming.
     */
    external fun nativeStartMic(handle: Long)

    /** Stop + join the mic thread and close the AAudio input stream. No-op on `0`. */
    external fun nativeStopMic(handle: Long)

    // ---- Input: Kotlin captures, Rust forwards to the host (send_input) ----

    /** Relative mouse move; dx/dy are device-pixel deltas (screen +y down). */
    external fun nativeSendPointerMove(handle: Long, dx: Int, dy: Int)

    /** One mouse-button transition. button: 1=left 2=middle 3=right 4=X1 5=X2. */
    external fun nativeSendPointerButton(handle: Long, button: Int, down: Boolean)

    /** One scroll step. axis: 0=vertical 1=horizontal. delta: signed, 120-scaled, +=up/right. */
    external fun nativeSendScroll(handle: Long, axis: Int, delta: Int)

    /** One key transition. vk: Windows VK (0 = dropped by Rust). mods: VK modifier mask (0 for now). */
    external fun nativeSendKey(handle: Long, vk: Int, down: Boolean, mods: Int)

    // ---- Gamepad: one pad forwarded as pad 0 (Rust hardcodes flags=0) ----

    /** One gamepad button transition. bit: a [Gamepad].BTN_* bit. down: press/release. */
    external fun nativeSendGamepadButton(handle: Long, bit: Int, down: Boolean)

    /** One gamepad axis update. axisId: [Gamepad].AXIS_* (0..5). value: stick i16 (+y=up) / trigger 0..255. */
    external fun nativeSendGamepadAxis(handle: Long, axisId: Int, value: Int)

    // ---- Host→client gamepad feedback: Rust pulls block ~100ms, Kotlin renders (see GamepadFeedback) ----

    /**
     * Block up to ~100 ms for the next rumble update. Returns `(low shl 16) or high` (each
     * 0..0xFFFF; 0 = stop), or -1 on timeout / session closed. Call from a dedicated poll thread.
     */
    external fun nativeNextRumble(handle: Long): Long

    /**
     * Block up to ~100 ms for the next DualSense HID-output event, written into [buf] (a direct
     * ByteBuffer, capacity >= 64) as `[kind][fields…]`: Led=01 r g b, PlayerLeds=02 bits,
     * Trigger=03 which effect…. Returns the byte count, or -1 on timeout / session closed.
     */
    external fun nativeNextHidout(handle: Long, buf: java.nio.ByteBuffer): Int
}
