// The gamepad-driven presentation of the game library (iOS/iPadOS/macOS/tvOS — see LibraryView's
// `gamepadUIActive` branch): a classic coverflow instead of the touch grid. All the
// scrolling/snapping/navigation/haptics live in GamepadCarousel; this file is the coverflow card
// (poster + the 3D recede treatment via `.scrollTransition`), the "now focused" detail panel, and
// the controller-glyph hints. A steps through covers, A launches the centered title, B closes, and
// the shoulders (L1/R1) jump a handful at a time through a long library.
//
// Layout discipline (so nothing is EVER clipped, portrait or landscape): the gradient is a
// `.background` modifier — NOT a ZStack sibling — because an `.ignoresSafeArea()` sibling expands the
// stack to full-screen and hands the GeometryReader the full height, laying content out under the
// status bar / home indicator. As a background it draws behind without affecting layout, so the
// GeometryReader is sized to the safe area, and the controller-glyph hints are pinned inside it with
// `.safeAreaInset(.bottom, alignment: .leading)`. Cover size is then derived from the height that
// remains, so a tall 2:3 poster + the detail line always fit.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

struct LibraryCoverflowView: View {
    @Environment(\.gamepadInk) private var ink
    let games: [GameEntry]
    let imageSession: URLSession?
    var onLaunch: ((String) -> Void)?
    /// Button B (back) — dismisses the library screen. No touch equivalent needed here (the toolbar
    /// Close button already covers that); this is what makes gamepad-only exit possible.
    var onDismiss: (() -> Void)?
    /// Whether the carousel owns the controller — the in-place shell gates it (mid-transition,
    /// and under the connect takeover after A launches a title, where this coverflow used to
    /// keep polling underneath). Cover/sheet presentations keep the default.
    var controllerActive = true
    @Environment(\.gamepadHostedInShell) private var hostedInShell

    #if os(iOS)
    /// `.compact` in a landscape phone window — drives a tighter poster so everything still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS
    #endif
    @State private var selection: String?

    var body: some View {
        GeometryReader { geo in
            content(for: geo.size)
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            GamepadHintBar(hints: hints)
                .padding(.leading, 22)
                .padding(.vertical, compact ? 6 : 10)
        }
        // Hosted in the shell, the field is the shell's own persistent aurora (the library is
        // an aurora screen — the calm mix simply stays 0, so nothing even chases).
        .background {
            if !hostedInShell { GamepadScreenBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a
        // pale palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
    }

    @ViewBuilder private func content(for size: CGSize) -> some View {
        // Fit the tallest poster into the height the detail line + paddings leave (the hints are a
        // safe-area inset, already out of this budget) — capped so it never dwarfs a large iPad and
        // clamped by width on a narrow screen.
        let reserved: CGFloat = (compact ? 72 : 96) + (showsGroupHeading ? 26 : 0)
        let coverHeight = min(360, min(max(140, size.height - reserved), size.width * 0.9))
        let coverWidth = coverHeight * 2 / 3

        VStack(spacing: 0) {
            Spacer(minLength: 4)
            if showsGroupHeading {
                groupHeading.padding(.bottom, 6)
            }
            carousel(coverWidth: coverWidth, coverHeight: coverHeight)
            detailPanel
                .padding(.top, 12)
            Spacer(minLength: 4)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func carousel(coverWidth: CGFloat, coverHeight: CGFloat) -> some View {
        GamepadCarousel(
            items: games,
            selection: $selection,
            itemWidth: coverWidth,
            spacing: 34,
            onActivate: { onLaunch?($0.id) },
            onBack: { onDismiss?() },
            shoulderJump: 5,
            isActive: controllerActive
        ) { game in
            cover(game, width: coverWidth, height: coverHeight)
        }
        .frame(height: coverHeight + 44)
    }

    /// One cover + the coverflow recede. Every continuous visual reads the scroll view's own
    /// per-frame `phase` (real distance-from-centered), so the tilt tracks what's actually on screen
    /// mid-scroll. `.shadow` isn't a `VisualEffect`, so it's baked constant into the card; the
    /// scale/rotation/opacity ramp already makes the centered cover prominent.
    private func cover(_ game: GameEntry, width: CGFloat, height: CGFloat) -> some View {
        PosterImage(candidates: game.art.posterCandidates, title: game.title, session: imageSession)
            .frame(width: width, height: height)
            .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay(alignment: .topLeading) {
                // `solid`: a frosted chip can't sample a backdrop through this card's own
                // composited transform, so it would only show up on the centred card.
                StoreBadge(label: game.storeLabel, isLauncher: game.isLauncher, solid: true)
            }
            .overlay {
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .strokeBorder(ink.fg(0.12), lineWidth: 1)
            }
            .shadow(color: ink.shadow(0.5), radius: 16, y: 12)
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
        let selected = games.first { $0.id == selection }
        return Text(selected?.isLauncher == true ? "LAUNCHERS" : "GAMES")
            .font(.geist(11, .semibold, relativeTo: .caption2))
            .tracking(1.4)
            .foregroundStyle(ink.fg(0.45))
    }

    /// The centered title + store tag — empty (not hidden) so the layout doesn't jump.
    @ViewBuilder private var detailPanel: some View {
        let game = games.first { $0.id == selection }
        VStack(spacing: 6) {
            Text(game?.title ?? " ")
                .font(.geist(compact ? 22 : 25, .bold, relativeTo: .title))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.75)
                .multilineTextAlignment(.center)
            if let game {
                // main's richer store label, in the palette's ink.
                Text(
                    game.isLauncher
                        ? "\(game.storeLabel.uppercased()) · LAUNCHER" : game.storeLabel.uppercased()
                )
                .font(.geist(11, .semibold, relativeTo: .caption2))
                .tracking(1.2)
                .foregroundStyle(ink.fg(0.5))
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 24)
        .animation(.smooth(duration: 0.25), value: selection)
    }

    // MARK: - Hint bar (pinned bottom-leading via safeAreaInset)

    private var hints: [GamepadHint] {
        var hints: [GamepadHint] = []
        if onLaunch != nil {
            // You *open* a launcher and *launch* a game — the hint follows the focused entry.
            let opens = games.first { $0.id == selection }?.isLauncher == true
            hints.append(
                .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: opens ? "Open" : "Launch"))
        }
        hints.append(.init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Close"))
        return hints
    }
}
#endif
