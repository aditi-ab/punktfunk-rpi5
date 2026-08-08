// Session audio, both directions:
//
//   host → speaker: a drain thread pulls Opus packets (nextAudio, its own plane in the
//   core), decodes via OpusDecoder, and writes PCM into a jitter ring; an
//   AVAudioSourceNode pulls from the ring (silence on underrun with re-priming, so a
//   network gap costs one dip, not permanent crackle).
//
//   mic → host: a tap on the input node folds the capture to one mono bus (the chosen channel
//   of a multi-channel interface, or a sum of all channels), resamples to 48 kHz mono, slices
//   10 ms chunks, Opus-encodes, and sendMic()s each packet — the host feeds them into a
//   virtual PipeWire source.
//
// Engine topology. With the mic enabled and echo cancellation on (both defaults), BOTH
// directions run on ONE AVAudioEngine with the system voice processor engaged
// (`setVoiceProcessingEnabled`) — AEC needs render and capture on the same unit so it can
// subtract what the speaker is playing from what the mic hears; without it, a loudspeaker
// client feeds the host's own game audio straight back to it (the primary reported echo
// source). The voice processor can only follow the system DEFAULT devices, so explicit
// endpoint choices fall back to the old two-engine topology — see `wantsCombined` for the
// exact decision, and `startCapture` for why two engines handle arbitrary device pairs.
//
// Devices are chosen by UID ("" = system default: the engine is then never pinned to a
// concrete device and follows default-device changes).

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
    /// The one engine running BOTH directions when the voice processor is engaged;
    /// `playbackEngine`/`captureEngine` stay nil while this is set.
    private var combinedEngine: AVAudioEngine?
    private var drainStarted = false
    /// The mute LATCH: the effective mute the owner last asked for (see `setMicMuted`). Held
    /// because the uplink engine can appear LATER than the request — the mic permission prompt
    /// is answered at the user's leisure, and the engine is built on the grant — so a mute set
    /// in the meantime must be waiting for it. Applied by whichever start path wins the race.
    /// Guarded by `stateLock`, like the engines it applies to.
    private var micMuted = false
    /// The playback jitter ring — created by whichever engine starts playback first and KEPT
    /// across an engine rebuild (the permission-grant upgrade in `startEngines` swaps engines,
    /// not the ring, so the drain thread never has to be re-pointed). Main-thread confined,
    /// like every start path.
    private var ring: AudioRing?
    /// The video plane's end-to-end meter (capture→on-glass), if the owner wired one — the
    /// reference the A/V sync loop steers the ring against. `nil` leaves the loop inert and the
    /// ring exactly as it was before sync existed, which is also what the stage-1 fallback
    /// presenter gets: it decodes and presents inside the layer with no per-frame stamp, so it can
    /// offer no reference, and a loop with no reference must not invent one. Main-thread confined,
    /// like `ring`; the meter itself is internally locked and read from the drain thread.
    private var videoLatency: LatencyMeter?
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
    #if os(iOS)
    /// Live only for a `.playAndRecord` session: the token for the route-change observer that
    /// keeps the BUILT-IN output on the speaker rather than the earpiece (see
    /// `steerBuiltInOutputToSpeaker`). A `.playback` session already prefers the speaker and
    /// never needs steering, so the mic-off path installs nothing. Guarded by `stateLock`.
    private var routeObserver: NSObjectProtocol?
    #endif

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Backstop for an owner dropping us without stop() — unblocks the drain thread
    /// (which captures the connection strongly, NOT self) within one poll timeout.
    /// Engine teardown still belongs to stop().
    deinit {
        flag.stop()
        #if os(iOS)
        // The observer only holds self weakly, so we can be deinited with it still registered;
        // drop the token here too rather than leaking it when an owner skips stop().
        if let routeObserver { NotificationCenter.default.removeObserver(routeObserver) }
        #endif
    }

    /// Start playback (and, if enabled+authorized, the mic uplink). Empty UIDs = system default
    /// device; on iOS the UIDs are ignored entirely (routes are AVAudioSession-managed). On macOS
    /// the engines start synchronously on the caller's (main) thread. On iOS/tvOS start() is
    /// ASYNCHRONOUS: it activates the AVAudioSession off the main thread, then starts the engines on
    /// a later main-queue hop (gated by `!flag.isStopped`) — so playback is live shortly after, not
    /// on return. The mic may start later still if the permission prompt is pending.
    /// `echoCancel` picks the engine topology — see the header note and `wantsCombined`.
    ///
    /// `videoLatency` is the session's END-TO-END latency meter (capture→on-glass). Pass it to arm
    /// A/V sync: it is the only thing that tells the audio plane where the picture actually is, and
    /// without it the ring keeps today's free-running behaviour. Omit it for a playback-only or
    /// stage-1 session, where no such figure is measured.
    public func start(
        speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool, echoCancel: Bool,
        videoLatency: LatencyMeter? = nil
    ) {
        self.videoLatency = videoLatency
        #if os(macOS)
        // No AVAudioSession on macOS — start the engines directly (caller's thread, as before).
        startEngines(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
            micEnabled: micEnabled, echoCancel: echoCancel)
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
                    micEnabled: micEnabled, echoCancel: echoCancel)
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
                // NO .defaultToSpeaker here, deliberately. It reads like "prefer the speaker over
                // the earpiece", and the comment that used to sit here claimed headphones and
                // Bluetooth still won. That is true of WIRED headphones and false of Bluetooth —
                // a cable is the one way to test this and see the right answer. It is an
                // OVERRIDE, and it outranks an A2DP route: with it set, every Bluetooth headset
                // lost the stream to the phone's own speaker. That is the 0.25 field report ("no
                // audio over Bluetooth ... plays through speakers if Mic input is enabled") — mic
                // and echo cancellation both default to ON, so this branch is the DEFAULT path
                // and every Bluetooth listener hit it; turning the mic off was the accidental
                // workaround, because that lands on `.playback` below, which routes to A2DP
                // happily.
                //
                // The earpiece problem it was reaching for is real, so it is solved after
                // activation instead, against the route we were ACTUALLY given —
                // see `steerBuiltInOutputToSpeaker`.
                //
                // `.allowBluetoothA2DP` alone, also deliberately: adding `.allowBluetooth` would
                // make a headset's MIC usable, but it buys that by dragging the whole route onto
                // HFP/SCO and collapsing game audio to narrowband. High-quality A2DP output plus
                // the built-in mic is the better trade for a game-streaming client.
                try session.setCategory(
                    .playAndRecord, mode: .default,
                    options: [.allowBluetoothA2DP])
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
            #if os(iOS)
            // Only the `.playAndRecord` session can land on the earpiece, and only it accepts an
            // output override — so the mic-off (`.playback`) path deliberately does neither.
            if micEnabled {
                steerBuiltInOutputToSpeaker(session)
                installRouteObserver()
            }
            #endif
        } catch {
            log.warning("AVAudioSession setup failed: \(error.localizedDescription)")
        }
    }
    #endif

    #if os(iOS)
    /// `.playAndRecord` parks the BUILT-IN output on the earpiece — right for a phone call,
    /// useless for a game. Move it to the speaker, but ONLY when the route we were actually given
    /// is the receiver: anything external (Bluetooth, wired, CarPlay, AirPlay) is left strictly
    /// alone. That "look first" is the whole difference between this and the `.defaultToSpeaker`
    /// option it replaced, which forced the speaker unconditionally and so beat Bluetooth.
    ///
    /// Idempotent and cheap, so the route observer can simply call it again.
    private func steerBuiltInOutputToSpeaker(_ session: AVAudioSession) {
        // An override already in force shows up as `.builtInSpeaker`, not `.builtInReceiver`, so
        // re-running this never fights its own previous result.
        guard session.currentRoute.outputs.contains(where: { $0.portType == .builtInReceiver })
        else { return }
        do {
            try session.overrideOutputAudioPort(.speaker)
        } catch {
            log.warning("could not move audio off the earpiece: \(error.localizedDescription)")
        }
    }

    /// Routes change under a live session: a headset connects mid-stream, or disconnects and hands
    /// the stream back to the built-in output. iOS drops an output override whenever the route
    /// changes — which is what lets a newly-connected headset win — so the earpiece steer is a
    /// property of the CURRENT route and has to be re-applied per route. Without this, dropping
    /// Bluetooth mid-stream would land the game on the earpiece.
    private func installRouteObserver() {
        let observer = NotificationCenter.default.addObserver(
            forName: AVAudioSession.routeChangeNotification,
            object: AVAudioSession.sharedInstance(), queue: nil
        ) { [weak self] _ in
            // Arrives on whatever thread AVFoundation posts it from, and the session API blocks
            // on the audio server — so do the work on the shared session queue, like every
            // other call into it.
            SessionAudio.sessionQueue.async {
                guard let self, !self.flag.isStopped else { return }
                self.steerBuiltInOutputToSpeaker(AVAudioSession.sharedInstance())
            }
        }
        stateLock.lock()
        let stale = routeObserver
        routeObserver = observer
        stateLock.unlock()
        if let stale { NotificationCenter.default.removeObserver(stale) }
    }
    #endif

    /// Build + start the engines — combined (voice-processed) or split, per `wantsCombined` —
    /// with the mic uplink only when enabled + authorized. Main thread (engine setup); on
    /// iOS/tvOS the session is already active by the time this runs.
    private func startEngines(
        speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool, echoCancel: Bool
    ) {
        #if os(tvOS)
        // No app-accessible microphone input on tvOS — playback only.
        startPlayback(speakerUID: speakerUID)
        #else
        guard micEnabled else {
            startPlayback(speakerUID: speakerUID)
            return
        }
        let combined = wantsCombined(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
            echoCancel: echoCancel)
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            if combined {
                startCombined(speakerUID: speakerUID, micUID: micUID, micChannel: micChannel)
            } else {
                startPlayback(speakerUID: speakerUID)
                startCapture(micUID: micUID, micChannel: micChannel)
            }
        case .notDetermined:
            // Playback must not wait out the permission prompt (the user answers at their
            // leisure) — start it now, and on a grant either bolt the capture engine on
            // (split) or swap the playback engine for the combined one (the ring and its
            // drain thread carry over — see `makePlaybackChain`).
            startPlayback(speakerUID: speakerUID)
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self, granted, !self.flag.isStopped else { return }
                    if combined {
                        self.stateLock.lock()
                        let playback = self.playbackEngine
                        self.playbackEngine = nil
                        self.stateLock.unlock()
                        playback?.stop()
                        self.startCombined(
                            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel)
                    } else {
                        self.startCapture(micUID: micUID, micChannel: micChannel)
                    }
                }
            }
        default:
            startPlayback(speakerUID: speakerUID)
            log.warning("microphone access denied — mic uplink disabled (System Settings → Privacy)")
        }
        #endif
    }

    #if !os(tvOS)
    /// One engine or two: the voice processor requires render + capture on one unit, and that
    /// unit can only follow the system DEFAULT devices — so echo cancellation gets the combined
    /// engine only while nothing is explicitly pinned. On macOS a chosen speaker/mic UID or a
    /// picked input channel (the voice processor's capture side is its own mono mix — a
    /// per-channel pick can't survive it) keeps today's two-engine path, AEC-less but honoring
    /// the exact endpoints the user named. On iOS routes are session-managed and the UIDs are
    /// ignored, so the toggle alone decides.
    private func wantsCombined(
        speakerUID: String, micUID: String, micChannel: Int, echoCancel: Bool
    ) -> Bool {
        guard echoCancel else { return false }
        #if os(macOS)
        return speakerUID.isEmpty && micUID.isEmpty && micChannel == 0
        #else
        return true
        #endif
    }
    #endif

    /// Stop both directions. Safe from any thread; waits the drain thread out (≤ its
    /// poll timeout) so the caller can close the connection right after.
    public func stop() {
        flag.stop() // before taking the engines — see stateLock's comment
        stateLock.lock()
        let capture = captureEngine
        captureEngine = nil
        let playback = playbackEngine
        playbackEngine = nil
        let combined = combinedEngine
        combinedEngine = nil
        let wasDraining = drainStarted
        drainStarted = false
        #if os(iOS)
        let route = routeObserver
        routeObserver = nil
        #endif
        stateLock.unlock()
        #if os(iOS)
        // Before the deactivate below, so a route change during teardown can't re-steer a session
        // we are in the middle of releasing.
        if let route { NotificationCenter.default.removeObserver(route) }
        #endif
        if let capture {
            capture.inputNode.removeTap(onBus: 0)
            capture.stop()
        }
        playback?.stop()
        if let combined {
            combined.inputNode.removeTap(onBus: 0)
            combined.stop()
        }
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

    /// Silence the mic uplink (no room audio leaves the device) or restore it. THE one muting
    /// mechanism: the owner composes its reasons — the user's in-stream mute and the background
    /// keep-alive's privacy mute — into one effective state and passes that here, so neither can
    /// clear the other (see `SessionModel.applyMicMute`).
    ///
    /// Two-engine sessions pause/resume the capture engine; a combined session instead mutes the
    /// voice processor's input (playback shares that engine and must keep running, so the engine
    /// itself never pauses — the mute zeroes the mic at the IO unit, and the tap encodes silence).
    /// Local and instant either way: nothing is negotiated with the host, and the packets that do
    /// leave carry silence. A no-op when there's no uplink (playback-only / tvOS / mic disabled),
    /// except that the state is LATCHED for an uplink that starts later. The audio SESSION stays
    /// active for background playback, so iOS may keep showing the recording indicator until a
    /// full reconfigure — either path stops room audio leaving the device, which is the
    /// privacy-relevant part. Main thread.
    public func setMicMuted(_ muted: Bool) {
        stateLock.lock()
        micMuted = muted
        let capture = captureEngine
        let combined = combinedEngine
        stateLock.unlock()
        apply(micMuted: muted, capture: capture, combined: combined)
    }

    /// Push the latched mute onto whichever engine carries the uplink. Split out from
    /// `setMicMuted` because the start paths call it too, with the engine they just started —
    /// that's how a mute requested before the permission grant lands on the engine the grant
    /// creates. Never resumes a stopped session's engine.
    private func apply(micMuted muted: Bool, capture: AVAudioEngine?, combined: AVAudioEngine?) {
        if let combined {
            combined.inputNode.isVoiceProcessingInputMuted = muted
            return
        }
        guard let capture else { return }
        if muted {
            capture.pause()
        } else if !flag.isStopped {
            try? capture.start()
        }
    }

    // MARK: - Stats

    /// The playback plane's two latency numbers, for the stats overlay.
    ///
    /// Both, never just the depth: a deep ring on a jittery link is CORRECT behaviour — the
    /// adaptive floor put it there because the link kept starving — and only the offset separates
    /// that from a ring that is simply holding audio late. Before this pair existed the plane
    /// published nothing any surface could render (depth and target lived in a periodic log line),
    /// and a field investigation into "the audio delay seems way too high" ran all the way to its
    /// conclusion without either number.
    public struct Stats: Sendable {
        /// Decoded audio queued ahead of the speaker (ms).
        public let bufferMS: Int
        /// The A/V sync loop's smoothed offset (ms): positive = audio playing BEHIND the picture.
        /// `0` before the loop has evidence, with sync unwired, or genuinely aligned.
        public let avOffsetMS: Int
    }

    /// A snapshot of `Stats`, or nil before playback starts. Main thread (`ring` is main-confined;
    /// the ring's own numbers are taken under its lock, so they describe one instant).
    public var stats: Stats? {
        guard let s = ring?.stats else { return nil }
        return Stats(bufferMS: s.bufferedMS, avOffsetMS: s.avOffsetMS)
    }

    // MARK: - Playback (host → speaker)

    /// The playback jitter ring + the source node draining it — shared by the plain playback
    /// engine and the combined voice-processing engine, and REUSED across an engine rebuild
    /// (same session, same ring: the drain thread keeps writing right through the swap). nil
    /// when the host's channel layout can't be expressed (already logged). Main thread.
    private func makePlaybackChain()
        -> (ring: AudioRing, source: AVAudioSourceNode, format: AVAudioFormat)?
    {
        // Build the playback layout from the host-RESOLVED channel count (never the request):
        // 2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR.
        let channels = Int(connection.resolvedAudioChannels)
        // 1 s interleaved capacity, scaled by the channel count. The de-jitter depth itself is
        // the ring's own business now (`AudioRing.targetMS`, mirroring `JitterTuning::COREAUDIO`)
        // rather than a prefill passed in here.
        let ring = self.ring ?? AudioRing(capacity: 48_000 * channels, channels: channels)
        self.ring = ring

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
            return nil
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
        return (ring, source, format)
    }

    private func startPlayback(speakerUID: String) {
        guard let (ring, source, format) = makePlaybackChain() else { return }
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

    /// Idempotent — the permission-grant engine swap reaches here a second time with the
    /// drain thread already feeding the (carried-over) ring.
    private func startDrain(into ring: AudioRing) {
        stateLock.lock()
        if drainStarted {
            stateLock.unlock()
            return
        }
        drainStarted = true
        stateLock.unlock()
        // A/V sync. This thread is the only place that holds all three ingredients at once: the
        // packet's host capture `ptsNs`, the ring depth, and the video plane's end-to-end figure.
        // `ptsNs` was decoded into `AudioPCM` and then dropped on the floor right here for the
        // plane's entire existence, which is why audio ran at whatever depth its jitter ring
        // happened to settle at and nothing ever placed it against the picture.
        //
        // The escape hatch mirrors the Rust clients': a field regression in a loop that steers
        // PLAYBACK should be bisectable without a rebuild. macOS honours it from the environment;
        // elsewhere it simply never trips, which is the same as today's behaviour.
        let syncEnabled = !["1", "true"].contains(
            ProcessInfo.processInfo.environment["PUNKTFUNK_NO_AV_SYNC"] ?? "")
        // nil disarms the loop entirely — no reference, no correction (see `videoLatency`).
        let videoLatency = syncEnabled ? self.videoLatency : nil
        if !syncEnabled { log.info("A/V sync disabled by PUNKTFUNK_NO_AV_SYNC") }
        let channels = Int(connection.resolvedAudioChannels)
        let thread = Thread { [connection, flag, drainDone] in
            defer { drainDone.signal() }
            var drained = 0
            var av = AvSync(channels: channels)
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
                // Place this frame against the picture it belongs with BEFORE queueing it: the
                // depth read here is everything that must still play first, which is exactly what
                // delays it. Skipped wholesale when no meter was wired, so an un-armed session
                // does not even read the ring.
                if let videoLatency {
                    let depth = ring.bufferedSamples
                    var ts = timespec()
                    clock_gettime(CLOCK_REALTIME, &ts)
                    let nowNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
                    // Half a second of tolerance on the reference: long enough to ride out a
                    // stalled or hitching present path, short enough that a backgrounded session
                    // (video decode dropped, audio still playing) stops steering almost at once.
                    av.observe(AvSync.Observation(
                        ptsNs: pcm.ptsNs, nowLocalNs: nowNs,
                        clockOffsetNs: connection.clockOffsetNs, bufferedAhead: depth,
                        videoE2eNs: videoLatency.latestSample(asOfNs: nowNs, maxAgeMs: 500)))
                    ring.setSyncTarget(av.desiredDepth(currentDepth: depth))
                    ring.noteAvOffset(av.offsetMS)
                }
                pcm.samples.withUnsafeBufferPointer { p in
                    if let base = p.baseAddress {
                        ring.write(base, count: pcm.frameCount * pcm.channels)
                    }
                }
                // Periodic vitals (~10 s at the protocol's 5 ms frames). The other three clients
                // log buffer depth and underruns; without this an Apple audio report — latency or
                // dropout — arrives with no numbers at all, which is the position every platform
                // was in before the 2026-08 audio work.
                drained += 1
                if drained % 2_000 == 0 {
                    let s = ring.stats
                    log.info(
                        "audio: buffer_ms=\(s.bufferedMS) target_ms=\(s.targetMS) underruns=\(s.underruns) drift_sheds=\(s.sheds) av_offset_ms=\(s.avOffsetMS)"
                    )
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
    /// One engine, both directions: engage the system voice processor on the shared IO unit
    /// (AEC + noise suppression + AGC), hang the playback source off its render side and the
    /// mic tap off its capture side. Every failure falls back to a WORKING configuration —
    /// the split path (no AEC) when the voice processor won't engage, plain playback when the
    /// mic chain can't be built — a session never loses audio to the echo-cancel feature.
    private func startCombined(speakerUID: String, micUID: String, micChannel: Int) {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        do {
            // Before anything reads the input's format: the voice processor changes it (often
            // to its own mono mix, sometimes at a lower rate) — installMicTap reads the format
            // AFTER this, so the converter chain adapts to whatever the processor emits.
            try input.setVoiceProcessingEnabled(true)
        } catch {
            log.warning("""
                voice processing unavailable (\(error.localizedDescription)) — separate \
                engines, no echo cancellation
                """)
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        // Symmetric enable for the render side; with both directions on one engine the
        // input-node enable already covers it, so a refusal here is not a failure.
        try? engine.outputNode.setVoiceProcessingEnabled(true)
        // This is a game stream, not a call: never duck the host's audio under the outgoing
        // voice. .min is the closest to "off" the API offers, and advanced (selective)
        // ducking stays off with it.
        input.voiceProcessingOtherAudioDuckingConfiguration = .init(
            enableAdvancedDucking: false, duckingLevel: .min)

        guard let (ring, source, format) = makePlaybackChain() else {
            // Playback impossible (logged) — keep the uplink alive, as the split path would.
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)

        // The capture side must be PULLED, and only the render graph pulls anything. An input
        // node carrying nothing but a tap is not part of that graph, so on the combined engine
        // nobody drove it: the IO unit came up (the recording indicator lit for a beat, then went
        // out as the input went idle) and NOT ONE BUFFER ever reached the tap — no error, no
        // failed start, just a session that quietly sent no microphone at all. Routing the input
        // through a silent sink puts it in the graph, which is what Apple's own voice-processing
        // sample does. The split path never needed it: a capture-only engine has the input node
        // AS its graph, so it is pulled by definition — which is why this only broke when the
        // combined topology became the default.
        //
        // `outputVolume = 0` on the sink: the mic has to reach the graph, never the speaker. At
        // any audible volume this is a microphone wired straight to the earpiece.
        let micSink = AVAudioMixerNode()
        engine.attach(micSink)
        micSink.outputVolume = 0
        engine.connect(engine.inputNode, to: micSink, format: nil)
        engine.connect(micSink, to: engine.mainMixerNode, format: nil)

        // BEFORE the tap reads a format. Enabling voice processing swaps the engine's IO unit
        // for the VPIO one and renegotiates its formats, and until the engine is prepared the
        // input node can still report the pre-swap state — 0 Hz / 0 channels included, which
        // `installMicTap` (correctly) refuses as "no usable input device". Preparing first means
        // the chain is built against what the voice processor will actually emit.
        engine.prepare()
        guard installMicTap(on: engine.inputNode, micUID: micUID, micChannel: micChannel) else {
            // Mic chain unavailable on the VOICE-PROCESSED engine (logged). The mic outranks the
            // echo cancellation, so fall back to the split path — its own engine, no voice
            // processor, the topology that shipped before AEC existed — rather than dropping the
            // uplink for the rest of the session. (The sibling failure above, where the voice
            // processor won't engage at all, already does exactly this; this arm used to give up
            // on the mic instead, which is how a whole session could go silent uplink-only.)
            engine.stop()
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        do {
            try engine.start()
        } catch {
            log.error("combined engine failed to start: \(error.localizedDescription)")
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            // Same rule: a working mic without echo cancellation beats no mic at all.
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        stateLock.lock()
        if flag.isStopped {
            stateLock.unlock()
            input.removeTap(onBus: 0)
            engine.stop() // stop() already ran — don't strand a started engine (or a hot mic)
            return
        }
        combinedEngine = engine
        let muted = micMuted // latched before this engine existed (a mute during the prompt)
        stateLock.unlock()
        apply(micMuted: muted, capture: nil, combined: engine)
        startDrain(into: ring)
        log.info("audio engines joined — voice processing (echo cancellation) active")
    }

    /// The split path: capture on its OWN engine, playback on another — the pre-echo-cancel
    /// topology, kept verbatim. Two engines, not one — a single AVAudioEngine ties
    /// input+output to one aggregate clock, separate engines keep arbitrary mic/speaker
    /// combinations trivial. That freedom is exactly why the voice processor can't ride this
    /// path (AEC needs both directions on one unit) and why explicitly pinned endpoints land
    /// here — see `wantsCombined`.
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
        // Prepared before the tap reads a format, for the same reason the combined path does it:
        // a node that hasn't been through `prepare()` can still report the pre-negotiation
        // format (0 Hz / 0 channels on a device that is perfectly fine), which reads downstream
        // as "no microphone".
        engine.prepare()
        guard installMicTap(on: engine.inputNode, micUID: micUID, micChannel: micChannel) else {
            log.error("mic uplink unavailable — this session sends no microphone audio")
            engine.stop()
            return
        }
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
        let muted = micMuted // latched before this engine existed (a mute during the prompt)
        stateLock.unlock()
        apply(micMuted: muted, capture: engine, combined: nil)
        log.info("mic uplink started (\(micUID.isEmpty ? "default input" : micUID))")
    }

    /// Resolve the input's live format + fold plan, build the mono→Opus chain, and install the
    /// capture tap on `input` — everything mic except engine ownership, shared verbatim by the
    /// combined and split topologies. Reads `input.outputFormat(forBus:)` at call time, so the
    /// chain follows whatever the node emits: the raw device format, or the voice processor's
    /// own mix when that's enabled. False (logged) when no input is usable or the encoder
    /// can't be built; the tap is installed on true.
    private func installMicTap(
        on input: AVAudioInputNode, micUID: String, micChannel: Int
    ) -> Bool {
        let inFormat = input.outputFormat(forBus: 0)
        guard inFormat.sampleRate > 0, inFormat.channelCount > 0 else {
            log.error("no usable input device — mic uplink disabled")
            return false
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
            return false
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
        return true
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
