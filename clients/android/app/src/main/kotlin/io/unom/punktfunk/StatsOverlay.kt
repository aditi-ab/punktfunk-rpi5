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
 * The live stats overlay — the unified HUD (`design/stats-unification.md`, Android v1: headline is
 * `capture→decoded`, tiled by `host+network` + `decode`). Reads the 22-double layout from
 * [NativeBridge.nativeVideoStats]:
 * `[fps, mbps, e2eP50Ms, e2eP95Ms, latValid, skew, w, h, hz, lostTotal, bitDepth, colorPrimaries,
 * colorTransfer, chromaFormatIdc, hostNetP50Ms, decodeP50Ms, hostP50Ms, netP50Ms, lost, skipped,
 * fec, frames]`.
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
    modifier: Modifier = Modifier,
) {
    if (verbosity == StatsVerbosity.OFF || s.size < 10) return
    val w = s[6].toInt()
    val h = s[7].toInt()
    val hz = s[8].toInt()
    val latValid = s[4] != 0.0
    val skew = s[5] != 0.0
    val lost = s[9].toLong()
    val detailed = verbosity == StatsVerbosity.DETAILED

    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(6.dp))
            .padding(horizontal = 8.dp, vertical = 4.dp),
    ) {
        // Compact: everything the glance-value needs on one line, nothing else.
        if (verbosity == StatsVerbosity.COMPACT) {
            statLine(compactLine(s, latValid), Color.White)
            return@Column
        }

        statLine("$w×$h@$hz   ${s[0].roundToInt()} fps   ${"%.1f".format(s[1])} Mb/s", Color.White)
        if (detailed && decoderLabel.isNotEmpty()) {
            statLine(decoderLabel, Color(0xFFB0D0FF))
        }
        if (detailed) {
            videoFeedLine(s)?.let { statLine(it, Color.White) }
        }
        if (latValid) {
            val tag = if (skew) "" else " (same-host clock)"
            statLine(
                "end-to-end ${"%.1f".format(s[2])} ms p50 · ${"%.1f".format(s[3])} p95 · capture→decoded$tag",
                Color.White,
            )
            if (detailed && s.size >= 16) {
                // Phase-2 split (s[16]/s[17]): render `host + network` separately when the host
                // reported its share this window; otherwise the combined term (old host / no
                // matched 0xCF timing).
                val equation = if (s.size >= 18 && s[16] > 0) {
                    "= host ${"%.1f".format(s[16])} + network ${"%.1f".format(s[17])} + decode ${"%.1f".format(s[15])}"
                } else {
                    "= host+network ${"%.1f".format(s[14])} + decode ${"%.1f".format(s[15])}"
                }
                statLine(equation, Color.White)
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
    val parts = buildList {
        add("${s[0].roundToInt()} fps")
        if (latValid) add("${"%.1f".format(s[2])} ms")
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
 * Format the negotiated video-feed descriptor from the trailing four stats doubles
 * `[bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc]`, e.g.
 * `HEVC · 10-bit · HDR (BT.2020 PQ) · 4:2:0`. Returns `null` on a pre-video-feed layout (< 14 doubles)
 * so the overlay simply omits the line. The codes are CICP / H.273: transfer 16 = PQ, 18 = HLG (else
 * SDR); primaries 9 = BT.2020, 1 = BT.709; chroma_format_idc 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4. The
 * Android decoder is always HEVC (`video/hevc`).
 */
private fun videoFeedLine(s: DoubleArray): String? {
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
    return "HEVC · $depthLabel · $dynamicRange ($colorSpace) · $chromaLabel"
}
