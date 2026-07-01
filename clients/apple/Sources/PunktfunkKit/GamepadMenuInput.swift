// Explicit left-stick/dpad-driven menu navigation for the gamepad UI's host carousel and library
// coverflow (iOS/iPadOS only — see GamepadUIEnvironment).
//
// Polls the active controller at 60 Hz rather than installing `valueChangedHandler`/
// `pressedChangedHandler` callbacks — mirroring `ControllerTestView`'s "Input" card (see its own
// comment: "Poll the live controller ... — no handlers installed"), the one thing in this codebase
// already confirmed on real hardware to read a controller reliably outside a streaming session. Two
// earlier versions of this class both installed handlers directly (first reading the dpad's combined
// `.xAxis`/`.yAxis`, then its discrete `.isPressed` states, matching `GamepadCapture`'s pattern) and
// neither one's callbacks fired on-device even though the SAME controller's input showed up correctly
// in `ControllerTestView`'s poll-based readout — so polling isn't just a style choice here, it's the
// only approach confirmed to actually work outside a stream. Being read-only, it also can't conflict
// with `GamepadCapture` installing its own handlers once a stream starts — there's nothing to hand
// off or race over.
//
// The button set mirrors a console launcher: A confirms, B backs out, Y is a screen's secondary
// action, and the shoulders (L1/R1) are optional fast "jump" steps. Directional moves auto-repeat
// on a held stick/dpad after an initial delay; every button is edge-triggered (fires once per press).

import Foundation
import GameController

@MainActor
public final class GamepadMenuInput {
    public enum Direction: Equatable, Sendable {
        case up, down, left, right
    }

    private let manager: GamepadManager
    private var pollTimer: Timer?
    private var isActive = false
    private var currentDirection: Direction?
    private var repeatTimer: Timer?
    private var wasConfirmPressed = false
    private var wasSecondaryPressed = false
    private var wasBackPressed = false
    private var wasLeftShoulderPressed = false
    private var wasRightShoulderPressed = false

    /// Discrete directional move — already debounced (fires once on a fresh press, then repeats
    /// on a hold after an initial delay, like a standard menu).
    public var onMove: ((Direction) -> Void)?
    /// Button A (or equivalent primary action) — edge-triggered, fires once per press.
    public var onConfirm: (() -> Void)?
    /// Button Y (or equivalent secondary action, e.g. "open library") — edge-triggered.
    public var onSecondary: (() -> Void)?
    /// Button B (or equivalent back/dismiss) — edge-triggered.
    public var onBack: (() -> Void)?
    /// Shoulder buttons (L1 `false` / R1 `true`) — edge-triggered fast-jump steps, optional per
    /// screen. Unset ⇒ the shoulders do nothing.
    public var onShoulder: ((Bool) -> Void)?

    /// Stick magnitude below this reads as neutral (dead zone).
    private let deadzone: Float = 0.5
    private let initialRepeatDelay: TimeInterval = 0.38
    private let repeatInterval: TimeInterval = 0.16
    private let pollInterval: TimeInterval = 1.0 / 60.0

    public init(manager: GamepadManager) {
        self.manager = manager
    }

    public func start() {
        guard !isActive else { return }
        isActive = true
        let timer = Timer(timeInterval: pollInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.poll() }
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    public func stop() {
        isActive = false
        pollTimer?.invalidate()
        pollTimer = nil
        repeatTimer?.invalidate()
        repeatTimer = nil
        currentDirection = nil
        wasConfirmPressed = false
        wasSecondaryPressed = false
        wasBackPressed = false
        wasLeftShoulderPressed = false
        wasRightShoulderPressed = false
    }

    /// Reads `manager.active` fresh every tick (no persistent binding to a specific controller
    /// needed) — a disconnect/reconnect or a controller switch is just picked up on the next poll.
    private func poll() {
        guard isActive, let gamepad = manager.active?.controller.extendedGamepad else { return }

        edge(gamepad.buttonA.isPressed, &wasConfirmPressed) { onConfirm?() }
        edge(gamepad.buttonY.isPressed, &wasSecondaryPressed) { onSecondary?() }
        edge(gamepad.buttonB.isPressed, &wasBackPressed) { onBack?() }
        edge(gamepad.leftShoulder.isPressed, &wasLeftShoulderPressed) { onShoulder?(false) }
        edge(gamepad.rightShoulder.isPressed, &wasRightShoulderPressed) { onShoulder?(true) }

        updateDirection(directionFrom(gamepad))
    }

    /// Fire `action` on the rising edge of `pressed`, tracking the last state in `was`.
    private func edge(_ pressed: Bool, _ was: inout Bool, _ action: () -> Void) {
        if pressed, !was { action() }
        was = pressed
    }

    /// The current requested direction: the left stick is the primary/natural input; the dpad is an
    /// alternative. Read via discrete `.isPressed` / analog `.value` (never the dpad's combined axis
    /// — the first version of this class did that and it silently never registered a press on-device).
    private func directionFrom(_ gamepad: GCExtendedGamepad) -> Direction? {
        let stick = gamepad.leftThumbstick
        let x = stick.xAxis.value
        let y = stick.yAxis.value
        if abs(x) > abs(y), abs(x) > deadzone {
            return x > 0 ? .right : .left
        } else if abs(y) > deadzone {
            return y > 0 ? .up : .down
        }
        let dpad = gamepad.dpad
        if dpad.left.isPressed { return .left }
        if dpad.right.isPressed { return .right }
        if dpad.up.isPressed { return .up }
        if dpad.down.isPressed { return .down }
        return nil
    }

    private func updateDirection(_ direction: Direction?) {
        guard direction != currentDirection else { return }
        repeatTimer?.invalidate()
        repeatTimer = nil
        currentDirection = direction
        guard let direction else { return }
        onMove?(direction)
        // First repeat after a longer delay (so a quick tap doesn't double-move), then steady.
        let timer = Timer(timeInterval: initialRepeatDelay, repeats: false) { [weak self] _ in
            Task { @MainActor in
                guard let self else { return }
                self.repeatTimer?.invalidate()
                let repeating = Timer(timeInterval: self.repeatInterval, repeats: true) { [weak self] _ in
                    Task { @MainActor in self?.onMove?(direction) }
                }
                RunLoop.main.add(repeating, forMode: .common)
                self.repeatTimer = repeating
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        repeatTimer = timer
    }
}
