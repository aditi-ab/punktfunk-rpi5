package io.unom.punktfunk

import android.graphics.Bitmap
import android.view.InputDevice
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.View
import androidx.activity.ComponentActivity
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotSame
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [StreamPointer] is the icon Android resolves over the stream — the host's, or none — whatever
 * icon Compose's own view still holds from a hovered text field. `sdk = [36]`: see [OsdScaleTest].
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class StreamPointerTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    @Test
    fun wearsTheHostIconOrHidesUntilTheRingOpens() {
        var hidden by mutableStateOf(true)
        var host by mutableStateOf<HostShape?>(null)
        var scale by mutableStateOf(1f)
        lateinit var composeView: View
        compose.setContent {
            composeView = LocalView.current
            StreamPointer(hidden, host, scale, Modifier.fillMaxSize())
        }
        val ctx = compose.activity
        val arrow = PointerIcon.getSystemIcon(ctx, PointerIcon.TYPE_ARROW)
        lateinit var hover: MotionEvent
        compose.runOnIdle {
            // What a text field's hover exit leaves on Compose's view.
            composeView.pointerIcon = arrow
            hover = MotionEvent.obtain(
                0, 0, MotionEvent.ACTION_HOVER_MOVE, composeView.width / 2f, composeView.height / 2f, 0,
            ).apply { source = InputDevice.SOURCE_MOUSE }
            assertEquals(
                PointerIcon.getSystemIcon(ctx, PointerIcon.TYPE_NULL),
                composeView.onResolvePointerIcon(hover, 0),
            )
        }
        val shape = HostShape(Bitmap.createBitmap(4, 4, Bitmap.Config.ARGB_8888), 1, 1)
        host = shape
        lateinit var atOne: PointerIcon
        compose.runOnIdle {
            atOne = composeView.onResolvePointerIcon(hover, 0)
            assertEquals(shape.icon(1f), atOne)
        }
        // A rotation or resize rescales the worn shape without a new one from the host.
        scale = 2f
        compose.runOnIdle {
            val atTwo = composeView.onResolvePointerIcon(hover, 0)
            assertNotSame(atOne, atTwo)
            assertEquals(shape.icon(2f), atTwo)
        }
        hidden = false
        compose.runOnIdle { assertEquals(arrow, composeView.onResolvePointerIcon(hover, 0)) }
    }
}
