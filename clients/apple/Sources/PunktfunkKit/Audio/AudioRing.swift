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
/// Keep the constants here in step with `JitterTuning.COREAUDIO`.
final class AudioRing: @unchecked Sendable {
    /// Mirrors `JitterTuning::COREAUDIO` — see that type for the rationale.
    private static let targetMS = 20
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

    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var primed = false
    private var renderQuantum = 0
    private var emptyReads = 0
    private var depthAvg: Double = 0
    private var overRun = 0
    private let channels: Int
    private let perMS: Int
    private let lock = OSAllocatedUnfairLock()

    /// `capacity`/`prefill` in samples (interleaved — `channels` per frame, both whole frames).
    /// `prefill` is accepted for source compatibility but the target now comes from `targetMS`.
    init(capacity: Int, prefill: Int = 0, channels: Int) {
        buf = [Float](repeating: 0, count: capacity)
        self.channels = channels
        perMS = 48 * channels
    }

    /// Live target depth in interleaved samples, lifted so it can always serve one device quantum
    /// plus a packet (a large-buffer device cannot sustain a target below its own quantum).
    private var target: Int {
        max(Self.targetMS * perMS, renderQuantum + Self.frameMS * perMS)
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
        // Backstop only: the smooth shed in `read` is what normally holds the depth down.
        let cap = min(target + Self.headroomMS * perMS, Self.hardCapMS * perMS)
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
            // De-prime only after a RUN of short reads: a single transient drain must not
            // manufacture a whole target's worth of fresh silence.
            emptyReads += 1
            if emptyReads >= Self.deprimeAfter { primed = false }
        } else {
            emptyReads = 0
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

    /// Current buffered depth in milliseconds — for the stats overlay.
    var bufferedMS: Int {
        lock.lock()
        defer { lock.unlock() }
        return (writeIdx - readIdx) / max(perMS, 1)
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
