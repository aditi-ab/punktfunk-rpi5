package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The console UI's cross-client contract, against `clients/shared/console-vectors.json`.
 *
 * The background palettes, the settings section names and the screen-transition motion each exist
 * in three hand-written copies — this client, `pf-console-ui` (Rust) and the Apple client — and
 * until this file they were held together by nothing but a comment asking the next person to keep
 * them in step. Two of the three had already drifted.
 *
 * Read straight off disk with a relative path rather than copied into test resources, for the
 * reason the deeplink vectors state: a copy would be a fourth contract, free to go stale. Gradle
 * runs a unit test with the MODULE directory as its working directory, so `../../shared/` from
 * `clients/android/app` lands on `clients/shared`.
 *
 * What this pins that the older [GamepadPaletteTest] could not: the DERIVED 16-cell mesh and the
 * 4 blob colours per palette. Those are what actually reach the screen — the mesh through the AGSL
 * shader on API 33+, the blobs through the fallback field below it — and the existing tests only
 * ever measured the `stops` they are computed from.
 */
class ConsoleVectorsTest {
    private companion object {
        /** One step of Compose's 8-bit-per-component sRGB packing — see the blob comparison. */
        const val EIGHT_BIT_STEP = 1.0 / 255.0
    }

    private val vectors: JSONObject by lazy {
        val file = File("../../shared/console-vectors.json")
        assertTrue(
            "the shared vector file must be reachable at ${file.absolutePath}",
            file.isFile,
        )
        JSONObject(file.readText())
    }

    private fun JSONObject.doubles(key: String): List<Double> =
        getJSONArray(key).let { a -> (0 until a.length()).map { a.getDouble(it) } }

    private fun close(what: String, got: Double, want: Double, tol: Double = 1e-6) {
        assertTrue(
            "$what: vectors say $want, this client computes $got",
            kotlin.math.abs(got - want) <= tol,
        )
    }

    @Test
    fun cellRampAndMeshInteriorMatch() {
        assertEquals("CELL_RAMP", vectors.doubles("cell_ramp"), GamepadPalette.CELL_RAMP)

        val interior = vectors.getJSONArray("mesh_interior")
        assertEquals("mesh interior count", GamepadPalette.MESH_INTERIOR.size, interior.length())
        GamepadPalette.MESH_INTERIOR.forEachIndexed { i, p ->
            val w = interior.getJSONArray(i)
            val got = listOf(p.x, p.y, p.amp, p.sx, p.sy, p.phase)
            got.forEachIndexed { k, v -> close("mesh_interior[$i][$k]", v, w.getDouble(k)) }
        }
    }

    /** Every palette, field by field — and then the two tables derived from it. */
    @Test
    fun everyPaletteMatchesTheSharedVectors() {
        val want = vectors.getJSONArray("palettes")
        assertEquals("palette count", want.length(), GamepadPalette.ALL.size)
        GamepadPalette.ALL.forEachIndexed { i, p ->
            val w = want.getJSONObject(i)
            val id = w.getString("id")
            assertEquals("palette order", id, p.id)
            assertEquals("$id name", w.getString("name"), p.name)
            assertEquals("$id light", w.getBoolean("light"), p.light)

            val stops = w.getJSONArray("stops")
            assertEquals("$id stop count", stops.length(), p.stops.size)
            p.stops.forEachIndexed { s, t ->
                val ws = stops.getJSONArray(s)
                close("$id stops[$s].r", t.first, ws.getDouble(0))
                close("$id stops[$s].g", t.second, ws.getDouble(1))
                close("$id stops[$s].b", t.third, ws.getDouble(2))
            }

            val ground = w.doubles("ground")
            close("$id ground.r", p.ground.first, ground[0])
            close("$id ground.g", p.ground.second, ground[1])
            close("$id ground.b", p.ground.third, ground[2])
            val accent = w.doubles("accent")
            close("$id accent.r", p.accent.first, accent[0])
            close("$id accent.g", p.accent.second, accent[1])
            close("$id accent.b", p.accent.third, accent[2])

            // The mesh the shader is built from — 16 cells, sampled off the ramp per CELL_RAMP.
            val mesh = w.getJSONArray("mesh")
            assertEquals("$id mesh cells", mesh.length(), p.meshColors.size)
            p.meshColors.forEachIndexed { c, t ->
                val wc = mesh.getJSONArray(c)
                close("$id mesh[$c].r", t.first, wc.getDouble(0))
                close("$id mesh[$c].g", t.second, wc.getDouble(1))
                close("$id mesh[$c].b", t.third, wc.getDouble(2))
            }

            // The four blobs the API 28–32 fallback field drifts. These come back as Compose
            // `Color`s, which pack an sRGB colour at 8 bits per component — so the table
            // round-trips through 1/255 quantisation and the tolerance below IS that quantisation,
            // not slack. Anything the contract actually cares about (a mistyped stop, a shifted
            // sample point) moves these by far more than one 8-bit step.
            val blobs = w.getJSONArray("blobs")
            assertEquals("$id blob count", blobs.length(), p.blobColors.size)
            p.blobColors.forEachIndexed { b, colour ->
                val wb = blobs.getJSONArray(b)
                close("$id blob[$b].r", colour.red.toDouble(), wb.getDouble(0), EIGHT_BIT_STEP)
                close("$id blob[$b].g", colour.green.toDouble(), wb.getDouble(1), EIGHT_BIT_STEP)
                close("$id blob[$b].b", colour.blue.toDouble(), wb.getDouble(2), EIGHT_BIT_STEP)
            }
        }
    }

    // The tab-names assertion moved with the tabs: the Skia shell renders them, and its Rust twin
    // (`tab_names_match_the_shared_vectors` in pf-console-ui) pins them against the same file.

    /**
     * The screen-transition contract. The easing is sampled rather than compared as Bézier
     * control points: this client evaluates the desktop's analytic `1 − (1−t)³` directly, while
     * SwiftUI can only approximate it — samples with a tolerance are the one form all three can
     * meet. It is also the assertion that would have caught the curve this client shipped with
     * first, a "cubic-bezier(0.215, 0.61, 0.355, 1)" that is a full 0.08 slack at the midpoint.
     */
    @Test
    fun motionMatchesTheSharedVectors() {
        val motion = vectors.getJSONObject("motion")
        close("transition", ConsoleMotion.TRANSITION_MS / 1000.0, motion.getDouble("transition_s"))
        close("push slide", ConsoleMotion.PUSH_SLIDE.value.toDouble(), motion.getDouble("push_slide_dp"))
        close("enter scale", ConsoleMotion.ENTER_SCALE.toDouble(), motion.getDouble("enter_scale"), 1e-5)
        close("exit scale", ConsoleMotion.EXIT_SCALE.toDouble(), motion.getDouble("exit_scale"), 1e-5)
        close("reveal alpha", ConsoleMotion.REVEAL_ALPHA.toDouble(), motion.getDouble("reveal_alpha"), 1e-5)

        val curve = motion.getJSONObject("ease_out_cubic")
        val tol = curve.getDouble("tolerance")
        val samples = curve.getJSONArray("samples")
        assertTrue("the curve needs enough samples to pin it", samples.length() >= 5)
        for (i in 0 until samples.length()) {
            val s = samples.getJSONObject(i)
            val t = s.getDouble("t")
            close(
                "ease_out_cubic($t)",
                ConsoleMotion.EaseOutCubic.transform(t.toFloat()).toDouble(),
                s.getDouble("p"),
                tol,
            )
        }
    }
}
