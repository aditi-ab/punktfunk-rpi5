package io.unom.punktfunk

import androidx.compose.runtime.Composable
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.PendingTrust

/**
 * Everything `ConnectScreen` puts ON TOP of whichever home it drew — the trust and pairing
 * ceremony, the parked "Waiting for approval…", the console's host options, the speed test, the
 * edit form, the local-network rationale, and finally the connect takeover.
 *
 * They live together because their ORDER is the contract: this is a stack of siblings in one tree,
 * so the last one drawn is the one on top, and [ConnectOverlay] is last on purpose — a dial can
 * start from any of the prompts above it, and its takeover has to cover the prompt that started it.
 *
 * Only the state each prompt reads comes in; every action goes back out as a callback, because they
 * all end in the connect/pair engine or in the host store, which the screen owns. Nothing in here
 * decides anything — it decides only what is visible.
 */
@Composable
internal fun ConnectPrompts(
    gamepadUi: Boolean,
    /** The client identity — the PIN ceremony needs it to run SPAKE2; null while it is still minting. */
    identity: ClientIdentity?,
    profiles: List<StreamProfile>,
    isOnline: (KnownHost) -> Boolean,
    // ---- trust / pairing --------------------------------------------------------------------
    pendingTrust: PendingTrust?,
    /** Dismiss (null) or re-aim the SAME decision at another kind — "Pair with PIN…" does that. */
    onPendingTrustChange: (PendingTrust?) -> Unit,
    /** Trust-on-first-use accepted: dial with no pin. Offered only for a `pair=optional` host. */
    onTrustNew: (PendingTrust) -> Unit,
    /** The PIN ceremony completed with this host fingerprint — save as paired, then dial. */
    onPaired: (PendingTrust, String) -> Unit,
    onRequestAccess: (PendingTrust) -> Unit,
    // ---- the parked no-PIN request ----------------------------------------------------------
    /** Non-null while a "request access" connect sits parked on the host awaiting approval. */
    awaitingHostName: String?,
    onCancelApproval: () -> Unit,
    // ---- console host options (Up on a saved carousel tile) ---------------------------------
    optionsTarget: HostCardEntry?,
    onDismissOptions: () -> Unit,
    libraryEnabled: Boolean,
    onOpenLibrary: (KnownHost) -> Unit,
    onWake: (KnownHost) -> Unit,
    onSpeedTest: (KnownHost) -> Unit,
    onCopyLink: (KnownHost, StreamProfile?) -> Unit,
    onEditHost: (KnownHost) -> Unit,
    onForgetHost: (KnownHost) -> Unit,
    onTogglePin: (KnownHost, StreamProfile) -> Unit,
    // ---- speed test --------------------------------------------------------------------------
    speedTest: HostCardEntry?,
    /** Which layer Apply writes to. Resolved by the caller (it holds the store); set with [speedTest]. */
    speedTestTarget: SpeedTestTarget?,
    speedTestPhase: SpeedTestPhase,
    /** true = write the measured bitrate to the profile, false = to the global default. */
    onApplySpeedTest: (Boolean) -> Unit,
    onDismissSpeedTest: () -> Unit,
    // ---- edit host ---------------------------------------------------------------------------
    editTarget: KnownHost?,
    /** A MAC from the live advert, for a host whose own is not learned yet. */
    editSuggestedMacs: List<String>,
    onSaveHost: (KnownHost) -> Unit,
    onDismissEdit: () -> Unit,
    // ---- local network permission ------------------------------------------------------------
    lnpPrompt: Boolean,
    onAllowLocalNetwork: () -> Unit,
    onOpenSystemSettings: () -> Unit,
    onDismissLnpPrompt: () -> Unit,
    // ---- the connect takeover ----------------------------------------------------------------
    connectingHostName: String?,
    waker: WakeController,
    onCancelConnect: () -> Unit,
) {
    pendingTrust?.let { pt ->
        // Same trust/pairing logic, console-styled + controller-navigable in gamepad mode.
        val onPair = { onPendingTrustChange(pt.copy(kind = PendingTrust.Kind.PAIR)) }
        // Three of the four say the same thing in both interfaces, so they are ONE prompt that
        // knows which one is running. Only the PIN ceremony genuinely differs — a keyboard field
        // against four D-pad digit slots is a different input model, not a different skin.
        when (pt.kind) {
            PendingTrust.Kind.TRUST_NEW -> TrustNewHostPrompt(
                gamepadUi, pt,
                onTrust = { onTrustNew(pt) },
                onPairInstead = onPair,
                onDismiss = { onPendingTrustChange(null) },
            )
            PendingTrust.Kind.FP_CHANGED ->
                FingerprintChangedPrompt(gamepadUi, pt, onPair) { onPendingTrustChange(null) }
            PendingTrust.Kind.REQUEST_ACCESS -> RequestAccessPrompt(
                gamepadUi, pt,
                onRequestAccess = { onRequestAccess(pt) },
                onUsePin = onPair,
                onDismiss = { onPendingTrustChange(null) },
            )
            PendingTrust.Kind.PAIR -> {
                val onSavePaired = { fp: String -> onPaired(pt, fp) }
                if (gamepadUi) {
                    GamepadPairPinDialog(pt, identity, onSavePaired) { onPendingTrustChange(null) }
                } else {
                    PairPinDialog(pt, identity, onSavePaired) { onPendingTrustChange(null) }
                }
            }
        }
    }

    awaitingHostName?.let { hostLabel ->
        AwaitingApprovalPrompt(gamepadUi, hostLabel = hostLabel, onCancel = onCancelApproval)
    }

    // Console host options (Up on a saved carousel tile): Wake / Edit / Forget.
    optionsTarget?.let { entry ->
        val kh = entry.host
        val pin = entry.pin
        val offline = !isOnline(kh)
        GamepadHostOptionsDialog(
            hostName = kh.name,
            canWake = kh.mac.isNotEmpty() && offline,
            onWake = { onDismissOptions(); onWake(kh) },
            // A saved host always has a library (it's a knownHost) → offer it when the setting's on,
            // so a TV remote reaches the library here instead of via the Y face button.
            onLibrary = if (libraryEnabled && pin == null) {
                { onDismissOptions(); onOpenLibrary(kh) }
            } else {
                null
            },
            onSpeedTest = if (pin == null) {
                { onDismissOptions(); onSpeedTest(kh) }
            } else {
                null
            },
            onCopyLink = { onDismissOptions(); onCopyLink(kh, pin) },
            onEdit = { onDismissOptions(); onEditHost(kh) },
            onForget = { onForgetHost(kh); onDismissOptions() },
            onDismiss = onDismissOptions,
            // A pin's only action: unpinning touches neither the host nor the profile.
            onUnpin = pin?.let { p -> { onTogglePin(kh, p); onDismissOptions() } },
            profileName = pin?.name,
        )
    }

    if (speedTest != null && speedTestTarget != null) {
        SpeedTestPrompt(
            gamepadUi, speedTest.host.name, speedTestTarget, speedTestPhase,
            onApplySpeedTest, onDismissSpeedTest,
        )
    }

    editTarget?.let { kh ->
        if (gamepadUi) {
            // Console edit: the same field list + on-screen keyboard as Add-Host, seeded from the
            // host with an extra MAC row; the action SAVES instead of connecting.
            GamepadAddHostScreen(
                onAdd = { _, _, _ -> },
                onDismiss = onDismissEdit,
                editHost = kh,
                suggestedMacs = editSuggestedMacs,
                onSave = onSaveHost,
                // Shared clipboard and the profile binding — the two host decisions that used to
                // exist only in the touch edit sheet, which a TV box has no way to reach.
                profiles = profiles,
            )
        } else {
            EditHostDialog(
                target = kh,
                suggestedMacs = editSuggestedMacs,
                profiles = profiles,
                onSave = onSaveHost,
                onDismiss = onDismissEdit,
            )
        }
    }

    if (lnpPrompt) {
        // Android 17+ local-network-permission rationale: re-request (a permanently-denied request
        // returns instantly without a system prompt — hence the settings deep link alongside).
        LocalNetworkPrompt(
            gamepadUi,
            onAllow = onAllowLocalNetwork,
            onSettings = onOpenSystemSettings,
            onDismiss = onDismissLnpPrompt,
        )
    }

    // Topmost: the full-screen connect takeover — instant "Connecting…" feedback on any dial, flowing
    // seamlessly into the "Waking…" wait if the host turns out to be asleep. Rides over both the touch
    // grid and the console home.
    ConnectOverlay(
        connectingHostName = connectingHostName,
        waker = waker,
        gamepadUi = gamepadUi,
        onCancelConnect = onCancelConnect,
    )
}
