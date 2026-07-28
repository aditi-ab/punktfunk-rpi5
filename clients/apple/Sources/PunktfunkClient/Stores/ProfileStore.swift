// The settings-profile catalog as an observable store — the app-side wrapper around
// `ProfileCatalog` (design/client-settings-profiles.md §4.2), matching what `HostStore` is to
// `[StoredHost]`.
//
// The catalog lives in the App Group suite beside the saved hosts, because that is where the
// things that POINT at it live: a binding is `StoredHost.profileID` and a pin is an entry in
// `StoredHost.pinnedProfileIDs`. Nothing here is keyed by host — "Work" applied to three hosts is
// one profile, and the per-host part is only the binding (§4.1).

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
final class ProfileStore: ObservableObject {
    /// One catalog for the whole app. Unlike `HostStore` (which ContentView owns and hands down),
    /// the settings surface reaches this from a SEPARATE macOS `Settings` scene, where no parent
    /// can pass it — and two instances would mean editing a profile in Preferences while the host
    /// grid still shows the old one. Same shape as `GamepadManager.shared`.
    static let shared = ProfileStore()

    @Published private(set) var catalog: ProfileCatalog {
        didSet { catalog.save() }
    }

    var profiles: [StreamProfile] { catalog.profiles }

    init(catalog: ProfileCatalog? = nil) {
        self.catalog = catalog ?? ProfileCatalog.load()
    }

    func profile(id: String?) -> StreamProfile? {
        id.flatMap { catalog.profile(id: $0) }
    }

    /// This host's default profile, dangling ids dropped — a deleted profile resolves as "Default
    /// settings", never an error (§4.4).
    func binding(for host: StoredHost) -> StreamProfile? { catalog.binding(for: host) }

    /// This host's pinned profiles in card order, duplicates and dangling ids dropped.
    func pinned(for host: StoredHost) -> [StreamProfile] { catalog.pinned(for: host) }

    func nameTaken(_ name: String, except: String? = nil) -> Bool {
        catalog.nameTaken(name, except: except)
    }

    // MARK: - Catalog management (the scope menu's Rename / Duplicate / Delete)

    /// Create an EMPTY profile — it inherits everything, which is the right creation default under
    /// inherit-by-exception. "Duplicate" covers starting from another profile.
    @discardableResult
    func create(name: String) -> StreamProfile {
        let profile = StreamProfile(name: name)
        catalog.profiles.append(profile)
        return profile
    }

    @discardableResult
    func duplicate(_ id: String, name: String) -> StreamProfile? {
        guard let source = catalog.profile(id: id) else { return nil }
        var copy = source
        copy.id = newProfileID()
        copy.name = name
        catalog.profiles.append(copy)
        return copy
    }

    func rename(_ id: String, to name: String) {
        guard let i = catalog.profiles.firstIndex(where: { $0.id == id }) else { return }
        catalog.profiles[i].name = name
    }

    func setAccent(_ id: String, to accent: String?) {
        guard let i = catalog.profiles.firstIndex(where: { $0.id == id }) else { return }
        catalog.profiles[i].accent = accent
    }

    /// Delete a profile. Bindings and pins pointing at it are left alone deliberately: they
    /// degrade to "Default settings" / a dropped card at read time (§6), so a delete never has to
    /// walk the host store — and a host record saved by an older build can't resurrect a stale id.
    func delete(_ id: String) {
        catalog.profiles.removeAll { $0.id == id }
    }

    /// How the delete warning counts what it is about to change: hosts bound to this profile and
    /// pinned cards that will disappear.
    func usage(of id: String) -> (bound: Int, pinned: Int) {
        let hosts = Self.savedHosts()
        return (
            hosts.filter { $0.profileID == id }.count,
            hosts.filter { ($0.pinnedProfileIDs ?? []).contains(id) }.count
        )
    }

    /// Saved hosts straight from the shared store. The settings surface owns no `HostStore` — it
    /// only needs to COUNT what a delete is about to change, and reading the same App-Group blob
    /// the widget reads beats threading a store through a separate macOS Settings scene.
    static func savedHosts() -> [StoredHost] {
        guard let data = AppGroup.defaults.data(forKey: DefaultsKey.hosts),
              let hosts = try? JSONDecoder().decode([StoredHost].self, from: data)
        else { return [] }
        return hosts
    }

    // MARK: - Overrides

    /// Record an override, always by explicit write — never by comparing the new value against
    /// today's global. A value that happens to equal the global is a legitimate PIN: the profile
    /// keeps it when the global later moves, and that is the whole difference between this feature
    /// and "copy the settings" (§4.1).
    func setOverride<Value>(
        _ id: String, _ keyPath: WritableKeyPath<SettingsOverlay, Value?>, _ value: Value
    ) {
        guard let i = catalog.profiles.firstIndex(where: { $0.id == id }) else { return }
        catalog.profiles[i].overrides[keyPath: keyPath] = value
    }

    /// The only way back to inheriting: an explicit per-row reset. `field` is the overlay's own
    /// serialized name, with `resolution` covering the width/height/match-window tri-state.
    func clearOverride(_ id: String, field: String) {
        guard let i = catalog.profiles.firstIndex(where: { $0.id == id }) else { return }
        OverlayField.clear(field, in: &catalog.profiles[i].overrides)
    }
}
