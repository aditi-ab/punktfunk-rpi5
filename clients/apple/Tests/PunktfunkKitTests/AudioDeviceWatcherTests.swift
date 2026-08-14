// The trigger half of surviving a device change: does the session actually get TOLD?
//
// An AVAudioEngine stops itself when its output hardware changes and never restarts on its own, so
// everything downstream of these notifications is dead code if the notification never arrives. The
// rebuild itself needs a live session to exercise (and so a host, which does not build on macOS),
// but the wiring does not — and the wiring is where a silent failure costs a session all of its
// audio, which is exactly the shape of the bug this watcher exists to fix.

import AVFoundation
import XCTest
#if os(macOS)
import CoreAudio
#endif

@testable import PunktfunkKit

final class AudioDeviceWatcherTests: XCTestCase {
    /// The callbacks land on the main queue, so a test that slept would block the thing it waits
    /// for. Pumps until `predicate` holds or the deadline passes.
    private func pump(until predicate: () -> Bool, timeout: TimeInterval = 2) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if predicate() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.02))
        }
        return predicate()
    }

    /// The identity gate is the one line that could swallow every notification silently: get it
    /// wrong and the recovery compiles, installs, runs — and never fires.
    func testAConfigurationChangeFromOurEngineReachesTheOwner() {
        let engine = AVAudioEngine()
        var reasons: [AudioDeviceWatcher.Reason] = []
        let watcher = AudioDeviceWatcher(
            isOurs: { $0 === engine }, onChange: { reason, _ in reasons.append(reason) })
        watcher.start()
        defer { watcher.stop() }

        NotificationCenter.default.post(
            name: .AVAudioEngineConfigurationChange, object: engine)

        XCTAssertTrue(
            pump(until: { reasons.contains(.engineConfiguration) }),
            "the session was never told its engine's configuration changed")
    }

    /// A retired engine posts one last change as it is torn down, and other AVAudioEngines in the
    /// process are not ours to restart — rebuilding for either would interrupt healthy playback.
    func testAConfigurationChangeFromAForeignEngineIsIgnored() {
        let ours = AVAudioEngine()
        let stranger = AVAudioEngine()
        var reasons: [AudioDeviceWatcher.Reason] = []
        let watcher = AudioDeviceWatcher(
            isOurs: { $0 === ours }, onChange: { reason, _ in reasons.append(reason) })
        watcher.start()
        defer { watcher.stop() }

        NotificationCenter.default.post(
            name: .AVAudioEngineConfigurationChange, object: stranger)
        // Give it the same grace the positive case gets, then require silence.
        _ = pump(until: { !reasons.isEmpty }, timeout: 0.5)
        XCTAssertTrue(reasons.isEmpty, "a foreign engine's change was taken for ours")
    }

    func testStopSilencesTheWatcher() {
        let engine = AVAudioEngine()
        var reasons: [AudioDeviceWatcher.Reason] = []
        let watcher = AudioDeviceWatcher(
            isOurs: { $0 === engine }, onChange: { reason, _ in reasons.append(reason) })
        watcher.start()
        watcher.stop()

        NotificationCenter.default.post(
            name: .AVAudioEngineConfigurationChange, object: engine)
        _ = pump(until: { !reasons.isEmpty }, timeout: 0.5)
        XCTAssertTrue(reasons.isEmpty, "a stopped watcher still reported")
    }

    #if os(macOS)
    /// The backstop, against the real HAL: move the system's default output device — the thing that
    /// happens when AirPods come out of an ear — and require that the session hears about it. This
    /// is the trigger the recovery leans on for the voice-processing engine, whose own notification
    /// behaviour cannot be verified here (no Mac in this project's fleet can initialize VPIO).
    func testTheDefaultOutputDeviceMovingReachesTheOwner() throws {
        guard let original = AudioDevices.defaultOutputDevice() else {
            throw XCTSkip("no default output device")
        }
        let others = AudioDevices.outputs()
            .compactMap { AudioDevices.deviceID(forUID: $0.uid) }
            .filter { $0 != original }
        guard let target = others.first else {
            throw XCTSkip("needs a second output device to switch to")
        }

        var reasons: [AudioDeviceWatcher.Reason] = []
        let watcher = AudioDeviceWatcher(isOurs: { _ in false }, onChange: { reason, _ in reasons.append(reason) })
        watcher.start()
        defer {
            _ = Self.setDefaultOutput(original)
            watcher.stop()
        }

        XCTAssertEqual(Self.setDefaultOutput(target), noErr)
        XCTAssertTrue(
            pump(until: { reasons.contains(.defaultOutputDevice) }, timeout: 5),
            "the session was never told the default output device moved")
    }

    /// Test-local on purpose: nothing in the app ever changes the user's device, it only follows it.
    private static func setDefaultOutput(_ id: AudioDeviceID) -> OSStatus {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var dev = id
        return AudioObjectSetPropertyData(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil,
            UInt32(MemoryLayout<AudioDeviceID>.size), &dev)
    }
    #endif
}
