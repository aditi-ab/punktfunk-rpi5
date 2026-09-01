// The virtual controller's layout editor: the same `PadControlHost` views the stream mounts,
// over a stand-in backdrop, with the editor's own SwiftUI gestures on top — the UIKit views
// are mounted wire-less and non-interactive, so identical pixels answer to the editor's drag
// instead of a game. Drag moves a control; tap selects it for the size slider and the
// visibility toggle; hidden controls stay visible here, ghosted. The overrides land in the
// class the screen is in (wide or narrow, `padControls`' own split), so an iPad edited in
// landscape keeps its upright preset untouched. The chrome sits where the trio of small discs
// is not: they ride the bottom edge on a wide layer and the top edge on a narrow one.

#if os(iOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

struct PadLayoutEditor: View {
    /// The pad of the blob being edited; `commit` writes the whole pad back through the same
    /// scoped binding the quick-actions editor uses, so profile rules hold.
    let pad: PadConfig
    let commit: (PadConfig) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var selected: String?
    /// The drag in flight: which control, and how far, so the finger leads and the blob
    /// follows only on release.
    @State private var dragging: String?
    @State private var drag = CGSize.zero
    @State private var size: Float = 1

    var body: some View {
        GeometryReader { geo in
            let scale = CGFloat(min(max(pad.scale, VirtualPad.scaleRange.lowerBound), VirtualPad.scaleRange.upperBound))
            let opacity = CGFloat(min(max(pad.opacity, VirtualPad.opacityRange.lowerBound), 1))
            let narrow = Float(geo.size.width) / Float(scale) < VirtualPad.narrow
            let controls = padControls(pad: pad,
                                       w: Float(geo.size.width / scale), h: Float(geo.size.height / scale))
            ZStack {
                LinearGradient(colors: [Color(red: 0.08, green: 0.10, blue: 0.12),
                                        Color(red: 0.14, green: 0.19, blue: 0.22)],
                               startPoint: .top, endPoint: .bottom)
                    .contentShape(Rectangle())
                    .onTapGesture { selected = nil }
                ForEach(controls, id: \.id) { c in
                    control(c, scale: scale, opacity: opacity, container: geo.size, narrow: narrow)
                }
                chrome(controls: controls, narrow: narrow)
                    .frame(maxHeight: .infinity, alignment: narrow ? .bottom : .top)
                    .padding(.top, geo.safeAreaInsets.top + 16)
                    .padding(.bottom, geo.safeAreaInsets.bottom + 16)
            }
        }
        .ignoresSafeArea()
        .environment(\.colorScheme, .dark)
    }

    private func control(_ c: PadControl, scale: CGFloat, opacity: CGFloat,
                         container: CGSize, narrow: Bool) -> some View {
        let centre = CGPoint(x: (CGFloat(c.rect.x) + CGFloat(c.rect.w) / 2) * scale,
                             y: (CGFloat(c.rect.y) + CGFloat(c.rect.h) / 2) * scale)
        let live = dragging == c.id ? drag : .zero
        return PadControlHost(control: c, scale: scale, opacity: opacity, wire: nil, interactive: false)
            .frame(width: CGFloat(c.rect.w) * scale, height: CGFloat(c.rect.h) * scale)
            .opacity(c.hidden ? 0.35 : 1)
            .overlay {
                if selected == c.id {
                    RoundedRectangle(cornerRadius: 12).strokeBorder(Color.white, lineWidth: 2)
                }
            }
            .contentShape(Rectangle())
            .position(x: centre.x + live.width, y: centre.y + live.height)
            .onTapGesture { select(c) }
            .gesture(DragGesture()
                .onChanged { v in
                    if dragging != c.id { select(c) }
                    dragging = c.id
                    drag = v.translation
                }
                .onEnded { v in
                    dragging = nil
                    drag = .zero
                    update(narrow, c.id) {
                        $0.x = Float(min(max((centre.x + v.translation.width) / container.width, 0), 1))
                        $0.y = Float(min(max((centre.y + v.translation.height) / container.height, 0), 1))
                    }
                })
            .accessibilityLabel(c.label)
            .accessibilityHint("Drag to move; tap to select")
    }

    private func chrome(controls: [PadControl], narrow: Bool) -> some View {
        VStack(spacing: 10) {
            HStack(spacing: 12) {
                Button("Done") { dismiss() }
                    .fontWeight(.semibold)
                    .keyboardShortcut(.cancelAction)
                VStack(spacing: 1) {
                    Text("Controller layout").font(.headline)
                    Text(narrow
                         ? "Editing the upright layout — wide screens keep their own."
                         : "Editing the wide layout — upright screens keep their own.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Button("Reset layout") { write(narrow, [:]); selected = nil }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16))
            if let c = selected.flatMap({ id in controls.first { $0.id == id } }) {
                VStack(alignment: .leading, spacing: 4) {
                    HStack {
                        Text(c.label).font(.subheadline.weight(.semibold))
                        Spacer(minLength: 16)
                        Button(c.hidden ? "Show" : "Hide") {
                            update(narrow, c.id) { $0.hidden = !c.hidden }
                        }
                        Button("Reset") {
                            write(narrow, tweaks(narrow).filter { $0.key != c.id })
                            size = 1
                        }
                    }
                    Text("Size · \(Int((size * 100).rounded()))%").font(.caption)
                    Slider(value: $size, in: VirtualPad.tweakScaleRange) { editing in
                        if !editing { update(narrow, c.id) { $0.scale = size } }
                    }
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
                .frame(width: 320)
                .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 16))
            } else {
                Text("Drag a control to move it; tap one for size and visibility.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private func select(_ c: PadControl) {
        selected = c.id
        size = c.sc
    }

    private func tweaks(_ narrow: Bool) -> [String: PadTweak] {
        narrow ? pad.controlsNarrow : pad.controls
    }

    private func write(_ narrow: Bool, _ map: [String: PadTweak]) {
        var p = pad
        if narrow { p.controlsNarrow = map } else { p.controls = map }
        commit(p)
    }

    private func update(_ narrow: Bool, _ id: String, _ change: (inout PadTweak) -> Void) {
        var t = tweaks(narrow)[id] ?? PadTweak()
        change(&t)
        write(narrow, tweaks(narrow).merging([id: t]) { _, new in new })
    }
}
#endif
