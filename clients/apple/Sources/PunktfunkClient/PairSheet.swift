// PIN pairing sheet. The host, started with --allow-pairing (or --require-pairing),
// prints a short PIN at startup ("PAIRING ARMED — enter this PIN on the client to
// pair"); the user types it here. The ceremony is SPAKE2, so a wrong PIN buys an
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

    var body: some View {
        #if os(tvOS)
        VStack(spacing: 24) {
            Label("Pair with \(host.displayName)", systemImage: "lock.shield")
                .font(.title3.weight(.semibold))
                .foregroundStyle(.tint)
            Text("The host prints the PIN when pairing is armed "
                + "(--allow-pairing, \u{201C}PAIRING ARMED\u{201D} in its log). "
                + "Pairing verifies both sides at once — no fingerprint comparison "
                + "needed.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            TextField("PIN", text: $pin, prompt: Text("Shown in the host's log"))
                .labelsHidden()
            TextField(
                "Client name", text: $clientName,
                prompt: Text("How the host lists this device"))
                .labelsHidden()
            if let errorText {
                Text(errorText)
                    .font(.callout)
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
        .onDisappear { token.cancelled = true }
        #else
        VStack(spacing: 0) {
            Form {
                Section {
                    TextField("PIN", text: $pin, prompt: Text("Shown in the host's log"))
                        .font(.system(.title3, design: .monospaced))
                        #if os(iOS)
                        .keyboardType(.numberPad)
                        #endif
                    TextField(
                        "Client name", text: $clientName,
                        prompt: Text("How the host lists this Mac"))
                        #if os(tvOS)
                        .labelsHidden() // prefilled → tvOS floats the label off-center
                        #endif
                } header: {
                    Label("Pair with \(host.displayName)", systemImage: "lock.shield")
                        .foregroundStyle(.tint)
                } footer: {
                    Text("The host prints the PIN when pairing is armed "
                        + "(--allow-pairing, \u{201C}PAIRING ARMED\u{201D} in its log). "
                        + "Pairing verifies both sides at once — no fingerprint "
                        + "comparison needed.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if let errorText {
                    Section {
                        Text(errorText)
                            .font(.callout)
                            .foregroundStyle(.red)
                    }
                }
            }
            #if !os(tvOS)
        .formStyle(.grouped)
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
                    .buttonStyle(.borderedProminent)
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
        .interactiveDismissDisabled(busy)
        .onDisappear { token.cancelled = true } // any other dismissal path
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
                    errorText = "Wrong PIN — check the host's \u{201C}PAIRING ARMED\u{201D} "
                        + "line and try again."
                case .failure(is ClientIdentityStore.IdentityError):
                    errorText = "Can't store this Mac's identity in the Keychain, so the "
                        + "pairing would not survive a relaunch. Unlock the login "
                        + "keychain and try again."
                case .failure:
                    errorText = "Pairing failed. Is the host reachable, armed with "
                        + "--allow-pairing, and not mid-session? Retries are rate-limited "
                        + "to one per 2 seconds."
                }
            }
        }
    }
}
