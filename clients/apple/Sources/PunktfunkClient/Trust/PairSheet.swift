// PIN pairing sheet. The host shows the pairing PIN in its web console (port 47992 →
// Pairing; also printed in the host's log when armed via --allow-pairing); the user
// types it here. The ceremony is SPAKE2, so a wrong PIN buys an
// attacker exactly one online guess — for the user a typo just means "try again" (the
// host rate-limits ceremonies to one per 2 s). Success returns the host's now-VERIFIED
// fingerprint: the caller pins it, no manual comparison needed, and the host stores this
// client's identity in return.

import Foundation
import PunktfunkKit
import SwiftUI

/// Dismissing the sheet must abandon an in-flight ceremony: the blocking pair() call
/// can't be interrupted, so its completion checks this flag and self-discards — a late
/// success must NOT pin and auto-connect to a host the user cancelled out of. Only
/// touched on the main actor.
private final class CeremonyToken: @unchecked Sendable {
    var cancelled = false
}

struct PairSheet: View {
    @Environment(\.dismiss) private var dismiss
    let host: StoredHost
    /// Called with the verified host fingerprint after a successful ceremony.
    let onPaired: (Data) -> Void

    @State private var pin = ""
    #if os(macOS)
    @State private var clientName = Host.current().localizedName ?? "Mac"
    #else
    @State private var clientName = UIDevice.current.name
    #endif
    @State private var busy = false
    @State private var errorText: String?
    @State private var token = CeremonyToken()
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case pin, clientName
        var id: String { rawValue }
    }
    @State private var editing: EditField?
    #endif

    var body: some View {
        #if os(tvOS)
        VStack(spacing: 24) {
            Text("The PIN is shown in the host's web console "
                + "(https://<host>:47992 → Pairing). "
                + "Pairing verifies both sides at once — no fingerprint comparison "
                + "needed.")
                .font(.geist(22, relativeTo: .callout)) // TV-legible (system callout is ~25 there)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            TVFieldRow(
                label: "PIN", value: pin, placeholder: "Shown in the host's web console"
            ) { editing = .pin }
            TVFieldRow(
                label: "Device name", value: clientName, placeholder: "Apple TV"
            ) { editing = .clientName }
            if let errorText {
                Text(errorText)
                    .font(.geist(22, relativeTo: .callout))
                    .foregroundStyle(.red)
            }
            HStack(spacing: 32) {
                Button("Cancel", role: .cancel) {
                    token.cancelled = true
                    dismiss()
                }
                if busy {
                    ProgressView()
                }
                Button("Pair & Connect") { runCeremony() }
                    .disabled(busy || pin.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .navigationTitle("Pair with \(host.displayName)")
        .onDisappear { token.cancelled = true }
        .fullScreenCover(item: $editing) { field in
            switch field {
            case .pin:
                TVTextEntry(
                    title: "PIN (shown in the host's web console)", text: pin,
                    keyboardType: .numberPad
                ) {
                    pin = $0.trimmingCharacters(in: .whitespaces)
                    editing = nil
                }
            case .clientName:
                TVTextEntry(title: "Device name", text: clientName) {
                    clientName = $0
                    editing = nil
                }
            }
        }
        #else
        VStack(spacing: 0) {
            Form {
                Section {
                    TextField(
                        "PIN", text: $pin,
                        prompt: Text("Shown in the host's web console"))
                        .font(.geistFixed(16)) // prominent, but on-brand mono (not oversized title3)
                        #if os(iOS)
                        .keyboardType(.numberPad)
                        #endif
                    TextField(
                        "Client name", text: $clientName,
                        prompt: Text(Self.clientNamePrompt))
                        #if os(tvOS)
                        .labelsHidden() // prefilled → tvOS floats the label off-center
                        #endif
                } header: {
                    Label("Pair with \(host.displayName)", systemImage: "lock.shield")
                        .foregroundStyle(.tint)
                } footer: {
                    Text("The PIN is shown in the host's web console "
                        + "(https://<host>:47992 → Pairing). "
                        + "Pairing verifies both sides at once — no fingerprint "
                        + "comparison needed.")
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
                if let errorText {
                    Section {
                        Text(errorText)
                            .font(.geist(16, relativeTo: .callout))
                            .foregroundStyle(.red)
                    }
                }
            }
            #if !os(tvOS)
        .formStyle(.grouped)
        // Bring the grouped form's default system text down to the app's Geist scale so the sheet
        // doesn't read oversized / out of place (matches AddHostSheet). The PIN field keeps its own
        // explicit Geist Mono font.
        .font(.geist(12, relativeTo: .callout))
        .controlSize(.small)
        #endif
            HStack {
                Button("Cancel", role: .cancel) {
                    token.cancelled = true
                    dismiss()
                }
                #if !os(tvOS)
                .keyboardShortcut(.cancelAction)
                #endif
                Spacer()
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .padding(.trailing, 8)
                }
                Button("Pair & Connect") { runCeremony() }
                    .glassProminentButtonStyle()
                    #if !os(tvOS)
                    .keyboardShortcut(.defaultAction)
                    #endif
                    .disabled(busy || pin.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            #if os(iOS)
            .controlSize(.large)
            #endif
            .padding(16)
        }
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #if os(iOS)
        // Bottom sheet instead of a full-screen modal (Liquid Glass background on iOS 26).
        // .medium rests; .large is included so the sheet grows to keep the Pair/Cancel row
        // above the keyboard when the PIN field is focused. Hide the grabber while the ceremony
        // is in flight — dismissal is disabled then (interactiveDismissDisabled), so a drag
        // would only rubber-band; the always-enabled Cancel button is the exit.
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(busy ? .hidden : .visible)
        #endif
        .interactiveDismissDisabled(busy)
        .onDisappear { token.cancelled = true } // any other dismissal path
        #endif
    }

    /// The field prompt names the device you are actually on — it said "this Mac" on every
    /// platform, which on an iPhone is simply wrong.
    private static var clientNamePrompt: String {
        #if os(macOS)
        "How the host lists this Mac"
        #else
        "How the host lists this device"
        #endif
    }

    private func runCeremony() {
        busy = true
        errorText = nil
        let pin = pin.trimmingCharacters(in: .whitespaces)
        let name = clientName.trimmingCharacters(in: .whitespaces)
        let address = host.address
        let port = host.port
        let token = token
        Task.detached(priority: .userInitiated) {
            // Identity load + the ceremony both block — keep them off the main actor.
            // loadForPairing is the strict variant: the host durably trusts this
            // identity, so it must have made it into the Keychain.
            let result = Result {
                let identity = try ClientIdentityStore.shared.loadForPairing()
                return try PunktfunkKit.pair(
                    host: address, port: port, identity: identity,
                    pin: pin, name: name.isEmpty ? "Mac" : name)
            }
            await MainActor.run {
                guard !token.cancelled else { return } // sheet dismissed mid-ceremony
                busy = false
                switch result {
                case .success(let fingerprint):
                    onPaired(fingerprint)
                    dismiss()
                case .failure(PunktfunkClientError.wrongPIN):
                    errorText = "Wrong PIN — check the host's web console (port 47992) "
                        + "and try again."
                case .failure(PunktfunkClientError.rejected(let rejection)):
                    // The host answered and said why (not armed / rate-limited / armed for
                    // another device) — show that instead of the guessing-game fallback.
                    errorText = rejection.userMessage
                case .failure(is ClientIdentityStore.IdentityError):
                    errorText = "Can't store this Mac's identity in the Keychain, so the "
                        + "pairing would not survive a relaunch. Unlock the login "
                        + "keychain and try again."
                case .failure:
                    errorText = "Pairing failed — the host didn't answer. Is it running, "
                        + "and is this device on the same network (no VPN, no guest-Wi-Fi "
                        + "isolation)?"
                }
            }
        }
    }
}

#if DEBUG
extension PairSheet {
    /// Screenshot-harness seed (`ShotScenes`). A capture of the untouched sheet shows an empty PIN
    /// field, a DISABLED "Pair & Connect", and — because the client name defaults to the device's
    /// own — whatever the capture simulator happens to be called (`pf-shot-iphone-6.9` reached App
    /// Store Connect that way). Seeding both fields captures the ceremony as a user meets it,
    /// mid-entry, with a live primary button.
    ///
    /// An extension so `PairSheet` keeps its memberwise initialiser, and THIS file so it can reach
    /// the private state.
    init(
        host: StoredHost, shotPIN: String, shotClientName: String,
        onPaired: @escaping (Data) -> Void
    ) {
        self.init(host: host, onPaired: onPaired)
        _pin = State(initialValue: shotPIN)
        _clientName = State(initialValue: shotClientName)
    }
}
#endif
