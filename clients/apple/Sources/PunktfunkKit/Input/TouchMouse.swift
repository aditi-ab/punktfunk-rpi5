// Finger touches → host mouse, for the touchscreen devices: a port of the Android client's
// touch gesture model (clients/android .../TouchInput.kt) so the two touch clients feel
// identical. Two mouse modes share one gesture vocabulary — tap = left click · two-finger
// tap = right click · two-finger drag = scroll · tap-then-press-and-drag = held left drag
// (text selection / window moves) · three-finger tap = cycles the stats overlay tiers
// (off → compact → normal → detailed, matching Android) · three-finger swipe up/down =
// summon/dismiss the local soft keyboard for typing on the host (`onKeyboardGesture`):
//
//  * trackpad (default): the cursor STAYS PUT on touch-down and moves by the finger's
//    relative delta with mild acceleration — swipe to nudge, lift and re-swipe to walk it
//    across, tap to click where it is. This is what makes the cursor reachable on a small
//    screen.
//  * pointer: the cursor jumps to the finger and follows it (absolute moves through the
//    aspect-fit letterbox) — direct pointing for desktop-style use.
//
// The third `TouchInputMode` (`touch`) never reaches this type: `StreamLayerUIView` forwards
// those fingers as REAL wire touches (multi-touch passthrough) instead.

#if os(iOS)
import Foundation
import PunktfunkCore
import PunktfunkShared
import UIKit

/// How touchscreen fingers drive the host — persisted under `DefaultsKey.touchMode`, latched
/// per gesture by `StreamLayerUIView` (a Settings change applies from the NEXT touch, and a
/// gesture never splits across models). `trackpad` is the default: a cursor is the
/// universally workable model; passthrough only helps hosts/apps that actually speak touch.
public enum TouchInputMode: String, CaseIterable, Sendable {
    case trackpad
    case pointer
    case touch

    /// The persisted setting, defaulting to trackpad when unset/unknown.
    public static var current: TouchInputMode {
        TouchInputMode(
            rawValue: SessionSettings.current.touchMode
        ) ?? .trackpad
    }
}

/// The gesture state machine behind the two mouse modes. One instance per stream view, fed
/// only the DIRECT touches (fingers/Pencil — indirect pointers have their own path). Runs
/// entirely on the main thread (UIKit touch delivery). Touches are tracked by identity key
/// with positions cached per event — `UITouch` objects are never retained.
final class TouchMouse {
    /// Gesture/ballistics tuning. Distances are in points where they gate gestures; the
    /// relative ballistics work in PHYSICAL pixels (point deltas × screen scale) so the
    /// acceleration curve matches the Android client's pixel-based constants 1:1.
    enum Tuning {
        /// Movement under this (pt) still counts as a tap, not a drag.
        static let tapSlop: CGFloat = 8
        /// A new touch this soon (s) after a tap, near it, starts a held left-button drag.
        static let tapDragWindow: TimeInterval = 0.25
        /// Two-finger pan distance (pt) per 120-unit wheel notch — matches the feel of the
        /// indirect-trackpad scroll path in StreamViewIOS (~10 pt per notch).
        static let scrollNotchPt: CGFloat = 10
        /// Base finger-px → host-px gain (~1:1, never twitchy). The acceleration below lets a
        /// flick cross the screen while a slow drag stays precise.
        static let pointerSens: CGFloat = 1.3
        /// Above `accelSpeedFloor` px/ms the gain ramps by `accelGain` per px/ms, capped at
        /// `accelMax` (so a fast swipe can't fling the cursor uncontrollably).
        static let accelGain: CGFloat = 0.6
        static let accelSpeedFloor: CGFloat = 0.3
        static let accelMax: CGFloat = 3.0
        /// Three-finger vertical swipe: the fraction of the view height the centroid must
        /// travel to summon (up) / dismiss (down) the local soft keyboard.
        static let keyboardSwipeFraction: CGFloat = 0.10

        /// Acceleration multiplier for a finger speed in physical px per ms.
        static func accel(forSpeed speed: CGFloat) -> CGFloat {
            min(1 + accelGain * max(speed - accelSpeedFloor, 0), accelMax)
        }
    }

    /// Wire events out (the owner gates them on its capture state).
    var send: ((PunktfunkInputEvent) -> Void)?
    /// View-space point → host-mode pixels through the letterbox (pointer mode's moves).
    var hostPoint: ((CGPoint) -> StreamLayerUIView.HostPoint?)?
    /// Three-finger vertical swipe crossed the threshold: `true` = show the local soft
    /// keyboard (swipe up), `false` = dismiss it (swipe down). Fires at most once per gesture.
    var onKeyboardGesture: ((Bool) -> Void)?

    /// No gesture in flight (all fingers up) — the view uses this to release its mode latch.
    var isIdle: Bool { !sessionActive && lastPos.isEmpty }

    private var trackpad = true
    /// Last known position per active finger (identity key) — kept because moved events only
    /// carry the CHANGED touches while the scroll centroid needs every finger.
    private var lastPos: [ObjectIdentifier: CGPoint] = [:]
    private var sessionActive = false
    private var startPoint = CGPoint.zero
    private var maxFingers = 0
    private var moved = false
    private var scrolling = false
    private var dragHeld = false
    // Trackpad relative-motion state: the tracked finger, its last position/time, and the
    // sub-pixel remainder so a slow drag isn't lost to integer truncation.
    private var trackKey: ObjectIdentifier?
    private var prevPoint = CGPoint.zero
    private var prevTime: TimeInterval = 0
    private var carryX: CGFloat = 0
    private var carryY: CGFloat = 0
    /// Scroll anchor (centroid) — re-anchored every time a notch fires.
    private var scrollAnchor = CGPoint.zero
    // Keyboard-swipe state: the 3+-finger centroid anchor (per finger count, like the scroll
    // anchor) and a once-per-gesture latch.
    private var kbCount = 0
    private var kbAnchor = CGPoint.zero
    private var kbFired = false
    // Tap-drag arming: a quick tap leaves a window in which the next nearby touch drags.
    private var lastTapUp: TimeInterval = 0
    private var lastTapPoint = CGPoint.zero

    /// GameStream mouse button ids.
    private enum Button { static let left: UInt32 = 1; static let right: UInt32 = 3 }

    func began(_ touches: Set<UITouch>, in view: UIView, trackpad: Bool) {
        let starting = lastPos.isEmpty
        for touch in touches {
            lastPos[ObjectIdentifier(touch)] = touch.location(in: view)
        }
        if starting, let first = touches.first {
            self.trackpad = trackpad
            sessionActive = true
            startPoint = first.location(in: view)
            maxFingers = 0
            moved = false
            scrolling = false
            kbCount = 0
            kbFired = false
            // A touch landing just after a quick tap nearby = tap-and-drag: hold the left
            // button for this whole gesture (laptop-trackpad convention).
            dragHeld = first.timestamp - lastTapUp < Tuning.tapDragWindow
                && abs(startPoint.x - lastTapPoint.x) < Tuning.tapSlop
                && abs(startPoint.y - lastTapPoint.y) < Tuning.tapSlop
            lastTapUp = 0 // consume the arming either way
            // Pointer mode jumps the cursor to the finger; trackpad leaves it put (the whole
            // point — you nudge it with swipes instead).
            if !trackpad, let h = hostPoint?(startPoint) {
                send?(.mouseMoveAbs(x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
            }
            if dragHeld { send?(.mouseButton(Button.left, down: true)) }
            trackKey = ObjectIdentifier(first)
            prevPoint = startPoint
            prevTime = first.timestamp
            carryX = 0
            carryY = 0
        }
        maxFingers = max(maxFingers, lastPos.count)
    }

    func moved(_ touches: Set<UITouch>, in view: UIView) {
        guard sessionActive else { return }
        for touch in touches where lastPos[ObjectIdentifier(touch)] != nil {
            lastPos[ObjectIdentifier(touch)] = touch.location(in: view)
        }
        // Dropping below three fingers forgets the keyboard-swipe anchor, so a 3→2→3 bounce
        // re-anchors instead of reading the count change as swipe travel.
        if lastPos.count < 3 { kbCount = 0 }
        if lastPos.count == 2 {
            scrollByCentroid()
        } else if lastPos.count >= 3 {
            keyboardSwipe(in: view)
        } else if !scrolling, let touch = touches.first(where: {
            lastPos[ObjectIdentifier($0)] != nil
        }) {
            singleFinger(touch, in: view)
        }
    }

    func ended(_ touches: Set<UITouch>, in view: UIView) {
        guard sessionActive || !lastPos.isEmpty else { return }
        var upTime: TimeInterval = 0
        for touch in touches {
            lastPos.removeValue(forKey: ObjectIdentifier(touch))
            if trackKey == ObjectIdentifier(touch) { trackKey = nil }
            upTime = max(upTime, touch.timestamp)
        }
        guard lastPos.isEmpty, sessionActive else { return }
        sessionActive = false
        if dragHeld {
            dragHeld = false
            send?(.mouseButton(Button.left, down: false)) // end the drag
        } else if !moved {
            switch maxFingers {
            case 3...:
                Self.cycleStats() // in-stream stats-tier cycle, same as Android
            case 2: // two-finger tap → right click
                send?(.mouseButton(Button.right, down: true))
                send?(.mouseButton(Button.right, down: false))
            default: // tap → left click (at the cursor's current spot), arm tap-drag
                send?(.mouseButton(Button.left, down: true))
                send?(.mouseButton(Button.left, down: false))
                lastTapUp = upTime
                lastTapPoint = startPoint
            }
        }
    }

    /// System-cancelled touches (incoming call, gesture takeover): release anything held but
    /// never synthesize a click out of a cancellation.
    func cancelled(_ touches: Set<UITouch>) {
        for touch in touches {
            lastPos.removeValue(forKey: ObjectIdentifier(touch))
            if trackKey == ObjectIdentifier(touch) { trackKey = nil }
        }
        if lastPos.isEmpty { abortSession() }
    }

    /// Session teardown: release anything held on the wire and forget all gesture state.
    func reset() {
        lastPos.removeAll()
        trackKey = nil
        abortSession()
        lastTapUp = 0
    }

    private func abortSession() {
        if dragHeld {
            dragHeld = false
            send?(.mouseButton(Button.left, down: false))
        }
        sessionActive = false
        scrolling = false
        moved = false
    }

    // MARK: - Per-event work

    /// Two fingers → scroll by the centroid delta; never move the cursor. Fires a notch per
    /// `scrollNotchPt` of pan and re-anchors on fire; finger up scrolls up, finger right
    /// scrolls right (the host WHEEL(120) convention).
    private func scrollByCentroid() {
        let n = CGFloat(lastPos.count)
        let cx = lastPos.values.reduce(0) { $0 + $1.x } / n
        let cy = lastPos.values.reduce(0) { $0 + $1.y } / n
        if !scrolling {
            scrolling = true
            scrollAnchor = CGPoint(x: cx, y: cy)
        }
        // Read live, like `InputCapture.sendScroll`: these notches bypass that sink (they go
        // straight to the connection), so the invert setting is applied here, at the notch.
        let sign: Int32 = SessionSettings.current.invertScroll ? -1 : 1
        let notchesY = Int32((scrollAnchor.y - cy) / Tuning.scrollNotchPt)
        let notchesX = Int32((cx - scrollAnchor.x) / Tuning.scrollNotchPt)
        if notchesY != 0 {
            send?(.scroll(notchesY * 120 * sign))
            scrollAnchor.y = cy
            moved = true
        }
        if notchesX != 0 {
            send?(.scroll(notchesX * 120 * sign, horizontal: true))
            scrollAnchor.x = cx
            moved = true
        }
    }

    /// Three+ fingers → the keyboard swipe, never scroll (the documented vocabulary is
    /// TWO-finger scroll; 3+ only fell into the scroll path as an accident of its old `>= 2`
    /// bound). The centroid is anchored per finger count — real fingers never land or lift in
    /// the same event, so a count change must re-anchor rather than read as travel — and the
    /// gesture fires at most once, when the vertical travel crosses the threshold: up = show
    /// the local soft keyboard, down = dismiss it.
    private func keyboardSwipe(in view: UIView) {
        let n = CGFloat(lastPos.count)
        let cx = lastPos.values.reduce(0) { $0 + $1.x } / n
        let cy = lastPos.values.reduce(0) { $0 + $1.y } / n
        if lastPos.count != kbCount {
            kbCount = lastPos.count
            kbAnchor = CGPoint(x: cx, y: cy)
        } else {
            let dy = cy - kbAnchor.y
            // Real centroid travel disqualifies the tap classification in `ended` (else a
            // sub-threshold swipe would still fire the three-finger stats tap).
            if abs(dy) > Tuning.tapSlop || abs(cx - kbAnchor.x) > Tuning.tapSlop { moved = true }
            if !kbFired, abs(dy) >= view.bounds.height * Tuning.keyboardSwipeFraction {
                kbFired = true
                onKeyboardGesture?(dy < 0) // finger up → show, finger down → dismiss
            }
        }
        // Leaving the scroll state stale would read the 3→2 centroid jump as a wheel notch;
        // clearing it makes a return to two fingers re-anchor fresh. Same for the trackpad's
        // tracked finger: its prev position froze while 3+ fingers were down, so dropping
        // straight back to one finger must re-anchor (zero delta), not replay the whole
        // 3-finger phase as one cursor jump.
        scrolling = false
        trackKey = nil
    }

    /// One finger (and the gesture never became a scroll — dropping back from two fingers to
    /// one must not jerk the cursor).
    private func singleFinger(_ touch: UITouch, in view: UIView) {
        let loc = touch.location(in: view)
        if abs(loc.x - startPoint.x) > Tuning.tapSlop || abs(loc.y - startPoint.y) > Tuning.tapSlop {
            moved = true
        }
        guard trackpad else {
            if let h = hostPoint?(loc) { // pointer mode: the cursor follows the finger
                send?(.mouseMoveAbs(x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
            }
            return
        }
        // Relative: move by the finger delta × (sensitivity × acceleration), carrying the
        // sub-pixel remainder. Re-anchor (zero delta this frame) if the tracked finger
        // changed, so lifting one of several fingers never jumps the cursor.
        let key = ObjectIdentifier(touch)
        if key != trackKey {
            trackKey = key
            prevPoint = loc
            prevTime = touch.timestamp
            return
        }
        // Ballistics in physical pixels so the curve matches the Android tuning exactly.
        let scale = view.window?.screen.scale ?? view.traitCollection.displayScale
        let dx = (loc.x - prevPoint.x) * scale
        let dy = (loc.y - prevPoint.y) * scale
        let dtMs = max((touch.timestamp - prevTime) * 1000, 1)
        prevPoint = loc
        prevTime = touch.timestamp
        let gain = Tuning.pointerSens * Tuning.accel(forSpeed: hypot(dx, dy) / dtMs)
        carryX += dx * gain
        carryY += dy * gain
        let outX = Int32(carryX) // truncates toward zero → remainder kept with its sign
        let outY = Int32(carryY)
        if outX != 0 || outY != 0 {
            send?(.mouseMove(dx: outX, dy: outY))
            carryX -= CGFloat(outX)
            carryY -= CGFloat(outY)
        }
    }

    /// Three-finger tap cycles the stats overlay tiers (off → compact → normal → detailed) —
    /// through the shared `statsVerbosity` default, which the app's HUD views observe via
    /// @AppStorage (so this needs no wiring to them). Same cycle as Android's triple-tap.
    private static func cycleStats() {
        StatsVerbosity.cycle()
    }
}
#endif
