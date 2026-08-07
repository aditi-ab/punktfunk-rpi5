package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.roundToInt

/**
 * The live stats overlay — the unified HUD (`design/stats-unification.md`): headline is
 * `capture→displayed` tiled by `host+network` + `decode` + `display` when the platform delivered
 * OnFrameRendered render callbacks this window (`dispValid`), falling back to the v1
 * `capture→decoded` headline without the `display` term when it didn't. Reads the 35-double
 * layout from [NativeBridge.nativeVideoStats] (that KDoc is the authoritative index list):
 * `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skew, w, h, hz, lostTotal, bitDepth, colorPrimaries,
 * colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms, netP50Ms, lost, skipped,
 * fec, frames, dispValid, displayP50Ms, e2eDispP50Ms, e2eDispP95Ms, paceP50Ms, latchP50Ms,
 * presentsWindow, presenterActive, feedP50Ms, codecP50Ms, skippedOverflowWindow, audioBufferMs,
 * audioAvOffsetMs]`. Every read
 * is length-guarded, so an older native lib simply omits the lines it can't feed.
 *
 * The shown `display` and `end-to-end` numbers EXCLUDE the OS present floor (see [osFloorMs]) at
 * every tier, and the detailed tier names what was excluded on its own line. The principle is the
 * Apple client's: metrics report what Punktfunk controls, so the compositor's own latch and scanout
 * — which no client can pace under — is reported rather than charged. It also stops the HUD reading
 * worse than it is: the usual Android streaming overlays stop measuring at decode-complete, so a
 * headline that carried the compositor's wait was compared against numbers that never contained it.
 *
 * The RAW figures are not lost — the native 1 Hz `pf.present` logcat line keeps `paceMs`, `latchMs`
 * and `e2eMs` unshaved, so a HUD-off A/B and any cross-session comparison still work off the
 * untouched numbers.
 *
 * [verbosity] selects how many lines render (each tier a superset of the last — see
 * [StatsVerbosity]):
 * - [StatsVerbosity.COMPACT] — one line, `fps · end-to-end ms · Mb/s` (+ a loss flag).
 * - [StatsVerbosity.NORMAL] — the res/fps/Mb·s line, the end-to-end p50/p95 headline, and the
 *   reliability counters (18–21) when nonzero.
 * - [StatsVerbosity.DETAILED] — also the decoder label, the video-feed descriptor (10–13), the
 *   stage equation (14/15, split into `host + network` when the Phase-2 terms at 16/17 are nonzero),
 *   the excluded-floor line when one was measured, and the audio plane's own latency (33/34).
 * [StatsVerbosity.OFF] renders nothing. Older native layouts simply omit the lines they lack (the
 * counter line falls back to the cumulative `lostTotal` at index 9 on a pre-window lib).
 */
@Composable
internal fun StatsOverlay(
    s: DoubleArray,
    verbosity: StatsVerbosity,
    decoderLabel: String = "",
    codecLabel: String = "",
    /**
     * The settings profile this session resolved, appended to the first line when there is one —
     * the in-stream answer to "which profile am I on?", as on the other clients. Absent (the
     * common case: no profile) the line is exactly what it always was.
     */
    profileName: String? = null,
    /**
     * The panel's live refresh rate (0 = unknown). Shown as a warning on the first line whenever
     * it sits below the stream rate — the "an OEM governor ignored the mode pin" tell, which
     * otherwise reads as inexplicable judder and an extra refresh of latency.
     */
    panelHz: Float = 0f,
    modifier: Modifier = Modifier,
) {
    if (verbosity == StatsVerbosity.OFF || s.size < 10) return
    val w = s[6].toInt()
    val h = s[7].toInt()
    val hz = s[8].toInt()
    val panelBelowStream = panelHz > 0f && hz > 0 && panelHz + 1f < hz.toFloat()
    val panelTag = if (panelBelowStream) "   ⚠ panel ${panelHz.roundToInt()} Hz" else ""
    val latValid = s[4] != 0.0
    val skew = s[5] != 0.0
    val lost = s[9].toLong()
    val detailed = verbosity == StatsVerbosity.DETAILED

    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(6.dp))
            .padding(horizontal = 8.dp, vertical = 4.dp),
    ) {
        val profileTag = profileName?.let { "   · $it" }.orEmpty()
        // Compact: everything the glance-value needs on one line, nothing else.
        if (verbosity == StatsVerbosity.COMPACT) {
            statLine(compactLine(s, latValid) + profileTag + panelTag, Color.White)
            return@Column
        }

        statLine(
            "$w×$h@$hz   ${s[0].roundToInt()} fps   ${"%.1f".format(s[1])} Mb/s$profileTag$panelTag",
            Color.White,
        )
        if (detailed && decoderLabel.isNotEmpty()) {
            statLine(decoderLabel, Color(0xFFB0D0FF))
        }
        if (detailed) {
            videoFeedLine(s, codecLabel)?.let { statLine(it, Color.White) }
        }
        if (latValid) {
            // Display stage (s[22]–s[25], from OnFrameRendered): when a render timestamp landed
            // this window the headline is the directly-measured capture→displayed pair and the
            // equation gains its `display` term; otherwise (older lib / no callbacks) the endpoint
            // honestly stays capture→decoded — the equation always tiles the headline interval.
            val dispValid = s.size >= 26 && s[22] != 0.0
            // The OS present floor this window (see [osFloorMs]) is excluded from every shown
            // display / end-to-end number, at every tier — it is pipeline depth no client can pace
            // under, so charging it to Punktfunk made our HUD read worse than clients that simply
            // never measure it. 0.0 when unmeasured, which leaves the numbers exactly as raw as
            // they were.
            val floorMs = osFloorMs(s)
            val tag = if (skew) "" else " (same-host clock)"
            val (p50, p95, endpoint) = if (dispValid) {
                Triple(shave(s[24], floorMs), shave(s[25], floorMs), "capture→displayed")
            } else {
                Triple(s[2], s[3], "capture→decoded")
            }
            statLine(
                "end-to-end ${"%.1f".format(p50)} ms p50 · ${"%.1f".format(p95)} p95 · $endpoint$tag",
                Color.White,
            )
            if (detailed && s.size >= 16) {
                // Phase-2 split (s[16]/s[17]): render `host + network` separately when the host
                // reported its share this window; otherwise the combined term (old host / no
                // matched 0xCF timing).
                val hostTerms = if (s.size >= 18 && s[16] > 0) {
                    "host ${"%.1f".format(s[16])} + network ${"%.1f".format(s[17])}"
                } else {
                    "host+network ${"%.1f".format(s[14])}"
                }
                // Timeline-presenter split (s[26]/s[27], when s[29] flags it active): the display
                // term decomposes into pace (store + glass budget) + latch (SurfaceFlinger), and
                // s[28] is the on-glass confirm count — presents ≪ fps means the presenter is
                // dropping/serializing, an fps deficit is upstream.
                val split = s.size >= 30 && s[29] != 0.0 && (s[26] > 0 || s[27] > 0)
                val displayTerm = when {
                    // Floor excluded: what remains of the `display` term is the half Punktfunk
                    // owns (the presenter's pace wait), and the excluded line below carries the
                    // latch — printing the split too would report the same milliseconds twice.
                    dispValid && floorMs > 0 ->
                        " + display ${"%.1f".format(shave(s[23], floorMs))}"
                    dispValid && split ->
                        " + display ${"%.1f".format(s[23])} " +
                            "(pace ${"%.1f".format(s[26])} + latch ${"%.1f".format(s[27])})"
                    dispValid -> " + display ${"%.1f".format(s[23])}"
                    else -> ""
                }
                val presents = if (s.size >= 30 && s[29] != 0.0) {
                    "   · presents ${s[28].toInt()}"
                } else {
                    ""
                }
                // P3 decode split (s[30]/s[31]): `feed` = received→queued (hand-off + input-slot
                // wait) + `codec` = queued→decoded (codec-pure) — rendered when a sample landed.
                val decodeTerm = if (s.size >= 33 && (s[30] > 0 || s[31] > 0)) {
                    "decode ${"%.1f".format(s[15])} " +
                        "(feed ${"%.1f".format(s[30])} + codec ${"%.1f".format(s[31])})"
                } else {
                    "decode ${"%.1f".format(s[15])}"
                }
                statLine(
                    "= $hostTerms + $decodeTerm$displayTerm$presents",
                    Color.White,
                )
                // What the numbers above leave out, named — the Apple client's
                // `os present +N excluded` line, same wording so the two HUDs read alike.
                // (This replaces the old "≈ Apple-HUD equiv" twin: both clients now shave, and
                // Android's shave is measured rather than assumed at 2 refresh periods.)
                if (floorMs > 0) {
                    statLine(
                        "os present +${"%.1f".format(floorMs)} excluded (display pipeline minimum)",
                        Color(0xFF9AA6B8),
                    )
                }
            }
        }
        if (detailed) {
            audioLine(s)?.let { statLine(it, Color.White) }
        }
        counterLine(s, lost)?.let { statLine(it, Color(0xFFFFB0B0)) }
    }
}

/**
 * The audio plane's own latency from the live gauges at 33/34 — `audio buffer 42 ms · a/v +18 ms`,
 * the same wording the desktop HUD uses. `buffer` is how much decoded audio is queued ahead of the
 * speaker; `a/v` is where that PUTS it relative to the picture (positive = audio behind). `null`
 * before any audio has been queued (buffer 0 — audio off, or the ring not yet primed) and on an
 * older native layout.
 *
 * Both terms, not just the depth: a deep ring on a jittery link is correct behaviour — the
 * underrun-driven floor earned that buffer — and only the offset distinguishes it from a ring that
 * is simply holding audio late. The offset term is dropped at zero, which is both "aligned" and
 * "no measurement yet"; the depth alone is still the triage number, and it is the one that did not
 * exist at all before (the plane published nothing any surface could render, so a "the audio delay
 * is way too high" report had no instrument behind it).
 *
 * NOT shaved by [osFloorMs], unlike every video figure above. That shave is a reporting policy —
 * metrics report what Punktfunk controls — but sound has to reach the ear when the light reaches
 * the eye, so the sync loop aligns against the RAW capture→displayed time (see the native
 * `DisplayTracker`) and this offset is stated in those same terms. Subtracting the floor here would
 * report an alignment the listener is not getting.
 */
private fun audioLine(s: DoubleArray): String? {
    if (s.size < 35) return null
    val bufferMs = s[33].roundToInt()
    if (bufferMs <= 0) return null
    val avOffset = s[34].roundToInt()
    val avTerm = if (avOffset != 0) " · a/v ${if (avOffset > 0) "+" else ""}$avOffset ms" else ""
    return "audio buffer $bufferMs ms$avTerm"
}

/** One monospace HUD line — the shared type ramp so every tier's rows line up. */
@Composable
private fun statLine(text: String, color: Color) {
    Text(text, color = color, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
}

/**
 * The OS present floor to exclude from the shown `display` / `end-to-end` numbers, ms — the
 * measured `latch` p50 at index 27, i.e. release→`OnFrameRendered`: SurfaceFlinger's own latch and
 * scanout. That is compositor pipeline depth no client can pace under, so it is reported as
 * excluded rather than charged to Punktfunk — the Apple client's policy since its presentation
 * rebuild, where the same floor is measured from the display link's vend lead.
 *
 * Measured, not assumed: the previous Android treatment used a fixed `2000/hz` twin, but the latch
 * varies with panel rate, tunnelled playback and the vendor's low-latency mode (~21 ms p50 observed
 * where the ~2-interval model predicts less), and this term self-adapts to all three. It is also
 * available on every render path — the presenter's and both legacy release-immediately ones — since
 * the release stamp it starts from is parked on every render, so it does not depend on
 * `presenterActive` (29).
 *
 * `0.0` means unmeasured — no display stage this window (an older native lib, API < 33, or a
 * platform that refused the callback), or no latch sample paired — and every caller then leaves its
 * number raw, which is the honest fallback: we exclude only what we actually measured.
 */
private fun osFloorMs(s: DoubleArray): Double {
    val dispValid = s.size >= 26 && s[22] != 0.0
    if (!dispValid || s.size < 28) return 0.0
    return s[27].coerceAtLeast(0.0)
}

/**
 * Subtract the excluded [floorMs] from a shown latency [ms], clamped at zero — the percentiles are
 * drawn from different sample sets (a p50 latch against a p50/p95 end-to-end), so the difference can
 * legitimately go slightly negative on a well-paced window without anything being wrong.
 */
private fun shave(ms: Double, floorMs: Double): Double = (ms - floorMs).coerceAtLeast(0.0)

/**
 * The single [StatsVerbosity.COMPACT] line: `238 fps · 1.3 ms · 921 Mb/s`. The end-to-end p50 term
 * is dropped when no in-range latency sample landed (`latValid` false), and a loss flag
 * `· ⚠ lost {n}` is appended when the window (or, on an old lib, the session) dropped frames — the
 * one reliability signal worth surfacing even at the tersest tier.
 */
private fun compactLine(s: DoubleArray, latValid: Boolean): String {
    // Prefer the capture→displayed end-to-end (s[24]) when a render timestamp landed this window,
    // less the excluded OS present floor — the same number the richer tiers headline.
    val e2eP50 = if (s.size >= 26 && s[22] != 0.0) shave(s[24], osFloorMs(s)) else s[2]
    val parts = buildList {
        add("${s[0].roundToInt()} fps")
        if (latValid) add("${"%.1f".format(e2eP50)} ms")
        add("${s[1].roundToInt()} Mb/s")
    }
    val lostWindow = if (s.size >= 22) s[18].toLong() else s[9].toLong()
    val suffix = if (lostWindow > 0) "   ⚠ lost $lostWindow" else ""
    return parts.joinToString(" · ") + suffix
}

/**
 * Format the spec's line-4 counters from the per-window doubles at 18–21 —
 * `lost {n} ({pct}%) · skipped {m} · FEC {k}`, each term only when nonzero, the whole line `null`
 * when all are zero (spec: "only rendered when any value is nonzero"). `pct = lost/(frames+lost)`
 * (the received count rides at index 21). A pre-window layout (< 22 doubles) falls back to the
 * session-cumulative `lostTotal` so an older native lib still reports loss.
 */
private fun counterLine(s: DoubleArray, lostTotal: Long): String? {
    if (s.size < 22) return if (lostTotal > 0) "lost $lostTotal" else null
    val lost = s[18].toLong()
    val skipped = s[19].toLong()
    val fec = s[20].toLong()
    val frames = s[21].toLong()
    if (lost == 0L && skipped == 0L && fec == 0L) return null
    // The overflow subset of `skipped` (s[32]): whole AUs dropped before feeding — the decoder
    // fell behind. Absent (0 / old layout) the plain count keeps meaning benign pacing drops.
    val overflow = if (s.size >= 33) s[32].toLong() else 0L
    return buildList {
        if (lost > 0) {
            val pct = 100.0 * lost / (frames + lost).coerceAtLeast(1)
            add("lost $lost (${"%.1f".format(pct)}%)")
        }
        if (skipped > 0) {
            add(if (overflow > 0) "skipped $skipped (⚠ $overflow overflow)" else "skipped $skipped")
        }
        if (fec > 0) add("FEC $fec")
    }.joinToString(" · ")
}

/**
 * Format the negotiated video-feed descriptor from [codecLabel] plus the trailing four stats
 * doubles `[bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc]`, e.g.
 * `AV1 · 10-bit · HDR (BT.2020 PQ) · 4:2:0`. Returns `null` on a pre-video-feed layout (< 14 doubles)
 * so the overlay simply omits the line. The codes are CICP / H.273: transfer 16 = PQ, 18 = HLG (else
 * SDR); primaries 9 = BT.2020, 1 = BT.709; chroma_format_idc 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
 * [codecLabel] is the host-resolved codec (`nativeVideoCodecLabel`); a blank one falls back to
 * `HEVC` (the pre-negotiation default) for the brief window before it's resolved.
 */
private fun videoFeedLine(s: DoubleArray, codecLabel: String): String? {
    if (s.size < 14) return null
    val bitDepth = s[10].toInt()
    val primaries = s[11].toInt()
    val transfer = s[12].toInt()
    val chromaIdc = s[13].toInt()
    val depthLabel = if (bitDepth > 0) "$bitDepth-bit" else "8-bit"
    val (dynamicRange, colorSpace) = when (transfer) {
        16 -> "HDR" to "BT.2020 PQ"
        18 -> "HDR" to "BT.2020 HLG"
        else -> "SDR" to if (primaries == 9) "BT.2020" else "BT.709"
    }
    val chromaLabel = when (chromaIdc) {
        3 -> "4:4:4"
        2 -> "4:2:2"
        else -> "4:2:0"
    }
    val codec = codecLabel.ifEmpty { "HEVC" }
    return "$codec · $depthLabel · $dynamicRange ($colorSpace) · $chromaLabel"
}
