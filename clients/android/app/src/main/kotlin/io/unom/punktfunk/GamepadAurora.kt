package io.unom.punktfunk

import android.graphics.RuntimeShader
import android.os.Build
import androidx.annotation.RequiresApi
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.core.withInfiniteAnimationFrameMillis
import androidx.compose.foundation.Canvas
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ShaderBrush
import java.util.Locale
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.max
import kotlin.math.sin

// The living console backdrop, in two renderings of ONE design.
//
// On API 33+ this is the desktop console's actual field: `pf-console-ui`'s `mesh_sksl`
// (library.rs) ported to AGSL — a 4×4 bicubic colour mesh warped by four drifting interior points,
// swayed ±8° in hue, vignetted and scrimmed. AGSL is the SkSL subset Android 13 ships, so the
// shader body is very nearly the same source, and `GamepadPalette.meshColors` is literally the same
// 16-cell table the Rust samples. Below 33 (`RuntimeShader` is 33+) the field falls back to four
// drifting radial blobs sampled from the same palette ramp — an approximation of the same look, and
// the honest one: emulating a mesh gradient with bitmaps would cost more than it bought.
//
// Either way it is AMBIENCE, never content: it runs full-bleed under the cutout and the system bars,
// and every console screen's chrome floats over it.

/**
 * The console backdrop. [calm] is what the FORM screens (settings, add-host) wear: the pools dim
 * onto the ground so the glass rows keep real colour and luminance without the launcher's contrast.
 * Motion is identical either way on purpose — only the contrast differs, so moving between screens
 * can't make the field jump.
 *
 * Honours the system's "remove animations" accessibility setting by freezing at a fixed phase, the
 * same courtesy the Apple client pays Reduce Motion — which doubles as the deterministic mode the
 * screenshot harness captures in, since the phase is just a uniform.
 */
@Composable
fun GamepadAuroraBackground(modifier: Modifier = Modifier, calm: Boolean = false) {
    val palette = LocalGamepadPalette.current
    val animated = animationsEnabled()
    // Compiled once per palette and cached process-wide: stepping the Background row recolours the
    // field under the very row being stepped, and a shader compile per D-pad press would be felt on
    // a TV box. A compile failure resolves null and takes the blob path — a vendor Skia that
    // rejects the source must not take the console UI down with it.
    val shader = if (Build.VERSION.SDK_INT >= 33) {
        remember(palette.id) { meshShaderFor(palette) }
    } else {
        null
    }
    if (shader != null) {
        MeshAurora(modifier, shader, calm, animated)
    } else {
        BlobAurora(modifier, palette, calm, animated)
    }
}

/**
 * The backdrop for the console FORM screens (settings, add-host) — the launcher's own living field
 * at `calm`, so no screen in the console UI is backed by a still image and the palette setting
 * reaches every one of them. Mirrors the Apple client's GamepadFormBackground and the desktop's
 * single `calm` uniform.
 */
@Composable
fun GamepadFormBackground(modifier: Modifier = Modifier) {
    GamepadAuroraBackground(modifier, calm = true)
}

// --- The mesh field (API 33+) ---------------------------------------------------------------

/** The phase a frozen (reduce-motion / screenshot) field is drawn at — the desktop's t = 0. */
private const val FROZEN_PHASE = 0f

@RequiresApi(33)
@Composable
private fun MeshAurora(
    modifier: Modifier,
    shader: RuntimeShader,
    calm: Boolean,
    animated: Boolean,
) {
    val ink = LocalGamepadInk.current
    val palette = LocalGamepadPalette.current
    val brush = remember(shader) { ShaderBrush(shader) }
    // Real monotonic seconds, not a wrapping sweep: the four warp points and the hue sway run at
    // mutually irrational rates (periods ~90–130 s), so no loop length exists that would rejoin
    // them seamlessly — which is exactly why the desktop feeds its shader elapsed time too. Frozen
    // under reduce-motion, where it also makes the field deterministic for a screenshot.
    val time by produceState(FROZEN_PHASE, animated) {
        if (!animated) return@produceState
        while (true) {
            withInfiniteAnimationFrameMillis { value = it / 1000f }
        }
    }
    val (gr, gg, gb) = palette.ground
    Canvas(modifier) {
        // Uniforms are set per draw, not per recomposition: `time` is read HERE, inside the draw
        // scope, so a new frame invalidates the draw only — the composition never re-runs.
        shader.setFloatUniform("u_res", size.width, size.height)
        shader.setFloatUniform("u_tc", time, if (calm) 1f else 0f)
        // The calm lift: the palette's ground scaled to 0.4, what the field flattens toward.
        shader.setFloatUniform(
            "u_lift",
            (gr * 0.4).toFloat(), (gg * 0.4).toFloat(), (gb * 0.4).toFloat(), 0f,
        )
        // Where the vignette and scrims tend, and how hard — black at full strength on a dark
        // field, white at well under half on a pale one (mixing a pastel toward white at the dark
        // field's strength bleaches the chroma straight out of the gradient).
        shader.setFloatUniform(
            "u_scrim",
            ink.shade.red, ink.shade.green, ink.shade.blue, ink.shadeScale,
        )
        drawRect(brush)
    }
}

/**
 * Compiled mesh shaders by palette id — at most the 13 shipped palettes, so it is bounded by the
 * table rather than by use. Touched only from the composition (main) thread.
 */
private val meshShaders = HashMap<String, RuntimeShader?>()

@RequiresApi(33)
private fun meshShaderFor(palette: GamepadPalette): RuntimeShader? =
    meshShaders.getOrPut(palette.id) {
        runCatching { RuntimeShader(meshAgsl(palette.meshColors)) }.getOrNull()
    }

/**
 * Format a shader constant. `Locale.ROOT` is not optional: `String.format` on a German-locale
 * device emits `0,075`, which is a syntax error in the shader source and would take the whole
 * backdrop out on exactly the devices it was authored on. `%f` also keeps a very small ramp value
 * out of exponent notation, which SkSL would still parse but nobody would enjoy reading.
 */
private fun n(v: Double): String = String.format(Locale.ROOT, "%.6f", v)

/**
 * The mesh gradient as AGSL, the palette baked into the source and resolution/time/calm/scrim left
 * as uniforms — the direct port of `pf-console-ui`'s `mesh_sksl`, kept structurally line-for-line
 * with it so the two can be diffed. A smooth bicubic blend of the 16 colours (a separable
 * cubic-Bézier basis in x then y, the fragment-shader analogue of SwiftUI's
 * `MeshGradient(smoothsColors: true)`), four interior points driving a bounded domain warp, then
 * the ±8° hue sway, an elliptical vignette and the vertical legibility scrim.
 */
private fun meshAgsl(colors: List<Triple<Double, Double, Double>>): String {
    fun c(i: Int): String {
        val (r, g, b) = colors[i]
        return "float3(${n(r)}, ${n(g)}, ${n(b)})"
    }
    // The four interior-point domain-warp accumulators. SIG (0.30) sets how far each point's pull
    // reaches; the warp is the weight-normalised average displacement, so |warp| ≤ max|amp|.
    val warp = buildString {
        for (p in GamepadPalette.MESH_INTERIOR) {
            append("    q = uv - float2(${n(p.x)}, ${n(p.y)});\n")
            append("    ww = exp(-dot(q, q) / (2.0 * 0.30 * 0.30));\n")
            append("    d = float2(${n(p.amp)} * sin(tt * ${n(p.sx)} + ${n(p.phase)}),\n")
            append("               ${n(p.amp)} * cos(tt * ${n(p.sy)} + ${n(p.phase)} * 1.3));\n")
            append("    wsum += d * ww; wtot += ww;\n")
        }
    }
    return """
uniform float2 u_res;
// x = seconds since this field started, y = the calm mix (0 launcher, 1 form).
uniform float2 u_tc;
// rgb = the palette's corner colour scaled for the calm lift; a is unused.
uniform float4 u_lift;
// rgb = what the vignette and scrims tend toward, a = how hard.
uniform float4 u_scrim;

// Cubic-Bézier basis over four control values — the smooth 4-point blend per axis.
float bz(float t, float a, float b, float c, float d) {
    float u = 1.0 - t;
    return u*u*u*a + 3.0*u*u*t*b + 3.0*u*t*t*c + t*t*t*d;
}
float3 bz3(float t, float3 a, float3 b, float3 c, float3 d) {
    return float3(bz(t, a.r, b.r, c.r, d.r), bz(t, a.g, b.g, c.g, d.g), bz(t, a.b, b.b, c.b, d.b));
}
// Hue rotation about the grey axis (Rodrigues) — the ±8° warm/cool sway. The desktop's `cross(k,
// col)` is written out here: with k = (c, c, c) it collapses to c·(b-g, r-b, g-r), which needs no
// builtin at all — AGSL's function set is a subset of SkSL's and not worth betting the field on.
float3 hue(float3 col, float a) {
    float c = 0.5773503;
    float cs = cos(a); float sn = sin(a);
    float3 kx = c * float3(col.b - col.g, col.r - col.b, col.g - col.r);
    return col*cs + kx*sn + float3(c) * dot(float3(c), col) * (1.0 - cs);
}

half4 main(float2 xy) {
    float tt = u_tc.x; float calm = u_tc.y;
    float2 uv = xy / u_res;
    // Interior control points wander → bounded domain warp (pools follow them).
    float2 wsum = float2(0.0); float wtot = 0.0; float2 q; float ww; float2 d;
$warp
    uv = clamp(uv - wsum / (wtot + 0.0001), 0.0, 1.0);

    // Bicubic blend of the 16 mesh colours: cubic-Bézier in x per row, then in y.
    float3 r0 = bz3(uv.x, ${c(0)}, ${c(1)}, ${c(2)}, ${c(3)});
    float3 r1 = bz3(uv.x, ${c(4)}, ${c(5)}, ${c(6)}, ${c(7)});
    float3 r2 = bz3(uv.x, ${c(8)}, ${c(9)}, ${c(10)}, ${c(11)});
    float3 r3 = bz3(uv.x, ${c(12)}, ${c(13)}, ${c(14)}, ${c(15)});
    float3 col = bz3(uv.y, r0, r1, r2, r3);

    col = hue(col, sin(tt * 0.021) * 0.1396263);

    // Calm: flatten the field toward its own corner colour — the pools dim and the corners lift,
    // so a form screen keeps real colour under its glass rows while losing the launcher's
    // contrast. Motion is untouched.
    col = mix(col, col * 0.60 + u_lift.rgb, calm);

    // Elliptical vignette: clear at r=0.25 → scrim·0.42 at r=1.15. Halved under calm — a
    // launcher's cards sit in the pooled centre, but a form screen's rows run out toward the
    // edges, where crushing them just eats the list.
    float2 e = (xy / u_res - 0.5) * 2.0;
    float vig = clamp((length(e) - 0.25) / 0.90, 0.0, 1.0) * mix(0.42, 0.21, calm) * u_scrim.a;
    col = mix(col, u_scrim.rgb, vig);

    // Vertical legibility scrim for the pinned heading + the floating legend.
    float v = xy.y / u_res.y;
    float s = v < 0.32 ? mix(0.38, 0.06, v / 0.32)
            : v < 0.68 ? mix(0.06, 0.08, (v - 0.32) / 0.36)
            : mix(0.08, 0.40, (v - 0.68) / 0.32);
    col = mix(col, u_scrim.rgb, s * u_scrim.a);

    return half4(half3(col), 1.0);
}
"""
}

// --- The blob field (API 28–32 fallback) -----------------------------------------------------

/**
 * One drifting blob of the fallback field: where it sits, how far it wanders, and how fast. Integer
 * [sx]/[sy] keep the loop seamless at wrap. The COLOUR is the palette's, taken from its ramp at
 * draw time, so the field always shows several of that palette's tones at once.
 */
private class AuroraBlob(
    val baseX: Float,
    val baseY: Float,
    val driftX: Float,
    val driftY: Float,
    val sx: Int,
    val sy: Int,
    val phase: Float,
    val radiusFrac: Float,
    val alpha: Float,
)

private val auroraBlobs = listOf(
    AuroraBlob(0.30f, 0.26f, 0.16f, 0.10f, 1, 1, 0.0f, 0.62f, 0.55f),
    AuroraBlob(0.78f, 0.68f, 0.13f, 0.14f, 1, 2, 2.4f, 0.68f, 0.58f),
    AuroraBlob(0.16f, 0.82f, 0.12f, 0.09f, 2, 1, 4.1f, 0.52f, 0.42f),
    AuroraBlob(0.72f, 0.14f, 0.10f, 0.08f, 1, 3, 1.2f, 0.48f, 0.40f),
)

/**
 * Soft blobs from the palette's ramp drifting over its ground on slow, seamless loops, finished
 * with a centre-pooling vignette and top/bottom legibility scrims. What API 28–32 sees in place of
 * the mesh: the same colour families, the same "ambience, never content" role, and the same
 * [GamepadPalette] setting recolours it.
 */
@Composable
private fun BlobAurora(
    modifier: Modifier,
    palette: GamepadPalette,
    calm: Boolean,
    animated: Boolean,
) {
    val ink = LocalGamepadInk.current
    val transition = rememberInfiniteTransition(label = "aurora")
    // A full 0..2π sweep over ~96 s; integer per-blob multipliers make sin/cos continuous at the
    // wrap so the field never visibly jumps when the animation restarts.
    val swept by transition.animateFloat(
        initialValue = 0f,
        targetValue = (2 * PI).toFloat(),
        animationSpec = infiniteRepeatable(tween(96_000, easing = LinearEasing), RepeatMode.Restart),
        label = "angle",
    )
    val angle = if (animated) swept else 0f
    val tones = palette.blobColors
    val ground = palette.groundColor
    // Where the scrims tend, and how hard. Mixing a PALE field toward white at the dark field's
    // strength bleaches the chroma straight out of the gradient, so a pale palette gets under
    // half — the same scrim strength the desktop console's shader carries.
    val scrim = if (palette.light) ink.fg else Color.Black
    val strength = if (palette.light) 0.45f else 1f
    Canvas(modifier) {
        drawRect(ground)
        val span = max(size.width, size.height)
        for ((i, b) in auroraBlobs.withIndex()) {
            val cx = (b.baseX + b.driftX * sin(angle * b.sx + b.phase)) * size.width
            val cy = (b.baseY + b.driftY * cos(angle * b.sy + b.phase)) * size.height
            val r = span * b.radiusFrac
            // Calm scales each blob's contribution rather than dimming the whole canvas: the
            // ground stays put and only the pools come down to meet it, which is the same "lower
            // the contrast, keep the colour" the desktop console's `calm` uniform does.
            val alpha = if (calm) b.alpha * 0.62f else b.alpha
            drawCircle(
                brush = Brush.radialGradient(
                    colors = listOf(tones[i].copy(alpha = alpha), Color.Transparent),
                    center = Offset(cx, cy),
                    radius = r,
                ),
                center = Offset(cx, cy),
                radius = r,
                // Additive only works over a DARK ground; over a pale one every blob
                // saturates to white and the field turns grey. Pale palettes tint instead.
                blendMode = if (palette.light) BlendMode.SrcOver else BlendMode.Plus,
            )
        }
        // Cinematic vignette: pool light centre, settle the corners toward the scrim. Halved under
        // calm: a launcher's cards sit in the pooled centre, but a form screen's rows run out
        // toward the edges, where crushing them just eats the list.
        drawRect(
            Brush.radialGradient(
                colors = listOf(
                    Color.Transparent,
                    scrim.copy(alpha = (if (calm) 0.22f else 0.44f) * strength),
                ),
                center = Offset(size.width / 2, size.height / 2),
                radius = span * 0.92f,
            ),
        )
        // Top/bottom legibility scrim for the pinned title + hint bar.
        drawRect(
            Brush.verticalGradient(
                0.0f to scrim.copy(alpha = 0.40f * strength),
                0.30f to scrim.copy(alpha = 0.05f * strength),
                0.70f to scrim.copy(alpha = 0.06f * strength),
                1.0f to scrim.copy(alpha = 0.42f * strength),
            ),
        )
    }
}
