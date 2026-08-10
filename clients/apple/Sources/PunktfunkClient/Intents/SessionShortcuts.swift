// Siri / Shortcuts / Spotlight surface (design §M4, extended by client-deep-links.md §6).
// Deliberately thin: every action already has an internal entry point — the deep-link router
// (connect / connect-and-launch / connect-with-a-profile, and the `browse` route into a host's
// library), the in-process end-session hook, and the existing Wake-on-LAN path — so these
// intents only wrap them.
//
// Connect and Wake compile on macOS and tvOS too: AppIntents is genuinely available there
// (macOS 13+ / tvOS 16+), and "Stream Desktop with Work" from Spotlight on a Mac is part of the
// ask. Only the AppShortcutsProvider stays iOS-gated, because it bundles `EndStreamIntent` — a
// LiveActivityIntent, and ActivityKit is iPhone/iPad only.
//
// `HostEntity` and `ProfileEntity` (the parameter types) live in PunktfunkShared so a widget's
// configuration intent, which executes in the EXTENSION process, can share them.

#if canImport(AppIntents)
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

/// Start a session with a stored host (optionally launching a title, optionally with a settings
/// profile). Foregrounds the app and routes through the SAME `.onOpenURL` path a widget tap uses —
/// trust policy, WoL and the approval sheet all apply, and its guards (unknown host, ambiguous or
/// unknown profile, already-streaming) hold. One router, not a second connect path.
struct ConnectToHostIntent: AppIntent {
    static let title: LocalizedStringResource = "Connect to Host"
    static let description = IntentDescription("Start a Punktfunk streaming session with a host.")
    static let openAppWhenRun = true

    @Parameter(title: "Host") var host: HostEntity
    @Parameter(title: "Game ID", description: "Optional store id like steam:570")
    var launchID: String?
    @Parameter(
        title: "Profile",
        description: "Which settings profile to use — leave empty for the host's default")
    var profile: ProfileEntity?

    func perform() async throws -> some IntentResult {
        let url = DeepLink.connect(host: host.id, launchID: launchID, profile: profile?.id).url
        await MainActor.run {
            NotificationCenter.default.post(name: .punktfunkOpenDeepLink, object: url)
        }
        return .result()
    }
}

/// Jump straight into a host's game library — no session. Foregrounds the app and routes the
/// `browse` route through the same `.onOpenURL` path a widget tap uses, which drives the one
/// `libraryTarget` every surface shares — so the shortcut lands in whichever library presentation
/// the current mode owns: the gamepad console's library screen when the gamepad UI is active, the
/// touch/desktop library otherwise. A session starts only when a title is picked there.
struct OpenLibraryIntent: AppIntent {
    static let title: LocalizedStringResource = "Open Game Library"
    static let description = IntentDescription(
        "Open a host's game library in Punktfunk, without starting a stream.")
    static let openAppWhenRun = true

    @Parameter(title: "Host") var host: HostEntity

    func perform() async throws -> some IntentResult {
        let url = DeepLink.browse(host: host.id).url
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

#if os(iOS)
/// Zero-setup Siri / Spotlight phrases. Parameterized phrases resolve a `HostEntity` by name; stays
/// well under the 10-shortcut cap. Phrases stay host-parameterized — App Shortcut phrases can't
/// embed a second arbitrary `AppEntity`, so the profile is picked in the Shortcuts editor. No
/// phrase regression.
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
            intent: OpenLibraryIntent(),
            phrases: [
                "Open \(\.$host) library in \(.applicationName)",
                "Show \(\.$host) games in \(.applicationName)",
            ],
            shortTitle: "Game Library", systemImageName: "square.grid.2x2.fill")
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
#endif
