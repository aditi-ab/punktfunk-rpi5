// The one piece of gamepad-menu machinery shared by the host launcher (GamepadHomeView) and the
// library coverflow (LibraryCoverflowView): a horizontal, center-snapping carousel driven entirely
// by a controller (iOS/iPadOS/macOS) — or, on tvOS, by the NATIVE FOCUS ENGINE: every card is a
// focusable Button, so the Siri Remote and a game controller both navigate through the system
// (dpad/swipe moves focus, select activates, Menu backs out at the presentation level), and the
// cursor/scroll chase the focused card instead of the poll. The poll still runs on tvOS but
// carries ONLY the Y/X actions (library/settings) — buttons the focus engine has no concept of.
// The iOS/macOS poll-driven behavior is untouched by the tvOS mode.
//
// The scrolling is pure native SwiftUI — `.scrollTargetLayout()` + `.scrollTargetBehavior(.viewAligned)`
// snap exactly one item to center, and symmetric `.safeAreaPadding(.horizontal)` (sized off the live
// container width, so it's correct in an iPad split view too) lets the first and last item reach the
// middle. The CALLER owns each card's look, including its own `.scrollTransition` — this component
// deliberately applies none, so a screen can chain the VisualEffect-only transition modifiers without
// the generic wrapper here pushing the type-checker onto an overload it can't satisfy.
//
// Navigation authority: an internal `cursor` (an index), NOT the scroll-position binding, is the
// source of truth for where the gamepad is. `.scrollPosition(id:)` is a two-way binding and the
// scroll view WRITES intermediate ids into it while a programmatic animation is in flight — so
// reading the "current" item back out of it to compute the next one desyncs badly on a fast held
// stick (each move reads a lagging value and the cursor stalls before the last item). Instead a move
// advances `cursor` synchronously and points the scroll view at `items[cursor]`; scroll read-back is
// only allowed to move the cursor when the gamepad hasn't driven recently (i.e. a touch drag).
//
// Feedback is dual-channel by design: `.sensoryFeedback` ticks the DEVICE Taptic engine (for a
// handheld/touch user) and `MenuHaptics` ticks the CONTROLLER (for a couch user holding the pad).
// Both fire on a move, on confirm, and — for a non-wrapping list — a duller bump plus a short visual
// recoil when a move is refused at either end.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

struct GamepadCarousel<Item: Identifiable, Card: View>: View where Item.ID: Hashable {
    let items: [Item]
    /// Output only: the carousel WRITES the focused item's id here for the caller's detail panel.
    /// It is deliberately not what drives the scroll (see the file header).
    @Binding var selection: Item.ID?
    /// Every card is laid out at this fixed width so `.viewAligned` snapping + symmetric side
    /// insets center exactly one at a time.
    let itemWidth: CGFloat
    let spacing: CGFloat
    /// A → activate the centered item.
    let onActivate: (Item) -> Void
    /// Y → the screen's secondary action (e.g. open a host's library); nil disables it.
    var onSecondary: (() -> Void)?
    /// X → the screen's tertiary action (e.g. open settings); nil disables it.
    var onTertiary: (() -> Void)?
    /// B → back/dismiss; nil disables it (e.g. the root launcher has nowhere to go back to).
    var onBack: (() -> Void)?
    /// L1/R1 → jump this many items at once (clamped to the ends); 0 disables the shoulders.
    var shoulderJump: Int = 0
    /// Whether this carousel currently owns controller input. A presenting screen (e.g. the host
    /// launcher) stays mounted behind a presented one (e.g. the library), and both carousels would
    /// otherwise poll the SAME controller at once — driving both. The parent sets this false while
    /// something is presented on top so only the front-most carousel consumes the gamepad.
    var isActive: Bool = true
    @ViewBuilder let card: (Item) -> Card

    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    #if os(tvOS)
    /// tvOS: the focus engine is the navigation authority — `cursor`/`scrolledID` chase this,
    /// never the other way around (mirroring the poll's cursor-first discipline).
    @FocusState private var focusedID: Item.ID?
    #endif
    /// Authoritative gamepad cursor (index into `items`). Never assigned from scroll read-back
    /// while the gamepad is driving — that's the whole desync fix.
    @State private var cursor = 0
    /// The id the scroll view is aligned to — its own two-way `.scrollPosition` state.
    @State private var scrolledID: Item.ID?
    /// When the gamepad last moved the cursor; gates scroll read-back so a mid-animation write can't
    /// drag the cursor backward during a fast held direction.
    @State private var lastNav = Date.distantPast
    /// True while a programmatic scroll animation is in flight. `.scrollPosition(id:)` DROPS a new
    /// write that lands mid-animation — the scroll view stays stuck on the old item even though the
    /// binding updated — so we never issue one until the previous animation reports complete, then
    /// `commitScroll` re-targets the current cursor (coalescing a fast burst; see `commitScroll`).
    @State private var isScrolling = false
    /// A short horizontal recoil when a move is refused at a list end.
    @State private var bumpOffset: CGFloat = 0
    /// `.sensoryFeedback` fires on a change of its trigger; counters request a device tick for the
    /// confirm and end-stop events (moves trigger on `cursor`).
    @State private var activateTick = 0
    @State private var boundaryTick = 0

    /// Read-back from a touch drag is honoured only once the gamepad has been quiet this long
    /// (longer than a move animation, so overlapping held-stick moves never let it through).
    private let navSettle: TimeInterval = 0.4

    var body: some View {
        GeometryReader { geo in
            let inset = max(0, (geo.size.width - itemWidth) / 2)
            ScrollViewReader { proxy in
                ScrollView(.horizontal) {
                    HStack(spacing: spacing) {
                        ForEach(items) { item in
                            #if os(tvOS)
                            // A focusable Button per card: the focus engine does the navigating
                            // (remote swipes and pad dpad alike), select activates. The bare style
                            // below keeps the tile's own look — the `.scrollTransition` center pop
                            // is the focus treatment, since focus and center track each other.
                            Button { activate(item) } label: {
                                card(item)
                                    .frame(width: itemWidth)
                            }
                            .buttonStyle(ConsoleBareButtonStyle())
                            .focused($focusedID, equals: item.id)
                            .id(item.id)
                            #else
                            card(item)
                                .frame(width: itemWidth)
                                .contentShape(Rectangle())
                                .onTapGesture { tap(item) }
                            #endif
                        }
                    }
                    .frame(height: geo.size.height) // fill so shorter cards center vertically
                    .scrollTargetLayout()
                }
                // The two-way `.scrollPosition` + snap machinery serves the POLL/touch platforms.
                // Not on tvOS: that binding DROPS a write landing mid-animation (the very desync
                // the poll's cursor design exists to avoid — see the header), and on tvOS the
                // focus engine's own reveal-scrolls are always in flight, so drops were routine
                // ("navigation not reflected in the scroll view"). tvOS scrolls imperatively
                // below instead — scrollTo RE-TARGETS mid-animation (the GamepadMenuList pattern).
                #if !os(tvOS)
                .scrollPosition(id: $scrolledID)
                .scrollTargetBehavior(.viewAligned)
                #endif
                // .never, not .hidden — macOS's "always show scroll bars" setting overrides .hidden
                // and paints a scroller across the console strip.
                .scrollIndicators(.never)
                .scrollClipDisabled() // let the focused card scale up past the strip bounds
                .safeAreaPadding(.horizontal, inset)
                .offset(x: bumpOffset)
                #if os(tvOS)
                // Land initial focus on the first card (the launcher's first host / the coverflow's
                // first title) instead of wherever the engine guesses.
                .defaultFocus($focusedID, items.first?.id)
                // Focus moved (remote swipe / pad dpad) — chase it: cursor, detail selection,
                // controller detent, and an imperative center scroll.
                .onChange(of: focusedID) { _, newValue in
                    guard let idx = index(of: newValue), idx != cursor else { return }
                    cursor = idx
                    lastNav = Date()
                    haptics.move()
                    selection = newValue
                    withAnimation(.easeOut(duration: scrollAnim)) {
                        proxy.scrollTo(newValue, anchor: .center)
                    }
                }
                // The list changed under a stable focus (discovered hosts prepend tiles): the
                // content shifted but no focus change fires above — re-center the focused card.
                .onChange(of: items.map(\.id)) { _, _ in
                    if let id = focusedID { proxy.scrollTo(id, anchor: .center) }
                }
                #endif
            }
        }
        .sensoryFeedback(.selection, trigger: cursor)
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
        // Hand controller input to/from a screen presented on top (see `isActive`): a covered
        // carousel stops polling so it can't navigate behind the front-most one.
        .onChange(of: isActive) { _, active in
            if active {
                wire()
                input.start()
            } else {
                input.stop()
                haptics.stop()
            }
        }
        // A touch drag settles the scroll onto a new id: adopt it as the cursor. Ignored while a
        // programmatic scroll is animating (its own intermediate id write-backs would regress the
        // cursor) and briefly after a gamepad move (the same reason), so only a genuine touch drag
        // — which never sets `isScrolling` — moves the cursor here. Not on tvOS: there is no touch
        // drag, and the focus engine's own reveal-scrolls must never steal the cursor from focus.
        #if !os(tvOS)
        .onChange(of: scrolledID) { _, newValue in
            guard !isScrolling, Date().timeIntervalSince(lastNav) > navSettle else { return }
            guard let idx = index(of: newValue), idx != cursor else { return }
            cursor = idx
            selection = newValue
        }
        #endif
        // Re-seed a dropped/changed selection AND re-wire the input callbacks so they capture the
        // current `items` value (a plain array — unlike an observed object it would otherwise go
        // stale in the closures stored on `input`).
        .onChange(of: items.map(\.id)) { _, _ in
            reconcile()
            wire()
        }
    }

    // MARK: - Input wiring

    private func wire() {
        #if os(tvOS)
        // The focus engine owns move/confirm/back on tvOS (that's what keeps the Siri Remote
        // working on this screen — and what routes Menu through the system's back semantics).
        // The poll carries only the buttons focus has no concept of: Y/X, the screen actions.
        input.onSecondary = onSecondary
        input.onTertiary = onTertiary
        #else
        input.onMove = { move($0) }
        input.onConfirm = { activate() }
        input.onSecondary = onSecondary
        input.onTertiary = onTertiary
        input.onBack = onBack
        input.onShoulder = shoulderJump > 0 ? { shoulder(right: $0) } : nil
        #endif
    }

    private func move(_ direction: GamepadMenuInput.Direction) {
        let forward = direction == .right || direction == .down
        step(by: forward ? 1 : -1, clampAtEnds: false)
    }

    private func shoulder(right: Bool) {
        step(by: right ? shoulderJump : -shoulderJump, clampAtEnds: true)
    }

    /// Advance the cursor by `delta`. A single move (`clampAtEnds: false`) that would leave the list
    /// recoils + bumps; a shoulder jump (`clampAtEnds: true`) lands on the end item, bumping only if
    /// already there. The cursor is the authority — the scroll view is pointed at it, never read for it.
    private func step(by delta: Int, clampAtEnds: Bool) {
        guard !items.isEmpty else { return }
        var target = cursor + delta
        if target < 0 || target >= items.count {
            guard clampAtEnds else { return boundaryBump(forward: delta > 0) }
            target = min(max(target, 0), items.count - 1)
        }
        guard target != cursor else { return boundaryBump(forward: delta > 0) }
        cursor = target
        lastNav = Date()
        haptics.move()
        selection = items[target].id // text/detail updates immediately; the scroll chases
        commitScroll()
    }

    private let scrollAnim: TimeInterval = 0.24
    /// A hair past `scrollAnim` — long enough that the scroll has actually settled before the next
    /// write, short enough to stay responsive.
    private var scrollSettle: TimeInterval { scrollAnim + 0.05 }

    /// Drive the scroll toward the current cursor, one honoured write at a time. `.scrollPosition(id:)`
    /// DROPS a write that lands while a scroll is still animating, so we issue at most one at a time and
    /// re-target the LATEST cursor once it settles — coalescing a fast burst (hold OR quick flicks) and
    /// always converging on the final item, instead of getting stuck on the old card.
    ///
    /// The settle is timed by a plain timer rather than `withAnimation`'s completion: `scrolledID` is a
    /// discrete id, not an animatable value, so `withAnimation` has no tracked animation to fire a
    /// reliable completion against (it can fire early — which is exactly what let quick flicks slip a
    /// write through mid-scroll and stick). `asyncAfter` always fires, so `isScrolling` can never latch.
    private func commitScroll() {
        guard !isScrolling, cursor >= 0, cursor < items.count else { return }
        let id = items[cursor].id
        guard scrolledID != id else { return }
        isScrolling = true
        withAnimation(.easeOut(duration: scrollAnim)) { scrolledID = id }
        DispatchQueue.main.asyncAfter(deadline: .now() + scrollSettle) {
            MainActor.assumeIsolated {
                isScrolling = false
                commitScroll() // the cursor may have advanced while this scroll ran — chase it
            }
        }
    }

    private func activate() {
        guard cursor >= 0, cursor < items.count else { return }
        activate(items[cursor])
    }

    /// Shared confirm tail — the poll activates the cursor's item, a tvOS Button its own.
    private func activate(_ item: Item) {
        activateTick &+= 1
        haptics.confirm()
        onActivate(item)
    }

    /// Touch fallback matching the rest of the app: tapping the centered card activates it, tapping
    /// any other re-centers on it.
    private func tap(_ item: Item) {
        if let idx = index(of: item.id), idx == cursor {
            activate()
        } else if let idx = index(of: item.id) {
            cursor = idx
            lastNav = Date()
            haptics.move()
            selection = item.id
            commitScroll()
        }
    }

    // MARK: - Selection housekeeping

    private func index(of id: Item.ID?) -> Int? {
        guard let id else { return nil }
        return items.firstIndex { $0.id == id }
    }

    /// Keep `cursor`/`scrolledID`/`selection` consistent with `items`: seed on appear, and on a list
    /// change keep the same focused item when it survives, else clamp the cursor into range.
    private func reconcile() {
        guard !items.isEmpty else {
            cursor = 0
            if scrolledID != nil { scrolledID = nil }
            if selection != nil { selection = nil }
            return
        }
        if let sid = scrolledID, let idx = index(of: sid) {
            cursor = idx
            if selection != sid { selection = sid }
        } else {
            let idx = min(max(cursor, 0), items.count - 1)
            cursor = idx
            let id = items[idx].id
            scrolledID = id
            selection = id
        }
        #if os(tvOS)
        // Keep real focus on the reconciled item when its old target vanished from the list —
        // the engine would otherwise pick a neighbour by geometry and drag the cursor with it.
        if focusedID == nil || index(of: focusedID) == nil, cursor < items.count {
            focusedID = items[cursor].id
        }
        #endif
    }

    private func boundaryBump(forward: Bool) {
        boundaryTick &+= 1
        haptics.boundary()
        let recoil: CGFloat = forward ? -16 : 16
        withAnimation(.spring(response: 0.16, dampingFraction: 0.42)) { bumpOffset = recoil }
        withAnimation(.spring(response: 0.34, dampingFraction: 0.7).delay(0.1)) { bumpOffset = 0 }
    }
}
#endif
