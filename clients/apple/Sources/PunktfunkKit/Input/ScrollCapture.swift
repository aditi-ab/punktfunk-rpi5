// Floating-point scroll samples → quantized `InputKind::Scroll` wire events: the Swift twin
// of the core's `ScrollAccumulator` plus the capture-side axis lifecycle. The unsent Q24.8
// fraction rides per axis, a source switch cancels the axis the old source still holds before
// the new one moves it, and a platform stop that carries its last translation splits into the
// movement first and the zero stop after — the wire never carries distance on a boundary.

import Foundation
import PunktfunkCore

public struct ScrollCapture {
    private var rem: [Double] = [0, 0]
    private var lastSource: [PunktfunkScrollSource?] = [nil, nil]
    private var openSource: [PunktfunkScrollSource?] = [nil, nil]
    private var momentumSource: [PunktfunkScrollSource?] = [nil, nil]

    public init() {}

    /// Distances use v120 for wheels and DIP otherwise. Final displacement precedes a zero stop.
    public mutating func event(
        dx: Double, dy: Double, source: PunktfunkScrollSource, phase: PunktfunkScrollPhase
    ) -> [PunktfunkInputEvent] {
        axisEvent(axis: 0, delta: dy, source: source, phase: phase)
            + axisEvent(axis: 1, delta: dx, source: source, phase: phase)
    }

    public mutating func cancelAll() -> [PunktfunkInputEvent] {
        var out: [PunktfunkInputEvent] = []
        for axis in 0...1 {
            if let source = openSource[axis] ?? momentumSource[axis] {
                out.append(.normalizedScroll(
                    0, axis: UInt32(axis), source: source, phase: PUNKTFUNK_SCROLL_PHASE_CANCEL))
            }
        }
        self = ScrollCapture()
        return out
    }

    private static let scale = Double(PUNKTFUNK_SCROLL_SCALE)

    private static func admits(
        source: PunktfunkScrollSource, phase: PunktfunkScrollPhase, momentum: Bool
    ) -> Bool {
        guard (0...5).contains(source.rawValue), (0...7).contains(phase.rawValue) else { return false }
        if source == PUNKTFUNK_SCROLL_SOURCE_WHEEL || source == PUNKTFUNK_SCROLL_SOURCE_UNKNOWN {
            return phase == PUNKTFUNK_SCROLL_PHASE_NONE
        }
        return !momentum || source == PUNKTFUNK_SCROLL_SOURCE_FINGER
            || source == PUNKTFUNK_SCROLL_SOURCE_CONTINUOUS || source == PUNKTFUNK_SCROLL_SOURCE_TOUCH
    }

    private mutating func finishAxis(
        axis: Int, delta: Double, source: PunktfunkScrollSource, phase: PunktfunkScrollPhase
    ) -> [PunktfunkInputEvent] {
        let momentum = phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END
        let held = momentum ? momentumSource[axis] : (openSource[axis] ?? momentumSource[axis])
        guard held == source else { return [] }
        var out = delta == 0 ? [] : axisEvent(
            axis: axis, delta: delta, source: source,
            phase: momentum ? PUNKTFUNK_SCROLL_PHASE_MOMENTUM : PUNKTFUNK_SCROLL_PHASE_UPDATE)
        openSource[axis] = nil
        momentumSource[axis] = nil
        rem[axis] = 0
        out.append(.normalizedScroll(0, axis: UInt32(axis), source: source, phase: phase))
        return out
    }

    private mutating func axisEvent(
        axis: Int, delta: Double, source: PunktfunkScrollSource, phase: PunktfunkScrollPhase
    ) -> [PunktfunkInputEvent] {
        let stop = phase == PUNKTFUNK_SCROLL_PHASE_END || phase == PUNKTFUNK_SCROLL_PHASE_CANCEL
            || phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END
        let momentum = phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN
            || phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM || phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM_END
        guard delta.isFinite, Self.admits(source: source, phase: phase, momentum: momentum)
        else { return [] }
        if stop { return finishAxis(axis: axis, delta: delta, source: source, phase: phase) }
        let beginning = phase == PUNKTFUNK_SCROLL_PHASE_BEGIN
            || phase == PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN
        let held = openSource[axis] ?? momentumSource[axis]
        guard delta != 0 || (beginning && held != nil) else { return [] }
        var out: [PunktfunkInputEvent] = []
        if let held, held != source || beginning {
            out.append(.normalizedScroll(
                0, axis: UInt32(axis), source: held, phase: PUNKTFUNK_SCROLL_PHASE_CANCEL))
            openSource[axis] = nil
            momentumSource[axis] = nil
        }
        if lastSource[axis] != source || beginning { rem[axis] = 0 }
        lastSource[axis] = source
        let total = (delta * Self.scale + rem[axis]).clamped(to: Double(Int32.min)...Double(Int32.max))
        let q = Int32(total)
        rem[axis] = total - Double(q)
        guard q != 0 || beginning else { return out }
        let wirePhase: PunktfunkScrollPhase
        if momentum {
            wirePhase = momentumSource[axis] == nil
                ? PUNKTFUNK_SCROLL_PHASE_MOMENTUM_BEGIN : phase
            momentumSource[axis] = source
        } else if phase == PUNKTFUNK_SCROLL_PHASE_NONE {
            wirePhase = phase
        } else {
            wirePhase = openSource[axis] == nil ? PUNKTFUNK_SCROLL_PHASE_BEGIN : PUNKTFUNK_SCROLL_PHASE_UPDATE
            openSource[axis] = source
        }
        out.append(.normalizedScroll(q, axis: UInt32(axis), source: source, phase: wirePhase))
        return out
    }
}
