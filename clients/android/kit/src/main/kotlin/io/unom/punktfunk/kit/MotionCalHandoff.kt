package io.unom.punktfunk.kit

/**
 * The hand-off of one claim's motion calibration, from the thread that reads it off the pad to the
 * link thread that scales every input report with it.
 *
 * [DsCapture] reads a captured Sony pad's calibration feature report **off** the claiming thread —
 * it is a blocking EP0 control transfer and the claim runs on the UI's thread — so the value lands
 * a moment after the capture goes live. Two things have to hold across that gap, and a plain field
 * gives neither:
 *
 *  - **No report is ever scaled by the wrong pad's numbers.** [begin] forgets whatever the last
 *    capture published, so the link thread reads null — "no calibration yet", parse nothing — rather
 *    than inheriting the previous controller's scale factors, which are per unit and simply wrong
 *    for this one. It is also why the gap is a *drop* rather than a fallback: the fallback is what
 *    the read is about to replace, and a millisecond of unparsed reports costs nothing (they carry
 *    absolute state, and the next one is 1–4 ms behind).
 *  - **A read that outlived its claim publishes nothing.** An unplug, a [DsCapture.stop] and a fast
 *    re-claim can all land while a read is in flight; [publish] only accepts a value whose token is
 *    still the live claim's, so a straggler can never scale a pad it never read.
 *
 * Thread-safe: claimed and ended by the claiming thread, published by the reading thread, read by
 * the link thread.
 */
internal class MotionCalHandoff {
    /** Handed out by [begin] and burned by [end] — never reused, so a straggler can't match. */
    private var token = 0

    @Volatile private var cal: DsDevice.MotionCal? = null

    /** The live claim's calibration, or null while its read is still in flight. */
    val current: DsDevice.MotionCal? get() = cal

    /** Open a claim: forget the previous pad's calibration, and take this claim's token. */
    @Synchronized
    fun begin(): Int {
        cal = null
        return ++token
    }

    /** End the live claim. Nothing read under an older token can land after this. */
    @Synchronized
    fun end() {
        cal = null
        token++
    }

    /** Publish [value] if [claim] is still the live claim; returns whether it landed. */
    @Synchronized
    fun publish(claim: Int, value: DsDevice.MotionCal): Boolean {
        if (claim != token) return false
        cal = value
        return true
    }
}
