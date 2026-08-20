package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Android-only console settings ride `trust::Settings::extra`, which is `#[serde(flatten)]`:
 * they are TOP-LEVEL keys of the settings document, beside `width` and `codec`.
 *
 * They were written and read nested under an `"extra"` object instead. Serde put that whole
 * object into the map under the literal key `"extra"`, so no console row ever found
 * `android.gamepad_ui_enabled` — and the value the console saved came back to Kotlin as the one
 * Kotlin had just sent. On glass that was a "Controller-optimized UI" switch you could turn off
 * with nothing happening: the console stayed up, because the setting never moved.
 */
class ConsoleSettingsExtraTest {
    @Test
    fun androidKeysAreWrittenFlat() {
        val j = ConsoleJson.settings(Settings(gamepadUiEnabled = false, lowLatencyMode = false), null)
        assertTrue("the console reads this key at the top level", j.has("android.gamepad_ui_enabled"))
        assertFalse(j.getBoolean("android.gamepad_ui_enabled"))
        assertFalse(j.getBoolean("android.low_latency"))
        assertFalse("a nested wrapper is what serde swallows whole", j.has("extra"))
    }

    /** A store written by the nesting build must not keep echoing its dead wrapper. */
    @Test
    fun aStaleNestedWrapperIsDropped() {
        val base = JSONObject().put(
            "extra",
            JSONObject().put("android.gamepad_ui_enabled", true),
        )
        assertFalse(ConsoleJson.settings(Settings(gamepadUiEnabled = false), base).has("extra"))
    }

    @Test
    fun theConsolesOwnSaveIsReadBack() {
        val saved = JSONObject()
            .put("android.gamepad_ui_enabled", false)
            .put("android.gamepad_ui_mode", GAMEPAD_UI_ALWAYS)
            .put("android.ds_capture", false)
        val next = ConsoleJson.applySettings(Settings(), saved)
        assertFalse("turning the console off must reach the store", next.gamepadUiEnabled)
        assertEquals(GAMEPAD_UI_ALWAYS, next.gamepadUiMode)
        assertFalse(next.dsCapture)
    }

    /** Both halves against each other — the shape only holds if they agree. */
    @Test
    fun theRoundTripKeepsEveryAndroidRow() {
        val want = Settings(
            gamepadUiEnabled = false,
            gamepadUiMode = GAMEPAD_UI_ALWAYS,
            lowLatencyMode = false,
            rumbleOnPhone = true,
            gyroOnPhone = true,
            sc2Capture = false,
            dsCapture = false,
        )
        val got = ConsoleJson.applySettings(Settings(), ConsoleJson.settings(want, null))
        assertEquals(want.gamepadUiEnabled, got.gamepadUiEnabled)
        assertEquals(want.gamepadUiMode, got.gamepadUiMode)
        assertEquals(want.lowLatencyMode, got.lowLatencyMode)
        assertEquals(want.rumbleOnPhone, got.rumbleOnPhone)
        assertEquals(want.gyroOnPhone, got.gyroOnPhone)
        assertEquals(want.sc2Capture, got.sc2Capture)
        assertEquals(want.dsCapture, got.dsCapture)
    }
}
