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

import AVFoundation
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "audio")

/// SPSC-ish jitter ring (interleaved float, `channels` per frame), drain thread → render
/// callback. The unfair lock is held for microseconds; fine at render-callback rates. Priming:
/// reads return silence until enough is buffered (at least `prefill`, and at least one
/// packet more than the device's render quantum — large-buffer devices would otherwise
/// chronically out-demand the prefill and oscillate prime → dropout → re-prime), and an
/// underrun re-primes, concealing jitter as one short dip instead of sustained crackle.
/// All counts stay whole frames (multiples of `channels`), so the interleave can never slip.
final class AudioRing: @unchecked Sendable {
    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var primed = false
    private var renderQuantum = 0
    private let prefill: Int
    private let highWater: Int
    private let channels: Int
    private let lock = OSAllocatedUnfairLock()

    /// `capacity`/`prefill` in samples (interleaved — `channels` per frame, both whole frames).
    init(capacity: Int, prefill: Int, channels: Int) {
        buf = [Float](repeating: 0, count: capacity)
        self.prefill = prefill
        self.channels = channels
        highWater = prefill * 4
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
            // One 5 ms host packet (240 frames × channels) of slack beyond the device's demand.
            if available >= max(prefill, renderQuantum + 240 * channels) {
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
    // 8192 frames × up to 8 channels (7.1) — the render block caps `frames` at 8192.
    let ptr = UnsafeMutablePointer<Float>.allocate(capacity: 8192 * 8)
    deinit { ptr.deallocate() }
}

/// CoreAudio channel layout for the canonical wire order FL FR FC LFE RL RR [SL SR]. nil for
/// stereo (the standard layout is correct). For 5.1/7.1 we list explicit channel labels via
/// `kAudioChannelLayoutTag_UseChannelDescriptions` — preset tags (DTS_5_1 etc.) don't reliably
/// match Moonlight's order. NB the 7.1 mapping (verified against the WASAPI 0x63F + SPA orderings):
/// wire idx 4-5 = RL/RR = the WAVE *back* pair → LeftSurround/RightSurround; idx 6-7 = SL/SR = the
/// WAVE *side* pair → LeftSurroundDirect/RightSurroundDirect. (Using RearSurround* for 6-7 would
/// swap side/back vs the Windows/Linux clients.)
private func wireChannelLayout(channels: Int) -> AVAudioChannelLayout? {
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
    #if !os(macOS)
    /// AVAudioSession `setCategory`/`setActive` are synchronous and block on the audio server, so
    /// they must not run on the main thread (UI stall — AVFoundation warns about it). PROCESS-WIDE
    /// (static) so every SessionAudio shares one serial queue: the AVAudioSession is a process
    /// singleton, and across a reconnect the old session's deactivate must be ordered before the
    /// new session's activate (a per-instance queue would let them race and leave the new session's
    /// audio deactivated). stop() enqueues its deactivate promptly so it lands before the next
    /// session's activate.
    private static let sessionQueue = DispatchQueue(label: "io.unom.punktfunk.audio.session")
    #endif

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Backstop for an owner dropping us without stop() — unblocks the drain thread
    /// (which captures the connection strongly, NOT self) within one poll timeout.
    /// Engine teardown still belongs to stop().
    deinit {
        flag.stop()
    }

    /// Start playback (and, if enabled+authorized, the mic uplink). Empty UIDs = system default
    /// device; on iOS the UIDs are ignored entirely (routes are AVAudioSession-managed). On macOS
    /// the engines start synchronously on the caller's (main) thread. On iOS/tvOS start() is
    /// ASYNCHRONOUS: it activates the AVAudioSession off the main thread, then starts the engines on
    /// a later main-queue hop (gated by `!flag.isStopped`) — so playback is live shortly after, not
    /// on return. The mic may start later still if the permission prompt is pending.
    public func start(speakerUID: String, micUID: String, micEnabled: Bool) {
        #if os(macOS)
        // No AVAudioSession on macOS — start the engines directly (caller's thread, as before).
        startEngines(speakerUID: speakerUID, micUID: micUID, micEnabled: micEnabled)
        #else
        // Configure + activate the session OFF the main thread (it blocks on the audio server),
        // then start the engines back on the main thread once it's active — engine routing/format
        // depend on the active session. A stop() racing in between is caught by the flag guard.
        Self.sessionQueue.async { [weak self] in
            guard let self else { return }
            self.activateAudioSession(micEnabled: micEnabled)
            DispatchQueue.main.async { [weak self] in
                guard let self, !self.flag.isStopped else { return }
                self.startEngines(speakerUID: speakerUID, micUID: micUID, micEnabled: micEnabled)
            }
        }
        #endif
    }

    #if !os(macOS)
    /// Route + policy live in the session, not per-engine: stereo playback, mic capture when
    /// enabled, Bluetooth allowed. Failure is non-fatal (defaults). Runs on `sessionQueue`.
    private func activateAudioSession(micEnabled: Bool) {
        let session = AVAudioSession.sharedInstance()
        do {
            #if os(iOS)
            if micEnabled {
                // .defaultToSpeaker: .playAndRecord otherwise routes to the iPhone EARPIECE; only
                // affects the built-in route (headphones/BT still win).
                try session.setCategory(
                    .playAndRecord, mode: .default,
                    options: [.allowBluetoothA2DP, .defaultToSpeaker])
            } else {
                try session.setCategory(.playback, mode: .default)
            }
            #else // tvOS — no app-accessible mic
            try session.setCategory(.playback, mode: .default)
            #endif
            try session.setActive(true)
        } catch {
            log.warning("AVAudioSession setup failed: \(error.localizedDescription)")
        }
    }
    #endif

    /// Build + start the playback engine (and the mic uplink when enabled + authorized). Main
    /// thread (engine setup); on iOS/tvOS the session is already active by the time this runs.
    private func startEngines(speakerUID: String, micUID: String, micEnabled: Bool) {
        startPlayback(speakerUID: speakerUID)
        #if os(tvOS)
        // No app-accessible microphone input on tvOS — playback only.
        #else
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
        #endif
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
        #if !os(macOS)
        // Release the session so audio we interrupted (Music, podcasts) gets its resume cue. Like
        // activation, setActive is synchronous/blocking — run it on the shared serial session queue
        // (off the main thread). Enqueued HERE — engines already stopped, and BEFORE the drain wait
        // below — so across a reconnect it lands ahead of the next session's activate on the shared
        // queue (otherwise a deferred deactivate could deactivate the new session). Fire-and-forget.
        Self.sessionQueue.async {
            do {
                try AVAudioSession.sharedInstance().setActive(
                    false, options: .notifyOthersOnDeactivation)
            } catch {
                log.warning("AVAudioSession deactivation failed: \(error.localizedDescription)")
            }
        }
        #endif
        if wasDraining {
            _ = drainDone.wait(timeout: .now() + .milliseconds(400))
        }
    }

    // MARK: - Playback (host → speaker)

    private func startPlayback(speakerUID: String) {
        // Build the playback layout from the host-RESOLVED channel count (never the request):
        // 2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR.
        let channels = Int(connection.resolvedAudioChannels)
        // 1 s interleaved capacity, ~20 ms prefill (four 5 ms host packets of jitter absorption
        // before the first sample plays), both scaled by the channel count.
        let ring = AudioRing(
            capacity: 48_000 * channels, prefill: 960 * channels, channels: channels)

        let engine = AVAudioEngine()
        #if os(macOS)
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
        #endif

        // Engine-native deinterleaved float; the render block deinterleaves from the ring. Surround
        // uses an explicit wire-order channel layout; the mixer downmixes to the output device when
        // it has fewer speakers (e.g. an iPhone's stereo built-ins). (Explicit if/else rather than
        // map/flatMap so it's correct whether the channelLayout initializer is failable or not.)
        var format: AVAudioFormat?
        if channels == 2 {
            format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 2)
        } else if let layout = wireChannelLayout(channels: channels) {
            format = AVAudioFormat(standardFormatWithSampleRate: 48_000, channelLayout: layout)
        }
        guard let format else {
            log.error("could not build \(channels)-channel audio format — audio disabled")
            return
        }
        let scratch = ScratchBuffer() // block-owned; freed with the closure
        let source = AVAudioSourceNode(format: format) { _, _, frameCount, abl -> OSStatus in
            let frames = Int(frameCount)
            guard frames <= 8192 else { return kAudioUnitErr_TooManyFramesToProcess }
            ring.read(into: scratch.ptr, count: frames * channels)
            let buffers = UnsafeMutableAudioBufferListPointer(abl)
            // Deinterleave the wire-order interleaved ring into the engine's per-channel buses.
            if buffers.count >= channels {
                for ch in 0..<channels {
                    if let dst = buffers[ch].mData?.assumingMemoryBound(to: Float.self) {
                        for f in 0..<frames { dst[f] = scratch.ptr[f * channels + ch] }
                    }
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
            // Decode happens IN-CORE (libopus multistream) — AudioToolbox's Opus path is
            // stereo-only — and is handed back as interleaved f32 PCM in wire channel order.
            while !flag.isStopped {
                let pcm: PunktfunkConnection.AudioPCM?
                do {
                    pcm = try connection.nextAudioPcm(timeoutMs: 100)
                } catch {
                    break // session closed
                }
                guard let pcm, pcm.frameCount > 0 else { continue }
                pcm.samples.withUnsafeBufferPointer { p in
                    if let base = p.baseAddress {
                        ring.write(base, count: pcm.frameCount * pcm.channels)
                    }
                }
            }
        }
        thread.name = "punktfunk-audio"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    // MARK: - Mic (mic → host)

    #if !os(tvOS)
    private func startCapture(micUID: String) {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        #if os(macOS)
        if !micUID.isEmpty {
            if let dev = AudioDevices.deviceID(forUID: micUID), let unit = input.audioUnit {
                if !Self.setDevice(dev, on: unit) {
                    log.error("could not select microphone \(micUID) — using default")
                }
            } else {
                log.warning("microphone \(micUID) not present — using default")
            }
        }
        #endif

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
    #endif

    #if os(macOS)
    private static func setDevice(_ id: AudioDeviceID, on unit: AudioUnit) -> Bool {
        var dev = id
        return AudioUnitSetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
            &dev, UInt32(MemoryLayout<AudioDeviceID>.size)) == noErr
    }
    #endif
}
