// A settings profile as an App Intents entity — the parameter type for "Stream <host> with
// <profile>" in Shortcuts (design/client-deep-links.md §6). Lives beside `HostEntity` in the
// shared module for the same reason: an intent (and, later, a configurable widget) executes
// outside the app, so the entity and its query must not need PunktfunkKit.
//
// The query reads the App-Group catalog blob directly — no Rust core, no app process — exactly
// like `HostEntityQuery` reads the saved hosts.

#if canImport(AppIntents)
import AppIntents
import Foundation

public struct ProfileEntity: AppEntity, Identifiable {
    public static let typeDisplayRepresentation = TypeDisplayRepresentation(name: "Settings Profile")
    public static let defaultQuery = ProfileEntityQuery()

    /// The catalog id — stable across renames, which is why a shortcut keeps working after the
    /// user renames the profile it points at.
    public let id: String
    public let name: String
    /// `#RRGGBB`, when the profile has been given one. Carried so a future display representation
    /// (and the configurable widget) can tint without a second lookup.
    public let accent: String?

    public init(id: String, name: String, accent: String? = nil) {
        self.id = id
        self.name = name
        self.accent = accent
    }

    public init(_ profile: StreamProfile) {
        self.init(id: profile.id, name: profile.name, accent: profile.accent)
    }

    public var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }
}

public struct ProfileEntityQuery: EntityQuery {
    public init() {}

    public func entities(for identifiers: [String]) async throws -> [ProfileEntity] {
        ProfileCatalog.load().profiles
            .filter { identifiers.contains($0.id) }
            .map(ProfileEntity.init)
    }

    public func suggestedEntities() async throws -> [ProfileEntity] {
        ProfileCatalog.load().profiles.map(ProfileEntity.init)
    }
}
#endif
