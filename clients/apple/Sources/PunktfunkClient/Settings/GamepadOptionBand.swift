// The gamepad settings' "select" value as a REAL band: every option sits on a drum rotating
// about a vertical axis — the current one faces you flat, its neighbours curve away with
// perspective, shrinking and fading toward the edges. The old presentation animated a single
// Text keyed by its value (an old-out/new-in crossfade that merely implied motion), which fell
// apart under fast repeated steps: each press restarted the fade. Here the drum's position is
// one continuous value driven by a spring, and SwiftUI's spring retargeting preserves velocity —
// rapid presses accumulate into one accelerating spin instead of five restarted crossfades.
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

    /// Where the drum rests, in option steps — UNBOUNDED: a forward wrap keeps adding 1, never
    /// modded back, so the ring distance below is what brings option 0 around from the right.
    /// Rendering only ever reads it modulo `options.count`.
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
                    neighborGate: focused ? 1 : 0,
                    target: drumPosition,
                    // Puts the ±1 neighbour ~40 % of the band off-centre, curling to the edge.
                    radius: width * 0.72)
            }
        }
        .frame(width: width)
        .clipped()
        // Soft edges: the drum dissolves before it reaches the chevrons instead of ending on a cut.
        .mask {
            LinearGradient(
                stops: [
                    .init(color: .clear, location: 0),
                    .init(color: .black, location: 0.12),
                    .init(color: .black, location: 0.88),
                    .init(color: .clear, location: 1),
                ],
                startPoint: .leading, endPoint: .trailing)
        }
        .onChange(of: selection) { old, new in step(from: old, to: new) }
        // The options list itself can mutate under the drum (a custom resolution appears, a
        // controller connects, the buffer options re-derive from a new refresh rate) — the ring
        // math is only valid while drumPosition ≡ selection (mod count), so re-seat without a spin.
        .onChange(of: options.count) { _, _ in snap() }
        // One element to VoiceOver — the neighbour texts are rendering, not content.
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(current)
    }

    private var current: String {
        options.indices.contains(selection) ? options[selection] : ""
    }

    /// One step spins the drum; anything else (an external write from the touch settings, a
    /// re-derived options list) re-seats it — a spin to a value the user didn't step to would
    /// read as the UI acting on its own.
    private func step(from old: Int, to new: Int) {
        let n = options.count
        let raw = new - old
        let delta: Int? = if n > 1 && old == n - 1 && new == 0 {
            1 // A's wrap from the last option: keep spinning FORWARD, the way the thumb pressed.
        } else if abs(raw) == 1 {
            raw
        } else {
            nil
        }
        guard let delta, !reduceMotion else { return snap() }
        withAnimation(.spring(response: 0.32, dampingFraction: 0.78)) {
            drumPosition += Double(delta)
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
/// and options more than one step away genuinely enter and leave mid-spin. (A plain `.animation`
/// on independent modifiers can't do that: each modifier would lerp its own endpoints and the
/// in-between options would never appear.)
private struct Drum: View, Animatable {
    let options: [String]
    /// The interpolated drum position, in option steps.
    var rotation: Double
    /// 1 while the row is focused — the resting drum shows its neighbours only under focus (an
    /// unfocused row is one flat Text, visually and costwise what it was before the band).
    var neighborGate: Double
    /// Where the spring is headed (jumps instantly on a step; only `rotation` chases it). The
    /// distance between them is "how mid-flight are we" — it keeps the neighbours visible while
    /// an unfocused drum finishes settling, fading them continuously as it lands.
    let target: Double
    /// Drum radius in points (from the band width — see the caller).
    let radius: Double

    var animatableData: AnimatablePair<Double, Double> {
        get { AnimatablePair(rotation, neighborGate) }
        set {
            rotation = newValue.first
            neighborGate = newValue.second
        }
    }

    /// Angular pitch between adjacent options on the drum.
    private static let stepAngle = 34.0 * .pi / 180.0

    var body: some View {
        let n = options.count
        let flight = min(1, abs(rotation - target) * 3)
        let content = ZStack {
            ForEach(0..<n, id: \.self) { i in
                // Signed ring distance to the drum position, wrapped into (-n/2, n/2] — the
                // whole wrap story: a monotonically grown position brings option 0 around from
                // the right of the last option with no special casing.
                let d = (Double(i) - rotation).remainder(dividingBy: Double(n))
                if abs(d) <= 2.5 {
                    option(i, distance: d, gate: max(neighborGate, flight))
                }
            }
        }
        #if os(tvOS)
        // Flatten the transform stack while spinning — the 10-foot GPU already made these rows
        // drop Liquid Glass, and five projected texts per step is the same class of cost.
        content.drawingGroup()
        #else
        content
        #endif
    }

    @ViewBuilder private func option(_ i: Int, distance d: Double, gate: Double) -> some View {
        let angle = d * Self.stepAngle
        let depth = cos(angle)
        // The facing option never gates: an unfocused row still shows its value.
        let alpha = pow(max(depth, 0), 3) * (abs(d) < 0.5 ? 1 : gate)
        Text(options[i])
            .lineLimit(1)
            .scaleEffect(0.70 + 0.30 * depth)
            // Foreshorten the label as it turns away — this is what sells the cylinder.
            .rotation3DEffect(.radians(angle), axis: (x: 0, y: 1, z: 0), perspective: 0.4)
            .offset(x: radius * sin(angle))
            .opacity(alpha)
            .zIndex(depth)
    }
}

#endif
