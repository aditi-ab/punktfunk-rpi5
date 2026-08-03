package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.security.KnownHost
import java.security.SecureRandom
import org.json.JSONObject

/**
 * Client settings profiles — named bundles of setting overrides applied on top of the global
 * [Settings] (design/client-settings-profiles.md §4). The Kotlin mirror of
 * `crates/pf-client-core/src/profiles.rs`; the model is the same on every client, so get it right
 * here rather than re-deciding it.
 *
 * A profile overrides only the fields the user touched; everything else keeps following the global
 * defaults **live**, so fixing a global once fixes it everywhere. That is why an overlay is sparse
 * nullable fields rather than a snapshot copy, and why a value is written on touch and cleared only
 * on an explicit "reset to default" — never by diffing against the current global at save time. A
 * stored value that happens to equal today's global is a legitimate *pin*: the profile keeps it
 * when the global later moves.
 *
 * Only tier-P settings are here. Device facts (which pad this device forwards, whether its console
 * UI is on) and host facts (clipboard sync, which lives on the host record) are deliberately absent
 * — see the design's §3 curation.
 *
 * Values are stored exactly as [SettingsStore] persists them — ints for the compositor/gamepad wire
 * bytes, enum names for the rest — so there is one encoding of a setting on this platform rather
 * than two. The catalog is client-local (v1 has no profile sync or export), so nothing else reads
 * it.
 */
data class SettingsOverlay(
    val width: Int? = null,
    val height: Int? = null,
    val hz: Int? = null,
    val bitrateKbps: Int? = null,
    val renderScale: Double? = null,
    val codec: String? = null,
    val hdrEnabled: Boolean? = null,
    val compositor: Int? = null,
    val audioChannels: Int? = null,
    val micEnabled: Boolean? = null,
    val echoCancel: Boolean? = null,
    val touchMode: TouchMode? = null,
    val mouseMode: MouseMode? = null,
    val invertScroll: Boolean? = null,
    val gamepad: Int? = null,
    val gamepadForwarding: Boolean? = null,
    val statsVerbosity: StatsVerbosity? = null,
    /**
     * Android-only tier-P addition (design §3): the decode pipeline is a device fact everywhere
     * else, but here it is the one knob a marginal link wants turned off per host.
     */
    val lowLatencyMode: Boolean? = null,
    /** The timeline presenter's intent pair — cross-client keys, see [Settings.presentPriority]. */
    val presentPriority: String? = null,
    val smoothBuffer: Int? = null,
    /**
     * Overlay keys a newer build wrote and this one doesn't model — carried through a load→save
     * round-trip untouched. The don't-clobber rule: opening and saving a profile on an older client
     * must not erase what a newer one stored.
     */
    val extra: Map<String, Any> = emptyMap(),
) {
    /** The one resolution seam: this overlay on top of [base]. Pure, so it is fully testable. */
    fun apply(base: Settings): Settings = base.copy(
        width = width ?: base.width,
        height = height ?: base.height,
        hz = hz ?: base.hz,
        bitrateKbps = bitrateKbps ?: base.bitrateKbps,
        renderScale = renderScale ?: base.renderScale,
        codec = codec ?: base.codec,
        hdrEnabled = hdrEnabled ?: base.hdrEnabled,
        compositor = compositor ?: base.compositor,
        audioChannels = audioChannels ?: base.audioChannels,
        micEnabled = micEnabled ?: base.micEnabled,
        echoCancel = echoCancel ?: base.echoCancel,
        touchMode = touchMode ?: base.touchMode,
        mouseMode = mouseMode ?: base.mouseMode,
        invertScroll = invertScroll ?: base.invertScroll,
        gamepad = gamepad ?: base.gamepad,
        gamepadForwarding = gamepadForwarding ?: base.gamepadForwarding,
        statsVerbosity = statsVerbosity ?: base.statsVerbosity,
        lowLatencyMode = lowLatencyMode ?: base.lowLatencyMode,
        presentPriority = presentPriority ?: base.presentPriority,
        smoothBuffer = smoothBuffer ?: base.smoothBuffer,
    )

    /**
     * Record, as overrides, every tier-P field that differs between two settings snapshots.
     *
     * The settings UI commits a whole `Settings` per control (`update(s.copy(codec = …))`), so it
     * can't hand over a list of touched fields — it hands over "what the control was showing" and
     * "what it shows now", and the only field that can differ is the one the user just touched.
     *
     * This is NOT the diff-on-save the design rejects: the comparison is against the EFFECTIVE
     * settings the control was displaying, not against the globals, so setting a value back to
     * whatever the global happens to be still records an override — the pin. It only ever adds
     * overrides; removing one is [clear], a different, explicit operation.
     */
    fun absorb(before: Settings, after: Settings): SettingsOverlay = copy(
        width = if (after.width != before.width) after.width else width,
        height = if (after.height != before.height) after.height else height,
        hz = if (after.hz != before.hz) after.hz else hz,
        bitrateKbps = if (after.bitrateKbps != before.bitrateKbps) after.bitrateKbps else bitrateKbps,
        renderScale = if (after.renderScale != before.renderScale) after.renderScale else renderScale,
        codec = if (after.codec != before.codec) after.codec else codec,
        hdrEnabled = if (after.hdrEnabled != before.hdrEnabled) after.hdrEnabled else hdrEnabled,
        compositor = if (after.compositor != before.compositor) after.compositor else compositor,
        audioChannels = if (after.audioChannels != before.audioChannels) after.audioChannels else audioChannels,
        micEnabled = if (after.micEnabled != before.micEnabled) after.micEnabled else micEnabled,
        echoCancel = if (after.echoCancel != before.echoCancel) after.echoCancel else echoCancel,
        touchMode = if (after.touchMode != before.touchMode) after.touchMode else touchMode,
        mouseMode = if (after.mouseMode != before.mouseMode) after.mouseMode else mouseMode,
        invertScroll = if (after.invertScroll != before.invertScroll) after.invertScroll else invertScroll,
        gamepad = if (after.gamepad != before.gamepad) after.gamepad else gamepad,
        gamepadForwarding =
            if (after.gamepadForwarding != before.gamepadForwarding) after.gamepadForwarding
            else gamepadForwarding,
        statsVerbosity = if (after.statsVerbosity != before.statsVerbosity) after.statsVerbosity else statsVerbosity,
        lowLatencyMode = if (after.lowLatencyMode != before.lowLatencyMode) after.lowLatencyMode else lowLatencyMode,
        presentPriority = if (after.presentPriority != before.presentPriority) after.presentPriority else presentPriority,
        smoothBuffer = if (after.smoothBuffer != before.smoothBuffer) after.smoothBuffer else smoothBuffer,
    )

    /**
     * Drop one override by its field name, putting the row back to inheriting. [FIELD_RESOLUTION]
     * is the one alias, covering the width/height pair a single control drives. An unknown name is
     * a no-op.
     */
    fun clear(field: String): SettingsOverlay = when (field) {
        FIELD_RESOLUTION -> copy(width = null, height = null)
        "refresh_hz" -> copy(hz = null)
        "bitrate_kbps" -> copy(bitrateKbps = null)
        "render_scale" -> copy(renderScale = null)
        "codec" -> copy(codec = null)
        "hdr_enabled" -> copy(hdrEnabled = null)
        "compositor" -> copy(compositor = null)
        "audio_channels" -> copy(audioChannels = null)
        "mic_enabled" -> copy(micEnabled = null)
        "echo_cancel" -> copy(echoCancel = null)
        "touch_mode" -> copy(touchMode = null)
        "mouse_mode" -> copy(mouseMode = null)
        "invert_scroll" -> copy(invertScroll = null)
        "gamepad" -> copy(gamepad = null)
        "gamepad_forwarding" -> copy(gamepadForwarding = null)
        "stats_verbosity" -> copy(statsVerbosity = null)
        "low_latency_mode" -> copy(lowLatencyMode = null)
        "present_priority" -> copy(presentPriority = null)
        "smooth_buffer" -> copy(smoothBuffer = null)
        else -> this
    }

    /** The field names this overlay overrides — what the settings rows draw their markers from. */
    fun overridden(): Set<String> = buildSet {
        if (width != null || height != null) add(FIELD_RESOLUTION)
        if (hz != null) add("refresh_hz")
        if (bitrateKbps != null) add("bitrate_kbps")
        if (renderScale != null) add("render_scale")
        if (codec != null) add("codec")
        if (hdrEnabled != null) add("hdr_enabled")
        if (compositor != null) add("compositor")
        if (audioChannels != null) add("audio_channels")
        if (micEnabled != null) add("mic_enabled")
        if (echoCancel != null) add("echo_cancel")
        if (touchMode != null) add("touch_mode")
        if (mouseMode != null) add("mouse_mode")
        if (invertScroll != null) add("invert_scroll")
        if (gamepad != null) add("gamepad")
        if (gamepadForwarding != null) add("gamepad_forwarding")
        if (statsVerbosity != null) add("stats_verbosity")
        if (lowLatencyMode != null) add("low_latency_mode")
        if (presentPriority != null) add("present_priority")
        if (smoothBuffer != null) add("smooth_buffer")
    }

    /**
     * True when the profile overrides nothing — "inherits everything", the state a freshly created
     * profile starts in. A profile holding only a newer build's field is NOT empty.
     */
    fun isEmpty(): Boolean = overridden().isEmpty() && extra.isEmpty()

    internal fun toJson(): JSONObject {
        val j = JSONObject()
        // Unknown keys first, so a modelled field always wins over a stale carried-through one.
        extra.forEach { (k, v) -> j.put(k, v) }
        width?.let { j.put("width", it) }
        height?.let { j.put("height", it) }
        hz?.let { j.put("refresh_hz", it) }
        bitrateKbps?.let { j.put("bitrate_kbps", it) }
        renderScale?.let { j.put("render_scale", it) }
        codec?.let { j.put("codec", it) }
        hdrEnabled?.let { j.put("hdr_enabled", it) }
        compositor?.let { j.put("compositor", it) }
        audioChannels?.let { j.put("audio_channels", it) }
        micEnabled?.let { j.put("mic_enabled", it) }
        echoCancel?.let { j.put("echo_cancel", it) }
        touchMode?.let { j.put("touch_mode", it.name) }
        mouseMode?.let { j.put("mouse_mode", it.storedName) }
        invertScroll?.let { j.put("invert_scroll", it) }
        gamepad?.let { j.put("gamepad", it) }
        gamepadForwarding?.let { j.put("gamepad_forwarding", it) }
        statsVerbosity?.let { j.put("stats_verbosity", it.name) }
        lowLatencyMode?.let { j.put("low_latency_mode", it) }
        presentPriority?.let { j.put("present_priority", it) }
        smoothBuffer?.let { j.put("smooth_buffer", it) }
        return j
    }

    companion object {
        /** The width/height pair, which one control drives — the reset alias, as on every client. */
        const val FIELD_RESOLUTION = "resolution"

        /** Keys this build models; everything else in a stored overlay is carried through. */
        private val KNOWN = setOf(
            "width", "height", "refresh_hz", "bitrate_kbps", "render_scale", "codec",
            "hdr_enabled", "compositor", "audio_channels", "mic_enabled", "echo_cancel",
            "touch_mode", "mouse_mode", "invert_scroll", "gamepad", "gamepad_forwarding",
            "stats_verbosity",
            "low_latency_mode", "present_priority", "smooth_buffer",
        )

        internal fun fromJson(j: JSONObject): SettingsOverlay = SettingsOverlay(
            width = j.optIntOrNull("width"),
            height = j.optIntOrNull("height"),
            hz = j.optIntOrNull("refresh_hz"),
            bitrateKbps = j.optIntOrNull("bitrate_kbps"),
            renderScale = if (j.has("render_scale")) j.optDouble("render_scale") else null,
            codec = j.optStringOrNull("codec"),
            hdrEnabled = j.optBooleanOrNull("hdr_enabled"),
            compositor = j.optIntOrNull("compositor"),
            audioChannels = j.optIntOrNull("audio_channels"),
            micEnabled = j.optBooleanOrNull("mic_enabled"),
            echoCancel = j.optBooleanOrNull("echo_cancel"),
            touchMode = j.optStringOrNull("touch_mode")
                ?.let { n -> TouchMode.entries.firstOrNull { it.name == n } },
            mouseMode = j.optStringOrNull("mouse_mode")
                ?.let { n -> MouseMode.entries.firstOrNull { it.storedName == n } },
            invertScroll = j.optBooleanOrNull("invert_scroll"),
            gamepad = j.optIntOrNull("gamepad"),
            gamepadForwarding = j.optBooleanOrNull("gamepad_forwarding"),
            statsVerbosity = j.optStringOrNull("stats_verbosity")
                ?.let { n -> StatsVerbosity.entries.firstOrNull { it.name == n } },
            lowLatencyMode = j.optBooleanOrNull("low_latency_mode"),
            presentPriority = j.optStringOrNull("present_priority"),
            smoothBuffer = j.optIntOrNull("smooth_buffer"),
            extra = j.keys().asSequence().filter { it !in KNOWN }.associateWith { j.get(it) },
        )
    }
}

/**
 * One named bundle of overrides. [id] is stable across renames — host bindings, pinned cards and
 * `punktfunk://` links all point at it, never at the name.
 */
data class StreamProfile(
    val id: String,
    /** User-facing and editable; unique case-insensitively (menus are ambiguous otherwise). */
    val name: String,
    /** `#RRGGBB` chip colour. Reserved by the schema; pinned cards tint their subtitle with it. */
    val accent: String? = null,
    val overrides: SettingsOverlay = SettingsOverlay(),
    /** Profile keys a newer build wrote — preserved across a load→save round-trip. */
    val extra: Map<String, Any> = emptyMap(),
)

/** What a `profile=` / one-off reference resolved to. Ambiguity is reported, never guessed. */
enum class ProfileResolution { FOUND, NOT_FOUND, AMBIGUOUS }

/**
 * The profile catalog — client-wide, not per host: "Work" applied to three hosts is one profile,
 * and the per-host part is only the binding on the host record ([KnownHost.profileId]).
 *
 * Stored one JSON string per profile keyed by id in its own `punktfunk_profiles` prefs file — the
 * `KnownHostStore` pattern, and deliberately not inside the settings file, which is rewritten
 * wholesale by several writers.
 */
class ProfileStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_profiles", Context.MODE_PRIVATE)

    /** Every profile, name-sorted — the order the scope switcher and the menus show. */
    fun all(): List<StreamProfile> = prefs.all.values
        .mapNotNull { (it as? String)?.let(::parse) }
        .sortedBy { it.name.lowercase() }

    fun byId(id: String): StreamProfile? = prefs.getString(id, null)?.let(::parse)

    fun save(profile: StreamProfile) {
        prefs.edit().putString(profile.id, encode(profile)).apply()
    }

    fun delete(id: String) {
        prefs.edit().remove(id).apply()
    }

    /**
     * Resolve a reference the way every surface must: exact id first, then a unique
     * case-insensitive name. Two profiles sharing a name resolve to [ProfileResolution.AMBIGUOUS]
     * — a link or a flag naming two profiles must refuse, not pick whichever came first.
     */
    fun resolve(reference: String): Pair<StreamProfile?, ProfileResolution> {
        if (reference.isEmpty()) return null to ProfileResolution.NOT_FOUND
        byId(reference)?.let { return it to ProfileResolution.FOUND }
        val hits = all().filter { it.name.equals(reference, ignoreCase = true) }
        return when (hits.size) {
            1 -> hits[0] to ProfileResolution.FOUND
            0 -> null to ProfileResolution.NOT_FOUND
            else -> null to ProfileResolution.AMBIGUOUS
        }
    }

    /**
     * Is this name already used (case-insensitively) by a *different* profile? The create/rename
     * guard — [except] is the profile being renamed, so renaming "Work" to "work" is allowed.
     */
    fun nameTaken(name: String, except: String? = null): Boolean =
        all().any { it.name.equals(name, ignoreCase = true) && it.id != except }

    /**
     * The profile a connect to [host] should use: the one-off pick, else the host's binding, else
     * none. [oneOff] is a reference (id or unique name); the empty string means "force the global
     * defaults" — a real choice ("Connect with ▸ Default settings" on a bound host), not "unset",
     * which is why it must survive as a value all the way down here. A binding whose profile was
     * deleted resolves as none: never an error, never a blocked connect.
     */
    fun resolveFor(host: KnownHost?, oneOff: String?): StreamProfile? = when {
        oneOff != null -> resolve(oneOff).first
        else -> host?.profileId?.let(::byId)
    }

    /** [host]'s pinned profiles, in card order, with duplicates and deleted profiles dropped. */
    fun pinsFor(host: KnownHost): List<StreamProfile> =
        host.pinnedProfileIds.distinct().mapNotNull(::byId)

    private fun parse(s: String): StreamProfile? = runCatching {
        val j = JSONObject(s)
        StreamProfile(
            id = j.getString("id"),
            name = j.getString("name"),
            accent = j.optStringOrNull("accent"),
            overrides = SettingsOverlay.fromJson(j.optJSONObject("overrides") ?: JSONObject()),
            extra = j.keys().asSequence()
                .filter { it !in setOf("id", "name", "accent", "overrides") }
                .associateWith { j.get(it) },
        )
    }.getOrNull()

    private fun encode(p: StreamProfile): String {
        val j = JSONObject()
        p.extra.forEach { (k, v) -> j.put(k, v) }
        j.put("id", p.id)
        j.put("name", p.name)
        p.accent?.let { j.put("accent", it) }
        j.put("overrides", p.overrides.toJson())
        return j.toString()
    }
}

/**
 * Chip colours a profile can wear. Chosen to stay legible on a dark surface and to be
 * distinguishable from each other at the size they are actually used — a 6dp dot on a chip and a
 * tint on a pinned card — and held at one saturation and lightness so no single swatch shouts
 * over its neighbours. Deliberately NOT the presence green ([HostCard]'s online dot), which means
 * something else entirely.
 *
 * **Ordered by hue**, so the picker reads as one sweep of the colour wheel rather than a bag of
 * colours; the degrees are in the comments to keep it that way when one is swapped out. That order
 * is also the order [nextAccent] hands them out in, so a user creating profiles one after another
 * walks the spectrum instead of getting an arbitrary sequence.
 */
val PROFILE_ACCENTS = listOf(
    "#FF8A4C", // orange   21°
    "#FBBF24", // amber    45°
    "#A3E635", // lime     82°
    "#34D399", // green   160°
    "#22D3EE", // cyan    187°
    "#60A5FA", // blue    213°
    "#818CF8", // indigo  239°
    "#A78BFA", // violet  258°
    "#F472B6", // pink    330°
    "#FB7185", // rose    350°
)

/** The first accent no existing profile is using, so two profiles don't look alike by accident. */
fun nextAccent(existing: List<StreamProfile>): String {
    val taken = existing.mapNotNull { it.accent?.lowercase() }.toSet()
    return PROFILE_ACCENTS.firstOrNull { it.lowercase() !in taken } ?: PROFILE_ACCENTS.first()
}

/**
 * A new, empty profile: it inherits everything, which is the right creation default under
 * inherit-by-exception (Duplicate covers "start from that other profile"). The id is 12 lowercase
 * hex characters — the shape the Rust `new_profile_id` mints.
 *
 * [accent] is presentation, not a setting, so it does NOT inherit — a profile with no colour would
 * be indistinguishable from the defaults everywhere the accent is the whole signal (a bound card's
 * chip, a pinned card's tint). Callers creating a profile from the UI pass [nextAccent].
 */
fun newProfile(name: String, accent: String? = null): StreamProfile =
    StreamProfile(id = newProfileId(), name = name, accent = accent)

private val PROFILE_ID_RNG = SecureRandom()

fun newProfileId(): String {
    val b = ByteArray(6)
    PROFILE_ID_RNG.nextBytes(b)
    return b.joinToString("") { "%02x".format(it) }
}

/**
 * The settings a connect to [host] should use: the resolved profile's overrides on top of these
 * globals, resolved ONCE per connect (matching the latch-at-connect model the "applies from the
 * next session" footers promise). See [ProfileStore.resolveFor] for the precedence.
 */
fun Settings.effectiveFor(profile: StreamProfile?): Settings =
    profile?.overrides?.apply(this) ?: this

// ---- org.json null-vs-absent helpers (optInt and friends can't tell 0 from "not there") ---------

private fun JSONObject.optIntOrNull(key: String): Int? = if (has(key)) optInt(key) else null
private fun JSONObject.optBooleanOrNull(key: String): Boolean? =
    if (has(key)) optBoolean(key) else null

private fun JSONObject.optStringOrNull(key: String): String? =
    if (has(key)) optString(key).ifEmpty { null } else null
