package io.unom.punktfunk.kit.discovery

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
     * Blocking. [send] is one magic packet; [now] and [sleep] are the clock in milliseconds.
     * [onTick] gets the status after every probe the wait was not [cancelled] across, the last
     * one included. True when the host came up.
     */
    fun run(
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
