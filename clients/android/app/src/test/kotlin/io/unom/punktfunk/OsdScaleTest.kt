package io.unom.punktfunk

import android.content.pm.PackageManager
import androidx.activity.ComponentActivity
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * The overlay scale is derived, not stored, so what is worth pinning is the derivation: a TV draws
 * at the console's couch scale and nothing else changes, and [OsdScaled] actually reaches the `dp`
 * inside it.
 *
 * `sdk = [36]` for the same reason as the screenshot tests: Robolectric ships android-all jars only
 * up to API 36 while the app's compileSdk is 37.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class OsdScaleTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private fun beATv() {
        val context = compose.activity
        shadowOf(context.packageManager).setSystemFeature(PackageManager.FEATURE_LEANBACK, true)
    }

    /** The density inside [OsdScaled], the one outside it, and the fontScale inside. */
    private fun measure(): Triple<Float, Float, Float> {
        var inner = 0f
        var outer = 0f
        var fontScale = 0f
        compose.setContent {
            outer = LocalDensity.current.density
            OsdScaled {
                inner = LocalDensity.current.density
                fontScale = LocalDensity.current.fontScale
            }
        }
        compose.waitForIdle()
        return Triple(inner, outer, fontScale)
    }

    @Test
    fun anOrdinaryDeviceDrawsAtItsNativeSize() {
        val (inner, outer, _) = measure()
        assertEquals(outer, inner, 1e-4f)
    }

    @Test
    fun aTvDrawsAtTheConsolesCouchScale() {
        beATv()
        val m = compose.activity.resources.displayMetrics
        val (inner, _, _) = measure()
        assertEquals(minOf(m.widthPixels, m.heightPixels) / COUCH_HEIGHT, inner, 1e-4f)
    }

    /** The system text size the user chose still applies on top; the scale must not swallow it. */
    @Test
    fun theSystemFontScaleSurvives() {
        beATv()
        assertEquals(compose.activity.resources.configuration.fontScale, measure().third, 1e-4f)
    }
}
