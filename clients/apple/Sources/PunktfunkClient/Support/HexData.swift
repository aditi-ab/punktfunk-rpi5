// Hex encode/decode for the trust surface — pinned certificate fingerprints and the mDNS `fp`
// TXT value travel as lowercase hex.

import Foundation

extension Data {
    /// Lowercase hex, no separators — to compare a pinned fingerprint against the mDNS `fp`.
    var hexLower: String { map { String(format: "%02x", $0) }.joined() }

    /// Parse an even-length hex string into bytes; nil on any non-hex character or odd length.
    /// Used to turn an mDNS-advertised cert fingerprint into a connect pin.
    init?(hexString: String) {
        let chars = Array(hexString)
        guard chars.count.isMultiple(of: 2) else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(chars.count / 2)
        var i = 0
        while i < chars.count {
            guard let hi = chars[i].hexDigitValue, let lo = chars[i + 1].hexDigitValue else {
                return nil
            }
            bytes.append(UInt8(hi << 4 | lo))
            i += 2
        }
        self = Data(bytes)
    }
}
