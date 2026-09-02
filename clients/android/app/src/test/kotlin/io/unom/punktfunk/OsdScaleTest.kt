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
 * The overlay scale is derived, not stored, so what is worth pinning is the derivation: a TV gets
 * [TV_OSD_SCALE] and nothing else does, and [OsdScaled] actually reaches the `dp` inside it. The
 * Apple twin is `OsdScaleTests`.
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

    /** Ratio of the density inside [OsdScaled] to the one outside it, and the fontScale inside. */
    private fun measure(): Pair<Float, Float> {
        var ratio = 0f
        var fontScale = 0f
        compose.setContent {
            val outer = LocalDensity.current
            OsdScaled {
                ratio = LocalDensity.current.density / outer.density
                fontScale = LocalDensity.current.fontScale
            }
        }
        compose.waitForIdle()
        return ratio to fontScale
    }

    @Test
    fun anOrdinaryDeviceDrawsAtItsNativeSize() {
        assertEquals(1f, osdScale(compose.activity), 1e-4f)
        assertEquals(1f, measure().first, 1e-4f)
    }

    @Test
    fun aTvEnlargesTheChrome() {
        beATv()
        assertEquals(TV_OSD_SCALE, osdScale(compose.activity), 1e-4f)
        assertEquals(TV_OSD_SCALE, measure().first, 1e-4f)
    }

    /** The system text size the user chose still applies on top; the scale must not swallow it. */
    @Test
    fun theSystemFontScaleSurvives() {
        beATv()
        assertEquals(compose.activity.resources.configuration.fontScale, measure().second, 1e-4f)
    }
}
