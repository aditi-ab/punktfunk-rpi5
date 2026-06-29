package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import io.unom.punktfunk.components.EmptyHostsState
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.HostStatus
import io.unom.punktfunk.models.PendingTrust
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** Handshake budget for a normal connect (the prior hardcoded value, now passed explicitly). */
private const val CONNECT_TIMEOUT_MS = 10_000

/**
 * Handshake budget for the no-PIN "request access" connect. Must exceed the host's approval-park
 * window (~180 s) so a slow operator approval still lands on this same parked connection rather than
 * timing the client out first. Mirrors the Linux client's 185 s.
 */
private const val REQUEST_ACCESS_TIMEOUT_MS = 185_000

/**
 * A no-PIN "request access" connect in flight — the host being requested (drives the cancelable
 * "Waiting for approval…" dialog) and a per-attempt flag the Cancel button trips. The connect is a
 * blocking call with no abort, so Cancel returns the UI immediately and a late result checks
 * [cancelled] and tears the (possibly just-approved) session down silently rather than navigating.
 */
private class RequestAccessState(val target: PendingTrust) {
    val cancelled = AtomicBoolean(false)
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectScreen(settings: Settings, onConnected: (Long) -> Unit) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    var host by remember { mutableStateOf("") }
    var hostName by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    // The host streams at exactly this mode; "Native" settings resolve from the device display.
    val (w, h, hz) = settings.effectiveMode(context)

    // mDNS discovery scoped to this screen, via the native mdns-sd browse (HostDiscovery) — its
    // onChange fires on the main thread, so it can set Compose state directly. (Emulator SLIRP drops
    // multicast → empty; that's the network, not the API.) Raw multicast reception only needs the
    // Wi-Fi MulticastLock (HostDiscovery holds it), NOT NEARBY_WIFI_DEVICES — that gated the old
    // NsdManager path. We still request NEARBY_WIFI_DEVICES opportunistically (some OEMs filter
    // multicast without it; harmless where it isn't), but never block discovery on the grant — a
    // denial used to leave discovery dead forever.
    val discovery = remember { HostDiscovery(context) }
    var discovered by remember { mutableStateOf<List<DiscoveredHost>>(emptyList()) }
    val nearbyLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { _ -> /* best-effort hint; discovery runs regardless of the result */ }
    LaunchedEffect(Unit) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !hasNearbyPermission(context)) {
            nearbyLauncher.launch(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
    }
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
    // A trust decision awaiting the user (first-connect TOFU / fp changed / PIN pairing / the
    // request-access-or-PIN choice).
    var pendingTrust by remember { mutableStateOf<PendingTrust?>(null) }
    // A no-PIN "request access" connect in flight (the cancelable "Waiting for approval…" dialog).
    var awaiting by remember { mutableStateOf<RequestAccessState?>(null) }
    // A saved host whose label is being edited (the Rename dialog).
    var renameTarget by remember { mutableStateOf<KnownHost?>(null) }

    // Discovered hosts not already saved — a saved host (paired or TOFU) belongs in "Saved hosts",
    // not also in "Discovered", so we hide the overlap (matched by fingerprint when both carry it, so
    // it survives a DHCP address change; else by address:port). Mirrors the Apple client.
    val discoveredUnsaved = discovered.filter { dh -> savedHosts.none { it.matches(dh) } }

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
            // Advertise HDR only when the user enabled it AND this device's display can present it
            // (else the host sends a proper SDR stream rather than PQ the panel would mis-tone-map).
            val hdrEnabled = settings.hdrEnabled && displaySupportsHdr(context)
            // "Automatic" resolves to a concrete pad type from the connected controller's VID/PID
            // (Android exposes no controller-type enum) — parity with the Linux/Apple clients. An
            // explicit choice is passed through unchanged.
            val gamepadPref = Gamepad.resolvePref(settings.gamepad)
            val handle = withContext(Dispatchers.IO) {
                NativeBridge.nativeConnect(
                    targetHost, targetPort, w, h, hz,
                    id.certPem, id.privateKeyPem, pinHex ?: "",
                    settings.bitrateKbps, settings.compositor, gamepadPref,
                    hdrEnabled, settings.audioChannels, CONNECT_TIMEOUT_MS,
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

    // The no-PIN "request access" path (delegated approval): open a normal identified connect that
    // the host PARKS until the operator clicks Approve in its console/web UI, showing a cancelable
    // "Waiting for approval…" dialog meanwhile. The SAME connection is admitted on approval (no
    // reconnect), so on success we record the host as PAIRED — the operator's approval IS the pairing.
    // The connect can't be aborted, so Cancel returns the UI immediately and a late result is torn
    // down silently via the per-attempt flag (mirrors the Linux client's request-access flow).
    fun requestAccess(target: PendingTrust) {
        val id = identity
        if (id == null) {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        val req = RequestAccessState(target)
        awaiting = req
        connecting = true
        status = null
        discovery.stop() // free the Wi-Fi radio before the (parked) stream session
        scope.launch {
            val hdrEnabled = settings.hdrEnabled && displaySupportsHdr(context)
            val gamepadPref = Gamepad.resolvePref(settings.gamepad)
            // Pin the advertised fingerprint for a discovered host (defence against an impostor while
            // we wait); a manually-typed host has none, so trust-on-first-use.
            val pinHex = target.advertisedFp ?: ""
            val handle = withContext(Dispatchers.IO) {
                NativeBridge.nativeConnect(
                    target.host, target.port, w, h, hz,
                    id.certPem, id.privateKeyPem, pinHex,
                    settings.bitrateKbps, settings.compositor, gamepadPref,
                    hdrEnabled, settings.audioChannels, REQUEST_ACCESS_TIMEOUT_MS,
                )
            }
            // Cancelled while we were parked: tear the (possibly just-approved) session down and
            // don't touch UI a fresh action may now own.
            if (req.cancelled.get()) {
                if (handle != 0L) withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
                return@launch
            }
            awaiting = null
            connecting = false
            if (handle != 0L) {
                // Approved — save the host as PAIRED, pinning the fingerprint it presented, so
                // future connects are silent (exactly like after a PIN ceremony).
                val fp = NativeBridge.nativeHostFingerprint(handle)
                if (fp.isNotEmpty()) {
                    knownHostStore.save(KnownHost(target.host, target.port, target.name, fp, paired = true))
                    savedHosts = knownHostStore.all()
                }
                onConnected(handle)
            } else {
                status = "Request timed out — approve this device in the host's console, then retry."
                discovery.start()
            }
        }
    }

    // Decide pinned-reconnect vs fp-changed vs TOFU vs pairing before connecting. Trust state is
    // keyed by address:port, so a discovered and a manually-typed connection to the same host share
    // one record. Trust-on-first-use is permitted ONLY when the host advertised pair=optional; a
    // pair=required host, or a manual/unknown-policy host, must pair — either by no-PIN request
    // access (approve in the console) or by the SPAKE2 PIN ceremony.
    fun connect(
        targetHost: String,
        targetPort: Int,
        dh: DiscoveredHost? = null,
        manualName: String? = null,
    ) {
        val known = knownHostStore.get(targetHost, targetPort)
        val adv = dh?.fingerprint?.lowercase()
        // Label precedence: a saved host keeps its (possibly user-renamed) name; else the discovered
        // mDNS name; else the name typed in the Add-host sheet; else the bare address.
        val name = known?.name ?: dh?.name ?: manualName?.trim()?.takeIf { it.isNotEmpty() } ?: targetHost
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
            // pair=required, or a manual/unknown-policy host → offer the two ways in: a no-PIN
            // "request access" (approve in the console) or the SPAKE2 PIN ceremony.
            else -> pendingTrust =
                PendingTrust(targetHost, targetPort, name, adv, PendingTrust.Kind.REQUEST_ACCESS)
        }
    }

    val sheetState = rememberModalBottomSheetState()
    var showManualSheet by remember { mutableStateOf(false) }

    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Spacer(Modifier.height(8.dp))
                    Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "stream a remote desktop",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(24.dp))

                    status?.let {
                        // While connecting it's progress (spinner, neutral); otherwise it's a
                        // result/error (red). Previously every status showed in error-red, so a
                        // normal "Connecting…" looked like a failure.
                        if (connecting) {
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                CircularProgressIndicator(
                                    modifier = Modifier.size(16.dp),
                                    strokeWidth = 2.dp,
                                )
                                Text(
                                    it,
                                    style = MaterialTheme.typography.bodyMedium,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                            }
                        } else {
                            // Result/error: a filled error container reads as a real failure banner,
                            // not just red text lost in the layout.
                            Surface(
                                color = MaterialTheme.colorScheme.errorContainer,
                                shape = MaterialTheme.shapes.medium,
                                modifier = Modifier.fillMaxWidth(),
                            ) {
                                Text(
                                    it,
                                    style = MaterialTheme.typography.bodyMedium,
                                    color = MaterialTheme.colorScheme.onErrorContainer,
                                    textAlign = TextAlign.Center,
                                    modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                                )
                            }
                        }
                        Spacer(Modifier.height(16.dp))
                    }
                }
            }

            if (savedHosts.isEmpty() && discoveredUnsaved.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    EmptyHostsState()
                }
            }

            if (savedHosts.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    SectionLabel("Saved hosts")
                }
                items(savedHosts, key = { "saved-${it.address}-${it.port}" }) { kh ->
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
                        onRename = { renameTarget = kh },
                    )
                }
            }

            if (discoveredUnsaved.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Spacer(Modifier.height(12.dp))
                    SectionLabel("Discovered on the network")
                }
                items(discoveredUnsaved, key = { "disc-${it.host}-${it.port}" }) { dh ->
                    HostCard(
                        name = dh.name,
                        address = "${dh.host}:${dh.port}",
                        status = if (dh.pairingRequired) HostStatus.PAIRING else HostStatus.TOFU,
                        enabled = !connecting,
                        onConnect = { connect(dh.host, dh.port, dh) },
                        onForget = null,
                    )
                }
            }

            // Active-discovery hint: discovery runs whenever this screen is up, so while it's
            // scanning but nothing's turned up yet (and we're not mid-connect), show it's working
            // rather than looking idle/empty.
            if (!connecting && discovered.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CircularProgressIndicator(modifier = Modifier.size(16.dp), strokeWidth = 2.dp)
                        Spacer(Modifier.width(8.dp))
                        Text(
                            "Searching the local network…",
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(96.dp))
            }
        }

        AnimatedVisibility(
            visible = true, // Static for now, could be based on scroll if needed
            enter = scaleIn() + fadeIn(),
            exit = scaleOut() + fadeOut(),
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(20.dp)
        ) {
            ExtendedFloatingActionButton(
                onClick = { showManualSheet = true },
                icon = { Icon(Icons.Filled.Add, contentDescription = null) },
                text = { Text("Add host") },
                expanded = !connecting,
            )
        }
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
                    value = hostName,
                    onValueChange = { hostName = it },
                    label = { Text("Name (optional)") },
                    placeholder = { Text("e.g. Living Room") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(16.dp))
                OutlinedTextField(
                    value = host,
                    onValueChange = { host = it },
                    label = { Text("Host") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(16.dp))
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
                        val n = hostName
                        scope.launch { sheetState.hide() }.invokeOnCompletion {
                            showManualSheet = false
                            connect(h, p, manualName = n)
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
            // A fresh pair=required (or manual/unknown-policy) host: offer the two ways in. "Request
            // access" is the no-PIN path — connect and wait for the operator to click Approve in the
            // host's console; "Use a PIN…" switches to the SPAKE2 ceremony.
            PendingTrust.Kind.REQUEST_ACCESS -> AlertDialog(
                onDismissRequest = { pendingTrust = null },
                title = { Text("Pairing required") },
                text = {
                    Column {
                        Text("${pt.host}:${pt.port} requires pairing before it will stream.")
                        Text(
                            "Request access and approve this device in the host's console (or web " +
                                "UI) — no PIN needed. Or pair with the 4-digit PIN the host displays.",
                        )
                    }
                },
                confirmButton = {
                    TextButton({ pendingTrust = null; requestAccess(pt) }) { Text("Request access") }
                },
                dismissButton = {
                    Row {
                        TextButton({ pendingTrust = pt.copy(kind = PendingTrust.Kind.PAIR) }) {
                            Text("Use a PIN…")
                        }
                        TextButton({ pendingTrust = null }) { Text("Cancel") }
                    }
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

    // The no-PIN "request access" wait: the connect is parked on the host until the operator
    // approves this device. Cancel returns the UI immediately — it trips the per-attempt flag so a
    // late approval is torn down silently (see requestAccess) and resumes discovery.
    awaiting?.let { req ->
        fun cancel() {
            req.cancelled.set(true)
            awaiting = null
            connecting = false
            discovery.start() // the request may still be pending on the host; keep scanning
        }
        AlertDialog(
            onDismissRequest = { cancel() },
            title = { Text("Waiting for approval") },
            text = {
                val deviceName = Build.MODEL ?: "this device"
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp)
                        Text("Approve this device on ${req.target.name}.")
                    }
                    Text(
                        "Open the host's console (or web UI) and approve “$deviceName”. It connects " +
                            "automatically once you approve — no PIN needed.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            },
            confirmButton = {},
            dismissButton = {
                TextButton(onClick = { cancel() }) { Text("Cancel") }
            },
        )
    }

    // Rename a saved host's label (discovered hosts are named by mDNS; this is how you give one a
    // friendly name like "Living Room" after pairing). Keyed by the host so reopening resets the field.
    renameTarget?.let { kh ->
        var newName by remember(kh) { mutableStateOf(kh.name) }
        AlertDialog(
            onDismissRequest = { renameTarget = null },
            title = { Text("Rename host") },
            text = {
                OutlinedTextField(
                    value = newName,
                    onValueChange = { newName = it },
                    label = { Text("Name") },
                    placeholder = { Text(kh.address) },
                    singleLine = true,
                )
            },
            confirmButton = {
                TextButton(
                    enabled = newName.isNotBlank(),
                    onClick = {
                        knownHostStore.rename(kh.address, kh.port, newName.trim())
                        savedHosts = knownHostStore.all()
                        renameTarget = null
                    },
                ) { Text("Save") }
            },
            dismissButton = {
                TextButton(onClick = { renameTarget = null }) { Text("Cancel") }
            },
        )
    }
}

/**
 * Whether NEARBY_WIFI_DEVICES is held (API 33+; not applicable below). We request it opportunistically
 * as a multicast-reception hedge on OEMs that filter multicast without it, but discovery (raw mDNS via
 * the native core + MulticastLock) does not depend on it.
 */
fun hasNearbyPermission(context: Context): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
        ContextCompat.checkSelfPermission(context, Manifest.permission.NEARBY_WIFI_DEVICES) ==
        PackageManager.PERMISSION_GRANTED

/**
 * True when a saved host and a discovered advert are the same machine — matched by certificate
 * fingerprint when both carry it (so it survives a DHCP address change), else by address:port.
 * Mirrors the Apple client's `StoredHost.matches`; de-dupes "Discovered" against "Saved hosts".
 */
private fun KnownHost.matches(dh: DiscoveredHost): Boolean {
    val advFp = dh.fingerprint?.lowercase()
    if (!advFp.isNullOrEmpty() && fpHex.isNotEmpty() && fpHex.lowercase() == advFp) return true
    return address == dh.host && port == dh.port
}
