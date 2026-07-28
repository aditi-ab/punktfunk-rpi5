// The settings surface edits SETTINGS PROFILES too (design/client-settings-profiles.md §5.1): a
// scope switcher swaps the whole screen between the global defaults and one profile's overrides.
//
// It is deliberately not a second editor. A parallel profile editor would drift from this one
// field by field, and the caption/curation work of the 2026-07 revamp would have to be done twice.
// So the section builders stay the single definition of every row and simply change which LAYER
// their binding reads and writes.
//
// Three rules make it a profile editor rather than a settings copier:
//   • every row shows the EFFECTIVE value — the inherited global until this profile overrides it;
//   • touching a control records the override, always, even when the new value happens to equal
//     today's global (that is a PIN: it keeps its value when the global later moves). Overrides
//     are never inferred by diffing at save time;
//   • the only way back to inheriting is an explicit per-row reset — which is why the marker and
//     its Reset action below are not optional garnish: without them a profile is a one-way door.
//
// Tier-G/H rows (this device's endpoints and hardware, properties of a host) simply don't render
// in profile scope — see §3's curation and `isProfileable` at each call site.

import PunktfunkKit
import SwiftUI

/// Which layer the settings surface is editing.
enum SettingsScope: Equatable, Hashable {
    /// The global defaults every profile inherits from — the only scope before this feature.
    case defaults
    /// One profile's overrides, by id.
    case profile(String)

    var profileID: String? {
        if case .profile(let id) = self { return id }
        return nil
    }
}

/// One profileable setting, in the one place that knows all three of its faces: the
/// `UserDefaults` key its global lives under, the overlay slot an override lives in, and the
/// serialized name a per-row reset carries. Keeping them together is what stops a row from
/// writing an override the reset button can't find.
struct SettingsField<Value> {
    /// The overlay's serialized field name — also the reset key. `resolution` is the one alias,
    /// covering the width/height/match-window tri-state a single control drives.
    let name: String
    let key: String
    let overlay: WritableKeyPath<SettingsOverlay, Value?>
    let effective: KeyPath<EffectiveSettings, Value>
}

/// The catalog of profileable rows. A plain namespace rather than statics on the generic
/// `SettingsField` itself: members of a generic type can't have their parameter inferred from the
/// member alone, so every call site would need to spell the type out anyway.
enum SettingsFields {
    static var refreshHz: SettingsField<Int> {
        .init(name: "refresh_hz", key: DefaultsKey.streamHz,
              overlay: \.refreshHz, effective: \.refreshHz)
    }
    static var matchWindow: SettingsField<Bool> {
        // Part of the resolution tri-state, so its RESET name is the shared alias.
        .init(name: OverlayField.resolution, key: DefaultsKey.matchWindow,
              overlay: \.matchWindow, effective: \.matchWindow)
    }
    static var width: SettingsField<Int> {
        .init(name: OverlayField.resolution, key: DefaultsKey.streamWidth,
              overlay: \.width, effective: \.width)
    }
    static var height: SettingsField<Int> {
        .init(name: OverlayField.resolution, key: DefaultsKey.streamHeight,
              overlay: \.height, effective: \.height)
    }
    static var renderScale: SettingsField<Double> {
        .init(name: "render_scale", key: DefaultsKey.renderScale,
              overlay: \.renderScale, effective: \.renderScale)
    }
    static var bitrateKbps: SettingsField<Int> {
        .init(name: "bitrate_kbps", key: DefaultsKey.bitrateKbps,
              overlay: \.bitrateKbps, effective: \.bitrateKbps)
    }
    static var codec: SettingsField<String> {
        .init(name: "codec", key: DefaultsKey.codec, overlay: \.codec, effective: \.codec)
    }
    static var hdrEnabled: SettingsField<Bool> {
        .init(name: "hdr_enabled", key: DefaultsKey.hdrEnabled,
              overlay: \.hdrEnabled, effective: \.hdrEnabled)
    }
    static var enable444: SettingsField<Bool> {
        .init(name: "enable_444", key: DefaultsKey.enable444,
              overlay: \.enable444, effective: \.enable444)
    }
    static var compositor: SettingsField<Int> {
        .init(name: "compositor", key: DefaultsKey.compositor,
              overlay: \.compositor, effective: \.compositor)
    }
    static var audioChannels: SettingsField<Int> {
        .init(name: "audio_channels", key: DefaultsKey.audioChannels,
              overlay: \.audioChannels, effective: \.audioChannels)
    }
    static var micEnabled: SettingsField<Bool> {
        .init(name: "mic_enabled", key: DefaultsKey.micEnabled,
              overlay: \.micEnabled, effective: \.micEnabled)
    }
    static var touchMode: SettingsField<String> {
        .init(name: "touch_mode", key: DefaultsKey.touchMode,
              overlay: \.touchMode, effective: \.touchMode)
    }
    static var mouseMode: SettingsField<String> {
        .init(name: "mouse_mode", key: DefaultsKey.mouseMode,
              overlay: \.mouseMode, effective: \.mouseMode)
    }
    static var invertScroll: SettingsField<Bool> {
        .init(name: "invert_scroll", key: DefaultsKey.invertScroll,
              overlay: \.invertScroll, effective: \.invertScroll)
    }
    static var modifierLayout: SettingsField<String> {
        .init(name: "modifier_layout", key: DefaultsKey.modifierLayout,
              overlay: \.modifierLayout, effective: \.modifierLayout)
    }
    static var gamepadType: SettingsField<Int> {
        .init(name: "gamepad", key: DefaultsKey.gamepadType,
              overlay: \.gamepadType, effective: \.gamepadType)
    }
    static var statsVerbosity: SettingsField<String> {
        .init(name: "stats_verbosity", key: DefaultsKey.statsVerbosity,
              overlay: \.statsVerbosity, effective: \.statsVerbosity)
    }
    static var fullscreenWhileStreaming: SettingsField<Bool> {
        .init(name: "fullscreen_on_stream", key: DefaultsKey.fullscreenWhileStreaming,
              overlay: \.fullscreenWhileStreaming, effective: \.fullscreenWhileStreaming)
    }
    static var presentPriority: SettingsField<String> {
        .init(name: "present_priority", key: DefaultsKey.presentPriority,
              overlay: \.presentPriority, effective: \.presentPriority)
    }
    static var smoothBuffer: SettingsField<Int> {
        .init(name: "smooth_buffer", key: DefaultsKey.smoothBuffer,
              overlay: \.smoothBuffer, effective: \.smoothBuffer)
    }
    static var vsync: SettingsField<Bool> {
        .init(name: "vsync", key: DefaultsKey.vsync, overlay: \.vsync, effective: \.vsync)
    }
    static var allowVRR: SettingsField<Bool> {
        .init(name: "allow_vrr", key: DefaultsKey.allowVRR,
              overlay: \.allowVRR, effective: \.allowVRR)
    }
    static var windowedSafePresent: SettingsField<Bool> {
        .init(name: "windowed_safe_present", key: DefaultsKey.windowedSafePresent,
              overlay: \.windowedSafePresent, effective: \.windowedSafePresent)
    }
}

extension SettingsView {
    // MARK: - The layer being edited

    var activeProfile: StreamProfile? {
        scope.profileID.flatMap { profiles.profile(id: $0) }
    }

    /// True while a profile is being edited — the gate every tier-G/H row is hidden behind.
    var inProfileScope: Bool { activeProfile != nil }

    /// What every row displays: the globals as this view's `@AppStorage` sees them (so SwiftUI
    /// tracks each of them), with the edited profile's overrides on top. A row the profile doesn't
    /// override therefore reads as the LIVE global, which is what inherit-by-default has to look
    /// like.
    var effective: EffectiveSettings {
        var base = EffectiveSettings()
        base.width = width
        base.height = height
        base.refreshHz = hz
        base.matchWindow = matchWindow
        base.bitrateKbps = bitrateKbps
        base.renderScale = renderScale
        base.codec = codec
        base.hdrEnabled = hdrEnabled
        base.enable444 = enable444
        base.compositor = compositor
        base.audioChannels = audioChannels
        base.micEnabled = micEnabled
        base.gamepadType = gamepadType
        base.statsVerbosity = statsVerbosityRaw
        base.fullscreenWhileStreaming = fullscreenWhileStreaming
        base.presentPriority = presentPriority
        base.smoothBuffer = smoothBuffer
        #if !os(tvOS)
        base.invertScroll = invertScroll
        base.modifierLayout = modifierLayout
        base.allowVRR = allowVRR
        #endif
        #if os(macOS)
        base.mouseMode = mouseMode
        base.vsync = vsync
        base.windowedSafePresent = windowedSafePresent
        #endif
        #if os(iOS)
        base.touchMode = touchMode
        #endif
        guard let profile = activeProfile else { return base }
        return base.applying(profile.overrides)
    }

    // MARK: - Scoped bindings

    /// A control's binding for whichever layer is being edited: `UserDefaults` in defaults scope,
    /// the profile's overlay in profile scope.
    ///
    /// The setter always WRITES on touch. It never compares the new value against the global to
    /// decide whether to record an override — that comparison is exactly the "copy the settings"
    /// behaviour the design rejects, and it would silently drop a deliberate pin.
    func scoped<Value>(_ field: SettingsField<Value>) -> Binding<Value> {
        Binding(
            get: { effective[keyPath: field.effective] },
            set: { newValue in
                guard let id = scope.profileID else {
                    // @AppStorage observes this key, so the write re-renders the whole surface.
                    UserDefaults.standard.set(newValue, forKey: field.key)
                    return
                }
                profiles.setOverride(id, field.overlay, newValue)
            })
    }

    /// The resolution row's tri-state, written as one act: a single control drives width, height
    /// and (where it exists) match-window, so all of them move together and one reset clears all
    /// three.
    func setResolution(width newWidth: Int? = nil, height newHeight: Int? = nil,
                       matchWindow newMatch: Bool? = nil) {
        if let newWidth { scoped(SettingsFields.width).wrappedValue = newWidth }
        if let newHeight { scoped(SettingsFields.height).wrappedValue = newHeight }
        if let newMatch { scoped(SettingsFields.matchWindow).wrappedValue = newMatch }
    }

    // MARK: - Override markers + per-row reset

    /// Does the edited profile override this row? False in defaults scope, where there is nothing
    /// to inherit from.
    func isOverridden(_ name: String) -> Bool {
        guard let profile = activeProfile else { return false }
        return OverlayField.isOverridden(name, in: profile.overrides)
    }

    /// Put a row back to inheriting. The ONLY way an override is removed — the model never infers
    /// "not overridden" from a value comparison.
    func resetOverride(_ name: String) {
        guard let id = scope.profileID else { return }
        profiles.clearOverride(id, field: name)
    }

    /// The accent dot + "Reset" affordance a row carries while it overrides the defaults. Rendered
    /// inside `described`'s caption line, so it sits with the row it belongs to instead of in some
    /// separate list of exceptions.
    @ViewBuilder
    func overrideMarker(_ name: String) -> some View {
        if isOverridden(name) {
            HStack(spacing: 5) {
                Circle()
                    .fill(Color.brand)
                    .frame(width: 6, height: 6)
                    // The adjacent text says the state; the dot is the at-a-glance version.
                    .accessibilityHidden(true)
                Text("Overrides Default settings")
                    .font(.geist(12, .medium, relativeTo: .caption2))
                    .foregroundStyle(Color.brand)
                Button("Reset") { resetOverride(name) }
                    .buttonStyle(.plain)
                    .font(.geist(12, .medium, relativeTo: .caption2))
                    .foregroundStyle(.tint)
                    .accessibilityLabel("Reset to Default settings")
            }
        }
    }

    // MARK: - The scope switcher

    /// "Editing: Default settings ▾" plus the edited profile's own management actions. One
    /// control, at the top of the surface, so the layer being edited is never ambiguous.
    ///
    /// Absent on tvOS: controller-first surfaces honor profiles and render pinned cards, but don't
    /// EDIT them in v1 (design §5.4) — a name prompt and a nested management menu are not what a
    /// remote does well, and the pattern should prove itself on the primary surfaces first.
    @ViewBuilder
    var scopeSwitcher: some View {
        #if !os(tvOS)
        VStack(alignment: .leading, spacing: 4) {
            Menu {
                Picker("Editing", selection: scopeSelection) {
                    Text("Default settings").tag(SettingsScope.defaults)
                    ForEach(profiles.profiles) { profile in
                        Text(profile.name).tag(SettingsScope.profile(profile.id))
                    }
                }
                .pickerStyle(.inline)
                Divider()
                Button("New Profile…") {
                    nameDraft = ""
                    nameAction = .create
                }
                if let active = activeProfile {
                    Button("Rename “\(active.name)”…") {
                        nameDraft = active.name
                        nameAction = .rename(active.id)
                    }
                    Button("Duplicate “\(active.name)”") {
                        nameDraft = Self.copyName(of: active.name, in: profiles)
                        nameAction = .duplicate(active.id)
                    }
                    Button("Delete “\(active.name)”…", role: .destructive) {
                        profilePendingDelete = active
                    }
                }
            } label: {
                Label(
                    activeProfile?.name ?? "Default settings",
                    systemImage: activeProfile == nil ? "gearshape" : "slider.horizontal.3")
            }
            .menuStyle(.button)
            .fixedSize()
            Text(activeProfile == nil
                ? "The settings every profile inherits from."
                : "Overrides Default settings for hosts that use this profile. "
                    + "Untouched rows keep following the defaults.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        // Name prompts (create / rename / duplicate) share one alert: they differ only in the
        // title and what happens with the name.
        .alert(nameAction?.title ?? "", isPresented: namePromptPresented) {
            TextField("Name", text: $nameDraft)
            Button("Cancel", role: .cancel) { nameAction = nil }
            Button(nameAction?.accept ?? "OK") { commitNamePrompt() }
                .disabled(!isNameAcceptable)
        } message: {
            Text(nameMessage)
        }
        // Deleting warns with what it will change — bindings fall back to Default settings and
        // pinned cards disappear (§6). Neither is an error, but neither should be a surprise.
        .alert(
            "Delete “\(profilePendingDelete?.name ?? "")”?",
            isPresented: Binding(
                get: { profilePendingDelete != nil },
                set: { if !$0 { profilePendingDelete = nil } }),
            presenting: profilePendingDelete
        ) { profile in
            Button("Cancel", role: .cancel) { profilePendingDelete = nil }
            Button("Delete", role: .destructive) {
                profiles.delete(profile.id)
                scope = .defaults
                profilePendingDelete = nil
            }
        } message: { profile in
            Text(Self.deleteWarning(for: profile, in: profiles))
        }
        #endif
    }

    /// Switching scope is a plain state change — the rows are built by one code path and simply
    /// read the other layer, so there is nothing to commit on the way out.
    private var scopeSelection: Binding<SettingsScope> {
        Binding(get: { scope }, set: { scope = $0 })
    }

    private var namePromptPresented: Binding<Bool> {
        Binding(get: { nameAction != nil }, set: { if !$0 { nameAction = nil } })
    }

    /// Names are unique case-insensitively — two "Work"s make every menu ambiguous, and the
    /// deep-link grammar has to refuse an ambiguous reference rather than guess.
    private var isNameAcceptable: Bool {
        let name = nameDraft.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty else { return false }
        let except: String? = {
            if case .rename(let id) = nameAction { return id }
            return nil
        }()
        return !profiles.nameTaken(name, except: except)
    }

    private var nameMessage: String {
        let name = nameDraft.trimmingCharacters(in: .whitespaces)
        if !name.isEmpty, !isNameAcceptable {
            return "Another profile is already called “\(name)”."
        }
        if case .create = nameAction {
            return "A new profile inherits everything. Change a setting here and only that "
                + "setting is overridden."
        }
        return ""
    }

    private func commitNamePrompt() {
        let name = nameDraft.trimmingCharacters(in: .whitespaces)
        guard !name.isEmpty, let action = nameAction else { return }
        switch action {
        case .create:
            scope = .profile(profiles.create(name: name).id)
        case .rename(let id):
            profiles.rename(id, to: name)
        case .duplicate(let id):
            if let copy = profiles.duplicate(id, name: name) { scope = .profile(copy.id) }
        }
        nameAction = nil
    }

    /// "Game copy", "Game copy 2", … — the first name that isn't taken, so Duplicate never opens
    /// with a name the accept button refuses.
    private static func copyName(of name: String, in store: ProfileStore) -> String {
        let base = "\(name) copy"
        if !store.nameTaken(base) { return base }
        for n in 2...99 where !store.nameTaken("\(base) \(n)") { return "\(base) \(n)" }
        return base
    }

    private static func deleteWarning(for profile: StreamProfile, in store: ProfileStore) -> String {
        let (bound, pinned) = store.usage(of: profile.id)
        var parts: [String] = []
        if bound > 0 {
            parts.append("\(bound) host\(bound == 1 ? "" : "s") will fall back to Default settings")
        }
        if pinned > 0 {
            parts.append("\(pinned) pinned card\(pinned == 1 ? "" : "s") will disappear")
        }
        guard !parts.isEmpty else { return "Nothing uses this profile." }
        return parts.joined(separator: ", ") + "."
    }
}

/// The three name prompts the scope menu raises. One alert serves all of them; only the title,
/// the accept verb and what happens with the name differ.
enum ProfileNameAction: Equatable {
    case create
    case rename(String)
    case duplicate(String)

    var title: String {
        switch self {
        case .create: return "New Profile"
        case .rename: return "Rename Profile"
        case .duplicate: return "Duplicate Profile"
        }
    }

    var accept: String {
        switch self {
        case .create: return "Create"
        case .rename: return "Rename"
        case .duplicate: return "Duplicate"
        }
    }
}
