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
 * pair=optional; a pair=required host or a manually-typed/unknown-policy host goes straight to PIN
 * pairing ([Kind.PAIR]), and a changed fingerprint forces re-pairing — never a silent re-trust.
 */
data class PendingTrust(
    val host: String,
    val port: Int,
    val name: String,
    val advertisedFp: String?,
    val kind: Kind,
) {
    enum class Kind { TRUST_NEW, FP_CHANGED, PAIR }
}

/** Trust state of a host, shown as a colored pill on its card. */
enum class HostStatus(val label: String) {
    PAIRED("Paired"),
    PAIRING("PIN pairing"),
    TOFU("Trust on first use"),
}
