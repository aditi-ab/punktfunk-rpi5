// The gamepad library's live view/sort bar — the desktop console's `screens/library.rs` bar, in
// SwiftUI: one band across the top of the field with two anchored pill groups, `SORT` (Default ·
// A–Z · Platform · Store) leading and `VIEW` (Shelf · Grid) trailing. Both controls apply by
// WRITING THE SETTING (`LibraryConsoleView` owns the keys), so the field re-sorts / re-arranges
// live under the strip and the Interface settings row can never disagree with it.
//
// Focus is the WHOLE bar: an accent wash over the pill row — no border, no halo, no glass. The
// C7 note on the desktop's wash being taller than the row it highlighted is the trap here: wash
// the pill row's extent, not the band's. On Apple the bar is a TRAY the container slides down over
// the field on ▲ (a fixed band ate a third of a landscape phone), so it is only ever drawn focused;
// while down ◀▶ step the sort (clamped, no wrap), L1/R1 pick the arrangement outright, and
// ▼ / A / B hand the controller back — that routing lives in the container, this view only draws.
// Pills are tappable/clickable where there is a pointer, and focusable Buttons on tvOS.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

struct LibraryBarView: View {
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    let sort: LibrarySortKey
    let arrangement: LibraryArrangement
    /// The bar owns the controller (▲ from the field). Draws the wash.
    let focused: Bool
    /// The Collections screen shows the SORT group alone (its arrangement is the tiles).
    var showsView = true
    var compact = false
    var onSort: (LibrarySortKey) -> Void
    var onArrangement: (LibraryArrangement) -> Void
    @Namespace private var sortHighlight
    @Namespace private var viewHighlight

    /// The band's height, matching the settings tab strip's rhythm.
    static let bandHeight: CGFloat = 46
    /// The air under the band before the field.
    static let gap: CGFloat = 12

    var body: some View {
        // One row where it fits (every landscape screen); a portrait phone gets the two groups
        // stacked rather than a row spilling off the edge.
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 0) {
                sortGroup
                Spacer(minLength: 12)
                if showsView { viewGroup }
            }
            VStack(alignment: .leading, spacing: 6) {
                sortGroup
                if showsView { viewGroup }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
        // The focus wash: the pill row's extent, corner 14, accent at 14 % — reads as "this row
        // has the controller" without pretending to be a panel.
        .background {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(ink.accent(0.14))
                .opacity(focused ? 1 : 0)
        }
        .animation(.smooth(duration: 0.18), value: focused)
        .padding(.horizontal, 12)
        .frame(minHeight: Self.bandHeight)
    }

    private var sortGroup: some View {
        group(caption: "SORT") {
            ForEach(LibrarySortKey.all, id: \.self) { key in
                pill(key.label, selected: key == sort, namespace: sortHighlight) { onSort(key) }
            }
        }
    }

    private var viewGroup: some View {
        group(caption: "VIEW") {
            ForEach(LibraryArrangement.all, id: \.self) { view in
                pill(view.label, selected: view == arrangement, namespace: viewHighlight) {
                    onArrangement(view)
                }
            }
        }
    }

    /// A tracked small-caps caption followed by its pills.
    private func group<Pills: View>(caption: String, @ViewBuilder pills: () -> Pills) -> some View {
        HStack(spacing: 10) {
            Text(caption)
                .font(.geist(11, .semibold, relativeTo: .caption2))
                .tracking(1.4)
                .foregroundStyle(ink.fg(0.45))
            HStack(spacing: 6, content: pills)
        }
    }

    private func pill(
        _ label: String, selected: Bool, namespace: Namespace.ID, action: @escaping () -> Void
    ) -> some View {
        let text = Text(label)
            .font(.geist(compact ? 12 : metrics.tabFont, .semibold, relativeTo: .footnote))
            // `onAccent`, not `fg`: the selected pill is FILLED with the accent (see the settings
            // strip for the pale-palette lesson).
            .foregroundStyle(selected ? ink.onAccent : ink.fg(0.55))
            .padding(.horizontal, metrics.rowHPad * 0.7)
            .padding(.vertical, metrics.rowVPad * 0.4)
            .background {
                // One capsule per group that MOVES between its pills, the settings strip's move.
                if selected {
                    Color.clear
                        .consoleGlass(Capsule(), tint: ink.accent(0.85))
                        .matchedGeometryEffect(id: "pill", in: namespace)
                }
            }
            .contentShape(Capsule())
        #if os(tvOS)
        return Button(action: action) { text }.buttonStyle(ConsoleBareButtonStyle())
        #else
        return text.onTapGesture(perform: action)
        #endif
    }
}
#endif
