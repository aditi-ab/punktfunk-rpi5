// A touch surface that runs the stream's own two-finger twist (`TouchMouse`, with no mouse
// behind it) and nothing else: the quick-actions editor's backdrop, so the ring there answers the
// real gesture with the real thresholds (design/touch-client-overlay.md §3.3 — "the editor is the
// tutorial"). A one-finger tap reports separately; the editor closes the ring on it.

#if os(iOS)
import SwiftUI
import UIKit

public struct DialCatcher: UIViewRepresentable {
    public var onDial: (DialEvent) -> Void
    public var onTap: () -> Void

    public init(onDial: @escaping (DialEvent) -> Void, onTap: @escaping () -> Void) {
        self.onDial = onDial
        self.onTap = onTap
    }

    public func makeUIView(context: Context) -> DialCatcherView {
        let view = DialCatcherView()
        view.onDial = onDial
        view.onTap = onTap
        return view
    }

    public func updateUIView(_ view: DialCatcherView, context: Context) {
        view.onDial = onDial
        view.onTap = onTap
    }
}

public final class DialCatcherView: UIView {
    var onDial: ((DialEvent) -> Void)?
    var onTap: (() -> Void)?
    /// The stream's touch mouse with its wire unplugged: clicks and scroll notches go nowhere,
    /// the dial's arm/commit/cancel come out of `onDial` exactly as in a session.
    private let mouse = TouchMouse()

    public override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
        backgroundColor = .clear
        mouse.onDial = { [weak self] event in self?.onDial?(event) }
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        tap.cancelsTouchesInView = false
        addGestureRecognizer(tap)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    @objc private func tapped() { onTap?() }

    public override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        mouse.began(touches, in: self, trackpad: true)
    }

    public override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        mouse.moved(touches, in: self)
    }

    public override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        mouse.ended(touches, in: self)
    }

    public override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        mouse.cancelled(touches)
    }
}
#endif
