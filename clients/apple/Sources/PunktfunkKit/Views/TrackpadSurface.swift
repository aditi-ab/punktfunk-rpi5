// The flat half of a half-open fold as a trackpad (design/touch-client-overlay.md §4.4): the
// stream view's own gesture engine in its relative mode, on a surface that shows no picture.

#if os(iOS)
import PunktfunkCore
import PunktfunkShared
import SwiftUI
import UIKit

/// A trackpad for `connection`'s pointer. One finger moves, a tap clicks, two fingers scroll,
/// the twist turns the ring and the three-finger swipe shows or hides the keyboard.
public struct TrackpadSurface: UIViewRepresentable {
    private let connection: PunktfunkConnection
    private let enabled: Bool
    private let onDial: ((DialEvent) -> Void)?

    /// `enabled` is the session's trust gate: off, a finger reaches the host as nothing.
    public init(
        connection: PunktfunkConnection, enabled: Bool, onDial: ((DialEvent) -> Void)? = nil
    ) {
        self.connection = connection
        self.enabled = enabled
        self.onDial = onDial
    }

    public func makeUIView(context: Context) -> TrackpadUIView {
        let view = TrackpadUIView()
        updateUIView(view, context: context)
        return view
    }

    public func updateUIView(_ view: TrackpadUIView, context: Context) {
        view.connection = connection
        view.enabled = enabled
        view.onDial = onDial
    }
}

public final class TrackpadUIView: UIView {
    weak var connection: PunktfunkConnection?
    var enabled = false
    var onDial: ((DialEvent) -> Void)?

    private lazy var mouse: TouchMouse = {
        let mouse = TouchMouse()
        mouse.send = { [weak self] event in
            guard let self, self.enabled else { return }
            self.connection?.send(event)
        }
        // The stream view owns the keyboard: it is the first responder that types to the host.
        mouse.onKeyboardGesture = { [weak self] show in
            guard let self, self.enabled else { return }
            NotificationCenter.default.post(
                name: .punktfunkToggleSoftKeyboard, object: self.connection,
                userInfo: [SoftKeyboardNote.show: show])
        }
        mouse.onDial = { [weak self] event in
            guard let self, self.enabled else { return }
            self.onDial?(event)
        }
        return mouse
    }()

    override init(frame: CGRect) {
        super.init(frame: frame)
        isMultipleTouchEnabled = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

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

    /// Leaving the hierarchy mid-gesture (the device opened flat) releases a held button.
    public override func willMove(toWindow newWindow: UIWindow?) {
        super.willMove(toWindow: newWindow)
        if newWindow == nil { mouse.reset() }
    }
}

#endif

/// `userInfo` keys of `.punktfunkToggleSoftKeyboard`.
enum SoftKeyboardNote {
    /// A `Bool`: show or hide. Absent, the keyboard toggles.
    static let show = "show"
}
