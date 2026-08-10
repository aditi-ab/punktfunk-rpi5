// The vertical sibling of GamepadCarousel (iOS/iPadOS/macOS/tvOS): a controller-driven focus list
// for the gamepad UI's form-like screens (GamepadSettingsView, GamepadAddHostView). Up/down moves
// a focus bar through the rows, left/right adjusts the focused row's value, A activates it, B
// backs out. The CALLER owns each row's look (it gets the focused flag); this component owns the
// focus cursor, controller polling, haptics, and keeping the focused row scrolled into view.
//
// On tvOS the rows are focusable Buttons and the NATIVE FOCUS ENGINE replaces the poll entirely
// (Siri Remote and pads both drive it: up/down moves focus, select activates, Menu — via
// onExitCommand — backs out). Left/right value-adjust isn't wired there; select cycles a value
// forward exactly like A does elsewhere, the standard tvOS settings interaction. The iOS/macOS
// poll-driven behavior is untouched by the tvOS mode.
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
#if os(iOS) || os(macOS) || os(tvOS)

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
    /// L1 (`-1`) / R1 (`+1`) — a step SIDEWAYS out of the list: the settings screen's section
    /// tabs. Wired on tvOS too, where the focus engine owns up/down but leaves the shoulders
    /// to the poll. nil ⇒ the shoulders do nothing.
    var onShoulder: ((Int) -> Void)?
    /// Whether this list currently owns controller input — same handoff contract as
    /// GamepadCarousel's `isActive` (a covered screen must stop polling the shared pad).
    var isActive: Bool = true
    @ViewBuilder let row: (Item, _ focused: Bool) -> Row

    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    #if os(tvOS)
    /// tvOS: the focus engine is the navigation authority for UP/DOWN — `cursor` chases this, so
    /// the caller's `focused` row styling always matches real system focus. LEFT/RIGHT adjust
    /// comes from the POLL (see `wire`), never from `.onMoveCommand`: the command stream is
    /// 4-way with no axis data (diagonal scroll wobble buckets into left/right), and its
    /// interception of up/down proved INPUT-SOURCE-DEPENDENT on hardware — keyboard arrows were
    /// intercepted but a pad's dpad was not, so programmatic stepping double-moved every press.
    @FocusState private var focusedID: Item.ID?
    #endif
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
                        #if os(tvOS)
                        // A focusable Button per row: the engine moves between them, select
                        // activates (`tap` keeps the cursor in step before firing). The row's
                        // own `focused` styling is the focus treatment — the bare style adds
                        // no system chrome on top of it.
                        Button { tap(idx) } label: {
                            row(item, focusedID == item.id)
                        }
                        .buttonStyle(ConsoleBareButtonStyle())
                        .focused($focusedID, equals: item.id)
                        .id(item.id)
                        #else
                        row(item, idx == cursor && isActive)
                            .contentShape(Rectangle())
                            .onTapGesture { tap(idx) }
                            .id(item.id)
                        #endif
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
        #if os(tvOS)
        // Focus moved (remote swipe / pad dpad) — keep the cursor, the caller's focusID mirror,
        // and the controller detent in step. Menu = the list's back action (both tvOS callers
        // pass one; the screen behind would otherwise catch the press and peel too far).
        .onChange(of: focusedID) { _, newValue in
            guard let id = newValue, let idx = items.firstIndex(where: { $0.id == id }),
                  idx != cursor else { return }
            cursor = idx
            focusID = id
            haptics.move()
        }
        .defaultFocus($focusedID, items.first?.id)
        .onExitCommand { onBack?() }
        #endif
        .sensoryFeedback(.selection, trigger: cursor)
        .sensoryFeedback(.selection, trigger: adjustTick)
        .sensoryFeedback(.impact(weight: .medium), trigger: activateTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: boundaryTick)
        #if os(iOS) || os(macOS)
        // Hardware keyboard: up/down step the focus bar, left/right adjust the focused row's
        // value (exactly what the stick does), Return activates, Esc backs out.
        .gamepadKeyNavigation(
            active: isActive,
            onMove: { direction in
                switch direction {
                case .up: step(by: -1)
                case .down: step(by: 1)
                case .left: adjust(by: -1)
                case .right: adjust(by: 1)
                }
            },
            onConfirm: { activate() },
            onBack: onBack)
        #endif
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
        #if os(tvOS)
        // The focus engine owns up/down and select (Button rows) and Menu (onExitCommand) — the
        // poll carries ONLY the horizontal axis, where its dominant-axis deadzone + hold-repeat
        // are exactly the adjust feel the other platforms have, and where the focus engine has
        // nothing to move to in a vertical list. Vertical poll directions are deliberately
        // dropped: acting on them would double the engine's own focus moves. (The Siri Remote
        // never reaches this poll — no extended profile — so remote users cycle values with
        // select instead, which `activate` already does.)
        input.onMove = { direction in
            switch direction {
            case .left: adjust(by: -1)
            case .right: adjust(by: 1)
            case .up, .down: break
            }
        }
        input.onShoulder = { forward in onShoulder?(forward ? 1 : -1) }
        #else
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
        input.onShoulder = { forward in onShoulder?(forward ? 1 : -1) }
        #endif
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
        #if os(tvOS)
        // Keep real focus on the reconciled row when its old target vanished from the list.
        if focusedID == nil || !items.contains(where: { $0.id == focusedID }), cursor < items.count {
            focusedID = items[cursor].id
        }
        #endif
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
