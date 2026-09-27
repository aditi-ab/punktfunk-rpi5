//! Decode-latency bookkeeping: realtime clock + decoded-pts / user-flags stat recording.

use punktfunk_core::client::NativeClient;
use punktfunk_core::session::Frame;
use std::collections::VecDeque;
use std::time::Duration;

pub(crate) use punktfunk_core::client::now_realtime_ns;

/// HUD `decoded` point for one dequeued output frame, keyed by the echoed `presentationTimeUs`:
/// hand the frame and its `decode` span (received→decoded, single-clock local, ≥ 0) to
/// [`crate::stats::VideoStats::note_decoded`]. The pts keys the receipt stamp in `in_flight`;
/// entries older than it are evicted (decode order == input order here — low-latency, no
/// B-frames — so anything before it was dropped inside the codec or stamped before a flush).
/// `decoded_ns` is the availability instant: the dequeue (sync loop) or the output callback's
/// stamp (async loop). Returns the receipt stamp it paired (if any) so the caller can split the
/// `decode` stage further (feed wait vs codec-pure) without re-walking the map.
pub(super) fn note_decoded_pts(
    client: &NativeClient,
    measure_decode: bool,
    stats: &crate::stats::VideoStats,
    in_flight: &mut VecDeque<(u64, i128)>,
    pts_us: u64,
    decoded_ns: i128,
) -> Option<i128> {
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
    // Overlay only while it is visible (a measure-only caller enters here for the ABR report
    // alone). `pts_us` is the truncated capture pts we queued: ×1000 is within 1 µs of it.
    if stats.enabled() {
        stats.note_decoded(pts_us * 1000, decoded_ns, decode_us);
    }
    received_ns
}

/// The queued-instant stamp for a decoded output, keyed by the echoed `presentationTimeUs` — the
/// same monotonic evict-as-you-go pairing as [`take_flags`], over an `(pts_us, realtime_ns)` map
/// (the feed side stamps each AU as its last piece enters the codec). A miss returns `None` —
/// the split is simply not recorded for that frame.
pub(super) fn take_stamp(map: &mut VecDeque<(u64, i128)>, pts_us: u64) -> Option<i128> {
    while let Some(&(p, t)) = map.front() {
        if p > pts_us {
            break; // future frame — leave it for its own output buffer
        }
        map.pop_front();
        if p == pts_us {
            return Some(t);
        }
    }
    None
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

/// p50/max of an unsorted µs sample vec, in ms — the HUD's per-stage summary, shared by both
/// presenters. `(0, 0)` when empty.
pub(super) fn p50_max_ms(mut v: Vec<u64>) -> (f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0);
    }
    v.sort_unstable();
    (
        v[v.len() / 2] as f64 / 1000.0,
        v[v.len() - 1] as f64 / 1000.0,
    )
}

/// The `received` point for one arriving AU: the core's reassembly stamp, which keys the
/// in-flight map the decode stage pairs against. The connector already noted receipt for the
/// overlay and matches each 0xCF to its frame; draining the timings here logs the host's
/// phase-lock ACK when it changes.
pub(super) fn note_received_frame(
    client: &NativeClient,
    frame: &Frame,
    last_phase_ack: &mut Option<i32>,
) -> i128 {
    // Reassembly completion, NOT the pull instant: stamping at the pull would fold the hand-off
    // queue wait into the network figure. 0 = older core.
    let received_ns = if frame.received_ns > 0 {
        frame.received_ns as i128
    } else {
        now_realtime_ns()
    };
    while let Ok(t) = client.next_host_timing(Duration::ZERO) {
        if t.applied_phase_ns != *last_phase_ack {
            log::info!(
                target: "pf.phase",
                "host applied_phase={:?}us",
                t.applied_phase_ns.map(|n| n / 1000)
            );
            *last_phase_ack = t.applied_phase_ns;
        }
    }
    received_ns
}
