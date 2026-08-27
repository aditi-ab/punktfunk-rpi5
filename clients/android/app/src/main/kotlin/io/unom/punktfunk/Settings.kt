package io.unom.punktfunk

import android.content.Context
import android.hardware.display.DisplayManager
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
    /**
     * Forward this device's controllers to the host at all. Default on — that was the
     * unconditional behaviour before this became a setting.
     *
     * Off is for a couch whose controller reaches the host another way: a USB passthrough tool
     * (VirtualHere and friends), or a pad simply plugged into the host itself. Leaving it on
     * there gives the host two controllers for one pair of hands, and games read both. It also
     * stops this device CLAIMING the pad — a device held open is one a passthrough tool can't
     * bind — which is why it gates the USB capture paths, not just the wire sends.
     */
    val gamepadForwarding: Boolean = true,
    /**
     * Where the guide (Xbox/PS) and misc/share presses land while streaming — the
     * cross-client `system_buttons` key: `"auto"` (forward on Android — the press reaches
     * the app on most devices) | `"forward"` | `"local"`.
     */
    val systemButtons: String = "auto",
    /**
     * The hold-Select guide gesture — the cross-client `guide_gesture` key: `"auto"` (off
     * on Android) | `"on"` | `"off"`. On: holding Select alone ≥350 ms sends the HOST's
     * guide, down until release (long hold = the host's long-press → a Gaming-Mode host's
     * QAM); a Select tap is delivered on release, slightly delayed. For devices whose
     * shell intercepts the physical guide button.
     */
    val guideGesture: String = "auto",
    /** Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
     * can capture; the resolved count drives the decoder + AAudio layout. */
    val audioChannels: Int = 2,
    /**
     * Requested audio format — the cross-client `audio_format` key: [AUDIO_FORMAT_OPUS] (the
     * default, and byte-for-byte the session every build before the lossless plane ran) or one of
     * the lossless rows in [AUDIO_FORMAT_OPTIONS], which span both rate families.
     *
     * Off by default and deliberately: lossless takes 2.1–8.5 Mbps off the top of the link,
     * OUTSIDE the ABR loop that manages the video budget, against the ~256 kbps Opus it replaces —
     * so a user has to pick it. Since 2026-08-17 this setting is the ONLY opt-in: the host's half
     * (`PUNKTFUNK_AUDIO_HIRES`) defaults ON and is an opt-OUT (`=0`), so this choice is enough on
     * any host that has not deliberately turned the plane off.
     * A REQUEST, never a fact: the host runs its gate and may answer Opus anyway, and
     * the native side downgrades the rate first if THIS device will not open it. What actually
     * happened is on the stats HUD, and in logcat's `audio: plane codec=… rate=…` line.
     */
    val audioFormat: String = AUDIO_FORMAT_OPUS,
    /** Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`. A soft
     * preference — the host emits it when it can, else falls back. AMediaCodec decodes whichever
     * the host resolves (AV1 is only advertised/offered when the device has a real AV1 decoder). */
    val codec: String = "auto",
    val micEnabled: Boolean = false,
    /**
     * Cancel acoustic echo on the mic uplink (plus noise suppression): the capture opens under
     * the VoiceCommunication preset so the HAL's own AEC/NS process it, with the Java effects
     * attached as a backstop where available. On by default — a phone/tablet plays the game audio
     * out of the same device its mic hears, so without this the host hears its own stream back.
     * Turn off for a headset-only setup where the untouched full-band capture sounds better.
     * Only meaningful while [micEnabled] is on.
     */
    val echoCancel: Boolean = true,
    /**
     * Ask the host to leave ITS OWN audio devices alone for this session
     * (`CLIENT_CAP_KEEP_HOST_AUDIO`): it captures whatever its default playback device already is,
     * so the speakers or headphones on the host PC keep playing while this device hears the same
     * audio. Off — the default, and what every build before this did — has the host park playback
     * on a silent endpoint, which is why the host goes quiet the moment a stream starts.
     *
     * REQUEST-only: there is no host-cap echo, so an older host ignores the ask and re-routes as it
     * always did ("audio still works, the host went quiet"), never a broken session.
     */
    val keepHostAudio: Boolean = false,
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
     * gamepad chrome) — mirrors the Apple client's `gamepadUIEnabled`. On by default; turn it off
     * to keep the touch UI even with a pad attached. WHEN it takes over is [gamepadUiMode].
     * A TV (leanback) is always in this mode regardless (its remote/pad is the only input).
     */
    val gamepadUiEnabled: Boolean = true,
    /**
     * Draw the console UI at 1080p and let the display scale it up, instead of at the panel's own
     * resolution. Off by default — this is a deliberate sharpness-for-smoothness trade, not
     * something to impose on a device that does not need it.
     *
     * It exists for 4K TVs and projectors. Their graphics chips are chosen to decode and composite
     * video, not to shade a UI, and are far slower than a phone's; at 4K every pass the console
     * draws — the mesh backdrop above all — costs four times what it does at 1080p on hardware
     * that is nowhere near four times faster. A "premium" 4K box is MORE likely to want this than
     * a cheap 1080p stick, which never had the extra pixels to begin with.
     *
     * Read by [io.unom.punktfunk.console.SkiaConsoleShell], which applies it with
     * `SurfaceHolder.setFixedSize` — the compositor then scales the smaller buffer up for free.
     * The stream is untouched; that has its own `renderScale`.
     */
    val reduceUiResolution: Boolean = false,
    /**
     * When [gamepadUiEnabled] actually takes over — the cross-client `gamepad_ui_mode` pair,
     * mirroring the Apple client's `gamepadUIMode`: `"connected"` (default, and what the switch
     * has always meant) waits for a controller; `"always"` keeps the console UI with no pad in
     * reach, for a phone or tablet that lives docked to a TV. Read only while [gamepadUiEnabled]
     * is on, which is why both settings screens hide the row when the switch is off. Anything
     * unrecognized resolves to `"connected"`. A TV ignores it — it is always in console mode.
     */
    val gamepadUiMode: String = GAMEPAD_UI_WHEN_CONNECTED,
    /**
     * Show the experimental game-library browser (the coverflow reached with Y from a saved host).
     * Fetched from the host's management API over mTLS; needs a paired host. Mirrors the Apple
     * client's `libraryEnabled`.
     */
    val libraryEnabled: Boolean = true,
    /**
     * Which colour family the console (gamepad) UI's living backdrop drifts through — the
     * cross-client `ui_palette` key: `"violet"` (the brand default), then `"oled"`, `"nebula"`,
     * `"abyss"`, `"ember"`, `"moss"`, `"graphite"`, then the six pale fields. See
     * [GamepadPalette], whose table and maths mirror the desktop console's and the Apple
     * client's under the same names. Presentation only: nothing
     * about a stream depends on it, so it is a device preference and never part of a profile.
     * An unknown value reads as the default rather than failing — a newer client may have shipped
     * a palette this build doesn't know.
     */
    val uiPalette: String = "violet",
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
     * The timeline presenter's intent — the cross-client `present_priority` pair (the Apple
     * client's "Prioritize" picker, same stored values): `"latency"` (default) = newest-wins,
     * a frame reaches glass the instant the glass budget opens; `"smooth"` = a small FIFO
     * drained one frame per vsync, absorbing network/decode jitter at one refresh of added
     * display latency per buffered frame. Anything unrecognized resolves to latency.
     */
    val presentPriority: String = "latency",
    /**
     * The smoothness buffer depth (`smooth_buffer`): 0 = Automatic (2 frames), else 1..3.
     * Only meaningful when [presentPriority] is `"smooth"`.
     */
    val smoothBuffer: Int = 0,
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
     * Opt-in: use this phone's own gyroscope as controller 1's motion when the forwarded pad has
     * none of its own — for clip-on gamepads without an IMU, where the phone body moves with the
     * player's hands. The rumble mirror's sibling, data flowing the other way. Off by default;
     * read once per session by StreamScreen (it starts a [io.unom.punktfunk.kit.DeviceGyro] only
     * when set), and the mirror stands down by itself whenever wire pad 0 is fed by a capture
     * link (USB DualSense / SC2 — pads with a real gyro). The toggle is hidden on devices
     * without a gyroscope (TVs), where this would be a silent no-op.
     */
    val gyroOnPhone: Boolean = false,

    /**
     * Capture a Steam Controller 2 (wired / Puck dongle over USB, or an already-paired BLE pad)
     * and pass it through AS-IS: the host presents a real `28DE:1302` that its Steam drives
     * directly (Linux hosts). ON by default — it engages only when such a controller is actually
     * present at stream start, so it costs nothing otherwise; the toggle exists for the rare
     * setup where the OS-level pad (lizard mode) is preferred.
     */
    val sc2Capture: Boolean = true,

    /**
     * Capture a USB-connected Sony controller (DualSense / DualSense Edge / DualShock 4) and
     * drive it directly: the app claims the pad's HID interface and renders the host's feedback
     * by writing USB output reports — rumble works on every phone (no kernel force-feedback
     * driver needed), and adaptive triggers + lightbar + player LEDs work at all (Android has no
     * platform API for any of them). ON by default — it engages only when such a pad is attached
     * over USB at stream start; uncaptured (toggle off / no permission / Bluetooth) the pad stays
     * on the ordinary InputDevice path. USB only: Android exposes no raw path to a Bluetooth
     * Classic pad, which is also why Sony's own Remote Play has no Android trigger support.
     */
    val dsCapture: Boolean = true,

    /**
     * Render the host's DualSense **voice-coil haptics** on a captured USB pad (tier A).
     *
     * The pad's own 4-channel audio device carries them, driven directly over usbfs — Android's
     * audio framework denylists that device by VID/PID, so there is no supported route to it. The
     * two kinds are arbitrated rather than mixed, and on evidence: wire rumble is suppressed only
     * while haptics frames are actually arriving, so a title that drives classic rumble and sends
     * no haptics audio keeps rumbling. Off, or on an uncaptured/Bluetooth pad, the pad stays on
     * ordinary rumble (tier C), which on this client already drives the same actuators.
     */
    val padHaptics: Boolean = true,

    /**
     * Render the pad's **built-in speaker** on a captured USB pad. Independent of [padHaptics] —
     * the host sends the two as separate streams and either can play alone. Off by default: the
     * speaker is a small, easily-startling loudspeaker in the user's hands, and unlike haptics it
     * duplicates audio they are already hearing.
     */
    val padSpeaker: Boolean = false,

    /**
     * How a physical mouse drives the host — the cross-client mouse model (see [MouseMode]).
     * [MouseMode.DESKTOP] (default here) points absolutely; [MouseMode.CAPTURE] locks the pointer
     * to the stream ([android.view.View.requestPointerCapture]) and forwards raw relative motion.
     * Read once per session by StreamScreen; Ctrl+Alt+Shift+Q flips the capture live either way.
     */
    val mouseMode: MouseMode = MouseMode.DESKTOP,

    /**
     * Flip scroll direction — the mouse wheel and the two-finger touch scroll both. Parity with
     * the Apple/GTK clients' "Invert scroll direction".
     */
    val invertScroll: Boolean = false,
    // NOTE: clipboard sync is NOT here. It is a decision about a HOST, not about this device or
    // this stream (design/client-settings-profiles.md §3, tier H), so it lives on the host record
    // — see `KnownHost.clipboardSync`. It used to be a global here; `KnownHostStore.migrate`
    // copied that value onto every saved host and retired the key.
)

/** [Settings.touchMode] values; persisted by name. */
enum class TouchMode { TRACKPAD, POINTER, TOUCH }

/**
 * How a physical mouse drives the host — the cross-client mouse model (the Rust `MouseMode`,
 * persisted as the same lowercase names). Only meaningful with a mouse attached.
 * - [CAPTURE] — pointer lock: relative deltas, the local cursor hidden, the host's cursor the only
 *   one you see. The game model, and the desktop clients' default.
 * - [DESKTOP] — uncaptured absolute pointing: the cursor enters and leaves the stream freely. The
 *   remote-desktop model, and Android's default (a phone/TV is far more often driven by touch or a
 *   pad than by a locked mouse, and this is what the platform did before the setting existed).
 */
enum class MouseMode(val storedName: String, val label: String) {
    CAPTURE("capture", "Capture (games)"),
    DESKTOP("desktop", "Desktop (absolute)"),
}

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
        gamepadForwarding = prefs.getBoolean(K_GAMEPAD_FORWARDING, true),
        systemButtons = prefs.getString(K_SYSTEM_BUTTONS, "auto") ?: "auto",
        guideGesture = prefs.getString(K_GUIDE_GESTURE, "auto") ?: "auto",
        audioChannels = prefs.getInt(K_AUDIO_CH, 2),
        audioFormat = prefs.getString(K_AUDIO_FORMAT, AUDIO_FORMAT_OPUS) ?: AUDIO_FORMAT_OPUS,
        codec = prefs.getString(K_CODEC, "auto") ?: "auto",
        micEnabled = prefs.getBoolean(K_MIC, false),
        echoCancel = prefs.getBoolean(K_ECHO_CANCEL, true),
        keepHostAudio = prefs.getBoolean(K_KEEP_HOST_AUDIO, false),
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
        reduceUiResolution = prefs.getBoolean(K_REDUCE_UI_RES, false),
        gamepadUiMode = prefs.getString(K_GAMEPAD_UI_MODE, GAMEPAD_UI_WHEN_CONNECTED)
            ?: GAMEPAD_UI_WHEN_CONNECTED,
        libraryEnabled = prefs.getBoolean(K_LIBRARY, true),
        uiPalette = prefs.getString(K_UI_PALETTE, "violet") ?: "violet",
        lowLatencyMode = prefs.getBoolean(K_LOW_LATENCY, true),
        presentPriority = prefs.getString(K_PRESENT_PRIORITY, "latency") ?: "latency",
        smoothBuffer = prefs.getInt(K_SMOOTH_BUFFER, 0),
        autoWakeEnabled = prefs.getBoolean(K_AUTO_WAKE, true),
        rumbleOnPhone = prefs.getBoolean(K_RUMBLE_ON_PHONE, false),
        gyroOnPhone = prefs.getBoolean(K_GYRO_ON_PHONE, false),
        sc2Capture = prefs.getBoolean(K_SC2_CAPTURE, true),
        dsCapture = prefs.getBoolean(K_DS_CAPTURE, true),
        padHaptics = prefs.getBoolean(K_PAD_HAPTICS, true),
        padSpeaker = prefs.getBoolean(K_PAD_SPEAKER, false),
        mouseMode = prefs.getString(K_MOUSE_MODE, null)
            ?.let { name -> MouseMode.entries.firstOrNull { it.storedName == name } }
            // Migration: the pre-enum Boolean "pointer_capture" (true = lock the pointer). Its
            // default was false, which IS `desktop` — so an install that never touched the toggle
            // lands where it already was.
            ?: if (prefs.getBoolean(K_POINTER_CAPTURE, false)) MouseMode.CAPTURE else MouseMode.DESKTOP,
        invertScroll = prefs.getBoolean(K_INVERT_SCROLL, false),
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
            .putBoolean(K_GAMEPAD_FORWARDING, s.gamepadForwarding)
            .putString(K_SYSTEM_BUTTONS, s.systemButtons)
            .putString(K_GUIDE_GESTURE, s.guideGesture)
            .putInt(K_AUDIO_CH, s.audioChannels)
            .putString(K_AUDIO_FORMAT, s.audioFormat)
            .putString(K_CODEC, s.codec)
            .putBoolean(K_MIC, s.micEnabled)
            .putBoolean(K_ECHO_CANCEL, s.echoCancel)
            .putBoolean(K_KEEP_HOST_AUDIO, s.keepHostAudio)
            .putString(K_STATS_VERBOSITY, s.statsVerbosity.name)
            .putString(K_TOUCH_MODE, s.touchMode.name)
            .putBoolean(K_GAMEPAD_UI, s.gamepadUiEnabled)
            .putBoolean(K_REDUCE_UI_RES, s.reduceUiResolution)
            .putString(K_GAMEPAD_UI_MODE, s.gamepadUiMode)
            .putBoolean(K_LIBRARY, s.libraryEnabled)
            .putString(K_UI_PALETTE, s.uiPalette)
            .putBoolean(K_LOW_LATENCY, s.lowLatencyMode)
            .putString(K_PRESENT_PRIORITY, s.presentPriority)
            .putInt(K_SMOOTH_BUFFER, s.smoothBuffer)
            .putBoolean(K_AUTO_WAKE, s.autoWakeEnabled)
            .putBoolean(K_RUMBLE_ON_PHONE, s.rumbleOnPhone)
            .putBoolean(K_GYRO_ON_PHONE, s.gyroOnPhone)
            .putBoolean(K_SC2_CAPTURE, s.sc2Capture)
            .putBoolean(K_DS_CAPTURE, s.dsCapture)
            .putBoolean(K_PAD_HAPTICS, s.padHaptics)
            .putBoolean(K_PAD_SPEAKER, s.padSpeaker)
            .putString(K_MOUSE_MODE, s.mouseMode.storedName)
            .putBoolean(K_INVERT_SCROLL, s.invertScroll)
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
        const val K_GAMEPAD_FORWARDING = "gamepad_forwarding"
        const val K_SYSTEM_BUTTONS = "system_buttons"
        const val K_GUIDE_GESTURE = "guide_gesture"
        const val K_AUDIO_CH = "audio_channels"
        const val K_AUDIO_FORMAT = "audio_format"
        const val K_CODEC = "codec"
        const val K_MIC = "mic_enabled"
        const val K_ECHO_CANCEL = "echo_cancel"
        const val K_KEEP_HOST_AUDIO = "keep_host_audio"
        const val K_STATS_VERBOSITY = "stats_verbosity"

        /** Pre-tier Boolean the [K_STATS_VERBOSITY] enum replaced — read once for migration, never
         * written. */
        const val K_HUD = "stats_hud_enabled"
        const val K_TOUCH_MODE = "touch_mode"
        const val K_GAMEPAD_UI = "gamepad_ui_enabled"
        const val K_REDUCE_UI_RES = "reduce_ui_resolution"
        const val K_GAMEPAD_UI_MODE = "gamepad_ui_mode"
        const val K_LIBRARY = "library_enabled"
        const val K_UI_PALETTE = "ui_palette"

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
        const val K_PRESENT_PRIORITY = "present_priority"
        const val K_SMOOTH_BUFFER = "smooth_buffer"
        const val K_AUTO_WAKE = "auto_wake_enabled"
        const val K_RUMBLE_ON_PHONE = "rumble_on_phone"
        const val K_GYRO_ON_PHONE = "gyro_on_phone"
        const val K_SC2_CAPTURE = "sc2_capture"
        const val K_DS_CAPTURE = "ds_capture"
        const val K_PAD_HAPTICS = "pad_haptics"
        const val K_PAD_SPEAKER = "pad_speaker"
        const val K_MOUSE_MODE = "mouse_mode"

        /** Legacy Boolean the [K_MOUSE_MODE] enum replaced — read once for migration, never written. */
        const val K_POINTER_CAPTURE = "pointer_capture"
        const val K_INVERT_SCROLL = "invert_scroll"

        /** Legacy Boolean the enum replaced — read once as the migration default, never written. */
        const val K_TRACKPAD = "trackpad_mode"
    }
}

/**
 * The display to probe for capability/mode queries: the context's own display when it is already
 * associated with one, else the DEFAULT display via [DisplayManager]. A `punktfunk://` deep-link
 * COLD start can reach the connect before the activity is attached to its display —
 * `context.display` then throws, and the old `false`/1080p60 fallbacks silently downgraded the
 * whole session (no HDR advertised / non-native mode) with nothing in the log. The default
 * display IS the panel on phones and TVs; the activity-display distinction only matters on
 * multi-display setups, where the attached path still wins whenever it is available.
 */
private fun probeDisplay(context: Context): Display? =
    runCatching { context.display }.getOrNull()
        ?: runCatching {
            context.getSystemService(DisplayManager::class.java)
                ?.getDisplay(Display.DEFAULT_DISPLAY)
        }.getOrNull().also {
            if (it != null) Log.i("punktfunk", "display probe: context unattached — using DEFAULT_DISPLAY")
        }

/**
 * The device's native display mode as a landscape `(width, height, hz)` — the long edge is the
 * width, since we stream a desktop. Falls back to 1920×1080@60 if no display can be read at all
 * (see [probeDisplay] for the cold-start fallback that makes that a last resort).
 */
fun nativeDisplayMode(context: Context): Triple<Int, Int, Int> {
    val display = probeDisplay(context) ?: return Triple(1920, 1080, 60)
    val mode = display.mode
    val w = mode.physicalWidth
    val h = mode.physicalHeight
    // ROUNDED, not truncated: TVs report the fractional NTSC rates over HDMI (59.94, 29.97,
    // 23.976), and `toInt()` turns 59.94 into 59 — a rate no display mode anywhere has, which the
    // host then serves by clamping DOWN to the highest mode it advertises at or below it. Rounding
    // also keeps this agreeing with `MainActivity.streamPanelFps`, which already rounds; the two
    // describe the same panel and must not disagree.
    val hz = kotlin.math.round(mode.refreshRate).toInt().coerceAtLeast(1)
    return Triple(maxOf(w, h), minOf(w, h), hz)
}

/**
 * Sentinel [Settings.width]/[Settings.height] meaning "the native mode, narrowed so the picture
 * clears the display cutout and the rounded corners" — resolved at connect by [safeDisplayMode],
 * exactly as `0` is resolved by [nativeDisplayMode]. Negative, so it can never collide with a real
 * size; distinct from the UI's `-1` "Custom…" sentinel.
 */
const val SAFE_AREA_MODE = -2

/**
 * Safe-area stream geometry — the pure part, so it is unit-testable without a Display.
 *
 * The phone clips the picture in HARDWARE: the cutout (notch / punch-hole) and the four rounded
 * corners eat whatever the stream draws under them. [StreamScreen] deliberately draws edge-to-edge
 * (`LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS`) and centres the video at its own aspect ratio
 * (`Modifier.aspectRatio`), so which pixels survive is decided purely by the mode's aspect:
 *
 *  * A 16:9 mode on a 20:9 phone pillarboxes, and those black bars land exactly on the unsafe
 *    regions — which is why the presets have always "just worked".
 *  * The NATIVE mode has the panel's own aspect, so it fills every pixel, cutout and corners
 *    included. That is the mode that loses its corners.
 *
 * So asking the host for a mode narrower by the unsafe inset is the entire fix: the existing
 * aspect-fit centres it inside the safe region, and pointer mapping follows for free (MouseInput
 * derives the picture rect from the live video size, not from the window).
 */
object SafeArea {
    /** The host rejects odd dimensions and anything under 320 px wide (`validate_dimensions`). */
    const val MIN_WIDTH = 320

    /**
     * [nativeWidth] reduced by [perSideInsetPx] on each side, even-floored and clamped to the
     * host's floor. Height is deliberately untouched: under aspect-fit only one axis can bind, and
     * on a landscape phone that axis is always the horizontal one — insetting height as well would
     * shrink the picture without uncovering anything.
     */
    fun insetWidth(nativeWidth: Int, perSideInsetPx: Int): Int {
        val inset = perSideInsetPx.coerceAtLeast(0)
        return (nativeWidth - inset * 2).coerceAtLeast(MIN_WIDTH) / 2 * 2
    }
}

/**
 * The per-side inset, in pixels, that the **landscape** stream must clear on this display.
 *
 * Two contributions, and the larger wins:
 *  * **The cutout.** [DisplayCutout] is rotation-aware, so in landscape the housing shows up on
 *    `left`/`right`. The settings screen may be portrait though, where the very same housing is
 *    reported on `top`/`bottom` and the horizontal insets read zero — which would compute "no inset
 *    needed" for exactly the devices that need one. The stream is always landscape, so a vertical
 *    inset now becomes a horizontal one then: fall back to it.
 *  * **The rounded corners.** These are NOT part of the cutout insets. For a FULL-HEIGHT picture the
 *    horizontal clearance a corner of radius `r` needs is exactly `r`: at the topmost row the
 *    display boundary sits at `x = r`, so anything left of that is clipped. Not conservative — it is
 *    the precise requirement for a picture that spans the full height.
 *
 * `0` when the display has neither, which makes the safe mode identical to the native one.
 */
private fun displaySideInsetPx(context: Context): Int {
    val display = probeDisplay(context) ?: return 0
    var inset = 0
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
        display.cutout?.let { cut ->
            val horizontal = maxOf(cut.safeInsetLeft, cut.safeInsetRight)
            val vertical = maxOf(cut.safeInsetTop, cut.safeInsetBottom)
            inset = maxOf(inset, if (horizontal > 0) horizontal else vertical)
        }
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        for (position in intArrayOf(
            android.view.RoundedCorner.POSITION_TOP_LEFT,
            android.view.RoundedCorner.POSITION_TOP_RIGHT,
            android.view.RoundedCorner.POSITION_BOTTOM_LEFT,
            android.view.RoundedCorner.POSITION_BOTTOM_RIGHT,
        )) {
            display.getRoundedCorner(position)?.let { inset = maxOf(inset, it.radius) }
        }
    }
    return inset
}

/**
 * The native mode narrowed to clear the cutout and the rounded corners — the [SAFE_AREA_MODE]
 * resolution, as a landscape `(width, height, hz)`. Same height and refresh as [nativeDisplayMode];
 * only the width moves.
 */
fun safeDisplayMode(context: Context): Triple<Int, Int, Int> {
    val (w, h, hz) = nativeDisplayMode(context)
    return Triple(SafeArea.insetWidth(w, displaySideInsetPx(context)), h, hz)
}

/**
 * True when this device's display can actually present HDR10, so we should advertise HDR to the
 * host. On an SDR panel we advertise `0` instead — the host then sends a proper 8-bit BT.709 stream
 * rather than BT.2020 PQ the panel would mis-tone-map (the washed-out/dark failure). Mirrors the
 * capability gate the Apple/Windows clients apply.
 */
fun displaySupportsHdr(context: Context): Boolean {
    val display = probeDisplay(context)
    if (display == null) {
        // Distinguishable from a real SDR verdict — a silent `false` here cost an HDR session.
        Log.w("punktfunk", "display HDR probe: no display reachable — advertising SDR")
        return false
    }
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

/**
 * Resolve [Settings] (with its `0`=native and [SAFE_AREA_MODE] placeholders) to the concrete mode to
 * request. The safe-area sentinel is checked first because it resolves BOTH axes together — it is one
 * mode, not an independent width and height, and mixing half of it with a native height would ask
 * for a size neither sentinel means.
 */
fun Settings.effectiveMode(context: Context): Triple<Int, Int, Int> {
    val base = if (width == SAFE_AREA_MODE && height == SAFE_AREA_MODE) {
        safeDisplayMode(context)
    } else {
        nativeDisplayMode(context)
    }
    val w = if (width > 0) width else base.first
    val h = if (height > 0) height else base.second
    val hz = if (hz > 0) hz else base.third
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

/** (width, height, label). `(0,0)` = native display; [SAFE_AREA_MODE] = native minus the cutout. */
val RESOLUTION_OPTIONS = listOf(
    Triple(0, 0, "Native display"),
    Triple(SAFE_AREA_MODE, SAFE_AREA_MODE, "Native display (safe area)"),
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

/** Opus 48 kHz — the default, and byte-for-byte the session every earlier build ran. */
const val AUDIO_FORMAT_OPUS = "opus"

/**
 * Bit-exact PCM at 44.1 kHz / 24-bit (~2.1 Mbps). The CD family's base rate: what an ordinary
 * Windows endpoint or a 44.1 kHz interface reports as its own engine rate, and the request that
 * spares such a host a resample it would otherwise do on the way out.
 */
const val AUDIO_FORMAT_LOSSLESS_441 = "lossless441"

/**
 * Bit-exact PCM at 48 kHz / 24-bit (~2.3 Mbps). The honest win even without a hi-res interface:
 * no lossy stage at all, and no double resample on a host whose engine already runs at 48 kHz.
 */
const val AUDIO_FORMAT_LOSSLESS_48 = "lossless48"

/** Bit-exact PCM at 88.2 kHz / 24-bit (~4.2 Mbps) — 96 kHz's counterpart in the 44.1 family. */
const val AUDIO_FORMAT_LOSSLESS_882 = "lossless882"

/**
 * Bit-exact PCM at 96 kHz / 24-bit (~4.6 Mbps), and only real if the host's capture endpoint
 * genuinely runs at 96 kHz — the host declines rather than upsampling to meet the request.
 */
const val AUDIO_FORMAT_LOSSLESS_96 = "lossless96"

/**
 * Bit-exact PCM at 176.4 kHz / 24-bit — **8.5 Mbps**, and the one row far more likely to be
 * declined than granted. Three separate things have to go right: the host's bandwidth gate gives
 * audio at most a quarter of the video budget, so the session needs ~34 Mbps of video before it
 * will even consider it; a stereo frame only fits a QUIC datagram on the ladder's shortest rung
 * (1 ms — a thousand datagrams a second — at ~1 069 B, so the first connection with a smaller
 * datagram declines it), and a surround one fits no rung at all; and very few Android outputs will
 * open the rate, which the native probe settles before the handshake. Offered because it is
 * reachable, not because it is likely — the HUD's `audio lossless …` line is what says which
 * happened.
 */
const val AUDIO_FORMAT_LOSSLESS_1764 = "lossless1764"

/**
 * (stored value, label) for the requested audio format — the cross-client table, matching the
 * Apple client's `AudioFormatChoice` raw values and the desktop `AUDIO_FORMATS` so a profile
 * written on any of them is honoured on the others.
 *
 * ⚠ **The stored values are shared VERBATIM and must never be renamed.** A profile carries the key
 * through untouched, so a spelling that differs by one character fails in the worst possible way:
 * the profile keeps "working" on the other client and silently inherits its global default
 * instead. The naming rule is the kHz figure with the decimal point dropped — `lossless48`,
 * `lossless96`, and for the 44.1 family `lossless441` / `lossless882` / `lossless1764`.
 *
 * **Both rate families are here now.** They were not: every buffer figure in the shared jitter
 * policy used to be `ms × perMs` with `perMs` an INTEGER number of samples per millisecond, which
 * made 44 100 → 44.1 truncate to 44 — a silent 2.3 % error in every target, every de-prime fuse
 * and every reported buffer depth, and the whole reason the 44.1 family was deferred rather than
 * refused (design/hi-res-audio.md §4.1). Core now multiplies before it divides, which is exact at
 * every rate, so the deferral is lifted.
 *
 * A row being offered is not a promise it can be delivered: the host's gate, this device's own
 * output, and the path MTU each get a veto, and the ones at the top of the list get vetoed often.
 * What actually happened is on the HUD.
 *
 * Lossless at **16**-bit is deliberately absent at every rate: it spends ~1.4–1.5 Mbps to sound
 * like the transparent 256 kbps Opus it replaces, and it is the one lossless request whose wire
 * parameters are indistinguishable from a legacy one. 24-bit is where the plane earns its
 * bandwidth.
 */
val AUDIO_FORMAT_OPTIONS = listOf(
    AUDIO_FORMAT_OPUS to "Standard (Opus)",
    AUDIO_FORMAT_LOSSLESS_441 to "Lossless 44.1 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_48 to "Lossless 48 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_882 to "Lossless 88.2 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_96 to "Lossless 96 kHz / 24-bit",
    AUDIO_FORMAT_LOSSLESS_1764 to "Lossless 176.4 kHz / 24-bit",
)

/**
 * The `(rateHz, bits)` pair [audioFormat] asks the host for, in `nativeConnect`'s terms.
 *
 * ⚠⚠ **Opus is `0`/`0`, the "did not ask" sentinel — NOT `48000`/`16`.** Core sets
 * `CLIENT_CAP_AUDIO_HIRES` when either field is non-zero, because it keys on *a format was
 * specified* rather than *the format differs from the default*: 48 kHz/16-bit is the cheapest
 * lossless rung as well as the legacy pair, so the other rule would make it the one rung nobody
 * could ask for. Sending `48000`/`16` for a user who chose Standard therefore advertises the
 * capability, and the host then hands that user 1.5 Mbps of lossless PCM instead of 256 kbps of
 * Opus. This returned that pair until all four clients were compared.
 *
 * ⚠⚠ **That bug got worse on 2026-08-17, when the host's `PUNKTFUNK_AUDIO_HIRES` gate went
 * default-ON.** It used to need a host whose operator had opted in — rare, so a slip here would
 * have been survivable and probably unnoticed. The blast radius is now every host that has not
 * deliberately opted out, i.e. all of them. The zeroes below are load-bearing.
 *
 * The zeroes are also what keeps a default `Hello` byte-identical to a pre-lossless one — the wire
 * encodes an explicit 48 000/16 the same as absent, and the whole difference is the capability bit.
 *
 * Deriving the pair FROM the stored format is what stops the two ever disagreeing. An unrecognized
 * stored value — a newer build's, or a corrupted pref — resolves to Opus rather than blocking the
 * connect.
 *
 * The rate this returns is only the REQUEST. The native side runs it down a fallback ladder first
 * (`session::connect::rate_fallback_ladder`), because AAudio grants an explicitly-asked rate or
 * fails the open and never substitutes — so a rate this device cannot play must never reach the
 * wire.
 */
fun Settings.audioFormatWire(): Pair<Int, Int> = when (audioFormat) {
    AUDIO_FORMAT_LOSSLESS_441 -> 44_100 to 24
    AUDIO_FORMAT_LOSSLESS_48 -> 48_000 to 24
    AUDIO_FORMAT_LOSSLESS_882 -> 88_200 to 24
    AUDIO_FORMAT_LOSSLESS_96 -> 96_000 to 24
    AUDIO_FORMAT_LOSSLESS_1764 -> 176_400 to 24
    else -> AUDIO_FORMAT_WIRE_UNSPECIFIED
}

/**
 * The `(rateHz, bits)` that mean "this session is not asking for the lossless plane" — see
 * [audioFormatWire] for why it is a pair of zeroes rather than the legacy 48 000/16.
 */
val AUDIO_FORMAT_WIRE_UNSPECIFIED = 0 to 0

/**
 * (stored value, label) for the preferred video codec — the cross-client table (the Rust
 * `CODECS`), so a value another client or a profile stored is always representable here.
 * `"auto"` = host decides.
 *
 * Two rows are capability-gated by [codecOptionsFor] rather than dropped from the table: `"av1"`
 * needs a real `video/av01` decoder on this device, and `"pyrowave"` needs a PyroWave decoder,
 * which this platform does not have at all (it is a Vulkan-compute codec living in `pf-presenter`;
 * the JNI client decodes through MediaCodec and never advertises the bit, so preferring it would
 * be a dead setting that silently resolves to HEVC).
 */
val CODEC_OPTIONS = listOf(
    "auto" to "Automatic",
    "hevc" to "HEVC (H.265)",
    "h264" to "H.264 (AVC)",
    "av1" to "AV1",
    "pyrowave" to "PyroWave (wired LAN)",
)

/**
 * [CODEC_OPTIONS] minus the rows this device can't decode — a preference the client never
 * advertises is a setting that does nothing. [stored] is the currently persisted value, which is
 * always kept selectable so the selection can be rendered (the don't-clobber rule: a codec chosen
 * on another device, or by a newer build, must survive being looked at here).
 */
fun codecOptionsFor(stored: String, av1Capable: Boolean): List<Pair<String, String>> =
    CODEC_OPTIONS.filter { (v, _) ->
        when (v) {
            "av1" -> av1Capable || stored == "av1"
            "pyrowave" -> stored == "pyrowave" // no PyroWave decoder on Android — see above
            else -> true
        }
    }

/** Resolved [Settings.systemButtons]: forward the raw guide/misc presses? Auto = forward on
 * Android — the press reaches the app on most devices, and where the shell shows its own UI
 * for it that's the shell's business. */
fun Settings.systemButtonsForward(): Boolean = systemButtons != "local"

/** Resolved [Settings.guideGesture]: auto = OFF on Android (the raw press already reaches the
 * host); "on" is for devices whose shell intercepts the physical guide button. */
fun Settings.guideGestureEnabled(): Boolean = guideGesture == "on"

/** The [Settings.codec] string as a `quic::CODEC_*` preference byte (`0` = auto). H264=1, HEVC=2,
 * AV1=4, PyroWave=8 (never decodable here, but the byte is the shared contract). */
fun Settings.preferredCodec(): Int = when (codec) {
    "h264" -> 1
    "hevc" -> 2
    "av1" -> 4
    "pyrowave" -> 8
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

/** [Settings.presentPriority] as the wire int `nativeStartVideo` takes (0 = latency, 1 = smooth).
 * Unrecognized values resolve to latency — same rule as the Apple client. */
fun Settings.presentPriorityWire(): Int = if (presentPriority == "smooth") 1 else 0

/** (stored value, label) for the presenter-intent picker — the Apple client's table verbatim. */
val PRESENT_PRIORITY_OPTIONS = listOf(
    "latency" to "Lowest latency",
    "smooth" to "Smoothness",
)

/** (frames, label) for the smoothness-buffer picker; each buffered frame ≈ one refresh interval
 * of jitter absorbed for one interval of added display latency ([hz] labels the cost). */
fun smoothBufferOptions(hz: Int): List<Pair<Int, String>> {
    val periodMs = 1000.0 / maxOf(24, hz)
    fun cost(frames: Int) = "+%.0f ms".format(periodMs * frames)
    return listOf(
        0 to "Automatic",
        1 to "1 frame (${cost(1)})",
        2 to "2 frames (${cost(2)})",
        3 to "3 frames (${cost(3)})",
    )
}

/** (stored value, label) for when the console UI takes over — the Apple client's table verbatim.
 * Only offered while [Settings.gamepadUiEnabled] is on; a TV is in console mode either way. */
val GAMEPAD_UI_MODE_OPTIONS = listOf(
    GAMEPAD_UI_WHEN_CONNECTED to "With a controller",
    GAMEPAD_UI_ALWAYS to "Always",
)

/** (mode, label) for the touch-input model. */
val TOUCH_MODE_OPTIONS = listOf(
    TouchMode.TRACKPAD to "Trackpad",
    TouchMode.POINTER to "Direct pointer",
    TouchMode.TOUCH to "Touch passthrough",
)

/** (mode, label) for the physical-mouse model. */
val MOUSE_MODE_OPTIONS = MouseMode.entries.map { it to it.label }

/**
 * (GamepadPref wire byte, label) for the emulated pad the host creates. NOT positional: the wire
 * bytes are `punktfunk_core::config::GamepadPref` (see `Gamepad.PREF_*`), and Steam Deck is `6`
 * with `5` (the classic Steam Controller) deliberately not offered — the same subset the desktop
 * clients' picker shows.
 */
val GAMEPAD_OPTIONS = listOf(
    io.unom.punktfunk.kit.Gamepad.PREF_AUTO to "Automatic",
    io.unom.punktfunk.kit.Gamepad.PREF_XBOX360 to "Xbox 360",
    io.unom.punktfunk.kit.Gamepad.PREF_DUALSENSE to "DualSense",
    io.unom.punktfunk.kit.Gamepad.PREF_XBOXONE to "Xbox One",
    io.unom.punktfunk.kit.Gamepad.PREF_DUALSHOCK4 to "DualShock 4",
    io.unom.punktfunk.kit.Gamepad.PREF_STEAMDECK to "Steam Deck",
)

/** (stored `system_buttons` value, label) — where the guide/share presses land while streaming. */
val SYSTEM_BUTTON_OPTIONS = listOf(
    "auto" to "Automatic",
    "forward" to "Send to host",
    "local" to "This device",
)

/** (stored `guide_gesture` value, label) — the hold-Select guide gesture. */
val GUIDE_GESTURE_OPTIONS = listOf(
    "auto" to "Automatic",
    "on" to "On",
    "off" to "Off",
)
