// Creating, duplicating, renaming, recolouring and deleting a settings profile — one sheet for
// all five (design/client-settings-profiles.md §5.1).
//
// It replaced a menu of four separate actions, three of which raised their own bare text alert
// and the fourth a submenu of colour NAMES with no colour anywhere on it. Making a profile then
// meant: name it in an alert, find it again in the scope menu, open a submenu, and pick "Amber"
// on faith. The two things a profile has — a name and a colour — are decided together here, with
// a live chip showing exactly what the host cards will render.

import PunktfunkKit
import SwiftUI

/// What the sheet was opened to do. Carrying the seed values rather than an id keeps the sheet
/// free of "which store do I read to find out what I'm editing" — the caller already knows.
struct ProfileDraft: Identifiable {
    /// nil = creating (a blank profile, or a duplicate); set = editing that profile.
    var editingID: String?
    var name: String
    var accent: String?
    /// What a newly created profile starts with. Empty for a blank one; the source's overrides
    /// for a duplicate, which is the whole point of duplicating.
    var overrides = SettingsOverlay()
    var title: String
    var accept: String

    var id: String { editingID ?? "new-\(title)" }

    static func create() -> ProfileDraft {
        ProfileDraft(name: "", accent: nil, title: "New Profile", accept: "Create")
    }

    static func edit(_ profile: StreamProfile) -> ProfileDraft {
        ProfileDraft(
            editingID: profile.id, name: profile.name, accent: profile.accent,
            title: "Edit Profile", accept: "Save")
    }

    static func duplicate(_ profile: StreamProfile, name: String) -> ProfileDraft {
        ProfileDraft(
            name: name, accent: profile.accent, overrides: profile.overrides,
            title: "Duplicate Profile", accept: "Duplicate")
    }
}

struct ProfileEditorSheet: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject private var profiles = ProfileStore.shared

    let draft: ProfileDraft
    /// Where the settings surface should be pointing afterwards — at the new profile, or back at
    /// the defaults when this one was deleted.
    let onScope: (SettingsScope) -> Void

    @State private var name: String
    @State private var accent: String?
    @State private var confirmingDelete = false

    init(draft: ProfileDraft, onScope: @escaping (SettingsScope) -> Void) {
        self.draft = draft
        self.onScope = onScope
        _name = State(initialValue: draft.name)
        _accent = State(initialValue: draft.accent)
    }

    var body: some View {
        #if os(macOS)
        withDeletePrompt(VStack(spacing: 0) {
            form
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button(draft.accept) { commit() }
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(!isNameAcceptable)
            }
            .padding(16)
        }
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true))
        #else
        withDeletePrompt(NavigationStack {
            form
                .navigationTitle(draft.title)
                #if os(iOS)
                .navigationBarTitleDisplayMode(.inline)
                #endif
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { dismiss() }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button(draft.accept) { commit() }
                            .disabled(!isNameAcceptable)
                    }
                }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible))
        #endif
    }

    // MARK: - Form

    private var form: some View {
        Form {
            Section {
                preview
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 8)
                    .listRowInsets(EdgeInsets())
                    .listRowBackground(Color.clear)
            }
            Section {
                TextField("Name", text: $name, prompt: Text("Name — e.g. Game, Work, Travel"))
                    #if os(iOS)
                    .textInputAutocapitalization(.words)
                    #endif
            } footer: {
                Text(nameFootnote)
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(duplicateName ? Color.red : Color.secondary)
            }
            Section {
                swatches
                    .listRowInsets(EdgeInsets(top: 12, leading: 16, bottom: 12, trailing: 16))
            } header: {
                Text("Color")
            } footer: {
                Text("Tints this profile's chip on host cards and in the stream overlay.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
            if draft.editingID != nil {
                Section {
                    Button("Delete Profile", role: .destructive) { confirmingDelete = true }
                } footer: {
                    Text(deleteFootnote)
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        #if os(macOS)
        .frame(minHeight: 420)
        #endif
    }

    /// The profile exactly as a host card will show it — the answer to "what am I choosing?",
    /// which a list of colour names never gave.
    private var preview: some View {
        let shown = name.trimmingCharacters(in: .whitespaces)
        return ProfileChip(
            profile: StreamProfile(name: shown.isEmpty ? "Profile" : shown, accent: accent),
            size: 13, prominent: true)
            .opacity(shown.isEmpty ? 0.5 : 1)
            .animation(.easeOut(duration: 0.15), value: accent)
    }

    /// The palette, as colours. `nil` is the brand default and leads, so "no colour" is a choice
    /// on the same shelf as the rest rather than the absence of one.
    private var swatches: some View {
        LazyVGrid(
            columns: [GridItem(.adaptive(minimum: 44), spacing: 14, alignment: .center)],
            spacing: 14
        ) {
            swatch(nil, label: "Default")
            ForEach(ProfileAccent.palette) { option in
                swatch(option.hex, label: option.name)
            }
        }
    }

    private func swatch(_ hex: String?, label: String) -> some View {
        let selected = accent?.caseInsensitiveCompare(hex ?? "") == .orderedSame
            || (accent == nil && hex == nil)
        let color = hex.flatMap { Color(hex: $0) } ?? .brand
        return Button {
            accent = hex
        } label: {
            ZStack {
                Circle()
                    .fill(color)
                    .frame(width: 30, height: 30)
                if selected {
                    Image(systemName: "checkmark")
                        .font(.footnote.weight(.bold))
                        .foregroundStyle(.white)
                    // A ring OUTSIDE the swatch as well as the checkmark: the tick alone washes
                    // out on the pale hues and the ring alone is easy to miss at this size.
                    Circle()
                        .strokeBorder(color, lineWidth: 2)
                        .frame(width: 40, height: 40)
                }
            }
            // 30pt is under the touch minimum; the cell is the target, not the dot.
            .frame(width: 44, height: 44)
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
        .help(label)
    }

    // MARK: - Validation + commit

    private var trimmedName: String { name.trimmingCharacters(in: .whitespaces) }

    /// Case-insensitively unique — two "Work"s make every menu ambiguous, and the deep-link
    /// grammar has to refuse an ambiguous reference rather than guess which one was meant.
    private var duplicateName: Bool {
        !trimmedName.isEmpty && profiles.nameTaken(trimmedName, except: draft.editingID)
    }

    private var isNameAcceptable: Bool { !trimmedName.isEmpty && !duplicateName }

    private var nameFootnote: String {
        if duplicateName { return "Another profile is already called “\(trimmedName)”." }
        return draft.editingID == nil
            ? "A new profile inherits every setting. Change one here and only that one is overridden."
            : "Hosts and pinned cards follow this profile by id, so renaming keeps them attached."
    }

    private var deleteFootnote: String {
        guard let id = draft.editingID else { return "" }
        let (bound, pinned) = profiles.usage(of: id)
        var parts: [String] = []
        if bound > 0 {
            parts.append("\(bound) host\(bound == 1 ? "" : "s") use\(bound == 1 ? "s" : "") it")
        }
        if pinned > 0 {
            parts.append("\(pinned) pinned card\(pinned == 1 ? "" : "s") show\(pinned == 1 ? "s" : "") it")
        }
        guard !parts.isEmpty else { return "Nothing uses this profile yet." }
        return parts.joined(separator: " and ") + "."
    }

    private var deleteMessage: String {
        let usage = deleteFootnote
        return "Hosts using it fall back to Default settings and its pinned cards disappear."
            + (usage.isEmpty ? "" : " \(usage)")
    }

    private func commit() {
        guard isNameAcceptable else { return }
        if let id = draft.editingID {
            profiles.rename(id, to: trimmedName)
            profiles.setAccent(id, to: accent)
        } else {
            var profile = StreamProfile(name: trimmedName, accent: accent)
            profile.overrides = draft.overrides
            profiles.add(profile)
            // Land in the thing that was just made — creating a profile and being left on the
            // defaults is how you end up editing the wrong layer.
            onScope(.profile(profile.id))
        }
        dismiss()
    }

    private func delete() {
        guard let id = draft.editingID else { return }
        profiles.delete(id)
        onScope(.defaults)
        dismiss()
    }

    /// Deleting warns with what it changes (§6): bindings fall back to Default settings and
    /// pinned cards disappear. Neither is an error; neither should be a surprise.
    private func withDeletePrompt<Content: View>(_ content: Content) -> some View {
        content.alert("Delete “\(draft.name)”?", isPresented: $confirmingDelete) {
            Button("Cancel", role: .cancel) {}
            Button("Delete", role: .destructive) { delete() }
        } message: {
            Text(deleteMessage)
        }
    }
}
