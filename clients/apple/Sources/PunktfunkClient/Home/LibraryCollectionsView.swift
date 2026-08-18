// The gamepad library's COLLECTIONS place — the desktop console's `screens/collections.rs`: the
// user's flow verbatim is "group by platform → walk the platforms → pick PS3 → see its games". One
// tile per collated group in the home carousel's tile language (same size, same glass, same
// recede), each carrying its label, its count, its kind (`LAUNCHERS` / `PLATFORM` / `STORE`) and
// a DECK of up to three covers from its sort-first titles. A opens the group as a filtered shelf,
// Y (Collections root only) opens the plain shelf as "All titles", L1/R1 step the sort — WRAPPING
// here, unlike the shelf's bar, which clamps; the desktop draws the same distinction — and B pops.
//
// The deck: front cover 118 tall (2:3), corner 11, in a reserved 120×130 box at the tile's padded
// top-leading; each slot further back is 7 % smaller, 18 pt right, 7 pt up and 6° turned — four
// cues, drawn back-to-front so the sort-first cover is on top. Under each a HARD contact plate
// (no blur — the plate is what gives the stack depth on a Deck-class GPU, and the same reasoning
// holds for a phone). A launcher slot fans its brand mark and is never fetched; a group with fewer
// than three titles fans fewer — a one-title group never fakes a stack.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

/// One tile of the Collections strip. `Identifiable` on the group's key so the carousel keeps its
/// cursor across a resort (the groups' ORDER may change; their identity does not).
struct CollectionTile: Identifiable, Hashable {
    let group: LibraryGroup
    var id: String {
        switch group.key {
        case .launchers: return "launchers"
        case .platform(let name): return "platform:\(name)"
        case .store(let name): return "store:\(name)"
        }
    }
}

struct LibraryCollectionsView: View {
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    /// The catalog in the model's order; the groups' indices point into it.
    let games: [GameEntry]
    /// `LibraryCollation.collate(games, sort, .platform)` — the container derives it.
    let groups: [LibraryGroup]
    let artLoader: (any LibraryArtSource)?
    /// The focused tile's id (`CollectionTile.id`), published for the container's legend.
    @Binding var focusID: String?
    /// A — open this group as a filtered shelf.
    var onOpen: (LibraryGroupKey) -> Void
    /// Y — "All titles" (the Collections ROOT only); nil disables it.
    var onAllTitles: (() -> Void)?
    var onBack: (() -> Void)?
    /// L1 (-1) / R1 (+1) — step the sort, wrapping.
    var onSortStep: (Int) -> Void
    var controllerActive = true
    #if os(iOS)
    @Environment(\.verticalSizeClass) private var vSizeClass
    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif

    private var tiles: [CollectionTile] { groups.map { CollectionTile(group: $0) } }

    var body: some View {
        GeometryReader { geo in
            let size = geo.size
            #if os(tvOS)
            let cardWidth = min(560, size.width * 0.34)
            let cardHeight = min(350, max(240, size.height - 40))
            let spacing: CGFloat = 44
            #else
            let cardWidth = min(340, size.width * 0.84)
            let cardHeight = min(compact ? 176 : 224, max(118, size.height - 48))
            let spacing: CGFloat = 30
            #endif
            VStack(spacing: 0) {
                Spacer(minLength: 0)
                if tiles.isEmpty {
                    Text("Nothing to collect yet — this library is still loading.")
                        .font(.geist(15, relativeTo: .body))
                        .foregroundStyle(ink.fg(0.55))
                        .frame(maxWidth: .infinity)
                } else {
                    GamepadCarousel(
                        items: tiles,
                        selection: $focusID,
                        itemWidth: cardWidth,
                        spacing: spacing,
                        onActivate: { onOpen($0.group.key) },
                        onSecondary: onAllTitles,
                        onBack: onBack,
                        onShoulder: { right in onSortStep(right ? 1 : -1) },
                        isActive: controllerActive
                    ) { tile, entrance in
                        card(tile, size: CGSize(width: cardWidth, height: cardHeight), entrance: entrance)
                    }
                    .frame(height: cardHeight + 40)
                }
                Spacer(minLength: 0)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// The tile plus the home strip's focus treatment (recede in scale, brightness, saturation,
    /// blur and alpha) — the same arithmetic `GamepadHomeView.hostCard` applies.
    private func card(_ tile: CollectionTile, size: CGSize, entrance: CardEntrance) -> some View {
        CollectionTileView(tile: tile, games: games, artLoader: artLoader, size: size)
            .modifier(entrance)
            .scrollTransition { content, phase in
                let d = CGFloat(min(abs(phase.value), 1))
                return content
                    .scaleEffect(1 - d * 0.12)
                    .brightness(Double(-d * 0.24))
                    .saturation(Double(1 - d * 0.42))
                    .blur(radius: d * 3)
                    .opacity(Double(1 - d * 0.22))
            }
    }
}

/// One collection tile: the deck top-leading, the kind caption top-trailing, the label and count
/// on the bottom rail — on the home tile's glass, to the pixel.
private struct CollectionTileView: View {
    @Environment(\.gamepadInk) private var ink
    let tile: CollectionTile
    let games: [GameEntry]
    let artLoader: (any LibraryArtSource)?
    let size: CGSize

    #if os(tvOS)
    private static let titleFont: CGFloat = 33
    private static let countFont: CGFloat = 19
    private static let kindFont: CGFloat = 15
    private static let pad: CGFloat = 28
    private static let corner: CGFloat = 30
    private static let coverH: CGFloat = 170
    #else
    private static let titleFont: CGFloat = 23
    private static let countFont: CGFloat = 13
    private static let kindFont: CGFloat = 11
    private static let pad: CGFloat = 20
    private static let corner: CGFloat = 26
    private static let coverH: CGFloat = 118
    #endif
    /// The deck's cues per slot further back: scale, right, up, turn.
    private static let fanScaleStep: CGFloat = 0.07
    private static let fanDX: CGFloat = 18
    private static let fanDY: CGFloat = 7
    private static let fanRotDeg: Double = 6
    private static let fan = 3

    private var count: Int { tile.group.indices.count }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top, spacing: 8) {
                deck
                Spacer(minLength: 0)
                Text(tile.group.key.kindCaption)
                    .font(.geist(Self.kindFont, .semibold, relativeTo: .caption2))
                    .tracking(1.4)
                    .foregroundStyle(ink.fg(0.45))
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            Text(tile.group.label)
                .font(.geist(Self.titleFont, .bold, relativeTo: .title2))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            Text(count == 1 ? "1 title" : "\(count) titles")
                .font(.geist(Self.countFont, relativeTo: .caption))
                .foregroundStyle(ink.fg(0.55))
                .lineLimit(1)
                .padding(.top, 2)
        }
        .padding(Self.pad)
        .frame(width: size.width, height: size.height, alignment: .leading)
        // The home tile's glass — `forceMaterial` for the same reason it gives: this card is
        // transformed by the entrance and the recede, and Liquid Glass cannot sample through that.
        .consoleGlass(
            RoundedRectangle(cornerRadius: Self.corner, style: .continuous),
            tint: ink.accent(0.20),
            forceMaterial: true)
        .overlay {
            RoundedRectangle(cornerRadius: Self.corner, style: .continuous)
                .strokeBorder(
                    LinearGradient(
                        colors: [ink.fg(0.22), ink.fg(0.04)],
                        startPoint: .top, endPoint: .bottom),
                    lineWidth: 1)
        }
        .shadow(color: ink.shadow(0.45), radius: 20, y: 14)
    }

    /// Up to three covers from the group's sort-first titles, fanned back-to-front.
    private var deck: some View {
        let coverH = Self.coverH
        let coverW = coverH * 2 / 3
        let slots = min(Self.fan, max(1, count))
        // The box the fan lives in: front cover + the back slots' rightward/upward drift.
        let boxW = coverW + Self.fanDX * CGFloat(slots - 1) + 8
        let boxH = coverH + Self.fanDY * CGFloat(slots - 1) + 8
        return ZStack(alignment: .bottomLeading) {
            ForEach((0..<slots).reversed(), id: \.self) { slot in
                let game = games[tile.group.indices[slot]]
                cover(game, width: coverW, height: coverH, slot: slot)
                    .scaleEffect(1 - Self.fanScaleStep * CGFloat(slot), anchor: .bottomLeading)
                    .rotationEffect(.degrees(Self.fanRotDeg * Double(slot)), anchor: .bottomLeading)
                    .offset(x: Self.fanDX * CGFloat(slot), y: -Self.fanDY * CGFloat(slot))
            }
        }
        .frame(width: boxW, height: boxH, alignment: .bottomLeading)
    }

    private func cover(_ game: GameEntry, width: CGFloat, height: CGFloat, slot: Int) -> some View {
        let shape = RoundedRectangle(cornerRadius: 11, style: .continuous)
        return PosterImage(
            // A launcher fans its brand mark and is never fetched.
            candidates: game.isLauncher ? [] : game.art.posterCandidates,
            title: game.title, loader: artLoader, icon: game.iconToken,
            drawnSize: CGSize(width: width, height: height))
            .frame(width: width, height: height)
            .clipShape(shape)
            // Back cards recede: a shade proportional to their depth, and a stronger hairline.
            .overlay { shape.fill(ink.shade(0.14 * Double(slot))) }
            .overlay { shape.strokeBorder(ink.fg(slot == 0 ? 0.18 : 0.28), lineWidth: 1) }
            // The HARD contact plate: outset 3, offset (1.5, 2), softer on a pale palette.
            .background {
                shape
                    .inset(by: -3)
                    .fill(ink.shade(0.38 * (ink.isLight ? 0.40 : 1)))
                    .offset(x: 1.5, y: 2)
            }
    }
}
#endif
