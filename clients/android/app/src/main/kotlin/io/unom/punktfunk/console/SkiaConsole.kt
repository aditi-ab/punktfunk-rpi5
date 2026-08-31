package io.unom.punktfunk.console

import android.app.ActivityManager
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.InputDevice
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.CONNECT_TIMEOUT_MS
import io.unom.punktfunk.ConnectErrors
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.ProfileStore
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsStore
import io.unom.punktfunk.StreamProfile
import io.unom.punktfunk.connectToHost
import io.unom.punktfunk.deviceName
import io.unom.punktfunk.effectiveFor
import io.unom.punktfunk.matches
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONArray
import org.json.JSONObject

/**
 * The Skia console (`crates/pf-console-ui`, drawn by native over EGL — design
 * `android-skia-console-port.md`) as this app holds it: ONE instance for the process, created
 * lazily and never torn down while the app lives, so the console's screen stack survives a trip
 * through the stream exactly as the desktop's does (the shelf is where you left it when the game
 * exits). [SkiaConsoleShell] attaches a surface, the pad probes and the overlays to it while the
 * console is on screen; between, it idles parked.
 *
 * This object is the SERVICE side of the console's model (`ConsoleShared` / `LibraryShared` /
 * `ConsoleBus`): it feeds host rows from the trust store + discovery + the reachability probe,
 * runs the library fetch/cache/art pipeline, pairing, wake-and-wait, and the settings round-trip,
 * and turns the console's own asks (`OverlayAction`) into a connect, a clipboard write, or a
 * task-to-back. Everything blocking runs on its own executor; every native call is cheap.
 */
object SkiaConsole {
    private const val TAG = "pf.console"

    /**
     * On-glass triage switch: `adb shell setprop debug.punktfunk.console_backend none` makes the
     * app behave as if the native console host were absent (the touch UI fronts everything, a
     * controller drives it through Compose focus). Anything else = the console.
     */
    private const val BACKEND_PROP = "debug.punktfunk.console_backend"

    /** Where the console-owned settings keys (`library_view`, `reduce_motion`, …) persist. */
    private const val PREFS = "punktfunk_console_settings"

    private var handle = 0L

    /**
     * False once the console has proven it cannot draw — the native create failed, or the render
     * thread died (a GL context that never came up, or one Android reclaimed and that would not
     * come back). Compose observes it: `App` folds it into the gamepad-UI gate, so the answer to a
     * dead console is the touch UI — not the gray, never-painted `SurfaceView` the shell would
     * otherwise sit on for the rest of the process.
     */
    var healthy by mutableStateOf(true)
        private set

    private var appContext: Context? = null
    private val main = Handler(Looper.getMainLooper())
    private val ioPool = Executors.newCachedThreadPool { r -> Thread(r, "pf-console-io").apply { isDaemon = true } }
    private val artPool = Executors.newFixedThreadPool(3) { r -> Thread(r, "pf-console-art").apply { isDaemon = true } }
    private val artHttp by lazy { OkHttpClient() }
    private var eventThread: Thread? = null
    private val running = AtomicBoolean(false)

    // Services.
    private lateinit var knownHostStore: KnownHostStore
    private lateinit var profileStore: ProfileStore
    private lateinit var settingsStore: SettingsStore
    private var identity: ClientIdentity? = null
    private var discovery: HostDiscovery? = null
    private var discovered: List<DiscoveredHost> = emptyList()
    private var reachable: Set<String> = emptySet()
    private var settings: Settings = Settings()

    /** What each paired host last said this device may do TO it, by fingerprint, and when we
     *  last asked — the Android half of the desktop's shared actions cache. Main-thread only. */
    private val hostActions = mutableMapOf<String, List<HostActions.Action>>()
    private val hostActionsAt = mutableMapOf<String, Long>()

    // What the composable hands us while it is on screen.
    private var onConnected: ((ActiveSession) -> Unit)? = null
    private var onSettingsChange: ((Settings) -> Unit)? = null
    private var onQuit: (() -> Unit)? = null
    private var onPlatformScreen: ((String) -> Unit)? = null
    private var onPadAction: ((String, String) -> Unit)? = null
    private var onPulse: ((String) -> Unit)? = null

    /** The connect in flight, if any — cancelable through `OverlayAction::CancelConnect`. */
    private class Dial(val cancelled: AtomicBoolean = AtomicBoolean(false))
    private var dial: Dial? = null

    /** The wake-and-wait loop in flight, if any. */
    private var wakeGen = AtomicLong(0)

    /** The library fetch in flight (its generation; a newer one supersedes it). */
    private val fetchGen = AtomicLong(0)

    // ---- availability -------------------------------------------------------------------

    /**
     * Whether the console can front the gamepad UI on this device: the native host must be in
     * this build (every shipping ABI today — see `nativeConsoleAvailable`) and the triage sysprop
     * must not say `none`.
     */
    fun wanted(): Boolean {
        val available = runCatching { NativeBridge.nativeConsoleAvailable() }.getOrDefault(false)
        if (!available) return false
        return backendProp() != "none"
    }

    /**
     * Why the console cannot front the gamepad UI here, or null when it can — for the settings
     * screen to print under the switch that asks for it.
     *
     * `App` gates the console on `wanted() && healthy` on top of the user's own setting, and those
     * two terms are the ONLY ones that can veto "Always": the mode, the attached pad, the TV check
     * and the dev flag are ORed together, so a device where the console never comes up ignores
     * every one of them. Until this existed that produced a switch the app silently disobeyed —
     * indistinguishable, from the outside, from the switch itself being broken, and it is what a
     * report of "the gamepad UI just doesn't activate, even on Always, even with a controller"
     * looks like. Reads [healthy] as Compose state, so the note clears itself if it ever recovers.
     */
    fun unavailable(): String? = when {
        !wanted() -> "This device has no console UI in this build, so the touch layout stays up."
        !healthy -> "The console UI couldn't start on this device, so the touch layout is " +
            "standing in. Restart the app to try again — and if it keeps happening, send this " +
            "host your logs from a saved host's ⋮ menu."
        else -> null
    }

    private fun backendProp(): String = runCatching {
        val cls = Class.forName("android.os.SystemProperties")
        cls.getMethod("get", String::class.java, String::class.java)
            .invoke(null, BACKEND_PROP, "") as String
    }.getOrDefault("").trim().lowercase()

    // ---- lifecycle -----------------------------------------------------------------------

    /**
     * Build the console if it does not exist yet. Idempotent; call from the main thread. Returns
     * the native handle (`0` = the console could not be built; the caller keeps the Compose
     * console).
     */
    fun ensure(context: Context, initial: Settings): Long {
        if (handle != 0L) return handle
        val app = context.applicationContext
        appContext = app
        knownHostStore = KnownHostStore(app)
        profileStore = ProfileStore(app)
        settingsStore = SettingsStore(app)
        settings = initial
        val prefs = app.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val base = prefs.getString("json", null)?.let { runCatching { JSONObject(it) }.getOrNull() }
        val profiles = profileStore.all()
        val opts = JSONObject()
            .put("device_name", deviceName(app))
            .put("gpu_cache_bytes", gpuCacheBytes(app))
            // The touch shell exists as a fallback on phones/tablets but not on a TV —
            // gates the console's own "Controller-optimized UI" off switch.
            .put("fallback_ui", !io.unom.punktfunk.isTvDevice(app))
            .put("settings", ConsoleJson.settings(initial, base))
            .put("profiles", JSONArray(ConsoleJson.profiles(profiles)))
            .put("known_hosts", JSONObject(ConsoleJson.knownHosts(knownHostStore.all())))
            .put("entry", JSONObject())
        handle = runCatching { NativeBridge.nativeConsoleCreate(opts.toString()) }.getOrDefault(0L)
        if (handle == 0L) {
            Log.e(TAG, "console: native create failed")
            healthy = false // see [healthy] — the touch UI fronts everything from here
            return 0L
        }
        Log.i(TAG, "console: created (gpu cache ${gpuCacheBytes(app) shr 20} MB)")
        startEventThread()
        startServices(app)
        return handle
    }

    /**
     * Skia's resource budget: a quarter of the desktop's 160 MB on a ≤ 2 GB box, the desktop
     * figure above (design D11 — a 160 MB texture cache is how a TV box gets its process killed).
     */
    private fun gpuCacheBytes(context: Context): Int {
        val am = context.getSystemService(Context.ACTIVITY_SERVICE) as? ActivityManager
        val classMb = am?.memoryClass ?: 128
        return if (classMb >= 256) 160 shl 20 else 64 shl 20
    }

    private fun startServices(app: Context) {
        ioPool.execute {
            identity = runCatching { obtainIdentity(IdentityStore(app)) }
                .onFailure { Log.w(TAG, "identity unavailable: ${it.message}") }
                .getOrNull()
        }
        val d = HostDiscovery(app)
        d.onChange = { list ->
            discovered = list
            // Learn wake MACs / mgmt ports from live adverts, as the desktop service does.
            ioPool.execute {
                var changed = false
                for (dh in list) {
                    val kh = knownHostStore.all().firstOrNull { it.matches(dh) } ?: continue
                    if (dh.mac.isNotEmpty() && dh.mac.toSet() != kh.mac.toSet()) {
                        knownHostStore.learnMac(kh.address, kh.port, dh.mac); changed = true
                    }
                    dh.mgmtPort?.let { if (it != kh.mgmtPort) { knownHostStore.learnMgmtPort(kh.address, kh.port, it); changed = true } }
                    if (dh.os.isNotEmpty() && dh.os != kh.os) { knownHostStore.learnOs(kh.address, kh.port, dh.os); changed = true }
                }
                main.post { pushHosts(); if (changed) pushKnownHosts() }
            }
            pushHosts()
        }
        discovery = d
        d.start()
        // The reachability sweep: saved hosts not on mDNS, every ~12 s (the desktop's cadence).
        main.post(object : Runnable {
            override fun run() {
                if (handle == 0L) return
                // Only while the console is ON SCREEN (attached): parked behind the touch UI
                // or a stream there is nobody to show the presence pips to — and mid-stream
                // the radio belongs to the session, which is exactly why discovery stops for
                // it. The timer keeps ticking so probes resume within a cadence of re-attach.
                if (onConnected == null) {
                    main.postDelayed(this, 12_000)
                    return
                }
                val targets = knownHostStore.all().filter { kh -> discovered.none { kh.matches(it) } }
                ioPool.execute {
                    val up = targets.filter { NativeBridge.nativeProbe(it.address, it.port, 3_000) }
                        .map { "${it.address}:${it.port}" }.toSet()
                    main.post { if (up != reachable) { reachable = up; pushHosts() } }
                }
                main.postDelayed(this, 12_000)
            }
        })
        // Commands from the console, drained on a short cadence.
        main.post(object : Runnable {
            override fun run() {
                if (handle == 0L) return
                drainCommands()
                main.postDelayed(this, 100)
            }
        })
        pushHosts()
    }

    private fun startEventThread() {
        running.set(true)
        eventThread = Thread({
            while (running.get() && handle != 0L) {
                val json = runCatching { NativeBridge.nativeConsoleNextEvent(handle) }.getOrDefault("")
                if (json.isEmpty()) continue
                val ev = runCatching { JSONObject(json) }.getOrNull() ?: continue
                main.post { onEvent(ev) }
            }
        }, "pf-console-events").apply { isDaemon = true; start() }
    }

    // ---- what the composable attaches ------------------------------------------------------

    fun attach(
        onConnected: (ActiveSession) -> Unit,
        onSettingsChange: (Settings) -> Unit,
        onQuit: () -> Unit,
        onPlatformScreen: (String) -> Unit,
        onPadAction: (String, String) -> Unit,
        onPulse: (String) -> Unit,
    ) {
        this.onConnected = onConnected
        this.onSettingsChange = onSettingsChange
        this.onQuit = onQuit
        this.onPlatformScreen = onPlatformScreen
        this.onPadAction = onPadAction
        this.onPulse = onPulse
        discovery?.restart()
        // The touch UI may have paired/forgotten/edited hosts or profiles while we were away.
        pushHosts()
        pushKnownHosts()
        if (handle != 0L) NativeBridge.nativeConsoleSetProfiles(handle, ConsoleJson.profiles(profileStore.all()))
    }

    fun detach() {
        onConnected = null
        onSettingsChange = null
        onQuit = null
        onPlatformScreen = null
        onPadAction = null
        onPulse = null
    }

    /** The touch UI (or a link) changed settings: the console reads the new snapshot next. */
    fun settingsChanged(s: Settings) {
        settings = s
        if (handle == 0L) return
        val prefs = appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val base = prefs?.getString("json", null)?.let { runCatching { JSONObject(it) }.getOrNull() }
        NativeBridge.nativeConsoleSetSettings(handle, ConsoleJson.settings(s, base).toString())
    }

    /** The profile catalog changed (the touch settings edited it). */
    fun profilesChanged() {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetProfiles(handle, ConsoleJson.profiles(profileStore.all()))
        pushHosts()
    }

    /** The host store changed outside the console (touch UI pairing / forget). */
    fun hostsChanged() {
        if (handle == 0L) return
        pushHosts()
        pushKnownHosts()
    }

    /** A session the console started (or any session) has ended; [reason] = the abnormal one. */
    fun sessionEnded(reason: String?) {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSessionPhase(handle, 3, reason.orEmpty())
        discovery?.restart()
    }

    /** Re-root the console on a host's shelf (a game launched from it just exited; a deep link). */
    fun openLibrary(hostId: String, pinId: String?) {
        if (handle == 0L) return
        val kh = knownHostStore.byId(hostId) ?: return
        val profiles = profileStore.all()
        val pin = pinId?.let { id -> profiles.firstOrNull { it.id == id } }
        val entry = JSONObject().put("library", ConsoleJson.hostRow(kh, pin, profiles))
        NativeBridge.nativeConsoleNavigate(handle, entry.toString())
    }

    /**
     * A `punktfunk://` link while the console is up. Named-by-id and pinned is the one-click
     * contract (the same dial the console's own Launch takes); anything that would need a trust
     * decision — or that named the host by a guessable label or address — is a notice here. A link
     * may never establish trust, the console's Pair screen is reached from the host's tile rather
     * than from a URL, and the console draws no prompt this shell could ask a question through.
     */
    fun handleDeepLink(url: String) {
        if (handle == 0L) return
        val parsed = io.unom.punktfunk.kit.link.DeepLinks.parse(url)
        if (parsed is io.unom.punktfunk.kit.link.DeepLinkResult.Refused) {
            if (parsed.error != io.unom.punktfunk.kit.link.LinkError.NOT_OUR_SCHEME) notice(parsed.message())
            return
        }
        val link = (parsed as io.unom.punktfunk.kit.link.DeepLinkResult.Parsed).link
        if (link.route != io.unom.punktfunk.kit.link.LinkRoute.CONNECT) {
            notice("Punktfunk on Android can't do “${link.route.word}” links yet.")
            return
        }
        val profileRef = link.profile
        if (profileRef != null) {
            val (_, resolution) = profileStore.resolve(profileRef)
            if (resolution != io.unom.punktfunk.ProfileResolution.FOUND) {
                notice("That link asks for a profile called “$profileRef”, which isn't on this device.")
                return
            }
        }
        when (val resolved = io.unom.punktfunk.kit.link.DeepLinks.resolveHost(link, knownHostStore.all())) {
            is io.unom.punktfunk.kit.link.HostResolution.Record -> {
                val kh = resolved.host
                if (link.pinConflict(kh)) {
                    notice("That link's fingerprint doesn't match the one pinned for ${kh.name}.")
                    return
                }
                if (kh.fpHex.isEmpty() || !kh.paired) {
                    notice("Pair with ${kh.name} first — a link can't establish trust.")
                    return
                }
                if (resolved is io.unom.punktfunk.kit.link.HostResolution.Confirm) {
                    notice("A link can only dial ${kh.name} by its id — open it from the list.")
                    return
                }
                launch(
                    JSONObject()
                        .put("addr", kh.address).put("port", kh.port).put("fp_hex", kh.fpHex)
                        .put("launch", link.launch ?: JSONObject.NULL)
                        .put("profile", profileRef?.let { profileStore.resolve(it).first?.id } ?: JSONObject.NULL)
                        .put("request_access", false),
                )
            }
            is io.unom.punktfunk.kit.link.HostResolution.Unknown ->
                notice("That link points at a host this device hasn't paired with.")
            io.unom.punktfunk.kit.link.HostResolution.Ambiguous ->
                notice("More than one saved host is called “${link.hostRef}”.")
            io.unom.punktfunk.kit.link.HostResolution.Unresolvable ->
                notice("That link points at a host this device doesn't know.")
        }
    }

    /** The connected controllers, for the chip + settings rows. */
    fun padsChanged(driving: InputDevice?) {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetPads(handle, ConsoleJson.pads(Gamepad.pads(), driving ?: Gamepad.firstPad()))
    }

    // ---- model pushers -----------------------------------------------------------------------

    private fun pushHosts() {
        if (handle == 0L) return
        refreshHostActions()
        NativeBridge.nativeConsoleSetHosts(
            handle,
            ConsoleJson.hostRows(
                knownHostStore.all(), discovered, reachable, profileStore.all(), hostActions,
            ),
        )
    }

    /**
     * Keep each paired, reachable host's advertised actions fresh (`design/host-actions.md` §7),
     * mirroring the desktop's `pf_client_core::host_actions::refresh`.
     *
     * On a slow TTL and never when a menu opens: the row list has to be SETTLED before the menu
     * draws, or rows would appear under a cursor already moving toward something else — and two
     * of those rows shut a machine down.
     */
    private fun refreshHostActions() {
        val id = identity ?: return
        val now = android.os.SystemClock.elapsedRealtime()
        for (h in knownHostStore.all()) {
            if (!h.paired || h.fpHex.isEmpty()) continue
            val online = discovered.any {
                it.fingerprint.equals(h.fpHex, ignoreCase = true) ||
                    (it.host == h.address && it.port == h.port)
            } || "${h.address}:${h.port}" in reachable
            if (!online) continue
            // Stamp BEFORE the request, so a slow host cannot make every push spawn another.
            if (now - (hostActionsAt[h.fpHex] ?: 0L) < HOST_ACTIONS_TTL_MS) continue
            hostActionsAt[h.fpHex] = now
            val (addr, mgmt, fp) = Triple(h.address, h.effectiveMgmtPort, h.fpHex)
            ioPool.execute {
                val found = HostActions.list(id, addr, mgmt, fp)
                main.post {
                    hostActions[fp] = found
                    pushHosts()
                }
            }
        }
    }

    private fun pushKnownHosts() {
        if (handle == 0L) return
        NativeBridge.nativeConsoleSetKnownHosts(handle, ConsoleJson.knownHosts(knownHostStore.all()))
    }

    internal fun notice(text: String) {
        if (handle != 0L) NativeBridge.nativeConsoleNotice(handle, text)
    }

    // ---- events from the console ---------------------------------------------------------

    private fun onEvent(ev: JSONObject) {
        when {
            ev.has("action") -> onAction(ev.get("action"))
            ev.has("pulse") -> onPulse?.invoke(ev.optString("pulse"))
            ev.has("editing") -> {} // the shell draws its own keyboard; nothing to raise here
            ev.has("settings") -> onSettingsSaved(ev.getJSONObject("settings"))
            ev.has("gles") -> Log.i(TAG, "console: GLES ${ev.optInt("gles")}")
            ev.has("dead") -> {
                Log.e(TAG, "console: render thread died: ${ev.optString("dead")}")
                healthy = false // the touch UI takes over; only a process restart tries again
            }
        }
    }

    private fun onSettingsSaved(j: JSONObject) {
        appContext?.getSharedPreferences(PREFS, Context.MODE_PRIVATE)?.edit()
            ?.putString("json", j.toString())?.apply()
        val next = ConsoleJson.applySettings(settings, j)
        if (next != settings) {
            settings = next
            settingsStore.save(next)
            onSettingsChange?.invoke(next)
        }
    }

    private fun onAction(action: Any) {
        when (action) {
            is String -> when (action) {
                "Quit" -> onQuit?.invoke()
                "CancelConnect" -> {
                    dial?.cancelled?.set(true)
                    dial = null
                    discovery?.restart()
                }
            }
            is JSONObject -> {
                action.optJSONObject("Launch")?.let(::launch)
                action.optString("CopyText").takeIf { action.has("CopyText") }?.let { text ->
                    val cm = appContext?.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                    cm?.setPrimaryClip(ClipData.newPlainText("punktfunk", text))
                }
            }
        }
    }

    /**
     * `OverlayAction::Launch` — the console asked for a session. The trust decision was the
     * console's (an unpaired host went to its Pair screen first), so this is the dial itself:
     * pinned by the row's fingerprint, with the host's bound profile or the pinned card's
     * one-off, and — for the pair screen's "Request access" — the long approval budget.
     */
    private fun launch(a: JSONObject) {
        val app = appContext ?: return
        val addr = a.optString("addr")
        val port = a.optInt("port")
        val fp = a.optString("fp_hex")
        val launchId = a.optString("launch").takeIf { a.has("launch") && !a.isNull("launch") && it.isNotEmpty() }
        val profileId = a.optString("profile").takeIf { a.has("profile") && !a.isNull("profile") && it.isNotEmpty() }
        val requestAccess = a.optBoolean("request_access", false)
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSessionPhase(handle, 2, "Identity not ready yet — try again in a moment")
            return
        }
        val kh = knownHostStore.get(addr, port)
        val profile: StreamProfile? = profileStore.resolveFor(kh, profileId)
        val effective = settings.effectiveFor(profile)
        val d = Dial()
        dial = d
        NativeBridge.nativeConsoleSessionPhase(handle, 0, "")
        discovery?.stop() // free the Wi-Fi radio before the stream session
        ioPool.execute {
            val timeout = if (requestAccess) REQUEST_ACCESS_TIMEOUT_MS else CONNECT_TIMEOUT_MS
            val h = kotlinx.coroutines.runBlocking {
                connectToHost(app, effective, id, addr, port, fp, launchId, timeout)
            }
            main.post {
                if (d.cancelled.get()) {
                    if (h != 0L) ioPool.execute { NativeBridge.nativeClose(h) }
                    return@post
                }
                dial = null
                if (h != 0L) {
                    var record = kh
                    // A request-access approval, or a first TOFU-less connect: save the host as
                    // PAIRED, pinning what it presented, so the next connect is silent.
                    if (record == null || (requestAccess && !record.paired)) {
                        val seen = NativeBridge.nativeHostFingerprint(h)
                        if (seen.isNotEmpty()) {
                            val name = record?.name
                                ?: discovered.firstOrNull { it.host == addr && it.port == port }?.name
                                ?: addr
                            record = knownHostStore.trust(addr, port, name, seen, paired = requestAccess || record?.paired == true)
                            pushHosts(); pushKnownHosts()
                        }
                    }
                    if (record != null) {
                        NativeBridge.nativeHostMgmtPort(h).takeIf { it > 0 }?.let {
                            knownHostStore.learnMgmtPort(record.address, record.port, it)
                        }
                    }
                    NativeBridge.nativeConsoleSessionPhase(handle, 1, "")
                    onConnected?.invoke(
                        ActiveSession(
                            h,
                            effective,
                            clipboardSync = record?.clipboardSync ?: false,
                            profileName = profile?.name,
                            hostId = record?.id,
                            launchedFromLibrary = launchId != null,
                            libraryProfileId = profileId,
                        ),
                    )
                } else {
                    val token = NativeBridge.nativeTakeLastError()
                    NativeBridge.nativeConsoleSessionPhase(
                        handle, 2, ConnectErrors.connectMessage(token, requestAccess),
                    )
                    discovery?.restart()
                }
            }
        }
    }

    // ---- commands from the console -----------------------------------------------------

    private fun drainCommands() {
        val arr = runCatching { JSONArray(NativeBridge.nativeConsoleDrainCmds(handle)) }.getOrNull() ?: return
        for (i in 0 until arr.length()) {
            when (val c = arr.opt(i)) {
                is String -> when (c) {
                    "CancelWake" -> { wakeGen.incrementAndGet(); NativeBridge.nativeConsoleSetWake(handle, "null") }
                    "Probe" -> { discovery?.restart(); pushHosts() }
                }
                is JSONObject -> {
                    c.optJSONObject("FetchLibrary")?.let { fetchLibrary(it, refreshOnly = false) }
                    c.optJSONObject("RefreshRunning")?.let { fetchLibrary(it, refreshOnly = true) }
                    c.optJSONObject("Pair")?.let(::pair)
                    c.optJSONObject("SendLogs")?.let(::sendLogs)
                    c.optJSONObject("HostAction")?.let(::hostAction)
                    c.optJSONObject("SaveHost")?.let(::saveHost)
                    c.optJSONObject("UpdateHost")?.let(::updateHost)
                    c.optJSONObject("ForgetHost")?.let(::forgetHost)
                    c.optJSONObject("Wake")?.let(::wake)
                    c.optJSONObject("SetPin")?.let(::setPin)
                    c.optJSONObject("BindProfile")?.let(::bindProfile)
                    c.optJSONObject("SetClipboard")?.let(::setClipboard)
                    c.optJSONObject("OpenPlatformScreen")?.let { onPlatformScreen?.invoke(it.optString("id")) }
                    c.optJSONObject("PadAction")?.let { onPadAction?.invoke(it.optString("action"), it.optString("pad_key")) }
                    c.optString("OpenPlatformScreen").takeIf { c.has("OpenPlatformScreen") && c.opt("OpenPlatformScreen") is String }
                        ?.let { onPlatformScreen?.invoke(it) }
                }
            }
        }
    }

    private fun hostForKey(key: String): KnownHost? {
        val primary = key.substringBefore('\u0000')
        return knownHostStore.all().firstOrNull { ConsoleJson.rowKey(it.fpHex, it.address, it.port) == primary }
    }

    private fun saveHost(c: JSONObject) {
        val addr = c.optString("addr"); val port = c.optInt("port"); val name = c.optString("name")
        val existing = knownHostStore.get(addr, port)
        if (existing != null) {
            if (name.isNotEmpty()) knownHostStore.save(existing.copy(name = name))
        } else {
            knownHostStore.save(KnownHost(address = addr, port = port, name = name.ifEmpty { addr }, fpHex = "", paired = false))
        }
        pushHosts(); pushKnownHosts()
    }

    private fun updateHost(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val name = c.optString("name").trim(); val addr = c.optString("addr"); val port = c.optInt("port")
        if (addr != kh.address || port != kh.port) knownHostStore.remove(kh)
        knownHostStore.save(kh.copy(name = name.ifEmpty { addr }, address = addr, port = port))
        pushHosts(); pushKnownHosts()
    }

    private fun forgetHost(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        knownHostStore.remove(kh)
        appContext?.let { LibraryCache.standard(it.cacheDir).forget(kh.id) }
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::BindProfile` — the host's default binding (`KnownHost.profileId`); null clears. */
    private fun bindProfile(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("profile_id")
            .takeIf { c.has("profile_id") && !c.isNull("profile_id") && it.isNotEmpty() }
        knownHostStore.save(kh.copy(profileId = pid))
        pushHosts(); pushKnownHosts()
    }

    /** `ConsoleCmd::SetClipboard` — the per-host clipboard trust toggle. */
    private fun setClipboard(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        knownHostStore.save(kh.copy(clipboardSync = c.optBoolean("on")))
        pushHosts(); pushKnownHosts()
    }

    private fun setPin(c: JSONObject) {
        val kh = hostForKey(c.optString("key")) ?: return
        val pid = c.optString("profile_id"); val pin = c.optBoolean("pin")
        val pins = kh.pinnedProfileIds.toMutableList()
        if (pin && pid !in pins) pins.add(pid) else if (!pin) pins.remove(pid)
        knownHostStore.save(kh.copy(pinnedProfileIds = pins))
        pushHosts(); pushKnownHosts()
    }

    /**
     * `ConsoleCmd::SendLogs` — [io.unom.punktfunk.SendLogs], the same upload the touch home's
     * card menu runs; the result comes back here as a notice.
     */
    private fun sendLogs(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val hostName = c.optString("host_name").ifEmpty { addr }
        val id = identity
        if (id == null) {
            notice("Identity not ready yet — try again in a moment")
            return
        }
        val app = appContext ?: return
        ioPool.execute {
            val message = io.unom.punktfunk.SendLogs.toHost(app, id, addr, mgmt, fp, hostName)
            main.post { notice(message) }
        }
    }

    /**
     * Sleep / restart / shut the host down (`design/host-actions.md` §7) — the console already
     * confirmed a destructive one twice before raising this, and the host re-checks this
     * device's Host-power grant on arrival, so nothing is decided here.
     */
    private fun hostAction(c: JSONObject) {
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val hostName = c.optString("host_name").ifEmpty { addr }
        val actionId = c.optString("action_id"); val label = c.optString("label")
        val id = identity
        if (id == null) {
            notice("Identity not ready yet — try again in a moment")
            return
        }
        ioPool.execute {
            val message = HostActions.invoke(id, addr, mgmt, fp, hostName, actionId, label)
            main.post { notice(message) }
        }
    }

    private fun pair(c: JSONObject) {
        val addr = c.optString("addr"); val port = c.optInt("port")
        val pin = c.optString("pin"); val name = c.optString("device_name")
        val id = identity
        if (id == null) {
            NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed("Identity not ready yet — try again in a moment"))
            return
        }
        val hostName = knownHostStore.get(addr, port)?.name
            ?: discovered.firstOrNull { it.host == addr && it.port == port }?.name ?: addr
        NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairBusy())
        ioPool.execute {
            val fp = runCatching { NativeBridge.nativePair(addr, port, id.certPem, id.privateKeyPem, pin, name) }.getOrDefault("")
            main.post {
                if (fp.isNotEmpty()) {
                    knownHostStore.trust(addr, port, hostName, fp, paired = true)
                    pushHosts(); pushKnownHosts()
                    NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairPaired(fp))
                } else {
                    NativeBridge.nativeConsoleSetPair(handle, ConsoleJson.pairFailed(ConnectErrors.pairMessage(NativeBridge.nativeTakeLastError())))
                }
            }
        }
    }

    /**
     * The wake-and-wait loop (the desktop's `spawn_wake`): resend the magic packet every 6 s,
     * probe once a second, 90 s timeout; the console reads `online`/`timed_out` off the status
     * and acts (a `then_connect` wake dials from the shell's side once online).
     */
    private fun wake(c: JSONObject) {
        val key = c.optString("key"); val thenConnect = c.optBoolean("then_connect")
        val kh = hostForKey(key) ?: return
        if (kh.mac.isEmpty()) return
        val gen = wakeGen.incrementAndGet()
        val name = kh.name.ifBlank { kh.address }
        ioPool.execute {
            val started = System.currentTimeMillis()
            var lastPacket = 0L
            while (wakeGen.get() == gen && handle != 0L) {
                val elapsed = ((System.currentTimeMillis() - started) / 1000).toInt()
                val timedOut = elapsed >= 90
                if (!timedOut && System.currentTimeMillis() - lastPacket >= 6_000) {
                    NativeBridge.nativeWakeOnLan(kh.mac.joinToString(","), kh.address)
                    lastPacket = System.currentTimeMillis()
                }
                val online = NativeBridge.nativeProbe(kh.address, kh.port, 900) ||
                    discovered.any { kh.matches(it) }
                if (wakeGen.get() != gen) return@execute
                NativeBridge.nativeConsoleSetWake(
                    handle,
                    ConsoleJson.wakeStatus(key, name, elapsed, timedOut, online, thenConnect),
                )
                if (online || timedOut) return@execute
                Thread.sleep(1000)
            }
        }
    }

    /**
     * The library pipeline (the desktop's `spawn_fetch`): cached shelf first, wake + retry
     * across the boot window when the host has a MAC, then the catalog, the running set and
     * the posters — each poster fetched over the same mTLS client and pushed as bytes.
     */
    private fun fetchLibrary(c: JSONObject, refreshOnly: Boolean) {
        val app = appContext ?: return
        val addr = c.optString("addr"); val mgmt = c.optInt("mgmt"); val fp = c.optString("fp_hex")
        val id = identity
        val kh = knownHostStore.all().firstOrNull { it.fpHex.equals(fp, true) && fp.isNotEmpty() }
            ?: knownHostStore.get(addr, mgmt)
        if (refreshOnly) {
            if (id == null) return
            ioPool.execute {
                val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                    .filter { it.isUp }.mapNotNull { it.appId }
                main.post { if (handle != 0L) NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.stringArray(up)) }
            }
            return
        }
        val gen = fetchGen.incrementAndGet()
        NativeBridge.nativeConsoleLibraryBegin(handle)
        if (id == null) {
            NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", "Identity not ready yet — try again in a moment", true))
            return
        }
        val cache = LibraryCache.standard(app.cacheDir)
        val cacheKey = kh?.id ?: fp.ifEmpty { "$addr:$mgmt" }
        ioPool.execute {
            val cached = cache.load(cacheKey)?.games?.takeIf { it.isNotEmpty() }
            if (cached != null) main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(cached), true) }
            val macs = kh?.mac.orEmpty()
            val waking = macs.isNotEmpty() && settings.autoWakeEnabled
            if (waking) NativeBridge.nativeWakeOnLan(macs.joinToString(","), addr)
            val attempts = if (waking) 12 else 1
            var result: LibraryResult? = null
            for (attempt in 0 until attempts) {
                if (gen != fetchGen.get()) return@execute
                val r = LibraryClient.fetch(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                result = r
                if (r is LibraryResult.Ok || r is LibraryResult.Unauthorized) break
                if (attempt + 1 >= attempts) break
                if (attempt % 2 == 1) NativeBridge.nativeWakeOnLan(macs.joinToString(","), addr)
                main.post { if (gen == fetchGen.get()) NativeBridge.nativeConsoleLibraryStale(handle, 1) }
                Thread.sleep(5_000)
            }
            if (gen != fetchGen.get()) return@execute
            when (val r = result) {
                is LibraryResult.Ok -> {
                    val games = r.games
                    cache.store(cacheKey, games)
                    val up = LibraryClient.fetchRunning(addr, mgmt, id.certPem, id.privateKeyPem, fp)
                        .filter { it.isUp }.mapNotNull { it.appId }
                    main.post {
                        if (gen != fetchGen.get()) return@post
                        NativeBridge.nativeConsoleLibraryGames(handle, ConsoleJson.libraryGames(games), false)
                        NativeBridge.nativeConsoleLibraryStale(handle, 0)
                        NativeBridge.nativeConsoleLibraryRunning(handle, ConsoleJson.stringArray(up))
                    }
                    for (g in games) {
                        val candidates = g.art.posterCandidates
                        if (candidates.isEmpty()) continue
                        artPool.execute {
                            if (gen != fetchGen.get()) return@execute
                            val bytes = fetchArt(candidates, id, addr, fp) ?: return@execute
                            main.post { if (gen == fetchGen.get() && handle != 0L) NativeBridge.nativeConsoleLibraryArt(handle, g.id, bytes) }
                        }
                    }
                }
                is LibraryResult.Unauthorized -> main.post {
                    if (gen != fetchGen.get()) return@post
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Not paired", r.message, false))
                }
                is LibraryResult.Error -> main.post {
                    if (gen != fetchGen.get()) return@post
                    if (cached != null) NativeBridge.nativeConsoleLibraryStale(handle, 2)
                    else NativeBridge.nativeConsoleLibraryPhase(handle, ConsoleJson.libraryError("Couldn't load the library", r.message, true))
                }
                null -> {}
            }
        }
    }

    /** One poster: the candidates in order, first success wins; the host's art proxy over mTLS. */
    private fun fetchArt(candidates: List<String>, id: ClientIdentity, addr: String, fp: String): ByteArray? {
        for (url in candidates) {
            val client = if (url.contains(addr)) {
                runCatching { io.unom.punktfunk.kit.library.mtlsHttpClient(id.certPem, id.privateKeyPem, addr, fp) }.getOrNull() ?: continue
            } else artHttp
            val bytes = runCatching {
                client.newCall(Request.Builder().url(url).build()).execute().use { resp ->
                    if (resp.code == 200) resp.body?.bytes()?.takeIf { it.isNotEmpty() && it.size <= 16 shl 20 } else null
                }
            }.getOrNull()
            if (bytes != null) return bytes
        }
        return null
    }

    /** The no-PIN request-access park (≥ the host's approval window) — ConnectScreen's figure. */
    private const val REQUEST_ACCESS_TIMEOUT_MS = 185_000

    /** How long a host's advertised actions stay fresh before we ask again — the desktop's
     *  `pf_client_core::host_actions::TTL`. Long on purpose: what it governs changes when an
     *  operator edits access, not minute to minute, and each refresh is a TLS handshake. */
    private const val HOST_ACTIONS_TTL_MS = 300_000L
}
