package io.unom.punktfunk

import io.unom.punktfunk.kit.NativeBridge

/**
 * The wire the touch gestures drive. [NativeTouchSink] is the only production implementation;
 * the seam exists so the gesture machine in `TouchInput.kt` can run on the JVM against a recorder.
 */
internal interface TouchSink {
    /** Relative mouse motion (screen +y down). */
    fun pointerMove(dx: Int, dy: Int)

    /** Absolute cursor position in a [w]×[h] pixel space. */
    fun pointerAbs(x: Int, y: Int, w: Int, h: Int)

    /** One button transition: 1 = left, 2 = middle, 3 = right. */
    fun button(button: Int, down: Boolean)

    /** One normalized scroll step: [axis] 0 = vertical, 1 = horizontal; [delta] signed Q24.8 in
     *  the [source]'s unit (v120 wheel, DIP distance); [source]/[phase] are ScrollWire bytes. */
    fun scroll(axis: Int, delta: Int, source: Int, phase: Int)

    /** One touchscreen contact transition: kind 0 = down, 1 = move, 2 = up. */
    fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int)
}

internal class NativeTouchSink(private val handle: Long) : TouchSink {
    override fun pointerMove(dx: Int, dy: Int) = NativeBridge.nativeSendPointerMove(handle, dx, dy)
    override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) = NativeBridge.nativeSendPointerAbs(handle, x, y, w, h)
    override fun button(button: Int, down: Boolean) = NativeBridge.nativeSendPointerButton(handle, button, down)
    override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) =
        NativeBridge.nativeSendNormalizedScroll(handle, axis, delta, source, phase)
    override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) =
        NativeBridge.nativeSendTouch(handle, id, kind, x, y, w, h)
}
