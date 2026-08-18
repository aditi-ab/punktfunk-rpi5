// The gamepad library's SHELF arrangement (iOS/iPadOS/macOS/tvOS — see LibraryView's
// `gamepadUIActive` branch): a classic coverflow. All the scrolling/snapping/navigation/haptics
// live in GamepadCarousel; this file is the coverflow card (poster + the 3D recede treatment via
// `.scrollTransition`) and the LAUNCHERS/GAMES heading. The detail band, the legend, the backdrop
// and the view/sort bar belong to `LibraryConsoleView`, which hosts this arrangement and the grid
// alike — one shelf, two arrangements, one set of chrome, exactly the desktop console's split.
//
// A steps through covers, A launches the centered title, B closes, the shoulders (L1/R1) jump a
// handful at a time through a long library, and ▲ hands the controller to the bar above.
//
// Layout discipline (so nothing is EVER clipped, portrait or landscape): the container hands this
// view the height that remains after its own chrome; cover size is derived from that, so a tall
// 2:3 poster always fits.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

struct LibraryCoverflowView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — the container publishes that
    /// value and this view sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    /// The shelf in DISPLAY order — collated by the container (launchers lead, then the sort).
    let games: [GameEntry]
    let artLoader: (any LibraryArtSource)?
    /// The centred title, published for the container's detail band and legend. Output only —
    /// the carousel's cursor is the authority (see GamepadCarousel's header).
    @Binding var focusID: String?
    var onLaunch: ((String) -> Void)?
    /// Which titles the host already has up, keyed by library id — so a card the player can return
    /// to says `Resume` rather than looking like every other one. Empty on an older host.
    var running: [String: RunningGame] = [:]
    /// The title to assemble the strip around on mount — the one last opened from this shelf
    /// (`LibraryScrollMemory`), so coming back from a stream lands where the player left rather
    /// than on the first cover. Ignored when it is no longer in the list.
    var initialSelection: String?
    /// Button B (back) — the container decides what that means (pop a place, or dismiss).
    var onBack: (() -> Void)?
    /// Button Y — the container's secondary action (Collections); nil disables it.
    var onSecondary: (() -> Void)?
    /// Button X — copy the centred title's link; nil where the platform has no clipboard.
    var onTertiary: (() -> Void)?
    /// ▲ — hand the controller to the view/sort bar above the field. Wiring it makes vertical
    /// the bar's axis (down goes inert), the reading the desktop shelf has.
    var onUp: (() -> Void)?
    /// Whether the carousel owns the controller — the container gates it while the bar has
    /// focus, and the shell gates it mid-transition and under the connect takeover.
    var controllerActive = true

    #if os(iOS)
    /// `.compact` in a landscape phone window — a tighter poster so the title band gets air.
    @Environment(\.verticalSizeClass) private var vSizeClass
    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS
    #endif

    /// How many covers have settled (art loaded, or every candidate exhausted).
    @State private var artSettled = 0
    /// The backstop below has fired: play the entrance regardless of what the art is doing.
    @State private var artWaitOver = false

    /// Whether the strip may play its entrance yet. Cards swinging in as grey placeholders and
    /// then filling with artwork afterwards is the whole effect wasted, so the entrance waits for
    /// the first few covers — every poster is fetched in parallel, so those land together and
    /// cover the visible strip. The wait is capped: a slow or artless library still animates.
    private var contentReady: Bool {
        artWaitOver || artSettled >= min(4, games.count)
    }

    var body: some View {
        GeometryReader { geo in
            content(for: geo.size)
        }
        // The entrance's backstop (see `contentReady`).
        .task {
            try? await Task.sleep(for: .milliseconds(700))
            artWaitOver = true
        }
    }

    @ViewBuilder private func content(for size: CGSize) -> some View {
        // Fit the tallest poster into the height the container left us (the detail band and the
        // legend are already out of this budget) — capped so it never dwarfs a large iPad and
        // clamped by width on a narrow screen.
        // In a landscape phone's height the title band sits right under the strip; hold a
        // little of the cover's height back so the two don't touch.
        let reserved: CGFloat = (compact ? 26 : 8) + (showsGroupHeading ? 26 : 0)
        let coverHeight = min(360, min(max(140, size.height - reserved), size.width * 0.9))
        let coverWidth = coverHeight * 2 / 3

        VStack(spacing: 0) {
            Spacer(minLength: 4)
            if showsGroupHeading {
                groupHeading.padding(.bottom, 6)
            }
            carousel(coverWidth: coverWidth, coverHeight: coverHeight)
            Spacer(minLength: 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func carousel(coverWidth: CGFloat, coverHeight: CGFloat) -> some View {
        GamepadCarousel(
            items: games,
            selection: $focusID,
            itemWidth: coverWidth,
            spacing: 34,
            initialItemID: initialSelection,
            onActivate: { onLaunch?($0.id) },
            onSecondary: onSecondary,
            onTertiary: onTertiary,
            onBack: onBack,
            onUp: onUp,
            shoulderJump: 5,
            isActive: controllerActive,
            contentReady: contentReady
        ) { game, entrance in
            cover(game, width: coverWidth, height: coverHeight, entrance: entrance)
        }
        .frame(height: coverHeight + 44)
    }

    /// One cover + the coverflow recede. Every continuous visual reads the scroll view's own
    /// per-frame `phase` (real distance-from-centered), so the tilt tracks what's actually on screen
    /// mid-scroll. `.shadow` isn't a `VisualEffect`, so it's baked constant into the card; the
    /// scale/rotation/opacity ramp already makes the centered cover prominent.
    private func cover(
        _ game: GameEntry, width: CGFloat, height: CGFloat, entrance: CardEntrance
    ) -> some View {
        PosterImage(
            candidates: game.art.posterCandidates, title: game.title, loader: artLoader,
            icon: game.iconToken,
            // Decode at the size drawn (the container's poster box), never at the CDN's.
            drawnSize: CGSize(width: width, height: height),
            onLoaded: { artSettled += 1 })
            .frame(width: width, height: height)
            .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(alignment: .topLeading) {
                // `solid`: a frosted chip can't sample a backdrop through this card's own
                // composited transform, so it would only show up on the centred card.
                StoreBadge(label: game.storeLabel, isLauncher: game.isLauncher, solid: true)
            }
            // Opposite corner from the store chip, and `solid` for the same compositing reason.
            .overlay(alignment: .topTrailing) {
                if running[game.id] != nil { RunningBadge(solid: true) }
            }
            .overlay {
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .strokeBorder(ink.fg(0.12), lineWidth: 1)
            }
            .shadow(color: ink.shadow(0.5), radius: 16, y: 12)
            // Beneath the scroll transition, never around it — see CardEntrance.
            .modifier(entrance)
            .scrollTransition { content, phase in
                let v = phase.value
                let d = CGFloat(min(abs(v), 1))
                let scale = 1 - d * 0.24
                let rot = v * -38
                let anchor: UnitPoint = v < 0 ? .trailing : .leading
                let bright = Double(-d * 0.22)
                let fade = Double(1 - d * 0.38)
                return content
                    .scaleEffect(scale)
                    .rotation3DEffect(
                        .degrees(rot), axis: (x: 0, y: 1, z: 0), anchor: anchor, perspective: 0.55)
                    .brightness(bright)
                    .opacity(fade)
            }
    }

    /// Does this library have both groups? Only then does the heading earn its row — a
    /// launcher-less library gets exactly the layout it had before design D4.
    private var showsGroupHeading: Bool {
        games.contains(where: \.isLauncher) && games.contains { !$0.isLauncher }
    }

    /// Which group the cursor is in. A coverflow is one-dimensional, so instead of a second focus
    /// rail (a whole new up/down nav model for two or three tiles) the heading names the group and
    /// changes as the selection crosses the boundary — the launcher entries lead the strip.
    private var groupHeading: some View {
        let selected = games.first { $0.id == focusID }
        return Text(selected?.isLauncher == true ? "LAUNCHERS" : "GAMES")
            .font(.geist(11, .semibold, relativeTo: .caption2))
            .tracking(1.4)
            .foregroundStyle(ink.fg(0.45))
    }
}
#endif
