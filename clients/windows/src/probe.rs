//! Network speed-test probe — the GUI's per-host "Test Network Speed…" ([`crate::app`]'s
//! speed page) and the `--headless --speed-test` CLI.
//!
//! Split out of the former in-process session module: the shared spawned-`punktfunk-session`
//! binary owns real streaming now, but the speed test is a shell-side, decode-less measurement
//! over the real data plane, so it stays here. [`decodable_codecs`] rode along for the same
//! reason — the probe connect still advertises which codecs this client can decode.

use ffmpeg_next as ffmpeg;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, GamepadPref, Mode};
use std::time::{Duration, Instant};

/// The `quic` codec bitfield this client can decode — whatever FFmpeg has a decoder for (HEVC/H.264
/// always; AV1 when built in). Advertised to the host so it never emits a codec we can't decode.
pub fn decodable_codecs() -> u8 {
    let _ = ffmpeg::init();
    let mut bits = 0u8;
    for (id, bit) in [
        (ffmpeg::codec::Id::HEVC, punktfunk_core::quic::CODEC_HEVC),
        (ffmpeg::codec::Id::H264, punktfunk_core::quic::CODEC_H264),
        (ffmpeg::codec::Id::AV1, punktfunk_core::quic::CODEC_AV1),
    ] {
        if ffmpeg::decoder::find(id).is_some() {
            bits |= bit;
        }
    }
    bits
}

/// Blocking speed-test probe (the GUI's per-host "Test" and the `--headless --speed-test` CLI):
/// a minimal identified connect (720p60 — the host builds a virtual output, but nothing is
/// decoded), then `request_probe` (a 2 s burst up to the host's 3 Gbps ceiling) polled to
/// completion. Run on a worker thread.
pub fn run_speed_probe(
    addr: &str,
    port: u16,
    fp_hex: Option<&str>,
    identity: (String, String),
) -> Result<punktfunk_core::client::ProbeOutcome, String> {
    // Pin the saved/advertised fingerprint when we have one; a manual host measures over TOFU.
    let pin = fp_hex.and_then(crate::trust::parse_hex32);
    let c = NativeClient::connect(
        addr,
        port,
        Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        },
        CompositorPref::Auto,
        GamepadPref::Auto,
        0, // bitrate_kbps: host default
        0, // video_caps: probe connect, nothing is decoded
        2, // audio_channels: stereo baseline
        decodable_codecs(),
        0,    // preferred_codec: no preference
        None, // display_hdr: probe connect, nothing presents
        0,    // client_caps: probe connect, nothing renders a cursor
        None, // launch: no game
        // Same label a real session sends — a speed test against a host that doesn't know us yet
        // should knock under this device's name, not a fingerprint placeholder.
        Some(punktfunk_core::client::device_name()),
        pin,
        Some(identity),
        Duration::from_secs(15),
    )
    .map_err(|e| format!("connect: {e:?}"))?;
    c.request_probe(3_000_000, 2_000)
        .map_err(|e| format!("probe: {e:?}"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if c.probe_result().done {
            // Let the last UDP shards land before tearing down.
            std::thread::sleep(Duration::from_millis(400));
            return Ok(c.probe_result());
        }
        if Instant::now() > deadline {
            return Err("probe timed out".to_string());
        }
    }
}
