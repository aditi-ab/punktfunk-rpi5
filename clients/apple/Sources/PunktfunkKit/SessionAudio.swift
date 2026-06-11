// Session audio, both directions:
//
//   host → speaker: a drain thread pulls Opus packets (nextAudio, its own plane in the
//   core), decodes via OpusDecoder, and writes PCM into a jitter ring; an
//   AVAudioSourceNode pulls from the ring (silence on underrun with re-priming, so a
//   network gap costs one dip, not permanent crackle).
//
//   mic → host: a second AVAudioEngine taps the input device, resamples to 48 kHz
//   stereo, slices 20 ms chunks, Opus-encodes, and sendMic()s each packet — the host
//   feeds them into a virtual PipeWire source.
//
// Devices are chosen by UID ("" = system default: the engine is then never pinned to a
// concrete device and follows default-device changes). Two engines, not one — a single
// AVAudioEngine ties input+output to one aggregate clock, separate engines keep
// arbitrary mic/speaker combinations trivial.

#if os(macOS)
import AVFoundation
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "audio")

/// SPSC-ish jitter ring (interleaved stereo float), drain thread → render callback.
/// The unfair lock is held for microseconds; fine at render-callback rates. Priming:
/// reads return silence until enough is buffered (at least `prefill`, and at least one
/// packet more than the device's render quantum — large-buffer devices would otherwise
/// chronically out-demand the prefill and oscillate prime → dropout → re-prime), and an
/// underrun re-primes, concealing jitter as one short dip instead of sustained crackle.
/// All counts stay even (whole stereo frames), so L/R interleave can never flip.
final class AudioRing: @unchecked Sendable {
    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var primed = false
    private var renderQuantum = 0
    private let prefill: Int
    private let highWater: Int
    private let lock = OSAllocatedUnfairLock()

    /// `capacity`/`prefill` in samples (interleaved — 2 per frame, both must be even).
    init(capacity: Int, prefill: Int) {
        buf = [Float](repeating: 0, count: capacity)
        self.prefill = prefill
        highWater = prefill * 4
    }

    func write(_ samples: UnsafePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = buf.count
        if writeIdx + count - readIdx > capacity {
            readIdx = writeIdx + count - capacity // overflow: drop oldest
        }
        for i in 0..<count {
            buf[(writeIdx + i) % capacity] = samples[i]
        }
        writeIdx += count
        // Latency clamp: both ends run at 48 kHz, so backlog from a network stall (or
        // creeping host-vs-DAC clock skew) never drains on its own — without this, one
        // 300 ms hiccup leaves audio 300 ms behind video for the rest of the session.
        // Shedding down to 2× prefill costs one audible blip instead.
        if writeIdx - readIdx > highWater {
            readIdx = writeIdx - prefill * 2
        }
    }

    /// Fills `out` completely (silence beyond what's buffered).
    func read(into out: UnsafeMutablePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        renderQuantum = max(renderQuantum, count)
        let available = writeIdx - readIdx
        if !primed {
            // 480 samples = one 5 ms host packet of slack beyond the device's demand.
            if available >= max(prefill, renderQuantum + 480) {
                primed = true
            } else {
                for i in 0..<count { out[i] = 0 }
                return
            }
        }
        let n = min(available, count)
        let capacity = buf.count
        for i in 0..<n {
            out[i] = buf[(readIdx + i) % capacity]
        }
        readIdx += n
        if n < count {
            for i in n..<count { out[i] = 0 }
            primed = false // underrun — re-prime before resuming
        }
    }
}

private final class StopFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var stopped = false
    var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }
    func stop() {
        lock.lock()
        stopped = true
        lock.unlock()
    }
}

/// Render-block-owned scratch storage: freed exactly when the closure (and thus the
/// last possible render call) is released — never racing CoreAudio.
private final class ScratchBuffer {
    let ptr = UnsafeMutablePointer<Float>.allocate(capacity: 8192 * 2)
    deinit { ptr.deallocate() }
}

public final class SessionAudio {
    private let connection: PunktfunkConnection
    private let flag = StopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    /// Owns the engine handles + drainStarted, paired with `flag`: stop() sets the flag
    /// BEFORE taking the engines, every publisher re-checks the flag under this lock
    /// after publishing-side work — so a startCapture racing stop() (the mic-permission
    /// callback arrives whenever the user clicks the prompt) can never leave a hot
    /// microphone with no owner.
    private let stateLock = NSLock()
    private var playbackEngine: AVAudioEngine?
    private var captureEngine: AVAudioEngine?
    private var drainStarted = false

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Backstop for an owner dropping us without stop() — unblocks the drain thread
    /// (which captures the connection strongly, NOT self) within one poll timeout.
    /// Engine teardown still belongs to stop().
    deinit {
        flag.stop()
    }

    /// Start playback (and, if enabled+authorized, the mic uplink). Empty UIDs = system
    /// default device. Main thread (engine setup); returns after the engines start —
    /// the mic may start slightly later if the permission prompt is pending.
    public func start(speakerUID: String, micUID: String, micEnabled: Bool) {
        startPlayback(speakerUID: speakerUID)
        guard micEnabled else { return }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            startCapture(micUID: micUID)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self, granted, !self.flag.isStopped else { return }
                    self.startCapture(micUID: micUID)
                }
            }
        default:
            log.warning("microphone access denied — mic uplink disabled (System Settings → Privacy)")
        }
    }

    /// Stop both directions. Safe from any thread; waits the drain thread out (≤ its
    /// poll timeout) so the caller can close the connection right after.
    public func stop() {
        flag.stop() // before taking the engines — see stateLock's comment
        stateLock.lock()
        let capture = captureEngine
        captureEngine = nil
        let playback = playbackEngine
        playbackEngine = nil
        let wasDraining = drainStarted
        drainStarted = false
        stateLock.unlock()
        if let capture {
            capture.inputNode.removeTap(onBus: 0)
            capture.stop()
        }
        playback?.stop()
        if wasDraining {
            _ = drainDone.wait(timeout: .now() + .milliseconds(400))
        }
    }

    // MARK: - Playback (host → speaker)

    private func startPlayback(speakerUID: String) {
        // 1 s of interleaved stereo capacity, ~20 ms prefill: four 5 ms host packets of
        // jitter absorption before the first sample plays.
        let ring = AudioRing(capacity: 96_000, prefill: 1920)

        let engine = AVAudioEngine()
        if !speakerUID.isEmpty {
            if let dev = AudioDevices.deviceID(forUID: speakerUID),
               let unit = engine.outputNode.audioUnit {
                if !Self.setDevice(dev, on: unit) {
                    log.error("could not select speaker \(speakerUID) — using default")
                }
            } else {
                log.warning("speaker \(speakerUID) not present — using default")
            }
        }

        // Engine-native deinterleaved float; the render block deinterleaves from the ring.
        guard let format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)
        else { return }
        let scratch = ScratchBuffer() // block-owned; freed with the closure
        let source = AVAudioSourceNode(format: format) { _, _, frameCount, abl -> OSStatus in
            let frames = Int(frameCount)
            guard frames <= 8192 else { return kAudioUnitErr_TooManyFramesToProcess }
            ring.read(into: scratch.ptr, count: frames * 2)
            let buffers = UnsafeMutableAudioBufferListPointer(abl)
            if buffers.count >= 2,
               let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
               let right = buffers[1].mData?.assumingMemoryBound(to: Float.self) {
                for f in 0..<frames {
                    left[f] = scratch.ptr[f * 2]
                    right[f] = scratch.ptr[f * 2 + 1]
                }
            }
            return noErr
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        engine.prepare()
        do {
            try engine.start()
        } catch {
            log.error("playback engine failed to start: \(error.localizedDescription)")
            return
        }
        stateLock.lock()
        if flag.isStopped {
            stateLock.unlock()
            engine.stop() // stop() already ran — don't strand a started engine
            return
        }
        playbackEngine = engine
        stateLock.unlock()
        startDrain(into: ring)
    }

    private func startDrain(into ring: AudioRing) {
        stateLock.lock()
        drainStarted = true
        stateLock.unlock()
        let thread = Thread { [connection, flag, drainDone] in
            defer { drainDone.signal() }
            guard let decoder = try? OpusDecoder(framesPerPacket: 240),
                  let pcm = AVAudioPCMBuffer(
                      pcmFormat: decoder.pcmFormat, frameCapacity: 5760)
            else {
                log.error("Opus decoder unavailable — audio playback disabled")
                return
            }
            while !flag.isStopped {
                let packet: AudioPacket?
                do {
                    packet = try connection.nextAudio(timeoutMs: 100)
                } catch {
                    break // session closed
                }
                guard let packet else { continue }
                do {
                    let frames = try decoder.decode(packet.data, into: pcm)
                    if frames > 0, let p = pcm.floatChannelData?[0] {
                        ring.write(p, count: Int(frames) * 2)
                    }
                } catch {
                    // One corrupt packet ≠ a dead stream; skip it.
                    log.warning("audio decode failed: \(error.localizedDescription)")
                }
            }
        }
        thread.name = "punktfunk-audio"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    // MARK: - Mic (mic → host)

    private func startCapture(micUID: String) {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        if !micUID.isEmpty {
            if let dev = AudioDevices.deviceID(forUID: micUID), let unit = input.audioUnit {
                if !Self.setDevice(dev, on: unit) {
                    log.error("could not select microphone \(micUID) — using default")
                }
            } else {
                log.warning("microphone \(micUID) not present — using default")
            }
        }

        let inFormat = input.outputFormat(forBus: 0)
        guard inFormat.sampleRate > 0, inFormat.channelCount > 0 else {
            log.error("no usable input device — mic uplink disabled")
            return
        }
        guard let encoder = try? OpusEncoder(),
              let resampler = AVAudioConverter(from: inFormat, to: encoder.pcmFormat),
              let chunk = AVAudioPCMBuffer(
                  pcmFormat: encoder.pcmFormat, frameCapacity: OpusEncoder.framesPerPacket)
        else {
            log.error("Opus encoder unavailable — mic uplink disabled")
            return
        }

        // Tap-thread-confined state: resample into `staging`, accumulate in `fifo`,
        // slice 960-frame chunks for the encoder.
        var fifo: [Float] = []
        fifo.reserveCapacity(48_000)
        var seq: UInt32 = 0
        let connection = connection
        let flag = flag

        input.installTap(onBus: 0, bufferSize: 2048, format: inFormat) { buffer, _ in
            if flag.isStopped { return }
            let ratio = 48_000 / inFormat.sampleRate
            let outCapacity = AVAudioFrameCount(
                (Double(buffer.frameLength) * ratio).rounded(.up) + 64)
            guard let staging = AVAudioPCMBuffer(
                pcmFormat: encoder.pcmFormat, frameCapacity: outCapacity)
            else { return }
            var fed = false
            var convError: NSError?
            let status = resampler.convert(to: staging, error: &convError) { _, outStatus in
                if fed {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                fed = true
                outStatus.pointee = .haveData
                return buffer
            }
            guard status != .error, let p = staging.floatChannelData?[0] else { return }
            fifo.append(contentsOf: UnsafeBufferPointer(
                start: p, count: Int(staging.frameLength) * 2))

            let samplesPerChunk = Int(OpusEncoder.framesPerPacket) * 2
            while fifo.count >= samplesPerChunk {
                chunk.frameLength = OpusEncoder.framesPerPacket
                fifo.withUnsafeBufferPointer { src in
                    chunk.floatChannelData![0].update(
                        from: src.baseAddress!, count: samplesPerChunk)
                }
                fifo.removeFirst(samplesPerChunk)
                guard let packets = try? encoder.encode(chunk) else { continue }
                for packet in packets {
                    connection.sendMic(
                        packet, seq: seq, ptsNs: DispatchTime.now().uptimeNanoseconds)
                    seq &+= 1
                }
            }
        }

        engine.prepare()
        do {
            try engine.start()
        } catch {
            log.error("capture engine failed to start: \(error.localizedDescription)")
            input.removeTap(onBus: 0)
            return
        }
        stateLock.lock()
        if flag.isStopped {
            // stop() ran while we were starting (the permission prompt resolves at the
            // user's leisure) — tear the engine down ourselves, nobody else owns it now.
            stateLock.unlock()
            input.removeTap(onBus: 0)
            engine.stop()
            return
        }
        captureEngine = engine
        stateLock.unlock()
        log.info("mic uplink started (\(micUID.isEmpty ? "default input" : micUID))")
    }

    private static func setDevice(_ id: AudioDeviceID, on unit: AudioUnit) -> Bool {
        var dev = id
        return AudioUnitSetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
            &dev, UInt32(MemoryLayout<AudioDeviceID>.size)) == noErr
    }
}
#endif
