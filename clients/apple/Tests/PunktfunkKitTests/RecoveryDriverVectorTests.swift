// Replays the `abi` cases of `clients/shared/recovery-driver-vectors.json` over a live connection
// to the demo host. Each event goes through the one call a pump makes for it, and what the call
// returns must be what the core's replay of the same case expects. The client holds no recovery
// logic, so this pins the wiring to the C exports; the rules are pinned in the core.

import Foundation
import XCTest

@testable import PunktfunkKit

final class RecoveryDriverVectorTests: XCTestCase {
    private struct VectorFile: Decodable {
        let cases: [Case]
    }

    private struct Case: Decodable {
        let name: String
        let abi: Bool?
        let steps: [Step]
    }

    private struct Step: Decodable {
        let ev: String
        let index: UInt32?
        let flags: [String]?
        let want: Want?
    }

    /// What a call can return here: `admit` and `gap` from an arrival, `hold` from a decode.
    private struct Want: Decodable {
        let admit: Bool?
        let gap: UInt32?
        let hold: Bool?
    }

    /// Read from the repo, like the other shared vectors: `Tests/PunktfunkKitTests/` → `clients/`.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("shared/recovery-driver-vectors.json")
    }

    private func bits(_ names: [String]?, _ at: String) -> UInt32 {
        (names ?? []).reduce(0) { acc, name in
            switch name {
            case "sof": return acc | PunktfunkConnection.flagSOF
            case "anchor": return acc | PunktfunkConnection.userFlagRecoveryAnchor
            case "point": return acc | PunktfunkConnection.userFlagRecoveryPoint
            default:
                XCTFail("\(at): unknown flag \(name)")
                return acc
            }
        }
    }

    func testEachPumpCallReachesItsExport() throws {
        let file = try JSONDecoder().decode(
            VectorFile.self, from: Data(contentsOf: Self.vectorFileURL))
        let cases = file.cases.filter { $0.abi == true }
        XCTAssertFalse(cases.isEmpty, "the vector marks at least one case for the C ABI")
        for c in cases {
            // The driver is the connection's, so each case gets its own.
            let host = try XCTUnwrap(DemoHost(titles: ["custom:aurora": "Aurora Drift"]))
            defer { host.stop() }
            let connection = try PunktfunkConnection(
                host: "127.0.0.1", port: host.port, width: 1280, height: 720, refreshHz: 60,
                pinSHA256: host.fingerprint, videoCodecs: PunktfunkConnection.codecH264,
                launchID: "custom:aurora")
            defer { connection.close() }
            for (i, step) in c.steps.enumerated() {
                let at = "\(c.name): step \(i)"
                switch step.ev {
                case "arrived":
                    let got = connection.recoveryOnFrame(try XCTUnwrap(step.index, at))
                    XCTAssertEqual(got.admit, step.want?.admit ?? true, at)
                    XCTAssertEqual(got.gap, step.want?.gap ?? 0, at)
                case "decoded":
                    let hold = connection.recoveryOnDecoded(flags: bits(step.flags, at))
                    XCTAssertEqual(hold, step.want?.hold ?? false, at)
                case "no_output": connection.recoveryOnNoOutput()
                case "lost": connection.recoveryLost()
                case "reset": connection.recoveryReset()
                case "need_keyframe": connection.requestKeyframe()
                default: XCTFail("\(at): no pump call for \(step.ev)")
                }
            }
        }
    }
}
