package io.unom.punktfunk

import android.graphics.Bitmap
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.PointerIcon
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs
import kotlin.math.roundToInt

/** Past a round trip: until then the host's hotspot may still trail the mouse's last move. */
private const val SETTLE_MS = 250L

/** Rounding between window and frame pixels; a smaller difference is the same spot. */
private const val SLACK_PX = 2

/** A session cycles through a handful of shapes; this stops a runaway host. */
private const val SHAPE_CACHE_MAX = 64

/** `CURSOR_VISIBLE` in the state's flags. */
private const val VISIBLE = 0x01

/**
 * Whether the host's hotspot ([x], [y]) left the mouse's last spot, with the mouse still for
 * [stillMs]: something other than the mouse moved the host pointer.
 */
internal fun movedElsewhere(x: Int, y: Int, mouseX: Int, mouseY: Int, stillMs: Long): Boolean =
    stillMs >= SETTLE_MS && (abs(x - mouseX) > SLACK_PX || abs(y - mouseY) > SLACK_PX)

/** One host pointer shape in frame pixels. */
internal class HostShape(val bitmap: Bitmap, val hotX: Int, val hotY: Int) {
    private var iconScale = 0f
    private var icon: PointerIcon? = null

    /** This shape at [k] view pixels per frame pixel, the size it has in the picture. Main thread. */
    fun icon(k: Float): PointerIcon {
        icon?.takeIf { iconScale == k }?.let { return it }
        val bmp = if (k == 1f) {
            bitmap
        } else {
            val w = (bitmap.width * k).roundToInt().coerceAtLeast(1)
            val h = (bitmap.height * k).roundToInt().coerceAtLeast(1)
            Bitmap.createScaledBitmap(bitmap, w, h, k != k.roundToInt().toFloat())
        }
        return PointerIcon.create(
            bmp,
            (hotX * k).coerceIn(0f, bmp.width - 1f),
            (hotY * k).coerceIn(0f, bmp.height - 1f),
        ).also {
            icon = it
            iconScale = k
        }
    }
}

/**
 * The host's pointer, worn by the mouse pointer so it moves at input rate instead of with the
 * picture. Runs only when the host forwards its cursor (Desktop mouse mode with a mouse asks).
 *
 * The client draws while the uncaptured mouse drives the host pointer over the picture. When
 * something else moves it — touch, the TV remote, a pad, a host app — or the mouse sits on a
 * bar, the host composites it again, since Android cannot move the mouse pointer to follow.
 * The mouse's next move over the picture takes it back.
 */
internal class HostCursor(
    private val handle: Long,
    /** The mouse is uncaptured and holds the pointer grant. Read on the poll thread. */
    private val mouseFree: () -> Boolean,
    /** Main thread: the shape to wear over the picture; `null` hides the pointer. */
    private val publish: (HostShape?) -> Unit,
) {
    private val main = Handler(Looper.getMainLooper())

    @Volatile private var running = false
    private var thread: Thread? = null

    // Written on the main thread.
    @Volatile private var mouseX = 0
    @Volatile private var mouseY = 0
    @Volatile private var mouseMs = 0L
    @Volatile private var otherMs = 0L

    // The poll thread's: the client draws while the mouse moved after this and [otherMs].
    private var handedBackMs = 0L

    fun start() {
        running = true
        thread = Thread({ pollLoop() }, "pf-cursor").also { it.start() }
    }

    fun stop() {
        running = false
        thread?.join(300) // one pull timeout (100 ms) + slack
        thread = null
    }

    /**
     * The mouse put the host pointer at frame pixel ([x], [y]). Off the picture the host clamps
     * it to the edge, where only the host can draw it. Main thread.
     */
    fun mouseAt(x: Int, y: Int, onPicture: Boolean) {
        if (!onPicture) return handBack()
        mouseX = x
        mouseY = y
        mouseMs = SystemClock.uptimeMillis()
    }

    /** Something other than the mouse moved the host pointer. Main thread. */
    fun handBack() {
        otherMs = SystemClock.uptimeMillis()
    }

    private fun pollLoop() {
        val shapes = HashMap<Int, HostShape>()
        var worn: HostShape? = null
        var shown: HostShape? = null
        var state: IntArray? = null
        var sent = false // the connect already asked the host to composite
        while (running) {
            val st = NativeBridge.nativeNextCursorState(handle)
            while (true) {
                val s = NativeBridge.nativeNextCursorShape(handle) ?: break
                if (shapes.size >= SHAPE_CACHE_MAX) shapes.clear()
                shapeOf(s)?.let { shapes[s[0]] = it }
            }
            val now = SystemClock.uptimeMillis()
            if (!mouseFree()) handedBackMs = now
            if (st != null && sent && st[1] and VISIBLE != 0 &&
                movedElsewhere(st[2], st[3], mouseX, mouseY, now - mouseMs)
            ) {
                handedBackMs = now
            }
            val draws = mouseMs > maxOf(handedBackMs, otherMs)
            if (draws != sent) {
                sent = draws
                NativeBridge.nativeSetCursorRender(handle, draws)
                state = null // what the host sent before the flip is not this mode's
            } else if (draws && st != null) {
                state = st
            }
            // A state names its serial before the shape lands; the previous shape bridges that.
            val want = state?.takeIf { it[1] and VISIBLE != 0 }?.let { shapes[it[0]] ?: worn }
            if (want != null) worn = want
            if (want !== shown) {
                shown = want
                main.post { if (running) publish(want) }
            }
        }
    }

    /** `[serial, w, h, hotX, hotY, pixels…]` → a shape; `null` for an empty one. */
    private fun shapeOf(s: IntArray): HostShape? {
        val (w, h) = s[1] to s[2]
        if (w <= 0 || h <= 0 || s.size < 5 + w * h) return null
        return HostShape(Bitmap.createBitmap(s, 5, w, w, h, Bitmap.Config.ARGB_8888), s[3], s[4])
    }
}
