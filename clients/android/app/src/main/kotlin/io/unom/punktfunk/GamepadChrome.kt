package io.unom.punktfunk

import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.view.InputDevice
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Easing
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.wrapContentWidth
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
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
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.hideFromAccessibility
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.semantics.toggleableState
import androidx.compose.ui.state.ToggleableState
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.deviceBodyVibrator
import androidx.compose.ui.zIndex
import kotlin.math.PI
import kotlin.math.abs
import kotlin.math.cos
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.math.sin
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch

// The console chrome shared by the gamepad-driven screens — the Android mirror of the Apple client's
// GamepadChrome.swift: the motion and shape vocabulary every console screen animates in, the glass
// every row and card is cut from, the bottom button-glyph legend, the section strip, and the
// connected-controller chip. One look and one set of timings across every screen is what makes the
// console UI read as a coherent mode rather than a set of themed pages.
//
// The living backdrop itself lives in GamepadAurora.kt (it grew a shader).

// --- The vocabulary -------------------------------------------------------------------------

/**
 * The console's motion vocabulary: one place for the curves and durations every console screen
 * animates on, so a screen transition, a focus cross-fade and a stepped value read as one system
 * rather than as three components that happen to animate.
 *
 * The push/pop half is the DESKTOP CONSOLE'S CONTRACT, not a local taste call: `pf-console-ui`'s
 * `TRANSITION_S = 0.26` (`shell.rs:29`) and the paint geometry in `shell/render.rs:120-151` —
 * incoming slides up 36 px out of a fade at 0.985→1 scale while the outgoing recedes to 0.96; a pop
 * runs the other way, the leaving screen sliding DOWN and the revealed one growing back from 0.96
 * at 0.4 alpha. The Apple client mirrors the same numbers in `GamepadShell.swift`.
 *
 * ⚠ That makes this the THIRD hand-copy of those constants (Rust, Swift, here). Design WP9 promotes
 * them to shared parity vectors consumed by all three clients' tests; until it lands, a change here
 * is a change owed to the other two.
 */
object ConsoleMotion {
    /**
     * The desktop's `ease_out_cubic` — `1 − (1−t)³`, ANALYTICALLY, not as a Bézier approximation
     * of it (`crates/pf-console-ui/src/anim.rs`).
     *
     * ⚠ Two different curves are published under the name "easeOutCubic" and neither is the real
     * one: the Penner/Ceaser CSS table's `cubic-bezier(0.215, 0.61, 0.355, 1)` and easings.net's
     * `cubic-bezier(0.33, 1, 0.68, 1)`. At the midpoint the true curve is 0.875, the second bezier
     * ≈0.87, and the first ≈0.80 — visibly slacker. Compose's `Easing` is a plain function, so
     * there is no reason to approximate at all; the Apple client uses the second bezier only
     * because SwiftUI's `timingCurve` cannot take a closure.
     */
    val EaseOutCubic = Easing { t ->
        val u = 1f - t
        1f - u * u * u
    }

    /** Screen push/pop, ms — the desktop's `TRANSITION_S` (0.26 s). */
    const val TRANSITION_MS = 260

    /**
     * What push/pop collapses to when the user has asked for less motion: a fast cross-fade with no
     * travel and no scale, the same courtesy the frozen backdrop pays.
     */
    const val REDUCED_MS = 90

    /** How far an incoming screen slides up out of its fade (the desktop's `36.0 * k`). */
    val PUSH_SLIDE = 36.dp

    /** The scale an incoming screen grows from. */
    const val ENTER_SCALE = 0.985f

    /** The scale an outgoing screen recedes to — and the one a revealed screen grows back from. */
    const val EXIT_SCALE = 0.96f

    /** The alpha a revealed (popped-back-to) screen fades up from. */
    const val REVEAL_ALPHA = 0.4f

    /** Focus arriving on a row/field/pill: background, border, chevrons, label colour. */
    const val FOCUS_MS = 160

    /** A stepped value sliding in behind the press; [VALUE_OUT_MS] is the outgoing half. */
    const val VALUE_MS = 180
    const val VALUE_OUT_MS = 140

    /** The settings strip stepping a section — a short directional slide of the row list. */
    const val TAB_MS = 200
    val TAB_SLIDE = 24.dp

    /** A refused press answering anyway: how far the value gives, and how long it takes to spring back. */
    val REFUSAL_NUDGE = 4.dp
    const val REFUSAL_MS = 120

    /** Everything eased on the console's own curve. */
    fun <T> ease(durationMillis: Int = TRANSITION_MS, delayMillis: Int = 0) =
        tween<T>(durationMillis, delayMillis, EaseOutCubic)
}

/**
 * The console's corner radii. These were four separate literals across as many files, which is how
 * a settings row and an add-host field — the same object on two screens — drift apart.
 */
object ConsoleShape {
    /** A settings row, an add-host field, a dialog button, a pin row. */
    val Row = RoundedCornerShape(14.dp)

    /** A pill: the section strip, the legend, a badge. */
    val Pill = RoundedCornerShape(50)

    /** A modal card. */
    val Card = RoundedCornerShape(24.dp)

    /** A launcher tile. */
    val Tile = RoundedCornerShape(26.dp)

    /** A library poster. */
    val Poster = RoundedCornerShape(16.dp)

    /** The on-screen keyboard's frame, and one of its keycaps. */
    val Keyboard = RoundedCornerShape(20.dp)
    val Keycap = RoundedCornerShape(9.dp)

    /** The floating detail band under a settings list. */
    val Band = RoundedCornerShape(14.dp)
}

/**
 * `false` when the user has turned animations off system-wide (Developer options' animator duration
 * scale, or the accessibility "Remove animations" switch, which sets the same global). Read once
 * per composition — it needs a settings trip to the system, and it changes about never.
 */
@Composable
internal fun animationsEnabled(): Boolean {
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

// --- Safe area ------------------------------------------------------------------------------

/**
 * The safe area a console screen's CONTENT keeps clear: the system bars UNION the display cutout.
 *
 * `systemBarsPadding()`, which every console screen used to pad with, EXCLUDES
 * `WindowInsets.displayCutout`. In portrait that mostly hides — a top-centre hole punch sits under
 * the status-bar inset anyway — but in landscape a cutout is a LEFT or RIGHT edge inset with no bar
 * behind it (`cutout=[0,162,0,0]` on the Nothing Phone 3, see the dump quoted in MainActivity), so
 * settings rows and add-host fields ran straight under the camera. Material3's own components lay
 * out against `systemBars.union(displayCutout)`; this is that rule, applied to the console screens,
 * which draw their own chrome instead of sitting in a Scaffold.
 *
 * The BACKDROP deliberately does not take it: the aurora is ambience, and running full-bleed under
 * the camera is exactly what ambience should do.
 */
@Composable
fun Modifier.consoleSafeArea(): Modifier =
    windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.displayCutout))

/**
 * The insets a FLOATING legend takes. In landscape it deliberately ignores the system bars so it
 * hugs the corner rather than the nav-bar inset — but it must still clear the CUTOUT: the console
 * screens are `SENSOR_LANDSCAPE`, so reverse-landscape parks the punch on exactly the corner the
 * legend lives in.
 */
@Composable
fun Modifier.consoleLegendInsets(landscape: Boolean): Modifier =
    if (landscape) windowInsetsPadding(WindowInsets.displayCutout) else consoleSafeArea()

/**
 * The exact inset every console screen places its floating legend at (bottom-start), so the legend
 * sits in the SAME spot across Home / Settings / Add-Host / Library and appears pinned while the
 * content behind it transitions.
 */
val ConsoleLegendInset = PaddingValues(start = 24.dp, end = 24.dp, bottom = 24.dp)

/** The shared horizontal inset for a console screen's heading (matches the legend's left edge). */
val ConsoleEdgeInset = 24.dp

/**
 * How much room a scrolling console list leaves at its bottom for the floating legend ZONE to sit
 * over — the legend pill PLUS the detail band above it (see [ConsoleDetailBand]). One constant so
 * the list's `contentPadding` and the keep-focus-visible scroll margin cannot drift apart; they
 * were 104 dp and a bare `96`, and the scroll target moved under the band the moment it grew.
 */
val ConsoleLegendClearance = 152.dp

// --- Headings and the section strip ----------------------------------------------------------

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
 * The horizontal section switcher above a console list. Purely presentational — the SCREEN owns
 * which tab is selected and what the shoulders do.
 *
 * ONE indicator pill glides between the sections rather than each pill cross-fading its own fill in
 * and out: a shared element that travels says "you moved along a strip", where a pair of fades says
 * "something over there turned off and something here turned on". It is drawn BEHIND the labels
 * (`drawBehind` on the strip's content node), so it costs no layout and can't push the pills around
 * as it moves. Scrollable, so a narrow phone in landscape never has to squeeze them.
 */
@Composable
fun ConsoleTabStrip(
    titles: List<String>,
    selected: Int,
    onSelect: (Int) -> Unit,
    modifier: Modifier = Modifier,
    /**
     * The strip itself holds the cursor (the caller moved focus UP out of its list). Rings the
     * indicator so it's clear left/right now walks sections rather than values — the route a D-pad
     * remote, which has no shoulder buttons, needs.
     */
    focused: Boolean = false,
) {
    val ink = LocalGamepadInk.current
    val scroll = rememberScrollState()
    val animated = animationsEnabled()
    // Pill geometry, measured in ROOT space for both the strip and its pills: the difference is
    // scroll-invariant (they scroll together) and needs no assumptions about which layout node sits
    // between them.
    var stripX by remember { mutableFloatStateOf(0f) }
    val pillX = remember { mutableStateMapOf<Int, Float>() }
    val pillW = remember { mutableStateMapOf<Int, Float>() }
    val target = pillX[selected]?.let { x -> pillW[selected]?.let { w -> x - stripX to w } }

    val indicatorX = remember { Animatable(0f) }
    val indicatorW = remember { Animatable(0f) }
    LaunchedEffect(target, animated) {
        val (x, w) = target ?: return@LaunchedEffect
        // Width 0 = never placed: the first measurement snaps, or the indicator would fly in from
        // the left edge every time the strip is composed.
        if (indicatorW.value == 0f || !animated) {
            indicatorX.snapTo(x)
            indicatorW.snapTo(w)
        } else {
            val spec = ConsoleMotion.ease<Float>(ConsoleMotion.TAB_MS)
            coroutineScope {
                launch { indicatorX.animateTo(x, spec) }
                launch { indicatorW.animateTo(w, spec) }
            }
        }
    }
    // The strip scrolls the selected pill into view whether it was reached by shoulder or by tap.
    LaunchedEffect(selected, target) {
        val (x, w) = target ?: return@LaunchedEffect
        val viewport = scroll.viewportSize.toFloat()
        if (viewport <= 0f) return@LaunchedEffect
        // A pill's own half-width of lead-in, so the neighbour you are stepping toward is visible
        // rather than flush against the edge.
        val lead = (w * 0.5f).coerceAtMost(60f)
        val left = x - lead
        val right = x + w
        val to = when {
            left < scroll.value -> left
            right > scroll.value + viewport -> right - viewport
            else -> return@LaunchedEffect
        }
        runCatching { scroll.animateScrollTo(to.roundToInt().coerceAtLeast(0)) }
    }

    val fill = ink.accent(0.85f)
    val ringAlpha by animateFloatAsState(
        if (focused) 0.85f else 0f,
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "tabRing",
    )
    val ring = ink.fg(ringAlpha)
    Box(modifier.horizontalScroll(scroll)) {
        Box(
            Modifier
                .onGloballyPositioned { stripX = it.positionInRoot().x }
                .drawBehind {
                    val w = indicatorW.value
                    if (w <= 0f) return@drawBehind
                    val r = CornerRadius(size.height / 2f)
                    drawRoundRect(
                        color = fill,
                        topLeft = Offset(indicatorX.value, 0f),
                        size = Size(w, size.height),
                        cornerRadius = r,
                    )
                    if (ring.alpha > 0f) {
                        drawRoundRect(
                            color = ring,
                            topLeft = Offset(indicatorX.value, 0f),
                            size = Size(w, size.height),
                            cornerRadius = r,
                            style = Stroke(width = 1.5.dp.toPx()),
                        )
                    }
                },
        ) {
            Row(
                // The pills are one mutually-exclusive set, and saying so is the only way a screen
                // reader can know it: the SELECTION is drawn as an indicator in the parent's
                // `drawBehind`, which is paint and nothing else — no pill differs from its
                // neighbours in the tree.
                Modifier.padding(horizontal = ConsoleEdgeInset).selectableGroup(),
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                titles.forEachIndexed { i, title ->
                    val active = i == selected
                    // Not `ink` — that name is the palette's, and shadowing it here cost a compile.
                    val labelColor by animateColorAsState(
                        if (active) ink.onAccent else ink.fg(0.55f),
                        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
                        label = "tabInk",
                    )
                    Text(
                        title,
                        style = MaterialTheme.typography.labelLarge,
                        fontWeight = FontWeight.SemiBold,
                        color = labelColor,
                        maxLines = 1,
                        modifier = Modifier
                            .onGloballyPositioned {
                                pillX[i] = it.positionInRoot().x
                                pillW[i] = it.size.width.toFloat()
                            }
                            .clip(ConsoleShape.Pill)
                            // `selectable`, not `clickable`: same ripple and the same click, but it
                            // also publishes Role.Tab + the selected flag, so "Video, tab, selected"
                            // reaches a screen reader that cannot see the indicator behind the pill.
                            .selectable(selected = active, role = Role.Tab) { onSelect(i) }
                            .padding(horizontal = 14.dp, vertical = 7.dp),
                    )
                }
            }
        }
    }
}

// --- Focus, glass, and the surfaces cut from it ------------------------------------------------

/** The animated focus visuals of one console row/field/button — see [animateConsoleFocus]. */
class ConsoleFocusVisuals(
    val scale: Float,
    val background: Color,
    val border: Color,
    /** 0 → 1 as focus arrives; drives the lift and the accent bloom in [consoleGlass]. */
    val focus: Float,
)

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
    val focus by animateFloatAsState(
        targetValue = if (active) 1f else 0f,
        animationSpec = ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "consoleFocus",
    )
    val background by animateColorAsState(
        // 0.28, not the 0.20 it launched with: with the shadow and bloom gone (see consoleGlass),
        // the tint IS the whole focus statement, and Apple's rows carry accent(0.30) for the same
        // reason. Slightly under Apple's because this fill also rides a luminance gradient.
        if (active) ink.accent(0.28f) else ink.glass,
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "consoleBg",
    )
    val border by animateColorAsState(
        when {
            editing -> ink.accent(0.70f)
            active -> ink.fg(0.28f)
            else -> ink.fg(0.06f)
        },
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "consoleBorder",
    )
    return ConsoleFocusVisuals(scale, background, border, focus)
}

/**
 * The console's glass: what every row, field, card and tile is cut from, in one place.
 *
 * Two things separate it from a flat translucent fill:
 *  * a vertical LUMINANCE gradient in the fill, so the pane has a lit top and a settled bottom
 *    instead of one even wash;
 *  * a 1 px top-edge HIGHLIGHT that fades down into the border — the cue that reads as "this is a
 *    physical pane catching the light above it".
 *
 * Focus is the FILL and the BORDER brightening — nothing else, which matches the Apple client's
 * glass (`GlassStyle.swift`: material + an animatable tint, full stop). Two earlier additions were
 * removed after the first on-glass review, and neither may come back:
 *  * a drop shadow. An elevation shadow is drawn UNDER the surface, and this surface is
 *    translucent — the shadow showed straight through the fill as a dark rectangle floating
 *    inside every row and card.
 *  * an accent bloom drawn outside the clip. Unclipped drawing does not stop at the row's
 *    neighbours either: in a list the glow painted over the rows above and below, and in the
 *    carousel it escaped the card entirely.
 */
@Composable
fun Modifier.consoleGlass(
    shape: Shape,
    visuals: ConsoleFocusVisuals,
    /**
     * Draw the edge DASHED instead of solid — the convention for a surface that is offered but not
     * yet yours (a host discovered on the network, the Add-Host tile). Dashes need a real stroke
     * rather than `Modifier.border`, which takes no path effect, so this branch draws the edge
     * itself; the top-edge highlight comes with it either way.
     */
    dashed: Boolean = false,
): Modifier {
    val ink = LocalGamepadInk.current
    val focus = visuals.focus
    val highlight = ink.highlight
    val fill = visuals.background
    val border = visuals.border
    if (dashed) {
        return this
            .graphicsLayer { scaleX = visuals.scale; scaleY = visuals.scale }
            // BEFORE the clip, so the full stroke width shows: drawn inside it, the outer half of
            // the line would be clipped away and the dashes would read as a hairline.
            .drawWithContent {
                drawContent()
                // The console's shapes are all rounded rects; anything else simply gets a square
                // dashed edge rather than no edge at all.
                val r = (shape as? RoundedCornerShape)?.topStart?.toPx(size, this) ?: 0f
                drawRoundRect(
                    brush = Brush.verticalGradient(
                        listOf(highlight.copy(alpha = highlight.alpha * 0.7f), border),
                    ),
                    cornerRadius = CornerRadius(r),
                    style = Stroke(
                        width = 1.dp.toPx(),
                        pathEffect = PathEffect.dashPathEffect(
                            floatArrayOf(6.dp.toPx(), 5.dp.toPx()),
                        ),
                    ),
                )
            }
            .clip(shape)
            .background(
                Brush.verticalGradient(
                    listOf(
                        fill.copy(alpha = (fill.alpha * 1.35f + 0.03f).coerceAtMost(1f)),
                        fill.copy(alpha = fill.alpha * 0.72f),
                    ),
                ),
            )
    }
    return this
        .graphicsLayer { scaleX = visuals.scale; scaleY = visuals.scale }
        .clip(shape)
        .background(
            Brush.verticalGradient(
                listOf(
                    fill.copy(alpha = (fill.alpha * 1.35f + 0.03f).coerceAtMost(1f)),
                    fill.copy(alpha = fill.alpha * 0.72f),
                ),
            ),
        )
        .border(
            width = 1.dp,
            brush = Brush.verticalGradient(
                0f to highlight.copy(alpha = highlight.alpha * (0.55f + 0.45f * focus)),
                0.45f to border,
                1f to border,
            ),
            shape = shape,
        )
}

/**
 * A MODAL card's surface. Unlike [consoleGlass] this one is near-opaque: a dialog's job is to
 * occlude the screen it covers, and it carries body text that has to stay readable over a moving
 * backdrop. It keeps the same lit top edge, so it still belongs to the console's material.
 *
 * The ground is [GamepadInk.card] — palette-derived, which is the fix for a real bug: the cards
 * were a hardcoded near-black indigo while their text came from the palette, so on any of the six
 * PALE palettes a dialog rendered dark ink on a dark card and was unreadable.
 */
@Composable
fun Modifier.consoleCard(): Modifier {
    val ink = LocalGamepadInk.current
    val card = ink.card
    return this
        .clip(ConsoleShape.Card)
        .background(
            Brush.verticalGradient(
                listOf(
                    // Lit from above like every other console surface, but gently — a modal is a
                    // slab, not a pane of glass.
                    card.copy(alpha = (card.alpha * 1.03f).coerceAtMost(1f)),
                    card,
                ),
            ),
        )
        .border(
            width = 1.dp,
            brush = Brush.verticalGradient(
                0f to ink.highlight.copy(alpha = ink.highlight.alpha * 0.8f),
                0.4f to ink.fg(0.12f),
                1f to ink.fg(0.12f),
            ),
            shape = ConsoleShape.Card,
        )
}

/**
 * The scrim + entrance every console modal shares: the backdrop dims, and the card fades up from
 * [ConsoleMotion.EXIT_SCALE] on the console's own curve. Deliberately NOT a spring — an overshoot
 * on a modal reads as bounciness rather than as arrival, which is why the desktop's dialogs don't
 * have one either. Collapses to a plain fast fade under reduce-motion.
 */
@Composable
fun ConsoleModal(content: @Composable () -> Unit) {
    val ink = LocalGamepadInk.current
    val animated = animationsEnabled()
    val enter = remember { Animatable(0f) }
    LaunchedEffect(Unit) {
        enter.animateTo(
            1f,
            ConsoleMotion.ease(if (animated) ConsoleMotion.TRANSITION_MS else ConsoleMotion.REDUCED_MS),
        )
    }
    Box(
        Modifier
            .fillMaxSize()
            .background(ink.modalScrim.copy(alpha = ink.modalScrim.alpha * enter.value)),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            Modifier.graphicsLayer {
                alpha = enter.value
                val s = if (animated) {
                    ConsoleMotion.EXIT_SCALE + (1f - ConsoleMotion.EXIT_SCALE) * enter.value
                } else {
                    1f
                }
                scaleX = s
                scaleY = s
            },
        ) {
            content()
        }
    }
}

/**
 * The frosted band that carries the FOCUSED row's description, floating above a screen's legend
 * pill rather than unfolding inside the row itself.
 *
 * This is the desktop console's reserved detail band (`screens/settings.rs`) achieved by FLOAT
 * instead of by subtraction: the band lives in the screen's bottom-start overlay, so it can never
 * displace the list behind it — which is the whole point. Growing the row instead (what this
 * replaces) shifted every row below the cursor on every single D-pad step, and fought the
 * keep-focus-visible scroll, whose target moved out from under it mid-animation.
 *
 * [key] is what the cross-fade keys on — the focused row's id, so stepping between two rows that
 * happen to share a description doesn't flicker.
 *
 * ⚠ The band is HIDDEN FROM ACCESSIBILITY, and the row carries the same words in its own
 * description instead (see `SettingRowView`). Of the two ways to make a floating band speak — hide
 * it and merge, or leave it and make it a live region — merging wins because the band is a
 * SIGHTED-ONLY relationship: it is a strip in the bottom-left corner whose only tie to the row it
 * describes is that they happen to be on screen together. A screen reader walking the list would
 * meet it as a stray paragraph several nodes away from its subject, and a live region would
 * additionally interrupt the row announcement it duplicates. `hideFromAccessibility` rather than
 * `clearAndSetSemantics` deliberately: the node stays in the semantics tree (where the layout tests
 * assert the description is still rendered), it is only skipped by screen readers.
 */
@Composable
fun ConsoleDetailBand(
    text: String,
    key: Any?,
    modifier: Modifier = Modifier,
    hazeState: HazeState? = null,
) {
    val ink = LocalGamepadInk.current
    AnimatedContent(
        targetState = key to text,
        transitionSpec = {
            fadeIn(ConsoleMotion.ease(ConsoleMotion.FOCUS_MS)) togetherWith
                fadeOut(ConsoleMotion.ease(ConsoleMotion.FOCUS_MS))
        },
        // Silent to a screen reader, deliberately. The band is a place to LOOK — it sits at the
        // far bottom of the screen, describing a row that may be anywhere in the list — so read
        // aloud in layout order it arrives long after, and out of any useful context. The SETTINGS
        // ROW merges this same string into its own announcement instead, which is where a reader
        // is when it matters. Sighted focus and screen-reader focus want the text in different
        // places; this is the one giving each what it needs rather than one of them both.
        modifier = modifier.semantics { hideFromAccessibility() },
        label = "detail",
    ) { (_, body) ->
        if (body.isBlank()) {
            // An empty band still occupies its slot in the AnimatedContent so the pill below it
            // doesn't hop up and down as focus crosses a row with no description.
            Spacer(Modifier.height(0.dp))
        } else {
            Text(
                body,
                style = MaterialTheme.typography.bodySmall,
                color = ink.fg(0.6f),
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier
                    .semantics { hideFromAccessibility() }
                    .widthIn(max = 560.dp)
                    .clip(ConsoleShape.Band)
                    .then(
                        if (hazeState != null) {
                            Modifier.hazeEffect(hazeState).background(ink.shade(0.25f))
                        } else {
                            Modifier.background(ink.shade(0.55f))
                        },
                    )
                    .padding(horizontal = 14.dp, vertical = 9.dp),
            )
        }
    }
}

/**
 * The console-styled switch a toggle row renders in place of an "On"/"Off" value: a brand-violet
 * track that tints as it engages while the knob slides across on a spring — the state change reads
 * from across the room, and the motion confirms the press.
 *
 * The knob SQUASHES as it travels (widest at mid-flight, round at either rest) — the elastic cue
 * that makes a slide read as a thrown object rather than a repositioned dot. Taken from the
 * travel itself rather than from a velocity read, so it can't disagree with where the knob is.
 *
 * Being hand-drawn, it announces nothing on its own — a `Box` with a gradient is not a switch to
 * anything but an eye. The semantics here make it one wherever it is used; a caller that MERGES it
 * into a bigger node (a settings row does) restates them on that node, since `Role` never
 * propagates from a child.
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
        if (on) ink.accent else ink.fg(0.15f),
        ConsoleMotion.ease(200),
        label = "switchTrack",
    )
    val outline by animateColorAsState(
        ink.fg(if (focused) 0.45f else 0.15f),
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "switchOutline",
    )
    val trackW = 44.dp
    val trackH = 24.dp
    val pad = 3.dp
    val knob = trackH - pad * 2
    Box(
        modifier
            // A hand-drawn track and knob are two `Box`es to a screen reader — nothing about them
            // says "switch" or says which way it is thrown. The row that owns this one is the
            // thing that gets pressed (the pad and a tap both act on the ROW), so the switch is
            // not itself toggleable here: it only has to REPORT. See the settings row, which
            // merges this into its own announcement.
            .semantics {
                role = Role.Switch
                toggleableState = if (on) ToggleableState.On else ToggleableState.Off
            }
            .size(trackW, trackH)
            .clip(ConsoleShape.Pill)
            .background(
                Brush.verticalGradient(
                    listOf(track.copy(alpha = track.alpha * 0.82f), track),
                ),
            )
            .border(1.dp, outline, ConsoleShape.Pill),
        contentAlignment = Alignment.CenterStart,
    ) {
        // 1 at either rest, 0 at mid-flight — so the squash peaks exactly where the knob is fastest.
        val settle = abs(travel * 2f - 1f)
        Box(
            Modifier
                .padding(horizontal = pad)
                .offset { IntOffset(((trackW - knob - pad * 2).toPx() * travel).roundToInt(), 0) }
                .graphicsLayer {
                    scaleX = 1f + 0.15f * (1f - settle)
                    scaleY = 1f - 0.07f * (1f - settle)
                }
                .size(knob)
                .clip(CircleShape)
                .background(ink.fg),
        )
    }
}

// --- The option band ---------------------------------------------------------------------------

/**
 * A choice row's value as a REAL band — the Android port of the Apple client's
 * `GamepadOptionBand.swift`, and the fix for "the select field is nowhere near the Apple client":
 * the options sit side by side on a drum segment curving about a vertical axis. The current one
 * faces you flat; a step rotates the next one in with perspective. The drum's position is ONE
 * continuous value driven by a spring, and Compose's `Animatable` retargets with velocity — rapid
 * presses accumulate into one accelerating travel instead of five restarted crossfades.
 *
 * The band is LINEAR, not a ring (the Apple field verdict, inherited whole): a ring showed the
 * first option waiting to the right of the last one, which left/right can't reach — a promise the
 * navigation doesn't keep. Positions are fixed, the ends are the ends, and A's wrap from the last
 * option travels BACK across the list to the first. Neighbours exist only while the drum is
 * actually moving — at rest a row shows exactly its value (a resting neighbour under a long label
 * rendered as overlapping, unreadable text on Apple, and would here too).
 *
 * [width] is FIXED by the row: a step must never reflow the row (the free-width value shifted the
 * chevrons with every label), and the drum needs its stage even when the facing label is short.
 *
 * Purely presentational: stepping semantics (clamp with a boundary thud, A cycles wrapping,
 * disabled rows refuse) stay in the settings screen's row closures.
 */
@Composable
fun ConsoleOptionBand(
    options: List<String>,
    selection: Int,
    focused: Boolean,
    width: Dp,
    modifier: Modifier = Modifier,
) {
    val ink = LocalGamepadInk.current
    val animated = animationsEnabled()
    val color = ink.fg(if (focused) 1f else 0.6f)
    val drum = remember { Animatable(selection.toFloat()) }
    // Where the spring is headed (jumps instantly on a step; only the drum chases it). The
    // distance between them is "how mid-flight are we" — the neighbours exist exactly as long as
    // the drum is moving.
    var target by remember { mutableIntStateOf(selection) }
    LaunchedEffect(selection, options.size, animated) {
        val old = target
        target = selection
        // A step (or A's wrap — on a linear band a fast travel back to the start) springs the
        // drum; anything else (an external write from the touch settings, a re-derived options
        // list) re-seats it — a travel to a value the user didn't step to would read as the UI
        // acting on its own.
        val wrapped = options.size > 1 && old == options.size - 1 && selection == 0
        if (animated && (abs(selection - old) == 1 || wrapped)) {
            drum.animateTo(
                selection.toFloat(),
                // Apple's `.spring(response: 0.32, dampingFraction: 0.78)`; stiffness is
                // (2π / response)² ≈ 385.
                spring(dampingRatio = 0.78f, stiffness = 385f),
            )
        } else {
            drum.snapTo(selection.toFloat())
        }
    }
    Box(
        modifier
            .width(width)
            .clipToBounds()
            // One element to a screen reader — the neighbour texts are rendering, not content.
            .clearAndSetSemantics {
                contentDescription = options.getOrNull(selection).orEmpty()
            },
        contentAlignment = Alignment.Center,
    ) {
        val rotation = drum.value
        val flight = min(1f, abs(rotation - target) * 3f)
        val widthPx = with(LocalDensity.current) { width.toPx() }
        // Puts the ±1 neighbour ~40 % of the band off-centre, curling toward the edge.
        val radius = widthPx * 0.72f
        options.forEachIndexed { i, label ->
            // Plain signed distance — option i has ONE home and nothing waits beyond the ends.
            val d = i - rotation
            // Only the facing option at rest; its neighbours join it for the travel.
            if (abs(d) < 0.5f || (flight > 0.001f && abs(d) <= 2.5f)) {
                val angle = d * DRUM_STEP_RAD
                val depth = cos(angle)
                val x = radius * sin(angle)
                val alpha = max(depth, 0f).let { it * it * it } *
                    (if (abs(d) < 0.5f) 1f else flight) * drumEdgeFade(x, widthPx)
                Text(
                    label,
                    style = MaterialTheme.typography.bodyMedium,
                    color = color,
                    maxLines = 1,
                    softWrap = false,
                    modifier = Modifier
                        // The Apple `fixedSize()`: never let a turning label re-wrap to the
                        // band's width mid-flight — it clips at the band's edge fade instead.
                        .wrapContentWidth(unbounded = true)
                        .zIndex(depth)
                        .graphicsLayer {
                            translationX = x
                            val s = 0.70f + 0.30f * depth
                            scaleX = s
                            scaleY = s
                            // Foreshorten the label as it turns away — what sells the cylinder.
                            rotationY = Math.toDegrees(angle.toDouble()).toFloat()
                            cameraDistance = 6f * density
                            this.alpha = alpha
                        },
                )
            }
        }
    }
}

/** Angular pitch between adjacent options on the drum — Apple's 34°. */
private const val DRUM_STEP_RAD = 34f * (PI.toFloat() / 180f)

/**
 * The soft edge, per option rather than as a container mask: full strength through the middle of
 * the band, dissolving to nothing by an option's rim, so the drum never ends on a hard cut. (The
 * Apple file documents why it must not be a mask: a mask rasterises what it covers, which throws
 * the 3D projection away every frame — the same trap exists in Compose via `graphicsLayer` alpha
 * masking a parent.)
 */
private fun drumEdgeFade(x: Float, width: Float): Float {
    val half = width / 2f
    if (half <= 0f) return 1f
    val fadeStart = half * 0.55f
    if (abs(x) <= fadeStart) return 1f
    return ((half - abs(x)) / (half - fadeStart)).coerceIn(0f, 1f)
}

// --- Menu haptics -----------------------------------------------------------------------------

/**
 * The console's menu feel: a tick as the cursor moves, a heavier thud when a press is refused at a
 * boundary, a pulse on confirm. The Apple client's `MenuHaptics` semantics and the desktop's
 * `menu_rumble(pulse)`, on whatever actuator this device actually has.
 *
 * Constructed by [rememberConsoleHaptics], which resolves the actuator in the order that matches
 * where the player's hands are: the DRIVING controller's own vibrator first, then the phone body
 * (which is what a clip-on pad without motors leaves you holding), and nothing at all on a TV — a
 * remote has no motor and a TV box has no body, so both resolve null and every call is a no-op.
 */
class ConsoleHaptics internal constructor(private val vibrator: Vibrator?) {
    /** The cursor moved one step. */
    fun tick() = play(7, 40)

    /** The press was refused — the list ended, or the value is already at its limit. */
    fun boundary() = play(18, 95)

    /** Something was confirmed, cycled, or flipped. */
    fun confirm() = play(12, 70)

    private fun play(durationMs: Long, amplitude: Int) {
        val v = vibrator ?: return
        runCatching {
            v.vibrate(
                if (v.hasAmplitudeControl()) {
                    VibrationEffect.createOneShot(durationMs, amplitude)
                } else {
                    VibrationEffect.createOneShot(durationMs, VibrationEffect.DEFAULT_AMPLITUDE)
                },
            )
        }
    }
}

/** A haptics object that renders nothing — previews, tests, and every device with no actuator. */
private val SilentHaptics = ConsoleHaptics(null)

/**
 * Resolve the console's haptics for whatever is driving the UI right now. Re-resolves when the
 * driving pad changes (a fresh controller may have motors where the last one had none), and honours
 * the system's own "touch feedback" switch — a user who turned haptics off meant it here too.
 */
@Composable
fun rememberConsoleHaptics(): ConsoleHaptics {
    val context = LocalContext.current
    val activity = context as? MainActivity ?: return SilentHaptics
    val padId = activity.lastPadDeviceId
    @Suppress("DEPRECATION") // still the canonical read for the user's touch-feedback switch
    val enabled = remember {
        runCatching {
            android.provider.Settings.System.getInt(
                context.contentResolver,
                android.provider.Settings.System.HAPTIC_FEEDBACK_ENABLED,
                1,
            ) != 0
        }.getOrDefault(true)
    }
    return remember(padId, enabled) {
        if (!enabled) SilentHaptics
        else ConsoleHaptics(padVibrator(padId) ?: deviceBodyVibrator(context))
    }
}

/** The vibrator of the controller with this device id, or null (no such device / no motors). */
private fun padVibrator(deviceId: Int): Vibrator? = runCatching {
    val dev = InputDevice.getDevice(deviceId) ?: return null
    val v = if (Build.VERSION.SDK_INT >= 31) {
        dev.vibratorManager.defaultVibrator
    } else {
        @Suppress("DEPRECATION")
        dev.vibrator
    }
    v?.takeIf { it.hasVibrator() }
}.getOrNull()

// --- Button glyphs and the legend --------------------------------------------------------------

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

    /** The tint the DIRECTIONAL hints (↔ ⇄ ↑ ↓) wear — not a face button, so not a face colour. */
    val Arrow = Color(0xFF9A93C7)

    fun hint(glyph: Char, text: String, onClick: (() -> Unit)? = null) = GamepadHint(
        glyph, when (glyph) { 'A' -> A; 'B' -> B; 'X' -> X; 'Y' -> Y; else -> Arrow }, text, onClick,
    )
}

/** The dark button-face fill shared by the PlayStation / Nintendo / select-button badges. */
internal val PadButtonFace = Color(0xFF2A2740)

/** A round face-button badge: a coloured disc with the button letter, like a controller's face. */
@Composable
fun GamepadButtonGlyph(glyph: Char, color: Color, size: Dp = 26.dp) {
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
private fun SelectGlyph(size: Dp = 26.dp) {
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
private fun BackGlyph(size: Dp = 26.dp) {
    GamepadButtonGlyph('↩', PadGlyph.B, size)
}

/**
 * A PlayStation face button: the dark button face with the coloured shape outline Sony prints on it.
 * Keyed by the SEMANTIC letter (Android keycode name): A = ✕ cross, B = ○ circle, X = □ square,
 * Y = △ triangle — exactly how a Sony pad's buttons map to `KEYCODE_BUTTON_*`, in the classic
 * DualShock colours.
 */
@Composable
internal fun PsFaceGlyph(glyph: Char, size: Dp = 26.dp) {
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
internal fun SelectButtonGlyph(style: Gamepad.PadStyle, size: Dp = 26.dp) {
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
                    .clip(ConsoleShape.Pill)
                    .border(1.6.dp, ink.fg(0.9f), ConsoleShape.Pill),
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
    val shape = ConsoleShape.Pill
    // With a haze source, blur the content behind the pill (real backdrop blur, API 31+; a translucent
    // scrim below) + a light tint; otherwise fall back to a solid frosted fill.
    val frosted = if (hazeState != null) {
        modifier.clip(shape).hazeEffect(hazeState).background(ink.shade(0.25f))
    } else {
        modifier.clip(shape).background(ink.shade(0.55f))
    }
    Row(
        modifier = frosted
            .border(
                width = 1.dp,
                // The same top-edge highlight the glass rows carry, so the legend belongs to the
                // same material rather than reading as a flat sticker over it.
                brush = Brush.verticalGradient(
                    0f to ink.highlight.copy(alpha = ink.highlight.alpha * 0.7f),
                    0.5f to ink.fg(0.14f),
                    1f to ink.fg(0.14f),
                ),
                shape = shape,
            )
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
                Modifier.clip(ConsoleShape.Pill).clickable(onClick = cb).padding(horizontal = 4.dp, vertical = 5.dp)
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
            .clip(ConsoleShape.Pill)
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
            // The chip yields to the screen title on a narrow phone — see GamepadHome's header row.
            overflow = TextOverflow.Ellipsis,
        )
    }
}
