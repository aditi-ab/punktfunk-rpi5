package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
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
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import io.unom.punktfunk.components.EmptyHostsState
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.HostDiscovery
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.link.LinkError
import io.unom.punktfunk.kit.link.LinkRoute
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.HostStatus
import io.unom.punktfunk.models.PendingTrust
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

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

/**
 * A plain dial in flight — [hostName] labels the unified [ConnectOverlay]'s "Connecting…" phase, and
 * [cancelled] lets its Cancel abort. The native connect is a blocking call with no abort, so Cancel
 * returns the UI immediately and a late-arriving handle is torn down silently rather than navigating
 * into a session the user already backed out of. Mirrors [RequestAccessState]'s late-result handling.
 */
private class ConnectAttempt(val hostName: String) {
    val cancelled = AtomicBoolean(false)
}

@Composable
fun ConnectScreen(
    settings: Settings,
    onConnected: (ActiveSession) -> Unit,
    // Writes the global defaults back. Only the speed test uses it — that is the one action on this
    // screen that can land in the defaults layer (design/client-settings-profiles.md §5.3).
    onSettingsChange: (Settings) -> Unit = {},
    // Console (gamepad) mode: render the host carousel instead of the touch grid, sharing all of this
    // screen's connect/trust/discovery logic. [onOpenSettings]/[onOpenLibrary] are the X/Y actions the
    // gamepad shell owns (the touch UI reaches Settings via the bottom bar and has no library button).
    gamepadUi: Boolean = false,
    onOpenSettings: () -> Unit = {},
    onOpenLibrary: (KnownHost) -> Unit = {},
    navGate: Boolean = true, // false while the console home is cross-fading out
    // A `punktfunk://` URL to route (design/client-deep-links.md §3). This screen owns it because
    // it owns the connect path — trust decisions, the local-network grant, wake-and-retry — and a
    // link must go through all of them, not around them.
    deepLink: String? = null,
    onDeepLinkHandled: () -> Unit = {},
) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current
    var host by remember { mutableStateOf("") }
    var hostName by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    // A confirmation, as opposed to [status]'s failures — "75 Mbit/s set in “Travel”". Separate
    // state because the two read completely differently: an error banner is red on purpose, and a
    // successful write dressed as one is a small lie every time it appears.
    var notice by remember { mutableStateOf<String?>(null) }
    // A plain dial in flight (drives the "Connecting…" phase of the full-screen ConnectOverlay); null
    // when idle or when the request-access / wake flows own the screen instead.
    var attempt by remember { mutableStateOf<ConnectAttempt?>(null) }
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
    // Android 17 Local Network Protection: with targetSdk 37, EVERYTHING this screen does — the mDNS
    // browse, the QUIC dial (UDP 9777), Wake-on-LAN, the library fetch — is blocked until the user
    // grants ACCESS_LOCAL_NETWORK (a runtime permission in the NEARBY_DEVICES group). Blocked UDP
    // fails with EPERM, which quinn experiences as a silent handshake timeout — so without this gate
    // a denial looks exactly like a dead host. Unlike NEARBY_WIFI_DEVICES below, this one is
    // load-bearing: request it on entry, and surface a denial as an actionable dialog/banner (with a
    // system-settings deep link) instead of dead-ending on timeouts.
    var lnpGranted by remember { mutableStateOf(hasLocalNetworkPermission(context)) }
    var lnpPrompt by remember { mutableStateOf(false) }
    val localNetLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted ->
        lnpGranted = granted
        if (granted) {
            lnpPrompt = false
            // The browse started while blocked (its sockets failed or received nothing) — restart it
            // now that the grant makes them work.
            discovery.stop()
            discovery.start()
        } else {
            lnpPrompt = true // rationale + "Open settings" (a permanently-denied request returns instantly)
        }
    }
    val nearbyLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { _ -> /* best-effort hint; discovery runs regardless of the result */ }
    LaunchedEffect(Unit) {
        if (!lnpGranted) {
            localNetLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
        } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && !hasNearbyPermission(context)) {
            // The old opportunistic multicast hedge (some OEMs filter multicast without it). On API
            // 37+ it shares the NEARBY_DEVICES group with ACCESS_LOCAL_NETWORK, so once that is
            // granted this auto-grants without a second prompt.
            nearbyLauncher.launch(Manifest.permission.NEARBY_WIFI_DEVICES)
        }
    }
    // Re-check on resume: our dialog deep-links to system settings, and granting there doesn't kill
    // or otherwise notify the app — this observer is what turns the grant into a live discovery.
    DisposableEffect(Unit) {
        val lifecycle = (context as? LifecycleOwner)?.lifecycle
        val obs = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME && !lnpGranted && hasLocalNetworkPermission(context)) {
                lnpGranted = true
                lnpPrompt = false
                discovery.stop()
                discovery.start()
            }
        }
        lifecycle?.addObserver(obs)
        onDispose { lifecycle?.removeObserver(obs) }
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
    // The settings-profile catalog. Read here (not in the settings screen's copy) because this is
    // where profiles are USED: to resolve what a tap connects with, to offer the one-offs, and to
    // render the pinned cards. Re-read on entry, since Settings may have changed it in between.
    val profileStore = remember { ProfileStore(context) }
    var profiles by remember { mutableStateOf(profileStore.all()) }
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
    // Saved hosts proven reachable by a QUIC probe this cycle, keyed "address:port" — the
    // routed-network (Tailscale/VPN) counterpart to mDNS presence, since such hosts never
    // advertise. OR'd into `isOnline` below so their pips light up. Probe only saved hosts NOT
    // already seen on mDNS, off the main thread, every ~12 s; gated on LNP (blocked UDP would
    // just time out). `rememberUpdatedState` keeps the 1 Hz mDNS updates from restarting the loop.
    var reachable by remember { mutableStateOf<Set<String>>(emptySet()) }
    val discoveredNow by rememberUpdatedState(discovered)
    LaunchedEffect(savedHosts, lnpGranted) {
        if (!lnpGranted) {
            reachable = emptySet()
            return@LaunchedEffect
        }
        while (true) {
            val targets = savedHosts.filter { kh -> discoveredNow.none { kh.matches(it) } }
            reachable = withContext(Dispatchers.IO) {
                targets
                    .filter { NativeBridge.nativeProbe(it.address, it.port, 3_000) }
                    .map { "${it.address}:${it.port}" }
                    .toSet()
            }
            delay(12_000)
        }
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
    var optionsTarget by remember { mutableStateOf<HostCardEntry?>(null) }

    // Discovered hosts not already saved — a saved host (paired or TOFU) belongs in "Saved hosts",
    // not also in "Discovered", so we hide the overlap (matched by fingerprint when both carry it, so
    // it survives a DHCP address change; else by address:port). Mirrors the Apple client.
    val discoveredUnsaved = discovered.filter { dh -> savedHosts.none { it.matches(dh) } }

    // Issue the native connect (shared by the normal connect and the request-access path). A plain
    // desktop connect (no library launch) — the library launcher calls [connectToHost] with an id.
    suspend fun connectNative(
        id: ClientIdentity,
        targetHost: String,
        targetPort: Int,
        pinHex: String,
        timeoutMs: Int,
        profile: StreamProfile?,
        launch: String?,
    ): Long = connectToHost(
        context, settings.effectiveFor(profile), id, targetHost, targetPort, pinHex,
        launch = launch, timeoutMs = timeoutMs,
    )

    // What the stream screen is handed: the settings this connect actually used, plus the HOST's
    // clipboard decision (a property of the record, not a global). A host we never saved — a
    // connect that failed to pin — falls back to the on default the setting always had.
    fun session(handle: Long, record: KnownHost?, profile: StreamProfile?) = ActiveSession(
        handle,
        settings.effectiveFor(profile),
        clipboardSync = record?.clipboardSync ?: true,
        profileName = profile?.name,
        hostId = record?.id,
    )

    // The actual dial (identity already ready). On a TOFU connect (pinHex null), pin the fingerprint
    // the host presented (as an unpaired known host) so the next connect goes straight through and it
    // appears in the saved-hosts list. [onFailure], when set, takes over a failed dial (the wake-wait
    // fallback) instead of the error status line — discovery is already restarted when it runs, so
    // the wait can observe the host reappear.
    fun doConnectDirect(
        targetHost: String,
        targetPort: Int,
        name: String,
        pinHex: String?,
        profile: StreamProfile?,
        launch: String? = null,
        onFailure: (() -> Unit)? = null,
    ) {
        val id = identity ?: run {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        val thisAttempt = ConnectAttempt(name)
        attempt = thisAttempt // shows the ConnectOverlay's "Connecting…" phase immediately
        connecting = true
        status = null
        notice = null
        discovery.stop() // free the Wi-Fi radio before the stream session
        scope.launch {
            val handle =
                connectNative(id, targetHost, targetPort, pinHex ?: "", CONNECT_TIMEOUT_MS, profile, launch)
            // Cancelled mid-dial: the UI's already been returned (and discovery restarted) by
            // cancelConnect — drop the just-opened session silently rather than navigating into it.
            if (thisAttempt.cancelled.get()) {
                if (handle != 0L) withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
                return@launch
            }
            attempt = null
            connecting = false
            if (handle != 0L) {
                var record = knownHostStore.get(targetHost, targetPort)
                if (pinHex == null) { // TOFU: pin what we observed (unpaired)
                    val fp = NativeBridge.nativeHostFingerprint(handle)
                    if (fp.isNotEmpty()) {
                        record = knownHostStore.trust(targetHost, targetPort, name, fp, paired = false)
                    }
                }
                onConnected(session(handle, record, profile))
            } else {
                discovery.start()
                val token = NativeBridge.nativeTakeLastError()
                val unreachable = token == "timeout" || token == "io" || token.isEmpty()
                if (onFailure != null && unreachable) {
                    // Unreachable — hand off to the wake-and-wait flow — clearing `attempt` above
                    // and setting `waker.waking` here land in one recompose, so the overlay slides
                    // Connecting → Waking without a blank frame.
                    onFailure()
                } else {
                    // A typed host rejection (busy / versions differ / pairing required) means the
                    // host is awake — waking it would be nonsense; show the stated reason instead.
                    status = ConnectErrors.connectMessage(token, requestAccess = false)
                }
            }
        }
    }

    // Cancel a plain dial in flight (the overlay's "Connecting…" phase, B / Cancel). The native
    // connect can't be aborted, so flag this attempt (a late handle is closed silently in
    // doConnectDirect) and return the UI now, resuming the discovery we paused for the dial.
    fun cancelConnect() {
        attempt?.cancelled?.set(true)
        attempt = null
        connecting = false
        discovery.start()
    }

    // Wake-aware connect. If auto-wake is on (Settings.autoWakeEnabled) and the target is a saved
    // host with a learned MAC that ISN'T currently advertising, fire a wake packet and DIAL
    // IMMEDIATELY — mDNS absence does NOT mean unreachable (a host reached over a routed network —
    // Tailscale/VPN/another subnet — is mDNS-blind forever, and gating the dial on presence bricked
    // exactly those reconnects). A genuinely-asleep box is already booting while the dial times out;
    // only a FAILED dial falls into the wake-and-WAIT-for-mDNS flow (WakeController's "Waking…"
    // overlay), which redials once the host reappears. Otherwise (auto-wake off, no MAC, or already
    // seen live) dial straight through.
    fun doConnect(
        targetHost: String,
        targetPort: Int,
        name: String,
        pinHex: String?,
        oneOffProfile: String?,
        launch: String? = null,
    ) {
        if (identity == null) {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        val kh = knownHostStore.get(targetHost, targetPort)
        // Latched here, not per dial attempt: a wake-and-redial must stream with the same profile
        // the user asked for, and the "applies from the next session" footers stay truthful.
        val profile = profileStore.resolveFor(kh, oneOffProfile)
        val macs = kh?.mac ?: emptyList()
        // "Up" = a live advert that is THIS host — matched by fingerprint first (so it survives a DHCP
        // address change on a cold boot), else by address:port. Returns the CURRENT advert so we can
        // dial its live address rather than the stale saved one.
        fun liveAdvert(): DiscoveredHost? =
            if (kh != null) discovered.firstOrNull { kh.matches(it) }
            else discovered.firstOrNull { it.host == targetHost && it.port == targetPort }
        if (settings.autoWakeEnabled && macs.isNotEmpty() && liveAdvert() == null) {
            // Fire-and-forget first packet (harmless if it's awake), then dial-first.
            scope.launch(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs.joinToString(","), targetHost) }
            doConnectDirect(targetHost, targetPort, name, pinHex, profile, launch, onFailure = {
                waker.start(
                    hostName = name,
                    connectsAfter = true,
                    macs = macs,
                    lastIp = targetHost,
                    isOnline = { liveAdvert() != null },
                    onOnline = {
                        val live = liveAdvert()
                        // Woke back on a new address? Re-key the saved record so it (and future
                        // connects) point at the live one, then dial there (no fallback on this
                        // redial — a second failure surfaces as the plain error).
                        if (live != null && kh != null && (live.host != kh.address || live.port != kh.port)) {
                            knownHostStore.save(kh.copy(address = live.host, port = live.port))
                            savedHosts = knownHostStore.all()
                        }
                        doConnectDirect(
                            live?.host ?: targetHost, live?.port ?: targetPort, name, pinHex,
                            profile, launch,
                        )
                    },
                )
            })
        } else {
            doConnectDirect(targetHost, targetPort, name, pinHex, profile, launch)
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
            // A host being trusted for the first time can't have a binding yet, so this is always
            // the plain defaults — a profile only ever enters via a later, deliberate choice.
            val handle = connectNative(
                id, target.host, target.port, pinHex, REQUEST_ACCESS_TIMEOUT_MS,
                profile = null, launch = target.launch,
            )
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
                var record = knownHostStore.get(target.host, target.port)
                if (fp.isNotEmpty()) {
                    record = knownHostStore.trust(target.host, target.port, target.name, fp, paired = true)
                    savedHosts = knownHostStore.all()
                }
                onConnected(session(handle, record, profile = null))
            } else {
                // Cause-specific: an operator denial, an approval timeout, and a request that
                // never reached the host are different problems with different fixes.
                status = ConnectErrors.connectMessage(
                    NativeBridge.nativeTakeLastError(),
                    requestAccess = true,
                )
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
        // A one-off "Connect with ▸" pick. `null` = follow the host's binding (a plain tap);
        // `""` = force the global defaults, which is a real choice on a bound host and must
        // therefore survive as a value rather than collapsing into "unset". NEVER rebinds.
        oneOffProfile: String? = null,
        // A library id the host should boot straight into (`launch=` on a link).
        launch: String? = null,
    ) {
        // Every dial/pair path funnels through here — with local network access denied the connect
        // can only EPERM its way to a 10 s timeout, so ask instead of pretending to try.
        if (!lnpGranted) {
            lnpPrompt = true
            return
        }
        val known = knownHostStore.get(targetHost, targetPort)
        val adv = dh?.fingerprint?.lowercase()
        // Label precedence: a saved host keeps its (possibly user-renamed) name; else the discovered
        // mDNS name; else the name typed in the Add-host sheet; else the bare address.
        val name = known?.name ?: dh?.name ?: manualName?.trim()?.takeIf { it.isNotEmpty() } ?: targetHost
        when {
            // Known host whose advertised fp still matches the pin → silent pinned reconnect.
            known != null && (adv == null || adv == known.fpHex) ->
                doConnect(targetHost, targetPort, known.name, known.fpHex, oneOffProfile, launch)
            // Known host whose fp changed → force re-pairing (no silent re-trust shortcut).
            known != null -> pendingTrust = PendingTrust(
                targetHost, targetPort, known.name, adv, PendingTrust.Kind.FP_CHANGED,
                oneOffProfile, launch,
            )
            // Host explicitly advertised pair=optional → trust-on-first-use is permitted (offer it,
            // clearly labeled, alongside PIN pairing). Smart-cast: this branch ⇒ dh != null.
            dh?.pairingRequired == false -> pendingTrust = PendingTrust(
                targetHost, targetPort, name, dh.fingerprint, PendingTrust.Kind.TRUST_NEW,
                oneOffProfile, launch,
            )
            // pair=required, or a manual/unknown-policy host → offer the two ways in: a no-PIN
            // "request access" (approve in the console) or the SPAKE2 PIN ceremony.
            else -> pendingTrust = PendingTrust(
                targetHost, targetPort, name, adv, PendingTrust.Kind.REQUEST_ACCESS,
                oneOffProfile, launch,
            )
        }
    }

    // A speed test in flight: which host+profile it is measuring, and how far it has got. The
    // measurement is over a real connect, so it takes the same `connecting` gate every dial does.
    var speedTest by remember { mutableStateOf<HostCardEntry?>(null) }
    var speedTestPhase by remember { mutableStateOf<SpeedTestPhase>(SpeedTestPhase.Connecting) }

    fun startSpeedTest(entry: HostCardEntry) {
        val id = identity ?: run {
            status = "Identity not ready yet — try again in a moment"
            return
        }
        // The magic packet isn't the only thing LNP blocks: without the grant this would EPERM its
        // way to a timeout and report a dead link on a perfectly good one.
        if (!lnpGranted) {
            lnpPrompt = true
            return
        }
        speedTest = entry
        speedTestPhase = SpeedTestPhase.Connecting
        notice = null
        connecting = true
        discovery.stop() // a browse running through the burst would measure itself
        scope.launch {
            runSpeedTest(context, id, entry.host.address, entry.host.port, entry.host.fpHex) { p ->
                // A dismissed dialog abandons the run; don't drag it back onto the screen.
                if (speedTest != null) speedTestPhase = p
            }
            connecting = false
            discovery.start()
        }
    }

    // Toggle a host+profile pin. Presentation only: it never touches the profile itself and never
    // changes the host's default binding.
    fun togglePin(kh: KnownHost, profile: StreamProfile) {
        val pins = if (profile.id in kh.pinnedProfileIds) {
            kh.pinnedProfileIds - profile.id
        } else {
            kh.pinnedProfileIds + profile.id
        }
        knownHostStore.save(kh.copy(pinnedProfileIds = pins))
        savedHosts = knownHostStore.all()
    }

    // The profile rows a card's overflow menu grows. With no profiles at all it stays empty — a
    // user who never wants this feature sees no new clutter anywhere but the settings scope chips.
    // "Connect with" is a ONE-OFF on every card: it never rebinds the host, which is why rebinding
    // lives in the Edit sheet instead.
    fun hostMenu(kh: KnownHost, pin: StreamProfile?): List<HostMenuItem> = buildList {
        if (pin == null) {
            add(HostMenuItem("Network speed test") { startSpeedTest(HostCardEntry(kh, null)) })
        }
        if (profiles.isEmpty()) return@buildList
        if (pin != null) {
            add(HostMenuItem("Unpin card", startsSection = true) { togglePin(kh, pin) })
        }
        add(
            HostMenuItem("Connect with: Default settings", startsSection = true) {
                // The empty reference is "force the defaults", not "unset" — on a bound host that
                // is a real, different action from a plain tap.
                connect(kh.address, kh.port, oneOffProfile = "")
            },
        )
        profiles.forEach { p ->
            add(HostMenuItem("Connect with: ${p.name}") { connect(kh.address, kh.port, oneOffProfile = p.id) })
        }
        if (pin == null) {
            profiles.forEachIndexed { i, p ->
                val pinned = p.id in kh.pinnedProfileIds
                add(
                    HostMenuItem(
                        if (pinned) "Unpin card: ${p.name}" else "Pin as card: ${p.name}",
                        startsSection = i == 0,
                    ) { togglePin(kh, p) },
                )
            }
        }
    }

    // The saved-hosts grid: each host's own card, then one card per profile it has pinned, so a
    // pinned combination is a plain one-click connect instead of a trip through a menu.
    val savedCards = savedHosts.flatMap { kh ->
        listOf(HostCardEntry(kh, null)) + profileStore.pinsFor(kh).map { HostCardEntry(kh, it) }
    }

    // ---- punktfunk:// routing (design/client-deep-links.md §3) --------------------------------
    //
    // The invariant: a URL may only ever do what a click on an existing card could do, MINUS trust
    // decisions. So it never pairs, never trusts on its own, and carries references rather than
    // values. Everything below is either "do exactly what the card does" or "refuse and say why" —
    // a shortcut that can't honour its reference must say so, because streaming with the wrong
    // settings is worse than an explanatory notice.
    LaunchedEffect(deepLink, identity, savedHosts) {
        val url = deepLink ?: return@LaunchedEffect
        // Wait for the identity rather than refusing: it arrives a beat after first composition and
        // the effect re-runs when it does.
        if (identity == null) return@LaunchedEffect
        onDeepLinkHandled()
        val parsed = DeepLinks.parse(url)
        if (parsed is DeepLinkResult.Refused) {
            // A link for someone else's scheme is not our business to complain about.
            if (parsed.error != LinkError.NOT_OUR_SCHEME) status = parsed.message()
            return@LaunchedEffect
        }
        val link = (parsed as DeepLinkResult.Parsed).link
        if (link.route != LinkRoute.CONNECT) {
            // `wake` and `browse` are reserved in the grammar and parse today; a front-end that
            // hasn't implemented them refuses with a notice rather than silently connecting.
            status = "Punktfunk on Android can't do “${link.route.word}” links yet."
            return@LaunchedEffect
        }
        // A profile reference that can't be honoured refuses: a "Work" shortcut streaming with the
        // wrong settings is worse than an error naming what failed.
        val profileRef = link.profile
        if (profileRef != null) {
            val (_, resolution) = profileStore.resolve(profileRef)
            if (resolution != ProfileResolution.FOUND) {
                status = if (resolution == ProfileResolution.AMBIGUOUS) {
                    "More than one profile is called “$profileRef” — rename one and try again."
                } else {
                    "That link asks for a profile called “$profileRef”, which isn't on this device."
                }
                return@LaunchedEffect
            }
        }
        when (val resolved = DeepLinks.resolveHost(link, savedHosts)) {
            // Known AND pinned is the one-click contract: do exactly what tapping its card does.
            is HostResolution.Known -> {
                // A pin that contradicts the stored one is the link being stale or lying. Hard
                // refusal: this is the one case where doing what the card does would be wrong.
                if (link.pinConflict(resolved.host)) {
                    status = "That link's fingerprint doesn't match the one pinned for " +
                        "${resolved.host.name} — it's out of date, or it isn't that host."
                    return@LaunchedEffect
                }
                if (resolved.host.fpHex.isEmpty()) {
                    // Saved but never pinned (nothing writes such a record today, but the rule is
                    // absolute): a link may not establish trust, so this is a confirmation.
                    pendingTrust = PendingTrust(
                        resolved.host.address, resolved.host.port, resolved.host.name,
                        link.fp, PendingTrust.Kind.REQUEST_ACCESS, profileRef, link.launch,
                    )
                    return@LaunchedEffect
                }
                connect(
                    resolved.host.address, resolved.host.port,
                    oneOffProfile = profileRef, launch = link.launch,
                )
            }
            // Unknown, or known only by address: the confirmation sheet, from which the normal
            // pairing flow proceeds under the user's eyes. Never a silent trust.
            is HostResolution.Unknown -> pendingTrust = PendingTrust(
                resolved.address,
                resolved.port,
                link.name ?: resolved.address,
                resolved.fp,
                PendingTrust.Kind.REQUEST_ACCESS,
                profileRef,
                link.launch,
            )
            HostResolution.Ambiguous ->
                status = "More than one saved host is called “${link.hostRef}” — " +
                    "rename one, or use its address."
            HostResolution.Unresolvable ->
                status = "That link points at a host this device doesn't know."
        }
    }

    var showManualSheet by remember { mutableStateOf(false) }

    if (gamepadUi) {
        // Console mode: the host carousel (saved → discovered → Add Host), driven by the pad. Shares
        // every action above; the trailing Add Host tile opens the same manual-entry sheet.
        val tiles = buildList {
            savedHosts.forEach { kh ->
                val bound = kh.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
                add(
                    HomeTile(
                        id = "saved-${kh.id}",
                        title = kh.name,
                        // The binding is what a press will actually do, so the tile says so — the
                        // console can't edit profiles, but it must never lie about which one it uses.
                        subtitle = bound?.let { "${kh.address}:${kh.port} · ${it.name}" }
                            ?: "${kh.address}:${kh.port}",
                        filled = true,
                        online = kh.isOnline(discovered, reachable),
                        paired = kh.paired,
                        knownHost = kh,
                        activate = { connect(kh.address, kh.port) },
                    ),
                )
                // Pinned host+profile combinations, right after their host: one focus-and-press
                // each, which is the affordance a controller surface does well (menus are not).
                profileStore.pinsFor(kh).forEach { p ->
                    add(
                        HomeTile(
                            id = "pin-${kh.id}-${p.id}",
                            title = kh.name,
                            subtitle = p.name,
                            filled = true,
                            online = kh.isOnline(discovered, reachable),
                            paired = kh.paired,
                            knownHost = kh,
                            pinnedProfileId = p.id,
                            activate = { connect(kh.address, kh.port, oneOffProfile = p.id) },
                        ),
                    )
                }
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
                awaiting == null && editTarget == null && optionsTarget == null &&
                waker.waking == null && !lnpPrompt,
            onActivate = { it.activate() },
            onOpenLibrary = { it.knownHost?.let(onOpenLibrary) },
            onOpenSettings = onOpenSettings,
            onOptions = { tile ->
                tile.knownHost?.let { kh ->
                    optionsTarget = HostCardEntry(kh, tile.pinnedProfileId?.let(profileStore::byId))
                }
            },
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

                    notice?.let {
                        Surface(
                            color = MaterialTheme.colorScheme.secondaryContainer,
                            shape = MaterialTheme.shapes.medium,
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSecondaryContainer,
                                textAlign = TextAlign.Center,
                                modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                            )
                        }
                        Spacer(Modifier.height(16.dp))
                    }

                    status?.let {
                        // In-flight progress (connecting / waking) is the full-screen ConnectOverlay's
                        // job now, so `status` only ever carries a result/error here — a filled error
                        // container reads as a real failure banner, not just red text lost in the layout.
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
                        Spacer(Modifier.height(16.dp))
                    }
                }
            }

            if (!lnpGranted) {
                // Local network access denied: discovery can't ever find anything and every connect
                // would time out — say so at the top, with the fix one tap away, instead of letting
                // the screen look idle/broken.
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Surface(
                        color = MaterialTheme.colorScheme.errorContainer,
                        shape = MaterialTheme.shapes.medium,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Column(
                            Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                            horizontalAlignment = Alignment.CenterHorizontally,
                        ) {
                            Text(
                                "Local network access is off",
                                style = MaterialTheme.typography.titleSmall,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                            )
                            Text(
                                "Android blocks punktfunk from finding or reaching hosts until you allow it.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                                textAlign = TextAlign.Center,
                            )
                            TextButton(onClick = { lnpPrompt = true }) { Text("Allow…") }
                        }
                    }
                    Spacer(Modifier.height(12.dp))
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
                items(savedCards, key = { it.key }) { entry ->
                    val kh = entry.host
                    val pin = entry.pin
                    val bound = kh.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
                    HostCard(
                        name = kh.name,
                        address = "${kh.address}:${kh.port}",
                        status = if (kh.paired) HostStatus.PAIRED else HostStatus.TOFU,
                        online = kh.isOnline(discovered, reachable),
                        enabled = !connecting,
                        // A pinned card connects with ITS profile; the host's own card follows the
                        // binding, which is exactly what its chip says it will do.
                        onConnect = {
                            if (pin != null) {
                                connect(kh.address, kh.port, oneOffProfile = pin.id)
                            } else {
                                connect(kh.address, kh.port)
                            }
                        },
                        // Edit / Forget / Wake live on the host's own card only: a pinned card is a
                        // shortcut, not a second host, and offering destructive host actions on it
                        // would blur exactly that.
                        onForget = if (pin != null) {
                            null
                        } else {
                            {
                                knownHostStore.remove(kh)
                                savedHosts = knownHostStore.all()
                            }
                        },
                        onEdit = if (pin != null) null else ({ editTarget = kh }),
                        // Explicit wake-only: offered when the host is offline and we have a MAC. Runs
                        // through the WakeController so it shows the "Waking…" overlay and waits for
                        // the host to come online (matched by fingerprint, so a new DHCP address on a
                        // cold boot still counts as "up") rather than firing a single silent packet.
                        onWake = if (pin == null && kh.mac.isNotEmpty() && !kh.isOnline(discovered, reachable)) {
                            {
                                // The magic packet is UDP broadcast — LNP-blocked like everything else.
                                if (!lnpGranted) {
                                    lnpPrompt = true
                                } else {
                                    waker.start(
                                        hostName = kh.name,
                                        connectsAfter = false,
                                        macs = kh.mac,
                                        lastIp = kh.address,
                                        isOnline = { discovered.any { kh.matches(it) } },
                                        onOnline = {},
                                    )
                                }
                            }
                        } else {
                            null
                        },
                        profileLabel = pin?.name ?: bound?.name,
                        profileProminent = pin != null,
                        accent = accentColor(pin?.accent ?: bound?.accent),
                        menuItems = hostMenu(kh, pin),
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
                        online = true, // in the discovered list ⇒ live on mDNS right now
                        enabled = !connecting,
                        onConnect = { connect(dh.host, dh.port, dh) },
                        onForget = null,
                    )
                }
            }

            // Active-discovery hint: discovery runs whenever this screen is up, so while it's
            // scanning but nothing's turned up yet (and we're not mid-connect), show it's working
            // rather than looking idle/empty. Suppressed while local network access is denied —
            // a spinner would be a lie there (the browse can't receive anything); the banner above
            // owns that state.
            if (lnpGranted && !connecting && discovered.isEmpty()) {
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
            knownHostStore.trust(pt.host, pt.port, pt.name, fp, paired = true)
            savedHosts = knownHostStore.all()
            pendingTrust = null
            doConnect(pt.host, pt.port, pt.name, fp, pt.profile, pt.launch)
        }
        when (pt.kind) {
            PendingTrust.Kind.TRUST_NEW ->
                if (gamepadUi) GamepadTrustNewDialog(pt, { pendingTrust = null; doConnect(pt.host, pt.port, pt.name, null, pt.profile, pt.launch) }, onPair, { pendingTrust = null })
                else TrustNewHostDialog(pt, { pendingTrust = null; doConnect(pt.host, pt.port, pt.name, null, pt.profile, pt.launch) }, onPair, { pendingTrust = null })
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
    optionsTarget?.let { entry ->
        val kh = entry.host
        val pin = entry.pin
        val offline = !kh.isOnline(discovered, reachable)
        GamepadHostOptionsDialog(
            hostName = kh.name,
            canWake = kh.mac.isNotEmpty() && offline,
            onWake = {
                optionsTarget = null
                // The magic packet is UDP broadcast — LNP-blocked like everything else.
                if (!lnpGranted) {
                    lnpPrompt = true
                } else {
                    waker.start(
                        hostName = kh.name, connectsAfter = false, macs = kh.mac, lastIp = kh.address,
                        isOnline = { discovered.any { kh.matches(it) } },
                        onOnline = {},
                    )
                }
            },
            // A saved host always has a library (it's a knownHost) → offer it when the setting's on,
            // so a TV remote reaches the library here instead of via the Y face button.
            onLibrary = if (settings.libraryEnabled && pin == null) {
                { optionsTarget = null; onOpenLibrary(kh) }
            } else {
                null
            },
            onSpeedTest = if (pin == null) {
                { optionsTarget = null; startSpeedTest(HostCardEntry(kh, null)) }
            } else {
                null
            },
            onEdit = { optionsTarget = null; editTarget = kh },
            onForget = {
                knownHostStore.remove(kh)
                savedHosts = knownHostStore.all()
                optionsTarget = null
            },
            onDismiss = { optionsTarget = null },
            // A pin's only action: unpinning touches neither the host nor the profile.
            onUnpin = pin?.let { p -> { togglePin(kh, p); optionsTarget = null } },
            profileName = pin?.name,
        )
    }

    speedTest?.let { entry ->
        val target = SpeedTestTarget.resolve(entry.host, entry.pin?.id, profileStore)
        val dismiss = { speedTest = null }
        val apply: (Boolean) -> Unit = { toProfile ->
            val done = speedTestPhase as? SpeedTestPhase.Done
            if (done != null) {
                val where = applySpeedTestResult(
                    done.recommendedKbps, target, toProfile, profileStore, settings, onSettingsChange,
                )
                profiles = profileStore.all()
                notice = "%.0f Mbit/s set in %s".format(done.recommendedMbps, where)
            }
            speedTest = null
        }
        if (gamepadUi) {
            GamepadSpeedTestDialog(entry.host.name, target, speedTestPhase, apply, dismiss)
        } else {
            SpeedTestDialog(entry.host.name, target, speedTestPhase, apply, dismiss)
        }
    }

    editTarget?.let { kh ->
        // Prefill a not-yet-learned MAC from the host's live advert, mirroring Apple's
        // `discovery.hosts.first { host.matches($0) }?.macAddresses`.
        val suggested = discovered.firstOrNull { kh.matches(it) }?.mac ?: emptyList()
        val onSaveHost: (KnownHost) -> Unit = { updated ->
            knownHostStore.save(updated)
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
                profiles = profiles,
                onSave = onSaveHost,
                onDismiss = { editTarget = null },
            )
        }
    }

    if (lnpPrompt) {
        // Android 17+ local-network-permission rationale: re-request (a permanently-denied request
        // returns instantly without a system prompt — hence the settings deep link alongside).
        val onAllow = {
            lnpPrompt = false
            localNetLauncher.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
        }
        val onSettings = {
            lnpPrompt = false
            context.startActivity(
                Intent(
                    android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", context.packageName, null),
                ),
            )
        }
        if (gamepadUi) {
            GamepadLocalNetworkDialog(onAllow = onAllow, onSettings = onSettings, onDismiss = { lnpPrompt = false })
        } else {
            LocalNetworkDialog(onAllow = onAllow, onSettings = onSettings, onDismiss = { lnpPrompt = false })
        }
    }

    // Topmost: the full-screen connect takeover — instant "Connecting…" feedback on any dial, flowing
    // seamlessly into the "Waking…" wait if the host turns out to be asleep. Rides over both the touch
    // grid and the console home.
    ConnectOverlay(
        connectingHostName = attempt?.hostName,
        waker = waker,
        gamepadUi = gamepadUi,
        onCancelConnect = { cancelConnect() },
    )
}

/**
 * One entry in the saved-hosts grid: a host's own card ([pin] null), or one of its pinned
 * host+profile cards. Pins are additive presentation state on the host record — never duplicated
 * host entries, which would fork pairing, trust and renames (design §5.2a).
 */
private data class HostCardEntry(val host: KnownHost, val pin: StreamProfile?) {
    val key: String get() = "card-${host.id}-${pin?.id ?: "primary"}"
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
 * Whether ACCESS_LOCAL_NETWORK is held (API 37+; below, the permission doesn't exist and local
 * network access is implicit). Android 17's Local Network Protection blocks ALL local-network
 * traffic for apps targeting SDK 37 without this runtime grant: UDP sends fail with EPERM, so the
 * QUIC dial surfaces as a silent handshake timeout and the mDNS browse receives nothing. Unlike
 * [hasNearbyPermission] this is load-bearing — nothing on the connect screen works without it.
 */
fun hasLocalNetworkPermission(context: Context): Boolean =
    Build.VERSION.SDK_INT < Build.VERSION_CODES.CINNAMON_BUN ||
        ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_LOCAL_NETWORK) ==
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

/**
 * True when a saved host is reachable RIGHT NOW: advertising on mDNS OR answering the QUIC probe
 * (a host reached over a routed network — Tailscale/VPN — never advertises but is reachable). The
 * display-side companion to dial-first: presence no longer means "on this LAN".
 */
private fun KnownHost.isOnline(discovered: List<DiscoveredHost>, reachable: Set<String>): Boolean =
    discovered.any { matches(it) } || reachable.contains("$address:$port")
