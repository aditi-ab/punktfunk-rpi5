package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Home
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import kotlin.math.abs
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    /**
     * The active stream session handle (0 = not streaming). Set by [StreamScreen] while it's shown.
     * `dispatchKeyEvent` is the earliest, most reliable key hook — above Compose's focus system —
     * so hardware keys are forwarded to the host regardless of which view holds focus.
     */
    var streamHandle: Long = 0L

    /** Joystick-axis state mapper for the active session (built/reset by StreamScreen). */
    var axisMapper: Gamepad.AxisMapper? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Dark, transparent system bars regardless of the system theme — our UI is always dark, so
        // the status/nav bars blend with our surface and get light icons. (The no-arg edge-to-edge
        // picks the *system* light/dark, which left a black status bar over our dark background.)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(modifier = Modifier.fillMaxSize()) { App() }
            }
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val handle = streamHandle
        if (handle != 0L) {
            // Gamepad buttons (incl. DPAD only when truly from a gamepad — else KEYCODE_DPAD_* are
            // keyboard arrows and belong to the VK path below).
            if (event.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                val bit = Gamepad.buttonBit(event.keyCode)
                if (bit != 0) {
                    when (event.action) {
                        // repeatCount guard: don't re-send a held button as auto-repeat.
                        KeyEvent.ACTION_DOWN ->
                            if (event.repeatCount == 0) NativeBridge.nativeSendGamepadButton(handle, bit, true)
                        KeyEvent.ACTION_UP -> NativeBridge.nativeSendGamepadButton(handle, bit, false)
                    }
                    return true // consumed
                }
            }
            when (event.keyCode) {
                // Leave these to the system even while streaming.
                KeyEvent.KEYCODE_BACK, // → BackHandler leaves the stream
                KeyEvent.KEYCODE_VOLUME_UP,
                KeyEvent.KEYCODE_VOLUME_DOWN,
                KeyEvent.KEYCODE_VOLUME_MUTE,
                KeyEvent.KEYCODE_POWER -> {}
                else -> {
                    val down = when (event.action) {
                        KeyEvent.ACTION_DOWN -> true
                        KeyEvent.ACTION_UP -> false
                        else -> return super.dispatchKeyEvent(event)
                    }
                    val vk = Keymap.toVk(event.keyCode)
                    if (vk != 0) {
                        NativeBridge.nativeSendKey(handle, vk, down, 0)
                        return true // consumed — don't let the system also act on it
                    }
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streamHandle != 0L && axisMapper?.onMotion(event) == true) return true
        return super.dispatchGenericMotionEvent(event)
    }
}

/** Bottom-bar destinations (the immersive stream view is shown full-screen, outside the bar). */
private enum class Tab(val label: String, val icon: ImageVector) {
    Connect("Connect", Icons.Filled.Home),
    Settings("Settings", Icons.Filled.Settings),
}

/**
 * A trust decision awaiting the user before a connect proceeds. [name] is the label to save the
 * host under. Trust-on-first-use ([Kind.TRUST_NEW]) is only ever offered when the host ADVERTISED
 * pair=optional; a pair=required host or a manually-typed/unknown-policy host goes straight to PIN
 * pairing ([Kind.PAIR]), and a changed fingerprint forces re-pairing — never a silent re-trust.
 */
private data class PendingTrust(
    val host: String,
    val port: Int,
    val name: String,
    val advertisedFp: String?,
    val kind: Kind,
) {
    enum class Kind { TRUST_NEW, FP_CHANGED, PAIR }
}

@Composable
private fun App() {
    val context = LocalContext.current
    val settingsStore = remember { SettingsStore(context) }
    var settings by remember { mutableStateOf(settingsStore.load()) }
    var streamHandle by remember { mutableStateOf(0L) } // 0 = not streaming
    var tab by remember { mutableStateOf(Tab.Connect) }

    if (streamHandle != 0L) {
        // Immersive: the stream takes the whole screen, no bottom bar.
        StreamScreen(streamHandle, micEnabled = settings.micEnabled, onDisconnect = { streamHandle = 0L })
    } else {
        Scaffold(
            bottomBar = {
                NavigationBar {
                    Tab.entries.forEach { t ->
                        NavigationBarItem(
                            selected = tab == t,
                            onClick = { tab = t },
                            icon = { Icon(t.icon, contentDescription = t.label) },
                            label = { Text(t.label) },
                        )
                    }
                }
            },
        ) { innerPadding ->
            Box(Modifier.fillMaxSize().padding(innerPadding)) {
                when (tab) {
                    Tab.Connect -> ConnectScreen(settings = settings, onConnected = { streamHandle = it })
                    Tab.Settings -> SettingsScreen(
                        initial = settings,
                        onChange = { settings = it; settingsStore.save(it) },
                        onBack = { tab = Tab.Connect },
                    )
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ConnectScreen(settings: Settings, onConnected: (Long) -> Unit) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    var host by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    // The host streams at exactly this mode; "Native" settings resolve from the device display.
    val (w, h, hz) = settings.effectiveMode(context)

    // mDNS discovery scoped to this screen; NsdManager callbacks arrive on the main thread, so the
    // onChange callback can set Compose state directly. (Emulator SLIRP drops multicast → empty.)
    val discovery = remember { HostDiscovery(context) }
    var discovered by remember { mutableStateOf<List<DiscoveredHost>>(emptyList()) }
    DisposableEffect(Unit) {
        discovery.onChange = { discovered = it }
        discovery.start()
        onDispose {
            discovery.onChange = null
            discovery.stop()
        }
    }

    val identityStore = remember { IdentityStore(context) }
    val knownHostStore = remember { KnownHostStore(context) }
    var savedHosts by remember { mutableStateOf(knownHostStore.all()) }
    // Mint-once on genuine first run; an Unrecoverable store (decrypt failure) surfaces here and
    // refuses to connect — never silently shadow-minting a new identity (which would force re-pair).
    var identity by remember { mutableStateOf<ClientIdentity?>(null) }
    LaunchedEffect(Unit) {
        runCatching { withContext(Dispatchers.IO) { obtainIdentity(identityStore) } }
            .onSuccess { identity = it }
            .onFailure { status = "Identity unavailable: ${it.message} — re-pair may be required" }
    }
    // A trust decision awaiting the user (first-connect TOFU / fp changed / PIN pairing).
    var pendingTrust by remember { mutableStateOf<PendingTrust?>(null) }

    // Issue the actual connect with identity + (optional) pin. On a TOFU connect (pinHex null),
    // pin the fingerprint the host presented (as an unpaired known host) so the next connect goes
    // straight through and it appears in the saved-hosts list.
    fun doConnect(targetHost: String, targetPort: Int, name: String, pinHex: String?) {
        val id = identity
        if (id == null) {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        connecting = true
        status = "Connecting to $targetHost:$targetPort…"
        discovery.stop() // free the Wi-Fi radio before the stream session
        scope.launch {
            val handle = withContext(Dispatchers.IO) {
                NativeBridge.nativeConnect(
                    targetHost, targetPort, w, h, hz,
                    id.certPem, id.privateKeyPem, pinHex ?: "",
                    settings.bitrateKbps, settings.compositor, settings.gamepad,
                )
            }
            connecting = false
            if (handle != 0L) {
                if (pinHex == null) { // TOFU: pin what we observed (unpaired)
                    val fp = NativeBridge.nativeHostFingerprint(handle)
                    if (fp.isNotEmpty()) {
                        knownHostStore.save(KnownHost(targetHost, targetPort, name, fp, paired = false))
                    }
                }
                onConnected(handle)
            } else {
                status = "Connection failed — check host/port, PIN, and logcat"
                discovery.start()
            }
        }
    }

    // Decide pinned-reconnect vs fp-changed vs TOFU vs PIN pairing before connecting. Trust state is
    // keyed by address:port, so a discovered and a manually-typed connection to the same host share
    // one record. Trust-on-first-use is permitted ONLY when the host advertised pair=optional; a
    // pair=required host, or a manual/unknown-policy host, must pair by PIN.
    fun connect(targetHost: String, targetPort: Int, dh: DiscoveredHost? = null) {
        val known = knownHostStore.get(targetHost, targetPort)
        val adv = dh?.fingerprint?.lowercase()
        val name = dh?.name ?: targetHost
        when {
            // Known host whose advertised fp still matches the pin → silent pinned reconnect.
            known != null && (adv == null || adv == known.fpHex) ->
                doConnect(targetHost, targetPort, known.name, known.fpHex)
            // Known host whose fp changed → force re-pairing (no silent re-trust shortcut).
            known != null -> pendingTrust =
                PendingTrust(targetHost, targetPort, known.name, adv, PendingTrust.Kind.FP_CHANGED)
            // Host explicitly advertised pair=optional → trust-on-first-use is permitted (offer it,
            // clearly labeled, alongside PIN pairing). Smart-cast: this branch ⇒ dh != null.
            dh?.pairingRequired == false -> pendingTrust =
                PendingTrust(targetHost, targetPort, name, dh.fingerprint, PendingTrust.Kind.TRUST_NEW)
            // pair=required, or a manual/unknown-policy host → PIN pairing is mandatory.
            else -> pendingTrust =
                PendingTrust(targetHost, targetPort, name, adv, PendingTrust.Kind.PAIR)
        }
    }

    val sheetState = rememberModalBottomSheetState()
    var showManualSheet by remember { mutableStateOf(false) }

    Box(Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp, vertical = 16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Spacer(Modifier.height(8.dp))
            Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
            Text(
                "stream a remote desktop",
                style = MaterialTheme.typography.bodyMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(24.dp))

            status?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.error,
                    textAlign = TextAlign.Center,
                )
                Spacer(Modifier.height(16.dp))
            }

            if (savedHosts.isEmpty() && discovered.isEmpty()) {
                EmptyHostsState()
            }

            if (savedHosts.isNotEmpty()) {
                SectionLabel("Saved hosts")
                savedHosts.forEach { kh ->
                    HostCard(
                        name = kh.name,
                        address = "${kh.address}:${kh.port}",
                        status = if (kh.paired) HostStatus.PAIRED else HostStatus.TOFU,
                        enabled = !connecting,
                        onConnect = { connect(kh.address, kh.port) },
                        onForget = {
                            knownHostStore.remove(kh.address, kh.port)
                            savedHosts = knownHostStore.all()
                        },
                    )
                }
                Spacer(Modifier.height(20.dp))
            }

            if (discovered.isNotEmpty()) {
                SectionLabel("Discovered on the network")
                discovered.forEach { dh ->
                    HostCard(
                        name = dh.name,
                        address = "${dh.host}:${dh.port}",
                        status = if (dh.pairingRequired) HostStatus.PAIRING else HostStatus.TOFU,
                        enabled = !connecting,
                        onConnect = { connect(dh.host, dh.port, dh) },
                        onForget = null,
                    )
                }
                Spacer(Modifier.height(20.dp))
            }

            Spacer(Modifier.height(96.dp)) // clearance so the last card scrolls clear of the FAB
        }

        ExtendedFloatingActionButton(
            onClick = { showManualSheet = true },
            icon = { Icon(Icons.Filled.Add, contentDescription = null) },
            text = { Text("Add host") },
            expanded = !connecting,
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(20.dp),
        )
    }

    if (showManualSheet) {
        ModalBottomSheet(
            onDismissRequest = { showManualSheet = false },
            sheetState = sheetState,
        ) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp)
                    .padding(bottom = 32.dp),
            ) {
                Text("Add a host", style = MaterialTheme.typography.titleLarge)
                Spacer(Modifier.height(4.dp))
                Text(
                    "Enter its address. You'll pair with the host's PIN on first connect.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(20.dp))
                OutlinedTextField(
                    value = host,
                    onValueChange = { host = it },
                    label = { Text("Host") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(8.dp))
                OutlinedTextField(
                    value = port,
                    onValueChange = { v -> port = v.filter { it.isDigit() }.take(5) },
                    label = { Text("Port") },
                    singleLine = true,
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(20.dp))
                Button(
                    enabled = !connecting && host.isNotBlank() && port.isNotBlank(),
                    onClick = {
                        val h = host.trim()
                        val p = port.toIntOrNull() ?: 9777
                        scope.launch { sheetState.hide() }.invokeOnCompletion {
                            showManualSheet = false
                            connect(h, p)
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Connect  ($w×$h@$hz)") }
            }
        }
    }

    pendingTrust?.let { pt ->
        when (pt.kind) {
            PendingTrust.Kind.TRUST_NEW -> AlertDialog(
                onDismissRequest = { pendingTrust = null },
                title = { Text("Trust this host?") },
                text = {
                    Column {
                        Text("First connection to ${pt.host}:${pt.port}.")
                        pt.advertisedFp?.let { Text("Fingerprint ${it.take(16)}…") }
                        Text(
                            "This host allows trust-on-first-use, but that can't tell an impostor " +
                                "from the real host. Pairing with a PIN is stronger — it proves both sides.",
                        )
                    }
                },
                confirmButton = {
                    TextButton({ pendingTrust = null; doConnect(pt.host, pt.port, pt.name, null) }) {
                        Text("Trust (TOFU)")
                    }
                },
                dismissButton = {
                    Row {
                        TextButton({ pendingTrust = pt.copy(kind = PendingTrust.Kind.PAIR) }) {
                            Text("Pair with PIN…")
                        }
                        TextButton({ pendingTrust = null }) { Text("Cancel") }
                    }
                },
            )
            PendingTrust.Kind.FP_CHANGED -> AlertDialog(
                onDismissRequest = { pendingTrust = null },
                title = { Text("Host identity changed") },
                text = {
                    Text(
                        "The pinned fingerprint for ${pt.host} no longer matches what it now " +
                            "advertises. This can mean a host reinstall — or an impostor. Re-pair " +
                            "with the host's PIN to continue.",
                    )
                },
                confirmButton = {
                    TextButton({ pendingTrust = pt.copy(kind = PendingTrust.Kind.PAIR) }) { Text("Re-pair") }
                },
                dismissButton = {
                    TextButton({ pendingTrust = null }) { Text("Cancel") }
                },
            )
            PendingTrust.Kind.PAIR -> {
                var pin by remember(pt) { mutableStateOf("") }
                var name by remember(pt) { mutableStateOf(Build.MODEL ?: "Android") }
                var pairing by remember(pt) { mutableStateOf(false) }
                var err by remember(pt) { mutableStateOf<String?>(null) }
                AlertDialog(
                    onDismissRequest = { if (!pairing) pendingTrust = null },
                    title = { Text("Pair with PIN") },
                    text = {
                        Column {
                            Text("Enter the 4-digit PIN shown on the host.")
                            OutlinedTextField(
                                value = pin,
                                onValueChange = { v -> pin = v.filter { it.isDigit() }.take(4) },
                                label = { Text("PIN") },
                                singleLine = true,
                                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                            )
                            OutlinedTextField(
                                value = name,
                                onValueChange = { name = it },
                                label = { Text("This device") },
                                singleLine = true,
                            )
                            err?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                        }
                    },
                    confirmButton = {
                        TextButton(
                            enabled = !pairing && pin.length == 4 && identity != null,
                            onClick = {
                                val id = identity
                                if (id != null) {
                                    pairing = true
                                    err = null
                                    scope.launch {
                                        val fp = withContext(Dispatchers.IO) {
                                            NativeBridge.nativePair(
                                                pt.host, pt.port, id.certPem, id.privateKeyPem, pin, name,
                                            )
                                        }
                                        pairing = false
                                        if (fp.isNotEmpty()) {
                                            // Verified host fp — save as a paired known host.
                                            knownHostStore.save(
                                                KnownHost(pt.host, pt.port, pt.name, fp, paired = true),
                                            )
                                            savedHosts = knownHostStore.all()
                                            pendingTrust = null
                                            doConnect(pt.host, pt.port, pt.name, fp)
                                        } else {
                                            err = "Pairing failed — wrong PIN, or the host isn't armed."
                                        }
                                    }
                                }
                            },
                        ) { Text(if (pairing) "Pairing…" else "Pair") }
                    },
                    dismissButton = {
                        TextButton(enabled = !pairing, onClick = { pendingTrust = null }) { Text("Cancel") }
                    },
                )
            }
        }
    }
}

/** Left-aligned section header above each block of the connect screen. */
@Composable
private fun SectionLabel(text: String) {
    Text(
        text,
        style = MaterialTheme.typography.titleSmall,
        color = MaterialTheme.colorScheme.primary,
        modifier = Modifier.fillMaxWidth().padding(bottom = 8.dp),
    )
}

/** Trust state of a host, shown as a colored pill on its card. */
private enum class HostStatus(val label: String) {
    PAIRED("Paired"),
    PAIRING("PIN pairing"),
    TOFU("Trust on first use"),
}

/**
 * A host as an Apple-style card: a colored letter-avatar, name + address, a trust pill, and (for
 * saved hosts) an overflow menu with Forget. Tapping the card connects.
 */
@Composable
private fun HostCard(
    name: String,
    address: String,
    status: HostStatus,
    enabled: Boolean,
    onConnect: () -> Unit,
    onForget: (() -> Unit)?,
) {
    ElevatedCard(
        onClick = onConnect,
        enabled = enabled,
        modifier = Modifier.fillMaxWidth().padding(vertical = 5.dp),
    ) {
        Row(
            modifier = Modifier.fillMaxWidth().padding(start = 14.dp, top = 12.dp, bottom = 12.dp, end = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            HostAvatar(name)
            Spacer(Modifier.width(14.dp))
            Column(Modifier.weight(1f)) {
                Text(
                    name,
                    style = MaterialTheme.typography.titleMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    address,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(6.dp))
                StatusPill(status)
            }
            if (onForget != null) {
                var menu by remember { mutableStateOf(false) }
                Box {
                    IconButton(enabled = enabled, onClick = { menu = true }) {
                        Icon(Icons.Filled.MoreVert, contentDescription = "More")
                    }
                    DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                        DropdownMenuItem(
                            text = { Text("Forget") },
                            onClick = {
                                menu = false
                                onForget()
                            },
                        )
                    }
                }
            } else {
                Spacer(Modifier.width(8.dp))
            }
        }
    }
}

/** A circular avatar with the host's first letter (Apple-contact style). */
@Composable
private fun HostAvatar(name: String) {
    val letter = name.trim().firstOrNull()?.uppercaseChar()?.toString() ?: "?"
    Box(
        modifier = Modifier
            .size(44.dp)
            .clip(CircleShape)
            .background(MaterialTheme.colorScheme.primaryContainer),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            letter,
            style = MaterialTheme.typography.titleMedium,
            color = MaterialTheme.colorScheme.onPrimaryContainer,
        )
    }
}

/** A small colored dot + label for the host's trust state. */
@Composable
private fun StatusPill(status: HostStatus) {
    val color = when (status) {
        HostStatus.PAIRED -> MaterialTheme.colorScheme.primary
        HostStatus.PAIRING -> MaterialTheme.colorScheme.tertiary
        HostStatus.TOFU -> MaterialTheme.colorScheme.onSurfaceVariant
    }
    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.size(8.dp).clip(CircleShape).background(color))
        Spacer(Modifier.width(6.dp))
        Text(status.label, style = MaterialTheme.typography.labelMedium, color = color)
    }
}

/** Shown when there are no saved or discovered hosts. */
@Composable
private fun EmptyHostsState() {
    Column(
        modifier = Modifier.fillMaxWidth().padding(vertical = 56.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("No hosts yet", style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(8.dp))
        Text(
            "Hosts on your network show up here automatically.\nTap “Add host” to enter one by address.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
    }
}

@Composable
private fun StreamScreen(handle: Long, micEnabled: Boolean, onDisconnect: () -> Unit) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val window = activity?.window
    // Start mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    val micWanted = micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        activity?.streamHandle = handle // route hardware keys to this session
        activity?.axisMapper = Gamepad.AxisMapper(handle) // route joystick axes
        // Host→client feedback (rumble + DualSense lightbar/LEDs); poll threads stopped before close.
        val feedback = GamepadFeedback(handle).also { it.start() }
        onDispose {
            feedback.stop() // stop + join the poll threads BEFORE nativeClose frees the handle
            activity?.axisMapper?.reset() // release-all so nothing sticks on the host
            activity?.axisMapper = null
            activity?.streamHandle = 0L
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            // Leaving the stream: stop the mic + audio + decode threads and tear down the session.
            NativeBridge.nativeStopMic(handle)
            NativeBridge.nativeStopAudio(handle)
            NativeBridge.nativeStopVideo(handle)
            NativeBridge.nativeClose(handle)
        }
    }

    BackHandler { onDisconnect() }

    Box(modifier = Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            NativeBridge.nativeStartVideo(handle, holder.surface)
                            NativeBridge.nativeStartAudio(handle)
                            if (micWanted) NativeBridge.nativeStartMic(handle)
                        }

                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            NativeBridge.nativeStopMic(handle)
                            NativeBridge.nativeStopAudio(handle)
                            NativeBridge.nativeStopVideo(handle)
                        }
                    })
                }
            },
        )
        // Touch virtual-trackpad overlay: 1-finger drag → relative mouse move; tap → left click;
        // 2-finger drag → scroll. (Physical-mouse pointer capture comes in a later increment.)
        Box(
            Modifier.fillMaxSize().pointerInput(handle) {
                awaitEachGesture {
                    val first = awaitFirstDown(requireUnconsumed = false)
                    var moved = false
                    var maxFingers = 1
                    while (true) {
                        val ev = awaitPointerEvent()
                        val fingers = ev.changes.count { it.pressed }
                        if (fingers == 0) break
                        if (fingers > maxFingers) maxFingers = fingers
                        val primary = ev.changes.firstOrNull { it.id == first.id } ?: ev.changes.first()
                        val d = primary.positionChange()
                        if (abs(d.x) > 0.5f || abs(d.y) > 0.5f) {
                            moved = true
                            if (fingers >= 2) {
                                // screen +y down → wire +up, so negate y. Coarse divisor; tune live.
                                val sy = (-d.y / 4f).toInt()
                                val sx = (d.x / 4f).toInt()
                                if (sy != 0) NativeBridge.nativeSendScroll(handle, 0, sy * 120)
                                if (sx != 0) NativeBridge.nativeSendScroll(handle, 1, sx * 120)
                            } else {
                                NativeBridge.nativeSendPointerMove(handle, d.x.toInt(), d.y.toInt())
                            }
                        }
                        ev.changes.forEach { it.consume() }
                    }
                    if (!moved && maxFingers == 1) {
                        NativeBridge.nativeSendPointerButton(handle, 1, true)
                        NativeBridge.nativeSendPointerButton(handle, 1, false)
                    }
                }
            },
        )
    }
}
