import AVFoundation
import os

/// SPSC-ish jitter ring (interleaved float, `channels` per frame), drain thread → render
/// callback. The unfair lock is held for microseconds; fine at render-callback rates. Priming:
/// reads return silence until enough is buffered (at least the target, and at least one
/// packet more than the device's render quantum — large-buffer devices would otherwise
/// chronically out-demand the prefill and oscillate prime → dropout → re-prime).
/// All counts stay whole frames (multiples of `channels`), so the interleave can never slip.
///
/// **Drift correction.** Both ends run at 48 kHz but on different crystals, so backlog from a
/// network stall or plain host-vs-DAC skew never drains on its own: without correction one 300 ms
/// hiccup leaves audio 300 ms behind video for the rest of the session. This used to be handled by
/// a `highWater` shed that dropped a whole `2 × prefill` at once — its own comment called that "one
/// audible blip". It is now the same two-stage scheme the Rust clients share
/// (`punktfunk_core::audio::JitterPolicy`): a slow depth average that sits above target for a
/// sustained window sheds ONE 5 ms frame with a crossfade, and the hard cap is only a backstop.
///
/// **Adaptive depth.** The target is a floor, not a constant: repeated genuine underruns grow it
/// a step at a time (`noteRead`, mirroring `JitterPolicy::note_read`) up to `maxTargetMS`, and a
/// long quiet spell relaxes it back toward the base — so a session on Wi-Fi that bunches arrivals
/// deepens until it stops crackling, while a clean LAN keeps the tight base latency. Keep the
/// constants here in step with `JitterTuning.COREAUDIO`.
final class AudioRing: @unchecked Sendable {
    /// Mirrors `JitterTuning::COREAUDIO` — see that type for the rationale.
    private static let targetMS = 20
    private static let maxTargetMS = 70
    private static let headroomMS = 30
    private static let hardCapMS = 90
    private static let deprimeAfter = 4
    /// The protocol's frame: the shed unit, and the slack added over a large device quantum.
    private static let frameMS = 5
    /// Depth average must exceed target by this before drift correction fires — the middle of the
    /// headroom band, so the smooth shed always gets its chance BEFORE the hard cap trims.
    private static let shedExcessMS = 15
    /// …and must stay there for this much consumed audio. Long, because a shed is the only thing
    /// here a listener could notice; it must never fire on a transient.
    private static let shedSustainMS = 2_000
    private static let crossfadeMS = 2
    /// Time constant of the depth average.
    private static let ewmaTauMS = 1_000
    /// Adaptive target floor, mirroring `JitterPolicy::note_read`: this many genuine underruns
    /// inside one window grow the live target a step (up to `maxTargetMS`), and a long quiet
    /// spell relaxes it a step back toward the base — so only the sessions that actually starve
    /// (Wi-Fi power-save bunching is the classic) pay for extra depth, and only while they need
    /// it. All spans are measured in consumed samples, like the Rust policy.
    private static let growUnderruns = 3
    private static let growWindowMS = 5_000
    private static let growStepMS = 10
    private static let shrinkQuietMS = 30_000

    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var primed = false
    private var renderQuantum = 0
    private var emptyReads = 0
    private var depthAvg: Double = 0
    private var overRun = 0
    /// The live target in interleaved samples — `targetMS` grown by underrun pressure
    /// (`noteRead`), never below the base. Set in `init` (needs `perMS`).
    private var targetLive = 0
    /// Underruns seen in the current growth window, and the window's consumed-sample count.
    private var underrunsInWindow = 0
    private var windowRun = 0
    /// Consumed samples since the last underrun (drives the relax-back-down step).
    private var quietRun = 0
    /// Reported, not acted on: short reads that actually starved the callback, and smooth drift
    /// corrections. A rising underrun count means the ring is being starved (network or CPU),
    /// which is a different problem from the depth being wrong.
    private var underrunCount = 0
    private var shedCount = 0
    private let channels: Int
    private let perMS: Int
    private let lock = OSAllocatedUnfairLock()

    /// `capacity` in samples (interleaved — `channels` per frame, a whole number of frames).
    /// The de-jitter depth is the ring's own business (`targetMS`), not a caller's prefill.
    init(capacity: Int, channels: Int) {
        buf = [Float](repeating: 0, count: capacity)
        self.channels = channels
        perMS = 48 * channels
        targetLive = Self.targetMS * perMS
    }

    /// Effective target depth in interleaved samples: the (adaptively grown) live target, lifted
    /// so it can always serve one device quantum plus a packet (a large-buffer device cannot
    /// sustain a target below its own quantum).
    private var target: Int {
        max(targetLive, renderQuantum + Self.frameMS * perMS)
    }

    func write(_ samples: UnsafePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = buf.count
        // A single write larger than the whole ring would push readIdx PAST writeIdx below
        // (inverting the valid range — corruption). It never happens (one decoded packet is far
        // under capacity), but guard rather than corrupt.
        guard count <= capacity else { return }
        if writeIdx + count - readIdx > capacity {
            readIdx = writeIdx + count - capacity // overflow: drop oldest
        }
        for i in 0..<count {
            buf[(writeIdx + i) % capacity] = samples[i]
        }
        writeIdx += count
        // Backstop only: the smooth shed in `read` is what normally holds the depth down. The
        // hard cap must always leave room for one device quantum past the target (mirrors the
        // Rust policy's `.max(target + want)`) or a large-quantum device would trim itself into
        // a permanent underrun.
        let cap = max(
            min(target + Self.headroomMS * perMS, Self.hardCapMS * perMS),
            target + renderQuantum)
        if writeIdx - readIdx > cap {
            readIdx = writeIdx - cap
            depthAvg = Double(cap)
            overRun = 0
        }
    }

    /// Fills `out` completely (silence beyond what's buffered).
    func read(into out: UnsafeMutablePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        renderQuantum = max(renderQuantum, count)
        let available = writeIdx - readIdx

        // Depth average, weighted by the callback size so its time constant is independent of the
        // device quantum.
        let alpha = min(1.0, Double(count) / Double(Self.ewmaTauMS * perMS))
        depthAvg += (Double(available) - depthAvg) * alpha

        if !primed {
            if available >= target {
                primed = true
                emptyReads = 0
            } else {
                for i in 0..<count { out[i] = 0 }
                return
            }
        }

        // Drift correction: shed exactly one frame, crossfaded, once the AVERAGE has sat above
        // the threshold for the sustain window. Anything shorter is jitter and must be left alone.
        if depthAvg > Double(target + Self.shedExcessMS * perMS) {
            overRun += count
            if overRun >= Self.shedSustainMS * perMS {
                overRun = 0
                shedOneFrame()
                shedCount += 1
                depthAvg = Double(writeIdx - readIdx)
            }
        } else {
            overRun = 0
        }

        let n = min(writeIdx - readIdx, count)
        let capacity = buf.count
        for i in 0..<n {
            out[i] = buf[(readIdx + i) % capacity]
        }
        readIdx += n
        if n < count {
            for i in n..<count { out[i] = 0 }
        }
        noteRead(ranShort: n < count, count: count)
    }

    /// The outcome accounting of one primed read — the Swift mirror of
    /// `JitterPolicy::note_read`. A short read drives both the de-prime hysteresis (a single
    /// transient drain must not manufacture a whole target's worth of fresh silence) and the
    /// adaptive target floor: a device that genuinely keeps starving gets more slack, one step
    /// per window, capped — and gives it back after a long quiet spell, so one bad minute
    /// doesn't cost latency for the rest of the session. Caller holds the lock.
    private func noteRead(ranShort: Bool, count: Int) {
        windowRun += count
        if windowRun >= Self.growWindowMS * perMS {
            windowRun = 0
            underrunsInWindow = 0
        }
        if ranShort {
            quietRun = 0
            emptyReads += 1
            underrunCount += 1
            if emptyReads >= Self.deprimeAfter {
                primed = false
                emptyReads = 0
            }
            underrunsInWindow += 1
            if underrunsInWindow >= Self.growUnderruns {
                underrunsInWindow = 0
                windowRun = 0
                targetLive = min(targetLive + Self.growStepMS * perMS, Self.maxTargetMS * perMS)
            }
        } else {
            emptyReads = 0
            quietRun += count
            if quietRun >= Self.shrinkQuietMS * perMS {
                quietRun = 0
                targetLive = max(targetLive - Self.growStepMS * perMS, Self.targetMS * perMS)
            }
        }
    }

    /// Drop one protocol frame from the front, linearly crossfading the seam so the correction is
    /// inaudible rather than a click. Mirrors `punktfunk_core::audio::crossfade_drop`; caller holds
    /// the lock.
    private func shedOneFrame() {
        let drop = Self.frameMS * perMS
        let available = writeIdx - readIdx
        guard available > drop else { return }
        let fade = min(Self.crossfadeMS * perMS, min(drop, available - drop))
        let capacity = buf.count
        if fade > 0 {
            // The tail of what we discard fades out into the head of what survives.
            for i in 0..<fade {
                let old = buf[(readIdx + drop - fade + i) % capacity]
                let new = buf[(readIdx + drop + i) % capacity]
                let t = Float(i + 1) / Float(fade + 1)
                buf[(readIdx + drop + i) % capacity] = old * (1 - t) + new * t
            }
        }
        readIdx += drop
    }

    /// Current buffered depth in milliseconds — for the stats overlay and the drain thread's
    /// periodic log.
    var bufferedMS: Int {
        lock.lock()
        defer { lock.unlock() }
        return (writeIdx - readIdx) / max(perMS, 1)
    }

    /// One consistent snapshot of the ring's vitals, taken under a single lock so the numbers in
    /// a log line describe the same instant. Mirrors what the three Rust clients report.
    struct Stats {
        let bufferedMS: Int
        let targetMS: Int
        let underruns: Int
        let sheds: Int
    }

    var stats: Stats {
        lock.lock()
        defer { lock.unlock() }
        return Stats(
            bufferedMS: (writeIdx - readIdx) / max(perMS, 1),
            targetMS: target / max(perMS, 1),
            underruns: underrunCount,
            sheds: shedCount)
    }
}

/// CoreAudio channel layout for the canonical wire order FL FR FC LFE RL RR [SL SR]. nil for
/// stereo (the standard layout is correct). For 5.1/7.1 we list explicit channel labels via
/// `kAudioChannelLayoutTag_UseChannelDescriptions` — preset tags (DTS_5_1 etc.) don't reliably
/// match Moonlight's order. NB the 7.1 mapping (verified against the WASAPI 0x63F + SPA orderings):
/// wire idx 4-5 = RL/RR = the WAVE *back* pair → LeftSurround/RightSurround; idx 6-7 = SL/SR = the
/// WAVE *side* pair → LeftSurroundDirect/RightSurroundDirect. (Using RearSurround* for 6-7 would
/// swap side/back vs the Windows/Linux clients.)
func wireChannelLayout(channels: Int) -> AVAudioChannelLayout? {
    let labels: [AudioChannelLabel]
    switch channels {
    case 6:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen, kAudioChannelLabel_LeftSurround,
            kAudioChannelLabel_RightSurround,
        ]
    case 8:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen,
            kAudioChannelLabel_LeftSurround, kAudioChannelLabel_RightSurround, // wire RL/RR (back)
            kAudioChannelLabel_LeftSurroundDirect, kAudioChannelLabel_RightSurroundDirect, // wire SL/SR (side)
        ]
    default:
        return nil
    }
    let size = MemoryLayout<AudioChannelLayout>.size
        + (labels.count - 1) * MemoryLayout<AudioChannelDescription>.stride
    let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
    defer { raw.deallocate() }
    let layout = raw.bindMemory(to: AudioChannelLayout.self, capacity: 1)
    layout.pointee.mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions
    layout.pointee.mChannelBitmap = AudioChannelBitmap(rawValue: 0)
    layout.pointee.mNumberChannelDescriptions = UInt32(labels.count)
    // `mChannelDescriptions` is the C variable-length tail array (declared `[1]`, over-allocated
    // above). Scope the pointer with `withUnsafeMutablePointer` — taking `&…mChannelDescriptions`
    // inline yields a pointer valid only for that expression, so building a buffer from it that
    // outlives the call is a dangling-pointer bug. Inside the closure it stays valid while we fill it.
    withUnsafeMutablePointer(to: &layout.pointee.mChannelDescriptions) { tail in
        let descs = UnsafeMutableBufferPointer(start: tail, count: labels.count)
        for (i, lbl) in labels.enumerated() {
            descs[i] = AudioChannelDescription(
                mChannelLabel: lbl, mChannelFlags: AudioChannelFlags(rawValue: 0),
                mCoordinates: (0, 0, 0))
        }
    }
    return AVAudioChannelLayout(layout: layout)
}
