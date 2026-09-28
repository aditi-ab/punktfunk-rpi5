package io.unom.punktfunk

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/** One overlay line from the shared formatter: [role] 0 primary, 1 detail, 2 muted, 3 warning. */
internal data class HudLine(val role: Int, val text: String)

/** `<role>\t<text>\n` per line, as `nativeVideoStatsLines` returns it. Malformed lines drop. */
internal fun decodeHudLines(encoded: String?): List<HudLine> =
    encoded.orEmpty().lineSequence().mapNotNull { line ->
        val tab = line.indexOf('\t')
        if (tab <= 0) null else HudLine(line.substring(0, tab).toIntOrNull() ?: 0, line.substring(tab + 1))
    }.toList()

/**
 * The live stats overlay: the lines `punktfunk_core::hud` built for this window, painted by role.
 * The tier, the vocabulary and every label are decided natively, the same as on every other
 * client; this only draws them. [scale] is the player's Statistics size on top of the density.
 */
@Composable
internal fun StatsOverlay(lines: List<HudLine>, modifier: Modifier = Modifier, scale: Float = 1f) {
    if (lines.isEmpty()) return
    val k = scale.coerceIn(0.5f, 4f)
    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape((6 * k).dp))
            .padding(horizontal = (8 * k).dp, vertical = (4 * k).dp),
    ) {
        lines.forEach { statLine(it.text, roleColor(it.role), k) }
    }
}

/** A cross-client `hud_placement` name as a Compose corner; "" and unknown are top left. */
internal fun hudAlignment(name: String): Alignment = when (name) {
    "topTrailing" -> Alignment.TopEnd
    "bottomLeading" -> Alignment.BottomStart
    "bottomTrailing" -> Alignment.BottomEnd
    else -> Alignment.TopStart
}

internal fun roleColor(role: Int): Color = when (role) {
    1 -> Color(0xFFB0D0FF)
    2 -> Color(0xFF9AA6B8)
    3 -> Color(0xFFFFD9A0)
    else -> Color.White
}

/**
 * One monospace HUD line — the shared type ramp so every line lines up. Line height and tracking
 * are pinned: the theme's `bodyLarge` would set 12 sp text on a 24 sp line.
 */
@Composable
private fun statLine(text: String, color: Color, k: Float) {
    Text(
        text, color = color, fontFamily = FontFamily.Monospace, fontSize = (12 * k).sp,
        lineHeight = (16 * k).sp, letterSpacing = 0.sp,
    )
}
