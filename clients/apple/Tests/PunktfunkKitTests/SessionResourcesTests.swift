// `SessionResources` with stand-in parts: the stop order a session's teardown depends on.

import Foundation
import XCTest
@testable import PunktfunkKit

@MainActor
final class SessionResourcesTests: XCTestCase {
    /// Each half appends its name; the joins append from a detached task.
    private final class Log: @unchecked Sendable {
        private let lock = NSLock()
        private var lines: [String] = []
        func append(_ line: String) { lock.lock(); lines.append(line); lock.unlock() }
        var all: [String] { lock.lock(); defer { lock.unlock() }; return lines }
    }

    private final class Part {}

    private func stopAndWait(_ resources: SessionResources, _ log: Log) {
        let done = expectation(description: "joins and close ran")
        resources.stop {
            log.append("connection.close")
            done.fulfill()
        }
        wait(for: [done], timeout: 5)
    }

    /// Started in `SessionModel.beginStreaming`'s order, stopped in the session's engine order:
    /// the main halves newest first, then the joins newest first, then the close.
    func testASessionStopsInTheEngineTeardownOrder() {
        let log = Log()
        let resources = SessionResources()
        resources.add(Part(), stop: { log.append("statsTimer.invalidate") })
        resources.add(Part(), stop: { log.append("sleepGuard.release") })
        resources.add(Part(), join: { log.append("clipboard.stop") })
        resources.add(Part(), join: { log.append("feedback.stop") })
        resources.add(Part(), join: { log.append("audio.stop") })
        resources.add(Part(), stop: { log.append("remotePointer.stop") })
        resources.add(Part(), stop: { log.append("feedback.unhookSink"); log.append("sc2.stop") })
        resources.add(Part(), stop: { log.append("gamepadCapture.stop") })
        resources.add(Part(), stop: { log.append("virtualPad.close") })
        stopAndWait(resources, log)
        XCTAssertEqual(log.all, [
            "virtualPad.close", "gamepadCapture.stop", "feedback.unhookSink", "sc2.stop",
            "remotePointer.stop", "sleepGuard.release", "statsTimer.invalidate",
            "audio.stop", "feedback.stop", "clipboard.stop", "connection.close",
        ])
    }

    /// Every main half has run when `stop` returns; no join has.
    func testMainHalvesRunBeforeStopReturns() {
        let log = Log()
        let resources = SessionResources()
        let gate = DispatchSemaphore(value: 0)
        resources.add(Part(), stop: { log.append("a.stop") }, join: {
            gate.wait()
            log.append("a.join")
        })
        resources.add(Part(), stop: { log.append("b.stop") })
        let done = expectation(description: "joins ran")
        resources.stop { done.fulfill() }
        XCTAssertEqual(log.all, ["b.stop", "a.stop"])
        gate.signal()
        wait(for: [done], timeout: 5)
        XCTAssertEqual(log.all, ["b.stop", "a.stop", "a.join"])
    }

    /// A part the session stopped on its own is not stopped again, and a stopped bundle is empty.
    func testRemovedPartsAndASecondStopRunNothing() {
        let log = Log()
        let resources = SessionResources()
        let pad = Part()
        resources.add(pad, stop: { log.append("pad.close") })
        resources.add(Part(), join: { log.append("audio.stop") })
        XCTAssertTrue(resources.part(Part.self) === pad)
        resources.remove(pad)
        stopAndWait(resources, log)
        XCTAssertEqual(log.all, ["audio.stop", "connection.close"])
        XCTAssertNil(resources.part(Part.self))
        stopAndWait(resources, log)
        XCTAssertEqual(log.all, ["audio.stop", "connection.close", "connection.close"])
    }
}
