// Replays `clients/shared/jitter-vectors.json` against the Swift twins of core's playback policy:
// `JitterPolicy`, `AvSync` and `DroughtConceal`. Core writes the file from its own types, so a
// case that fails here is a twin that no longer answers what core answers.

import Foundation
import XCTest
@testable import PunktfunkKit

final class JitterVectorTests: XCTestCase {
    private typealias Obj = [String: Any]

    /// Read from the source tree, never copied into the bundle: a copy drifts.
    private static func vectors() throws -> Obj {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/jitter-vectors.json")
        let raw = try JSONSerialization.jsonObject(with: Data(contentsOf: url))
        return try XCTUnwrap(raw as? Obj)
    }

    private static func cases(_ key: String) throws -> [Obj] {
        let list = try XCTUnwrap(try vectors()[key] as? [Obj], "\(key) section")
        XCTAssertGreaterThan(list.count, 3, "the vector file is the contract; keep it rich")
        return list
    }

    private static func int(_ v: Any?) -> Int? { (v as? NSNumber)?.intValue }

    func testJitterPolicyAnswersWhatCoreAnswers() throws {
        for c in try Self.cases("jitter") {
            let name = c["name"] as? String ?? "?"
            var p = JitterPolicy(
                channels: Self.int(c["channels"]) ?? 0, rateHz: Self.int(c["rate_hz"]) ?? 0)
            for (i, entry) in (c["script"] as? [Obj] ?? []).enumerated() {
                if entry.keys.contains("sync") {
                    p.setSyncTarget(Self.int(entry["sync"]))
                    continue
                }
                if let us = Self.int(entry["frame_us"]) {
                    p.setFrameUs(us)
                    continue
                }
                let run = try XCTUnwrap(entry["run"] as? Obj)
                let want = try XCTUnwrap(entry["expect"] as? Obj)
                let depth = Self.int(run["depth"]) ?? 0
                let quantum = Self.int(run["want"]) ?? 0
                let short = run["short"] as? Bool ?? false
                var got: [String: Int] = ["drop": 0, "insert": 0, "crossfade": 0, "trims": 0, "silent": 0]
                for _ in 0..<(Self.int(run["times"]) ?? 0) {
                    let s = p.step(depth: depth, want: quantum)
                    got["drop"]! += s.dropFront
                    got["insert"]! += s.insertFront
                    got["crossfade"]! += s.crossfade
                    got["trims"]! += s.hardTrim ? 1 : 0
                    got["silent"]! += s.silence ? 1 : 0
                    p.noteRead(ranShort: short)
                }
                got["target_ms"] = p.targetMS
                got["avg_depth_ms"] = p.avgDepthMS
                for (key, value) in got {
                    XCTAssertEqual(value, Self.int(want[key]), "\(name) step \(i): \(key)")
                }
                XCTAssertEqual(p.isPrimed, want["primed"] as? Bool, "\(name) step \(i): primed")
            }
        }
    }

    func testAvSyncAnswersWhatCoreAnswers() throws {
        for c in try Self.cases("av_sync") {
            let name = c["name"] as? String ?? "?"
            var s = AvSync(channels: Self.int(c["channels"]) ?? 0, rateHz: Self.int(c["rate_hz"]) ?? 0)
            for (i, entry) in (c["script"] as? [Obj] ?? []).enumerated() {
                let o = try XCTUnwrap(entry["observe"] as? Obj)
                let want = try XCTUnwrap(entry["expect"] as? Obj)
                let observation = AvSync.Observation(
                    ptsNs: UInt64(Self.int(o["pts_ns"]) ?? 0),
                    nowLocalNs: Int64(Self.int(o["now_ns"]) ?? 0),
                    clockOffsetNs: Int64(Self.int(o["clock_offset_ns"]) ?? 0),
                    bufferedAhead: Self.int(o["buffered"]) ?? 0,
                    outputLatencyNs: Int64(Self.int(o["output_latency_ns"]) ?? 0),
                    videoE2eNs: Self.int(o["video_e2e_ns"]).map(Int64.init))
                var last: Int64?
                for _ in 0..<(Self.int(o["times"]) ?? 0) {
                    last = s.observe(observation)
                }
                let desired = s.desiredDepth(currentDepth: Self.int(entry["depth"]) ?? 0)
                let tag = "\(name) step \(i)"
                XCTAssertEqual(last.map(Int.init), Self.int(want["observed_ns"]), "\(tag): observed")
                XCTAssertEqual(s.offsetMS, Self.int(want["offset_ms"]), "\(tag): offset_ms")
                XCTAssertEqual(s.settled, want["settled"] as? Bool, "\(tag): settled")
                XCTAssertEqual(s.implausible, want["implausible"] as? Bool, "\(tag): implausible")
                XCTAssertEqual(desired, Self.int(want["desired"]), "\(tag): desired")
            }
        }
    }

    func testDroughtConcealAnswersWhatCoreAnswers() throws {
        for c in try Self.cases("drought") {
            let name = c["name"] as? String ?? "?"
            var d = DroughtConceal(
                maxMS: Self.int(c["max_ms"]) ?? 0, frameUs: Self.int(c["frame_us"]) ?? 0)
            for (i, entry) in (c["script"] as? [Obj] ?? []).enumerated() {
                if entry["packet"] != nil {
                    // Core hands back the frames it concealed; the Swift twin leaves that
                    // subtraction to core's gap tracker, so only the reset is checked.
                    d.packet()
                    continue
                }
                let yes = d.conceal(
                    sinceLastPacketMS: Self.int(entry["since_ms"]) ?? 0,
                    depthMS: Self.int(entry["depth_ms"]) ?? 0)
                XCTAssertEqual(yes, entry["expect"] as? Bool, "\(name) step \(i)")
            }
            XCTAssertEqual(d.totalMS, Self.int(c["total_ms"]), "\(name): total_ms")
        }
    }
}
