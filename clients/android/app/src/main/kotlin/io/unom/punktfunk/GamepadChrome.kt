package io.unom.punktfunk

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect
import io.unom.punktfunk.kit.Gamepad
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.max
import kotlin.math.roundToInt
import kotlin.math.sin

// The console chrome shared by the gamepad-driven screens — the Android mirror of the Apple client's
// GamepadChrome.swift: a slow-drifting violet aurora backdrop, a bottom button-glyph hint bar, and a
// connected-controller status chip. One look across every screen is what makes the console UI read
// as a coherent mode rather than a set of themed pages.

/**
 * One drifting blob of the aurora field: where it sits, how far it wanders, and how fast. Integer
 * [sx]/[sy] keep the loop seamless at wrap. The COLOUR is the palette's, taken from its ramp at
 * draw time, so the field always shows several of that palette's tones at once.
 */
private class AuroraBlob(
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
    AuroraBlob(0.30f, 0.26f, 0.16f, 0.10f, 1, 1, 0.0f, 0.62f, 0.55f),
    AuroraBlob(0.78f, 0.68f, 0.13f, 0.14f, 1, 2, 2.4f, 0.68f, 0.58f),
    AuroraBlob(0.16f, 0.82f, 0.12f, 0.09f, 2, 1, 4.1f, 0.52f, 0.42f),
    AuroraBlob(0.72f, 0.14f, 0.10f, 0.08f, 1, 3, 1.2f, 0.48f, 0.40f),
)

/**
 * The living console backdrop: soft blobs from the palette's ramp drifting over its ground on
 * slow, seamless loops, finished with a centre-pooling vignette and top/bottom legibility scrims.
 * A Compose approximation of the Apple client's MeshGradient aurora — same colour families, same
 * "ambience, never content" role, and the same [GamepadPalette] setting recolours both.
 *
 * [calm] is what the FORM screens wear: the pools dim onto the ground so the glass rows keep real
 * colour and luminance without the launcher's contrast. Motion is identical either way on purpose
 * — only the contrast differs, so moving between screens can't make the field jump.
 *
 * Honours the system's "remove animations" accessibility setting by freezing at a fixed phase, the
 * same courtesy the Apple client pays Reduce Motion.
 */
@Composable
fun GamepadAuroraBackground(modifier: Modifier = Modifier, calm: Boolean = false) {
    val ink = LocalGamepadInk.current
    val palette = LocalGamepadPalette.current
    val animated = animationsEnabled()
    val transition = rememberInfiniteTransition(label = "aurora")
    // A full 0..2π sweep over ~96 s; integer per-blob multipliers make sin/cos continuous at the
    // wrap so the field never visibly jumps when the animation restarts.
    val swept by transition.animateFloat(
        initialValue = 0f,
        targetValue = (2 * PI).toFloat(),
        animationSpec = infiniteRepeatable(tween(96_000, easing = LinearEasing), RepeatMode.Restart),
        label = "angle",
    )
    val angle = if (animated) swept else 0f
    val tones = palette.blobColors
    val ground = palette.groundColor
    // Where the scrims tend, and how hard. Mixing a PALE field toward white at the dark field's
    // strength bleaches the chroma straight out of the gradient, so a pale palette gets under
    // half — the same scrim strength the desktop console's shader carries.
    val scrim = if (palette.light) ink.fg else Color.Black
    val strength = if (palette.light) 0.45f else 1f
    Canvas(modifier) {
        drawRect(ground)
        val span = max(size.width, size.height)
        for ((i, b) in auroraBlobs.withIndex()) {
            val cx = (b.baseX + b.driftX * sin(angle * b.sx + b.phase)) * size.width
            val cy = (b.baseY + b.driftY * cos(angle * b.sy + b.phase)) * size.height
            val r = span * b.radiusFrac
            // Calm scales each blob's contribution rather than dimming the whole canvas: the
            // ground stays put and only the pools come down to meet it, which is the same "lower
            // the contrast, keep the colour" the desktop console's `calm` uniform does.
            val alpha = if (calm) b.alpha * 0.62f else b.alpha
            drawCircle(
                brush = Brush.radialGradient(
                    colors = listOf(tones[i].copy(alpha = alpha), Color.Transparent),
                    center = Offset(cx, cy),
                    radius = r,
                ),
                center = Offset(cx, cy),
                radius = r,
                // Additive only works over a DARK ground; over a pale one every blob
                // saturates to white and the field turns grey. Pale palettes tint instead.
                blendMode = if (palette.light) BlendMode.SrcOver else BlendMode.Plus,
            )
        }
        // Cinematic vignette: pool light centre, settle the corners toward the scrim. Halved under
        // calm: a launcher's cards sit in the pooled centre, but a form screen's rows run out
        // toward the edges, where crushing them just eats the list.
        drawRect(
            Brush.radialGradient(
                colors = listOf(
                    Color.Transparent,
                    scrim.copy(alpha = (if (calm) 0.22f else 0.44f) * strength),
                ),
                center = Offset(size.width / 2, size.height / 2),
                radius = span * 0.92f,
            ),
        )
        // Top/bottom legibility scrim for the pinned title + hint bar.
        drawRect(
            Brush.verticalGradient(
                0.0f to scrim.copy(alpha = 0.40f * strength),
                0.30f to scrim.copy(alpha = 0.05f * strength),
                0.70f to scrim.copy(alpha = 0.06f * strength),
                1.0f to scrim.copy(alpha = 0.42f * strength),
            ),
        )
    }
}

/**
 * `false` when the user has turned animations off system-wide (Developer options' animator duration
 * scale, or the accessibility "Remove animations" switch, which sets the same global). Read once
 * per composition — it needs a settings trip to the system, and it changes about never.
 */
@Composable
private fun animationsEnabled(): Boolean {
    val context = LocalContext.current
    return remember {
        runCatching {
            android.provider.Settings.Global.getFloat(
                context.contentResolver,
                android.provider.Settings.Global.ANIMATOR_DURATION_SCALE,
                1f,
            ) != 0f
        }.getOrDefault(true)
    }
}

/**
 * The backdrop for the console FORM screens (settings, add-host). It used to be a STILL deep-indigo
 * base with two soft glows; it is now the launcher's own living field at `calm`, which keeps that
 * colour and luminance under the glass rows, honours the palette setting on every screen rather
 * than only the launcher, and leaves nothing in the console UI backed by a static image. Mirrors
 * the Apple client's GamepadFormBackground, which made the same substitution.
 */
@Composable
fun GamepadFormBackground(modifier: Modifier = Modifier) {
    GamepadAuroraBackground(modifier, calm = true)
}

/**
 * The horizontal section switcher above a console list. Purely presentational — the SCREEN owns
 * which tab is selected and what the shoulders do. Scrollable so a narrow phone in landscape never
 * has to squeeze the pills, and the selected one is always brought into view whether it was reached
 * by shoulder button or tap.
 */
@Composable
fun ConsoleTabStrip(
    titles: List<String>,
    selected: Int,
    onSelect: (Int) -> Unit,
    modifier: Modifier = Modifier,
    /**
     * The strip itself holds the cursor (the caller moved focus UP out of its list). Draws a ring
     * on the selected pill so it's clear left/right now walks sections rather than values — the
     * route a D-pad remote, which has no shoulder buttons, needs.
     */
    focused: Boolean = false,
) {
    val ink = LocalGamepadInk.current
    val listState = rememberLazyListState()
    LaunchedEffect(selected) {
        runCatching { listState.animateScrollToItem(selected.coerceAtLeast(0)) }
    }
    LazyRow(
        state = listState,
        modifier = modifier,
        contentPadding = PaddingValues(horizontal = ConsoleEdgeInset),
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        itemsIndexed(titles) { i, title ->
            val active = i == selected
            val background by animateColorAsState(
                if (active) ink.accent(0.85f) else ink.glass,
                tween(180),
                label = "tabBg",
            )
            // Not `ink` — that name is the palette's, and shadowing it here cost a compile.
            val labelColor by animateColorAsState(
                if (active) ink.onAccent else ink.fg(0.55f),
                tween(180),
                label = "tabInk",
            )
            val ring by animateColorAsState(
                ink.fg(if (active && focused) 0.85f else 0f),
                tween(180),
                label = "tabRing",
            )
            Text(
                title,
                style = MaterialTheme.typography.labelLarge,
                fontWeight = FontWeight.SemiBold,
                color = labelColor,
                maxLines = 1,
                modifier = Modifier
                    .clip(RoundedCornerShape(50))
                    .background(background)
                    .border(1.5.dp, ring, RoundedCornerShape(50))
                    .clickable { onSelect(i) }
                    .padding(horizontal = 14.dp, vertical = 7.dp),
            )
        }
    }
}

/**
 * The exact inset every console screen places its floating legend at (bottom-start), so the legend
 * sits in the SAME spot across Home / Settings / Add-Host and appears pinned while the content behind
 * it cross-fades between screens.
 */
val ConsoleLegendInset = PaddingValues(start = 24.dp, end = 24.dp, bottom = 24.dp)

/** The shared horizontal inset for a console screen's heading (matches the legend's left edge). */
val ConsoleEdgeInset = 24.dp

/**
 * The heading every console screen uses — one style, one inset, so titles line up across Home /
 * Settings / Add-Host / Library. Callers place it at the top of their content (or float it, on Home).
 */
@Composable
fun ConsoleHeader(title: String, modifier: Modifier = Modifier, horizontalInset: Boolean = true) {
    val ink = LocalGamepadInk.current
    // `horizontalInset = false` when the caller's container already pads to ConsoleEdgeInset (e.g. a
    // LazyColumn contentPadding) — so the heading lands at the SAME 24dp on every screen either way.
    val h = if (horizontalInset) ConsoleEdgeInset else 0.dp
    Text(
        title,
        style = MaterialTheme.typography.headlineMedium,
        fontWeight = FontWeight.Bold,
        color = ink.fg,
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
        modifier = modifier.padding(start = h, end = h, top = 18.dp, bottom = 10.dp),
    )
}

/**
 * One glyph + label cell of a hint bar. [glyph] is the SEMANTIC face letter (the Android
 * `KEYCODE_BUTTON_*` name — 'A' = confirm/south); [color] its Xbox-convention hue. How the pair is
 * actually DRAWN is the hint bar's decision, per the driving controller's [Gamepad.PadStyle] — a
 * DualSense renders 'A' as the ✕ shape, a Switch pad as a monochrome letter. [onClick], when set,
 * makes the cell tappable — a TOUCH escape hatch so a user without a working controller can still
 * drive the console UI (and reach Settings to switch it off).
 */
class GamepadHint(
    val glyph: Char,
    val color: Color,
    val text: String,
    val onClick: (() -> Unit)? = null,
    // Render as the D-pad-centre "select" button (a ring) instead of a lettered face-button disc —
    // for a TV remote, which has no A/B/X/Y.
    val select: Boolean = false,
    // Render as the pad's physical Select/View/Create/− button (per PadStyle) — the button that
    // delivers KEYCODE_BUTTON_SELECT.
    val viewButton: Boolean = false,
)

/**
 * Xbox-convention face-button colours, so the glyphs read at a glance across the room. These are
 * the DEFAULT (Xbox/generic) rendering; the hint bar swaps in PlayStation shapes or Nintendo
 * monochrome per the driving pad's [Gamepad.PadStyle] at draw time.
 */
object PadGlyph {
    val A = Color(0xFF6BBE45)
    val B = Color(0xFFD14B4B)
    val X = Color(0xFF4B7BD1)
    val Y = Color(0xFFE0B23C)
    fun hint(glyph: Char, text: String, onClick: (() -> Unit)? = null) = GamepadHint(
        glyph, when (glyph) { 'A' -> A; 'B' -> B; 'X' -> X; 'Y' -> Y; else -> Color(0xFF9A93C7) }, text, onClick,
    )
}

/** The dark button-face fill shared by the PlayStation / Nintendo / select-button badges. */
internal val PadButtonFace = Color(0xFF2A2740)

/** The animated focus visuals of one console row/field/button — see [animateConsoleFocus]. */
class ConsoleFocusVisuals(val scale: Float, val background: Color, val border: Color)

/**
 * The focus visuals every console form element shares (settings rows, add-host fields, action
 * rows), ANIMATED: the background/border cross-fade instead of snapping between the focused and
 * resting looks, and the scale pops on a soft spring. [editing] draws the brighter violet border
 * of a field actively receiving keyboard input.
 */
@Composable
fun animateConsoleFocus(active: Boolean, editing: Boolean = false): ConsoleFocusVisuals {
    val ink = LocalGamepadInk.current
    val scale by animateFloatAsState(
        targetValue = if (active) 1f else 0.98f,
        animationSpec = spring(dampingRatio = 0.7f, stiffness = Spring.StiffnessMediumLow),
        label = "consoleScale",
    )
    val background by animateColorAsState(
        if (active) ink.accent(0.20f) else ink.glass,
        tween(160),
        label = "consoleBg",
    )
    val border by animateColorAsState(
        when {
            editing -> ink.accent(0.70f)
            active -> ink.fg(0.28f)
            else -> ink.fg(0.06f)
        },
        tween(160),
        label = "consoleBorder",
    )
    return ConsoleFocusVisuals(scale, background, border)
}

/**
 * The console-styled switch a toggle row renders in place of an "On"/"Off" value: a brand-violet
 * track that tints as it engages while the knob slides across on a spring — the state change reads
 * from across the room, and the motion confirms the press.
 */
@Composable
fun ConsoleSwitch(on: Boolean, focused: Boolean, modifier: Modifier = Modifier) {
    val ink = LocalGamepadInk.current
    val travel by animateFloatAsState(
        targetValue = if (on) 1f else 0f,
        animationSpec = spring(dampingRatio = 0.8f, stiffness = 600f),
        label = "switchKnob",
    )
    val track by animateColorAsState(
        if (on) ink.accent else Color(0x26FFFFFF),
        tween(200),
        label = "switchTrack",
    )
    val outline by animateColorAsState(
        ink.fg(if (focused) 0.45f else 0.15f),
        tween(160),
        label = "switchOutline",
    )
    val trackW = 44.dp
    val trackH = 24.dp
    val pad = 3.dp
    val knob = trackH - pad * 2
    Box(
        modifier
            .size(trackW, trackH)
            .clip(RoundedCornerShape(50))
            .background(track)
            .border(1.dp, outline, RoundedCornerShape(50)),
        contentAlignment = Alignment.CenterStart,
    ) {
        Box(
            Modifier
                .padding(horizontal = pad)
                .offset { IntOffset(((trackW - knob - pad * 2).toPx() * travel).roundToInt(), 0) }
                .size(knob)
                .clip(CircleShape)
                .background(ink.fg),
        )
    }
}

/** A round face-button badge: a coloured disc with the button letter, like a controller's face. */
@Composable
fun GamepadButtonGlyph(glyph: Char, color: Color, size: androidx.compose.ui.unit.Dp = 26.dp) {
    val ink = LocalGamepadInk.current
    Box(
        modifier = Modifier
            .size(size)
            .clip(CircleShape)
            .background(color),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            glyph.toString(),
            color = ink.fg,
            fontWeight = FontWeight.Bold,
            fontSize = (size.value * 0.52f).sp,
            textAlign = TextAlign.Center,
        )
    }
}

/** The D-pad-centre "select" button — a green (confirm) disc with a ring; the TV-remote glyph for A. */
@Composable
private fun SelectGlyph(size: androidx.compose.ui.unit.Dp = 26.dp) {
    val ink = LocalGamepadInk.current
    Box(
        modifier = Modifier.size(size).clip(CircleShape).background(PadGlyph.A),
        contentAlignment = Alignment.Center,
    ) {
        Box(Modifier.size(size * 0.46f).clip(CircleShape).border(2.dp, ink.fg, CircleShape))
    }
}

/** The remote's "Back" button — a back-arrow disc; the TV-remote glyph for B (back / cancel / done). */
@Composable
private fun BackGlyph(size: androidx.compose.ui.unit.Dp = 26.dp) {
    GamepadButtonGlyph('↩', PadGlyph.B, size)
}

/**
 * A PlayStation face button: the dark button face with the coloured shape outline Sony prints on it.
 * Keyed by the SEMANTIC letter (Android keycode name): A = ✕ cross, B = ○ circle, X = □ square,
 * Y = △ triangle — exactly how a Sony pad's buttons map to `KEYCODE_BUTTON_*`, in the classic
 * DualShock colours.
 */
@Composable
internal fun PsFaceGlyph(glyph: Char, size: androidx.compose.ui.unit.Dp = 26.dp) {
    val color = when (glyph) {
        'A' -> Color(0xFF7C9CE8) // cross — light blue
        'B' -> Color(0xFFE0736F) // circle — red
        'X' -> Color(0xFFD48FC7) // square — pink
        else -> Color(0xFF5FBFA5) // triangle — green
    }
    Box(
        Modifier.size(size).clip(CircleShape).background(PadButtonFace),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(Modifier.size(size * 0.46f)) {
            val w = this.size.minDimension
            val stroke = Stroke(width = w * 0.17f, cap = StrokeCap.Round, join = StrokeJoin.Round)
            when (glyph) {
                'A' -> { // ✕ — the two diagonals
                    drawLine(color, Offset(0f, 0f), Offset(w, w), stroke.width, StrokeCap.Round)
                    drawLine(color, Offset(w, 0f), Offset(0f, w), stroke.width, StrokeCap.Round)
                }
                'B' -> drawCircle(color, radius = (w - stroke.width) / 2f, style = stroke)
                'X' -> drawRect(
                    color,
                    topLeft = Offset(stroke.width / 2f, stroke.width / 2f),
                    size = Size(w - stroke.width, w - stroke.width),
                    style = stroke,
                )
                else -> { // △
                    val p = Path().apply {
                        moveTo(w / 2f, stroke.width / 2f)
                        lineTo(w - stroke.width / 2f, w - stroke.width / 2f)
                        lineTo(stroke.width / 2f, w - stroke.width / 2f)
                        close()
                    }
                    drawPath(p, color, style = stroke)
                }
            }
        }
    }
}

/**
 * The pad's physical Select-family button — the one that delivers `KEYCODE_BUTTON_SELECT` and opens
 * Options — drawn per [Gamepad.PadStyle] as a badge with the button's real face: Xbox View (two
 * overlapping windows), PlayStation Create/Share (a slim capsule), Nintendo − (minus). The generic
 * fallback wears the capsule too (the near-universal select shape).
 */
@Composable
internal fun SelectButtonGlyph(style: Gamepad.PadStyle, size: androidx.compose.ui.unit.Dp = 26.dp) {
    val ink = LocalGamepadInk.current
    Box(
        Modifier.size(size).clip(CircleShape).background(PadButtonFace),
        contentAlignment = Alignment.Center,
    ) {
        when (style) {
            Gamepad.PadStyle.XBOX -> Box(Modifier.size(size * 0.50f)) {
                // The View icon: two overlapping outlined windows; the front one is filled with the
                // button face so it visibly occludes the back one.
                val corner = RoundedCornerShape(2.dp)
                Box(
                    Modifier.size(size * 0.32f).align(Alignment.TopEnd)
                        .border(1.4.dp, ink.fg(0.9f), corner),
                )
                Box(
                    Modifier.size(size * 0.32f).align(Alignment.BottomStart)
                        .clip(corner).background(PadButtonFace)
                        .border(1.4.dp, ink.fg(0.9f), corner),
                )
            }
            Gamepad.PadStyle.NINTENDO -> Text(
                "−",
                color = ink.fg,
                fontWeight = FontWeight.Bold,
                fontSize = (size.value * 0.62f).sp,
                textAlign = TextAlign.Center,
            )
            else -> Box(
                Modifier
                    .size(width = size * 0.58f, height = size * 0.30f)
                    .clip(RoundedCornerShape(50))
                    .border(1.6.dp, ink.fg(0.9f), RoundedCornerShape(50)),
            )
        }
    }
}

/**
 * The pinned controls legend every gamepad screen shows along the bottom — worn as a self-contained
 * translucent pill so it floats over the aurora rather than dissolving into it.
 */
@Composable
fun GamepadHintBar(hints: List<GamepadHint>, modifier: Modifier = Modifier, hazeState: HazeState? = null) {
    val ink = LocalGamepadInk.current
    // On a TV D-pad remote (no A/B/X/Y), auto-swap the two universal pad glyphs every screen uses:
    // A (confirm) → the select ring, B (back/cancel) → a back glyph. Screen-specific glyphs like the
    // home's Up/Down handle themselves. A real pad instead picks its glyph FAMILY (Xbox letters /
    // PlayStation shapes / Nintendo monochrome) from the controller that last drove the UI.
    // Defaults to the generic gamepad look off an Activity (preview/tests).
    val activity = LocalContext.current as? MainActivity
    val padIsGamepad = activity?.lastPadIsGamepad ?: true
    val padStyle = activity?.lastPadStyle ?: Gamepad.PadStyle.GENERIC
    val shape = RoundedCornerShape(50)
    // With a haze source, blur the content behind the pill (real backdrop blur, API 31+; a translucent
    // scrim below) + a light tint; otherwise fall back to a solid frosted fill.
    val frosted = if (hazeState != null) {
        modifier.clip(shape).hazeEffect(hazeState).background(ink.shade(0.25f))
    } else {
        modifier.clip(shape).background(ink.shade(0.55f))
    }
    Row(
        modifier = frosted
            .border(1.dp, ink.fg(0.14f), shape)
            .padding(horizontal = 16.dp, vertical = 10.dp)
            // The pill still hugs its content when it fits; when it doesn't (a narrow phone, or a
            // screen whose legend grew a cell) it scrolls rather than running off the edge and
            // silently eating the last hint — which is exactly what the settings screen's new
            // Section cell did on a 360 dp phone.
            .horizontalScroll(rememberScrollState()),
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
                    h.viewButton -> SelectButtonGlyph(padStyle)
                    h.select || (!padIsGamepad && h.glyph == 'A') -> SelectGlyph()
                    !padIsGamepad && h.glyph == 'B' -> BackGlyph()
                    padStyle == Gamepad.PadStyle.PLAYSTATION && h.glyph in "ABXY" ->
                        PsFaceGlyph(h.glyph)
                    padStyle == Gamepad.PadStyle.NINTENDO && h.glyph in "ABXY" ->
                        GamepadButtonGlyph(h.glyph, PadButtonFace)
                    else -> GamepadButtonGlyph(h.glyph, h.color)
                }
                Spacer(Modifier.width(6.dp))
                Text(
                    h.text,
                    style = MaterialTheme.typography.labelLarge,
                    color = ink.fg(0.9f),
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
    val ink = LocalGamepadInk.current
    Row(
        modifier = modifier
            .clip(RoundedCornerShape(50))
            .background(ink.fg(0.08f))
            .padding(horizontal = 12.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            Icons.Filled.SportsEsports,
            contentDescription = null,
            tint = ink.fg(0.75f),
            modifier = Modifier.size(16.dp),
        )
        Spacer(Modifier.width(7.dp))
        Text(
            name,
            style = MaterialTheme.typography.labelMedium,
            color = ink.fg(0.75f),
            maxLines = 1,
        )
    }
}
