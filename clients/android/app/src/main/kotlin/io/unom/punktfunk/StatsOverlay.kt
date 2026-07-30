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
 * `capture→decoded` headline without the `display` term when it didn't. Reads the 26-double
 * layout from [NativeBridge.nativeVideoStats]:
 * `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skew, w, h, hz, lostTotal, bitDepth, colorPrimaries,
 * colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms, netP50Ms, lost, skipped,
 * fec, frames, dispValid, displayP50Ms, e2eDispP50Ms, e2eDispP95Ms]`.
 *
 * [verbosity] selects how many lines render (each tier a superset of the last — see
 * [StatsVerbosity]):
 * - [StatsVerbosity.COMPACT] — one line, `fps · end-to-end ms · Mb/s` (+ a loss flag).
 * - [StatsVerbosity.NORMAL] — the res/fps/Mb·s line, the end-to-end p50/p95 headline, and the
 *   reliability counters (18–21) when nonzero.
 * - [StatsVerbosity.DETAILED] — also the decoder label, the video-feed descriptor (10–13), and the
 *   stage equation (14/15, split into `host + network` when the Phase-2 terms at 16/17 are nonzero).
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
            val tag = if (skew) "" else " (same-host clock)"
            val (p50, p95, endpoint) = if (dispValid) {
                Triple(s[24], s[25], "capture→displayed")
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
                statLine(
                    "= $hostTerms + decode ${"%.1f".format(s[15])}$displayTerm$presents",
                    Color.White,
                )
            }
        }
        counterLine(s, lost)?.let { statLine(it, Color(0xFFFFB0B0)) }
    }
}

/** One monospace HUD line — the shared type ramp so every tier's rows line up. */
@Composable
private fun statLine(text: String, color: Color) {
    Text(text, color = color, fontFamily = FontFamily.Monospace, fontSize = 12.sp)
}

/**
 * The single [StatsVerbosity.COMPACT] line: `238 fps · 1.3 ms · 921 Mb/s`. The end-to-end p50 term
 * is dropped when no in-range latency sample landed (`latValid` false), and a loss flag
 * `· ⚠ lost {n}` is appended when the window (or, on an old lib, the session) dropped frames — the
 * one reliability signal worth surfacing even at the tersest tier.
 */
private fun compactLine(s: DoubleArray, latValid: Boolean): String {
    // Prefer the capture→displayed end-to-end (s[24]) when a render timestamp landed this window.
    val e2eP50 = if (s.size >= 26 && s[22] != 0.0) s[24] else s[2]
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
    return buildList {
        if (lost > 0) {
            val pct = 100.0 * lost / (frames + lost).coerceAtLeast(1)
            add("lost $lost (${"%.1f".format(pct)}%)")
        }
        if (skipped > 0) add("skipped $skipped")
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
