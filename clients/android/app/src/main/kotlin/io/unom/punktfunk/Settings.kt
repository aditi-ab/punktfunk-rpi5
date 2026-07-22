package io.unom.punktfunk

import android.content.Context
import android.os.Build
import android.util.Log
import android.view.Display

/**
 * User-tunable stream settings, persisted in `SharedPreferences`. A `0` resolution/refresh means
 * "native display mode" (resolved at connect time from [nativeDisplayMode]); `0` bitrate means the
 * host's default. [compositor]/[gamepad] are the `CompositorPref`/`GamepadPref` wire bytes the host
 * understands (0 = Auto). Mirrors the Linux/Apple clients' settings.
 */
data class Settings(
    val width: Int = 0,
    val height: Int = 0,
    val hz: Int = 0,
    val bitrateKbps: Int = 0,
    /**
     * Render-resolution multiplier: the client asks the host to render/encode at `chosen mode ×
     * renderScale` and the compositor downscales the larger decoded frame to the SurfaceView
     * (`> 1` supersamples for sharpness, at more bandwidth AND decode; `< 1` renders under native
     * for a lighter host/link). `1.0` = Native. Applied at connect via [RenderScale.apply], clamped
     * even + to the codec's max dimension. Mirrors the Apple/Linux clients' render scale.
     */
    val renderScale: Double = 1.0,
    /**
     * Advertise HDR (10-bit BT.2020 PQ) to the host. Default on, but only *effective* on a panel that
     * can actually present HDR10 (see [displaySupportsHdr]) — on an SDR display HDR is never
     * advertised regardless, so the host sends a proper 8-bit BT.709 stream rather than PQ the panel
     * would mis-tone-map. Turning this off forces SDR even on a capable panel.
     */
    val hdrEnabled: Boolean = true,
    val compositor: Int = 0,
    val gamepad: Int = 0,
    /** Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
     * can capture; the resolved count drives the decoder + AAudio layout. */
    val audioChannels: Int = 2,
    /** Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`. A soft
     * preference — the host emits it when it can, else falls back. AMediaCodec decodes whichever
     * the host resolves (AV1 is only advertised/offered when the device has a real AV1 decoder). */
    val codec: String = "auto",
    val micEnabled: Boolean = false,
    /**
     * How much the in-stream stats overlay shows — see [StatsVerbosity]. Defaults to
     * [StatsVerbosity.NORMAL] (the res/fps line + latency headline + reliability counters); the full
     * decoder/feed/equation HUD is [StatsVerbosity.DETAILED], and a single terse line is
     * [StatsVerbosity.COMPACT]. A 3-finger tap cycles through the tiers live.
     */
    val statsVerbosity: StatsVerbosity = StatsVerbosity.NORMAL,
    /**
     * Touch input model — how touchscreen fingers drive the host. [TouchMode.TRACKPAD] (default):
     * the cursor stays put on touch-down and moves by the finger's relative delta (swipe to nudge,
     * lift and re-swipe to walk it across), tap to click where it is. [TouchMode.POINTER]: the
     * cursor jumps to the finger (direct pointing). [TouchMode.TOUCH]: real multi-touch
     * passthrough — every finger reaches the host as a touchscreen contact, for apps/games that
     * understand touch. Mirrors the Apple client's TouchInputMode.
     */
    val touchMode: TouchMode = TouchMode.TRACKPAD,
    /**
     * Swap the whole home screen for the controller-optimized "console" UI (the host carousel +
     * gamepad chrome) whenever a controller is connected — mirrors the Apple client's
     * `gamepadUIEnabled`. On by default; turn it off to keep the touch UI even with a pad attached.
     * A TV (leanback) is always in this mode regardless (its remote/pad is the only input).
     */
    val gamepadUiEnabled: Boolean = true,
    /**
     * Show the experimental game-library browser (the coverflow reached with Y from a saved host).
     * Fetched from the host's management API over mTLS; needs a paired host. Mirrors the Apple
     * client's `libraryEnabled`.
     */
    val libraryEnabled: Boolean = true,
    /**
     * "Low-latency mode" — the master switch over the latency pipeline: the async decode loop
     * (native; burst-feed + present-newest-per-vsync, the Apple client's discipline), decoder ranking
     * + per-SoC vendor keys, pipeline thread boosts + ADPF max-performance, game-tagged AAudio, DSCP
     * marking on the media sockets, HDMI ALLM, and the forced TV mode switch. (The Wi-Fi locks are NOT
     * part of this — both are always held while streaming; see StreamScreen.) On (default): the fast
     * pipeline. Off restores the original synchronous decode loop byte-for-byte, kept as a per-device
     * escape hatch. Promoted to default once the receive-side latency ratchet the overhaul interacted
     * badly with was fixed in the shared core — the pump now jumps to live on a standing backlog
     * instead of accumulating it (see `punktfunk-core` `FrameChannel`), so the async loop no longer
     * feeds a queue that only grows.
     */
    val lowLatencyMode: Boolean = true,
    /**
     * Wake-on-LAN a saved host before connecting when it isn't currently seen on mDNS. On (default):
     * a connect to a host with a learned MAC that isn't advertising sends a magic packet and waits
     * for it to reappear (see [WakeController]) before dialing. Off: always dial straight through,
     * skipping the mDNS-presence check entirely — for a host that's actually up but not visible on
     * mDNS (a flaky discovery path, a VLAN/subnet that blocks multicast, etc.), where auto-wake would
     * otherwise misfire and wait out its timeout despite the host already being reachable.
     */
    val autoWakeEnabled: Boolean = true,
    /**
     * Opt-in: ALSO play the rumble the host addresses to controller 1 (wire pad 0) on this
     * phone's own vibration motor — for clip-on gamepads that ship without rumble motors, where
     * the phone body is the only actuator in the player's hands. Off by default; read once per
     * session by StreamScreen (it hands GamepadFeedback the device vibrator only when set). The
     * toggle is hidden on devices without a vibrator (TVs), where this would be a silent no-op.
     */
    val rumbleOnPhone: Boolean = false,

    /**
     * Capture a Steam Controller 2 (wired / Puck dongle over USB, or an already-paired BLE pad)
     * and pass it through AS-IS: the host presents a real `28DE:1302` that its Steam drives
     * directly (Linux hosts). ON by default — it engages only when such a controller is actually
     * present at stream start, so it costs nothing otherwise; the toggle exists for the rare
     * setup where the OS-level pad (lizard mode) is preferred.
     */
    val sc2Capture: Boolean = true,

    /**
     * Lock a physical mouse to the stream ([android.view.View.requestPointerCapture]) and forward
     * raw relative motion — FPS mouse-look, the iPad "Capture pointer for games" twin. Engages at
     * stream start and on a click into the stream; Ctrl+Alt+Shift+Q toggles it live (the chord
     * works even with this off). Off (default): a mouse points absolutely, desktop-style.
     */
    val pointerCapture: Boolean = false,

    /**
     * Flip scroll direction — the mouse wheel and the two-finger touch scroll both. Parity with
     * the Apple/GTK clients' "Invert scroll direction".
     */
    val invertScroll: Boolean = false,

    /**
     * Sync text copied on this device to the host and vice versa while streaming (the desktop
     * clients' shared clipboard, text-only here). Only effective when the host advertises the
     * clipboard capability; the protocol is opt-in per session either way.
     */
    val clipboardSync: Boolean = true,
)

/** [Settings.touchMode] values; persisted by name. */
enum class TouchMode { TRACKPAD, POINTER, TOUCH }

/**
 * Stats-overlay detail tiers, in cycling order (persisted by name). Each tier is a strict superset
 * of the previous one, so toning down never hides a number a lower tier keeps:
 * - [OFF] — no overlay (and native sampling is gated off, one atomic load per frame).
 * - [COMPACT] — one line: `fps · end-to-end ms · Mb/s` (+ a loss flag when frames drop).
 * - [NORMAL] — adds the resolution/refresh line, the end-to-end p50/p95 headline, and the
 *   reliability counters (lost / skipped / FEC) when nonzero. The default.
 * - [DETAILED] — the full HUD: also the decoder label, the video-feed descriptor, and the
 *   `host+network + decode` stage equation.
 * A 3-finger tap in-stream cycles Off → Compact → Normal → Detailed → Off (see [next]).
 */
enum class StatsVerbosity(val label: String) {
    OFF("Off"),
    COMPACT("Compact"),
    NORMAL("Normal"),
    DETAILED("Detailed");

    /** The next tier for the live 3-finger-tap cycle (wraps Detailed → Off). */
    fun next(): StatsVerbosity = entries[(ordinal + 1) % entries.size]
}

/** Loads/saves [Settings] in the app-private `punktfunk_settings` prefs. */
class SettingsStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_settings", Context.MODE_PRIVATE)

    fun load(): Settings = Settings(
        width = prefs.getInt(K_W, 0),
        height = prefs.getInt(K_H, 0),
        hz = prefs.getInt(K_HZ, 0),
        bitrateKbps = prefs.getInt(K_BITRATE, 0),
        renderScale = prefs.getFloat(K_RENDER_SCALE, 1.0f).toDouble(),
        hdrEnabled = prefs.getBoolean(K_HDR, true),
        compositor = prefs.getInt(K_COMPOSITOR, 0),
        gamepad = prefs.getInt(K_GAMEPAD, 0),
        audioChannels = prefs.getInt(K_AUDIO_CH, 2),
        codec = prefs.getString(K_CODEC, "auto") ?: "auto",
        micEnabled = prefs.getBoolean(K_MIC, false),
        statsVerbosity = prefs.getString(K_STATS_VERBOSITY, null)
            ?.let { name -> StatsVerbosity.entries.firstOrNull { it.name == name } }
            // Migration from the pre-tier Boolean "stats_hud_enabled": an explicit OFF stays off;
            // everyone else (incl. fresh installs) lands on NORMAL — the old always-full HUD toned
            // down to the new default, which is the whole point of adding tiers.
            ?: if (prefs.contains(K_HUD) && !prefs.getBoolean(K_HUD, true)) {
                StatsVerbosity.OFF
            } else {
                StatsVerbosity.NORMAL
            },
        touchMode = prefs.getString(K_TOUCH_MODE, null)
            ?.let { name -> TouchMode.entries.firstOrNull { it.name == name } }
            // Migration: the pre-enum Boolean "trackpad_mode" (true = trackpad, false = direct).
            ?: if (prefs.getBoolean(K_TRACKPAD, true)) TouchMode.TRACKPAD else TouchMode.POINTER,
        gamepadUiEnabled = prefs.getBoolean(K_GAMEPAD_UI, true),
        libraryEnabled = prefs.getBoolean(K_LIBRARY, true),
        lowLatencyMode = prefs.getBoolean(K_LOW_LATENCY, true),
        autoWakeEnabled = prefs.getBoolean(K_AUTO_WAKE, true),
        rumbleOnPhone = prefs.getBoolean(K_RUMBLE_ON_PHONE, false),
        sc2Capture = prefs.getBoolean(K_SC2_CAPTURE, true),
        pointerCapture = prefs.getBoolean(K_POINTER_CAPTURE, false),
        invertScroll = prefs.getBoolean(K_INVERT_SCROLL, false),
        clipboardSync = prefs.getBoolean(K_CLIPBOARD_SYNC, true),
    )

    fun save(s: Settings) {
        prefs.edit()
            .putInt(K_W, s.width)
            .putInt(K_H, s.height)
            .putInt(K_HZ, s.hz)
            .putInt(K_BITRATE, s.bitrateKbps)
            .putFloat(K_RENDER_SCALE, s.renderScale.toFloat())
            .putBoolean(K_HDR, s.hdrEnabled)
            .putInt(K_COMPOSITOR, s.compositor)
            .putInt(K_GAMEPAD, s.gamepad)
            .putInt(K_AUDIO_CH, s.audioChannels)
            .putString(K_CODEC, s.codec)
            .putBoolean(K_MIC, s.micEnabled)
            .putString(K_STATS_VERBOSITY, s.statsVerbosity.name)
            .putString(K_TOUCH_MODE, s.touchMode.name)
            .putBoolean(K_GAMEPAD_UI, s.gamepadUiEnabled)
            .putBoolean(K_LIBRARY, s.libraryEnabled)
            .putBoolean(K_LOW_LATENCY, s.lowLatencyMode)
            .putBoolean(K_AUTO_WAKE, s.autoWakeEnabled)
            .putBoolean(K_RUMBLE_ON_PHONE, s.rumbleOnPhone)
            .putBoolean(K_SC2_CAPTURE, s.sc2Capture)
            .putBoolean(K_POINTER_CAPTURE, s.pointerCapture)
            .putBoolean(K_INVERT_SCROLL, s.invertScroll)
            .putBoolean(K_CLIPBOARD_SYNC, s.clipboardSync)
            .apply()
    }

    private companion object {
        const val K_W = "width"
        const val K_H = "height"
        const val K_HZ = "hz"
        const val K_BITRATE = "bitrate_kbps"
        const val K_RENDER_SCALE = "render_scale"
        const val K_HDR = "hdr_enabled"
        const val K_COMPOSITOR = "compositor"
        const val K_GAMEPAD = "gamepad"
        const val K_AUDIO_CH = "audio_channels"
        const val K_CODEC = "codec"
        const val K_MIC = "mic_enabled"
        const val K_STATS_VERBOSITY = "stats_verbosity"

        /** Pre-tier Boolean the [K_STATS_VERBOSITY] enum replaced — read once for migration, never
         * written. */
        const val K_HUD = "stats_hud_enabled"
        const val K_TOUCH_MODE = "touch_mode"
        const val K_GAMEPAD_UI = "gamepad_ui_enabled"
        const val K_LIBRARY = "library_enabled"

        /**
         * Bumped AGAIN to restart every install at the new default (ON). History: the original
         * `"low_latency_mode"` shipped default-ON; `"low_latency_mode_experimental"` restarted
         * everyone at OFF after the overhaul regressed on some phones. That regression was the
         * receive-side latency ratchet the async loop fed (a standing queue that only grew) — now
         * fixed in the shared core (`punktfunk-core` `FrameChannel`: the pump jumps to live on a
         * standing backlog instead of accumulating it), so the fast pipeline is the default again. A
         * fresh key re-defaults every install — including ones persisted OFF under the old key — to
         * on; both stale keys are abandoned unread. The toggle stays as a per-device escape hatch.
         */
        const val K_LOW_LATENCY = "low_latency_mode_v2"
        const val K_AUTO_WAKE = "auto_wake_enabled"
        const val K_RUMBLE_ON_PHONE = "rumble_on_phone"
        const val K_SC2_CAPTURE = "sc2_capture"
        const val K_POINTER_CAPTURE = "pointer_capture"
        const val K_INVERT_SCROLL = "invert_scroll"
        const val K_CLIPBOARD_SYNC = "clipboard_sync"

        /** Legacy Boolean the enum replaced — read once as the migration default, never written. */
        const val K_TRACKPAD = "trackpad_mode"
    }
}

/**
 * The device's native display mode as a landscape `(width, height, hz)` — the long edge is the
 * width, since we stream a desktop. Falls back to 1920×1080@60 if the display can't be read.
 * [context] must be a visual (Activity) context.
 */
fun nativeDisplayMode(context: Context): Triple<Int, Int, Int> {
    // getDisplay() throws on a non-visual context rather than returning null — guard it.
    val display = runCatching { context.display }.getOrNull() ?: return Triple(1920, 1080, 60)
    val mode = display.mode
    val w = mode.physicalWidth
    val h = mode.physicalHeight
    val hz = mode.refreshRate.toInt().coerceAtLeast(1)
    return Triple(maxOf(w, h), minOf(w, h), hz)
}

/**
 * True when this device's display can actually present HDR10, so we should advertise HDR to the
 * host. On an SDR panel we advertise `0` instead — the host then sends a proper 8-bit BT.709 stream
 * rather than BT.2020 PQ the panel would mis-tone-map (the washed-out/dark failure). Mirrors the
 * capability gate the Apple/Windows clients apply.
 */
fun displaySupportsHdr(context: Context): Boolean {
    val display = runCatching { context.display }.getOrNull() ?: return false
    val types = buildSet {
        // API 34+: the sanctioned per-mode query (Display.Mode.getSupportedHdrTypes). The
        // deprecated Display-level hdrCapabilities can return EMPTY on Android 14+ devices
        // (Pixel-class panels included), which would make a genuinely HDR display advertise
        // no-HDR and pin the whole session to 8-bit SDR.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            display.mode.supportedHdrTypes.forEach { add(it) }
        }
        // Union the legacy query defensively — the supported one on minSdk 31, and some vendors
        // populate only this on newer APIs.
        @Suppress("DEPRECATION")
        display.hdrCapabilities?.supportedHdrTypes?.forEach { add(it) }
    }
    // HDR10/HDR10+ only: the stream is BT.2020 PQ — a Dolby-Vision/HLG-only panel can't present it.
    val supported = types.any {
        it == Display.HdrCapabilities.HDR_TYPE_HDR10 || it == Display.HdrCapabilities.HDR_TYPE_HDR10_PLUS
    }
    Log.i("punktfunk", "display HDR types=$types → advertise HDR10=$supported")
    return supported
}

/** Resolve [Settings] (with its 0=native placeholders) to the concrete mode to request. */
fun Settings.effectiveMode(context: Context): Triple<Int, Int, Int> {
    val native = nativeDisplayMode(context)
    val w = if (width > 0) width else native.first
    val h = if (height > 0) height else native.second
    val hz = if (hz > 0) hz else native.third
    return Triple(w, h, hz)
}

/**
 * Client-side render-scale geometry — the Kotlin twin of `punktfunk-core`'s `render_scale` module
 * (and the Apple client's `RenderScale`). Multiply a base size, preserve aspect, even-floor (the
 * host rejects odd sizes), and clamp uniformly to the codec's per-axis ceiling so a connect can't
 * ask for a size the encoder rejects. `1.0` = Native. Pure + covered by [RenderScaleTest].
 */
object RenderScale {
    val PRESETS = listOf(0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0)

    /** H.264 tops out at 4096 px/axis; HEVC/AV1/auto at 8192 — the host's `codec.rs` walls. */
    fun maxDimension(codec: String): Int = if (codec == "h264") 4096 else 8192

    /** Clamp a raw multiplier into [0.5, 4.0]; a missing / non-positive / NaN value → 1.0. */
    fun sanitize(raw: Double): Double = if (raw > 0.0) raw.coerceIn(0.5, 4.0) else 1.0

    /** "Native (1×)" / "1.5×" / "2× · supersample" — the picker label. */
    fun label(scale: Double): String = when {
        scale == 1.0 -> "Native (1×)"
        scale > 1.0 -> "${trim(scale)}× · supersample"
        else -> "${trim(scale)}×"
    }

    private fun trim(s: Double): String =
        if (s == s.toLong().toDouble()) s.toLong().toString() else s.toString()

    /** Apply [scale] to a base size → a host-valid even, aspect-preserved, codec-clamped (w, h). */
    fun apply(baseW: Int, baseH: Int, scale: Double, maxDim: Int): Pair<Int, Int> {
        val s = sanitize(scale)
        var w = maxOf(baseW, 1) * s
        var h = maxOf(baseH, 1) * s
        val cap = maxDim.toDouble()
        val over = maxOf(w / cap, h / cap)
        if (over > 1.0) {
            w /= over
            h /= over
        }
        return Pair(evenFloor(w, 320), evenFloor(h, 200))
    }

    private fun evenFloor(value: Double, minimum: Int): Int {
        val v = maxOf(kotlin.math.floor(value).toInt(), minimum).coerceAtLeast(0)
        return v / 2 * 2
    }
}

/** (scale, label) for the render-scale picker. `1.0` = Native. */
val RENDER_SCALE_OPTIONS = RenderScale.PRESETS.map { it to RenderScale.label(it) }

// ---- UI option tables (value, label). The first entry is always the "auto/native" default. ----

/** (width, height, label). `(0,0)` = native display. */
val RESOLUTION_OPTIONS = listOf(
    Triple(0, 0, "Native display"),
    Triple(1280, 720, "1280 × 720"),
    Triple(1920, 1080, "1920 × 1080"),
    Triple(2560, 1440, "2560 × 1440"),
    Triple(3840, 2160, "3840 × 2160"),
)

/** True when the stored size is none of the [RESOLUTION_OPTIONS] presets — a custom resolution
 * typed in the touch settings. Detected from the size itself rather than a persisted flag, so it
 * can never disagree with what's actually stored (mirrors the Apple client). */
fun Settings.isCustomResolution(): Boolean =
    RESOLUTION_OPTIONS.none { (w, h, _) -> w == width && h == height }

/** (hz, label). `0` = native refresh. */
val REFRESH_OPTIONS = listOf(
    0 to "Native",
    30 to "30 Hz",
    60 to "60 Hz",
    90 to "90 Hz",
    120 to "120 Hz",
    144 to "144 Hz",
    165 to "165 Hz",
    240 to "240 Hz",
)

/** (channel count, label). 2 = stereo (default), 6 = 5.1, 8 = 7.1. */
val AUDIO_CHANNEL_OPTIONS = listOf(
    2 to "Stereo",
    6 to "5.1 Surround",
    8 to "7.1 Surround",
)

/** (stored value, label) for the preferred video codec. `"auto"` = host decides. The `"av1"` row
 * only makes sense on a device with a real AV1 decoder — SettingsScreen filters it out otherwise. */
val CODEC_OPTIONS = listOf(
    "auto" to "Automatic",
    "hevc" to "HEVC (H.265)",
    "h264" to "H.264 (AVC)",
    "av1" to "AV1",
)

/** The [Settings.codec] string as a `quic::CODEC_*` preference byte (`0` = auto). H264=1, HEVC=2,
 * AV1=4. */
fun Settings.preferredCodec(): Int = when (codec) {
    "h264" -> 1
    "hevc" -> 2
    "av1" -> 4
    else -> 0
}

/** (kbps, label). `0` = host default. */
val BITRATE_OPTIONS = listOf(
    0 to "Automatic",
    10_000 to "10 Mbps",
    20_000 to "20 Mbps",
    50_000 to "50 Mbps",
    100_000 to "100 Mbps",
    150_000 to "150 Mbps",
    200_000 to "200 Mbps",
    300_000 to "300 Mbps",
    500_000 to "500 Mbps",
)

/** index = CompositorPref wire byte. */
val COMPOSITOR_OPTIONS = listOf(
    "Automatic",
    "KWin (KDE Plasma)",
    "wlroots (Sway / Hyprland)",
    "Mutter (GNOME)",
    "gamescope",
)

/** (verbosity, label) for the stats-overlay detail picker. Order = the live 3-finger-tap cycle. */
val STATS_VERBOSITY_OPTIONS = StatsVerbosity.entries.map { it to it.label }

/** (mode, label) for the touch-input model. */
val TOUCH_MODE_OPTIONS = listOf(
    TouchMode.TRACKPAD to "Trackpad",
    TouchMode.POINTER to "Direct pointer",
    TouchMode.TOUCH to "Touch passthrough",
)

/** index = GamepadPref wire byte (0=Auto 1=Xbox360 2=DualSense 3=XboxOne 4=DualShock4). */
val GAMEPAD_OPTIONS = listOf(
    "Automatic",
    "Xbox 360",
    "DualSense",
    "Xbox One",
    "DualShock 4",
)
