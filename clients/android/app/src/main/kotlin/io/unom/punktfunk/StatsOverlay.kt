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
 * The live stats overlay — mirrors the Apple client's HUD. Reads the 14-double layout from
 * [NativeBridge.nativeVideoStats]:
 * `[fps, mbps, latP50Ms, latP95Ms, latValid, skew, w, h, hz, dropped, bitDepth, colorPrimaries,
 * colorTransfer, chromaFormatIdc]`. The trailing four (present on a current native lib) describe the
 * negotiated video feed and render as a codec/depth/colour/chroma line; older layouts just omit it.
 */
@Composable
internal fun StatsOverlay(s: DoubleArray, modifier: Modifier = Modifier) {
    if (s.size < 10) return
    val w = s[6].toInt()
    val h = s[7].toInt()
    val hz = s[8].toInt()
    val latValid = s[4] != 0.0
    val skew = s[5] != 0.0
    val dropped = s[9].toLong()
    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(6.dp))
            .padding(horizontal = 8.dp, vertical = 4.dp),
    ) {
        Text(
            "$w×$h@$hz   ${s[0].roundToInt()} fps   ${"%.1f".format(s[1])} Mb/s",
            color = Color.White,
            fontFamily = FontFamily.Monospace,
            fontSize = 12.sp,
        )
        videoFeedLine(s)?.let { feed ->
            Text(
                feed,
                color = Color.White,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
        }
        if (latValid) {
            val tag = if (skew) "" else " (same-host)"
            Text(
                "capture→client ${"%.1f".format(s[2])}/${"%.1f".format(s[3])} ms p50/p95$tag",
                color = Color.White,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
        }
        if (dropped > 0) {
            Text(
                "dropped $dropped",
                color = Color(0xFFFFB0B0),
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
        }
    }
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
