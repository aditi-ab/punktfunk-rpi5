// The AU intake both VideoToolbox pumps run before their sink: the background keep-alive drain,
// loss recovery through the re-anchor gate, the straggler filter, H.265 concealment and the
// format/keyframe bookkeeping of `AUPumpState`. Only the sink differs: stage-1 enqueues to an
// AVSampleBufferDisplayLayer, stage-2 submits to VideoToolbox.

import CoreMedia
import Foundation
import os

private let intakeLog = ClientLog(category: "video")

/// Thread-confined to one pump thread; one per session.
struct AUIntake {
    /// One AU the sink may take. The sink still honours `step.withhold`, after any check of its
    /// own that must see withheld AUs too.
    struct Intake {
        let au: AccessUnit
        let step: AUPumpState.Step
        /// The AU carries parameter sets (an IDR).
        let idr: Bool
    }

    private(set) var pump = AUPumpState()
    private let connection: PunktfunkConnection
    private let gate: ReanchorGate
    private let recovery: KeyframeRecovery
    /// The decoder reads the RPS itself, so a lost reference is concealed in the bitstream first.
    private let concealer: HevcConcealer?
    private var wasUnrecoverable = false
    /// When the current wait for parameter sets began, for the resume log.
    private var awaitingSince = Date.distantPast

    init(connection: PunktfunkConnection, gate: ReanchorGate, recovery: KeyframeRecovery) {
        self.connection = connection
        self.gate = gate
        self.recovery = recovery
        concealer = connection.videoCodec == .hevc ? HevcConcealer() : nil
    }

    /// Pull and vet the next AU (100 ms poll); nil when this iteration has nothing for the sink.
    /// `onHdrMeta` set drains the HDR mastering plane every iteration. Throws once the session
    /// closed.
    mutating func next(
        onFrame: ((AccessUnit) -> Void)?,
        onDecodedSize: ((Int, Int) -> Void)?,
        onHdrMeta: ((PunktfunkConnection.HdrMeta) -> Void)? = nil
    ) throws -> Intake? {
        // Background: drain one AU for flow control and host pacing, skip all bookkeeping.
        // exitBackground asks for an IDR and the gate re-arms on the resumed index gap.
        if connection.isVideoDropped {
            _ = try connection.nextAU(timeoutMs: 100)
            return nil
        }
        // Re-asked every iteration so a request the throttle swallowed is sent again. A drop-count
        // climb past the gap's credit arms the freeze; an overdue freeze re-asks for the anchor.
        if pump.awaitingIDR { recovery.request() }
        if gate.poll(framesDropped: connection.framesDropped()) { recovery.request() }
        // Polled regardless of the Welcome's HDR flag: a game can enter HDR mid-session.
        if let onHdrMeta, let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
            onHdrMeta(meta)
        }
        guard var au = try connection.nextAU(timeoutMs: 100) else { return nil }
        // A forward gap fires a throttled RFI and arms the freeze, credited with the gap width so
        // the reassembler's later drop count for the same loss cannot re-freeze a healed stream.
        let gapWidth = connection.noteFrameIndexGapWidth(au.frameIndex)
        if gapWidth > 0 { gate.arm(expectingDrops: UInt64(gapWidth)) }
        onFrame?(au)
        if pump.isStraggler(frameIndex: au.frameIndex) { return nil }
        let concealed = conceal(&au)
        let idrFormat = connection.videoCodec.formatDescription(fromKeyframe: au.data)
        let step = pump.note(
            frameIndex: au.frameIndex, idrFormat: idrFormat, lossAhead: gapWidth > 0,
            flags: au.flags, concealed: concealed)
        if step.straggler { return nil }
        if step.askKeyframe { recovery.request() }
        if let size = step.newSize { onDecodedSize?(size.width, size.height) }
        if step.resumed {
            let ms = Int(Date().timeIntervalSince(awaitingSince) * 1000)
            intakeLog.notice("video: recovery IDR received — resumed after \(ms, privacy: .public) ms")
        }
        if step.startedFormatWait {
            awaitingSince = Date()
            intakeLog.warning(
                "video: received AUs but no decodable format (missing/unparsed parameter sets) — requesting an IDR until one seeds it"
            )
        }
        return Intake(au: au, step: step, idr: idrFormat != nil)
    }

    /// The sink lost its decoder state: wait for the next IDR's parameter sets.
    mutating func requireIDR() {
        pump.requireIDR()
    }

    /// Run the H.265 concealer over `au`, swapping in its rewrite.
    private mutating func conceal(_ au: inout AccessUnit) -> AUPumpState.Concealment {
        guard let concealer else { return .none }
        let concealed: AUPumpState.Concealment
        switch concealer.conceal(au.data) {
        case .intact:
            concealed = .decodable
        case .rewritten(let data):
            concealed = .decodable
            au = au.replacing(data: data)
            intakeLog.notice(
                "video: frame \(au.frameIndex, privacy: .public) names a lost reference — moved to a present picture until the re-anchor"
            )
        case .unrecoverable:
            concealed = .unrecoverable
        }
        if concealed == .unrecoverable, !wasUnrecoverable {
            intakeLog.warning(
                "video: frame \(au.frameIndex, privacy: .public) names a lost reference with nothing to stand in — withholding until an IDR"
            )
        }
        wasUnrecoverable = concealed == .unrecoverable
        return concealed
    }
}
