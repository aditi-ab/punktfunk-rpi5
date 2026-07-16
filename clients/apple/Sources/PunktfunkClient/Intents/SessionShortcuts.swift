// Siri / Shortcuts / Spotlight surface (design §M4). Deliberately thin: every action already has an
// internal entry point — M0's deep-link router (connect / connect-and-launch), M3's in-process
// end-session hook, and the existing Wake-on-LAN path — so these intents only wrap them.
//
// Gated os(iOS): the AppShortcutsProvider bundles `EndStreamIntent`, which is a LiveActivityIntent
// (iPhone/iPad only). Connect/Wake themselves are plain AppIntents; they live here with the
// provider rather than being split across platforms. `HostEntity` (the parameter type) is in
// PunktfunkShared so the widget's configuration intent can share it.

#if os(iOS)
import AppIntents
import Foundation
import PunktfunkKit

/// Load a full saved host (MACs, address) from the shared App-Group store by id — HostEntity only
/// carries id + name.
private func loadStoredHost(_ id: UUID) -> StoredHost? {
    guard let data = AppGroup.defaults.data(forKey: DefaultsKey.hosts),
          let hosts = try? JSONDecoder().decode([StoredHost].self, from: data)
    else { return nil }
    return hosts.first { $0.id == id }
}

/// Start a session with a stored host (optionally launching a title). Foregrounds the app and
/// routes through the SAME `.onOpenURL` path a widget tap uses — trust policy, WoL and the approval
/// sheet all apply, and its guards (unknown host, already-streaming) hold.
struct ConnectToHostIntent: AppIntent {
    static let title: LocalizedStringResource = "Connect to Host"
    static let description = IntentDescription("Start a Punktfunk streaming session with a host.")
    static let openAppWhenRun = true

    @Parameter(title: "Host") var host: HostEntity
    @Parameter(title: "Game ID", description: "Optional store id like steam:570")
    var launchID: String?

    func perform() async throws -> some IntentResult {
        let url = DeepLink.connect(host: host.id, launchID: launchID).url
        await MainActor.run {
            NotificationCenter.default.post(name: .punktfunkOpenDeepLink, object: url)
        }
        return .result()
    }
}

/// Wake a sleeping host (magic packet). No `openAppWhenRun` — usable in automations ("when I get
/// home, wake the tower") without foregrounding the app.
struct WakeHostIntent: AppIntent {
    static let title: LocalizedStringResource = "Wake Host"
    static let description = IntentDescription("Send a Wake-on-LAN magic packet to a host.")

    @Parameter(title: "Host") var host: HostEntity

    func perform() async throws -> some IntentResult {
        guard let stored = loadStoredHost(host.id), !stored.wakeMacs.isEmpty else {
            throw IntentError.noWakeAddress
        }
        PunktfunkConnection.wakeOnLAN(macs: stored.wakeMacs, lastKnownIP: stored.address)
        return .result()
    }
}

/// Errors surfaced to Siri/Shortcuts. `CustomLocalizedStringResourceConvertible` makes the message
/// show as the intent's failure text.
enum IntentError: Error, CustomLocalizedStringResourceConvertible {
    case noWakeAddress

    var localizedStringResource: LocalizedStringResource {
        switch self {
        case .noWakeAddress:
            // One string LITERAL — LocalizedStringResource is ExpressibleByStringLiteral, but a
            // `"…" + "…"` concatenation is a runtime String it can't convert.
            return "That host has no saved Wake-on-LAN address yet. Connect to it once so Punktfunk can learn it."
        }
    }
}

/// Zero-setup Siri / Spotlight phrases. Parameterized phrases resolve a `HostEntity` by name; stays
/// well under the 10-shortcut cap.
struct PunktfunkShortcuts: AppShortcutsProvider {
    static var appShortcuts: [AppShortcut] {
        AppShortcut(
            intent: ConnectToHostIntent(),
            phrases: [
                "Connect to \(\.$host) in \(.applicationName)",
                "Stream \(\.$host) with \(.applicationName)",
            ],
            shortTitle: "Connect", systemImageName: "play.tv.fill")
        AppShortcut(
            intent: WakeHostIntent(),
            phrases: [
                "Wake \(\.$host) with \(.applicationName)",
            ],
            shortTitle: "Wake Host", systemImageName: "power")
        AppShortcut(
            intent: EndStreamIntent(),
            phrases: [
                "End the \(.applicationName) stream",
            ],
            shortTitle: "End Stream", systemImageName: "stop.fill")
    }
}
#endif
