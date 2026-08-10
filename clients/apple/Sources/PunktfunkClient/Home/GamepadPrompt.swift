// The gamepad UI's answer to a system alert / confirmation dialog (iOS/iPadOS/macOS).
//
// `.alert` and `.confirmationDialog` are UIKit/AppKit surfaces. A game controller cannot move
// through their buttons or press one — so on iOS/macOS every prompt in the connect path was a dead
// end for a pad-only user, and they are not incidental prompts:
//
//   - "Pairing required" (Request Access / Pair with PIN…) is the FIRST thing an unpaired host
//     shows. Pairing was unreachable before it even got to the PIN.
//   - "Connection failed" strands the console UI behind a modal only a finger can dismiss.
//   - "Waiting for approval" owns the only Cancel for a connect that may never complete.
//
// tvOS keeps the system alerts: the focus engine drives them natively there, which is the whole
// reason this gap was tvOS-invisible.
//
// Deliberately NOT built on GamepadMenuList: that is a ScrollView (right for a settings screen of
// unknown length, wrong for two buttons in a card, where it would need an invented height and
// could clip). A prompt has two or three actions, so it owns a plain VStack and a cursor.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)

/// One choice in a console prompt.
struct GamepadPromptAction: Identifiable {
    let id: String
    let title: String
    /// This is the action B (and Esc) performs, and the one the cursor opens on. Exactly one
    /// action should carry it — `GamepadPrompt` falls back to the LAST action when none does,
    /// which matches how a system alert treats its cancel role.
    var isCancel = false
    /// Drawn as the primary, accent-tinted row. At most one.
    var isPrimary = false
    let run: () -> Void
}

/// A prompt to show over the console UI: what happened, and what can be done about it.
struct GamepadPrompt: Identifiable {
    let id: String
    let title: String
    let message: String
    let actions: [GamepadPromptAction]
    /// A wait with no outcome yet (the delegated-approval hold) shows a spinner beside the title —
    /// the prompt is the UI for something still in flight, not a report that it finished.
    var busy = false
}

/// The prompt, worn as the console's own modal: a dimmed field, a glass card, a focus list of
/// actions, and the same legend every other gamepad screen carries.
struct GamepadPromptView: View {
    @Environment(\.gamepadInk) private var ink
    @Environment(\.gamepadMetrics) private var metrics
    let prompt: GamepadPrompt

    @State private var cursor = 0
    @State private var input = GamepadMenuInput(manager: .shared)
    @State private var haptics = MenuHaptics(manager: .shared)
    /// `.sensoryFeedback` counters — device ticks for confirm and for a refused move at an end.
    @State private var activateTick = 0
    @State private var boundaryTick = 0

    #if os(iOS)
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif

    var body: some View {
        ZStack {
            // Swallows touch to the launcher behind it, which is also gated out of the controller
            // poll for as long as this is up (ContentView's `promptActive`).
            Rectangle()
                .fill(.black.opacity(0.55))
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture {}
            card
        }
        .sensoryFeedback(.selection, trigger: cursor)
        .sensoryFeedback(.impact(weight: .medium), trigger: activateTick)
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: boundaryTick)
        .onAppear {
            cursor = prompt.actions.firstIndex(where: \.isCancel) ?? max(prompt.actions.count - 1, 0)
            wire()
            input.start()
        }
        // The prompt's identity is stable across a message change (same `id`), so re-wire rather
        // than rely on a remount: the stored closures captured the OLD actions array.
        .onChange(of: prompt.actions.map(\.id)) { _, _ in
            cursor = min(cursor, max(prompt.actions.count - 1, 0))
            wire()
        }
        .onDisappear {
            input.stop()
            haptics.stop()
        }
    }

    private var card: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(spacing: 10) {
                if prompt.busy {
                    ProgressView().controlSize(.small).tint(ink.fg(0.8))
                }
                Text(prompt.title)
                    .font(.geist(compact ? 19 : 22, .bold, relativeTo: .title3))
                    .foregroundStyle(ink.fg)
            }
            Text(prompt.message)
                .font(.geist(metrics.detailFont, relativeTo: .callout))
                .foregroundStyle(ink.fg(0.62))
                .fixedSize(horizontal: false, vertical: true)
            VStack(spacing: 6) {
                ForEach(Array(prompt.actions.enumerated()), id: \.element.id) { idx, action in
                    actionRow(action, focused: idx == cursor)
                        .contentShape(Rectangle())
                        .onTapGesture { tap(idx) }
                }
            }
            .padding(.top, 2)
            GamepadHintBar(hints: hints)
        }
        .padding(compact ? 20 : 26)
        .frame(maxWidth: 460)
        .consoleGlass(RoundedRectangle(cornerRadius: 24, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: 24, style: .continuous)
                .strokeBorder(ink.fg(0.12), lineWidth: 1)
        }
        .padding(24)
    }

    private func actionRow(_ action: GamepadPromptAction, focused: Bool) -> some View {
        let m = metrics
        return Text(action.title)
            .font(.geist(m.labelFont, .semibold, relativeTo: .body))
            .foregroundStyle(action.isPrimary ? ink.accent : ink.fg)
            .frame(maxWidth: .infinity)
            .padding(.horizontal, m.rowHPad)
            .padding(.vertical, m.rowVPad)
            .consoleGlass(
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous),
                tint: focused ? ink.accent(0.30) : nil,
                interactive: focused)
            .overlay {
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                    .strokeBorder(ink.fg(focused ? 0.28 : 0.06), lineWidth: 1)
            }
            .scaleEffect(focused ? 1.0 : 0.98)
            .animation(.smooth(duration: 0.18), value: focused)
    }

    private var hints: [GamepadHint] {
        var hints: [GamepadHint] = [.init(
            glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select",
            action: { activate() })]
        // Only where B has somewhere to go: a one-action prompt ("OK") is dismissed by that
        // action, and B does it too — naming it twice would just be noise.
        if prompt.actions.count > 1, let cancel = prompt.actions.first(where: \.isCancel) {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: cancel.title,
                action: { back() }))
        }
        return hints
    }

    // MARK: - Input

    private func wire() {
        input.onMove = { direction in
            switch direction {
            case .up: step(by: -1)
            case .down: step(by: 1)
            // A prompt's actions are a vertical list; left/right have nothing to mean here, and
            // silently treating them as up/down would make a nudged stick pick a different button.
            case .left, .right: break
            }
        }
        input.onConfirm = { activate() }
        input.onBack = { back() }
    }

    private func step(by delta: Int) {
        let target = cursor + delta
        guard target >= 0, target < prompt.actions.count else {
            boundaryTick &+= 1
            haptics.boundary()
            return
        }
        cursor = target
        haptics.move()
    }

    private func activate() {
        guard cursor >= 0, cursor < prompt.actions.count else { return }
        activateTick &+= 1
        haptics.confirm()
        prompt.actions[cursor].run()
    }

    /// B: the cancel action, else the last one — the same fallback a system alert applies when
    /// nothing carries the cancel role, so B always has a way out rather than doing nothing.
    private func back() {
        guard let action = prompt.actions.first(where: \.isCancel) ?? prompt.actions.last
        else { return }
        activateTick &+= 1
        haptics.confirm()
        action.run()
    }

    /// Touch fallback matching the rest of the gamepad UI: a tap focuses AND activates.
    private func tap(_ idx: Int) {
        guard idx >= 0, idx < prompt.actions.count else { return }
        cursor = idx
        activate()
    }
}
#endif
