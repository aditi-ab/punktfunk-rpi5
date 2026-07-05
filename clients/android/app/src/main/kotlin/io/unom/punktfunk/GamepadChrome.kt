package io.unom.punktfunk

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.max
import kotlin.math.sin

// The console chrome shared by the gamepad-driven screens — the Android mirror of the Apple client's
// GamepadChrome.swift: a slow-drifting violet aurora backdrop, a bottom button-glyph hint bar, and a
// connected-controller status chip. One look across every screen is what makes the console UI read
// as a coherent mode rather than a set of themed pages.

/** One drifting colour blob of the aurora field. Integer [sx]/[sy] keep the loop seamless at wrap. */
private class AuroraBlob(
    val color: Color,
    val baseX: Float,
    val baseY: Float,
    val driftX: Float,
    val driftY: Float,
    val sx: Int,
    val sy: Int,
    val phase: Float,
    val radiusFrac: Float,
    val alpha: Float,
)

private val auroraBlobs = listOf(
    AuroraBlob(Color(0xFF877AF5), 0.30f, 0.26f, 0.16f, 0.10f, 1, 1, 0.0f, 0.62f, 0.55f), // brand violet
    AuroraBlob(Color(0xFF3E33B8), 0.78f, 0.68f, 0.13f, 0.14f, 1, 2, 2.4f, 0.68f, 0.58f), // deep indigo
    AuroraBlob(Color(0xFF9E4CCC), 0.16f, 0.82f, 0.12f, 0.09f, 2, 1, 4.1f, 0.52f, 0.42f), // plum
    AuroraBlob(Color(0xFF3862DB), 0.72f, 0.14f, 0.10f, 0.08f, 1, 3, 1.2f, 0.48f, 0.40f), // cool blue
)

/**
 * The living console backdrop: soft violet-family blobs drifting over black on slow, seamless loops,
 * finished with a centre-pooling vignette and top/bottom legibility scrims. A Compose approximation
 * of the Apple client's MeshGradient aurora — same brand family, same "ambience, never content" role.
 */
@Composable
fun GamepadAuroraBackground(modifier: Modifier = Modifier) {
    val transition = rememberInfiniteTransition(label = "aurora")
    // A full 0..2π sweep over ~96 s; integer per-blob multipliers make sin/cos continuous at the wrap
    // so the field never visibly jumps when the animation restarts.
    val angle by transition.animateFloat(
        initialValue = 0f,
        targetValue = (2 * PI).toFloat(),
        animationSpec = infiniteRepeatable(tween(96_000, easing = LinearEasing), RepeatMode.Restart),
        label = "angle",
    )
    Canvas(modifier) {
        drawRect(Color.Black)
        val span = max(size.width, size.height)
        for (b in auroraBlobs) {
            val cx = (b.baseX + b.driftX * sin(angle * b.sx + b.phase)) * size.width
            val cy = (b.baseY + b.driftY * cos(angle * b.sy + b.phase)) * size.height
            val r = span * b.radiusFrac
            drawCircle(
                brush = Brush.radialGradient(
                    colors = listOf(b.color.copy(alpha = b.alpha), Color.Transparent),
                    center = Offset(cx, cy),
                    radius = r,
                ),
                center = Offset(cx, cy),
                radius = r,
                blendMode = BlendMode.Plus,
            )
        }
        // Cinematic vignette: pool light centre, sink the corners.
        drawRect(
            Brush.radialGradient(
                colors = listOf(Color.Transparent, Color.Black.copy(alpha = 0.44f)),
                center = Offset(size.width / 2, size.height / 2),
                radius = span * 0.92f,
            ),
        )
        // Top/bottom legibility scrim for the pinned title + hint bar.
        drawRect(
            Brush.verticalGradient(
                0.0f to Color.Black.copy(alpha = 0.40f),
                0.30f to Color.Black.copy(alpha = 0.05f),
                0.70f to Color.Black.copy(alpha = 0.06f),
                1.0f to Color.Black.copy(alpha = 0.42f),
            ),
        )
    }
}

/**
 * The calm backdrop for the console FORM screens (settings, add-host) — deliberately still and quiet
 * (unlike the launcher's drifting aurora), a deep indigo base with two soft brand glows so the glass
 * rows have some colour + luminance to sit on. Mirrors the Apple client's GamepadFormBackground.
 */
@Composable
fun GamepadFormBackground(modifier: Modifier = Modifier) {
    Canvas(modifier) {
        val span = max(size.width, size.height)
        drawRect(Color(0xFF131126))
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(Color(0xE6635AAE), Color.Transparent),
                center = Offset(size.width * 0.24f, size.height * 0.12f),
                radius = span * 0.7f,
            ),
            center = Offset(size.width * 0.24f, size.height * 0.12f),
            radius = span * 0.7f,
        )
        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(Color(0xBF343E96), Color.Transparent),
                center = Offset(size.width * 0.82f, size.height * 0.9f),
                radius = span * 0.7f,
            ),
            center = Offset(size.width * 0.82f, size.height * 0.9f),
            radius = span * 0.7f,
        )
    }
}

/**
 * The exact inset every console screen places its floating legend at (bottom-start), so the legend
 * sits in the SAME spot across Home / Settings / Add-Host and appears pinned while the content behind
 * it cross-fades between screens.
 */
val ConsoleLegendInset = PaddingValues(start = 24.dp, bottom = 24.dp)

/** The shared horizontal inset for a console screen's heading (matches the legend's left edge). */
val ConsoleEdgeInset = 24.dp

/**
 * The heading every console screen uses — one style, one inset, so titles line up across Home /
 * Settings / Add-Host / Library. Callers place it at the top of their content (or float it, on Home).
 */
@Composable
fun ConsoleHeader(title: String, modifier: Modifier = Modifier, horizontalInset: Boolean = true) {
    // `horizontalInset = false` when the caller's container already pads to ConsoleEdgeInset (e.g. a
    // LazyColumn contentPadding) — so the heading lands at the SAME 24dp on every screen either way.
    val h = if (horizontalInset) ConsoleEdgeInset else 0.dp
    Text(
        title,
        style = MaterialTheme.typography.headlineMedium,
        fontWeight = FontWeight.Bold,
        color = Color.White,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier.padding(start = h, end = h, top = 18.dp, bottom = 10.dp),
    )
}

/**
 * One glyph + label cell of a hint bar. [glyph] is the face letter; [color] its Xbox-convention hue.
 * [onClick], when set, makes the cell tappable — a TOUCH escape hatch so a user without a working
 * controller can still drive the console UI (and reach Settings to switch it off).
 */
class GamepadHint(
    val glyph: Char,
    val color: Color,
    val text: String,
    val onClick: (() -> Unit)? = null,
    // Render as the D-pad-centre "select" button (a ring) instead of a lettered face-button disc —
    // for a TV remote, which has no A/B/X/Y.
    val select: Boolean = false,
    // Render as the gamepad Select/View button (a small capsule).
    val viewButton: Boolean = false,
)

/** Xbox-convention face-button colours, so the glyphs read at a glance across the room. */
object PadGlyph {
    val A = Color(0xFF6BBE45)
    val B = Color(0xFFD14B4B)
    val X = Color(0xFF4B7BD1)
    val Y = Color(0xFFE0B23C)
    fun hint(glyph: Char, text: String, onClick: (() -> Unit)? = null) = GamepadHint(
        glyph, when (glyph) { 'A' -> A; 'B' -> B; 'X' -> X; 'Y' -> Y; else -> Color(0xFF9A93C7) }, text, onClick,
    )
}

/** A round face-button badge: a coloured disc with the button letter, like a controller's face. */
@Composable
fun GamepadButtonGlyph(glyph: Char, color: Color, size: androidx.compose.ui.unit.Dp = 26.dp) {
    Box(
        modifier = Modifier
            .size(size)
            .clip(CircleShape)
            .background(color),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            glyph.toString(),
            color = Color.White,
            fontWeight = FontWeight.Bold,
            fontSize = (size.value * 0.52f).sp,
            textAlign = TextAlign.Center,
        )
    }
}

/** The D-pad-centre "select" button — a green (confirm) disc with a ring; the TV-remote glyph for A. */
@Composable
private fun SelectGlyph(size: androidx.compose.ui.unit.Dp = 26.dp) {
    Box(
        modifier = Modifier.size(size).clip(CircleShape).background(PadGlyph.A),
        contentAlignment = Alignment.Center,
    ) {
        Box(Modifier.size(size * 0.46f).clip(CircleShape).border(2.dp, Color.White, CircleShape))
    }
}

/** The remote's "Back" button — a back-arrow disc; the TV-remote glyph for B (back / cancel / done). */
@Composable
private fun BackGlyph(size: androidx.compose.ui.unit.Dp = 26.dp) {
    GamepadButtonGlyph('↩', PadGlyph.B, size)
}

/** The gamepad "Select / View" button — a small capsule outline, matching its physical shape. */
@Composable
private fun ViewButtonGlyph(size: androidx.compose.ui.unit.Dp = 26.dp) {
    Box(Modifier.size(size), contentAlignment = Alignment.Center) {
        Box(
            Modifier
                .size(width = size * 0.74f, height = size * 0.46f)
                .clip(RoundedCornerShape(50))
                .border(1.6.dp, Color.White.copy(alpha = 0.85f), RoundedCornerShape(50)),
        )
    }
}

/**
 * The pinned controls legend every gamepad screen shows along the bottom — worn as a self-contained
 * translucent pill so it floats over the aurora rather than dissolving into it.
 */
@Composable
fun GamepadHintBar(hints: List<GamepadHint>, modifier: Modifier = Modifier, hazeState: HazeState? = null) {
    // On a TV D-pad remote (no A/B/X/Y), auto-swap the two universal pad glyphs every screen uses:
    // A (confirm) → the select ring, B (back/cancel) → a back glyph. Screen-specific glyphs like the
    // home's Up/Down handle themselves. Defaults to the gamepad look off an Activity (preview/tests).
    val padIsGamepad = (LocalContext.current as? MainActivity)?.lastPadIsGamepad ?: true
    val shape = RoundedCornerShape(50)
    // With a haze source, blur the content behind the pill (real backdrop blur, API 31+; a translucent
    // scrim below) + a light tint; otherwise fall back to a solid frosted fill.
    val frosted = if (hazeState != null) {
        modifier.clip(shape).hazeEffect(hazeState).background(Color(0x4014122A))
    } else {
        modifier.clip(shape).background(Color(0x8C14122A))
    }
    Row(
        modifier = frosted
            .border(1.dp, Color.White.copy(alpha = 0.14f), shape)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(11.dp),
    ) {
        for (h in hints) {
            val cb = h.onClick
            val cell = if (cb != null) {
                Modifier.clip(RoundedCornerShape(50)).clickable(onClick = cb).padding(horizontal = 4.dp, vertical = 5.dp)
            } else {
                Modifier
            }
            Row(modifier = cell, verticalAlignment = Alignment.CenterVertically) {
                when {
                    h.viewButton -> ViewButtonGlyph()
                    h.select || (!padIsGamepad && h.glyph == 'A') -> SelectGlyph()
                    !padIsGamepad && h.glyph == 'B' -> BackGlyph()
                    else -> GamepadButtonGlyph(h.glyph, h.color)
                }
                Spacer(Modifier.width(6.dp))
                Text(
                    h.text,
                    style = MaterialTheme.typography.labelLarge,
                    color = Color.White.copy(alpha = 0.9f),
                    maxLines = 1,
                    softWrap = false, // never char-wrap a label when several hints crowd a narrow pill
                )
            }
        }
    }
}

/** "Which pad is driving this UI" — a quiet chip in the console top bar with the controller's name. */
@Composable
fun ControllerStatusChip(name: String, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .clip(RoundedCornerShape(50))
            .background(Color.White.copy(alpha = 0.08f))
            .padding(horizontal = 12.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Filled.SportsEsports,
            contentDescription = null,
            tint = Color.White.copy(alpha = 0.75f),
            modifier = Modifier.size(16.dp),
        )
        Spacer(Modifier.width(7.dp))
        Text(
            name,
            style = MaterialTheme.typography.labelMedium,
            color = Color.White.copy(alpha = 0.75f),
            maxLines = 1,
        )
    }
}
