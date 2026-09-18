package io.unom.punktfunk

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.performTouchInput
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The stream's touch vocabulary, driven through Compose's pointer injection on the JVM and read
 * back off a recording [TouchSink]. Each test is one gesture and the exact wire it must produce —
 * a stuck button or a phantom click here is the bug nobody catches for a release.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class TouchInputTest {
    @get:Rule
    val compose = createComposeRule()

    /** Every call, in order, as a readable line. */
    private class Recorder : TouchSink {
        val log = mutableListOf<String>()
        override fun pointerMove(dx: Int, dy: Int) { log += "move $dx $dy" }
        override fun pointerAbs(x: Int, y: Int, w: Int, h: Int) { log += "abs $x $y" }
        override fun button(button: Int, down: Boolean) { log += "btn $button ${if (down) "down" else "up"}" }
        override fun scroll(axis: Int, delta: Int, source: Int, phase: Int) {
            log += "scroll $axis $delta s$source p$phase"
        }
        override fun touch(id: Int, kind: Int, x: Int, y: Int, w: Int, h: Int) { log += "touch $id $kind" }
        fun buttons() = log.filter { it.startsWith("btn") }
        fun scrolls() = log.filter { it.startsWith("scroll") }
        fun moves() = log.filter { it.startsWith("move") }
    }

    private val sink = Recorder()
    private var stats = 0
    private val keyboard = mutableListOf<Boolean>()
    private val dial = mutableListOf<DialEvent>()

    private fun compose(trackpad: Boolean = true) {
        compose.mainClock.autoAdvance = false
        compose.setContent {
            Box(
                Modifier.fillMaxSize().testTag("surface").pointerInput(Unit) {
                    streamTouchInput(
                        sink = sink, stylus = null,
                        video = { VideoFrame(io.unom.punktfunk.kit.VideoFit.FIT, 0, 0) },
                        trackpad = trackpad,
                        onCycleStats = { stats++ }, onKeyboard = { keyboard += it }, onDial = { dial += it },
                    )
                },
            )
        }
    }

    private fun settle() = compose.mainClock.advanceTimeBy(100)

    @Test
    fun tapIsALeftClickWithNoMotion() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput { down(center); advanceEventTime(50); up() }
        settle()
        assertEquals(listOf("btn 1 down", "btn 1 up"), sink.log)
    }

    @Test
    fun directModeJumpsTheCursorBeforeTheClick() {
        compose(trackpad = false)
        compose.onNodeWithTag("surface").performTouchInput { down(center); advanceEventTime(50); up() }
        settle()
        assertTrue(sink.log.first().startsWith("abs "))
        assertEquals(listOf("btn 1 down", "btn 1 up"), sink.buttons())
    }

    @Test
    fun twoFingerTapIsARightClick() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(0, center); down(1, center + Offset(80f, 0f)); advanceEventTime(50); up(0); up(1)
        }
        settle()
        assertEquals(listOf("btn 3 down", "btn 3 up"), sink.log)
    }

    @Test
    fun threeFingerTapCyclesTheStatsAndSendsNothing() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(0, center); down(1, center + Offset(80f, 0f)); down(2, center + Offset(160f, 0f))
            advanceEventTime(50); up(0); up(1); up(2)
        }
        settle()
        assertEquals(1, stats)
        assertEquals(emptyList<String>(), sink.log)
    }

    @Test
    fun trackpadDragMovesTheCursorAndNeverClicks() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(center)
            repeat(5) { advanceEventTime(16); moveBy(Offset(20f, 0f)) }
            up()
        }
        settle()
        assertTrue(sink.moves().isNotEmpty())
        assertTrue(sink.moves().all { it.split(" ")[1].toInt() > 0 && it.split(" ")[2].toInt() == 0 })
        assertEquals(emptyList<String>(), sink.buttons())
    }

    @Test
    fun twoFingerPanScrollsAndNeverClicks() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(0, center); down(1, center + Offset(80f, 0f))
            // Both fingers in ONE event, as a hand moves: one finger per event rotates the pair
            // and reads, correctly, as the dial twist.
            repeat(6) {
                advanceEventTime(16)
                updatePointerBy(0, Offset(0f, -30f)); updatePointerBy(1, Offset(0f, -30f)); move()
            }
            up(0); up(1)
        }
        settle()
        assertTrue(sink.scrolls().isNotEmpty())
        // Finger up → scroll up: positive, Touch-sourced, and the whole 180 px of travel —
        // 60 DIP at this density (xxhdpi = 3 px/dp), 256 wire units each. The gesture runs
        // Begin → Update… and the lift closes it with a zero-delta End.
        val deltas = sink.scrolls().map { it.split(" ")[2].toInt() }
        assertEquals("scroll 0 ${deltas[0]} s4 p1", sink.scrolls().first())
        assertEquals("scroll 0 0 s4 p3", sink.scrolls().last())
        assertTrue(
            sink.scrolls().drop(1).dropLast(1)
                .all { it.startsWith("scroll 0 ") && it.endsWith(" s4 p2") },
        )
        assertEquals(15360, deltas.sum())
        assertEquals(emptyList<String>(), sink.buttons())
    }

    @Test
    fun twoFingerTapTakesBackItsJitterBeforeTheRightClick() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(0, center); down(1, center + Offset(80f, 0f))
            advanceEventTime(16)
            updatePointerBy(0, Offset(0f, -5f)); updatePointerBy(1, Offset(0f, -5f)); move()
            advanceEventTime(30); up(0); up(1)
        }
        settle()
        // 5 px under the tap slop scrolls at once (426 = 5 px × 256/3 at xxhdpi), then the tap
        // sends it back as an Update and cancels the axis before clicking.
        assertEquals(
            listOf(
                "scroll 0 426 s4 p1", "scroll 0 -426 s4 p2", "scroll 0 0 s4 p4",
                "btn 3 down", "btn 3 up",
            ),
            sink.log,
        )
    }

    @Test
    fun longPressHoldsTheLeftButtonUntilTheLift() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput { down(center) }
        compose.mainClock.advanceTimeBy(700)
        assertEquals(listOf("btn 1 down"), sink.buttons())
        compose.onNodeWithTag("surface").performTouchInput { advanceEventTime(700); up() }
        settle()
        assertEquals(listOf("btn 1 down", "btn 1 up"), sink.buttons())
    }

    @Test
    fun tapThenTouchWithinTheWindowDragsWithTheLeftButton() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput { down(center); advanceEventTime(50); up() }
        settle()
        compose.onNodeWithTag("surface").performTouchInput {
            advanceEventTime(100)
            down(center)
            repeat(3) { advanceEventTime(16); moveBy(Offset(15f, 0f)) }
            up()
        }
        settle()
        // The tap's click, then the drag's hold released exactly once on the lift.
        assertEquals(listOf("btn 1 down", "btn 1 up", "btn 1 down", "btn 1 up"), sink.buttons())
        assertTrue(sink.moves().isNotEmpty())
    }

    @Test
    fun threeFingerSwipeUpSummonsTheKeyboardOnce() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            down(0, center); down(1, center + Offset(80f, 0f)); down(2, center + Offset(160f, 0f))
            repeat(12) {
                advanceEventTime(16)
                for (i in 0..2) updatePointerBy(i, Offset(0f, -40f))
                move()
            }
            up(0); up(1); up(2)
        }
        settle()
        assertEquals(listOf(true), keyboard)
        assertEquals(0, stats)
        assertEquals(emptyList<String>(), sink.log)
    }

    @Test
    fun twoFingerTwistTurnsAndCommitsTheDialWithoutScrolling() {
        compose()
        compose.onNodeWithTag("surface").performTouchInput {
            // Rotate the pair about a fixed centroid: no travel, only turn.
            val r = 60f
            fun at(deg: Double) = Offset(
                center.x + r * Math.cos(Math.toRadians(deg)).toFloat(),
                center.y + r * Math.sin(Math.toRadians(deg)).toFloat(),
            )
            down(0, at(0.0)); down(1, at(180.0))
            for (step in 1..8) {
                advanceEventTime(16)
                val a = step * 5.0
                updatePointerTo(0, at(a)); updatePointerTo(1, at(180.0 + a)); move()
            }
            up(0); up(1)
        }
        settle()
        assertTrue(dial.any { it is DialEvent.Turn })
        assertTrue(dial.contains(DialEvent.Commit))
        assertEquals(emptyList<String>(), sink.log)
    }
}
