// "Send logs to host" — the one action behind the host card's menu item and the gamepad options
// row. Posts `ClientLogRing` to the PAIRED host (`LibraryClient.sendLogs`), where the web
// console's Logs page shows it next to the host's own log. The Apple port of the Gaming Mode
// console's `ConsoleCmd::SendLogs` (clients/session/src/console.rs), same wording on success.

import Foundation
import PunktfunkKit

private let log = ClientLog(category: "logs")

enum SendLogs {
    /// Upload this device's recent log to `host`. Never throws: the caller shows `message` either
    /// way, and the outcome is itself the last line of the NEXT bundle.
    static func toHost(_ host: StoredHost) async -> (ok: Bool, message: String) {
        // The same two preconditions the library screen applies: this device's mTLS identity
        // (minted on the first connect) and the host's pinned fingerprint (pairing) — an upload
        // is an outbound write carrying the device's diagnostics, and it goes to a host the user
        // has actually paired with, not to whoever answers on that port.
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            return (false, "Connect to this host once first — sending logs uses the identity "
                + "created on the first connect.")
        }
        guard let pin = host.pinnedSHA256 else {
            return (false, "Pair with \(host.displayName) first — logs are only sent to a paired host.")
        }
        do {
            let id = try await LibraryClient.sendLogs(
                address: host.address, port: host.effectiveMgmtPort,
                certPEM: identity.certPEM, keyPEM: identity.keyPEM, hostFingerprint: pin)
            log.info("client logs uploaded to \(host.displayName, privacy: .public) id=\(id, privacy: .public)")
            return (true, "Logs sent to \(host.displayName) — download them from its web console's "
                + "Logs page.")
        } catch {
            let why = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            log.warning("client log upload to \(host.displayName, privacy: .public) failed: \(why, privacy: .public)")
            return (false, "Couldn't send logs — \(why)")
        }
    }
}
