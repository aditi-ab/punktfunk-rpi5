package io.unom.punktfunk.console

import android.view.InputDevice
import io.unom.punktfunk.HostActions
import io.unom.punktfunk.MouseMode
import io.unom.punktfunk.Settings
import io.unom.punktfunk.StatsVerbosity
import io.unom.punktfunk.StreamProfile
import io.unom.punktfunk.TouchMode
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.library.DEFAULT_MGMT_PORT
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.padInfoOf
import org.json.JSONArray
import org.json.JSONObject

/**
 * The JSON that crosses into the Skia console — written in the console's OWN model shapes
 * (`crates/pf-console-ui/src/model.rs` `HostRow`/`WakeStatus`/`PairPhase`, `library.rs`
 * `LibraryGame`/`LibraryPhase`, `pf-client-core/src/trust.rs` `Settings`/`KnownHosts`), so there
 * is no Android-side mirror type to drift; the Rust structs deserialize these directly.
 */
internal object ConsoleJson {
    // ---- host rows (`HostRow`) ------------------------------------------------------------

    /** `HostRow.key` — the pinned fingerprint when there is one, else `addr:port` (Rust parity). */
    fun rowKey(fpHex: String, address: String, port: Int): String =
        if (fpHex.isEmpty()) "$address:$port" else fpHex

    private fun profileChip(p: StreamProfile): JSONObject = JSONObject()
        .put("id", p.id)
        .put("name", p.name)
        .put("accent", p.accent ?: JSONObject.NULL)

    /** A host's advertised actions in the console model's shape (`HostRow.actions`). */
    private fun actionRows(actions: List<HostActions.Action>?): JSONArray {
        val arr = JSONArray()
        for (a in actions.orEmpty()) {
            arr.put(
                JSONObject()
                    .put("id", a.id)
                    .put("label", a.label)
                    .put("danger", a.danger)
                    .put("available", a.available)
                    .put("unavailable_reason", a.unavailableReason),
            )
        }
        return arr
    }

    /**
     * The home carousel: saved hosts (name order — Android records carry no last-used time),
     * each followed by its pinned profile cards, then discovered-but-unsaved hosts. Mirrors
     * `clients/session/src/console.rs::rows()` — the desktop service's ordering — so a
     * player who moves between a Deck and a phone finds the same carousel.
     */
    fun hostRows(
        saved: List<KnownHost>,
        discovered: List<DiscoveredHost>,
        reachable: Set<String>,
        profiles: List<StreamProfile>,
        /** What each paired host last said this device may do TO it, by fingerprint
         *  (`design/host-actions.md` §7). Absent = no rows, which is also what an older host
         *  and an ungranted device produce. */
        hostActions: Map<String, List<HostActions.Action>> = emptyMap(),
    ): String {
        val out = JSONArray()
        fun advertFor(h: KnownHost): DiscoveredHost? = discovered.firstOrNull { d ->
            (h.fpHex.isNotEmpty() && d.fingerprint.equals(h.fpHex, ignoreCase = true)) ||
                (d.host == h.address && d.port == h.port)
        }
        for (h in saved.sortedBy { it.name.lowercase() }) {
            val key = rowKey(h.fpHex, h.address, h.port)
            val advert = advertFor(h)
            val online = advert != null || "${h.address}:${h.port}" in reachable
            val base = JSONObject()
                .put("key", key)
                .put("name", h.name.ifBlank { h.address })
                .put("addr", h.address)
                .put("port", h.port)
                .put("fp_hex", h.fpHex)
                .put("paired", h.paired)
                .put("saved", true)
                .put("online", online)
                .put("mgmt_port", advert?.mgmtPort ?: h.mgmtPort ?: DEFAULT_MGMT_PORT)
                .put("can_wake", !online && h.mac.isNotEmpty())
                .put("clipboard_sync", h.clipboardSync)
                .put("last_used", JSONObject.NULL)
                .put("os", advert?.os?.takeIf { it.isNotEmpty() } ?: h.os)
                .put("actions", actionRows(hostActions[h.fpHex]))
                .put("pin", JSONObject.NULL)
                .put(
                    "bound_profile",
                    h.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
                        ?.let(::profileChip) ?: JSONObject.NULL,
                )
            out.put(base)
            // A pinned card shares the primary tile's live state; its key rides the profile id
            // behind a NUL (impossible in a fingerprint or `addr:port`) — Rust parity.
            for (pid in h.pinnedProfileIds) {
                val p = profiles.firstOrNull { it.id == pid } ?: continue
                out.put(
                    JSONObject(base.toString())
                        .put("key", "$key\u0000${p.id}")
                        .put("pin", profileChip(p))
                        .put("bound_profile", JSONObject.NULL),
                )
            }
        }
        val extra = discovered.filter { d ->
            saved.none { h ->
                (h.fpHex.isNotEmpty() && h.fpHex.equals(d.fingerprint, ignoreCase = true)) ||
                    (h.address == d.host && h.port == d.port)
            }
        }.sortedBy { it.name.lowercase() }
        for (d in extra) {
            val fp = d.fingerprint.orEmpty()
            out.put(
                JSONObject()
                    .put("key", rowKey(fp, d.host, d.port))
                    .put("name", d.name.ifBlank { d.host })
                    .put("addr", d.host)
                    .put("port", d.port)
                    .put("fp_hex", fp)
                    .put("paired", false)
                    .put("saved", false)
                    .put("online", true)
                    .put("mgmt_port", d.mgmtPort ?: DEFAULT_MGMT_PORT)
                    .put("can_wake", false)
                    .put("clipboard_sync", false)
                    .put("last_used", JSONObject.NULL)
                    .put("os", d.os)
                    .put("pin", JSONObject.NULL)
                    .put("bound_profile", JSONObject.NULL),
            )
        }
        return out.toString()
    }

    /** One `HostRow` for a console entry (`{"library": <HostRow>}`) — the shelf to open. */
    fun hostRow(h: KnownHost, pin: StreamProfile?, profiles: List<StreamProfile>): JSONObject {
        val key = rowKey(h.fpHex, h.address, h.port)
        return JSONObject()
            .put("key", if (pin == null) key else "$key\u0000${pin.id}")
            .put("name", h.name.ifBlank { h.address })
            .put("addr", h.address)
            .put("port", h.port)
            .put("fp_hex", h.fpHex)
            .put("paired", h.paired)
            .put("saved", true)
            .put("online", true)
            .put("mgmt_port", h.mgmtPort ?: DEFAULT_MGMT_PORT)
            .put("can_wake", false)
            .put("clipboard_sync", h.clipboardSync)
            .put("last_used", JSONObject.NULL)
            .put("os", h.os)
            .put("pin", pin?.let(::profileChip) ?: JSONObject.NULL)
            .put(
                "bound_profile",
                if (pin != null) JSONObject.NULL
                else h.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
                    ?.let(::profileChip) ?: JSONObject.NULL,
            )
    }

    /** `KnownHosts` (Rust) — only what the console needs to build a link: id, address, fp. */
    fun knownHosts(saved: List<KnownHost>): String {
        val hosts = JSONArray()
        for (h in saved) {
            hosts.put(
                JSONObject()
                    .put("name", h.name)
                    .put("addr", h.address)
                    .put("port", h.port)
                    .put("fp_hex", h.fpHex)
                    .put("paired", h.paired)
                    .put("id", h.id)
                    .put("mac", JSONArray(h.mac))
                    .put("os", h.os)
                    .put("mgmt_port", h.mgmtPort ?: JSONObject.NULL)
                    .put("profile_id", h.profileId ?: JSONObject.NULL)
                    .put("pinned_profiles", JSONArray(h.pinnedProfileIds)),
            )
        }
        return JSONObject().put("hosts", hosts).toString()
    }

    fun profiles(profiles: List<StreamProfile>): String {
        val out = JSONArray()
        for (p in profiles) out.put(JSONArray().put(p.id).put(p.name))
        return out.toString()
    }

    // ---- wake / pair ------------------------------------------------------------------------

    fun wakeStatus(
        key: String,
        name: String,
        seconds: Int,
        timedOut: Boolean,
        online: Boolean,
        thenConnect: Boolean,
    ): String = JSONObject()
        .put("key", key)
        .put("name", name)
        .put("seconds", seconds)
        .put("timed_out", timedOut)
        .put("online", online)
        .put("then_connect", thenConnect)
        .toString()

    fun pairIdle(): String = "\"Idle\""
    fun pairBusy(): String = "\"Busy\""
    fun pairFailed(msg: String): String = JSONObject().put("Failed", msg).toString()
    fun pairPaired(key: String): String =
        JSONObject().put("Paired", JSONObject().put("key", key)).toString()

    // ---- library ------------------------------------------------------------------------------

    /** `[LibraryGame]` from the Kotlin catalog — the desktop service's `to_model` mapping. */
    fun libraryGames(games: List<GameEntry>): String {
        val out = JSONArray()
        for (g in games) {
            out.put(
                JSONObject()
                    .put("id", g.id)
                    .put("title", g.title)
                    .put("store", g.store)
                    .put("launcher", g.isLauncher)
                    .put("icon", g.icon?.takeIf(::validIconToken) ?: "")
                    .put("platform", g.platform ?: JSONObject.NULL)
                    .put("running", false),
            )
        }
        return out.toString()
    }

    /** `GameEntry::icon_token`'s re-validation: lowercase-first, ≤ 32 chars of [a-z0-9-]. */
    private fun validIconToken(t: String): Boolean =
        t.isNotEmpty() && t.length <= 32 && t[0] in 'a'..'z' &&
            t.all { it in 'a'..'z' || it in '0'..'9' || it == '-' }

    fun libraryError(title: String, body: String, canRetry: Boolean): String = JSONObject()
        .put(
            "Error",
            JSONObject().put("title", title).put("body", body).put("can_retry", canRetry),
        )
        .toString()

    fun stringArray(items: Collection<String>): String = JSONArray(items).toString()

    // ---- pads -------------------------------------------------------------------------------

    /**
     * `{"label", "pref", "pads": [...]}` — the controller chip's text (the driving pad's name),
     * the glyph style's pref byte, and one entry per connected pad for the settings rows and the
     * console's Connected-controllers screen.
     *
     * `detail`/`forwarded`/`rumble` come straight from [padInfoOf], the same reader the touch
     * Controllers screen renders from: the support answer a user gets must not depend on which
     * interface asked, and two readers of `InputDevice` would be two answers waiting to drift.
     */
    fun pads(pads: List<InputDevice>, driving: InputDevice?): String {
        val arr = JSONArray()
        for (d in pads) {
            val info = padInfoOf(d)
            val entry = JSONObject()
                .put("name", d.name)
                .put("key", "${d.vendorId}:${d.productId}:${d.name}")
                .put("pref", Gamepad.prefFor(d))
                .put("steam_virtual", false)
                .put("detail", info.detail)
                .put("forwarded", info.forwarded)
                .put("rumble", info.canRumble)
            val battery = if (android.os.Build.VERSION.SDK_INT >= 31) {
                val b = d.batteryState
                if (b.isPresent && b.capacity >= 0f) {
                    JSONObject()
                        .put("percent", (b.capacity * 100f).toInt().coerceIn(0, 100))
                        .put(
                            "charging",
                            b.status == android.os.BatteryManager.BATTERY_STATUS_CHARGING ||
                                b.status == android.os.BatteryManager.BATTERY_STATUS_FULL,
                        )
                } else null
            } else null
            entry.put("battery", battery ?: JSONObject.NULL)
            arr.put(entry)
        }
        return JSONObject()
            .put("label", driving?.name ?: JSONObject.NULL)
            .put("pref", driving?.let { Gamepad.prefFor(it) } ?: JSONObject.NULL)
            .put("pads", arr)
            .toString()
    }

    // ---- settings (`trust::Settings`) -------------------------------------------------------

    private val GAMEPAD_NAMES = listOf(
        "auto", "xbox360", "dualsense", "xboxone", "dualshock4", "steamcontroller", "steamdeck",
        "dualsenseedge", "switchpro", "steamcontroller2", "steamcontroller2puck", "xboxelite",
    )
    private val COMPOSITOR_NAMES = listOf("auto", "kwin", "wlroots", "mutter", "gamescope")

    /**
     * The console's settings document: [base] is the last snapshot the console saved (it owns
     * keys Android has no field for — `library_sort`, `library_view`, `reduce_motion`, …), and
     * every field Android DOES own is written over it from [s], so the touch UI's edits win.
     * `trust::Settings` is `#[serde(default)]`, so a partial document is fine.
     */
    fun settings(s: Settings, base: JSONObject?): JSONObject {
        val j = base?.let { JSONObject(it.toString()) } ?: JSONObject()
        j.put("width", s.width)
        j.put("height", s.height)
        j.put("refresh_hz", s.hz)
        j.put("bitrate_kbps", s.bitrateKbps)
        j.put("render_scale", s.renderScale)
        j.put("gamepad", GAMEPAD_NAMES.getOrElse(s.gamepad) { "auto" })
        j.put("gamepad_forwarding", s.gamepadForwarding)
        j.put("system_buttons", s.systemButtons)
        j.put("guide_gesture", s.guideGesture)
        j.put("compositor", COMPOSITOR_NAMES.getOrElse(s.compositor) { "auto" })
        j.put("touch_mode", s.touchMode.name.lowercase())
        j.put("mouse_mode", s.mouseMode.storedName)
        j.put("mic_enabled", s.micEnabled)
        j.put("echo_cancel", s.echoCancel)
        j.put("keep_host_audio", s.keepHostAudio)
        j.put("audio_channels", s.audioChannels)
        j.put("audio_format", s.audioFormat)
        j.put("codec", s.codec)
        j.put("hdr_enabled", s.hdrEnabled)
        j.put("present_priority", s.presentPriority)
        j.put("smooth_buffer", s.smoothBuffer)
        j.put("show_stats", s.statsVerbosity != StatsVerbosity.OFF)
        j.put("stats_verbosity", s.statsVerbosity.name.lowercase())
        j.put("ui_palette", s.uiPalette)
        j.put("auto_wake", s.autoWakeEnabled)
        j.put("invert_scroll", s.invertScroll)
        j.put("overlay_actions", s.overlayActions)
        j.put("pad_haptics", s.padHaptics)
        j.put("pad_speaker", if (s.padSpeaker) "pad" else "off")
        // Android-only rows ride `Settings::extra`, which is `#[serde(flatten)]` — so they are
        // TOP-LEVEL keys of this document, not a nested `extra` object. Nesting them put the
        // whole object into the map under the literal key "extra", where no console row could
        // read it and every value the console wrote came straight back as the one we had sent.
        j.put("android.low_latency", s.lowLatencyMode)
        j.put("android.rumble_on_phone", s.rumbleOnPhone)
        j.put("android.gyro_on_phone", s.gyroOnPhone)
        j.put("android.sc2_capture", s.sc2Capture)
        j.put("android.ds_capture", s.dsCapture)
        j.put("android.gamepad_ui_mode", s.gamepadUiMode)
        j.put("android.gamepad_ui_enabled", s.gamepadUiEnabled)
        j.put("android.reduce_ui_resolution", s.reduceUiResolution)
        // A store written by the nesting build carries the stale wrapper; drop it rather than
        // round-trip a copy of these keys that nothing reads for the life of the install.
        j.remove("extra")
        return j
    }

    /**
     * The console saved [j]: fold every key Android owns back into [s]. Unknown values snap to
     * the field's current value — a newer console's spelling must never corrupt the store.
     */
    fun applySettings(s: Settings, j: JSONObject): Settings {
        fun str(k: String, cur: String) = j.optString(k, cur).ifEmpty { cur }
        // The `android.*` keys are TOP-LEVEL here, not nested: `Settings::extra` is
        // `#[serde(flatten)]`, so the console writes them beside `width` and `codec`.
        return s.copy(
            width = j.optInt("width", s.width),
            height = j.optInt("height", s.height),
            hz = j.optInt("refresh_hz", s.hz),
            bitrateKbps = j.optInt("bitrate_kbps", s.bitrateKbps),
            renderScale = j.optDouble("render_scale", s.renderScale),
            gamepad = GAMEPAD_NAMES.indexOf(str("gamepad", "")).takeIf { it >= 0 } ?: s.gamepad,
            gamepadForwarding = j.optBoolean("gamepad_forwarding", s.gamepadForwarding),
            systemButtons = str("system_buttons", s.systemButtons),
            guideGesture = str("guide_gesture", s.guideGesture),
            compositor = COMPOSITOR_NAMES.indexOf(str("compositor", "")).takeIf { it >= 0 }
                ?: s.compositor,
            touchMode = TouchMode.entries.firstOrNull { it.name.lowercase() == j.optString("touch_mode") }
                ?: s.touchMode,
            mouseMode = MouseMode.entries.firstOrNull { it.storedName == j.optString("mouse_mode") }
                ?: s.mouseMode,
            micEnabled = j.optBoolean("mic_enabled", s.micEnabled),
            echoCancel = j.optBoolean("echo_cancel", s.echoCancel),
            keepHostAudio = j.optBoolean("keep_host_audio", s.keepHostAudio),
            audioChannels = j.optInt("audio_channels", s.audioChannels),
            audioFormat = str("audio_format", s.audioFormat),
            codec = str("codec", s.codec),
            hdrEnabled = j.optBoolean("hdr_enabled", s.hdrEnabled),
            presentPriority = str("present_priority", s.presentPriority),
            smoothBuffer = j.optInt("smooth_buffer", s.smoothBuffer),
            statsVerbosity = StatsVerbosity.entries
                .firstOrNull { it.name.lowercase() == j.optString("stats_verbosity") }
                ?: s.statsVerbosity,
            uiPalette = str("ui_palette", s.uiPalette),
            autoWakeEnabled = j.optBoolean("auto_wake", s.autoWakeEnabled),
            invertScroll = j.optBoolean("invert_scroll", s.invertScroll),
            overlayActions = str("overlay_actions", s.overlayActions),
            padHaptics = j.optBoolean("pad_haptics", s.padHaptics),
            padSpeaker = when (j.optString("pad_speaker", "")) {
                "pad", "mix" -> true
                "off" -> false
                else -> s.padSpeaker
            },
            lowLatencyMode = j.optBoolean("android.low_latency", s.lowLatencyMode),
            rumbleOnPhone = j.optBoolean("android.rumble_on_phone", s.rumbleOnPhone),
            gyroOnPhone = j.optBoolean("android.gyro_on_phone", s.gyroOnPhone),
            sc2Capture = j.optBoolean("android.sc2_capture", s.sc2Capture),
            dsCapture = j.optBoolean("android.ds_capture", s.dsCapture),
            gamepadUiMode = j.optString("android.gamepad_ui_mode", s.gamepadUiMode)
                .ifEmpty { s.gamepadUiMode },
            gamepadUiEnabled = j.optBoolean("android.gamepad_ui_enabled", s.gamepadUiEnabled),
            reduceUiResolution = j.optBoolean("android.reduce_ui_resolution", s.reduceUiResolution),
        )
    }
}
