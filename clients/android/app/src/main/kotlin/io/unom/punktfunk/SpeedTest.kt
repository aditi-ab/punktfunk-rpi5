package io.unom.punktfunk

import android.content.Context
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

/**
 * The network speed test: measure the path to a host **over the real data plane** — connect, ask
 * the host to burst filler for two seconds, report goodput and loss, and offer to apply a
 * recommended bitrate in one tap.
 *
 * The measurement is the easy half. The half that was wrong everywhere for a long time is *where
 * the answer goes*: a measured bitrate belongs in the layer the tested host actually resolves
 * bitrate from (design/client-settings-profiles.md §5.3). Writing it to the global — the
 * long-standing behaviour — meant measuring the slow retro box downstairs quietly re-tuned the
 * desktop too. [SpeedTestTarget] is that decision, and because it depends only on the host it is
 * known *before* the result lands, so the button can say where it will write.
 */
sealed interface SpeedTestTarget {
    /** No profile in play — the global default, i.e. what has always happened. */
    data object Global : SpeedTestTarget

    /** The profile this host uses already overrides bitrate, so that override is what it reads. */
    data class Profile(val profile: StreamProfile) : SpeedTestTarget

    /**
     * The host uses a profile, but that profile inherits bitrate. Writing either layer is
     * defensible, so the user gets both buttons rather than us guessing which they meant.
     */
    data class Ask(val profile: StreamProfile) : SpeedTestTarget

    companion object {
        /**
         * Resolved exactly the way a connect resolves it (see [ProfileStore.resolveFor]): the
         * one-off pick this test was started from — a pinned card carries one — else the host's
         * binding. A dangling binding resolves as no profile here too.
         */
        fun resolve(
            host: KnownHost?,
            oneOffProfile: String?,
            profiles: ProfileStore,
        ): SpeedTestTarget {
            val profile = profiles.resolveFor(host, oneOffProfile) ?: return Global
            return if (profile.overrides.bitrateKbps != null) Profile(profile) else Ask(profile)
        }
    }
}

/** Where the speed test is: it connects, it measures, then it has an answer or a reason. */
sealed interface SpeedTestPhase {
    data object Connecting : SpeedTestPhase
    data object Measuring : SpeedTestPhase
    data class Failed(val message: String) : SpeedTestPhase

    /**
     * [recommendedKbps] is 70 % of the measured throughput — headroom for the FEC overhead and for
     * the loss a real stream will meet, the same margin the desktop clients apply.
     */
    data class Done(
        val throughputKbps: Int,
        val lossPct: Double,
        val recommendedKbps: Int,
    ) : SpeedTestPhase {
        val measuredMbps: Double get() = throughputKbps / 1000.0
        val recommendedMbps: Double get() = recommendedKbps / 1000.0
    }
}

/**
 * Connect to [host]:[port], run one burst, and report. Blocking-ish (it suspends on IO) — call
 * from a coroutine; [onPhase] is invoked as it progresses so the dialog can narrate.
 *
 * The connect is deliberately minimal: 1280×720@60, no launch, host-default bitrate. Nothing here
 * presents a frame, and asking a host to spin up a 4K encode for a three-second measurement would
 * be rude to it and slower for us.
 */
suspend fun runSpeedTest(
    context: Context,
    identity: ClientIdentity,
    host: String,
    port: Int,
    pinHex: String,
    onPhase: (SpeedTestPhase) -> Unit,
) {
    onPhase(SpeedTestPhase.Connecting)
    val probeSettings = Settings(
        width = 1280,
        height = 720,
        hz = 60,
        bitrateKbps = 0, // the host's default: this measures the link, not an encoder setting
        hdrEnabled = false,
        audioChannels = 2,
    )
    val handle = connectToHost(
        context, probeSettings, identity, host, port, pinHex,
        launch = null, timeoutMs = SPEED_TEST_CONNECT_TIMEOUT_MS,
    )
    if (handle == 0L) {
        onPhase(
            SpeedTestPhase.Failed(
                ConnectErrors.connectMessage(NativeBridge.nativeTakeLastError(), requestAccess = false),
            ),
        )
        return
    }
    try {
        onPhase(SpeedTestPhase.Measuring)
        if (!NativeBridge.nativeSpeedTest(handle, TARGET_KBPS, BURST_MS)) {
            onPhase(SpeedTestPhase.Failed("The host wouldn't start a measurement."))
            return
        }
        var waited = 0
        while (waited < POLL_BUDGET_MS) {
            delay(POLL_INTERVAL_MS.toLong())
            waited += POLL_INTERVAL_MS
            val r = NativeBridge.nativeProbeResult(handle)
            if (r == null || r.size < 3) {
                onPhase(SpeedTestPhase.Failed("The session ended before the measurement finished."))
                return
            }
            if (r[0] == 0.0) continue
            // Let the last UDP shards land before tearing the session down, or the tail of the
            // burst is counted as loss that never happened.
            delay(SETTLE_MS)
            val settled = NativeBridge.nativeProbeResult(handle) ?: r
            val kbps = settled[1].toInt()
            onPhase(
                SpeedTestPhase.Done(
                    throughputKbps = kbps,
                    lossPct = settled[2],
                    // Integer arithmetic in this order (not `* 0.7`) so the recommendation matches
                    // the desktop clients' to the kilobit.
                    recommendedKbps = kbps / 10 * 7,
                ),
            )
            return
        }
        onPhase(SpeedTestPhase.Failed("The measurement timed out."))
    } finally {
        withContext(Dispatchers.IO) { NativeBridge.nativeClose(handle) }
    }
}

/**
 * Write a measured bitrate into the layer [target] names. [toProfile] picks the side of a
 * [SpeedTestTarget.Ask]; it is ignored for the other targets, which have only one answer. Returns
 * a human phrase naming where it went, for the confirmation.
 */
fun applySpeedTestResult(
    kbps: Int,
    target: SpeedTestTarget,
    toProfile: Boolean,
    profiles: ProfileStore,
    settings: Settings,
    onGlobalChange: (Settings) -> Unit,
): String {
    val profile = when (target) {
        is SpeedTestTarget.Profile -> target.profile
        is SpeedTestTarget.Ask -> target.profile.takeIf { toProfile }
        SpeedTestTarget.Global -> null
    }
    return if (profile == null) {
        onGlobalChange(settings.copy(bitrateKbps = kbps))
        "the default bitrate"
    } else {
        // Only the bitrate moves — a speed test has nothing to say about the rest of the profile.
        // Re-read rather than trusting the copy this dialog was opened with, so a rename or another
        // edit in between isn't clobbered.
        val live = profiles.byId(profile.id) ?: profile
        profiles.save(live.copy(overrides = live.overrides.copy(bitrateKbps = kbps)))
        "“${live.name}”"
    }
}

/** Ask for far more than any real link can carry, so the link is what limits the answer. */
private const val TARGET_KBPS = 3_000_000

/** Long enough to fill the pipe and settle, short enough not to interrupt anyone for long. */
private const val BURST_MS = 2_000
private const val POLL_INTERVAL_MS = 250
private const val POLL_BUDGET_MS = 10_000
private const val SETTLE_MS = 400L
private const val SPEED_TEST_CONNECT_TIMEOUT_MS = 15_000
