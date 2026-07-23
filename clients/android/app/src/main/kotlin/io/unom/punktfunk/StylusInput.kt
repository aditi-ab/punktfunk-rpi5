package io.unom.punktfunk

import android.view.MotionEvent
import androidx.compose.ui.ExperimentalComposeUiApi
import androidx.compose.ui.input.pointer.PointerEvent
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.unit.IntSize
import io.unom.punktfunk.kit.NativeBridge
import kotlinx.coroutines.delay

// Wire PEN_* state bits (punktfunk_core::quic::pen; mirrored, asserted by the Rust shim's docs).
private const val PEN_IN_RANGE = 1f
private const val PEN_TOUCHING = 2f
private const val PEN_BARREL1 = 4f
private const val PEN_BARREL2 = 8f
private const val STRIDE = 10
private const val MAX_SAMPLES = 8

/**
 * Android stylus → the state-full pen plane (design/pen-tablet-input.md §7): pressure, tilt
 * (`AXIS_TILT`, radians from the surface normal), azimuth (`AXIS_ORIENTATION` — Android's 0 =
 * "pointed away from the user" IS the wire's north, no offset needed), hover with
 * `AXIS_DISTANCE`, the eraser tool, both stylus barrel buttons, and historical (coalesced)
 * samples batched oldest-first for full capture-rate fidelity. Android has no barrel-roll
 * axis — roll stays unknown on this client.
 *
 * Both touch loops call [intercept] first; stylus/eraser pointers are consumed here (against a
 * pen-capable host) and never reach the finger paths, independent of the touch-input mode.
 * [heartbeatLoop] implements the ≤100 ms keepalive wire contract: a stationary held stylus is
 * silent in Android's input pipeline, and the host force-releases a stroke after 200 ms
 * without samples.
 */
internal class StylusStream(private val handle: Long) {
    private var inRange = false
    private var touching = false
    private var sawHover = false
    private val last = FloatArray(STRIDE)
    private val batch = FloatArray(MAX_SAMPLES * STRIDE)

    init {
        idle(last)
    }

    /**
     * Consume the event's stylus pointers into pen samples. Returns true when this event
     * carried any (the caller's finger/gesture handling must then skip those changes).
     */
    @OptIn(ExperimentalComposeUiApi::class)
    fun intercept(ev: PointerEvent, size: IntSize): Boolean {
        val stylusChanges = ev.changes.filter {
            it.type == PointerType.Stylus || it.type == PointerType.Eraser
        }
        if (stylusChanges.isEmpty()) return false
        stylusChanges.forEach { it.consume() }
        val me = ev.motionEvent ?: return true
        if (size.width <= 0 || size.height <= 0) return true
        // At most one stylus exists — find its pointer index by tool type.
        val idx = (0 until me.pointerCount).firstOrNull {
            me.getToolType(it) == MotionEvent.TOOL_TYPE_STYLUS ||
                me.getToolType(it) == MotionEvent.TOOL_TYPE_ERASER
        } ?: return true

        when (me.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN,
            MotionEvent.ACTION_MOVE,
            -> {
                touching = true
                inRange = true
                emitSamples(me, idx, size)
            }
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> {
                sawHover = true
                inRange = true
                touching = false
                emitSamples(me, idx, size)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                touching = false
                // Hover-capable hardware keeps proximity (HOVER_EXIT owns the leave);
                // anything else leaves range on lift — the host never parks a phantom pen.
                inRange = sawHover
                emitSamples(me, idx, size)
            }
            MotionEvent.ACTION_HOVER_EXIT, MotionEvent.ACTION_CANCEL -> release()
            else -> {}
        }
        return true
    }

    /** Session/composition teardown: leave range so the host lifts anything still inked. */
    fun reset() {
        if (inRange || touching) release()
        sawHover = false
    }

    /** The ≤100 ms keepalive (80 ms leaves headroom for one lost datagram). Runs until
     *  cancelled; resends the last state-full sample while the pen is in range. */
    suspend fun heartbeatLoop() {
        try {
            while (true) {
                delay(80)
                if (inRange || touching) {
                    last[9] = 0f // dt
                    NativeBridge.nativeSendPen(handle, last, 1)
                }
            }
        } finally {
            reset()
        }
    }

    private fun release() {
        touching = false
        inRange = false
        last[0] = 0f // state: out of range
        last[4] = 0f // pressure
        NativeBridge.nativeSendPen(handle, last, 1)
    }

    /** Historical (coalesced) samples oldest-first, then the current one — a single batch. */
    private fun emitSamples(me: MotionEvent, idx: Int, size: IntSize) {
        val history = minOf(me.historySize, MAX_SAMPLES - 1)
        var count = 0
        var prevT = if (history > 0) me.getHistoricalEventTime(0) else me.eventTime
        for (h in (me.historySize - history) until me.historySize) {
            val t = me.getHistoricalEventTime(h)
            fill(
                batch, count * STRIDE, size,
                x = me.getHistoricalX(idx, h), y = me.getHistoricalY(idx, h),
                pressure = me.getHistoricalPressure(idx, h),
                tiltRad = me.getHistoricalAxisValue(MotionEvent.AXIS_TILT, idx, h),
                orientRad = me.getHistoricalAxisValue(MotionEvent.AXIS_ORIENTATION, idx, h),
                distance = me.getHistoricalAxisValue(MotionEvent.AXIS_DISTANCE, idx, h),
                buttons = me.buttonState, tool = me.getToolType(idx),
                dtUs = ((t - prevT) * 1000).coerceIn(0, 65535).toFloat(),
            )
            prevT = t
            count++
        }
        fill(
            batch, count * STRIDE, size,
            x = me.getX(idx), y = me.getY(idx), pressure = me.getPressure(idx),
            tiltRad = me.getAxisValue(MotionEvent.AXIS_TILT, idx),
            orientRad = me.getAxisValue(MotionEvent.AXIS_ORIENTATION, idx),
            distance = me.getAxisValue(MotionEvent.AXIS_DISTANCE, idx),
            buttons = me.buttonState, tool = me.getToolType(idx),
            dtUs = ((me.eventTime - prevT) * 1000).coerceIn(0, 65535).toFloat(),
        )
        count++
        batch.copyInto(last, 0, (count - 1) * STRIDE, count * STRIDE)
        NativeBridge.nativeSendPen(handle, batch, count)
    }

    private fun fill(
        out: FloatArray,
        off: Int,
        size: IntSize,
        x: Float,
        y: Float,
        pressure: Float,
        tiltRad: Float,
        orientRad: Float,
        distance: Float,
        buttons: Int,
        tool: Int,
        dtUs: Float,
    ) {
        var state = 0f
        if (inRange || touching) state += PEN_IN_RANGE
        if (touching) state += PEN_TOUCHING
        if (buttons and MotionEvent.BUTTON_STYLUS_PRIMARY != 0) state += PEN_BARREL1
        if (buttons and MotionEvent.BUTTON_STYLUS_SECONDARY != 0) state += PEN_BARREL2
        out[off + 0] = state
        out[off + 1] = if (tool == MotionEvent.TOOL_TYPE_ERASER) 1f else 0f
        out[off + 2] = (x / (size.width - 1).coerceAtLeast(1)).coerceIn(0f, 1f)
        out[off + 3] = (y / (size.height - 1).coerceAtLeast(1)).coerceIn(0f, 1f)
        out[off + 4] = if (touching) pressure.coerceIn(0f, 1f) else 0f
        // AXIS_DISTANCE units are device-arbitrary; 0..1 covers real hardware, and 0 while
        // hovering legitimately means "at the hover floor".
        out[off + 5] = if (touching) 0f else distance.coerceIn(0f, 1f)
        out[off + 6] = Math.toDegrees(tiltRad.toDouble()).toFloat().coerceIn(0f, 90f)
        // AXIS_ORIENTATION: 0 = pointed away from the user (= wire north), clockwise, −π..π.
        out[off + 7] = ((Math.toDegrees(orientRad.toDouble()) + 360.0) % 360.0).toFloat()
        out[off + 8] = -1f // no barrel-roll axis on Android
        out[off + 9] = dtUs
    }

    private fun idle(out: FloatArray) {
        out.fill(0f)
        out[5] = -1f // distance unknown
        out[6] = -1f // tilt unknown
        out[7] = -1f // azimuth unknown
        out[8] = -1f // roll unknown
    }
}
