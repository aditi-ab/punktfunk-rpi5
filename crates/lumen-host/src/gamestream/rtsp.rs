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
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Opaque per-session payload the client echoes as its first UDP datagram (port-learning).
const PING_PAYLOAD: &str = "0011223344556677";

/// Bind 48010 and accept RTSP connections on a dedicated thread.
pub fn spawn(state: Arc<AppState>) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", RTSP_PORT))
        .with_context(|| format!("bind RTSP {RTSP_PORT}"))?;
    tracing::info!(port = RTSP_PORT, "RTSP listening");
    std::thread::Builder::new()
        .name("lumen-rtsp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        let st = state.clone();
                        std::thread::spawn(move || {
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
            let head = std::str::from_utf8(&buf[..end]).context("RTSP header utf8")?;
            let content_len = header_value(head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let total = end + 4 + content_len;
            if buf.len() < total {
                // headers complete but body still arriving — read more
            } else {
                let head = head.to_string();
                let body = String::from_utf8_lossy(&buf[end + 4..total]).into_owned();
                buf.drain(..total);
                return Ok(Some(parse_request(&head, body)));
            }
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(None); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
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
            response(&req.cseq, &[], None)
        }
        "PLAY" => {
            let cfg = *state.stream.lock().unwrap();
            match cfg {
                Some(cfg) if !state.streaming.swap(true, Ordering::SeqCst) => {
                    tracing::info!("RTSP PLAY — starting video stream");
                    stream::start(
                        cfg,
                        state.streaming.clone(),
                        state.force_idr.clone(),
                        state.video_cap.clone(),
                    );
                }
                Some(_) => tracing::info!("RTSP PLAY — stream already running"),
                None => tracing::warn!("RTSP PLAY — no negotiated config (ANNOUNCE missing)"),
            }
            // Audio runs independently (stereo Opus on UDP 48000); it needs the launch key for
            // the AES-CBC payload encryption the client expects.
            let launch = *state.launch.lock().unwrap();
            if let Some(ls) = launch {
                if !state.audio_streaming.swap(true, Ordering::SeqCst) {
                    tracing::info!("RTSP PLAY — starting audio stream");
                    audio::start(
                        state.audio_streaming.clone(),
                        ls.gcm_key,
                        ls.rikeyid,
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
    [
        "a=x-ss-general.featureFlags:0",
        "a=x-ss-general.encryptionSupported:0",
        "a=x-ss-general.encryptionRequested:0",
        "sprop-parameter-sets=AAAAAU", // HEVC capability indicator
        "a=rtpmap:98 AV1/90000",       // AV1 capability indicator
        // Opus config the client matches by channel count (Sunshine emits one per config):
        // surround-params = channelCount, streams, coupledStreams, then the channel mapping.
        // The client negotiated stereo, so advertise just that.
        "a=fmtp:97 surround-params=21101", // stereo: 2ch, 1 stream, 1 coupled, mapping [0,1]
        "",
    ]
    .join("\r\n")
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
    let packet_size = parse_u("x-nv-video[0].packetSize")? as usize;
    let fps = parse_u("x-nv-video[0].maxFPS")
        .filter(|&f| f > 0)
        .unwrap_or(60);
    let bitrate_kbps = parse_u("x-nv-vqos[0].bw.maximumBitrateKbps").unwrap_or(20_000);
    let codec = match map.get("x-nv-vqos[0].bitStreamFormat").map(|s| s.trim()) {
        Some("1") => Codec::H265,
        Some("2") => Codec::Av1,
        _ => Codec::H264,
    };
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
