// The gamepad settings' "select" value as a REAL band: the options sit side by side on a drum
// segment curving about a vertical axis — the current one faces you flat, and a step rotates the
// next one in with perspective. The old presentation animated a single Text keyed by its value
// (an old-out/new-in crossfade that merely implied motion), which fell apart under fast repeated
// steps: each press restarted the fade. Here the drum's position is one continuous value driven
// by a spring, and SwiftUI's spring retargeting preserves velocity — rapid presses accumulate
// into one accelerating travel instead of five restarted crossfades.
//
// The band is LINEAR, not a ring (field verdict on the first cut): a ring showed the first
// option waiting to the right of the last one, which left/right can't reach (adjust clamps) —
// a promise the navigation doesn't keep. And on a 2-option ring the unselected option flipped
// sides with every step. So positions are fixed: option i sits i steps from the start, the ends
// are the ends, and A's wrap from the last option travels BACK across the list to the first.
// Options other than the facing one exist only while the drum is actually moving — at rest a row
// shows exactly its value (a resting neighbour under a long label rendered as overlapping,
// unreadable text).
//
// The band is purely presentational: stepping semantics (left/right clamps with a boundary thud,
// A cycles forward wrapping, disabled rows refuse input) stay in GamepadSettingsView's row
// closures. Font and ink come from the environment — the row applies the same value font/colour
// it always did, and the drum's own opacity ramp multiplies on top.

import Foundation
import SwiftUI

#if os(iOS) || os(macOS) || os(tvOS)

struct GamepadOptionBand: View {
    let options: [String]
    /// The committed selection — the caller's clamp/wrap already applied.
    let selection: Int
    let focused: Bool
    /// The band's footprint, FIXED by the row: a step must never reflow the row (the old
    /// free-width value shifted the chevrons with every label), and the drum needs its stage
    /// even when the facing label is short.
    let width: CGFloat

    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// Where the drum rests, in option steps — always chasing `Double(selection)`; only the
    /// spring's interpolation ever puts it between integers.
    @State private var drumPosition: Double

    init(options: [String], selection: Int, focused: Bool, width: CGFloat) {
        self.options = options
        self.selection = selection
        self.focused = focused
        self.width = width
        _drumPosition = State(initialValue: Double(selection))
    }

    var body: some View {
        Group {
            if reduceMotion {
                // No drum, no travel: today's quiet crossfade, minus even the 14 pt slip.
                ZStack {
                    Text(current)
                        .lineLimit(1)
                        .id(selection)
                        .transition(.opacity)
                }
                .animation(.smooth(duration: 0.2), value: selection)
            } else {
                Drum(
                    options: options,
                    rotation: drumPosition,
                    target: drumPosition,
                    // Puts the ±1 neighbour ~40 % of the band off-centre, curling to the edge.
                    radius: width * 0.72,
                    width: width)
            }
        }
        .frame(width: width)
        .clipped()
        // NO `.mask` here. The soft edges used to be a gradient mask over the whole band, and a
        // mask RASTERISES what it covers — which flattens `rotation3DEffect`'s perspective, so the
        // drum was being composited as a flat sideways slide rather than a turning cylinder. That
        // is the "3D effect isn't what it should be" the field kept seeing: the geometry was
        // always right, and the mask was throwing the projection away every frame.
        //
        // The same soft edge is folded into each option's own opacity instead (see `Drum.option`),
        // which costs nothing and leaves the projection intact.
        .onChange(of: selection) { old, new in step(from: old, to: new) }
        // The options list itself can mutate under the drum (a custom resolution appears, a
        // controller connects, the buffer options re-derive from a new refresh rate) — re-seat
        // without a travel.
        .onChange(of: options.count) { _, _ in snap() }
        // One element to VoiceOver — the neighbour texts are rendering, not content.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(current)
    }

    private var current: String {
        options.indices.contains(selection) ? options[selection] : ""
    }

    /// A step (or A's wrap — which on a linear band is a fast travel back to the start) springs
    /// the drum; anything else (an external write from the touch settings, a re-derived options
    /// list) re-seats it — a travel to a value the user didn't step to would read as the UI
    /// acting on its own.
    private func step(from old: Int, to new: Int) {
        let wrapped = options.count > 1 && old == options.count - 1 && new == 0
        guard (abs(new - old) == 1 || wrapped), !reduceMotion else { return snap() }
        withAnimation(.spring(response: 0.32, dampingFraction: 0.78)) {
            drumPosition = Double(new)
        }
    }

    private func snap() {
        var tx = Transaction()
        tx.disablesAnimations = true
        withTransaction(tx) { drumPosition = Double(selection) }
    }
}

/// The rotating drum itself. `Animatable` so SwiftUI re-evaluates the body with the INTERPOLATED
/// rotation every frame of the spring — each option's offset/scale/opacity follows the real arc,
/// and options along the travel genuinely enter and leave mid-flight. (A plain `.animation` on
/// independent modifiers can't do that: each modifier would lerp its own endpoints and the
/// in-between options would never appear.)
private struct Drum: View, Animatable {
    let options: [String]
    /// The interpolated drum position, in option steps.
    var rotation: Double
    /// Where the spring is headed (jumps instantly on a step; only `rotation` chases it). The
    /// distance between them is "how mid-flight are we" — the neighbours exist exactly as long
    /// as the drum is moving, fading continuously as it lands, so a resting row is one flat
    /// Text and a long label never sits under a resting neighbour.
    let target: Double
    /// Drum radius in points (from the band width — see the caller).
    let radius: Double
    /// The band's own width — the stage the options turn on, and what the edge fade is measured
    /// against now that the container no longer carries a mask.
    let width: Double

    var animatableData: Double {
        get { rotation }
        set { rotation = newValue }
    }

    /// Angular pitch between adjacent options on the drum.
    private static let stepAngle = 34.0 * .pi / 180.0

    // Neighbours exist only while the drum is MOVING, and that is not a compromise — it is the
    // documented field fix this file was written around. Showing them at rest was tried (to make a
    // settled row look more like a cylinder) and immediately reproduced the original defect: on the
    // simulator, "This device · 2752 × 2064" rendered with "280 ×" sitting on top of it, and
    // "Automatic" with "10 Mbps" through it. A long value and its neighbour occupy the same
    // pixels, and no opacity low enough to fix that is high enough to be worth drawing.
    //
    // The cylinder is meant to be READ WHILE IT TURNS. What was actually broken is fixed above:
    // the band used to mask itself, and the mask rasterised the drum and threw its perspective
    // away every frame, so the turn never looked like a turn.

    var body: some View {
        let flight = min(1, abs(rotation - target) * 3)
        let content = ZStack {
            ForEach(0..<options.count, id: \.self) { i in
                // Plain signed distance — the band is linear, so option i has ONE home and the
                // ends are the ends (nothing waits beyond the last option).
                let d = Double(i) - rotation
                // Only the facing option at rest; its neighbours join it for the travel (see the
                // note on `restingNeighbour`'s removal above).
                if abs(d) < 0.5 || (flight > 0.001 && abs(d) <= 2.5) {
                    option(i, distance: d, gate: flight)
                }
            }
        }
        #if os(tvOS)
        // Flatten the transform stack — the 10-foot GPU already made these rows drop Liquid
        // Glass, and several projected texts per step is the same class of cost. It costs the
        // projection (a rasterised layer has no perspective), which is the trade tvOS already
        // makes elsewhere on this screen.
        content.drawingGroup()
        #else
        content
        #endif
    }

    @ViewBuilder private func option(_ i: Int, distance d: Double, gate: Double) -> some View {
        let angle = d * Self.stepAngle
        let depth = cos(angle)
        let x = radius * sin(angle)
        // The facing option never gates: a resting row still shows its value.
        let alpha = pow(max(depth, 0), 3) * (abs(d) < 0.5 ? 1 : gate) * edgeFade(x)
        Text(options[i])
            .lineLimit(1)
            .fixedSize() // never let a turning label re-wrap to the band's width mid-flight
            .scaleEffect(0.70 + 0.30 * depth)
            // Foreshorten the label as it turns away — this is what sells the cylinder.
            .rotation3DEffect(.radians(angle), axis: (x: 0, y: 1, z: 0), perspective: 0.55)
            .offset(x: x)
            .opacity(alpha)
            .zIndex(depth)
    }

    /// The soft edge, per option, replacing the container mask that used to flatten the
    /// projection: full strength through the middle of the band, dissolving to nothing by the
    /// time an option reaches its rim, so the drum never ends on a cut.
    private func edgeFade(_ x: Double) -> Double {
        let halfWidth = width / 2
        guard halfWidth > 0 else { return 1 }
        let fadeStart = halfWidth * 0.55
        guard abs(x) > fadeStart else { return 1 }
        return max(0, min(1, (halfWidth - abs(x)) / (halfWidth - fadeStart)))
    }
}

#endif
