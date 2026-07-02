// A controller-driven on-screen keyboard for the gamepad UI's text fields (iOS/iPadOS only) —
// iOS has no system keyboard a game controller can drive (the tvOS fullscreen entry doesn't
// exist here), so without this, adding a host from the couch would end with "now touch the
// screen". Dpad/stick moves a key cursor over a fixed grid, A types, X backspaces, B/Y confirms.
// Lowercase + digits + the hostname/address punctuation is deliberately the whole character set:
// these fields hold names, addresses and ports, not prose.
//
// Edits are applied to the binding live (the caller's field row shows every keystroke), so
// closing the keyboard is always "done" — there is no separate cancel/commit step to get wrong.
// Touch stays a fallback: every keycap is tappable.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)

struct GamepadKeyboard: View {
    @Binding var text: String
    /// Restricts typed characters (e.g. digits for a port field); backspace always works.
    var allowed: CharacterSet?
    /// B / Y / the Done key — the binding already holds the final text.
    let onDone: () -> Void

    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    @State private var cursor = GridPos(row: 1, col: 0) // opens on "q"
    @State private var pressTick = 0
    @State private var boundaryTick = 0
    #if os(iOS)
    /// `.compact` (landscape phone): shorter keycaps so the tray leaves room for the field rows.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized generously
    #endif

    private struct GridPos: Hashable {
        var row: Int
        var col: Int
    }

    private enum Key: Hashable {
        case char(Character)
        case space
        case backspace
        case done
    }

    /// Digits first (addresses/ports), then letters; the last char column carries the
    /// hostname/address punctuation.
    private static let rows: [[Key]] = [
        Array("1234567890").map(Key.char),
        Array("qwertyuiop").map(Key.char),
        Array("asdfghjkl-").map(Key.char),
        Array("zxcvbnm._:").map(Key.char),
        [.space, .backspace, .done],
    ]

    var body: some View {
        VStack(spacing: compact ? 5 : 7) {
            ForEach(Self.rows.indices, id: \.self) { r in
                HStack(spacing: compact ? 5 : 7) {
                    ForEach(Self.rows[r].indices, id: \.self) { c in
                        keycap(Self.rows[r][c], focused: cursor == GridPos(row: r, col: c))
                            .onTapGesture {
                                cursor = GridPos(row: r, col: c)
                                press(Self.rows[r][c])
                            }
                    }
                }
            }
        }
        .frame(maxWidth: 560)
        .padding(compact ? 10 : 14)
        .background {
            RoundedRectangle(cornerRadius: 22, style: .continuous)
                .fill(.ultraThinMaterial)
                .environment(\.colorScheme, .dark)
        }
        .overlay {
            RoundedRectangle(cornerRadius: 22, style: .continuous)
                .strokeBorder(.white.opacity(0.12), lineWidth: 1)
        }
        .sensoryFeedback(.selection, trigger: cursor)
        .sensoryFeedback(.impact(weight: .light), trigger: pressTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: boundaryTick)
        .onAppear {
            wire()
            input.start()
        }
        .onDisappear {
            input.stop()
            haptics.stop()
        }
    }

    // MARK: - Keycaps

    @ViewBuilder private func keycap(_ key: Key, focused: Bool) -> some View {
        Group {
            switch key {
            case .char(let c):
                Text(String(c)).font(.geistFixed(18, .medium))
            case .space:
                Image(systemName: "space")
            case .backspace:
                Image(systemName: "delete.left")
            case .done:
                Label("Done", systemImage: "checkmark")
                    .font(.geist(15, .semibold, relativeTo: .callout))
            }
        }
        .foregroundStyle(focused ? Color.black : .white)
        .frame(maxWidth: .infinity, minHeight: compact ? 34 : 42)
        .background {
            RoundedRectangle(cornerRadius: 9, style: .continuous)
                .fill(focused ? AnyShapeStyle(Color.brand) : AnyShapeStyle(.white.opacity(0.08)))
        }
        .animation(.smooth(duration: 0.12), value: focused)
        .contentShape(Rectangle())
    }

    // MARK: - Input

    private func wire() {
        input.onMove = { move($0) }
        input.onConfirm = { press(Self.rows[cursor.row][cursor.col]) }
        input.onTertiary = { press(.backspace) }
        input.onSecondary = onDone
        input.onBack = onDone
    }

    private func move(_ direction: GamepadMenuInput.Direction) {
        var next = cursor
        switch direction {
        case .left: next.col -= 1
        case .right: next.col += 1
        case .up, .down:
            let row = cursor.row + (direction == .down ? 1 : -1)
            guard row >= 0, row < Self.rows.count else { return refuse() }
            // Map the column proportionally between rows of different widths, so e.g. Done
            // (rightmost of 3) goes up to the rightmost letters, not to "e".
            let from = max(1, Self.rows[cursor.row].count - 1)
            let to = Self.rows[row].count - 1
            next = GridPos(
                row: row,
                col: Int((Double(cursor.col) * Double(to) / Double(from)).rounded()))
        }
        guard next.row >= 0, next.row < Self.rows.count,
              next.col >= 0, next.col < Self.rows[next.row].count
        else { return refuse() }
        cursor = next
        haptics.move()
    }

    private func press(_ key: Key) {
        switch key {
        case .char(let c):
            if let allowed, !c.unicodeScalars.allSatisfy(allowed.contains) { return refuse() }
            text.append(c)
        case .space:
            if let allowed, !allowed.contains(" ") { return refuse() }
            text.append(" ")
        case .backspace:
            guard !text.isEmpty else { return refuse() }
            text.removeLast()
        case .done:
            haptics.confirm()
            onDone()
            return
        }
        pressTick &+= 1
        haptics.move()
    }

    /// Refused input (edge of the grid, a disallowed character, deleting nothing).
    private func refuse() {
        boundaryTick &+= 1
        haptics.boundary()
    }
}
#endif
