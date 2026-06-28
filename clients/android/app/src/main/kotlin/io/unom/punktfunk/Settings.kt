package io.unom.punktfunk

import android.content.Context
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
    val compositor: Int = 0,
    val gamepad: Int = 0,
    /** Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
     * can capture; the resolved count drives the decoder + AAudio layout. */
    val audioChannels: Int = 2,
    val micEnabled: Boolean = false,
    /** Show the live stats overlay (FPS / throughput / latency) during a stream. */
    val statsHudEnabled: Boolean = true,
    /**
     * Touch input model. `true` (default) = trackpad: the cursor stays put on touch-down and moves
     * by the finger's relative delta (swipe to nudge, lift and re-swipe to walk it across), tap to
     * click where it is. `false` = direct pointing: the cursor jumps to the finger (the old behaviour).
     */
    val trackpadMode: Boolean = true,
)

/** Loads/saves [Settings] in the app-private `punktfunk_settings` prefs. */
class SettingsStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_settings", Context.MODE_PRIVATE)

    fun load(): Settings = Settings(
        width = prefs.getInt(K_W, 0),
        height = prefs.getInt(K_H, 0),
        hz = prefs.getInt(K_HZ, 0),
        bitrateKbps = prefs.getInt(K_BITRATE, 0),
        compositor = prefs.getInt(K_COMPOSITOR, 0),
        gamepad = prefs.getInt(K_GAMEPAD, 0),
        audioChannels = prefs.getInt(K_AUDIO_CH, 2),
        micEnabled = prefs.getBoolean(K_MIC, false),
        statsHudEnabled = prefs.getBoolean(K_HUD, true),
        trackpadMode = prefs.getBoolean(K_TRACKPAD, true),
    )

    fun save(s: Settings) {
        prefs.edit()
            .putInt(K_W, s.width)
            .putInt(K_H, s.height)
            .putInt(K_HZ, s.hz)
            .putInt(K_BITRATE, s.bitrateKbps)
            .putInt(K_COMPOSITOR, s.compositor)
            .putInt(K_GAMEPAD, s.gamepad)
            .putInt(K_AUDIO_CH, s.audioChannels)
            .putBoolean(K_MIC, s.micEnabled)
            .putBoolean(K_HUD, s.statsHudEnabled)
            .putBoolean(K_TRACKPAD, s.trackpadMode)
            .apply()
    }

    private companion object {
        const val K_W = "width"
        const val K_H = "height"
        const val K_HZ = "hz"
        const val K_BITRATE = "bitrate_kbps"
        const val K_COMPOSITOR = "compositor"
        const val K_GAMEPAD = "gamepad"
        const val K_AUDIO_CH = "audio_channels"
        const val K_MIC = "mic_enabled"
        const val K_HUD = "stats_hud_enabled"
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
    @Suppress("DEPRECATION") // hdrCapabilities is the supported query on minSdk 31
    val caps = display.hdrCapabilities ?: return false
    return caps.supportedHdrTypes.any {
        it == Display.HdrCapabilities.HDR_TYPE_HDR10 || it == Display.HdrCapabilities.HDR_TYPE_HDR10_PLUS
    }
}

/** Resolve [Settings] (with its 0=native placeholders) to the concrete mode to request. */
fun Settings.effectiveMode(context: Context): Triple<Int, Int, Int> {
    val native = nativeDisplayMode(context)
    val w = if (width > 0) width else native.first
    val h = if (height > 0) height else native.second
    val hz = if (hz > 0) hz else native.third
    return Triple(w, h, hz)
}

// ---- UI option tables (value, label). The first entry is always the "auto/native" default. ----

/** (width, height, label). `(0,0)` = native display. */
val RESOLUTION_OPTIONS = listOf(
    Triple(0, 0, "Native display"),
    Triple(1280, 720, "1280 × 720"),
    Triple(1920, 1080, "1920 × 1080"),
    Triple(2560, 1440, "2560 × 1440"),
    Triple(3840, 2160, "3840 × 2160"),
)

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

/** (kbps, label). `0` = host default. */
val BITRATE_OPTIONS = listOf(
    0 to "Automatic",
    10_000 to "10 Mbps",
    20_000 to "20 Mbps",
    50_000 to "50 Mbps",
    100_000 to "100 Mbps",
)

/** index = CompositorPref wire byte. */
val COMPOSITOR_OPTIONS = listOf(
    "Automatic",
    "KWin (KDE Plasma)",
    "wlroots (Sway / Hyprland)",
    "Mutter (GNOME)",
    "gamescope",
)

/** index = GamepadPref wire byte (0=Auto 1=Xbox360 2=DualSense 3=XboxOne 4=DualShock4). */
val GAMEPAD_OPTIONS = listOf(
    "Automatic",
    "Xbox 360",
    "DualSense",
    "Xbox One",
    "DualShock 4",
)
