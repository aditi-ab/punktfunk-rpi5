//! Network speed test: one decode-less connect, one host burst, one measurement.
//!
//! Shared by every shell that offers "Test network speed…" — the Windows client's speed
//! page and its `--headless --speed-test`, and the console shell's host menu through the
//! session binary. The measurement runs over the REAL data plane, which is why it is a
//! connect and not a synthetic socket: a path that carries QUIC video is the path worth
//! measuring.
//!
//! Where the answer goes is the caller's decision, not this module's — a measured bitrate
//! belongs in the layer the tested host resolves bitrate from
//! (`design/client-settings-profiles.md` §5.3).

use punktfunk_core::client::{ConnectParams, NativeClient, ProbeOutcome};
use punktfunk_core::config::Mode;
use std::time::{Duration, Instant};

/// Ask for far more than any real link can carry, so the link is what limits the answer.
const TARGET_KBPS: u32 = 3_000_000;

/// Long enough to fill the pipe and settle, short enough not to interrupt anyone for long.
const BURST_MS: u32 = 2_000;

/// A burst that never reports is a dead session, not a slow link.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Let the last UDP shards land before tearing the session down, or the tail of the burst
/// counts as loss that never happened.
const SETTLE: Duration = Duration::from_millis(400);

/// Headroom a recommendation keeps under the measurement: the FEC overhead plus the loss a
/// real stream will meet. Integer arithmetic in this order (not `* 0.7`) so every client
/// recommends the same kilobit.
pub fn recommended_kbps(throughput_kbps: u32) -> u32 {
    throughput_kbps / 10 * 7
}

/// Connect to `addr`:`port`, run one burst, and return the host's final measurement.
/// Blocking — call it on a worker thread.
///
/// The connect is deliberately minimal: 720p60, no launch, host-default bitrate. Nothing
/// here presents a frame, and asking a host to spin up a 4K encode for a two-second
/// measurement would be rude to it and slower for us.
pub fn run_speed_probe(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
) -> Result<ProbeOutcome, String> {
    run_speed_probe_with(addr, port, fp_hex, identity, |_| {})
}

/// [`run_speed_probe`], reporting the burst's live throughput (kbps) at every poll, for
/// a shell that draws the measurement as it happens.
pub fn run_speed_probe_with(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
    mut progress: impl FnMut(u32),
) -> Result<ProbeOutcome, String> {
    // Pin the saved/advertised fingerprint when we have one; a manual host measures over TOFU.
    let pin = fp_hex.and_then(crate::trust::parse_hex32);
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    // The host's default rate: this measures the link, not an encoder setting. Nothing
    // presents, so every other Hello field stays default too.
    let c = NativeClient::connect(ConnectParams {
        // The DEVICE-FREE answer, not `decodable_codecs_for`: this connect creates no
        // presenter and has no `VulkanDecodeDevice` to gate AV1 on, and it decodes nothing.
        video_codecs: crate::video::decodable_codecs(),
        // Same label a real session sends — a speed test against a host that doesn't know us
        // yet should knock under this device's name, not a fingerprint placeholder.
        name: Some(punktfunk_core::client::device_name()),
        pin,
        identity: Some(identity),
        ..ConnectParams::new(addr, port, mode, Duration::from_secs(15))
    })
    .map_err(|e| {
        tracing::warn!(error = ?e, "speed test connect");
        "Couldn't start the speed test".to_string()
    })?;
    c.request_probe(TARGET_KBPS, BURST_MS).map_err(|e| {
        tracing::warn!(error = ?e, "speed test probe request");
        "The host didn't start the speed test".to_string()
    })?;
    let deadline = Instant::now() + POLL_BUDGET;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = c.probe_result();
        if now.done {
            std::thread::sleep(SETTLE);
            return Ok(c.probe_result());
        }
        progress(now.throughput_kbps);
        if Instant::now() > deadline {
            return Err("The speed test didn't finish in time".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_keeps_thirty_percent_back() {
        assert_eq!(recommended_kbps(100_000), 70_000);
        // Truncating, never rounding up: a recommendation must not exceed the measurement.
        assert_eq!(recommended_kbps(9), 0);
        assert_eq!(recommended_kbps(412_345), 288_638);
    }
}
