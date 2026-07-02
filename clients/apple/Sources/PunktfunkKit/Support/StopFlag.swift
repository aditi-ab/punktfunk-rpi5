// One NSLock-guarded boolean, set once: the cancellation handle shared by the session services
// (the two video pumps, audio playback/mic, gamepad feedback). Each start() creates a fresh flag
// and hands it to its worker thread(s); stop() sets it — permanently, so a stale worker can never
// be revived by a newer start.

import Foundation

final class StopFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var stopped = false
    var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }
    func stop() {
        lock.lock()
        stopped = true
        lock.unlock()
    }
}
