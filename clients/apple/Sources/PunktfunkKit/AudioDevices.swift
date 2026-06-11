// CoreAudio HAL device enumeration for the Settings pickers. Devices are persisted by
// UID (stable across reboots/replugs — AudioDeviceIDs are not); the empty UID means
// "system default", which additionally tracks default-device changes because we then
// never pin the engine to a concrete device.

#if os(macOS)
import CoreAudio
import Foundation

public struct AudioDevice: Hashable, Identifiable, Sendable {
    public let uid: String
    public let name: String
    public var id: String { uid }
}

public enum AudioDevices {
    /// Output-capable devices (speakers, headphones, multi-output…).
    public static func outputs() -> [AudioDevice] {
        all().filter { hasStreams($0, scope: kAudioObjectPropertyScopeOutput) }
            .compactMap(describe)
    }

    /// Input-capable devices (microphones, interfaces…).
    public static func inputs() -> [AudioDevice] {
        all().filter { hasStreams($0, scope: kAudioObjectPropertyScopeInput) }
            .compactMap(describe)
    }

    /// Resolve a persisted UID to the current AudioDeviceID — nil when unplugged.
    static func deviceID(forUID uid: String) -> AudioDeviceID? {
        all().first { id in
            stringProperty(id, kAudioDevicePropertyDeviceUID) == uid
        }
    }

    private static func all() -> [AudioDeviceID] {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size) == noErr,
            size > 0
        else { return [] }
        var ids = [AudioDeviceID](
            repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
        guard AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &ids) == noErr
        else { return [] }
        return ids
    }

    private static func hasStreams(
        _ id: AudioDeviceID, scope: AudioObjectPropertyScope
    ) -> Bool {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreams,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        return AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr && size > 0
    }

    private static func describe(_ id: AudioDeviceID) -> AudioDevice? {
        guard let uid = stringProperty(id, kAudioDevicePropertyDeviceUID),
              let name = stringProperty(id, kAudioObjectPropertyName)
        else { return nil }
        return AudioDevice(uid: uid, name: name)
    }

    private static func stringProperty(
        _ id: AudioDeviceID, _ selector: AudioObjectPropertySelector
    ) -> String? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var ref: CFString?
        var size = UInt32(MemoryLayout<CFString?>.size)
        let status = withUnsafeMutablePointer(to: &ref) { p in
            AudioObjectGetPropertyData(id, &address, 0, nil, &size, p)
        }
        guard status == noErr, let ref else { return nil }
        return ref as String
    }
}
#endif
