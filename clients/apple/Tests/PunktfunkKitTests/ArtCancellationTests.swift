// A poster scrolled past must give up what it was waiting for: its place in the connection
// pool's queue, and its fetch once nobody else wants it.

import Network
import XCTest
@testable import PunktfunkKit

final class ArtCancellationTests: XCTestCase {
    private let key = "198.51.100.7:47990:pin"

    private func connection() -> MgmtConnection {
        MgmtConnection(host: "198.51.100.7", port: 47990, identity: nil, pin: Data())
    }

    /// Polls `condition` for up to 2 s. Each step waits on another task's progress.
    private func eventually(_ condition: () async -> Bool) async -> Bool {
        for _ in 0..<200 {
            if await condition() { return true }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return false
    }

    func testCancelledPoolWaiterHandsItsWakeOn() async throws {
        let pool = MgmtConnectionPool()
        let key = key
        var held: [MgmtConnection] = []
        for _ in 0..<MgmtConnectionPool.maxPerHost {
            held.append(try await pool.acquire(key: key, make: connection))
        }
        let make: @Sendable () -> MgmtConnection = { [self] in connection() }
        let first = Task { try await pool.acquire(key: key, make: make) }
        let firstQueued = await eventually { await pool.waiting(key: key) == 1 }
        XCTAssertTrue(firstQueued)
        let served = Flag()
        let second = Task {
            _ = try await pool.acquire(key: key, make: make)
            served.set()
        }
        let secondQueued = await eventually { await pool.waiting(key: key) == 2 }
        XCTAssertTrue(secondQueued)

        first.cancel()
        await pool.release(held.removeLast(), key: key)

        do {
            _ = try await first.value
            XCTFail("a cancelled waiter took a connection")
        } catch {
            XCTAssertTrue(error is CancellationError)
        }
        // The one release woke the cancelled waiter; the connection must still reach this one.
        let reached = await eventually { served.isSet }
        XCTAssertTrue(reached, "the wake died with the cancelled waiter")
        second.cancel()
        await pool.release(held.removeLast(), key: key)
    }

    func testFlightIsCancelledOnlyWithItsLastWaiter() async throws {
        let flights = ArtFlights()
        let started = Flag()
        let cancelled = Flag()
        let fetch: @Sendable () async throws -> Data = {
            started.set()
            do {
                try await Task.sleep(for: .seconds(30))
            } catch {
                cancelled.set()
                throw error
            }
            return Data()
        }
        let a = Task { try await flights.value(for: "k", fetch: fetch) }
        let b = Task { try await flights.value(for: "k", fetch: fetch) }
        let flying = await eventually { started.isSet }
        XCTAssertTrue(flying)
        // Both must be aboard before the first leaves, or the second starts a flight of its own.
        try await Task.sleep(for: .milliseconds(50))

        a.cancel()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(cancelled.isSet, "one waiter leaving cancelled a fetch another awaits")

        b.cancel()
        let stopped = await eventually { cancelled.isSet }
        XCTAssertTrue(stopped)
        _ = await a.result
        _ = await b.result
    }
}

private final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    var isSet: Bool { lock.withLock { value } }
    func set() { lock.withLock { value = true } }
}
