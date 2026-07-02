package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure JVM test of the positional scancode table (`Keymap.evdevToVk`) — no Android runtime types
 * (the `KeyEvent` constants in the keycode table are compile-time-inlined ints). Run:
 * `./gradlew :kit:testDebugUnitTest`.
 */
class KeymapTest {
    /**
     * The German-scramble regression pins: the physical keys a QWERTZ board labels Z/Y/ö/ü/ä/ß
     * must leave this client as their US-position VKs, regardless of the user-selected physical
     * keyboard layout (which remaps `keyCode`, not `scanCode`).
     */
    @Test
    fun positionalPinsForTheQwertzScramble() {
        assertEquals(0x59, Keymap.evdevToVk(21)) // KEY_Y (QWERTZ: Z key) → VK_Y
        assertEquals(0x5A, Keymap.evdevToVk(44)) // KEY_Z (QWERTZ: Y key) → VK_Z
        assertEquals(0xBA, Keymap.evdevToVk(39)) // KEY_SEMICOLON (QWERTZ: ö) → VK_OEM_1
        assertEquals(0xDB, Keymap.evdevToVk(26)) // KEY_LEFTBRACE (QWERTZ: ü) → VK_OEM_4
        assertEquals(0xDE, Keymap.evdevToVk(40)) // KEY_APOSTROPHE (QWERTZ: ä) → VK_OEM_7
        assertEquals(0xBD, Keymap.evdevToVk(12)) // KEY_MINUS (QWERTZ: ß) → VK_OEM_MINUS
    }

    /**
     * Exactly the 48 typing-area keys are covered (10 digits + 26 letters + 12 OEM) with unique
     * VKs; everything else (nav, F-row, modifiers, gamepad buttons at 0x100+) falls through to
     * the keycode table.
     */
    @Test
    fun tableCoversTheTypingAreaBijectively() {
        val mapped = (0..0x200).mapNotNull { sc ->
            Keymap.evdevToVk(sc).takeIf { it != 0 }?.let { sc to it }
        }
        assertEquals(48, mapped.size)
        assertEquals(48, mapped.map { it.second }.toSet().size)
        assertEquals(0, Keymap.evdevToVk(1)) // KEY_ESC — layout-invariant, keycode path
        assertEquals(0, Keymap.evdevToVk(59)) // KEY_F1
        assertEquals(0, Keymap.evdevToVk(304)) // BTN_SOUTH — gamepad, never a typing key
    }
}
