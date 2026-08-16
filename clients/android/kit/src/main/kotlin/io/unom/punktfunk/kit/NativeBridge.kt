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
        /** Every decoder this device would use tolerates multi-slice AUs
         *  ([VideoDecoders.multiSliceTolerant]) — advertises `VIDEO_CAP_MULTI_SLICE`; false keeps
         *  the host at single-slice frames (the safe pre-0.17 wire shape). */
        multiSliceOk: Boolean,
        /** Every decoder this device would use accepts partial-frame input
         *  ([VideoDecoders.partialFrameCapable]) — opts into slice-progressive delivery (the
         *  decode loop then feeds slices with `BUFFER_FLAG_PARTIAL_FRAME` as they arrive). */
        framePartsOk: Boolean,
        audioChannels: Int,
        /** Requested audio sample rate: **`0` (with [audioBits] `0`) for the legacy Opus plane**, or
         *  any rung of the lossless ladder — `44100`, `48000`, `88200`, `96000`, `176400`, both rate
         *  families.
         *
         *  ⚠⚠ **`48000`/`16` is NOT "the default" — it is the cheapest lossless rung.** Core sets
         *  `CLIENT_CAP_AUDIO_HIRES` when either field is non-zero (it keys on "a format was
         *  specified", so that 48/16 lossless is requestable at all), and the host's gate accepts
         *  48 kHz/16-bit as a supported format. Passing it as a stand-in for "unset" opts every
         *  session into the `0xD3` plane on any host with `PUNKTFUNK_AUDIO_HIRES=1`. Send `0`/`0`.
         *
         *  A request on BOTH counts. The host runs its gate (its own `PUNKTFUNK_AUDIO_HIRES` switch
         *  among them, plus whether a frame of this format fits one datagram at all) and may answer
         *  Opus; and the native side first proves THIS device can open the rate — AAudio grants an
         *  explicit rate or fails the open, and there is no recovery once the wire is negotiated —
         *  walking a fallback ladder and downgrading the request if it cannot. */
        audioRateHz: Int,
        /** Requested audio sample depth: `0` alongside a `0` [audioRateHz] for the legacy Opus
         *  plane, else `16` or `24`. See [audioRateHz] for why `16` is a request rather than a
         *  default; 24-bit is where lossless earns its bandwidth. */
        audioBits: Int,
        /** `quic::CODEC_*` bitfield of codecs this device decodes ([VideoDecoders.decodableCodecBits]);
         *  `0` falls back to H.264|HEVC. The host resolves the emitted codec from this ∩ its GPU. */
        videoCodecs: Int,
        /** Preferred video codec as a `quic::CODEC_*` bit (`0` = auto). Soft — the host falls back. */
        preferredCodec: Int,
        timeoutMs: Int,
        /** Store-qualified library id (`steam:<appid>` / `custom:<id>`) to boot straight into a game,
         *  or `null`/empty for a plain desktop connect. Rides the Hello as `launch`. */
        launch: String?,
        /** This device's display name (rides the Hello as `name`) — what the host's pending-approval
         *  list and trust store show for it, same convention as [nativePair]'s `name`. `null`/blank ⇒
         *  the host falls back to a fingerprint-derived "device abcd1234" label. */
        deviceName: String?,
        /** Advertise `CLIENT_CAP_PAD_AUDIO` — the SESSION-level negotiation for the 0xD1 per-pad
         *  DualSense plane. Without it the host never sets `HOST_CAP_PAD_AUDIO` and emits nothing,
         *  so a captured pad's own render capabilities would have nothing to gate. */
        padAudioOk: Boolean,
    ): Long

    /** 64-hex SHA-256 of the cert the host presented on [handle]; valid after a successful connect. */
    external fun nativeHostFingerprint(handle: Long): String

    /**
     * Has the underlying QUIC session ended? `true` once the connection closed — a host suspend /
     * crash / network drop idle-timed it out (~8 s), or the host closed it — from then on no frame
     * ever arrives and the video sits frozen on its last one. The stream watchdog polls this (~1 Hz)
     * to leave a dead stream and return to the menu, where the user can Wake-on-LAN the host, instead
     * of stranding them on a frozen frame. `false` on a `0` handle. Cheap (one atomic load); UI-safe.
     */
    external fun nativeSessionEnded(handle: Long): Boolean

    /**
     * WHY the session ended, as a [SessionEndReason] ordinal — decode with
     * [SessionEndReason.fromNative]. `0` (NONE) before it ends, or on a `0` handle.
     *
     * The companion to [nativeSessionEnded], which only says THAT it ended. Both are needed: the
     * flag to leave a dead stream, this to decide what to tell the user. A player quitting their
     * game and a host falling off the network both end the session, and with no way to separate
     * them the watchdog said "the host may be asleep" for all of them — wrong for every deliberate
     * ending. Cheap (one atomic load); UI-safe.
     */
    external fun nativeEndReason(handle: Long): Int

    /**
     * The session's live access state as `[grants, remainingSecs, updateSeq]`, or `null` on a `0`
     * handle. `grants` is a [SessionAccess] bitmask; `remainingSecs` counts down to the access
     * expiry (`0` = permanent); `updateSeq` increments once per `AccessUpdate` the host sent
     * (latest-wins — the state IS the fold, this counter is how a poller tells a fresh T−5 m /
     * T−1 m warning arrived and owes a toast). Seeded from the Welcome's access advert; an old
     * host — or an old native lib — reads as full control, permanent, exactly what such a host
     * enforces. Poll ~1 Hz alongside [nativeSessionEnded]. Cheap; safe on the UI thread.
     */
    external fun nativeAccessState(handle: Long): IntArray?

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

    /**
     * The machine token of the most recent failed [nativeConnect]/[nativePair], cleared on read
     * (`""` when none) — call right after a `0` handle / `""` fingerprint. A typed host rejection
     * yields its wire token ("not-armed", "denied", "approval-timeout", "superseded", "busy",
     * "rate-limited", "bound-other", "identity-required", "wire-version"); transport-level causes
     * yield "crypto" (wrong PIN / identity mismatch), "timeout", "io", or "error". Lets the UI say
     * WHY instead of the old catch-all that blamed the PIN for dead network paths.
     */
    external fun nativeTakeLastError(): String

    /**
     * Signal a **deliberate** user disconnect on [handle] before [nativeClose]: the session closes
     * with `QUIT_CLOSE_CODE` so the host tears it down immediately instead of holding the keep-alive
     * linger for a reconnect. Call from an explicit disconnect gesture only — NOT from a
     * host-ended/network-drop end or an app-background (those keep the linger). No-op on `0`.
     */
    external fun nativeDisconnectQuit(handle: Long)

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
     * Bounded, trust-agnostic QUIC reachability probe to [host]:[port] (mDNS-independent): true if
     * the host completed the handshake within [timeoutMs]. No pin/identity presented. Lets a saved
     * host reached over a routed network (Tailscale/VPN/another subnet) — which never advertises on
     * mDNS — still show as online. Blocking (builds its own runtime) — run on a background
     * dispatcher, never the main thread.
     */
    external fun nativeProbe(host: String, port: Int, timeoutMs: Int): Boolean

    /**
     * Start a bandwidth speed test on [handle]: the host bursts filler over the real data plane at
     * [targetKbps] of goodput for [durationMs] (each clamped host-side to ≤ 3 Gbps / ≤ 5 s),
     * **briefly pausing video**. Measuring over the stream's own path is the point — the answer is
     * about the link this host's stream will take, not about generic throughput.
     *
     * Non-blocking: poll [nativeProbeResult] until it reports done. Starting a probe resets any
     * prior measurement. Returns false on a dead handle. Cheap; safe on the main thread.
     */
    external fun nativeSpeedTest(handle: Long, targetKbps: Int, durationMs: Int): Boolean

    /**
     * The current speed-test measurement, partial until `[0] != 0.0`:
     * `[done, throughputKbps, lossPct, hostDropPct, elapsedMs, recvBytes]`. Zeros before any
     * probe, null on a dead handle. Cheap (one lock + a copy); safe to poll on the main thread.
     */
    external fun nativeProbeResult(handle: Long): DoubleArray?

    /**
     * Apply the user's "Low-latency mode (experimental)" toggle to the process-wide transport
     * defaults — today just DSCP/QoS marking on the media sockets. Must be called BEFORE
     * [nativeConnect] (the tag is applied at socket creation); `HostConnect.connectToHost` does.
     * The rest of the toggle rides explicit per-session parameters ([nativeStartVideo] /
     * [nativeStartAudio]). Cheap (one atomic store); UI-safe.
     */
    external fun nativeSetLowLatencyMode(enabled: Boolean)

    /**
     * The MediaCodec MIME the host resolved for this session (`"video/hevc"` / `"video/avc"` /
     * `"video/av01"`), or `""` on a `0` handle. Kotlin ranks `MediaCodecList` decoders for this
     * MIME (see [io.unom.punktfunk.kit.VideoDecoders]) before [nativeStartVideo]. Cheap; UI-safe.
     */
    external fun nativeVideoMime(handle: Long): String

    /**
     * The negotiated video mode as `[width, height, refreshHz]`, or `null` on a `0` handle.
     * Resolved at the handshake, so it is known before the first frame — the stream view sizes
     * itself to THIS aspect rather than stretching the picture to the panel's, and pins the
     * panel's display mode to the stream refresh. The trailing `refreshHz` was appended later
     * (an older native lib returns only `[width, height]` — index defensively). Fixed for the
     * session; read once. Cheap; UI-safe.
     */
    external fun nativeVideoSize(handle: Long): IntArray?

    /**
     * A short human label for the codec the host resolved (`"H.264"` / `"HEVC"` / `"AV1"` /
     * `"PyroWave"`), for the stats HUD's video-feed line, or `""` on a `0` handle. Distinct from
     * [nativeVideoMime] because the MIME collapses PyroWave onto `video/hevc` and can't name it.
     * Fixed for the session (resolved at the handshake); read once. Cheap; UI-safe.
     */
    external fun nativeVideoCodecLabel(handle: Long): String

    /**
     * Start the decode thread rendering onto [surface] (a SurfaceView's surface). Decode runs
     * entirely in Rust (NDK AMediaCodec → ANativeWindow) — no per-frame JNI. [decoderName] is the
     * decoder Kotlin ranked from `MediaCodecList` (`""` = let the platform resolve the default for
     * the MIME — what the pre-overhaul client always did); [lowLatencyMode] is the user's
     * "Low-latency mode" master toggle (ON by default: async loop + per-SoC tuning; off runs the
     * original synchronous pipeline as the per-device escape hatch); [lowLatencyFeature] is whether
     * [decoderName] advertised `FEATURE_LowLatency` (HUD label only). [isTv] drives an active HDMI
     * mode switch to the stream refresh on TV boxes when the toggle is on (vs. the softer seamless
     * hint otherwise). [presentPriority]/[smoothBuffer] are the timeline presenter's intent
     * (0 = lowest latency / 1 = smoothness; buffer 0 = automatic, else 1..3 frames) — the Apple
     * client's `present_priority`/`smooth_buffer` pair. No-op if already started.
     */
    external fun nativeStartVideo(
        handle: Long,
        surface: android.view.Surface,
        decoderName: String,
        lowLatencyMode: Boolean,
        lowLatencyFeature: Boolean,
        isTv: Boolean,
        presentPriority: Int,
        smoothBuffer: Int,
        /** The display mode's own refresh rate (0 = unknown) — the latch grid the presenter
         *  subdivides onto when the platform down-rates the app's choreographer stream. */
        panelFps: Int,
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
     * Returns 38 doubles (unified stats spec, `design/stats-unification.md`):
     * `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skewCorrected, width, height, refreshHz, framesLost,
     * bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms,
     * netP50Ms, lostWindow, skippedWindow, fecWindow, framesWindow, dispValid, displayP50Ms,
     * e2eDispP50Ms, e2eDispP95Ms, paceP50Ms, latchP50Ms, presentsWindow, presenterActive,
     * feedP50Ms, codecP50Ms, skippedOverflowWindow, audioBufferMs, audioAvOffsetMs, audioCodec,
     * audioRateHz, audioBits]`
     * (the flags are 1.0/0.0; indexes 2/3 are the end-to-end capture→decoded headline; 10–13
     * describe the negotiated video feed — bit depth 8/10, CICP primaries/transfer, and the HEVC
     * chroma_format_idc 1=4:2:0 / 3=4:4:4; 14/15 are the stage p50s tiling the headline —
     * `host+network` = capture→received, `decode` = received→decoded; 16/17 split the
     * `host+network` term via the host's per-AU 0xCF timings — `host` = the host's capture→sent,
     * `network` = the remainder — both 0.0 when no timing matched this window, i.e. an old host;
     * 18–21 are the per-window reliability counters — lost/skipped/FEC/received; 22–25 are the
     * `display` stage from the OnFrameRendered render timestamps — when `dispValid` is 1.0 the
     * headline becomes the directly-measured capture→displayed pair at 24/25, tiled by
     * `host+network` + `decode` + `display` (23), and when 0.0 the HUD falls back to the
     * capture→decoded headline at 2/3 without the `display` term; 26–29 split the `display`
     * term the timeline presenter owns — `pace` = decoded→release, `latch` = release→displayed,
     * the window's on-glass confirm count, and whether the presenter is active at all; 30/31
     * split `decode` (15) the same way — `feed` = received→queued (hand-off + input-slot wait),
     * `codec` = queued→decoded, the decoder's own time; 32 is the parked-AU overflow subset of
     * `skipped` (19), i.e. the decoder falling behind rather than benign newest-wins pacing;
     * 33/34 are the AUDIO plane — the playback ring's live depth in ms and the A/V sync loop's
     * smoothed offset in ms, positive meaning audio plays BEHIND the picture. Those two are live
     * gauges, not windowed samples, and the offset reads 0 until the loop has a video reference;
     * 35–37 are the audio FORMAT the host RESOLVED at the handshake — `audioCodec` 0 = Opus on
     * `0xC9`, 2 = lossless PCM on `0xD3` — plus the resolved rate in Hz and depth in bits. Static
     * for the session, and separate from 33/34 because they answer a different question: not "how
     * late is the audio" but "is this the format the user asked for", which nothing else can tell
     * apart — a declined lossless session looks exactly like a granted one from the outside).
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
     * Start host→client audio: Opus decode → jitter ring → AAudio, all in Rust.
     * [lowLatencyMode] (the experimental toggle) additionally tags the stream usage=Game for the
     * HAL's game-audio routing. No-op if already started. Best-effort — a failure leaves video
     * streaming.
     *
     * [isTv] steers the AAudio open ladder: a TV box starts at Shared rather than betting the
     * audio plane on an Exclusive/MMAP path whose routing we cannot verify from inside the
     * process. Passed from `FEATURE_LEANBACK` (same source as [nativeStartVideo]) because the
     * native side's own `ro.build.characteristics` check is not answered by every TV device.
     */
    external fun nativeStartAudio(handle: Long, lowLatencyMode: Boolean, isTv: Boolean)

    /** Stop + join the audio thread and close AAudio, without closing the session. No-op on `0`. */
    external fun nativeStopAudio(handle: Long)

    /**
     * Start mic uplink: AAudio input → Opus (48 kHz mono, 10 ms) → host (`send_mic` / 0xCB), all in
     * Rust. [echoCancel] opens the capture under the VoiceCommunication preset (the HAL's own echo
     * canceller / noise suppressor) and allocates an audio session id; the return value is that id
     * (`> 0`) so the caller can attach the Java [android.media.audiofx.AcousticEchoCanceler] /
     * [android.media.audiofx.NoiseSuppressor] as a backstop — `0` when none was allocated
     * (echoCancel off, the device refused the preset and the open fell back to the plain path, or
     * the mic failed entirely). No-op if already running (returns the running capture's id). The
     * caller MUST hold RECORD_AUDIO; otherwise the AAudio input stream fails to open and the rest
     * of the session keeps streaming.
     */
    external fun nativeStartMic(handle: Long, echoCancel: Boolean): Int

    /**
     * Stop + join the mic thread and close the AAudio input stream. No-op on `0`. Leaves the
     * session's mute state ([nativeSetMicMuted]) alone — a surface recreate stops and restarts the
     * mic, and a user who muted must stay muted through it.
     */
    external fun nativeStopMic(handle: Long)

    /**
     * Mute/unmute the mic uplink mid-stream. Muting does NOT stop the capture: the AAudio input
     * stream, the input preset it settled on and its primed buffers stay as they are, and the
     * encode loop drops each 10 ms frame instead of encoding + sending it — so room audio is never
     * encoded and nothing goes on the wire, while a toggle costs an atomic store and takes effect
     * on the next 10 ms boundary (a stop/start would re-run the preset fallback ladder and re-prime
     * buffers every time).
     *
     * Sticky for the SESSION — the flag lives on the handle, not on the capture — so the mic
     * restart a surface recreate performs comes back muted, with no window for an unmuted frame to
     * escape; a fresh session always starts unmuted. Nothing here is persisted. No-op on `0`.
     * Cheap (one atomic store); UI-safe.
     *
     * One honest consequence of keeping the stream open: the platform's own recording indicator
     * stays lit while muted, because the mic really is still open. What stops is the encode and the
     * send — no captured audio leaves the process.
     */
    external fun nativeSetMicMuted(handle: Long, muted: Boolean)

    /**
     * Start tier-A DualSense pad audio: render the host's `0xD1` streams on the pad's own
     * 4-channel USB audio device.
     *
     * [fd] is an open [android.hardware.usb.UsbDeviceConnection]'s file descriptor. Native code
     * **borrows** it — it claims the pad's audio interface through usbfs (which leaves any HID
     * claim on the same device alone) and never closes the descriptor. The caller must keep the
     * connection open until [nativeStopPadAudio] returns.
     *
     * This also declares the pad's render capability to the host; without it no `0xD1` is sent.
     *
     * Returns false when there is nothing to render. A kernel that refuses the interface claim is
     * NOT reported here — the renderer discovers that on its own thread and the session simply
     * carries on without tier A, because some OEM kernels refuse and no app-side fix exists.
     */
    external fun nativeStartPadAudio(
        handle: Long,
        pad: Int,
        fd: Int,
        haptics: Boolean,
        speaker: Boolean,
    ): Boolean

    /**
     * Stop tier-A pad audio and join its render thread, and hand the pad back to wire rumble.
     *
     * Returns only once the thread is joined — so the `UsbDeviceConnection` may be closed as soon
     * as this returns, and not before.
     */
    external fun nativeStopPadAudio(handle: Long, pad: Int)

    /**
     * Drive the pad with a test tone through the real render path — no host, no session.
     *
     * [fd] must come from a connection **nothing else is driving transfers on**: two engines on
     * one usbfs descriptor reap each other's completions. Blocks for roughly [seconds]; run it off
     * the main thread. Returns sample frames written, or negative on failure.
     */
    external fun nativePadAudioSelfTest(fd: Int, seconds: Int, hz: Int): Int

    /**
     * Is a mic capture actually RUNNING — i.e. did [nativeStartMic] open a stream, and has
     * [nativeStopMic] not been called since? Offer the in-stream mute control on THIS rather than
     * on the user's setting: a device that refused every AAudio input rung (or a missing
     * RECORD_AUDIO grant) then shows no control instead of a lie about a mic being heard. `false`
     * on a `0` handle. Cheap; UI-safe.
     */
    external fun nativeMicActive(handle: Long): Boolean

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

    /**
     * Whether the host advertised full-fidelity stylus injection (`HOST_CAP_PEN`) — the gate
     * for splitting stylus pointers out of the touch path onto the pen plane. False on `0`.
     */
    external fun nativeHostSupportsPen(handle: Long): Boolean

    /**
     * One stylus batch of STATE-FULL samples (the pen plane; design/pen-tablet-input.md §7):
     * [count] × 10 floats, oldest first — `[state, tool, x, y, pressure, distance, tilt_deg,
     * azimuth_deg, roll_deg, dt_us]`. `state` = the wire in-range/touching/barrel bits; `tool`
     * 0=pen 1=eraser; x/y/pressure/distance normalized 0..1; distance/tilt/azimuth/roll < 0 =
     * unknown. Send only when [nativeHostSupportsPen]; repeat the last sample ≤100 ms while the
     * pen is in range (the host force-releases a silent stroke after 200 ms).
     */
    external fun nativeSendPen(handle: Long, samples: FloatArray, count: Int)

    /**
     * Whether the host advertised committed-text injection (`HOST_CAP_TEXT_INPUT`) — its inject
     * backend can type Unicode text directly. Picks the real IME `InputConnection` (autocorrect,
     * gesture typing, non-Latin scripts) over the TYPE_NULL raw-key fallback. False on `0`.
     */
    external fun nativeTextInputSupported(handle: Long): Boolean

    /**
     * Committed IME text → one `TextInput` wire event per Unicode scalar, in order. Control
     * characters are skipped natively (Enter/Backspace ride [nativeSendKey]). Only meaningful
     * when [nativeTextInputSupported] returned true — older hosts ignore the events.
     */
    external fun nativeSendText(handle: Long, text: String)

    // ---- Shared clipboard (text v1): Kotlin drives ClipboardManager, Rust the protocol ----
    // Opt-in per session (nativeClipControl). Local copies are announced as lazy offers; bytes
    // cross only when the host pastes (a "fetch:" event answered by nativeClipServeText). Host
    // copies arrive as "offer:" events, fetched eagerly into the system clipboard.

    /**
     * The management-API port the host reported in this session's `Welcome` — where its game
     * library is served — or 0 if it advertised none (older host, or no management API).
     *
     * Persist it on the host record: unlike the mDNS `mgmt` TXT, this arrives over the connection
     * we have already authenticated, so it is what makes a host that moved off 47990 browsable
     * over a VPN, a routed subnet, or when it was added by address.
     */
    external fun nativeHostMgmtPort(handle: Long): Int

    /** Whether the host advertised a working shared-clipboard service (HOST_CAP_CLIPBOARD). */
    external fun nativeClipSupported(handle: Long): Boolean

    /** Session-level clipboard opt-in/out; nothing happens until enabled=true crosses. */
    external fun nativeClipControl(handle: Long, enabled: Boolean)

    /** Announce "this device's clipboard now holds text". [seq]: monotonic, newest wins. */
    external fun nativeClipOfferText(handle: Long, seq: Int)

    /** Pull the text of the host's offer [seq] → transfer id echoed on "data:"/"error:", or -1. */
    external fun nativeClipFetchText(handle: Long, seq: Int): Int

    /** Answer a "fetch:" event with the clipboard's current text (the host is pasting). */
    external fun nativeClipServeText(handle: Long, reqId: Int, text: String)

    /** Abort a clipboard transfer by id (either direction). */
    external fun nativeClipCancel(handle: Long, id: Int)

    /**
     * Block ≤250 ms for the next clipboard event, as a compact string: `state:<0|1>` ·
     * `offer:<seq>:<hasText>` · `fetch:<reqId>` · `data:<xferId>:<text>` · `cancel:<id>` ·
     * `error:<id>:<code>` · `closed` (session gone) — null on timeout. Dedicated poll thread.
     */
    external fun nativeNextClip(handle: Long): String?

    // ---- Gamepad: each controller forwarded on its own wire pad index (0..15, low byte of flags) ----
    // The pad index is assigned per Android device by GamepadRouter; a single controller lands on 0,
    // so its wire is byte-identical to the old single-pad path. The core folds the per-transition
    // events into seq'd GamepadState snapshots keyed on this index and owns the per-pad seq.

    /** One gamepad button transition on wire pad [pad] (0..15). bit: a [Gamepad].BTN_* bit. down: press/release. */
    external fun nativeSendGamepadButton(handle: Long, bit: Int, down: Boolean, pad: Int)

    /** One gamepad axis update on wire pad [pad] (0..15). axisId: [Gamepad].AXIS_* (0..5). value: stick i16 (+y=up) / trigger 0..255. */
    external fun nativeSendGamepadAxis(handle: Long, axisId: Int, value: Int, pad: Int)

    /**
     * Declare the controller KIND presented on wire pad [pad] (0..15) so the host builds a matching
     * virtual device (mixed types across pads). pref: a [Gamepad].PREF_* wire byte. Send ONCE when a
     * pad opens, BEFORE any of its input; an older host ignores it (that pad then uses the handshake's
     * session-default kind — the pre-existing single-pad behaviour on pad 0).
     */
    external fun nativeSendGamepadArrival(handle: Long, pref: Int, pad: Int)

    /** Signal wire pad [pad] (0..15) was unplugged so the host tears its virtual device down. The core stamps the seq + re-sends. */
    external fun nativeSendGamepadRemove(handle: Long, pad: Int)

    /**
     * Whether motion sent for a pad that declared [declaredPref] (the [Gamepad].PREF_* byte passed
     * to [nativeSendGamepadArrival]) can actually reach the game, or would be decoded and dropped
     * by a host backend without a motion plane — the X-Box classes have no gyro in their HID
     * contract.
     *
     * Answered natively, off `punktfunk_core::config::pad_motion_reaches`, rather than
     * reconstructed here from the session's requested/resolved prefs. The rule is subtler than it
     * looks (the host builds each pad from its OWN declaration and folds what it cannot build, so
     * neither the declaration nor the session echo answers it alone) and every way of getting it
     * wrong is silent, so it lives in one place with one set of tests.
     *
     * Ask ONCE when a pad opens, not per sample. `true` when the session handle is dead — "don't
     * suppress" is the safe answer whenever we cannot tell.
     */
    external fun nativePadMotionReaches(handle: Long, declaredPref: Int): Boolean

    /**
     * One raw HID input report from a client-captured controller (the as-is Steam Controller 2
     * passthrough), forwarded verbatim on the rich-input plane. [buf] is a DIRECT ByteBuffer whose
     * first [len] bytes are the report, id byte first (0x42/0x45/0x47 state, 0x43 battery, …);
     * len is clamped to 64. Called from the capture thread at the controller's own report rate.
     */
    external fun nativeSendPadHidReport(handle: Long, pad: Int, buf: java.nio.ByteBuffer, len: Int)

    /**
     * One touchpad contact from a client-captured controller (the Sony USB capture), forwarded on
     * the rich-input plane (`RichInput::Touchpad`). [finger] is the contact slot (0/1); [x]/[y]
     * are normalized 0..65535 in SCREEN convention (+y down — the wire's fixed meaning); active
     * false lifts the finger. Send on change only — the host holds per-slot state.
     */
    external fun nativeSendPadTouch(handle: Long, pad: Int, finger: Int, active: Boolean, x: Int, y: Int)

    /**
     * One motion-sensor sample from a client-captured controller (`RichInput::Motion`): gyro
     * pitch/yaw/roll + accel, each a raw signed-16 value in the pad's own units — the host passes
     * them straight into the virtual DualSense report. Called at the pad's report rate.
     */
    external fun nativeSendPadMotion(
        handle: Long,
        pad: Int,
        gyroPitch: Int,
        gyroYaw: Int,
        gyroRoll: Int,
        accelX: Int,
        accelY: Int,
        accelZ: Int,
    )

    // ---- Host→client gamepad feedback: Rust pulls block ~100ms, Kotlin renders (see GamepadFeedback) ----

    /**
     * Block up to ~100 ms for the next rumble update. Returns a packed positive long: bits 49..52 =
     * wire pad index (0..15), bit 48 = has a v2 lease, bits 32..47 = ttl_ms, bits 16..31 = low, bits
     * 0..15 = high (each amplitude 0..0xFFFF; 0/0 = stop), or -1 on timeout / session closed. Kotlin
     * routes the update to the controller holding that pad index. Call from a dedicated poll thread.
     */
    external fun nativeNextRumble(handle: Long): Long

    /**
     * Block up to ~100 ms for the next HID-output event, written into [buf] (a direct ByteBuffer,
     * capacity >= 128) as `[pad][kind][fields…]` (leading pad = the wire pad index to route to):
     * Led=pad 01 r g b, PlayerLeds=pad 02 bits, Trigger=pad 03 which effect…, raw as-is
     * passthrough report=pad 05 kind report-bytes (kind 0 = output report, 1 = feature report).
     * Returns the byte count, or -1 on timeout / session closed.
     */
    external fun nativeNextHidout(handle: Long, buf: java.nio.ByteBuffer): Int
}
