package io.unom.punktfunk

import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.absoluteOffset
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.changedToDownIgnoreConsumed
import androidx.compose.ui.input.pointer.changedToUpIgnoreConsumed
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChangedIgnoreConsumed
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.Gamepad
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.roundToInt

/*
 * The virtual on-screen controller (design/touch-client-overlay.md §4): a layer of hit regions
 * above the stream's gesture layer and below the ring, driving one wire pad through the same
 * per-pad events a real controller sends. Every control is its own pointer-input node, so a
 * finger that lands between controls never reaches this layer and falls through to the touch
 * mode beneath — tap-to-click keeps working beside the pad. Buttons are hit tests; the D-pad is
 * an eight-way angle; a stick is a per-finger state machine; a trigger reads the finger's
 * position along its travel, so a slow press is a slow press.
 *
 * The layer knows nothing of the wire: [PadSink] is a router `ExternalPad` in-stream, which
 * gives the pad the lowest free wire index, its Arrival before any input, the ring's mask, and
 * its Remove on close — exactly a real controller's lifetime.
 */

/** Where the pad's events go: a router `ExternalPad` in-stream, nothing in a preview. */
class PadSink(val button: (bit: Int, down: Boolean) -> Unit, val axis: (axis: Int, value: Int) -> Unit)

const val PAD_SCALE_MIN = 0.6f
const val PAD_SCALE_MAX = 1.6f
const val PAD_OPACITY_MIN = 0.15f

/** A control's place in the layer, in dp before [PadConfig.scale]; the origin is the top-left. */
internal data class PadRect(val x: Float, val y: Float, val w: Float, val h: Float) {
    fun overlaps(o: PadRect): Boolean = x < o.x + o.w && o.x < x + w && y < o.y + o.h && o.y < y + h
}

/** One disc in a [PadControl.Buttons] group: its centre and radius relative to the group's rect. */
internal data class PadDisc(val label: String, val glyph: String, val bit: Int, val cx: Float, val cy: Float, val r: Float)

internal sealed class PadControl(val label: String, val rect: PadRect) {
    /** Discs that press while a finger is on them; a finger rolling from one to the next presses the next. */
    class Buttons(label: String, rect: PadRect, val discs: List<PadDisc>) : PadControl(label, rect)

    /** Eight directions by angle from the centre, as `BTN_DPAD_*` bits. */
    class Dpad(rect: PadRect) : PadControl("D-pad", rect)

    /** The first finger owns it; its travel from where it landed is the deflection. */
    class Stick(label: String, rect: PadRect, val axisX: Int, val axisY: Int) : PadControl(label, rect)

    /** The finger's position down the pill is the pull: the top is 0, the bottom is full. */
    class Trigger(label: String, rect: PadRect, val axis: Int) : PadControl(label, rect)
}

// Sizes in dp before the scale.
private const val MARGIN = 16f
private const val STICK_R = 60f
private const val STICK_KNOB_R = 26f
/** The node is wider than the base: a thumb landing just outside the ring still takes the stick. */
private const val STICK_HIT = 2 * (STICK_R + 20f)
private const val FACE_R = 24f
private const val FACE_HIT = 152f
private const val DPAD_HIT = 120f
private const val SMALL_R = 18f
private const val BUMPER_R = 22f
private const val TRIGGER_W = 56f
private const val TRIGGER_H = 84f
/** A disc takes a finger this far past its edge, so a thumb need not be exact. */
private const val HIT_SLOP = 1.3f
// ponytail: fixed dead zones; a setting when a device disagrees with these.
private const val STICK_DEAD = 6f
private const val DPAD_DEAD = 14f
/** A layer narrower than this (a phone upright) stacks the clusters instead of spreading them. */
private const val NARROW = 720f

private fun disc(label: String, glyph: String, bit: Int, cx: Float, cy: Float, r: Float) =
    PadControl.Buttons(label, PadRect(cx - r, cy - r, 2 * r, 2 * r), listOf(PadDisc(label, glyph, bit, r, r, r)))

/**
 * The preset's controls for a layer [w] × [h] dp (the container divided by the scale). Positions
 * are fixed per preset (§4.3): sticks in the bottom corners, the D-pad beside the left stick, the
 * face buttons in the bottom-right corner with the right stick beside them, the shoulders in the
 * top corners, Select, Guide and Start along the bottom edge. A narrow layer lifts the D-pad and
 * the right stick above their neighbours and puts the middle three along the top edge instead.
 * An unknown preset is `full`.
 */
internal fun padControls(layout: String, w: Float, h: Float): List<PadControl> {
    val narrow = w < NARROW
    val sticks = layout != "dpad"
    val face = layout != "sticks"
    val bottom = h - MARGIN
    val out = mutableListOf<PadControl>()
    if (sticks) {
        out += disc("Left bumper", "LB", Gamepad.BTN_LB, MARGIN + BUMPER_R, MARGIN + BUMPER_R, BUMPER_R)
        out += PadControl.Trigger("Left trigger", PadRect(MARGIN, MARGIN + 2 * BUMPER_R + 8, TRIGGER_W, TRIGGER_H), Gamepad.AXIS_LT)
        out += disc("Right bumper", "RB", Gamepad.BTN_RB, w - MARGIN - BUMPER_R, MARGIN + BUMPER_R, BUMPER_R)
        out += PadControl.Trigger("Right trigger", PadRect(w - MARGIN - TRIGGER_W, MARGIN + 2 * BUMPER_R + 8, TRIGGER_W, TRIGGER_H), Gamepad.AXIS_RT)
        out += PadControl.Stick("Left stick", PadRect(MARGIN, bottom - STICK_HIT, STICK_HIT, STICK_HIT), Gamepad.AXIS_LS_X, Gamepad.AXIS_LS_Y)
    }
    if (face) {
        val dpad = when {
            !sticks -> PadRect(MARGIN + 20, bottom - 20 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
            narrow -> PadRect(MARGIN + 20, bottom - STICK_HIT - 8 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
            else -> PadRect(MARGIN + STICK_HIT + 10, bottom - 110 - DPAD_HIT, DPAD_HIT, DPAD_HIT)
        }
        out += PadControl.Dpad(dpad)
        val c = FACE_HIT / 2
        val gap = 40f
        out += PadControl.Buttons(
            "Face buttons",
            PadRect(w - MARGIN - FACE_HIT, bottom - FACE_HIT, FACE_HIT, FACE_HIT),
            listOf(
                PadDisc("Y", "Y", Gamepad.BTN_Y, c, c - gap, FACE_R),
                PadDisc("X", "X", Gamepad.BTN_X, c - gap, c, FACE_R),
                PadDisc("B", "B", Gamepad.BTN_B, c + gap, c, FACE_R),
                PadDisc("A", "A", Gamepad.BTN_A, c, c + gap, FACE_R),
            ),
        )
    }
    if (sticks) {
        val right = when {
            !face -> PadRect(w - MARGIN - STICK_HIT, bottom - STICK_HIT, STICK_HIT, STICK_HIT)
            narrow -> PadRect(w - MARGIN - STICK_HIT, bottom - FACE_HIT - 8 - STICK_HIT, STICK_HIT, STICK_HIT)
            else -> PadRect(w - MARGIN - FACE_HIT - 16 - STICK_HIT, bottom - STICK_HIT, STICK_HIT, STICK_HIT)
        }
        out += PadControl.Stick("Right stick", right, Gamepad.AXIS_RS_X, Gamepad.AXIS_RS_Y)
    }
    val midY = if (narrow) MARGIN + SMALL_R else bottom - SMALL_R
    out += disc("Select", "⧉", Gamepad.BTN_BACK, w / 2 - 64, midY, SMALL_R)
    out += disc("Guide", "◎", Gamepad.BTN_GUIDE, w / 2, midY, SMALL_R)
    out += disc("Start", "☰", Gamepad.BTN_START, w / 2 + 64, midY, SMALL_R)
    return out
}

/** The D-pad bits for a finger [dx], [dy] px from the centre (screen +y down): eight ways, none inside [dead]. */
internal fun dpadBits(dx: Float, dy: Float, dead: Float): Int {
    if (hypot(dx, dy) < dead) return 0
    val deg = Math.toDegrees(atan2(-dy, dx).toDouble())
    return when (((deg + 22.5 + 360.0) % 360.0 / 45.0).toInt()) {
        0 -> Gamepad.BTN_DPAD_RIGHT
        1 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT
        2 -> Gamepad.BTN_DPAD_UP
        3 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_LEFT
        4 -> Gamepad.BTN_DPAD_LEFT
        5 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_LEFT
        6 -> Gamepad.BTN_DPAD_DOWN
        else -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_RIGHT
    }
}

/**
 * A stick's wire pair for a travel of [dx], [dy] px from where the finger landed: i16 with +y up,
 * nothing inside [dead], full deflection at [radius] and beyond.
 */
internal fun stickWire(dx: Float, dy: Float, radius: Float, dead: Float): Pair<Int, Int> {
    val mag = hypot(dx, dy)
    if (mag <= dead) return 0 to 0
    val v = ((mag - dead) / (radius - dead)).coerceIn(0f, 1f) * 32767f / mag
    return (dx * v).roundToInt() to (-dy * v).roundToInt()
}

/** A trigger's wire value for a finger [y] px down a pill [h] px tall: 0 at the top, 255 at the bottom. */
internal fun triggerWire(y: Float, h: Float): Int = ((y / h).coerceIn(0f, 1f) * 255f).roundToInt()

/**
 * The controller over the stream. [containerSize] is the whole container in px; the controls
 * anchor to its edges, not to the picture. Composed only while the pad is shown (tenet 1).
 */
@Composable
fun VirtualPadLayer(
    pad: PadConfig,
    containerSize: IntSize,
    sink: PadSink,
    haptics: ConsoleHaptics,
    modifier: Modifier = Modifier,
) {
    if (containerSize == IntSize.Zero) return
    val density = LocalDensity.current.density
    val scale = pad.scale.coerceIn(PAD_SCALE_MIN, PAD_SCALE_MAX)
    val opacity = pad.opacity.coerceIn(PAD_OPACITY_MIN, 1f)
    val px = density * scale
    val controls = remember(pad.layout, containerSize, px) {
        padControls(pad.layout, containerSize.width / px, containerSize.height / px)
    }
    // No pointer input on this box: a finger between the controls passes through to the
    // gesture layer beneath, which is the whole of the fall-through rule.
    Box(modifier.fillMaxSize()) {
        for (c in controls) {
            key(c.label) { PadControlView(c, scale, px, opacity, sink, haptics) }
        }
    }
}

@Composable
private fun PadControlView(
    ctl: PadControl,
    scale: Float,
    px: Float,
    opacity: Float,
    sink: PadSink,
    haptics: ConsoleHaptics,
) {
    val st = remember(ctl, px, sink) {
        when (ctl) {
            is PadControl.Buttons -> ButtonsTouch(ctl, px, sink, haptics)
            is PadControl.Dpad -> DpadTouch(ctl, px, sink, haptics)
            is PadControl.Stick -> StickTouch(ctl, px, sink)
            is PadControl.Trigger -> TriggerTouch(ctl, px, sink, haptics)
        }
    }
    val r = ctl.rect
    Box(
        Modifier
            .absoluteOffset((r.x * scale).dp, (r.y * scale).dp)
            .size((r.w * scale).dp, (r.h * scale).dp)
            .alpha(if (st.active) (opacity + 0.35f).coerceAtMost(1f) else opacity)
            .semantics { contentDescription = ctl.label }
            .pointerInput(st) {
                awaitPointerEventScope {
                    try {
                        while (true) {
                            val ev = awaitPointerEvent()
                            for (ch in ev.changes) {
                                when {
                                    ch.changedToDownIgnoreConsumed() -> st.down(ch.id, ch.position)
                                    ch.changedToUpIgnoreConsumed() -> st.up(ch.id)
                                    ch.pressed && ch.positionChangedIgnoreConsumed() -> st.move(ch.id, ch.position)
                                }
                                ch.consume()
                            }
                        }
                    } finally {
                        // The layer left (hidden, rotated, or the session ended) mid-touch.
                        st.reset()
                    }
                }
            },
    ) {
        when (st) {
            is ButtonsTouch -> Discs(st, scale)
            is DpadTouch -> Cross(st)
            is StickTouch -> StickFace(st, scale)
            is TriggerTouch -> Pill(st, scale)
        }
    }
}

private val FILL = Color.White.copy(alpha = 0.16f)
private val FILL_ON = Color.White.copy(alpha = 0.55f)
private val EDGE = Color.White.copy(alpha = 0.75f)

// ---- per-control finger state: what each does with the fingers it owns ----

private sealed class PadTouch {
    /** Pressed or deflected: the control draws brighter. */
    var active by mutableStateOf(false)
    abstract fun down(id: PointerId, p: Offset)
    abstract fun move(id: PointerId, p: Offset)
    abstract fun up(id: PointerId)
    abstract fun reset()
}

/** Controls that resolve to a set of button bits: the union over every finger, sent on change. */
private sealed class BitsTouch(private val sink: PadSink, private val haptics: ConsoleHaptics) : PadTouch() {
    private val fingers = HashMap<PointerId, Int>()
    var held by mutableIntStateOf(0)
        private set

    abstract fun hit(p: Offset): Int

    override fun down(id: PointerId, p: Offset) { fingers[id] = hit(p); sync() }
    override fun move(id: PointerId, p: Offset) { if (fingers.containsKey(id)) { fingers[id] = hit(p); sync() } }
    override fun up(id: PointerId) { fingers.remove(id); sync() }
    override fun reset() { fingers.clear(); sync() }

    private fun sync() {
        var next = 0
        for (b in fingers.values) next = next or b
        if (next == held) return
        var changed = next xor held
        while (changed != 0) {
            val bit = changed and -changed
            val down = next and bit != 0
            sink.button(bit, down)
            if (down) haptics.tick()
            changed = changed and bit.inv()
        }
        held = next
        active = next != 0
    }
}

private class ButtonsTouch(val c: PadControl.Buttons, private val px: Float, sink: PadSink, haptics: ConsoleHaptics) :
    BitsTouch(sink, haptics) {
    /** The nearest disc the finger is within slop of; 0 between discs. */
    override fun hit(p: Offset): Int {
        var best = 0
        var bestD = Float.MAX_VALUE
        for (d in c.discs) {
            val dist = hypot(p.x - d.cx * px, p.y - d.cy * px)
            if (dist <= d.r * px * HIT_SLOP && dist < bestD) { best = d.bit; bestD = dist }
        }
        return best
    }
}

private class DpadTouch(c: PadControl.Dpad, px: Float, sink: PadSink, haptics: ConsoleHaptics) : BitsTouch(sink, haptics) {
    private val centre = c.rect.w * px / 2
    private val dead = DPAD_DEAD * px
    override fun hit(p: Offset): Int = dpadBits(p.x - centre, p.y - centre, dead)
}

private class StickTouch(private val c: PadControl.Stick, px: Float, private val sink: PadSink) : PadTouch() {
    private var owner: PointerId? = null
    private var origin = Offset.Zero
    private var lastX = 0
    private var lastY = 0
    private val radius = STICK_R * px
    private val dead = STICK_DEAD * px
    /** The knob's offset from the base centre, px. */
    var knob by mutableStateOf(Offset.Zero)
        private set

    override fun down(id: PointerId, p: Offset) {
        if (owner != null) return
        owner = id
        origin = p
        emit(Offset.Zero)
    }
    override fun move(id: PointerId, p: Offset) { if (id == owner) emit(p - origin) }
    override fun up(id: PointerId) { if (id == owner) { owner = null; emit(Offset.Zero) } }
    override fun reset() { owner = null; emit(Offset.Zero) }

    private fun emit(d: Offset) {
        val (x, y) = stickWire(d.x, d.y, radius, dead)
        if (x != lastX) { sink.axis(c.axisX, x); lastX = x }
        if (y != lastY) { sink.axis(c.axisY, y); lastY = y }
        knob = Offset(x / 32767f * radius, -y / 32767f * radius)
        active = owner != null
    }
}

private class TriggerTouch(val c: PadControl.Trigger, px: Float, private val sink: PadSink, private val haptics: ConsoleHaptics) :
    PadTouch() {
    private var owner: PointerId? = null
    private var last = 0
    private val h = c.rect.h * px
    /** 0…1, how far down the pill the finger is. */
    var pull by mutableFloatStateOf(0f)
        private set

    override fun down(id: PointerId, p: Offset) {
        if (owner != null) return
        owner = id
        haptics.tick()
        emit(p.y)
    }
    override fun move(id: PointerId, p: Offset) { if (id == owner) emit(p.y) }
    override fun up(id: PointerId) { if (id == owner) { owner = null; emit(0f) } }
    override fun reset() { owner = null; emit(0f) }

    private fun emit(y: Float) {
        val v = triggerWire(y, h)
        if (v != last) { sink.axis(c.axis, v); last = v }
        pull = v / 255f
        active = v > 0
    }
}

// ---- how each kind draws ----

@Composable
private fun BoxScope.Discs(st: ButtonsTouch, scale: Float) {
    for (d in st.c.discs) {
        val on = st.held and d.bit != 0
        Box(
            Modifier
                .absoluteOffset(((d.cx - d.r) * scale).dp, ((d.cy - d.r) * scale).dp)
                .size((2 * d.r * scale).dp)
                .clip(CircleShape)
                .background(if (on) FILL_ON else FILL)
                .border(1.5.dp, EDGE, CircleShape),
            contentAlignment = Alignment.Center,
        ) {
            Text(d.glyph, color = Color.White, fontSize = (d.r * 0.7f * scale).sp, fontWeight = FontWeight.Bold)
        }
    }
}

@Composable
private fun BoxScope.Cross(st: DpadTouch) {
    Canvas(Modifier.fillMaxSize()) {
        val s = size.minDimension
        val arm = s * 0.34f
        val c = s / 2
        val corner = CornerRadius(arm / 4)
        val vertical = Offset(c - arm / 2, 0f) to Size(arm, s)
        val horizontal = Offset(0f, c - arm / 2) to Size(s, arm)
        drawRoundRect(FILL, vertical.first, vertical.second, corner)
        drawRoundRect(FILL, horizontal.first, horizontal.second, corner)
        val stroke = Stroke(1.5.dp.toPx())
        drawRoundRect(EDGE, vertical.first, vertical.second, corner, style = stroke)
        drawRoundRect(EDGE, horizontal.first, horizontal.second, corner, style = stroke)
        val reach = c - arm / 2
        if (st.held and Gamepad.BTN_DPAD_UP != 0) drawRoundRect(FILL_ON, Offset(c - arm / 2, 0f), Size(arm, reach), corner)
        if (st.held and Gamepad.BTN_DPAD_DOWN != 0) drawRoundRect(FILL_ON, Offset(c - arm / 2, c + arm / 2), Size(arm, reach), corner)
        if (st.held and Gamepad.BTN_DPAD_LEFT != 0) drawRoundRect(FILL_ON, Offset(0f, c - arm / 2), Size(reach, arm), corner)
        if (st.held and Gamepad.BTN_DPAD_RIGHT != 0) drawRoundRect(FILL_ON, Offset(c + arm / 2, c - arm / 2), Size(reach, arm), corner)
    }
}

@Composable
private fun BoxScope.StickFace(st: StickTouch, scale: Float) {
    Box(
        Modifier
            .align(Alignment.Center)
            .size((2 * STICK_R * scale).dp)
            .clip(CircleShape)
            .background(Color.White.copy(alpha = 0.12f))
            .border(1.5.dp, EDGE, CircleShape),
    )
    Box(
        Modifier
            .align(Alignment.Center)
            .absoluteOffset { IntOffset(st.knob.x.roundToInt(), st.knob.y.roundToInt()) }
            .size((2 * STICK_KNOB_R * scale).dp)
            .clip(CircleShape)
            .background(FILL_ON),
    )
}

@Composable
private fun BoxScope.Pill(st: TriggerTouch, scale: Float) {
    val shape = RoundedCornerShape(50)
    Box(
        Modifier
            .fillMaxSize()
            .clip(shape)
            .background(FILL)
            .border(1.5.dp, EDGE, shape),
    ) {
        if (st.pull > 0f) {
            Box(Modifier.align(Alignment.TopCenter).fillMaxWidth().fillMaxHeight(st.pull).background(FILL_ON))
        }
        Text(
            if (st.c.axis == Gamepad.AXIS_LT) "LT" else "RT",
            Modifier.align(Alignment.Center),
            color = Color.White,
            fontSize = (15f * scale).sp,
            fontWeight = FontWeight.Bold,
        )
    }
}
