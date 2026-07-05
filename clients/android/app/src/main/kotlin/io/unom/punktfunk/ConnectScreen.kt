package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
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
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
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

@Composable
fun ConnectScreen(
    settings: Settings,
    onConnected: (Long) -> Unit,
    // Console (gamepad) mode: render the host carousel instead of the touch grid, sharing all of this
    // screen's connect/trust/discovery logic. [onOpenSettings]/[onOpenLibrary] are the X/Y actions the
    // gamepad shell owns (the touch UI reaches Settings via the bottom bar and has no library button).
    gamepadUi: Boolean = false,
    onOpenSettings: () -> Unit = {},
    onOpenLibrary: (KnownHost) -> Unit = {},
    navGate: Boolean = true, // false while the console home is cross-fading out
) {
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
    // Wakes a sleeping saved host and waits for it to reappear on mDNS before dialing (its overlay
    // rides over both the touch and console home). Fire-and-forget WoL isn't enough — a cold boot can
    // take a minute-plus to advertise again.
    val waker = remember { WakeController(scope) }
    // Learn wake MAC(s) from live adverts for hosts we've saved (parity with the desktop clients),
    // so we can Wake-on-LAN them once they sleep. Runs only when the discovered set changes; the
    // prefs write is guarded (no-op when unchanged), and we refresh the saved list only if a MAC
    // was actually newly learned.
    LaunchedEffect(discovered) {
        val learned = withContext(Dispatchers.IO) {
            var any = false
            discovered.forEach { dh ->
                if (dh.mac.isNotEmpty() &&
                    knownHostStore.get(dh.host, dh.port)?.let { it.mac != dh.mac } == true
                ) {
                    knownHostStore.learnMac(dh.host, dh.port, dh.mac)
                    any = true
                }
            }
            any
        }
        if (learned) savedHosts = knownHostStore.all()
    }
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
    // A saved host being edited (name / address / port / MAC).
    var editTarget by remember { mutableStateOf<KnownHost?>(null) }
    // A saved host whose console options menu (Wake / Edit / Forget) is open — reached with Up on the
    // carousel (the console counterpart of the touch host card's overflow menu).
    var optionsTarget by remember { mutableStateOf<KnownHost?>(null) }

    // Discovered hosts not already saved — a saved host (paired or TOFU) belongs in "Saved hosts",
    // not also in "Discovered", so we hide the overlap (matched by fingerprint when both carry it, so
    // it survives a DHCP address change; else by address:port). Mirrors the Apple client.
    val discoveredUnsaved = discovered.filter { dh -> savedHosts.none { it.matches(dh) } }

    // The one place the full nativeConnect is issued (shared by the normal connect and the
    // request-access path), including the HDR/gamepad derivation both need.
    suspend fun connectNative(id: ClientIdentity, targetHost: String, targetPort: Int, pinHex: String, timeoutMs: Int): Long {
        // Advertise HDR only when the user enabled it AND this device's display can present it
        // (else the host sends a proper SDR stream rather than PQ the panel would mis-tone-map).
        val hdrEnabled = settings.hdrEnabled && displaySupportsHdr(context)
        // "Automatic" resolves to a concrete pad type from the connected controller's VID/PID
        // (Android exposes no controller-type enum) — parity with the Linux/Apple clients. An
        // explicit choice is passed through unchanged.
        val gamepadPref = Gamepad.resolvePref(settings.gamepad)
        return withContext(Dispatchers.IO) {
            NativeBridge.nativeConnect(
                targetHost, targetPort, w, h, hz,
                id.certPem, id.privateKeyPem, pinHex,
                settings.bitrateKbps, settings.compositor, gamepadPref,
                hdrEnabled, settings.audioChannels, settings.preferredCodec(), timeoutMs,
            )
        }
    }

    // The actual dial (identity already ready). On a TOFU connect (pinHex null), pin the fingerprint
    // the host presented (as an unpaired known host) so the next connect goes straight through and it
    // appears in the saved-hosts list.
    fun doConnectDirect(targetHost: String, targetPort: Int, name: String, pinHex: String?) {
        val id = identity ?: run {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        connecting = true
        status = "Connecting to $targetHost:$targetPort…"
        discovery.stop() // free the Wi-Fi radio before the stream session
        scope.launch {
            val handle = connectNative(id, targetHost, targetPort, pinHex ?: "", CONNECT_TIMEOUT_MS)
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

    // Wake-aware connect. If the target is a saved host with a learned MAC that ISN'T currently
    // advertising (asleep/off), wake it and WAIT for it to reappear on mDNS (WakeController shows the
    // "Waking…" overlay) before dialing — discovery stays running meanwhile so we can see it come
    // back. A fire-and-forget packet + the connect timeout wasn't enough for a cold boot. Otherwise
    // dial straight through.
    fun doConnect(targetHost: String, targetPort: Int, name: String, pinHex: String?) {
        if (identity == null) {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        val kh = knownHostStore.get(targetHost, targetPort)
        val macs = kh?.mac ?: emptyList()
        // "Up" = a live advert that is THIS host — matched by fingerprint first (so it survives a DHCP
        // address change on a cold boot), else by address:port. Returns the CURRENT advert so we can
        // dial its live address rather than the stale saved one.
        fun liveAdvert(): DiscoveredHost? =
            if (kh != null) discovered.firstOrNull { kh.matches(it) }
            else discovered.firstOrNull { it.host == targetHost && it.port == targetPort }
        if (macs.isNotEmpty() && liveAdvert() == null) {
            waker.start(
                hostName = name,
                connectsAfter = true,
                macs = macs,
                lastIp = targetHost,
                isOnline = { liveAdvert() != null },
                onOnline = {
                    val live = liveAdvert()
                    // Woke back on a new address? Re-key the saved record so it (and future connects)
                    // point at the live one, then dial there.
                    if (live != null && kh != null && (live.host != kh.address || live.port != kh.port)) {
                        knownHostStore.update(kh.address, kh.port, kh.copy(address = live.host, port = live.port))
                        savedHosts = knownHostStore.all()
                    }
                    doConnectDirect(live?.host ?: targetHost, live?.port ?: targetPort, name, pinHex)
                },
            )
        } else {
            doConnectDirect(targetHost, targetPort, name, pinHex)
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
            // Pin the advertised fingerprint for a discovered host (defence against an impostor while
            // we wait); a manually-typed host has none, so trust-on-first-use.
            val pinHex = target.advertisedFp ?: ""
            val handle = connectNative(id, target.host, target.port, pinHex, REQUEST_ACCESS_TIMEOUT_MS)
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

    var showManualSheet by remember { mutableStateOf(false) }

    if (gamepadUi) {
        // Console mode: the host carousel (saved → discovered → Add Host), driven by the pad. Shares
        // every action above; the trailing Add Host tile opens the same manual-entry sheet.
        val tiles = buildList {
            savedHosts.forEach { kh ->
                add(
                    HomeTile(
                        id = "saved-${kh.address}:${kh.port}",
                        title = kh.name,
                        subtitle = "${kh.address}:${kh.port}",
                        filled = true,
                        online = discovered.any { it.host == kh.address && it.port == kh.port },
                        paired = kh.paired,
                        knownHost = kh,
                        activate = { connect(kh.address, kh.port) },
                    ),
                )
            }
            discoveredUnsaved.forEach { dh ->
                add(
                    HomeTile(
                        id = "disc-${dh.host}:${dh.port}",
                        title = dh.name,
                        subtitle = "${dh.host}:${dh.port}",
                        online = true,
                        activate = { connect(dh.host, dh.port, dh) },
                    ),
                )
            }
            add(
                HomeTile(
                    id = "add",
                    title = "Add Host",
                    subtitle = "Register a host by address",
                    isAdd = true,
                    activate = { showManualSheet = true },
                ),
            )
        }
        GamepadHome(
            tiles = tiles,
            libraryEnabled = settings.libraryEnabled,
            controllerName = io.unom.punktfunk.kit.Gamepad.firstPad()?.name,
            // Stop the carousel from consuming the pad while a sheet/dialog/overlay owns the screen,
            // while a connect is in flight (else a second A launches a concurrent connect that leaks a
            // handle — the touch grid guards the same way with enabled=!connecting), or while the whole
            // console home is cross-fading out.
            navActive = navGate && !connecting && !showManualSheet && pendingTrust == null &&
                awaiting == null && editTarget == null && optionsTarget == null && waker.waking == null,
            onActivate = { it.activate() },
            onOpenLibrary = { it.knownHost?.let(onOpenLibrary) },
            onOpenSettings = onOpenSettings,
            onOptions = { it.knownHost?.let { kh -> optionsTarget = kh } },
        )
    } else {
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
                        onEdit = { editTarget = kh },
                        // Explicit wake-only: offered when the host is offline and we have a MAC. Runs
                        // through the WakeController so it shows the "Waking…" overlay and waits for
                        // the host to come online (matched by fingerprint, so a new DHCP address on a
                        // cold boot still counts as "up") rather than firing a single silent packet.
                        onWake = if (kh.mac.isNotEmpty() && discovered.none { kh.matches(it) }) {
                            {
                                waker.start(
                                    hostName = kh.name,
                                    connectsAfter = false,
                                    macs = kh.mac,
                                    lastIp = kh.address,
                                    isOnline = { discovered.any { kh.matches(it) } },
                                    onOnline = {},
                                )
                            }
                        } else {
                            null
                        },
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
    }

    if (showManualSheet) {
        if (gamepadUi) {
            // Console add-host: field list + on-screen controller keyboard. "Add" connects (which
            // saves the host on TOFU/pair), exactly like the touch sheet's Connect.
            GamepadAddHostScreen(
                onAdd = { n, addr, p ->
                    showManualSheet = false
                    connect(addr, p, manualName = n)
                },
                onDismiss = { showManualSheet = false },
            )
        } else {
            AddHostSheet(
                hostName = hostName,
                onHostNameChange = { hostName = it },
                host = host,
                onHostChange = { host = it },
                port = port,
                onPortChange = { port = it },
                connecting = connecting,
                modeLabel = "$w×$h@$hz",
                onDismiss = { showManualSheet = false },
                onConnect = { h2, p, n -> connect(h2, p, manualName = n) },
            )
        }
    }

    pendingTrust?.let { pt ->
        // Same trust/pairing logic, console-styled + controller-navigable in gamepad mode.
        val onPair = { pendingTrust = pt.copy(kind = PendingTrust.Kind.PAIR) }
        val onSavePaired = { fp: String ->
            knownHostStore.save(KnownHost(pt.host, pt.port, pt.name, fp, paired = true))
            savedHosts = knownHostStore.all()
            pendingTrust = null
            doConnect(pt.host, pt.port, pt.name, fp)
        }
        when (pt.kind) {
            PendingTrust.Kind.TRUST_NEW ->
                if (gamepadUi) GamepadTrustNewDialog(pt, { pendingTrust = null; doConnect(pt.host, pt.port, pt.name, null) }, onPair, { pendingTrust = null })
                else TrustNewHostDialog(pt, { pendingTrust = null; doConnect(pt.host, pt.port, pt.name, null) }, onPair, { pendingTrust = null })
            PendingTrust.Kind.FP_CHANGED ->
                if (gamepadUi) GamepadFingerprintChangedDialog(pt, onPair, { pendingTrust = null })
                else FingerprintChangedDialog(pt, onPair, { pendingTrust = null })
            PendingTrust.Kind.REQUEST_ACCESS ->
                if (gamepadUi) GamepadRequestAccessDialog(pt, { pendingTrust = null; requestAccess(pt) }, onPair, { pendingTrust = null })
                else RequestAccessDialog(pt, { pendingTrust = null; requestAccess(pt) }, onPair, { pendingTrust = null })
            PendingTrust.Kind.PAIR ->
                if (gamepadUi) GamepadPairPinDialog(pt, identity, onSavePaired, { pendingTrust = null })
                else PairPinDialog(pt, identity, onSavePaired, { pendingTrust = null })
        }
    }

    awaiting?.let { req ->
        val onCancel = {
            req.cancelled.set(true)
            awaiting = null
            connecting = false
            discovery.start() // the request may still be pending on the host; keep scanning
        }
        if (gamepadUi) GamepadAwaitingApprovalDialog(req.target.name, onCancel)
        else AwaitingApprovalDialog(hostLabel = req.target.name, onCancel = onCancel)
    }

    // Console host options (Up on a saved carousel tile): Wake / Edit / Forget.
    optionsTarget?.let { kh ->
        val offline = discovered.none { kh.matches(it) }
        GamepadHostOptionsDialog(
            hostName = kh.name,
            canWake = kh.mac.isNotEmpty() && offline,
            onWake = {
                optionsTarget = null
                waker.start(
                    hostName = kh.name, connectsAfter = false, macs = kh.mac, lastIp = kh.address,
                    isOnline = { discovered.any { kh.matches(it) } },
                    onOnline = {},
                )
            },
            // A saved host always has a library (it's a knownHost) → offer it when the setting's on,
            // so a TV remote reaches the library here instead of via the Y face button.
            onLibrary = if (settings.libraryEnabled) {
                { optionsTarget = null; onOpenLibrary(kh) }
            } else {
                null
            },
            onEdit = { optionsTarget = null; editTarget = kh },
            onForget = {
                knownHostStore.remove(kh.address, kh.port)
                savedHosts = knownHostStore.all()
                optionsTarget = null
            },
            onDismiss = { optionsTarget = null },
        )
    }

    editTarget?.let { kh ->
        // Prefill a not-yet-learned MAC from the host's live advert, mirroring Apple's
        // `discovery.hosts.first { host.matches($0) }?.macAddresses`.
        val suggested = discovered.firstOrNull { kh.matches(it) }?.mac ?: emptyList()
        val onSaveHost: (KnownHost) -> Unit = { updated ->
            knownHostStore.update(kh.address, kh.port, updated)
            savedHosts = knownHostStore.all()
            editTarget = null
        }
        if (gamepadUi) {
            // Console edit: the same field list + on-screen keyboard as Add-Host, seeded from the
            // host with an extra MAC row; the action SAVES instead of connecting.
            GamepadAddHostScreen(
                onAdd = { _, _, _ -> },
                onDismiss = { editTarget = null },
                editHost = kh,
                suggestedMacs = suggested,
                onSave = onSaveHost,
            )
        } else {
            EditHostDialog(
                target = kh,
                suggestedMacs = suggested,
                onSave = onSaveHost,
                onDismiss = { editTarget = null },
            )
        }
    }

    // Topmost: the "Waking…" overlay rides over both the touch grid and the console home.
    WakeOverlay(waker, gamepadUi)
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
