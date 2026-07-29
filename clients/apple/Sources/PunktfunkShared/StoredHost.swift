// The saved-host model + its on-disk JSON wire format — the widget/extension depends on BOTH, so
// they live in the dependency-free shared module. The `ObservableObject` store that wraps them
// (`HostStore`, with add/remove/pin/reachability) stays in the app target; discovery-join helpers
// (`matches`, `advertises`) stay there too because they reference PunktfunkKit's `DiscoveredHost`.
//
// Wire-format stability: the JSON encoding of `StoredHost` is now a shared contract between the app
// (writer) and the widget (reader). The `PunktfunkSharedTests` codec round-trip pins it — do not
// rename the coding keys or make a stored `Optional` non-optional (older saved JSON must still
// decode; synthesized Decodable treats a missing Optional as nil).

import Foundation

/// The management-API port default (distinct from the data-plane `port`). Lives here (not in
/// PunktfunkKit's LibraryClient, which re-exports it) so `StoredHost.effectiveMgmtPort` can resolve
/// it without the shared module taking a dependency on the kit.
public let punktfunkDefaultMgmtPort: UInt16 = 47990

public struct StoredHost: Identifiable, Codable, Hashable, Sendable {
    public var id = UUID()
    public var name: String
    public var address: String
    public var port: UInt16 = 9777
    /// SHA-256 of the host's certificate, set after the user explicitly trusted it.
    public var pinnedSHA256: Data?
    /// Last time a streaming session actually started (nil until the first one).
    public var lastConnected: Date?
    /// Management-API port for the library browser (distinct from the data-plane `port`). Optional
    /// (NOT a defaulted non-optional) so older saved hosts — whose JSON lacks this key — still
    /// decode: synthesized Decodable ignores property defaults but treats a missing Optional as
    /// nil. Resolve via `effectiveMgmtPort`. (Auth is mTLS by the pinned identity — no token.)
    public var mgmtPort: UInt16?
    /// Wake-on-LAN MAC address(es) of the host's wake-capable NIC(s), each `aa:bb:cc:dd:ee:ff`.
    /// Learned from the host's mDNS `mac` TXT record while it's awake and persisted here, so the
    /// client can send a magic packet to wake the host later (when it's asleep and no longer
    /// advertising). Optional (same forward-compat reason as `mgmtPort`); nil until first learned.
    public var macAddresses: [String]?
    /// Share the clipboard with this host (macOS sessions; design/clipboard-and-file-transfer.md
    /// §5.3). Opt-in per host: nil/false = off (nil also keeps older saved JSON decoding — same
    /// forward-compat reason as `mgmtPort`). Honored only when the host advertises
    /// `HOST_CAP_CLIPBOARD`.
    public var clipboardSync: Bool?
    /// This host's default settings profile (`StreamProfile.id`) — what a plain click/tap uses.
    /// nil, or an id whose profile was deleted, resolves as "Default settings", i.e. exactly
    /// today's behaviour: a dangling binding is never an error and never blocks a connect
    /// (design/client-settings-profiles.md §4.4). Optional and appended last for the same
    /// widget-contract reason as `mgmtPort`.
    public var profileID: String?
    /// Profiles pinned as additional cards for this host (design §5.2a), in card order. NOT the
    /// default — that is `profileID`; a pin is presentation only, and duplicates and dangling ids
    /// are dropped when the cards are built. Optional for the same forward-compat reason.
    public var pinnedProfileIDs: [String]?
    /// When this host was saved — what the grid's "Date Added" sort orders by. Optional and
    /// appended last for the same widget-contract reason as the rest; hosts saved before it
    /// existed have none, and keep their stored order, which IS the order they were added in.
    public var addedAt: Date?
    /// The host's OS-identity chain (`windows` | `linux/<family>/<id>`, ...) learned from its
    /// mDNS `os` TXT while online, so the card's OS mark survives the host going to sleep.
    /// Optional and appended last for the same widget-contract reason; nil until first learned.
    public var osChain: String?

    public init(
        id: UUID = UUID(), name: String, address: String, port: UInt16 = 9777,
        pinnedSHA256: Data? = nil, lastConnected: Date? = nil, mgmtPort: UInt16? = nil,
        macAddresses: [String]? = nil, clipboardSync: Bool? = nil,
        profileID: String? = nil, pinnedProfileIDs: [String]? = nil, addedAt: Date? = nil,
        osChain: String? = nil
    ) {
        self.id = id
        self.name = name
        self.address = address
        self.port = port
        self.pinnedSHA256 = pinnedSHA256
        self.lastConnected = lastConnected
        self.mgmtPort = mgmtPort
        self.macAddresses = macAddresses
        self.clipboardSync = clipboardSync
        self.profileID = profileID
        self.pinnedProfileIDs = pinnedProfileIDs
        self.addedAt = addedAt
        self.osChain = osChain
    }

    public var displayName: String { name.isEmpty ? address : name }
    public var effectiveMgmtPort: UInt16 { mgmtPort ?? punktfunkDefaultMgmtPort }
    /// Wake-capable, in a form the wake helper accepts (empty when none learned yet).
    public var wakeMacs: [String] { macAddresses ?? [] }
}
