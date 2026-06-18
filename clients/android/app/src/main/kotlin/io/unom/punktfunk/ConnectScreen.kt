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
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ConnectScreen(settings: Settings, onConnected: (Long) -> Unit) {
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
    // NsdManager discovery needs NEARBY_WIFI_DEVICES on Android 13+ (a runtime permission) — without
    // it discoverServices silently finds nothing. Request it once, then (re)start discovery on grant.
    val discovery = remember { HostDiscovery(context) }
    var discovered by remember { mutableStateOf<List<DiscoveredHost>>(emptyList()) }
    var nearbyGranted by remember { mutableStateOf(hasNearbyPermission(context)) }
    val nearbyLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> nearbyGranted = granted }
    LaunchedEffect(Unit) {
        if (!nearbyGranted && Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            nearbyLauncher.launch(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
    }
    DisposableEffect(nearbyGranted) {
        discovery.onChange = { discovered = it }
        if (nearbyGranted) discovery.start()
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
                            Text(
                                it,
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.error,
                                textAlign = TextAlign.Center,
                            )
                        }
                        Spacer(Modifier.height(16.dp))
                    }
                }
            }

            if (savedHosts.isEmpty() && discovered.isEmpty()) {
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
                    )
                }
            }

            if (discovered.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Spacer(Modifier.height(12.dp))
                    SectionLabel("Discovered on the network")
                }
                items(discovered, key = { "disc-${it.host}-${it.port}" }) { dh ->
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

/** NsdManager discovery needs NEARBY_WIFI_DEVICES on API 33+; below that it doesn't apply. */
fun hasNearbyPermission(context: Context): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
        ContextCompat.checkSelfPermission(context, Manifest.permission.NEARBY_WIFI_DEVICES) ==
        PackageManager.PERMISSION_GRANTED
