package io.unom.punktfunk.kit.discovery

import android.os.SystemClock
import io.unom.punktfunk.kit.NativeBridge

/**
 * Wake a host and wait for it, for both Android shells (the desktop's `spawn_wake`).
 *
 * Once a second: ask [isOnline], report, and while the host stays silent send a magic packet
 * every [RESEND_EVERY_S] until [TIMEOUT_S]. Asked before sent, so a host that is already up never
 * gets a packet. [isOnline] is a probe ([Presence.probeSelf]), never an advert: a sleeping host
 * keeps its mDNS record warm.
 */
object WakeLoop {
    /** A cold boot can take a minute or more. */
    const val TIMEOUT_S = 90

    /** One packet can be missed, and some NICs wake only on a fresh one after a deeper sleep. */
    const val RESEND_EVERY_S = 6

    /**
     * Blocking. [onTick] gets the status after every probe the wait was not [cancelled] across,
     * the last one included. True when the host came up.
     */
    fun run(
        macs: List<String>,
        lastIp: String,
        isOnline: () -> Boolean,
        cancelled: () -> Boolean,
        onTick: (seconds: Int, timedOut: Boolean, online: Boolean) -> Unit,
    ): Boolean {
        val csv = macs.joinToString(",")
        return run(
            send = { NativeBridge.nativeWakeOnLan(csv, lastIp) },
            isOnline = isOnline,
            cancelled = cancelled,
            onTick = onTick,
            now = { SystemClock.elapsedRealtime() },
            sleep = { Thread.sleep(it) },
        )
    }

    /** [run] with its effects and clock passed in, so the cadence is testable off-device. */
    internal fun run(
        send: () -> Unit,
        isOnline: () -> Boolean,
        cancelled: () -> Boolean,
        onTick: (seconds: Int, timedOut: Boolean, online: Boolean) -> Unit,
        now: () -> Long,
        sleep: (Long) -> Unit,
    ): Boolean {
        // Wall-clock, not a lap count: a probe can take most of a second on its own.
        val started = now()
        var sentAt: Int? = null
        while (!cancelled()) {
            val online = isOnline()
            if (cancelled()) break
            val seconds = ((now() - started) / 1000).toInt()
            val timedOut = !online && seconds >= TIMEOUT_S
            onTick(seconds, timedOut, online)
            if (online || timedOut) return online
            if (sentAt == null || seconds - sentAt >= RESEND_EVERY_S) {
                send()
                sentAt = seconds
            }
            sleep(1000)
        }
        return false
    }
}
