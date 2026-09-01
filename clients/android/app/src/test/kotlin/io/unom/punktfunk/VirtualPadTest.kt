package io.unom.punktfunk

import io.unom.punktfunk.kit.Gamepad
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The virtual controller's pure parts: preset geometry, the D-pad's angle, the stick's travel, the trigger's pull. */
class VirtualPadTest {
    private val sizes = listOf(933f to 420f, 420f to 933f, 1024f to 768f)

    @Test
    fun every_preset_fits_its_layer_with_no_two_controls_overlapping() {
        for (layout in listOf("full", "sticks", "dpad", "")) {
            for ((w, h) in sizes) {
                val ctls = padControls(layout, w, h)
                for (c in ctls) {
                    val r = c.rect
                    assertTrue("$layout ${c.label} inside ${w}x$h: $r", r.x >= 0f && r.y >= 0f && r.x + r.w <= w && r.y + r.h <= h)
                }
                for (i in ctls.indices) for (j in i + 1 until ctls.size) {
                    assertTrue(
                        "$layout ${ctls[i].label} overlaps ${ctls[j].label} at ${w}x$h",
                        !ctls[i].rect.overlaps(ctls[j].rect),
                    )
                }
            }
        }
    }

    @Test
    fun presets_carry_the_controls_the_design_names() {
        fun labels(layout: String) = padControls(layout, 933f, 420f).map { it.label }.toSet()
        val full = labels("full")
        assertEquals(11, full.size)
        assertTrue(full.containsAll(listOf("Left stick", "Right stick", "D-pad", "Face buttons", "Left trigger", "Right bumper", "Start")))
        val sticks = labels("sticks")
        assertTrue(sticks.containsAll(listOf("Left stick", "Right stick", "Left bumper", "Right trigger")))
        assertTrue("D-pad" !in sticks && "Face buttons" !in sticks)
        val dpad = labels("dpad")
        assertTrue(dpad.containsAll(listOf("D-pad", "Face buttons")))
        assertTrue("Left stick" !in dpad && "Left trigger" !in dpad)
        assertEquals(full, labels("bogus"))
    }

    @Test
    fun preset_ids_are_the_schema_ids() {
        assertEquals(
            setOf("lb", "lt", "rb", "rt", "ls", "rs", "dpad", "face", "select", "guide", "start"),
            padControls("full", 933f, 420f).map { it.id }.toSet(),
        )
    }

    @Test
    fun tweaks_move_scale_hide_and_clamp() {
        val base = padControls("full", 933f, 420f)
        val ls = base.first { it.id == "ls" }
        val out = applyPadTweaks(
            base,
            mapOf(
                "ls" to PadTweak(x = 0.5f, y = 0.5f, scale = 2f),
                "start" to PadTweak(scale = 9f), // clamps to the bound
                "face" to PadTweak(hidden = true),
                "select" to PadTweak(x = 0f, y = 0f), // clamps onto the layer
                "nope" to PadTweak(x = 0.9f), // no such control: ignored
            ),
            933f, 420f,
        )
        val moved = out.first { it.id == "ls" }
        assertEquals(2f * ls.rect.w, moved.rect.w, 1e-3f)
        assertEquals(466.5f, moved.rect.x + moved.rect.w / 2, 1e-3f)
        assertEquals(210f, moved.rect.y + moved.rect.h / 2, 1e-3f)
        val start = out.first { it.id == "start" } as PadControl.Buttons
        assertEquals(PAD_TWEAK_SCALE_MAX, start.sc, 0f)
        val baseStart = base.first { it.id == "start" } as PadControl.Buttons
        assertEquals(baseStart.discs[0].r * PAD_TWEAK_SCALE_MAX, start.discs[0].r, 1e-3f)
        assertTrue("hidden stays in the list, marked", out.first { it.id == "face" }.hidden)
        val clamped = out.first { it.id == "select" }.rect
        assertTrue("$clamped stays on the layer", clamped.x >= 0f && clamped.y >= 0f)
        assertEquals(base.size, out.size)
    }

    @Test
    fun a_pad_config_picks_the_layer_class_for_its_overrides() {
        val pad = PadConfig(
            controls = mapOf("face" to PadTweak(hidden = true)),
            controlsNarrow = mapOf("ls" to PadTweak(hidden = true)),
        )
        val wide = padControls(pad, 933f, 420f)
        assertTrue(wide.first { it.id == "face" }.hidden)
        assertTrue(!wide.first { it.id == "ls" }.hidden)
        val narrow = padControls(pad, 420f, 933f)
        assertTrue(narrow.first { it.id == "ls" }.hidden)
        assertTrue(!narrow.first { it.id == "face" }.hidden)
    }

    @Test
    fun dpad_reads_eight_ways_with_a_dead_centre() {
        assertEquals(0, dpadBits(3f, -3f, 10f))
        assertEquals(Gamepad.BTN_DPAD_UP, dpadBits(0f, -40f, 10f))
        assertEquals(Gamepad.BTN_DPAD_DOWN, dpadBits(0f, 40f, 10f))
        assertEquals(Gamepad.BTN_DPAD_LEFT, dpadBits(-40f, 0f, 10f))
        assertEquals(Gamepad.BTN_DPAD_RIGHT, dpadBits(40f, 0f, 10f))
        assertEquals(Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT, dpadBits(30f, -30f, 10f))
        assertEquals(Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_LEFT, dpadBits(-30f, 30f, 10f))
    }

    @Test
    fun stick_is_neutral_inside_the_dead_zone_and_full_at_the_radius() {
        assertEquals(0 to 0, stickWire(0f, 0f, 100f, 6f))
        assertEquals(0 to 0, stickWire(4f, -4f, 100f, 6f))
        assertEquals(32767 to 0, stickWire(100f, 0f, 100f, 6f))
        assertEquals(32767 to 0, stickWire(250f, 0f, 100f, 6f))
        // Screen +y down is wire +y up.
        assertEquals(0 to 32767, stickWire(0f, -100f, 100f, 6f))
        // Half way past the dead zone is half deflection (16383.5 rounds up).
        val (hx, hy) = stickWire(53f, 0f, 100f, 6f)
        assertEquals(16384, hx)
        assertEquals(0, hy)
    }

    @Test
    fun trigger_pulls_from_nothing_at_the_top_to_full_at_the_bottom() {
        assertEquals(0, triggerWire(-5f, 100f))
        assertEquals(0, triggerWire(0f, 100f))
        assertEquals(128, triggerWire(50f, 100f))
        assertEquals(255, triggerWire(100f, 100f))
        assertEquals(255, triggerWire(140f, 100f))
    }
}
