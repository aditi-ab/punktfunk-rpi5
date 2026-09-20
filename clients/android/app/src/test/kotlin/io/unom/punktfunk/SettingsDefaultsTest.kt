package io.unom.punktfunk

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * "Reduce interface resolution" is the one field whose absent-pref answer depends on the
 * device: a TV's GPU is the weak part, so it ships on there and off on a phone. The seed
 * lives in [SettingsStore]'s fold base, not in `Settings()` — so an explicit stored value
 * still wins in both directions, and a user who never touched the row has nothing persisted.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class SettingsDefaultsTest {

    private val app get() = RuntimeEnvironment.getApplication()

    @Test
    fun anAbsentPrefDefaultsByDeviceClass() {
        assertTrue(SettingsStore(app, tv = true).load().reduceUiResolution)
        assertFalse(SettingsStore(app, tv = false).load().reduceUiResolution)
    }

    @Test
    fun aStoredChoiceWinsOverTheDeviceDefault() {
        val tv = SettingsStore(app, tv = true)
        tv.save(Settings(reduceUiResolution = false))
        assertFalse(tv.load().reduceUiResolution)

        val phone = SettingsStore(app, tv = false)
        phone.save(Settings(reduceUiResolution = true))
        assertTrue(phone.load().reduceUiResolution)
    }
}
