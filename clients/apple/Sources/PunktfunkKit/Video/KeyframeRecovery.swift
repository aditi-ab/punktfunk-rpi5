// Throttled host keyframe requests for decode recovery, shared by both pumps (StreamPump /
// Stage2Pipeline). Wedge signals arrive from several threads — the decoder's async error callback
// (a VT thread), a submit failure on the pump thread, the framesDropped poll — and the decode stays
// stalled for several frames until the requested IDR lands, so requests are coalesced (100 ms, the
// throttle the working Android path uses: fast enough that a lost recovery IDR is re-requested
// promptly, bounded so a sustained freeze can't flood the control stream). Bound to the live
// connection at pump start, unbound on stop.

import Foundation

final class KeyframeRecovery: @unchecked Sendable {
    private let lock = NSLock()
    private var connection: PunktfunkConnection?
    private var lastNs: UInt64 = 0

    func bind(_ c: PunktfunkConnection?) {
        lock.lock(); connection = c; lastNs = 0; lock.unlock()
    }

    func request() {
        lock.lock()
        let now = DispatchTime.now().uptimeNanoseconds
        let due = lastNs == 0 || now &- lastNs > 100_000_000 // ≥ 100 ms since the last request
        if due { lastNs = now }
        let conn = due ? connection : nil
        lock.unlock()
        conn?.requestKeyframe()
    }
}
