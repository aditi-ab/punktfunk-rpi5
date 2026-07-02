// The vertical sibling of GamepadCarousel (iOS/iPadOS/macOS): a controller-driven focus list for
// the gamepad UI's form-like screens (GamepadSettingsView, GamepadAddHostView). Up/down moves a
// focus bar through the rows, left/right adjusts the focused row's value, A activates it, B backs
// out. The CALLER owns each row's look (it gets the focused flag); this component owns the focus
// cursor, controller polling, haptics, and keeping the focused row scrolled into view.
//
// Unlike the carousel there is no snapping and no `.scrollPosition` two-way binding to fight: the
// cursor is plainly authoritative, the scroll view just chases it with `scrollTo`. Touch stays a
// first-class fallback — tapping a row focuses AND activates it (rows are always fully visible, so
// the carousel's "first tap re-centers" step would only add friction here), and free finger
// scrolling is never hijacked back to the focused row until the next controller move.
//
// Feedback is dual-channel like the carousel: `.sensoryFeedback` ticks the DEVICE Taptic engine,
// `MenuHaptics` ticks the CONTROLLER. Moves and value changes get the crisp detent; a refused
// move at either end gets the dull boundary thud plus a short vertical recoil.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)

struct GamepadMenuList<Item: Identifiable, Row: View>: View where Item.ID: Hashable {
    let items: [Item]
    /// Output only: the list WRITES the focused item's id here (e.g. for a caller's hint bar).
    @Binding var focusID: Item.ID?
    /// Left/right on the focused row. Return whether the value actually changed — true plays the
    /// move detent, false the boundary thud (end of a clamped range, or nothing to adjust).
    var onAdjust: ((Item, Int) -> Bool)?
    /// A → activate the focused row (toggle it, open it, run it — the caller decides).
    let onActivate: (Item) -> Void
    /// B → back/dismiss; nil disables it.
    var onBack: (() -> Void)?
    /// Whether this list currently owns controller input — same handoff contract as
    /// GamepadCarousel's `isActive` (a covered screen must stop polling the shared pad).
    var isActive: Bool = true
    @ViewBuilder let row: (Item, _ focused: Bool) -> Row

    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    /// Authoritative focus cursor (index into `items`).
    @State private var cursor = 0
    /// A short vertical recoil when a move is refused at a list end.
    @State private var bumpOffset: CGFloat = 0
    /// `.sensoryFeedback` counters (see GamepadCarousel): device ticks for activate / value-change
    /// / end-stop events; moves trigger on `cursor` itself.
    @State private var activateTick = 0
    @State private var adjustTick = 0
    @State private var boundaryTick = 0

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView(.vertical) {
                LazyVStack(spacing: 6) {
                    ForEach(Array(items.enumerated()), id: \.element.id) { idx, item in
                        row(item, idx == cursor && isActive)
                            .contentShape(Rectangle())
                            .onTapGesture { tap(idx) }
                            .id(item.id)
                    }
                }
                .padding(.vertical, 10)
            }
            // .never, not .hidden — macOS's "always show scroll bars" setting overrides .hidden.
            .scrollIndicators(.never)
            .offset(y: bumpOffset)
            .onChange(of: cursor) { _, newValue in
                guard newValue >= 0, newValue < items.count else { return }
                withAnimation(.easeOut(duration: 0.2)) {
                    proxy.scrollTo(items[newValue].id)
                }
            }
        }
        .sensoryFeedback(.selection, trigger: cursor)
        .sensoryFeedback(.selection, trigger: adjustTick)
        .sensoryFeedback(.impact(weight: .medium), trigger: activateTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: boundaryTick)
        .onAppear {
            reconcile()
            wire()
            if isActive { input.start() }
        }
        .onDisappear {
            input.stop()
            haptics.stop()
        }
        .onChange(of: isActive) { _, active in
            if active {
                wire()
                input.start()
            } else {
                input.stop()
                haptics.stop()
            }
        }
        // Re-seed a dropped focus AND re-wire the input callbacks so they capture the current
        // `items` value (a plain array — it would otherwise go stale in the stored closures).
        .onChange(of: items.map(\.id)) { _, _ in
            reconcile()
            wire()
        }
    }

    // MARK: - Input wiring

    private func wire() {
        input.onMove = { direction in
            switch direction {
            case .up: step(by: -1)
            case .down: step(by: 1)
            case .left: adjust(by: -1)
            case .right: adjust(by: 1)
            }
        }
        input.onConfirm = { activate() }
        input.onBack = onBack
    }

    private func step(by delta: Int) {
        guard !items.isEmpty else { return }
        let target = cursor + delta
        guard target >= 0, target < items.count else { return boundaryBump(forward: delta > 0) }
        cursor = target
        focusID = items[target].id
        haptics.move()
    }

    private func adjust(by delta: Int) {
        guard let onAdjust, cursor >= 0, cursor < items.count else { return }
        if onAdjust(items[cursor], delta) {
            adjustTick &+= 1
            haptics.move()
        } else {
            boundaryTick &+= 1
            haptics.boundary()
        }
    }

    private func activate() {
        guard cursor >= 0, cursor < items.count else { return }
        activateTick &+= 1
        haptics.confirm()
        onActivate(items[cursor])
    }

    /// Touch fallback: a tap focuses the row and activates it in one go.
    private func tap(_ idx: Int) {
        guard idx >= 0, idx < items.count else { return }
        if cursor != idx {
            cursor = idx
            focusID = items[idx].id
        }
        activate()
    }

    /// Keep `cursor`/`focusID` consistent with `items`: seed on appear; on a list change keep the
    /// same focused item when it survives, else clamp the cursor into range.
    private func reconcile() {
        guard !items.isEmpty else {
            cursor = 0
            if focusID != nil { focusID = nil }
            return
        }
        if let id = focusID, let idx = items.firstIndex(where: { $0.id == id }) {
            cursor = idx
        } else {
            cursor = min(max(cursor, 0), items.count - 1)
            focusID = items[cursor].id
        }
    }

    private func boundaryBump(forward: Bool) {
        boundaryTick &+= 1
        haptics.boundary()
        let recoil: CGFloat = forward ? -14 : 14
        withAnimation(.spring(response: 0.16, dampingFraction: 0.42)) { bumpOffset = recoil }
        withAnimation(.spring(response: 0.34, dampingFraction: 0.7).delay(0.1)) { bumpOffset = 0 }
    }
}
#endif
