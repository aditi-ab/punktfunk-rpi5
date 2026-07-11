package io.unom.punktfunk

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import io.unom.punktfunk.kit.NativeBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

/**
 * Wake a sleeping host and WAIT for it to come back before proceeding — the Android mirror of the
 * Apple client's `HostWaker`.
 *
 * A magic packet is fire-and-forget, and a cold box can take 20–60 s to POST, boot, and start
 * advertising on mDNS again — far longer than a connect attempt will sit. So instead of firing one
 * packet and immediately dialing (which just fails on a genuinely-asleep host), this drives a visible
 * "Waking…" state: it (re-)sends the packet, polls the host's mDNS presence once a second via
 * [isOnline], and on success runs [onOnline] (the real connect for a Wake-&-Connect, or nothing for
 * a wake-only); on timeout it parks in a retry/cancel state. One wake at a time.
 *
 * [scope] is the composition's coroutine scope (main-dispatched), so [waking] mutations and the
 * [isOnline]/[onOnline] callbacks all run on the main thread; only the blocking send is off-loaded.
 */
class WakeController(private val scope: CoroutineScope) {
    /** null = idle; non-null drives the "Waking…" phase of [ConnectOverlay]. */
    data class Waking(
        val hostName: String,
        /** Whether coming online chains into a connect (Wake & Connect) vs. just stopping. */
        val connectsAfter: Boolean,
        val seconds: Int = 0,
        val timedOut: Boolean = false,
    )

    var waking by mutableStateOf<Waking?>(null)
        private set

    private var loop: Job? = null

    /** Captured so "Try Again" replays the exact same wait. */
    private var replay: (() -> Unit)? = null

    /**
     * Wake the host and wait for [isOnline] to go true, then run [onOnline]. [macs]/[lastIp] target
     * the magic packet. No-ops straight to [onOnline] when there's nothing to wake with or the host
     * is already up (a race between the caller's check and here).
     */
    fun start(
        hostName: String,
        connectsAfter: Boolean,
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        onOnline: () -> Unit,
    ) {
        if (macs.isEmpty() || isOnline()) {
            cancel()
            onOnline()
            return
        }
        replay = { run(hostName, connectsAfter, macs, lastIp, isOnline, onOnline) }
        replay?.invoke()
    }

    /** Stop waiting and dismiss the overlay (B / Cancel). */
    fun cancel() {
        loop?.cancel()
        loop = null
        replay = null
        waking = null
    }

    /** Restart the wait after a timeout (A / Try Again). */
    fun retry() {
        replay?.invoke()
    }

    private fun run(
        hostName: String,
        connectsAfter: Boolean,
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        onOnline: () -> Unit,
    ) {
        loop?.cancel()
        waking = Waking(hostName = hostName, connectsAfter = connectsAfter)
        loop = scope.launch {
            var elapsed = 0
            while (isActive) {
                // Re-send periodically: a single packet can be missed, and some NICs only wake on a
                // fresh packet after dropping into a deeper sleep state.
                if (elapsed % RESEND_EVERY_S == 0) {
                    val csv = macs.joinToString(",")
                    launch(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(csv, lastIp) }
                }
                if (isOnline()) {
                    waking = null
                    loop = null
                    onOnline()
                    return@launch
                }
                if (elapsed >= TIMEOUT_S) {
                    waking = waking?.copy(timedOut = true)
                    loop = null
                    return@launch
                }
                delay(1000)
                elapsed++
                waking = waking?.copy(seconds = elapsed)
            }
        }
    }

    companion object {
        /** How long to wait for the host to reappear before giving up (a cold boot can be a minute+). */
        const val TIMEOUT_S = 90

        /** Re-send the magic packet this often. */
        const val RESEND_EVERY_S = 6
    }
}
