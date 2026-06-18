package io.unom.punktfunk

import android.os.Build
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

// punktfunk brand violets (from the app icon: #6C5BF3 / #A79FF8 / #D2C9FB on a #16132A indigo).
// Used as the fallback dark scheme on pre-Android-12 devices; on 12+ we defer to Material You.
private val BrandDark = darkColorScheme(
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
    MaterialTheme(colorScheme = scheme, content = content)
}
