package io.unom.punktfunk.kit

import android.view.KeyEvent

/**
 * Android `KEYCODE_*` → Windows Virtual-Key code (the punktfunk wire contract; the host maps VK →
 * evdev via `inject::vk_to_evdev`). The Android analogue of the Linux client's evdev→VK table
 * (`punktfunk-client-linux/src/keymap.rs`) and the Apple client's `hidToVK`. Positional/US-layout —
 * we forward the physical key, not the typed character. Unmapped keys → 0 (the Rust side drops them).
 * Extend this alongside `punktfunk-host/src/inject.rs::vk_to_evdev` (emit only VKs the host knows).
 */
object Keymap {
    fun toVk(keyCode: Int): Int = when (keyCode) {
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z -> 0x41 + (keyCode - KeyEvent.KEYCODE_A) // A–Z
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 -> 0x30 + (keyCode - KeyEvent.KEYCODE_0) // 0–9 row
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> 0x70 + (keyCode - KeyEvent.KEYCODE_F1) // F1–F12
        in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 ->
            0x60 + (keyCode - KeyEvent.KEYCODE_NUMPAD_0) // numpad 0–9

        // Whitespace / editing
        KeyEvent.KEYCODE_DEL -> 0x08 // Backspace (Android KEYCODE_DEL == backspace)
        KeyEvent.KEYCODE_TAB -> 0x09
        KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> 0x0D
        KeyEvent.KEYCODE_ESCAPE -> 0x1B
        KeyEvent.KEYCODE_SPACE -> 0x20
        KeyEvent.KEYCODE_CAPS_LOCK -> 0x14
        KeyEvent.KEYCODE_BREAK -> 0x13 // Pause
        KeyEvent.KEYCODE_SYSRQ -> 0x2C // PrintScreen
        KeyEvent.KEYCODE_INSERT -> 0x2D
        KeyEvent.KEYCODE_FORWARD_DEL -> 0x2E // Delete (forward)
        KeyEvent.KEYCODE_NUM_LOCK -> 0x90
        KeyEvent.KEYCODE_SCROLL_LOCK -> 0x91

        // Navigation
        KeyEvent.KEYCODE_PAGE_UP -> 0x21
        KeyEvent.KEYCODE_PAGE_DOWN -> 0x22
        KeyEvent.KEYCODE_MOVE_END -> 0x23
        KeyEvent.KEYCODE_MOVE_HOME -> 0x24
        KeyEvent.KEYCODE_DPAD_LEFT -> 0x25
        KeyEvent.KEYCODE_DPAD_UP -> 0x26
        KeyEvent.KEYCODE_DPAD_RIGHT -> 0x27
        KeyEvent.KEYCODE_DPAD_DOWN -> 0x28

        // Modifiers (L/R-specific VKs; the host folds the generic ones onto the left variant)
        KeyEvent.KEYCODE_SHIFT_LEFT -> 0xA0
        KeyEvent.KEYCODE_SHIFT_RIGHT -> 0xA1
        KeyEvent.KEYCODE_CTRL_LEFT -> 0xA2
        KeyEvent.KEYCODE_CTRL_RIGHT -> 0xA3
        KeyEvent.KEYCODE_ALT_LEFT -> 0xA4
        KeyEvent.KEYCODE_ALT_RIGHT -> 0xA5 // AltGr
        KeyEvent.KEYCODE_META_LEFT -> 0x5B // Super/Win
        KeyEvent.KEYCODE_META_RIGHT -> 0x5C
        KeyEvent.KEYCODE_MENU -> 0x5D // Application

        // Numpad operators
        KeyEvent.KEYCODE_NUMPAD_MULTIPLY -> 0x6A
        KeyEvent.KEYCODE_NUMPAD_ADD -> 0x6B
        KeyEvent.KEYCODE_NUMPAD_SUBTRACT -> 0x6D
        KeyEvent.KEYCODE_NUMPAD_DOT -> 0x6E
        KeyEvent.KEYCODE_NUMPAD_DIVIDE -> 0x6F

        // OEM punctuation (US-layout positional)
        KeyEvent.KEYCODE_SEMICOLON -> 0xBA
        KeyEvent.KEYCODE_EQUALS -> 0xBB
        KeyEvent.KEYCODE_COMMA -> 0xBC
        KeyEvent.KEYCODE_MINUS -> 0xBD
        KeyEvent.KEYCODE_PERIOD -> 0xBE
        KeyEvent.KEYCODE_SLASH -> 0xBF
        KeyEvent.KEYCODE_GRAVE -> 0xC0
        KeyEvent.KEYCODE_LEFT_BRACKET -> 0xDB
        KeyEvent.KEYCODE_BACKSLASH -> 0xDC
        KeyEvent.KEYCODE_RIGHT_BRACKET -> 0xDD
        KeyEvent.KEYCODE_APOSTROPHE -> 0xDE

        else -> 0 // unmapped → Rust drops it
    }
}
