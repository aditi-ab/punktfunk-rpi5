// What one streaming session started, and the order it stops in.
//
// Each part has up to two halves. `stop` runs on the main actor while the connection is still
// up: a capture sends its pad removals, a pointer lets go of a held click. `join` runs off-main
// after every `stop` and may block on a thread join. Both halves walk the parts newest first,
// and the caller's `last` (the connection's close) runs after every join. The owner therefore
// starts its parts in the reverse of the order they have to stop in.
//
// `SessionResourcesTests` pins the order with stand-in parts.

import Foundation

@MainActor
public final class SessionResources {
    private struct Part {
        let object: AnyObject
        let stop: (@MainActor () -> Void)?
        let join: (@Sendable () -> Void)?
    }

    private var parts: [Part] = []

    public init() {}

    /// Record `object`, just started, as the newest part.
    public func add(
        _ object: AnyObject, stop: (@MainActor () -> Void)? = nil,
        join: (@Sendable () -> Void)? = nil
    ) {
        parts.append(Part(object: object, stop: stop, join: join))
    }

    /// Forget `object`: the session stopped it on its own (a toggle, the background timer).
    public func remove(_ object: AnyObject) {
        parts.removeAll { $0.object === object }
    }

    /// The live part of type `T`, if one was started.
    public func part<T>(_: T.Type = T.self) -> T? {
        parts.lazy.compactMap { $0.object as? T }.first
    }

    /// Stop every part, newest first: each `stop` now, then each `join` and `last` off-main.
    /// Leaves the bundle empty for the next session.
    public func stop(then last: @escaping @Sendable () -> Void) {
        let order = Array(parts.reversed())
        parts.removeAll()
        for part in order { part.stop?() }
        let joins = order.compactMap(\.join)
        Task.detached {
            for join in joins { join() }
            last()
        }
    }
}
