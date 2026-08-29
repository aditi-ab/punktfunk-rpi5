package io.unom.punktfunk

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/*
 * The in-stream quick-action ring's configuration — the `overlay_actions` setting, one JSON blob
 * (schema v2, design/touch-client-overlay.md §3.2). The Kotlin twin of pf-client-core's
 * `overlay_actions.rs`; the Rust tests are the contract, `OverlayActionsTest` ports them.
 *
 * Parsing never fails: fewer than six slots pad with empty, more are truncated, an unknown id or a
 * dangling `shortcut:` reference is an empty slot, an absent field takes its default, and an
 * unparseable blob is the platform default — profiles sync between client versions.
 */

/** What a ring slot does. [Host] carries a host-advertised action id; [Shortcut] refers into
 *  [OverlayConfig.shortcuts] by id. */
sealed class SlotId {
    object EndStream : SlotId()
    object DisconnectLinger : SlotId()
    object TouchMode : SlotId()
    object Keyboard : SlotId()
    object Stats : SlotId()
    object Mic : SlotId()
    object Pad : SlotId()
    object SendText : SlotId()
    data class Host(val actionId: String) : SlotId()
    data class Shortcut(val shortcutId: String) : SlotId()

    /** The wire id, the inverse of [parse]. */
    val id: String
        get() = when (this) {
            EndStream -> "end_stream"
            DisconnectLinger -> "disconnect_linger"
            TouchMode -> "touch_mode"
            Keyboard -> "keyboard"
            Stats -> "stats"
            Mic -> "mic"
            Pad -> "pad"
            SendText -> "send_text"
            is Host -> "host:$actionId"
            is Shortcut -> "shortcut:$shortcutId"
        }

    companion object {
        /** An id from the blob; `null` for one this build does not know (an empty slot). */
        fun parse(s: String): SlotId? = when (s) {
            "end_stream" -> EndStream
            "disconnect_linger" -> DisconnectLinger
            "touch_mode" -> TouchMode
            "keyboard" -> Keyboard
            "stats" -> Stats
            "mic" -> Mic
            "pad" -> Pad
            "send_text" -> SendText
            else -> when {
                s.startsWith("host:") && s.length > 5 -> Host(s.substring(5))
                s.startsWith("shortcut:") && s.length > 9 -> Shortcut(s.substring(9))
                else -> null
            }
        }
    }
}

/** A custom key chord. [keys] are names from the shared keymap tables, never raw key codes. */
data class Shortcut(val id: String, val label: String = "", val keys: List<String> = emptyList())

/**
 * The Windows virtual-key code a shortcut key name stands for (the wire speaks VKs); `null` for
 * a name this build does not know. Twin of the Rust `key_vk`.
 */
fun keyVk(name: String): Int? {
    val n = name.trim().lowercase()
    return when (n) {
        "ctrl", "control" -> 0x11
        "shift" -> 0x10
        "alt", "option" -> 0x12
        "win", "cmd", "super", "meta" -> 0x5B
        "escape", "esc" -> 0x1B
        "tab" -> 0x09
        "enter", "return" -> 0x0D
        "space" -> 0x20
        "backspace" -> 0x08
        "delete", "del" -> 0x2E
        "insert" -> 0x2D
        "home" -> 0x24
        "end" -> 0x23
        "pageup" -> 0x21
        "pagedown" -> 0x22
        "up" -> 0x26
        "down" -> 0x28
        "left" -> 0x25
        "right" -> 0x27
        "printscreen" -> 0x2C
        "pause" -> 0x13
        "capslock" -> 0x14
        else -> when {
            n.length == 1 && n[0] in 'a'..'z' -> 0x41 + (n[0] - 'a')
            n.length == 1 && n[0] in '0'..'9' -> 0x30 + (n[0] - '0')
            n.length in 2..3 && n[0] == 'f' ->
                n.substring(1).toIntOrNull()?.takeIf { it in 1..24 }?.let { 0x70 + it - 1 }
            else -> null
        }
    }
}

/** A chord as a keycap chip reads it: `Ctrl+⇧+Esc`. */
fun chordChip(keys: List<String>): String = keys.joinToString("+") { k ->
    when (k.lowercase()) {
        "ctrl", "control" -> "Ctrl"
        "shift" -> "⇧"
        "alt", "option" -> "Alt"
        "win", "cmd", "super", "meta" -> "❖"
        "escape", "esc" -> "Esc"
        "enter", "return" -> "↵"
        "backspace" -> "⌫"
        "delete", "del" -> "Del"
        "space" -> "␣"
        "up" -> "↑"
        "down" -> "↓"
        "left" -> "←"
        "right" -> "→"
        else -> k.replaceFirstChar { it.uppercase() }
    }
}

/** The virtual controller's preset: [layout] is `full`, `sticks` or `dpad`. */
data class PadConfig(val layout: String = "full", val opacity: Float = 0.45f, val scale: Float = 1f)

/** Which platform default ring applies. Android is always [TOUCH]; [DESKTOP] exists so the
 *  parser matches its twins exactly. */
enum class RingPlatform { TOUCH, DESKTOP }

data class OverlayConfig(
    /** Exactly [RING_SLOTS] entries, clockwise from 12 o'clock; `null` is an empty slot. */
    val ring: List<SlotId?>,
    val shortcuts: List<Shortcut> = emptyList(),
    val pad: PadConfig = PadConfig(),
) {
    fun shortcut(id: String): Shortcut? = shortcuts.firstOrNull { it.id == id }

    /** The blob to store — always the current schema version. */
    fun toJson(): String {
        val j = JSONObject()
        j.put("v", SCHEMA_VERSION)
        j.put("ring", JSONArray().also { arr -> ring.forEach { arr.put(it?.id ?: JSONObject.NULL) } })
        j.put(
            "shortcuts",
            JSONArray().also { arr ->
                shortcuts.forEach { s ->
                    arr.put(
                        JSONObject().put("id", s.id).put("label", s.label)
                            .put("keys", JSONArray(s.keys)),
                    )
                }
            },
        )
        j.put(
            "pad",
            JSONObject().put("layout", pad.layout).put("opacity", pad.opacity.toDouble())
                .put("scale", pad.scale.toDouble()),
        )
        return j.toString()
    }

    companion object {
        const val RING_SLOTS = 6
        const val SCHEMA_VERSION = 2

        fun platformDefault(platform: RingPlatform = RingPlatform.TOUCH): OverlayConfig = OverlayConfig(
            ring = when (platform) {
                RingPlatform.TOUCH -> listOf(
                    SlotId.EndStream, SlotId.Keyboard, SlotId.TouchMode,
                    SlotId.Stats, SlotId.Mic, SlotId.Pad,
                )
                RingPlatform.DESKTOP -> listOf(
                    SlotId.EndStream, SlotId.DisconnectLinger, SlotId.TouchMode,
                    SlotId.Stats, SlotId.Mic, SlotId.SendText,
                )
            },
        )

        /** Parse the setting; an empty or unparseable blob is the platform default. */
        fun parse(json: String?, platform: RingPlatform = RingPlatform.TOUCH): OverlayConfig {
            if (json.isNullOrBlank()) return platformDefault(platform)
            val j = try {
                JSONObject(json)
            } catch (_: JSONException) {
                return platformDefault(platform)
            }
            val shortcuts = buildList {
                val arr = j.optJSONArray("shortcuts") ?: JSONArray()
                for (i in 0 until arr.length()) {
                    val s = arr.optJSONObject(i) ?: continue
                    val id = s.optString("id", "")
                    if (id.isEmpty()) continue
                    val keys = s.optJSONArray("keys")?.let { k ->
                        (0 until k.length()).map { k.optString(it) }
                    } ?: emptyList()
                    add(Shortcut(id, s.optString("label", ""), keys))
                }
            }
            val ringIn = j.optJSONArray("ring") ?: JSONArray()
            val ring = (0 until RING_SLOTS).map { i ->
                if (i >= ringIn.length() || ringIn.isNull(i)) return@map null
                SlotId.parse(ringIn.optString(i))?.takeIf { slot ->
                    slot !is SlotId.Shortcut || shortcuts.any { it.id == slot.shortcutId }
                }
            }
            val padIn = j.optJSONObject("pad")
            val pad = PadConfig(
                layout = padIn?.optString("layout", "full")?.ifEmpty { "full" } ?: "full",
                opacity = padIn?.optDouble("opacity", 0.45)?.toFloat() ?: 0.45f,
                scale = padIn?.optDouble("scale", 1.0)?.toFloat() ?: 1f,
            )
            return OverlayConfig(ring, shortcuts, pad)
        }
    }
}
