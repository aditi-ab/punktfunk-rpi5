import Darwin

/// Client `CLOCK_REALTIME` now, in nanoseconds: the clock every latency stamp (received, pulled,
/// decoded, displayed, A/V sync) is read in.
@inline(__always)
func realtimeNowNs() -> Int64 {
    Int64(clock_gettime_nsec_np(CLOCK_REALTIME))
}
