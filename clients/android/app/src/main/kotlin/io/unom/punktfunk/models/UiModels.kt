package io.unom.punktfunk.models

import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.Settings
import androidx.compose.ui.graphics.vector.ImageVector

/** Bottom-bar destinations (the immersive stream view is shown full-screen, outside the bar). */
enum class Tab(val label: String, val icon: ImageVector) {
    Connect("Connect", Icons.Filled.Home),
    Settings("Settings", Icons.Filled.Settings),
}

/**
 * A trust decision awaiting the user before a connect proceeds. [name] is the label to save the
 * host under. Trust-on-first-use ([Kind.TRUST_NEW]) is only ever offered when the host ADVERTISED
 * pair=optional; a pair=required host or a manually-typed/unknown-policy host is offered the
 * two ways in ([Kind.REQUEST_ACCESS]): a no-PIN "request access" connect the operator approves in
 * the host's console, or the SPAKE2 PIN ceremony ([Kind.PAIR]). A changed fingerprint forces
 * re-pairing by PIN ([Kind.FP_CHANGED]) — never a silent re-trust.
 */
data class PendingTrust(
    val host: String,
    val port: Int,
    val name: String,
    val advertisedFp: String?,
    val kind: Kind,
    /**
     * What the connect on the far side of this decision should carry — a `punktfunk://` link's
     * one-off profile and library id. A link to an unknown host goes through the confirmation
     * first, and the user's stated intent must survive that detour rather than being silently
     * dropped on the way to a plain desktop session.
     */
    val profile: String? = null,
    val launch: String? = null,
) {
    enum class Kind { TRUST_NEW, FP_CHANGED, PAIR, REQUEST_ACCESS }
}

/**
 * A stream session that just opened, and the state the stream screen needs about it.
 *
 * [settings] is the settings the connect ACTUALLY used, resolved once at connect time — not
 * "whatever the settings store says now". Every post-connect read (the stats tier, the touch and
 * mouse models, the low-latency pipeline, rumble, SC2 capture) takes it, so the stream can never
 * disagree with the connect that produced it. [clipboardSync] comes from the host record, because
 * clipboard sync is a decision about that host rather than about this device.
 */
data class ActiveSession(
    val handle: Long,
    val settings: io.unom.punktfunk.Settings,
    val clipboardSync: Boolean,
    /**
     * The settings profile this session resolved, if any — shown on the stats overlay's first line
     * so "which profile am I on?" is answerable from inside the stream, as on the other clients.
     */
    val profileName: String? = null,
    /**
     * The stable id of the host being streamed, when it is a saved one — so a `punktfunk://` link
     * that arrives mid-stream can tell "this same host" (a no-op; the intent already focused us)
     * from "a different host" (a notice; a URL may never preempt a live session).
     */
    val hostId: String? = null,
    /**
     * This session was started by launching a title from [hostId]'s library, rather than by
     * connecting to the host's desktop.
     *
     * Decides where the client goes when the session ENDS: a title launched out of a library
     * belongs back in that library when its game exits — one press from the next one — not on the
     * host-selection screen. Only meaningful together with a
     * [io.unom.punktfunk.kit.SessionEndReason.GAME_EXITED] ending.
     */
    val launchedFromLibrary: Boolean = false,
    /**
     * Which of [hostId]'s shelves that library launch came off: the pinned host+profile card's
     * profile id (design §5.2a), or null for the host's own tile. Carried purely so the return
     * trip above lands back on the SAME shelf — a player who launched from a pinned card is still
     * on that card when the game exits, and coming back to the host's default shelf would silently
     * change what the next title streams with.
     */
    val libraryProfileId: String? = null,
)

/**
 * The library shelf a finished game launch should return to: the saved host's id, and the pinned
 * profile card it was opened from (null = the host's own tile). One value rather than two parallel
 * ones, because a hostId that arrives without its profile is not "the same shelf" — it is the
 * default one wearing the same name.
 */
data class LibraryReturn(val hostId: String, val profileId: String? = null)

/** Trust state of a host, shown as a colored pill on its card. */
enum class HostStatus(val label: String) {
    PAIRED("Paired"),
    PAIRING("PIN pairing"),
    TOFU("Trust on first use"),
}
