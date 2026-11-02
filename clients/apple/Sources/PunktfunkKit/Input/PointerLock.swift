// The iPad pointer lock's own state. SpringBoard grants the lock only to a frontmost scene that
// fills its screen, and only on a false→true edge of `prefersPointerLocked`. So a re-ask is one
// edge, fired only on an event that can change the answer: a drop while wanted, a click while
// unlocked, the scene going active, the window filling its screen again.
//
// `StreamViewController` reads the live facts, steps this, and does what `step` returns.
// `PointerLockChain`, the swizzle that lets SpringBoard reach the controller, is separate. Pure and
// on every platform, so `swift test` runs its table (`PointerLockTests`).

import Foundation

struct PointerLock: Equatable {
    /// Long enough for UIKit to push the false pass to the scene before the true one follows.
    static let edgeHold: TimeInterval = 0.05
    /// A grant SpringBoard revokes at once would re-ask on every revoke; one a second bounds it.
    static let dropGap: TimeInterval = 1

    /// What the controller reads live at each event.
    struct Facts: Equatable {
        /// Captured, ring down, pointer capture on, an iPad, and the session's pointer grant.
        var wanted: Bool
        /// The scene's lock right now; nil while there is no scene.
        var granted: Bool?
        /// Frontmost, and the window fills its screen.
        var sceneCanHold: Bool
    }

    enum Event: Equatable {
        /// A click up while unlocked, the scene going active, or the turn after a drop.
        case reask
        /// A layout pass, and whether the window fills its screen now.
        case layout(fills: Bool)
        /// SpringBoard changed a scene's lock; `ours` when it is this window's scene.
        case lockChanged(ours: Bool, at: TimeInterval)
        /// `edgeHold` ran out.
        case edgeEnded
    }

    enum Action: Equatable {
        case none
        /// Hold `prefersPointerLocked` false, re-anchor the chain, step `.edgeEnded` after
        /// `edgeHold`.
        case beginEdge
        /// Let `prefersPointerLocked` follow `wanted` again.
        case endEdge
        /// Step `.reask` on the next main-queue turn: a deliberate release clears `captured`
        /// first, so it is never fought.
        case reaskNextTurn
    }

    /// A re-ask holds `prefersPointerLocked` at false until its edge ends.
    private(set) var edgeInFlight = false
    /// When the last drop re-ask was scheduled.
    private var lastDropReask: TimeInterval = -.infinity
    /// The window filled its screen at the last layout pass (assumed so until one says otherwise).
    private var filledScreen = true

    /// What `prefersPointerLocked` answers.
    func prefersLocked(wanted: Bool) -> Bool { wanted && !edgeInFlight }

    /// Only a primary press engages capture from a click: InputCapture's latch swallows button 1's
    /// release, and another button's lone release would eat the next click.
    static func clickEngagesCapture(button: UInt32, down: Bool) -> Bool { down && button == 1 }

    mutating func step(_ event: Event, _ facts: Facts) -> Action {
        switch event {
        case .reask:
            return reask(facts)
        case .layout(let fills):
            // The pass that makes a windowed scene fill again, a re-maximise, is the one that asks.
            let refilled = fills && !filledScreen
            filledScreen = fills
            return refilled ? reask(facts) : .none
        case .lockChanged(let ours, let now):
            guard ours, facts.wanted, facts.granted == false, now - lastDropReask >= Self.dropGap
            else { return .none }
            lastDropReask = now
            return .reaskNextTurn
        case .edgeEnded:
            edgeInFlight = false
            return .endEdge
        }
    }

    /// One false→true edge while the lock is wanted, not held, and grantable. A call during an
    /// edge folds into it.
    private mutating func reask(_ facts: Facts) -> Action {
        guard facts.wanted, facts.granted != true, facts.sceneCanHold, !edgeInFlight else {
            return .none
        }
        edgeInFlight = true
        return .beginEdge
    }
}
