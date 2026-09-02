// Where the overlay scale meets SwiftUI: the modifier that draws the stats HUD at `OsdScale.current`.
//
// The HUD is scaled with `scaleEffect` from the corner it occupies, so it grows inward and cannot
// push itself off screen — the same anchor its enter transition already uses. One transform covers
// every font, padding and frame inside it; the alternative is multiplying a dozen metrics by hand,
// which is how the ring came to miss four of its own. `scaleEffect` transforms rendered output, so
// text is magnified rather than re-laid-out — verify on a TV before trusting it at 175 %.
//
// The quick-action ring is NOT scaled this way: it centres on wherever the twist happened, and a
// `scaleEffect` about a fixed anchor would walk an off-centre ring across the screen, so
// `RingOverlay` takes the multiplier as a parameter.

import PunktfunkShared
import SwiftUI

extension View {
    /// Draw the stats HUD at the overlay scale, growing out of `anchor` — the corner it is pinned
    /// to. A no-op off a TV, where `OsdScale.current` is 1.
    func osdScaled(anchor: UnitPoint) -> some View {
        scaleEffect(OsdScale.current, anchor: anchor)
    }
}
