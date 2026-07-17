//! Decode-latency bookkeeping: realtime clock + decoded-pts / user-flags stat recording.

use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;

/// Wall-clock now in nanoseconds (CLOCK_REALTIME basis), to compare against the host-stamped
/// capture `pts_ns` after the skew offset is applied.
pub(super) fn now_realtime_ns() -> i128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// HUD `decoded` point for one dequeued output frame, keyed by the echoed `presentationTimeUs`:
/// build the end-to-end (capture→decoded, skew-corrected, clamped to (0, 10 s)) and `decode`
/// (received→decoded, single-clock local, ≥ 0) samples and hand them to
/// [`crate::stats::VideoStats::note_decoded`]. The pts keys the receipt stamp in `in_flight`;
/// entries older than it are evicted (decode order == input order here — low-latency, no
/// B-frames — so anything before it was dropped inside the codec or stamped before a flush).
/// `decoded_ns` is the availability instant: the dequeue (sync loop) or the output callback's
/// stamp (async loop).
pub(super) fn note_decoded_pts(
    client: &NativeClient,
    measure_decode: bool,
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    clock_offset: i64,
    pts_us: u64,
    decoded_ns: i128,
) {
    // Pair the echoed pts back to its receipt stamp, evicting stale (older) entries as we go.
    let mut received_ns = None;
    while let Some(&(p, r)) = in_flight.front() {
        if p > pts_us {
            break; // future frame — leave it for its own output buffer
        }
        in_flight.pop_front();
        if p == pts_us {
            received_ns = Some(r);
            break;
        }
    }
    let decode_us = received_ns.map(|r| ((decoded_ns - r).max(0) / 1000) as u64);
    // Adaptive bitrate: the `decode` stage (received→decoded, single-clock local) IS the decoder-
    // backlog signal — the only bottleneck the host-side network signals can't see (a fast LAN
    // feeding a slower mobile decoder). Report it whenever the controller is armed, regardless of
    // the HUD; `report_decode_us` is a cheap accumulate the pump windows.
    if measure_decode {
        if let Some(us) = decode_us {
            client.report_decode_us(us.min(u32::MAX as u64) as u32);
        }
    }
    // HUD histogram: only while the overlay is visible (a measure-only caller enters here for the
    // ABR report alone). `end-to-end` = capture→decoded (skew-corrected) tiles the `decode` stage.
    // pts_us is the truncated frame.pts_ns/1000 we queued, so ×1000 re-approximates capture time to
    // < 1 µs — negligible against the ms-scale figures shown.
    if stats.enabled() {
        let e2e_ns = decoded_ns + clock_offset as i128 - pts_us as i128 * 1000;
        let e2e_us = (e2e_ns > 0 && e2e_ns < 10_000_000_000).then_some((e2e_ns / 1000) as u64);
        stats.note_decoded(e2e_us, decode_us);
    }
}

/// The AU `user_flags` for a decoded output, keyed by the echoed `presentationTimeUs`. Recovery
/// signalling (FLAG_SOF IDR marker / RECOVERY_ANCHOR / RECOVERY_POINT) rides the AU's flags, which are
/// only in scope at feed time — so the feed side parks `(pts_us, flags)` here and the present side
/// looks them up to fold [`ReanchorGate::on_decoded`]. Decode order == input order (low-latency, no
/// B-frames), so this evicts entries older than `pts_us` as it goes; a miss (probe filler, or an entry
/// aged past the cap) reads `0` — no recovery flags, decoded normally.
pub(super) fn take_flags(map: &mut VecDeque<(u64, u32)>, pts_us: u64) -> u32 {
    while let Some(&(p, f)) = map.front() {
        if p > pts_us {
            break; // future frame — leave it for its own output buffer
        }
        map.pop_front();
        if p == pts_us {
            return f;
        }
    }
    0
}
