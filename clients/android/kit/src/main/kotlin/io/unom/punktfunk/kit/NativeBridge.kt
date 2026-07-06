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
     * `CompositorPref`/`GamepadPref` wire bytes (0 = Auto). [timeoutMs] is the handshake budget — the
     * normal path passes a short value, the no-PIN "request access" path a long one (≥ the host's
     * approval-park window) so a slow operator approval lands on this same parked connection. Returns
     * an opaque session handle, or `0` on failure. Pair with exactly one [nativeClose].
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
        audioChannels: Int,
        /** Preferred video codec as a `quic::CODEC_*` bit (`0` = auto). Soft — the host falls back. */
        preferredCodec: Int,
        timeoutMs: Int,
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

    // ---- LAN discovery: mDNS browse of `_punktfunk._udp` in Rust (mdns-sd), polled by Kotlin ----
    // Replaces NsdManager. The caller holds the Wi-Fi MulticastLock for the browse lifetime; raw
    // multicast *reception* needs it. See io.unom.punktfunk.kit.discovery.HostDiscovery.

    /**
     * Start browsing `_punktfunk._udp` on the LAN. Returns an opaque discovery handle, or `0` on
     * failure. Pair with exactly one [nativeDiscoveryStop]. Cheap + non-blocking (spawns the mDNS
     * daemon + a fold thread).
     */
    external fun nativeDiscoveryStart(): Long

    /**
     * The current resolved-host snapshot for [handle]: newline-joined records, each
     * `key␟name␟addr␟port␟fp␟pair␟mac` (`␟` = U+001F). Empty string = no hosts / `0` handle. Poll ~1 Hz;
     * cheap (a lock + string build), safe to call on the main thread.
     */
    external fun nativeDiscoveryPoll(handle: Long): String

    /** Stop the browse, shut the mDNS daemon down and join its thread. No-op on `0`. */
    external fun nativeDiscoveryStop(handle: Long)

    /**
     * Send a Wake-on-LAN magic packet to wake a sleeping host. [macsCsv] is comma-separated MAC
     * addresses (`aa:bb:..,cc:dd:..`), learned from the host's mDNS `mac` TXT while it was online;
     * [lastIp] is the host's last-known IPv4 (or empty). Returns true if at least one datagram was
     * sent. No handle — callable without a live session. Do NOT call on the main thread (it does
     * blocking socket sends); run it on a background dispatcher.
     */
    external fun nativeWakeOnLan(macsCsv: String, lastIp: String): Boolean

    /**
     * The MediaCodec MIME the host resolved for this session (`"video/hevc"` / `"video/avc"` /
     * `"video/av01"`), or `""` on a `0` handle. Kotlin ranks `MediaCodecList` decoders for this
     * MIME (see [io.unom.punktfunk.kit.VideoDecoders]) before [nativeStartVideo]. Cheap; UI-safe.
     */
    external fun nativeVideoMime(handle: Long): String

    /**
     * Start the decode thread rendering onto [surface] (a SurfaceView's surface). Decode runs
     * entirely in Rust (NDK AMediaCodec → ANativeWindow) — no per-frame JNI. [decoderName] is the
     * decoder Kotlin ranked from `MediaCodecList` (`""` = let the platform resolve the default for
     * the MIME); [lowLatencyMode] is the user's master toggle (default on → aggressive per-SoC
     * tuning; off → conservative); [lowLatencyFeature] is whether [decoderName] advertised
     * `FEATURE_LowLatency` (HUD label only). [isTv] drives an active HDMI mode switch to the stream
     * refresh on TV boxes (vs. the softer seamless hint on phones). No-op if already started.
     */
    external fun nativeStartVideo(
        handle: Long,
        surface: android.view.Surface,
        decoderName: String,
        lowLatencyMode: Boolean,
        lowLatencyFeature: Boolean,
        isTv: Boolean,
    )

    /** Stop + join the decode thread without closing the session. No-op on `0`. */
    external fun nativeStopVideo(handle: Long)

    /**
     * The resolved decoder identity for the HUD, e.g. `c2.qti.avc.decoder · low-latency`, or `""`
     * before the decode thread has resolved one. One-shot (fixed for the session); poll once after
     * the HUD appears.
     */
    external fun nativeVideoDecoderLabel(handle: Long): String

    /**
     * Drain ~1 s of live decode stats for the on-stream HUD, or `null` when no decode thread runs.
     * Returns 18 doubles (unified stats spec, `design/stats-unification.md`):
     * `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skewCorrected, width, height, refreshHz, framesLost,
     * bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms,
     * netP50Ms]`
     * (the two flags are 1.0/0.0; indexes 2/3 are the end-to-end capture→decoded headline; 10–13
     * describe the negotiated video feed — bit depth 8/10, CICP primaries/transfer, and the HEVC
     * chroma_format_idc 1=4:2:0 / 3=4:4:4; 14/15 are the stage p50s tiling the headline —
     * `host+network` = capture→received, `decode` = received→decoded; 16/17 split the
     * `host+network` term via the host's per-AU 0xCF timings — `host` = the host's capture→sent,
     * `network` = the remainder — both 0.0 when no timing matched this window, i.e. an old host).
     * Poll ~1 Hz; each call resets the measurement window.
     */
    external fun nativeVideoStats(handle: Long): DoubleArray?

    /**
     * Gate per-frame stats sampling on the HUD being visible: while disabled the decode thread
     * skips the per-AU clock read + lock, so toggle this with the overlay (and only poll
     * [nativeVideoStats] while it's on). Enabling resets the measurement window — no stale data.
     * Sticky for the session (survives video stop/start). No-op on `0`.
     */
    external fun nativeSetVideoStatsEnabled(handle: Long, enabled: Boolean)

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

    /**
     * Absolute mouse position — the host moves the cursor to (x, y) in a [surfaceWidth]×[surfaceHeight]
     * pixel space (it normalizes against that size and maps into the output region). Touch
     * "direct pointing": the cursor jumps to the finger. Parity with the Apple client's absolute touch.
     */
    external fun nativeSendPointerAbs(handle: Long, x: Int, y: Int, surfaceWidth: Int, surfaceHeight: Int)

    /** One mouse-button transition. button: 1=left 2=middle 3=right 4=X1 5=X2. */
    external fun nativeSendPointerButton(handle: Long, button: Int, down: Boolean)

    /** One scroll step. axis: 0=vertical 1=horizontal. delta: signed, 120-scaled, +=up/right. */
    external fun nativeSendScroll(handle: Long, axis: Int, delta: Int)

    /**
     * One REAL touchscreen transition (the touch-passthrough input mode). [kind]: 0=down 1=move
     * 2=up. [id] distinguishes fingers and is reusable after up; coordinates are pixels on the
     * client's touch surface — the host rescales against [surfaceWidth]×[surfaceHeight] and
     * injects a real touch contact. On up only [id] matters.
     */
    external fun nativeSendTouch(
        handle: Long,
        id: Int,
        kind: Int,
        x: Int,
        y: Int,
        surfaceWidth: Int,
        surfaceHeight: Int,
    )

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
