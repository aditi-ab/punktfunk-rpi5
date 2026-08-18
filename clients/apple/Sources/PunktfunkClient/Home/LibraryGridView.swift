// The gamepad library's GRID arrangement — the desktop console's grid (`screens/library.rs`),
// in SwiftUI: 2:3 poster cells in rows that scroll vertically behind a viewport (a move scrolls
// only as far as it must to keep the focused cell in view; a restored cursor is seated a third of
// the way down), the launcher rows sitting squarely above the game rows with a heading between. Every cell is equally readable (no coverflow recede); focus is a ×1.06 pop, an
// accent ring OUTSIDE the cover, and the shadow. The `Resume` badge keeps the coverflow's corner —
// the same badge in two different corners on two arrangements of the same shelf would read as
// two different badges.
//
// The cursor is `LibraryGridCursor` over ONE `LibraryGridShape` that this view builds from what
// it actually laid out (the column count is a render fact, published before navigation is
// allowed — a move that arrives before the first layout is declined rather than guessed). ◀▶
// walk the row and refuse at its true ends with a bump; ▲▼ change row carrying the remembered
// column; L1/R1 page three rows and land on the ends; ▲ from the TOP row hands the controller to
// the bar. The detail band, legend and backdrop are `LibraryConsoleView`'s.
//
// Sizing follows the desktop's `k`: cells are 150×225 design units, gap 16, margin 48, scaled by
// `min(width, height) / 800` clamped to 0.75…3, columns = what fits, clamped 2…8. A Deck-shaped
// window gets 7, an iPad in landscape 6, a phone in landscape 6 small ones, an Apple TV 8.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

struct LibraryGridView: View {
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    /// The shelf in DISPLAY order — collated by the container (launchers lead, then the sort).
    let games: [GameEntry]
    let artLoader: (any LibraryArtSource)?
    /// The focused title, published for the container's detail band and legend.
    @Binding var focusID: String?
    var onLaunch: ((String) -> Void)?
    var running: [String: RunningGame] = [:]
    /// The title to open on — see `LibraryCoverflowView.initialSelection`.
    var initialSelection: String?
    var onBack: (() -> Void)?
    var onSecondary: (() -> Void)?
    var onTertiary: (() -> Void)?
    /// ▲ from the top row: hand the controller to the bar.
    var onUp: (() -> Void)?
    var controllerActive = true

    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    #if os(tvOS)
    /// tvOS: the focus engine is the navigation authority — `focusID` chases it.
    @FocusState private var tvFocus: String?
    #endif
    /// The column the user last CHOSE (a horizontal move), carried across vertical ones.
    @State private var colHint = 0
    /// Columns as laid out — 0 until the first layout pass, during which navigation declines.
    @State private var cols = 0
    /// Boundary recoil, deflected along the axis pushed.
    @State private var bump = CGSize.zero
    @State private var boundaryTick = 0
    @State private var activateTick = 0
    /// The field's entrance, one timeline (see GamepadCarousel's `entranceProgress`).
    @State private var entranceProgress: Double = 0
    @State private var entranceArmed = false
    @State private var entranceAnchor = 0
    @State private var artSettled = 0
    @State private var artWaitOver = false
    /// The first scroll on mount is seated (no animation), later ones are sprung.
    @State private var seated = false
    /// The field's offset — ONE sprung scalar, this view's own (see `body`).
    @State private var scrollY: CGFloat = 0
    /// Where the last scroll started, so rows stay rendered across the whole travel.
    @State private var prevScrollY: CGFloat = 0
    /// A touch drag's starting offset (iOS).
    @State private var dragStart: CGFloat?

    /// Launchers lead by construction (`LibraryOrder` / `LibraryCollation`), so the launcher run
    /// is a prefix.
    private var launcherCount: Int { games.prefix { $0.isLauncher }.count }
    private var shape: LibraryGridShape {
        LibraryGridShape(len: games.count, cols: cols, launchers: launcherCount)
    }
    private var cursor: Int {
        guard let id = focusID, let i = games.firstIndex(where: { $0.id == id }) else { return 0 }
        return i
    }
    private var contentReady: Bool {
        artWaitOver || artSettled >= min(6, games.count)
    }

    var body: some View {
        GeometryReader { geo in
            let g = GridGeometry(size: geo.size, len: games.count, launchers: launcherCount)
            // The field: every row at its exact place, the whole thing shifted by `scrollY`, and
            // clipped to the viewport. NOT a ScrollView — this view owns the offset. A
            // ScrollView-driven grid needed `scrollTo` against a lazy layout, and on glass one
            // step down read as TWO impulses (the scroll settled, then moved again as the lazy
            // rows laid out under it). All of this geometry is fixed and known, so the offset is
            // computed and sprung here, exactly as the desktop console does it — and rows are
            // culled by the same arithmetic instead of by a lazy container's guess.
            ZStack(alignment: .topLeading) {
                heading(g.shape.split > 0 ? "LAUNCHERS" : nil, height: g.headingH, k: g.k)
                    .padding(.horizontal, g.margin)
                if g.shape.split > 0 {
                    heading("GAMES", height: g.headingH, k: g.k)
                        .padding(.horizontal, g.margin)
                        .offset(y: g.gamesHeadingTop)
                }
                ForEach(0..<g.shape.rows, id: \.self) { r in
                    if g.isNear(row: r, scroll: scrollY, previous: prevScrollY) {
                        row(r, g: g)
                            .offset(x: g.margin, y: g.rowTop(r))
                    }
                }
            }
            .frame(width: geo.size.width, height: g.contentH, alignment: .topLeading)
            .offset(y: -scrollY)
            .frame(width: geo.size.width, height: geo.size.height, alignment: .topLeading)
            .clipped()
            .contentShape(Rectangle())
            .offset(bump)
            #if os(iOS)
            // A finger scrolls the field directly (no momentum — the pad and the cursor are the
            // primary drivers, this keeps a touch user from being stuck).
            .simultaneousGesture(
                DragGesture(minimumDistance: 8)
                    .onChanged { value in
                        if dragStart == nil { dragStart = scrollY }
                        prevScrollY = scrollY
                        scrollY = min(max((dragStart ?? 0) - value.translation.height, 0), g.maxScroll)
                    }
                    .onEnded { _ in dragStart = nil })
            #endif
            .onAppear {
                if cols != g.cols { cols = g.cols }
                seed()
                wire()
                if controllerActive { input.start() }
                // Seated, not sprung: the field opens where the cursor is (a restored cursor a
                // third of the way down — the desktop's `view_h·0.34`).
                scrollY = g.seat(row: g.shape.cell(of: cursor).row)
                prevScrollY = scrollY
                seated = true
                armEntrance()
            }
            .onChange(of: g.cols) { _, c in
                cols = c
                // A resize re-flows the rows; keep the focused title in view, seated.
                scrollY = g.reveal(row: g.shape.cell(of: cursor).row, from: scrollY)
                prevScrollY = scrollY
            }
            .onChange(of: focusID) { _, id in
                guard id != nil, seated else { return }
                // A MOVE scrolls only as far as it must to keep the focused cell (and its ring)
                // in view — never re-seats it: on glass, the "focused row rides at 34 %" rule cut
                // the row above half away after ONE step down with the whole field still in reach.
                // On the top row, all the way up so the heading band shows too.
                let row = g.shape.cell(of: cursor).row
                let target = row == 0 ? 0 : g.reveal(row: row, from: scrollY)
                guard target != scrollY else { return }
                prevScrollY = scrollY
                if reduceMotion {
                    scrollY = target
                } else {
                    // `springs::FOCUS` — the desktop's scroll chase. ONE sprung scalar.
                    withAnimation(.spring(response: 0.30, dampingFraction: 0.80)) { scrollY = target }
                }
            }
            .onChange(of: games.map(\.id)) { _, ids in
                // The list changed under us (a resort, a running title arriving): keep the
                // focused TITLE if it survives, else clamp — never reset to the first cell.
                if let id = focusID, ids.contains(id) { return }
                focusID = ids.isEmpty ? nil : ids[min(cursor, ids.count - 1)]
                wire()
            }
            .onChange(of: contentReady) { _, _ in armEntrance() }
            .onChange(of: controllerActive) { _, active in
                if active { input.start() } else { input.stop() }
            }
            #if os(tvOS)
            // Land initial focus on the seeded cell, then chase focus into `focusID`.
            .defaultFocus($tvFocus, focusID)
            .onChange(of: tvFocus) { _, id in
                guard let id, id != focusID, let i = games.firstIndex(where: { $0.id == id })
                else { return }
                haptics.move()
                colHint = shape.cell(of: i).col
                focusID = id
            }
            #endif
            .onDisappear {
                input.stop()
                haptics.stop()
            }
            #if os(iOS) || os(macOS)
            // Hardware keyboard: arrows move the cursor, Return launches — same as the strip.
            .gamepadKeyNavigation(
                active: controllerActive,
                onMove: { move($0) },
                onConfirm: { activate() },
                onBack: onBack)
            #endif
            .sensoryFeedback(.selection, trigger: focusID)
            .sensoryFeedback(.impact(weight: .medium), trigger: activateTick)
            .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: boundaryTick)
        }
        .task {
            try? await Task.sleep(for: .milliseconds(700))
            artWaitOver = true
        }
    }

    /// One row of cells at their exact places. Fixed-width columns so the two sections' cells
    /// align vertically and a partial last row sits leading — exactly the shape the cursor was told.
    private func row(_ r: Int, g: GridGeometry) -> some View {
        let start = g.shape.rowStart(r)
        let n = g.shape.rowLen(r)
        return HStack(alignment: .top, spacing: g.gap) {
            ForEach(start..<(start + n), id: \.self) { index in
                let game = games[index]
                #if os(tvOS)
                // A focusable Button per cell: the focus engine does the navigating (remote
                // swipes and pad dpad alike — a Siri Remote is no extended gamepad, so the poll
                // above never sees it), select activates. The bare style keeps the cell's own
                // look; the ring + pop below is the focus treatment, since `focusID` chases focus.
                Button { activate() } label: {
                    cell(game, index: index, width: g.cellW, height: g.cellH, k: g.k, shape: g.shape)
                }
                .buttonStyle(ConsoleBareButtonStyle())
                .focused($tvFocus, equals: game.id)
                #else
                cell(game, index: index, width: g.cellW, height: g.cellH, k: g.k, shape: g.shape)
                #endif
            }
        }
        // Rendered rows are keyed on their index so a row that scrolls out and back in remounts
        // fresh (its posters re-request from the loader's cache), like the desktop's cull.
        .id("row-\(r)")
    }

    /// A heading band: leading, tracked, `fg(0.45)` — or the same band empty (the top inset).
    /// The caption sits mid-band: the air on either side is what a focused cell's ×1.06 pop and
    /// its ring rise into, from the row above as much as the row below, so a heading never
    /// collides with either.
    private func heading(_ text: String?, height: CGFloat, k: CGFloat) -> some View {
        HStack {
            if let text {
                Text(text)
                    .font(.geist(11 * k, .semibold, relativeTo: .caption2))
                    .tracking(1.4)
                    .foregroundStyle(ink.fg(0.45))
            }
        }
        .frame(height: height, alignment: .leading)
    }

    private func cell(
        _ game: GameEntry, index: Int, width: CGFloat, height: CGFloat, k: CGFloat,
        shape: LibraryGridShape
    ) -> some View {
        let focused = game.id == focusID
        let corner = 12 * k
        return PosterImage(
            candidates: game.art.posterCandidates, title: game.title, loader: artLoader,
            icon: game.iconToken,
            drawnSize: CGSize(width: width, height: height),
            onLoaded: { artSettled += 1 })
            .frame(width: width, height: height)
            .clipShape(RoundedRectangle(cornerRadius: corner, style: .continuous))
            .overlay(alignment: .topTrailing) {
                if running[game.id] != nil { RunningBadge(solid: true) }
            }
            .overlay {
                RoundedRectangle(cornerRadius: corner, style: .continuous)
                    .strokeBorder(ink.fg(0.12), lineWidth: 1)
            }
            // The focus ring goes OUTSIDE the cover, so it reads as a ring around it rather than
            // a border painted onto it.
            .overlay {
                RoundedRectangle(cornerRadius: corner + 3, style: .continuous)
                    .inset(by: -3)
                    .strokeBorder(ink.accent(0.9), lineWidth: 2)
                    .opacity(focused ? 1 : 0)
            }
            .shadow(color: ink.shadow(focused ? 0.5 : 0.22), radius: focused ? 14 : 8, y: focused ? 10 : 5)
            .scaleEffect(focused ? 1.06 : 1)
            // `springs::FOCUS` — the pop, with a whisker of overshoot.
            .animation(
                reduceMotion ? .easeOut(duration: 0.12) : .spring(response: 0.30, dampingFraction: 0.80),
                value: focused)
            .modifier(entrance(index: index, shape: shape))
            .zIndex(focused ? 1 : 0)
            #if !os(tvOS)
            // Pointer/touch: a press on the focused cell activates, any other only brings it to
            // front — the carousel's rule.
            .contentShape(RoundedRectangle(cornerRadius: corner, style: .continuous))
            .onTapGesture {
                if focused { activate() } else { focus(index) }
            }
            #endif
    }

    // MARK: - Entrance

    private func armEntrance() {
        guard !entranceArmed, contentReady, !games.isEmpty else { return }
        entranceArmed = true
        entranceAnchor = cursor
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
            withAnimation(
                reduceMotion ? .easeOut(duration: 0.28) : .linear(duration: CardEntrance.total)
            ) {
                entranceProgress = 1
            }
        }
    }

    /// The cell's share of the field's entrance, fanning on `|Δrow| + |Δcol|` from the focused
    /// cell (the desktop's grid rule) — the same stagger and cap the strip uses.
    private func entrance(index: Int, shape: LibraryGridShape) -> GridCellEntrance {
        let a = shape.cell(of: min(entranceAnchor, max(shape.len - 1, 0)))
        let c = shape.cell(of: index)
        let distance = abs(a.row - c.row) + abs(a.col - c.col)
        let delay = min(CardEntrance.maxDelay, Double(distance) * 0.12)
        return GridCellEntrance(
            progress: entranceProgress, start: delay / CardEntrance.total, reduceMotion: reduceMotion)
    }

    // MARK: - Input

    private func seed() {
        guard !games.isEmpty else { return }
        if let id = focusID, games.contains(where: { $0.id == id }) {
            colHint = shape.cell(of: cursor).col
            return
        }
        if let id = initialSelection, games.contains(where: { $0.id == id }) {
            focusID = id
        } else {
            focusID = games[0].id
        }
        colHint = shape.cell(of: cursor).col
    }

    private func wire() {
        input.onMove = { move($0) }
        input.onConfirm = { activate() }
        input.onSecondary = onSecondary
        input.onTertiary = onTertiary
        input.onBack = onBack
        input.onShoulder = { right in page(forward: right) }
    }

    private func move(_ direction: GamepadMenuInput.Direction) {
        let dir: LibraryGridDirection
        switch direction {
        case .left: dir = .left
        case .right: dir = .right
        case .up: dir = .up
        case .down: dir = .down
        }
        step(dir)
    }

    private func page(forward: Bool) {
        step(forward ? .pageForward : .pageBack)
    }

    private func step(_ dir: LibraryGridDirection) {
        // No layout yet: decline rather than guess (the desktop's `grid_cols_last`).
        guard cols > 0, !games.isEmpty else { return }
        let s = shape
        switch LibraryGridCursor.step(cursor, shape: s, colHint: colHint, direction: dir) {
        case .moved(let to):
            colHint = LibraryGridCursor.colHint(shape: s, previous: colHint, direction: dir, landed: to)
            haptics.move()
            focusID = games[to].id
        case .boundary:
            // ▲ off the top row is the way to the bar, not a wall.
            if dir == .up, s.cell(of: cursor).row == 0, let onUp {
                onUp()
                return
            }
            boundaryBump(dir)
        }
    }

    private func focus(_ index: Int) {
        guard games.indices.contains(index) else { return }
        colHint = shape.cell(of: index).col
        haptics.move()
        focusID = games[index].id
    }

    private func activate() {
        guard let id = focusID, games.contains(where: { $0.id == id }) else { return }
        activateTick &+= 1
        haptics.confirm()
        onLaunch?(id)
    }

    /// The recoil is deflected along the axis pushed — horizontal shifts x, vertical shifts the
    /// scroll (here, the field). Travel dropped under Reduce Motion, haptic kept.
    private func boundaryBump(_ dir: LibraryGridDirection) {
        boundaryTick &+= 1
        haptics.boundary()
        guard !reduceMotion else { return }
        let recoil: CGSize
        switch dir {
        case .left: recoil = CGSize(width: 16, height: 0)
        case .right: recoil = CGSize(width: -16, height: 0)
        case .up, .pageBack: recoil = CGSize(width: 0, height: 16)
        case .down, .pageForward: recoil = CGSize(width: 0, height: -16)
        }
        withAnimation(.spring(response: 0.16, dampingFraction: 0.42)) { bump = recoil }
        withAnimation(.spring(response: 0.34, dampingFraction: 0.7).delay(0.1)) { bump = .zero }
    }
}

/// The grid's geometry, computed once per layout pass from the viewport — every number the field
/// and the cursor share. Cells are 150×225 design units, gap 16, margin 48, heading band 30, a
/// 10-unit label gap under each row, all × `k = min(w, h)/800` clamped 0.75…3; columns = what
/// fits, clamped 2…8.
private struct GridGeometry {
    let k: CGFloat
    let cellW: CGFloat, cellH: CGFloat, gap: CGFloat, margin: CGFloat
    let headingH: CGFloat, labelGap: CGFloat
    /// The air a focused cell's ring and ×1.06 pop need past its frame.
    let halo: CGFloat
    let cols: Int
    let shape: LibraryGridShape
    let viewH: CGFloat

    init(size: CGSize, len: Int, launchers: Int) {
        k = min(max(min(size.width, size.height) / 800, 0.75), 3)
        cellW = 150 * k; cellH = 225 * k; gap = 16 * k; margin = 48 * k
        headingH = 30 * k; labelGap = 10 * k; halo = 10 * k
        let avail = size.width - 2 * margin
        cols = min(max(Int((avail + gap) / (cellW + gap)), 2), 8)
        shape = LibraryGridShape(len: len, cols: cols, launchers: launchers)
        viewH = size.height
    }

    /// Row pitch: the cell, the gap, and the label air under it.
    var pitch: CGFloat { cellH + gap + labelGap }
    /// The GAMES heading's top: after the launcher rows.
    var gamesHeadingTop: CGFloat { headingH + CGFloat(shape.splitRow) * pitch }

    /// A row's top edge. Row 0 sits under the (unconditional) top heading band; the game rows
    /// under the launcher rows sit under a second band.
    func rowTop(_ r: Int) -> CGFloat {
        if shape.split > 0, r >= shape.splitRow {
            return gamesHeadingTop + headingH + CGFloat(r - shape.splitRow) * pitch
        }
        return headingH + CGFloat(r) * pitch
    }

    /// The whole field, plus a trailing band so the last row keeps its pop and ring at max scroll.
    var contentH: CGFloat {
        guard shape.rows > 0 else { return headingH * 2 }
        return rowTop(shape.rows - 1) + cellH + labelGap + headingH
    }
    var maxScroll: CGFloat { max(0, contentH - viewH) }

    /// Whether a row is drawn: it overlaps the span the field is travelling across (from the
    /// previous offset to the current one), with one row of look-ahead on either side.
    func isNear(row r: Int, scroll: CGFloat, previous: CGFloat) -> Bool {
        let lo = min(scroll, previous) - pitch
        let hi = max(scroll, previous) + viewH + pitch
        let top = rowTop(r)
        return top < hi && top + cellH > lo
    }

    /// The offset that keeps `row` (ring and pop included) inside the viewport, moving no
    /// further than it must from `current`.
    func reveal(row r: Int, from current: CGFloat) -> CGFloat {
        let top = rowTop(r) - halo
        let bottom = rowTop(r) + cellH + halo
        var y = current
        if top < y { y = top }
        if bottom > y + viewH { y = bottom - viewH }
        return min(max(y, 0), maxScroll)
    }

    /// Where a restored cursor is SEATED on mount: a third of the way down the viewport (the
    /// desktop's `view_h·0.34`), clamped; row 0 is simply the top.
    func seat(row r: Int) -> CGFloat {
        guard r > 0 else { return 0 }
        return min(max(rowTop(r) - viewH * 0.34, 0), maxScroll)
    }
}

/// How a grid cell arrives when its field does: small, low and invisible, then it grows and rises
/// into place on a spring soft enough to overshoot — the coverflow's `CardEntrance` without the
/// turn (the desktop's grid entrance has rise, scale and fade; the turn is the shelf's alone).
struct GridCellEntrance: ViewModifier, Animatable {
    var progress: Double
    let start: Double
    let reduceMotion: Bool

    var animatableData: Double {
        get { progress }
        set { progress = newValue }
    }

    func body(content: Content) -> some View {
        let span = CardEntrance.perCard / CardEntrance.total
        let raw = min(max((progress - start) / span, 0), 1)
        let travel = Self.easeOutBack(raw)
        let fade = Self.easeOut(min(raw / 0.34, 1))
        let away = reduceMotion ? 0 : 1 - travel
        return content
            .opacity(reduceMotion ? raw : fade)
            .scaleEffect(1 - 0.26 * away)
            .offset(y: 34 * away)
    }

    private static func easeOutBack(_ t: Double) -> Double {
        let c1 = 1.2, c3 = c1 + 1
        let u = t - 1
        return 1 + c3 * u * u * u + c1 * u * u
    }

    private static func easeOut(_ t: Double) -> Double {
        1 - pow(1 - t, 3)
    }
}
#endif
