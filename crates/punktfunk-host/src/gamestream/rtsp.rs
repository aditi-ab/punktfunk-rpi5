//! The GameStream RTSP handshake (TCP 48010). Hand-rolled because GameStream's RTSP is
//! non-standard (streamid= targets, the literal `DEADBEEFCAFE` session, the X-SS-* headers)
//! and off-the-shelf RTSP crates assume standard semantics. Sequence Moonlight drives:
//! OPTIONS → DESCRIBE → SETUP(audio/video/control) → ANNOUNCE → PLAY. ANNOUNCE carries the
//! negotiated stream config; PLAY is where the media stages start (P1.3+).
//!
//! Runs on its own native thread (control-plane setup, not the per-frame hot path), one
//! thread per connection. Plaintext only for now (encryption is negotiated; P1.5).

use super::audio;
use super::stream::{self, StreamConfig};
use super::{AppState, AUDIO_PORT, CONTROL_PORT, RTSP_PORT, VIDEO_PORT};
use crate::encode::Codec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Opaque per-session payload the client echoes as its first UDP datagram (port-learning).
const PING_PAYLOAD: &str = "0011223344556677";

// The RTSP listener is UNAUTHENTICATED (no TLS/pairing) and one-thread-per-connection, so bound
// every attacker-controllable dimension to deny a pre-auth slow-loris / memory-growth DoS: a hard
// cap on concurrent connections, a per-read timeout so a stalled peer can't pin a thread, and
// size caps on the request headers + body (real GameStream RTSP messages are a few hundred bytes).
const MAX_RTSP_CONNS: usize = 8;
const RTSP_READ_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RTSP_HEADER: usize = 16 * 1024;
const MAX_RTSP_BODY: usize = 64 * 1024;
const MAX_RTSP_MSG: usize = 128 * 1024;

/// Live RTSP connection count, so a flood can't spawn unbounded threads. Decremented by [`ConnGuard`].
static RTSP_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`RTSP_ACTIVE`] when a connection thread exits (normally OR on panic).
struct ConnGuard;
impl Drop for ConnGuard {
    fn drop(&mut self) {
        RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bind 48010 and accept RTSP connections on a dedicated thread.
pub fn spawn(state: Arc<AppState>) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", RTSP_PORT))
        .with_context(|| format!("bind RTSP {RTSP_PORT}"))?;
    tracing::info!(port = RTSP_PORT, "RTSP listening");
    std::thread::Builder::new()
        .name("punktfunk-rtsp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        // Reserve a slot; over the cap, drop the connection (close) without a thread.
                        if RTSP_ACTIVE.fetch_add(1, Ordering::Relaxed) >= MAX_RTSP_CONNS {
                            RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                            tracing::warn!("RTSP: too many concurrent connections — dropping");
                            continue; // `stream` drops → connection closed
                        }
                        // Construct the slot guard BEFORE spawning and move it into the worker, so the
                        // slot is released even if `thread::spawn` itself panics (OS thread-limit) —
                        // the closure (and its captured guard) is dropped during the unwind.
                        let guard = ConnGuard;
                        let st = state.clone();
                        std::thread::spawn(move || {
                            let _guard = guard; // releases the slot on exit/panic
                            if let Err(e) = handle_conn(stream, st) {
                                tracing::warn!(error = %format!("{e:#}"), "RTSP connection ended");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "RTSP accept failed"),
                }
            }
        })
        .context("spawn RTSP thread")?;
    Ok(())
}

struct Request {
    method: String,
    uri: String,
    cseq: String,
    head: String,
    body: String,
}

fn handle_conn(mut stream: TcpStream, state: Arc<AppState>) -> Result<()> {
    let peer = stream.peer_addr().ok();
    // A per-read timeout so a stalled/slow-loris peer can't pin this thread indefinitely.
    let _ = stream.set_read_timeout(Some(RTSP_READ_TIMEOUT));
    let mut buf: Vec<u8> = Vec::new();
    // GameStream RTSP is one request per TCP connection: moonlight-common-c reads the
    // response until EOF, so we answer one message and close the connection (which signals
    // the end of the response). Session state lives in `AppState`, not the connection.
    if let Some(req) = read_message(&mut stream, &mut buf)? {
        tracing::info!(
            method = %req.method, cseq = %req.cseq,
            "RTSP {} | {}", req.head.replace("\r\n", " | "),
            if req.body.is_empty() { String::new() } else { format!("body: {}", req.body.replace("\r\n", " | ")) }
        );
        let resp = handle_request(&req, &state);
        stream.write_all(resp.as_bytes()).context("RTSP write")?;
        stream.flush().ok();
        // Close (FIN after the flushed response) so the client detects end-of-response.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = peer;
    Ok(())
}

/// Read one complete RTSP message (headers + any Content-Length body) from the stream,
/// buffering across reads and leaving any pipelined remainder in `buf`.
fn read_message(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Option<Request>> {
    loop {
        if let Some(end) = find_subslice(buf, b"\r\n\r\n") {
            // Cap the header section even when the terminator IS present (a single oversized header
            // block that fits a `\r\n\r\n` would otherwise skip the no-terminator cap below).
            if end > MAX_RTSP_HEADER {
                anyhow::bail!("RTSP headers exceed limit");
            }
            let head = std::str::from_utf8(&buf[..end]).context("RTSP header utf8")?;
            let content_len = header_value(head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            // Reject an absurd Content-Length before waiting to buffer it (allocation amplification).
            if content_len > MAX_RTSP_BODY {
                anyhow::bail!("RTSP Content-Length {content_len} exceeds limit");
            }
            let total = end + 4 + content_len;
            if buf.len() < total {
                // headers complete but body still arriving — read more
            } else {
                let head = head.to_string();
                let body = String::from_utf8_lossy(&buf[end + 4..total]).into_owned();
                buf.drain(..total);
                return Ok(Some(parse_request(&head, body)));
            }
        } else if buf.len() > MAX_RTSP_HEADER {
            // No header terminator within the cap — a slow-loris dribbling headers forever.
            anyhow::bail!("RTSP headers exceed limit");
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(None); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_RTSP_MSG {
            anyhow::bail!("RTSP message exceeds limit");
        }
    }
}

fn parse_request(head: &str, body: String) -> Request {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("").to_string();
    let cseq = header_value(head, "cseq").unwrap_or("0").trim().to_string();
    Request {
        method,
        uri,
        cseq,
        head: head.to_string(),
        body,
    }
}

fn handle_request(req: &Request, state: &AppState) -> String {
    match req.method.as_str() {
        "OPTIONS" => response(
            &req.cseq,
            &[("Public", "OPTIONS DESCRIBE SETUP ANNOUNCE PLAY TEARDOWN")],
            None,
        ),
        "DESCRIBE" => response(
            &req.cseq,
            &[("Content-Type", "application/sdp")],
            Some(&describe_sdp()),
        ),
        "SETUP" => {
            let (port, extra_key) = match stream_type(&req.uri) {
                Some("audio") => (AUDIO_PORT, "X-SS-Ping-Payload"),
                Some("video") => (VIDEO_PORT, "X-SS-Ping-Payload"),
                Some("control") => (CONTROL_PORT, "X-SS-Connect-Data"),
                _ => return response_status("404 Not Found", &req.cseq, &[], None),
            };
            let transport = format!("server_port={port}");
            response(
                &req.cseq,
                &[
                    ("Session", "DEADBEEFCAFE;timeout = 90"),
                    ("Transport", &transport),
                    (extra_key, PING_PAYLOAD),
                ],
                None,
            )
        }
        "ANNOUNCE" => {
            let map = parse_announce(&req.body);
            match stream_config(&map) {
                Some(cfg) => {
                    tracing::info!(?cfg, "RTSP ANNOUNCE — negotiated stream config");
                    *state.stream.lock().unwrap() = Some(cfg);
                }
                None => tracing::warn!("RTSP ANNOUNCE — missing required video config keys"),
            }
            let ap = audio_params(&map);
            tracing::info!(?ap, "RTSP ANNOUNCE — negotiated audio params");
            *state.audio_params.lock().unwrap() = ap;
            response(&req.cseq, &[], None)
        }
        "PLAY" => {
            let cfg = *state.stream.lock().unwrap();
            match cfg {
                Some(cfg) if !state.streaming.swap(true, Ordering::SeqCst) => {
                    // Resolve the launched catalog entry (session recipe) for the stream.
                    let app = state
                        .launch
                        .lock()
                        .unwrap()
                        .map(|l| l.appid)
                        .and_then(super::apps::by_id);
                    tracing::info!(app = ?app.as_ref().map(|a| &a.title), "RTSP PLAY — starting video stream");
                    stream::start(
                        cfg,
                        app,
                        state.streaming.clone(),
                        state.force_idr.clone(),
                        state.rfi_range.clone(),
                        state.video_cap.clone(),
                    );
                }
                Some(_) => tracing::info!("RTSP PLAY — stream already running"),
                None => tracing::warn!("RTSP PLAY — no negotiated config (ANNOUNCE missing)"),
            }
            // Audio runs independently (Opus on UDP 48000, stereo or 5.1/7.1 multistream per
            // the ANNOUNCE); it needs the launch key for the AES-CBC payload encryption the
            // client expects.
            let launch = *state.launch.lock().unwrap();
            if let Some(ls) = launch {
                if !state.audio_streaming.swap(true, Ordering::SeqCst) {
                    tracing::info!("RTSP PLAY — starting audio stream");
                    audio::start(
                        state.audio_streaming.clone(),
                        ls.gcm_key,
                        ls.rikeyid,
                        *state.audio_params.lock().unwrap(),
                        state.audio_cap.clone(),
                    );
                }
            }
            response(&req.cseq, &[("Session", "DEADBEEFCAFE;timeout = 90")], None)
        }
        "TEARDOWN" => {
            // Signal both stream threads to stop.
            state.streaming.store(false, Ordering::SeqCst);
            state.audio_streaming.store(false, Ordering::SeqCst);
            response(&req.cseq, &[], None)
        }
        other => {
            tracing::warn!(method = other, "RTSP unsupported method");
            response_status("501 Not Implemented", &req.cseq, &[], None)
        }
    }
}

/// Host capability SDP returned by DESCRIBE. Advertises HEVC + AV1 and no encryption
/// (plaintext streams for now; P1.5 adds the negotiated AES paths).
fn describe_sdp() -> String {
    // Line-oriented a=key:value, matching what moonlight-common-c scans for.
    let mut lines: Vec<String> = vec![
        "a=x-ss-general.featureFlags:0".into(),
        "a=x-ss-general.encryptionSupported:0".into(),
        "a=x-ss-general.encryptionRequested:0".into(),
        "sprop-parameter-sets=AAAAAU".into(), // HEVC capability indicator
        "a=rtpmap:98 AV1/90000".into(),       // AV1 capability indicator
    ];
    // Opus configs, one line per layout (Sunshine's order): the client scans for the FIRST
    // `surround-params=<channelCount>` match as its normal-quality decoder config and a
    // SECOND match as the high-quality config (which is also what makes it offer HQ at all),
    // so normal must precede HQ per channel count. Stereo lines are emitted for parity with
    // Sunshine but ignored by 2-channel clients (they hardcode 21101). See
    // `audio::surround_params` for the mapping pre-rotation the normal-quality lines carry.
    for (layout, hq) in [
        (&audio::LAYOUT_STEREO, false),
        (&audio::LAYOUT_STEREO, true),
        (&audio::LAYOUT_51, false),
        (&audio::LAYOUT_51_HQ, true),
        (&audio::LAYOUT_71, false),
        (&audio::LAYOUT_71_HQ, true),
    ] {
        lines.push(format!(
            "a=fmtp:97 surround-params={}",
            audio::surround_params(layout, hq)
        ));
    }
    lines.push(String::new());
    lines.join("\r\n")
}

/// Parse an ANNOUNCE SDP body's `a=key:value` lines into a map.
fn parse_announce(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

/// Map the negotiated ANNOUNCE keys to a [`StreamConfig`] (resolution/packetSize required).
fn stream_config(map: &HashMap<String, String>) -> Option<StreamConfig> {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let width = parse_u("x-nv-video[0].clientViewportWd")?;
    let height = parse_u("x-nv-video[0].clientViewportHt")?;
    // packetSize is attacker-controlled and PRE-AUTH (the RTSP listener is unauthenticated). It sets
    // the per-shard payload (`packet_size - 16`); a tiny value underflows / div-by-zeros the video
    // thread, an absurd one amplifies per-shard allocation. Reject anything outside a sane range
    // (real Moonlight uses ~1024) so a malformed ANNOUNCE fails here instead of panicking the stream.
    const PACKET_SIZE_MIN: usize = 64;
    const PACKET_SIZE_MAX: usize = 2048;
    let packet_size = parse_u("x-nv-video[0].packetSize")? as usize;
    if !(PACKET_SIZE_MIN..=PACKET_SIZE_MAX).contains(&packet_size) {
        tracing::warn!(
            packet_size,
            "RTSP ANNOUNCE: out-of-range packetSize — rejecting"
        );
        return None;
    }
    let fps = parse_u("x-nv-video[0].maxFPS")
        .filter(|&f| f > 0)
        .unwrap_or(60);
    let bitrate_kbps = parse_u("x-nv-vqos[0].bw.maximumBitrateKbps").unwrap_or(20_000);
    // Client codec choice (moonlight-common-c SdpGenerator.c): 0=H264, 1=HEVC, 2=AV1.
    let codec = match map.get("x-nv-vqos[0].bitStreamFormat").map(|s| s.trim()) {
        Some("1") => Codec::H265,
        Some("2") => Codec::Av1,
        _ => Codec::H264,
    };
    // 10-bit/HDR request flag. We never advertise the Main10 SCM bits, so a compliant
    // client can't ask — if one does anyway, stream 8-bit SDR rather than failing.
    if parse_u("x-nv-video[0].dynamicRangeMode").unwrap_or(0) != 0 {
        tracing::warn!(
            "client requested HDR/10-bit (dynamicRangeMode != 0) — not advertised/supported, \
             streaming 8-bit SDR"
        );
    }
    // Parity floor the client asks for (protects small frames); clamp to a sane max.
    let min_fec = parse_u("x-nv-vqos[0].fec.minRequiredFecPackets")
        .unwrap_or(2)
        .min(16) as u8;
    Some(StreamConfig {
        width,
        height,
        fps,
        packet_size,
        bitrate_kbps,
        codec,
        min_fec,
    })
}

/// Map the negotiated ANNOUNCE keys to the session [`audio::AudioParams`]. Attribute names
/// per moonlight-common-c `SdpGenerator.c` (verified 2026-06-10): the client always emits
/// `x-nv-audio.surround.numChannels`/`channelMask` and `x-nv-aqos.packetDuration`;
/// `x-nv-audio.surround.AudioQuality` is 1 only when it saw our second surround-params line
/// and opted into high-quality surround. Unknown channel counts fall back to stereo.
fn audio_params(map: &HashMap<String, String>) -> audio::AudioParams {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let requested = parse_u("x-nv-audio.surround.numChannels").unwrap_or(2);
    let channels = match requested {
        2 | 6 | 8 => requested as u8,
        other => {
            tracing::warn!(channels = other, "unsupported channel count — using stereo");
            2
        }
    };
    let high_quality = parse_u("x-nv-audio.surround.AudioQuality") == Some(1);
    // Moonlight uses 5 ms (default) or 10 ms (slow decoder / low-bitrate links). Snap to
    // those two — an in-between value like 7 isn't a legal Opus frame size and would make
    // every encode fail; clamping (not snapping) would let it through.
    let packet_duration_ms = match parse_u("x-nv-aqos.packetDuration") {
        Some(d) if d >= 10 => 10,
        _ => 5,
    };
    audio::AudioParams {
        channels,
        high_quality,
        packet_duration_ms,
    }
}

/// Extract the stream type from a SETUP URI like `…/streamid=video/0/0`.
fn stream_type(uri: &str) -> Option<&str> {
    let after = uri.split("streamid=").nth(1)?;
    let token = after.split('/').next()?;
    match token {
        "audio" | "video" | "control" => Some(token),
        _ => None,
    }
}

fn response(cseq: &str, headers: &[(&str, &str)], body: Option<&str>) -> String {
    response_status("200 OK", cseq, headers, body)
}

fn response_status(
    status: &str,
    cseq: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let body = body.unwrap_or("");
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn header_value<'a>(head: &'a str, key_lower: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(key_lower)).then(|| v.trim_start())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut body = String::from(
            "v=0\r\n\
             a=x-nv-video[0].clientViewportWd:1920\r\n\
             a=x-nv-video[0].clientViewportHt:1080\r\n\
             a=x-nv-video[0].packetSize:1392\r\n\
             a=x-nv-video[0].maxFPS:120\r\n\
             a=x-nv-vqos[0].bw.maximumBitrateKbps:40000\r\n",
        );
        for (k, v) in extra {
            body.push_str(&format!("a={k}:{v}\r\n"));
        }
        parse_announce(&body)
    }

    /// `x-nv-vqos[0].bitStreamFormat` → codec (0=H264, 1=HEVC, 2=AV1; missing = H264).
    #[test]
    fn announce_codec_selection() {
        for (fmt, codec) in [
            (Some("0"), Codec::H264),
            (Some("1"), Codec::H265),
            (Some("2"), Codec::Av1),
            (None, Codec::H264),
        ] {
            let map = match fmt {
                Some(f) => announce(&[("x-nv-vqos[0].bitStreamFormat", f)]),
                None => announce(&[]),
            };
            let cfg = stream_config(&map).expect("required keys present");
            assert_eq!(cfg.codec, codec, "bitStreamFormat {fmt:?}");
            assert_eq!((cfg.width, cfg.height, cfg.fps), (1920, 1080, 120));
            assert_eq!(cfg.bitrate_kbps, 40_000);
        }
    }

    /// Missing required video keys → no config (the PLAY handler then refuses to stream).
    #[test]
    fn announce_missing_required_keys() {
        let mut map = announce(&[]);
        map.remove("x-nv-video[0].packetSize");
        assert!(stream_config(&map).is_none());
    }

    /// packetSize is attacker-controlled AND pre-auth (the RTSP listener is unauthenticated), so an
    /// out-of-range value must be rejected here rather than panic the video thread (≤16 → div-by-zero
    /// / underflow; absurd → allocation amplification). Sane values (real Moonlight ~1024) pass.
    #[test]
    fn announce_rejects_out_of_range_packet_size() {
        for bad in ["0", "16", "63", "4096", "999999"] {
            let map = announce(&[("x-nv-video[0].packetSize", bad)]);
            assert!(
                stream_config(&map).is_none(),
                "out-of-range packetSize {bad} must be rejected"
            );
        }
        for ok in ["64", "1024", "1392", "2048"] {
            let map = announce(&[("x-nv-video[0].packetSize", ok)]);
            assert!(
                stream_config(&map).is_some(),
                "in-range packetSize {ok} must be accepted"
            );
        }
    }

    /// Audio negotiation: numChannels/AudioQuality/packetDuration, with Moonlight defaults.
    #[test]
    fn announce_audio_params() {
        // Stereo defaults when the attributes are absent (and the legacy path).
        assert_eq!(audio_params(&announce(&[])), audio::AudioParams::default());
        // 5.1 normal quality at 5 ms.
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "6"),
            ("x-nv-audio.surround.channelMask", "63"),
            ("x-nv-audio.surround.AudioQuality", "0"),
            ("x-nv-aqos.packetDuration", "5"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (6, false, 5)
        );
        // 7.1 high quality; 10 ms duration honored.
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "8"),
            ("x-nv-audio.surround.AudioQuality", "1"),
            ("x-nv-aqos.packetDuration", "10"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (8, true, 10)
        );
        // Bogus channel count falls back to stereo.
        let ap = audio_params(&announce(&[("x-nv-audio.surround.numChannels", "4")]));
        assert_eq!(ap.channels, 2);
    }

    /// The DESCRIBE SDP carries the codec indicators and all six Opus configs, normal
    /// quality before high quality per channel count (the client takes the first match as
    /// its normal config and a second match as HQ).
    #[test]
    fn describe_advertises_codecs_and_surround() {
        let sdp = describe_sdp();
        assert!(
            sdp.contains("sprop-parameter-sets=AAAAAU"),
            "HEVC indicator"
        );
        assert!(sdp.contains("a=rtpmap:98 AV1/90000"), "AV1 indicator");
        for params in [
            "21101",       // stereo (clients hardcode this; emitted for Sunshine parity)
            "642012453",   // 5.1 normal — pre-rotated for the client's GFE-order swap
            "660012345",   // 5.1 high quality — verbatim
            "85301245673", // 7.1 normal — pre-rotated over [3, 8)
            "88001234567", // 7.1 high quality — verbatim
        ] {
            assert!(
                sdp.contains(&format!("a=fmtp:97 surround-params={params}")),
                "missing surround-params={params} in:\n{sdp}"
            );
        }
        // Normal precedes HQ for each surround channel count.
        let n51 = sdp.find("surround-params=642").unwrap();
        let h51 = sdp.find("surround-params=660").unwrap();
        let n71 = sdp.find("surround-params=853").unwrap();
        let h71 = sdp.find("surround-params=880").unwrap();
        assert!(n51 < h51 && n71 < h71);
    }
}
