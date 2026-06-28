package io.unom.punktfunk.screenshots

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.BrandDark
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsScreen
import io.unom.punktfunk.StatsOverlay
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.models.HostStatus

// The CI screenshot scenes: the REAL app composables, fed embedded mock state, under the forced
// brand palette (Material You has no wallpaper to seed from on the JVM). The stream-video surface
// and ConnectScreen/App are intentionally absent — they require the live JNI core / a session.

/** Forces the deterministic punktfunk brand scheme (see Theme.kt) instead of dynamic colour. */
@Composable
internal fun ShotTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = BrandDark, content = content)
}

private data class MockHost(val name: String, val address: String, val status: HostStatus)

private val SAVED = listOf(
    MockHost("Living Room PC", "192.168.1.42:9777", HostStatus.PAIRED),
    MockHost("Office", "192.168.1.50:9777", HostStatus.TOFU),
)
private val DISCOVERED = listOf(
    MockHost("studio-deck", "192.168.1.61:9777", HostStatus.PAIRING),
    MockHost("HTPC", "192.168.1.70:9777", HostStatus.TOFU),
)

/** The connect screen's host grid, reconstructed from the real HostCard/SectionLabel components. */
@Composable
internal fun HostsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Spacer(Modifier.height(8.dp))
                    Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "stream a remote desktop",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(24.dp))
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) { SectionLabel("Saved hosts") }
            items(SAVED) { h ->
                HostCard(h.name, h.address, h.status, enabled = true, onConnect = {}, onForget = {}, onRename = {})
            }
            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(12.dp))
                SectionLabel("Discovered on the network")
            }
            items(DISCOVERED) { h ->
                HostCard(h.name, h.address, h.status, enabled = true, onConnect = {}, onForget = null)
            }
        }
    }
}

/** The real SettingsScreen, fed a representative non-default Settings. */
@Composable
internal fun SettingsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(
            initial = Settings(
                width = 1920,
                height = 1080,
                hz = 120,
                bitrateKbps = 50_000,
                compositor = 1,
                gamepad = 2,
                micEnabled = true,
                statsHudEnabled = true,
                trackpadMode = true,
            ),
            onChange = {},
            onBack = {},
        )
    }
}

/** The real TOFU AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.TRUST_NEW), shown over the host grid. */
@Composable
internal fun TrustDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Trust this host?") },
        text = {
            Column {
                Text("First connection to 192.168.1.61:9777.")
                Text("Fingerprint 9f8e7d6c5b4a3928…")
                Text(
                    "This host allows trust-on-first-use, but that can't tell an impostor " +
                        "from the real host. Pairing with a PIN is stronger — it proves both sides.",
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Trust (TOFU)") } },
        dismissButton = { TextButton({}) { Text("Pair with PIN…") } },
    )
}

/** The PIN-pairing AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.PAIR). The live screen
 *  uses OutlinedTextFields, but a TextField inside a Dialog window never reaches idle under
 *  Robolectric (its focus/cursor machinery animates forever) — so the PIN is shown as a static
 *  display here, which also reads better in a marketing shot. */
@Composable
internal fun PairDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Pair with PIN") },
        text = {
            Column {
                Text("Enter the 4-digit PIN shown on the host.")
                Spacer(Modifier.height(16.dp))
                Surface(
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = MaterialTheme.shapes.medium,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        "4  8  2  7",
                        style = MaterialTheme.typography.headlineMedium,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.fillMaxWidth().padding(vertical = 16.dp),
                    )
                }
                Spacer(Modifier.height(12.dp))
                Text(
                    "This device: Pixel 9 Pro",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Pair") } },
        dismissButton = { TextButton({}) { Text("Cancel") } },
    )
}

/** The live stats HUD (the real StatsOverlay) over a synthetic "streamed frame" gradient. */
@Composable
internal fun StreamScene() {
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.linearGradient(listOf(Color(0xFF2A1E5C), Color(0xFF0E1B3D), Color(0xFF06122B))),
            ),
    ) {
        // [fps, mbps, latP50, latP95, latValid, skew, w, h, hz, dropped]
        StatsOverlay(
            doubleArrayOf(238.0, 921.4, 1.3, 2.1, 1.0, 1.0, 5120.0, 1440.0, 240.0, 0.0),
            Modifier.align(Alignment.TopStart).padding(12.dp),
        )
    }
}
