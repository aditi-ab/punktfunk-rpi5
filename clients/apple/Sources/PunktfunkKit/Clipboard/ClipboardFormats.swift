// The shared clipboard's format vocabulary (design/clipboard-and-file-transfer.md §3.5), stated
// once for AppKit and UIKit alike.
//
// Every Apple pasteboard type in the table IS a uniform type identifier, and both frameworks name
// them with the same strings — `NSPasteboard.PasteboardType.png` and the UIPasteboard type
// `"public.png"` are the same bytes. Keeping the table as plain strings is therefore not a
// lowest-common-denominator compromise; it is the actual shared spelling, and it keeps the two
// platform adapters from drifting apart in what they announce.
#if !os(tvOS)
import Foundation

enum ClipboardFormats {
    /// Wire MIME ↔ uniform type identifier, in announce order. Files
    /// (`application/x-punktfunk-files`) ride Phase 2 and are absent here.
    ///
    /// Original image formats sit beside the mandatory `image/png` floor rather than replacing it:
    /// a copied JPEG never balloons into PNG and a GIF keeps its animation, while a peer that can
    /// only place PNG still has something to take.
    static let table: [(wire: String, uti: String)] = [
        ("text/plain;charset=utf-8", "public.utf8-plain-text"),
        ("text/rtf", "public.rtf"),
        ("text/html", "public.html"),
        ("image/png", "public.png"),
        ("image/jpeg", "public.jpeg"),
        ("image/gif", "com.compuserve.gif"),
    ]

    /// Pasteboard marker types that must never cross the wire — password managers mark secrets
    /// with these (see nspasteboard.org). A Mac convention that costs nothing to honour on iOS:
    /// the cross-platform managers set them there too, and a pasteboard that carries neither is
    /// unaffected.
    static let concealed = "org.nspasteboard.ConcealedType"
    static let transient = "org.nspasteboard.TransientType"

    /// Image types we do not announce verbatim but CAN serve `image/png` from by transcoding at
    /// fetch time — screenshots and Preview leave TIFF, the camera roll leaves HEIC.
    static let pngSources = ["public.tiff", "public.heic"]

    static func uti(forWire wire: String) -> String? {
        table.first { $0.wire == wire }?.uti
    }

    static func wire(forUti uti: String) -> String? {
        table.first { $0.uti == uti }?.wire
    }

    /// True when the pasteboard is carrying a secret and must be ignored entirely.
    static func isConcealed(_ types: [String]) -> Bool {
        types.contains(concealed) || types.contains(transient)
    }

    /// The format list to announce for a pasteboard holding `types` — the lazy offer's whole
    /// payload (§3.2). Empty means "nothing we sync", which legitimately clears the peer's side.
    static func offerKinds(forTypes types: [String]) -> [PunktfunkConnection.ClipKind] {
        var kinds = table
            .filter { types.contains($0.uti) }
            .map { PunktfunkConnection.ClipKind(mime: $0.wire) }
        // PNG floor: announce the portable `image/png` whenever ANY convertible image is present —
        // native PNG, TIFF/HEIC, or a JPEG/GIF original already being offered verbatim above. The
        // adapters convert at fetch time, so the fallback costs nothing unless a peer pastes it.
        if !kinds.contains(where: { $0.mime == "image/png" }),
            types.contains(where: { pngSources.contains($0) })
                || kinds.contains(where: { $0.mime.hasPrefix("image/") })
        {
            kinds.append(PunktfunkConnection.ClipKind(mime: "image/png"))
        }
        return kinds
    }

    /// The uniform types to place for a remote offer, in the table's order and skipping kinds this
    /// client has no mapping for (files, and whatever a future host learns to offer).
    static func placeableUtis(for kinds: [PunktfunkConnection.ClipKind]) -> [String] {
        kinds.compactMap { uti(forWire: $0.mime) }
    }
}
#endif
