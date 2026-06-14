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
}
