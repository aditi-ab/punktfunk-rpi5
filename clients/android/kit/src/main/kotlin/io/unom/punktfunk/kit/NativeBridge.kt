package io.unom.punktfunk.kit

/**
 * The single JNI seam to `libpunktfunk_android.so` (the Rust-heavy client core).
 *
 * Symbols are implemented in `crates/punktfunk-android`. This object is intentionally thin —
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
     * Connect to a host (trust-on-first-use, anonymous) and return an opaque session handle, or
     * `0` on failure. Pair the handle with exactly one [nativeClose].
     *
     * TODO(M4): pin/identity/pairing, plane pumps (video/audio/rumble/HID), input, mode
     * renegotiation — see `crates/punktfunk-android/src/session.rs`.
     */
    external fun nativeConnect(host: String, port: Int, width: Int, height: Int, refreshHz: Int): Long

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
     * Start host→client audio: Opus decode → jitter ring → AAudio (LowLatency), all in Rust. No-op
     * if already started. Best-effort — a failure leaves video streaming.
     */
    external fun nativeStartAudio(handle: Long)

    /** Stop + join the audio thread and close AAudio, without closing the session. No-op on `0`. */
    external fun nativeStopAudio(handle: Long)

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
