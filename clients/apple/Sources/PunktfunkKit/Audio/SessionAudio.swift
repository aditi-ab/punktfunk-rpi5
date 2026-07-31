// Session audio, both directions:
//
//   host → speaker: a drain thread pulls Opus packets (nextAudio, its own plane in the
//   core), decodes via OpusDecoder, and writes PCM into a jitter ring; an
//   AVAudioSourceNode pulls from the ring (silence on underrun with re-priming, so a
//   network gap costs one dip, not permanent crackle).
//
//   mic → host: a second AVAudioEngine taps the input device, folds it to one mono bus (the
//   chosen channel of a multi-channel interface, or a sum of all channels), resamples to 48 kHz
//   mono, slices 10 ms chunks, Opus-encodes, and sendMic()s each packet — the host feeds them
//   into a virtual PipeWire source.
//
// Devices are chosen by UID ("" = system default: the engine is then never pinned to a
// concrete device and follows default-device changes). Two engines, not one — a single
// AVAudioEngine ties input+output to one aggregate clock, separate engines keep
// arbitrary mic/speaker combinations trivial.

import AVFoundation
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "audio")

/// Render-block-owned scratch storage: freed exactly when the closure (and thus the
/// last possible render call) is released — never racing CoreAudio.
private final class ScratchBuffer {
    // 8192 frames × up to 8 channels (7.1) — the render block caps `frames` at 8192.
    let ptr = UnsafeMutablePointer<Float>.allocate(capacity: 8192 * 8)
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
    public func start(speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool) {
        #if os(macOS)
        // No AVAudioSession on macOS — start the engines directly (caller's thread, as before).
        startEngines(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel, micEnabled: micEnabled)
        #else
        // Configure + activate the session OFF the main thread (it blocks on the audio server),
        // then start the engines back on the main thread once it's active — engine routing/format
        // depend on the active session. A stop() racing in between is caught by the flag guard.
        Self.sessionQueue.async { [weak self] in
            guard let self else { return }
            self.activateAudioSession(micEnabled: micEnabled)
            DispatchQueue.main.async { [weak self] in
                guard let self, !self.flag.isStopped else { return }
                self.startEngines(
                    speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
                    micEnabled: micEnabled)
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
                // Uplink latency: ask for 5 ms IO quanta at the wire rate (the default ~10-23 ms
                // quantum is most of the mic path's burst latency). Best-effort — the hardware
                // has the final word (a Bluetooth route will ignore both), and whatever quantum
                // is actually granted, the capture tap handles the buffers it gets.
                try? session.setPreferredIOBufferDuration(0.005)
                try? session.setPreferredSampleRate(48_000)
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
    private func startEngines(
        speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool
    ) {
        startPlayback(speakerUID: speakerUID)
        #if os(tvOS)
        // No app-accessible microphone input on tvOS — playback only.
        #else
        guard micEnabled else { return }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            startCapture(micUID: micUID, micChannel: micChannel)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self, granted, !self.flag.isStopped else { return }
                    self.startCapture(micUID: micUID, micChannel: micChannel)
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

    /// Background keep-alive: silence the mic uplink while backgrounded (privacy — no room audio
    /// leaves the device) and restore it on return. Pauses/resumes the capture engine; a no-op when
    /// there's no uplink (playback-only / tvOS / mic disabled). The audio SESSION stays active for
    /// background playback, so iOS may keep showing the recording indicator until a full reconfigure
    /// — this stops the actual capture, which is the privacy-relevant part. Main thread.
    public func setMicMuted(_ muted: Bool) {
        stateLock.lock()
        let capture = captureEngine
        stateLock.unlock()
        guard let capture else { return }
        if muted {
            capture.pause()
        } else if !flag.isStopped {
            try? capture.start()
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
            // Per-iteration autorelease pool: no runloop on this thread (see Stage2Pipeline).
            var alive = true
            while alive, !flag.isStopped {
                alive = autoreleasepool { () -> Bool in
                let pcm: PunktfunkConnection.AudioPCM?
                do {
                    pcm = try connection.nextAudioPcm(timeoutMs: 100)
                } catch {
                    return false // session closed
                }
                guard let pcm, pcm.frameCount > 0 else { return true }
                pcm.samples.withUnsafeBufferPointer { p in
                    if let base = p.baseAddress {
                        ring.write(base, count: pcm.frameCount * pcm.channels)
                    }
                }
                return true
                }
            }
        }
        thread.name = "punktfunk-audio"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    // MARK: - Mic (mic → host)

    #if !os(tvOS)
    private func startCapture(micUID: String, micChannel: Int) {
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

        // Multi-channel-interface handling. A pro interface exposes N discrete inputs with the mic
        // on ONE of them, but AVAudioConverter's N→stereo downmix takes channels 0/1 — dead
        // silence when the mic sits higher up (the classic "host receives zeros"). So we fold the
        // input to a single mono bus OURSELVES and resample that. micChannel: 0 = Auto (sum every
        // channel — a lone hot mic passes at full level), n≥1 pins 1-based input channel n.
        let inChannels = Int(inFormat.channelCount)
        let pinnedChannel: Int? = {
            guard micChannel >= 1 else { return nil }
            let idx = micChannel - 1
            guard idx < inChannels else {
                log.warning(
                    "mic channel \(micChannel) out of range (device has \(inChannels)) — mixing all")
                return nil
            }
            return idx
        }()
        let channelPlan = pinnedChannel.map { "channel \($0 + 1)/\(inChannels)" }
            ?? (inChannels > 1 ? "mix \(inChannels)ch→mono" : "mono")

        // Name the device we're ACTUALLY recording from + its format + how we fold it, once per
        // session. This single line localizes the whole class of "host receives silence" failures
        // that otherwise need a host-side tone injection to pin down: a UID that silently fell back
        // to the default, the wrong device being live, or the wrong channel picked.
        #if os(macOS)
        if let unit = input.audioUnit, let live = Self.currentDevice(of: unit),
           let dev = AudioDevices.describe(live) {
            if !micUID.isEmpty, dev.uid != micUID {
                log.warning("""
                    mic selection not honored — requested \(micUID) but capturing from \
                    \(dev.name) [\(dev.uid)]; the device's UID likely changed (replug) — \
                    reselect it in Settings
                    """)
            }
            log.info("""
                mic capture: \(dev.name) [\(dev.uid)] — \(Int(inFormat.sampleRate)) Hz, \
                \(inChannels) ch, \(channelPlan)
                """)
        } else {
            log.info("""
                mic capture: <device unavailable> — \(Int(inFormat.sampleRate)) Hz, \
                \(inChannels) ch, \(channelPlan)
                """)
        }
        #else
        log.info(
            "mic capture: \(Int(inFormat.sampleRate)) Hz, \(inChannels) ch, \(channelPlan)")
        #endif

        // Encode a single mono bus (folded from `inFormat` in the tap): the resampler goes
        // mono@inputSR → the encoder's 48 kHz mono, so it handles the rate change and the
        // wrong-channel downmix never happens. Mono end to end — the host's decoder upmixes,
        // so the old duplicate-into-stereo step only cost bits and cycles.
        //
        // `mono`/`staging` are the per-callback scratch buffers, preallocated HERE (grown only
        // if a larger-than-expected device quantum ever arrives) — the steady-state tap path
        // allocates nothing.
        let scratchFrames: AVAudioFrameCount = 8192
        let stagingCapacity = { (frames: AVAudioFrameCount) -> AVAudioFrameCount in
            AVAudioFrameCount(
                (Double(frames) * 48_000 / inFormat.sampleRate).rounded(.up)) + 64
        }
        guard let monoFormat = AVAudioFormat(
                  commonFormat: .pcmFormatFloat32, sampleRate: inFormat.sampleRate,
                  channels: 1, interleaved: false),
              let encoder = try? OpusEncoder(),
              let resampler = AVAudioConverter(from: monoFormat, to: encoder.pcmFormat),
              let chunk = AVAudioPCMBuffer(
                  pcmFormat: encoder.pcmFormat, frameCapacity: encoder.framesPerPacket),
              let monoScratch = AVAudioPCMBuffer(
                  pcmFormat: monoFormat, frameCapacity: scratchFrames),
              let stagingScratch = AVAudioPCMBuffer(
                  pcmFormat: encoder.pcmFormat, frameCapacity: stagingCapacity(scratchFrames))
        else {
            log.error("Opus encoder unavailable — mic uplink disabled")
            return
        }

        // Tap-thread-confined state: fold into `mono`, resample into `staging`, accumulate in
        // `fifo`, slice `framesPerPacket` (10 ms) chunks for the encoder.
        var mono = monoScratch
        var staging = stagingScratch
        var fifo: [Float] = []
        fifo.reserveCapacity(48_000)
        var seq: UInt32 = 0
        let connection = connection
        let flag = flag

        // Silence tripwire (tap-confined): a "recording" app can be handed pure digital zeros —
        // a zeroed input-volume slider, a stale TCC grant, a muted device, OR the wrong channel
        // picked — and everything downstream looks alive while the host gets silence. Track the
        // peak of the EXTRACTED mono bus over the first ~10 s (not the raw device — a mic present
        // on a channel we didn't grab must still read as silence) and emit exactly ONE verdict.
        // This is the log line whose absence made the last occurrence take a host-side tone.
        let silenceWindow = Int(inFormat.sampleRate * 10)
        let deviceLabel = micUID.isEmpty ? "default input" : micUID
        var framesInspected = 0
        var inputPeak: Float = 0
        var levelReported = false

        // 480 frames = 10 ms, matching the packet duration. Advisory — CoreAudio delivers the
        // device quantum whatever we ask (the old 2048 request came back as 42.7 ms bursts, most
        // of the uplink's latency) — but where the system honors it, the tap fires per-packet.
        input.installTap(onBus: 0, bufferSize: 480, format: inFormat) { buffer, _ in
            if flag.isStopped { return }
            let frames = Int(buffer.frameLength)
            guard frames > 0, let src = buffer.floatChannelData else { return }
            if frames > Int(mono.frameCapacity) {
                // A quantum larger than the scratch (bufferSize is advisory both ways) — regrow
                // once to the new high-water mark; the steady state stays allocation-free.
                guard let biggerMono = AVAudioPCMBuffer(
                          pcmFormat: monoFormat, frameCapacity: buffer.frameLength),
                      let biggerStaging = AVAudioPCMBuffer(
                          pcmFormat: encoder.pcmFormat,
                          frameCapacity: stagingCapacity(buffer.frameLength))
                else { return }
                mono = biggerMono
                staging = biggerStaging
            }
            guard let dst = mono.floatChannelData?[0] else { return }
            mono.frameLength = buffer.frameLength

            // Fold the multi-channel input down to the one mono bus we encode.
            Self.foldToMono(
                input: src, frames: frames, channels: Int(buffer.format.channelCount),
                interleaved: buffer.format.isInterleaved, pinned: pinnedChannel, out: dst)

            if !levelReported {
                var localPeak: Float = 0
                for i in 0..<frames where abs(dst[i]) > localPeak { localPeak = abs(dst[i]) }
                if localPeak > inputPeak { inputPeak = localPeak }
                framesInspected += frames
                if framesInspected >= silenceWindow {
                    levelReported = true
                    if inputPeak == 0 {
                        log.warning("""
                            mic uplink has been pure digital SILENCE for 10 s (\(deviceLabel), \
                            \(channelPlan)) — check the input level (System Settings → Sound → \
                            Input), Privacy & Security → Microphone, and the Microphone channel in \
                            Settings; the host is receiving zeros
                            """)
                    } else {
                        let dbfs = 20 * log10(inputPeak)
                        log.info("""
                            mic uplink OK — peak \(String(format: "%.1f", dbfs)) dBFS over first \
                            10 s (\(deviceLabel), \(channelPlan))
                            """)
                    }
                }
            }

            var fed = false
            var convError: NSError?
            let status = resampler.convert(to: staging, error: &convError) { _, outStatus in
                if fed {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                fed = true
                outStatus.pointee = .haveData
                return mono
            }
            guard status != .error, let p = staging.floatChannelData?[0] else { return }
            fifo.append(contentsOf: UnsafeBufferPointer(
                start: p, count: Int(staging.frameLength)))

            // Consume whole chunks through a head index, then drop the eaten prefix in ONE
            // move of the sub-chunk remainder. The old per-chunk removeFirst memmoved the
            // entire backlog for every packet — O(n) on the render-adjacent tap thread.
            let samplesPerChunk = Int(encoder.framesPerPacket)
            var head = 0
            while fifo.count - head >= samplesPerChunk {
                chunk.frameLength = encoder.framesPerPacket
                fifo.withUnsafeBufferPointer { src in
                    chunk.floatChannelData![0].update(
                        from: src.baseAddress! + head, count: samplesPerChunk)
                }
                head += samplesPerChunk
                guard let packets = try? encoder.encode(chunk) else { continue }
                for packet in packets {
                    connection.sendMic(
                        packet, seq: seq, ptsNs: DispatchTime.now().uptimeNanoseconds)
                    seq &+= 1
                }
            }
            if head > 0 { fifo.removeFirst(head) } // keeps capacity — no realloc
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

    /// Fold `channels` of input (`floatChannelData` layout: `interleaved` → one buffer strided by
    /// channel count; else one buffer per channel) down to a single mono bus in `out` (`frames`
    /// long). `pinned` (0-based, must be `< channels`) copies exactly that channel — the fix for a
    /// mic on one input of a multi-channel interface; `nil` sums every channel, clamped to
    /// [-1, 1], so a lone hot channel still passes at full level instead of the silent 0/1 the
    /// default N→stereo downmix would grab. Pure + `internal` for unit testing the index math.
    static func foldToMono(
        input: UnsafePointer<UnsafeMutablePointer<Float>>, frames: Int, channels: Int,
        interleaved: Bool, pinned: Int?, out: UnsafeMutablePointer<Float>
    ) {
        if let ch = pinned, ch < channels {
            if interleaved {
                let d = input[0]
                for i in 0..<frames { out[i] = d[i * channels + ch] }
            } else {
                let d = input[ch]
                for i in 0..<frames { out[i] = d[i] }
            }
        } else if interleaved {
            let d = input[0]
            for i in 0..<frames {
                var s: Float = 0
                for c in 0..<channels { s += d[i * channels + c] }
                out[i] = max(-1, min(1, s))
            }
        } else {
            let d0 = input[0]
            for i in 0..<frames { out[i] = d0[i] }
            for c in 1..<channels {
                let d = input[c]
                for i in 0..<frames { out[i] += d[i] }
            }
            if channels > 1 { for i in 0..<frames { out[i] = max(-1, min(1, out[i])) } }
        }
    }
    #endif

    #if os(macOS)
    private static func setDevice(_ id: AudioDeviceID, on unit: AudioUnit) -> Bool {
        var dev = id
        return AudioUnitSetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
            &dev, UInt32(MemoryLayout<AudioDeviceID>.size)) == noErr
    }

    /// Read back the AUHAL's live device — the definitive "what are we actually capturing
    /// from", which catches a selection that succeeded on paper but silently fell back to
    /// the system default (a stale/changed UID, a device that vanished between resolve and
    /// start). 0 / an error means we couldn't tell.
    private static func currentDevice(of unit: AudioUnit) -> AudioDeviceID? {
        var dev = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        let status = AudioUnitGetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &dev, &size)
        guard status == noErr, dev != 0 else { return nil }
        return dev
    }
    #endif
}
