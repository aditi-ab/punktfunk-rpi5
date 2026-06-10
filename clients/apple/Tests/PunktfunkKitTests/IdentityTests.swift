// Client identity generation through the ABI (punktfunk_generate_identity): the PEM pair
// hosts use to recognize a paired client. Pure local crypto — no host needed.

import XCTest
@testable import PunktfunkKit

final class IdentityTests: XCTestCase {
    func testGenerateIdentityYieldsDistinctPEMPairs() throws {
        let a = try generateIdentity()
        let b = try generateIdentity()

        XCTAssertTrue(a.certPEM.contains("BEGIN CERTIFICATE"), "cert is PEM")
        XCTAssertTrue(a.keyPEM.contains("PRIVATE KEY"), "key is PEM")
        XCTAssertTrue(a.certPEM.hasSuffix("\n") || a.certPEM.contains("END CERTIFICATE"))

        // Each call mints a fresh keypair — identical output would mean a broken RNG.
        XCTAssertNotEqual(a.certPEM, b.certPEM)
        XCTAssertNotEqual(a.keyPEM, b.keyPEM)
    }

    func testPairAgainstNothingFailsCleanly() {
        // Nothing listens on this port; the ceremony must throw within its timeout, and
        // must not report .wrongPIN (no SPAKE2 exchange ever happened).
        do {
            let identity = try generateIdentity()
            _ = try pair(
                host: "127.0.0.1", port: 9, identity: identity,
                pin: "0000", name: "test", timeoutMs: 2000)
            XCTFail("expected pair() against a dead port to throw")
        } catch PunktfunkClientError.wrongPIN {
            XCTFail("dead port must not look like a wrong PIN")
        } catch {
            // any other error is the correct outcome
        }
    }
}
