package io.unom.punktfunk

import android.content.Context
import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.isToggleable
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.core.app.ApplicationProvider
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.GraphicsMode
import org.robolectric.annotation.Config

/**
 * An edit must land in the scope the chips say is selected — the one thing the two-layer settings
 * surface can get wrong without looking wrong.
 *
 * The regression this pins: `update` reached the rows as `::update`, and two callable references
 * compare EQUAL however different the scope they captured, so Compose skipped the whole detail page
 * on a scope switch that moved nothing on screen (the ordinary case — a profile inherits the globals
 * until it overrides something). Each edit then wrote to the scope the user had just left: change a
 * default, switch to a profile, change the same row — the globals moved again and the profile
 * recorded nothing — and back on the defaults the next edit went into the profile, which reads as
 * "the default settings can't be changed any more". It needs the real Compose runtime to catch, so
 * this drives the actual screen rather than the model underneath it.
 *
 * `sdk = [36]` for the reason every Robolectric test here pins it: android-all jars stop at 36 while
 * the app compiles against 37.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class SettingsScopeTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val context: Context get() = ApplicationProvider.getApplicationContext()

    /**
     * "Invert scroll direction" is the row under test: Input is profileable end to end, and that
     * toggle is the only toggleable node on the page, so a click needs no fragile lookup.
     */
    private fun toggleTheRow() {
        compose.onNode(isToggleable()).performClick()
        compose.waitForIdle()
    }

    private fun selectScope(chip: String) {
        compose.onNodeWithText(chip).performClick()
        compose.waitForIdle()
    }

    @Test
    fun editsFollowTheSelectedScope() {
        val profiles = ProfileStore(context)
        profiles.save(newProfile("Work", PROFILE_ACCENTS.first()))

        // Mirrors App.kt: the screen is fed from state the host recomposes it with.
        var saved = Settings()
        compose.setContent {
            var settings by remember { mutableStateOf(saved) }
            SettingsScreen(
                initial = settings,
                onChange = { settings = it; saved = it },
                onBack = {},
                initialCategory = SettingsCategory.Input,
            )
        }

        // 1. On the defaults, the globals move and no profile records anything.
        toggleTheRow()
        assertEquals(true, saved.invertScroll)
        assertNull(profiles.all().single().overrides.invertScroll)

        // 2. In profile scope the SAME row — untouched by the profile, so it still shows the global
        //    value and nothing on the page changed — must record an override and leave the globals
        //    alone. This is the step that used to write straight through to the globals.
        selectScope("Work")
        toggleTheRow()
        assertEquals("the globals must not move while a profile is selected", true, saved.invertScroll)
        assertEquals(false, profiles.all().single().overrides.invertScroll)

        // 3. Back on the defaults the row is editable again, and the profile keeps its override.
        selectScope("Default settings")
        toggleTheRow()
        assertEquals(false, saved.invertScroll)
        assertEquals(
            "the profile's override must survive an edit made on the defaults",
            false,
            profiles.all().single().overrides.invertScroll,
        )
    }

    /** A reset puts the row back to inheriting — and, like an edit, it must obey the live scope. */
    @Test
    fun resetClearsTheSelectedProfilesOverride() {
        val profiles = ProfileStore(context)
        profiles.save(newProfile("Work", PROFILE_ACCENTS.first()))

        compose.setContent {
            var settings by remember { mutableStateOf(Settings()) }
            SettingsScreen(
                initial = settings,
                onChange = { settings = it },
                onBack = {},
                initialCategory = SettingsCategory.Input,
                initialProfileId = profiles.all().single().id,
            )
        }

        toggleTheRow()
        assertEquals(true, profiles.all().single().overrides.invertScroll)

        compose.onNodeWithText("Reset").performClick()
        compose.waitForIdle()
        assertNull(profiles.all().single().overrides.invertScroll)
    }
}
