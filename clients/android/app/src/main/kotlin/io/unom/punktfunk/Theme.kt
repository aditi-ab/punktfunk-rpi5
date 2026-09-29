package io.unom.punktfunk

import android.os.Build
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

// punktfunk brand violets (from the app icon: #6C5BF3 / #A79FF8 / #D2C9FB on a #16132A indigo).
// Used as the fallback dark scheme on pre-Android-12 devices; on 12+ we defer to Material You.
// `internal` (not private) so the CI screenshot tests can force the deterministic brand palette —
// Material You dynamic colour has no wallpaper to seed from under the Robolectric JVM renderer.
internal val BrandDark = darkColorScheme(
    primary = Color(0xFFA79FF8),
    onPrimary = Color(0xFF1B1442),
    primaryContainer = Color(0xFF4C3FB3),
    onPrimaryContainer = Color(0xFFE5E0FF),
    secondary = Color(0xFFC8C2EC),
    onSecondary = Color(0xFF2E2A4D),
    tertiary = Color(0xFF8FD0E8),
    onTertiary = Color(0xFF053543),
    background = Color(0xFF131129),
    onBackground = Color(0xFFE5E1F2),
    surface = Color(0xFF1A1733),
    onSurface = Color(0xFFE5E1F2),
    surfaceVariant = Color(0xFF2A2647),
    onSurfaceVariant = Color(0xFFC7C2DE),
)

/**
 * App theme — always dark (a streaming client reads best on a dark canvas, and the immersive
 * stream view assumes it), but uses **Material You** dynamic colour on Android 12+ so the UI
 * harmonises with the user's wallpaper, falling back to the punktfunk brand violets below that.
 */
@Composable
fun PunktfunkTheme(content: @Composable () -> Unit) {
    val scheme = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        dynamicDarkColorScheme(LocalContext.current)
    } else {
        BrandDark
    }
    // Geist Sans across the whole type scale — the brand typeface the website and the Apple client
    // already ship (see Type.kt).
    MaterialTheme(colorScheme = scheme, typography = PunktfunkTypography, content = content)
}

/** A pill: a badge, a chip. */
val PillShape = RoundedCornerShape(50)

/**
 * The area content keeps clear: the system bars UNION the display cutout. `systemBarsPadding()`
 * leaves out the cutout, which in landscape is a side inset with no bar behind it.
 */
@Composable
fun Modifier.safeArea(): Modifier =
    windowInsetsPadding(WindowInsets.systemBars.union(WindowInsets.displayCutout))
